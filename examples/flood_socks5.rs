//! SOCKS5 真实协议压测客户端：按目标 QPS 持续建连 → 握手 → CONNECT → 小请求回显 → 关闭。
//!
//! 用法：
//!   `cargo run --release --example flood_socks5 -- <balancer_addr> <target_addr> [concurrency] [qps] [duration_secs]`
//!   balancer_addr  balancer 监听地址，如 127.0.0.1:5700
//!   target_addr    CONNECT 目标（**仅 IPv4**），如 127.0.0.1:6100（echo_upstream）
//!   concurrency    最大在途连接数（默认 64；信号量背压）
//!   qps            目标建连速率（默认 1000；0 = 不限速，仅受并发上限约束）
//!   duration_secs  持续秒数（默认 60）
//!
//! 每条连接流程（RFC1928）：
//!   1) TCP 连 balancer；
//!   2) 发 `05 01 00`（只提供无认证），校验回 `05 00`；
//!   3) 发 CONNECT（IPv4），校验 `05 00 00 ...`（BND 地址按 ATYP 读全，兼容域名/IPv6 应答）；
//!   4) 发 `ping\r\n` 并读回等长回显，内容不一致记为失败；
//!   5) 以 SO_LINGER=0 主动 RST 关闭，避免客户端侧堆积 TIME_WAIT（高 QPS 长跑必需）。
//!
//! 统计口径：
//!   - 成功：完整走完 1)~4) 且回显一致；
//!   - 失败：任一步骤出错/超时/回显不一致；
//!   - 握手延迟：**成功连接**从 TCP 建连开始到收到 CONNECT 成功应答的耗时（p50/p95/p99）；
//!   - 实际 QPS：完成连接数（成功+失败）/ 总耗时。
//!
//! 结束时打印一行汇总（字段固定、无空格，便于脚本 grep）：
//!   `flood_socks5 done: ok=... failed=... success_rate=...% actual_qps=... handshake_p50_ms=... handshake_p95_ms=... handshake_p99_ms=... spawned=... duration_s=...`
//!
//! 参照 examples/flood.rs 的参数风格；压测工具不是产品代码。

use std::net::{SocketAddr, SocketAddrV4};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;

/// 单步（方法协商 / CONNECT 应答 / 回显）超时。
const STEP_TIMEOUT: Duration = Duration::from_secs(5);
/// 小请求 payload（echo_upstream 原样回显）。
const PAYLOAD: &[u8] = b"ping\r\n";
/// 默认并发连接数。
const DEFAULT_CONCURRENCY: usize = 64;
/// 默认目标 QPS。
const DEFAULT_QPS: u64 = 1000;
/// 默认持续秒数。
const DEFAULT_DURATION_SECS: u64 = 60;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(balancer_raw) = args.next() else {
        eprintln!(
            "用法: flood_socks5 <balancer_addr> <target_addr> [concurrency] [qps] [duration_secs]"
        );
        std::process::exit(2);
    };
    let Some(target_raw) = args.next() else {
        eprintln!(
            "用法: flood_socks5 <balancer_addr> <target_addr> [concurrency] [qps] [duration_secs]"
        );
        std::process::exit(2);
    };

    let balancer: SocketAddr = balancer_raw
        .parse()
        .unwrap_or_else(|_| panic!("balancer_addr 必须是 host:port，收到: {balancer_raw}"));
    let target: SocketAddr = target_raw
        .parse()
        .unwrap_or_else(|_| panic!("target_addr 必须是 host:port，收到: {target_raw}"));
    if !target.is_ipv4() {
        // CONNECT 固定用 IPv4 报文；避免目标为 IPv6 时构造出错误地址类型。
        panic!("target_addr 仅支持 IPv4，收到: {target}");
    }

    let concurrency: usize = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CONCURRENCY);
    let qps: u64 = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_QPS);
    let duration_secs: u64 = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_DURATION_SECS);
    if concurrency == 0 {
        panic!("concurrency 必须大于 0");
    }
    if duration_secs == 0 {
        panic!("duration_secs 必须大于 0");
    }
    let qps_desc = if qps == 0 {
        "不限速".to_string()
    } else {
        format!("{qps}/s")
    };
    println!(
        "flood_socks5 start: balancer={balancer} target={target} concurrency={concurrency} \
         target_qps={qps_desc} duration_s={duration_secs}"
    );

    let started = Instant::now();
    let deadline = started + Duration::from_secs(duration_secs);
    // 信号量限制在途连接数：既防止瞬间打爆进程 fd，也让实际 QPS 有界。
    let sem = std::sync::Arc::new(Semaphore::new(concurrency));
    let mut tasks = tokio::task::JoinSet::new();
    let mut spawned: u64 = 0;

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        // 按目标 QPS 计算本次建连的理想时刻；落后时不补睡（尽力追赶，避免雪崩式补偿）。
        if qps > 0 {
            let pace_at = started + Duration::from_secs_f64(spawned as f64 / qps as f64);
            if pace_at > now {
                tokio::time::sleep_until(tokio::time::Instant::from_std(pace_at)).await;
                if Instant::now() >= deadline {
                    break;
                }
            }
        }
        // 并发背压：在途连接达到上限时先等额度再建连。
        let permit = sem.clone().acquire_owned().await.expect("信号量不会被关闭");
        tasks.spawn(async move {
            let result = run_once(balancer, target).await;
            drop(permit); // 连接结束立即归还并发额度
            result
        });
        spawned += 1;
    }
    let spawn_elapsed = started.elapsed();

    // 收敛在途连接（每条连接各步骤都有超时，不会长期挂住）。
    let mut ok: u64 = 0;
    let mut failed: u64 = 0;
    let mut latencies: Vec<u64> = Vec::with_capacity(spawned as usize);
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Some(latency_us)) => {
                ok += 1;
                latencies.push(latency_us);
            }
            _ => failed += 1,
        }
    }
    let total = started.elapsed();

    // 百分位：最近秩法（nearest-rank），排序后取索引，样本少时也稳定。
    latencies.sort_unstable();
    let p50 = percentile(&latencies, 0.50);
    let p95 = percentile(&latencies, 0.95);
    let p99 = percentile(&latencies, 0.99);
    let completed = ok + failed;
    let success_rate = if completed == 0 {
        0.0
    } else {
        ok as f64 * 100.0 / completed as f64
    };
    let actual_qps = if total.as_secs_f64() > 0.0 {
        completed as f64 / total.as_secs_f64()
    } else {
        0.0
    };

    println!(
        "flood_socks5 done: ok={ok} failed={failed} success_rate={success_rate:.2}% \
         actual_qps={actual_qps:.1} handshake_p50_ms={:.3} handshake_p95_ms={:.3} \
         handshake_p99_ms={:.3} spawned={spawned} duration_s={:.2} spawn_s={:.2}",
        p50 as f64 / 1000.0,
        p95 as f64 / 1000.0,
        p99 as f64 / 1000.0,
        total.as_secs_f64(),
        spawn_elapsed.as_secs_f64(),
    );
    Ok(())
}

