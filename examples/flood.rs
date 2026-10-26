//! 连接压测工具：建立 N 个并发连接并保持一段时间，统计成功率、建连延迟与目标速率。
//!
//! 用法：`cargo run --example flood -- <target> [n] [hold_secs] [interval_ms] [rate_per_sec]`
//!   target       目标地址，如 127.0.0.1:5100
//!   n            并发连接数（默认 1000）
//!   hold_secs    每条连接保持秒数（默认 10）
//!   interval_ms  每建立一条连接后的间隔毫秒（默认 0 = 全速；仅在未给 rate_per_sec 时使用）
//!   rate_per_sec 目标建连速率（条/秒，默认 0 = 不控速）。
//!
//! 注意：本工具只建立 TCP 连接（首包不发），用于观察 RSS、fd、accept 速率与上限行为；
//! 真实 SOCKS5 协议口径见 `examples/flood_socks5.rs`。
//!
//! ## 为什么要有 `rate_per_sec`
//!
//! 早期版本用 `sleep(interval_ms)` 串行控速：`interval_ms=1` 的定时器实际约 1–2ms，
//! 累加后 30,000 条连接要 60s 以上（实测仅约 469/s），无法验证"1000 新连接/秒"的验收口径。
//! 现在改为**绝对时间调度**：每 10ms 唤醒一次，把"到此刻应当已建立的条数"
//! （`floor(rate × elapsed)`）补齐。这样长期平均速率精确、不受定时器粒度影响，
//! 同时每条连接仍独立成任务并发建连（不串行等待 connect 完成）。

use std::net::SocketAddr;
use std::time::{Duration, Instant};

fn spawn_conn(tasks: &mut tokio::task::JoinSet<Option<u64>>, target: SocketAddr, hold: u64) {
    tasks.spawn(async move {
        let t0 = Instant::now();
        match tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::TcpStream::connect(target),
        )
        .await
        {
            Ok(Ok(stream)) => {
                let connect_us = t0.elapsed().as_micros() as u64;
                // 保持连接，模拟空闲长连接
                tokio::time::sleep(Duration::from_secs(hold)).await;
                // 压测工具主动 RST 关闭，避免客户端侧堆积大量 TIME_WAIT 端口。
                // 用 socket2 设置 SO_LINGER=0（tokio 的 set_linger 已废弃：非 0 linger 会在 drop 时阻塞线程）。
                let _ = socket2::SockRef::from(&stream).set_linger(Some(Duration::from_secs(0)));
                drop(stream);
                Some(connect_us)
            }
            _ => None,
        }
    });
}

fn percentile_us(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx] as f64 / 1000.0
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let target: std::net::SocketAddr = args
        .next()
        .unwrap_or_else(|| "127.0.0.1:5100".to_string())
        .parse()
        .expect("target 必须是 host:port");
    let n: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1000);
    let hold: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10);
    let interval_ms: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let rate: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);

    println!(
        "flood: target={target} connections={n} hold={hold}s interval={interval_ms}ms rate={rate}/s"
    );

    let started = Instant::now();
    let mut tasks: tokio::task::JoinSet<Option<u64>> = tokio::task::JoinSet::new();

    if rate > 0.0 {
        // 绝对时间调度：每 tick 把"应已建立条数"补齐，长期平均速率 = rate。
        let tick = Duration::from_millis(10);
        let mut spawned: usize = 0;
        loop {
            let elapsed = started.elapsed().as_secs_f64();
            let want = ((elapsed * rate).floor() as usize).min(n);
            while spawned < want {
                spawn_conn(&mut tasks, target, hold);
                spawned += 1;
            }
            if spawned >= n {
                break;
            }
            tokio::time::sleep(tick).await;
        }
    } else {
        for _ in 0..n {
            spawn_conn(&mut tasks, target, hold);
            if interval_ms > 0 {
                tokio::time::sleep(Duration::from_millis(interval_ms)).await;
            }
        }
    }
    let spawn_elapsed = started.elapsed();

    let mut ok = 0usize;
    let mut failed = 0usize;
    let mut latencies: Vec<u64> = Vec::with_capacity(n);
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Some(us)) => {
                ok += 1;
                latencies.push(us);
            }
            _ => failed += 1,
        }
    }
    let total = started.elapsed();
    latencies.sort_unstable();

    println!(
        "flood done: ok={ok} failed={failed} spawn={:.3}s total={:.3}s (spawn rate={:.1}/s) connect_p50_ms={:.3} connect_p95_ms={:.3} connect_p99_ms={:.3}",
        spawn_elapsed.as_secs_f64(),
        total.as_secs_f64(),
        n as f64 / spawn_elapsed.as_secs_f64().max(0.001),
        percentile_us(&latencies, 0.50),
        percentile_us(&latencies, 0.95),
        percentile_us(&latencies, 0.99),
    );
    Ok(())
}