/// 最近秩百分位；空样本返回 0。
fn percentile(sorted_us: &[u64], p: f64) -> u64 {
    if sorted_us.is_empty() {
        return 0;
    }
    let idx = ((sorted_us.len() - 1) as f64 * p).round() as usize;
    sorted_us[idx.min(sorted_us.len() - 1)]
}

/// 执行一次完整连接；成功返回「建连开始 → CONNECT 成功应答」的耗时（微秒）。
async fn run_once(balancer: SocketAddr, target: SocketAddr) -> Option<u64> {
    let started = Instant::now();
    let SocketAddr::V4(target_v4) = target else {
        return None;
    };

    // 1) TCP 建连 balancer
    let mut stream = TcpStream::connect(balancer).await.ok()?;
    // 小包来回，关闭 Nagle 降低本地握手时延抖动。
    let _ = stream.set_nodelay(true);

    // 2) 方法协商：VER=0x05，NMETHODS=1，METHOD=0x00（无认证）
    stream.write_all(&[0x05, 0x01, 0x00]).await.ok()?;
    let mut selected = [0u8; 2];
    read_exact_timeout(&mut stream, &mut selected).await.ok()?;
    if selected != [0x05, 0x00] {
        return None;
    }

    // 3) CONNECT（IPv4 固定报文）
    stream.write_all(&connect_request(target_v4)).await.ok()?;
    let mut head = [0u8; 4]; // VER REP RSV ATYP
    read_exact_timeout(&mut stream, &mut head).await.ok()?;
    if head[0] != 0x05 || head[1] != 0x00 {
        return None;
    }
    read_bnd_addr(&mut stream, head[3]).await.ok()?;
    // 握手完成时间点：CONNECT 成功应答已读全。
    let handshake_us = started.elapsed().as_micros() as u64;

    // 4) 小请求 + 回显校验
    stream.write_all(PAYLOAD).await.ok()?;
    let mut echoed = vec![0u8; PAYLOAD.len()];
    read_exact_timeout(&mut stream, &mut echoed).await.ok()?;
    if echoed != PAYLOAD {
        return None;
    }

    // 5) RST 关闭：不进入 TIME_WAIT，长时间高 QPS 下不耗尽本地临时端口。
    let _ = socket2::SockRef::from(&stream).set_linger(Some(Duration::from_secs(0)));
    drop(stream);
    Some(handshake_us)
}

/// 构造 CONNECT 请求：`05 01 00 01 <IPv4 4B> <port 2B>`。
fn connect_request(target: SocketAddrV4) -> [u8; 10] {
    let mut req = [0u8; 10];
    req[0] = 0x05; // VER
    req[1] = 0x01; // CMD = CONNECT
    req[2] = 0x00; // RSV
    req[3] = 0x01; // ATYP = IPv4
    req[4..8].copy_from_slice(&target.ip().octets());
    req[8..10].copy_from_slice(&target.port().to_be_bytes());
    req
}

/// 读掉应答里的 BND.ADDR/BND.PORT（按 ATYP 定长/变长读取）。
async fn read_bnd_addr(stream: &mut TcpStream, atyp: u8) -> Result<(), ()> {
    match atyp {
        0x01 => {
            let mut rest = [0u8; 4 + 2];
            read_exact_timeout(stream, &mut rest).await
        }
        0x04 => {
            let mut rest = [0u8; 16 + 2];
            read_exact_timeout(stream, &mut rest).await
        }
        0x03 => {
            let mut len = [0u8; 1];
            read_exact_timeout(stream, &mut len).await?;
            let mut rest = vec![0u8; len[0] as usize + 2];
            read_exact_timeout(stream, &mut rest).await
        }
        _ => Err(()),
    }
}

/// 带超时的 `read_exact`；失败统一折叠成 `Err(())`（压测只关心成败）。
async fn read_exact_timeout(stream: &mut TcpStream, buf: &mut [u8]) -> Result<(), ()> {
    match tokio::time::timeout(STEP_TIMEOUT, stream.read_exact(buf)).await {
        Ok(Ok(_)) => Ok(()),
        _ => Err(()),
    }
}
