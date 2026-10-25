//! 连接压测工具：建立 N 个并发连接并保持一段时间，统计成功率与耗时。
//!
//! 用法：`cargo run --example flood -- <target> [n] [hold_secs] [interval_ms]`
//!   target       目标地址，如 127.0.0.1:5100
//!   n            并发连接数（默认 1000）
//!   hold_secs    每条连接保持秒数（默认 10）
//!   interval_ms  每建立一条连接后的间隔毫秒（默认 0 = 全速；用于模拟 1K/s）
//!
//! 注意：本工具只建立 TCP 连接（M0 是纯透传，无需协议握手），
//! 用于观察 RSS、fd、accept 速率与上限行为。

use std::time::{Duration, Instant};

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

    println!("flood: target={target} connections={n} hold={hold}s interval={interval_ms}ms");

    let started = Instant::now();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..n {
        tasks.spawn(async move {
            match tokio::time::timeout(
                Duration::from_secs(5),
                tokio::net::TcpStream::connect(target),
            )
            .await
            {
                Ok(Ok(stream)) => {
                    // 保持连接，模拟空闲长连接
                    tokio::time::sleep(Duration::from_secs(hold)).await;
                    // 压测工具主动 RST 关闭，避免客户端侧堆积大量 TIME_WAIT 端口。
                    // 用 socket2 设置 SO_LINGER=0（tokio 的 set_linger 已废弃：非 0 linger 会在 drop 时阻塞线程）。
                    let _ =
                        socket2::SockRef::from(&stream).set_linger(Some(Duration::from_secs(0)));
                    drop(stream);
                    true
                }
                _ => false,
            }
        });
        if interval_ms > 0 {
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
        }
    }
    let spawn_elapsed = started.elapsed();

    let mut ok = 0usize;
    let mut failed = 0usize;
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(true) => ok += 1,
            _ => failed += 1,
        }
    }
    let total = started.elapsed();

    println!(
        "flood done: ok={ok} failed={failed} spawn={:.3}s total={:.3}s (spawn rate={:.1}/s)",
        spawn_elapsed.as_secs_f64(),
        total.as_secs_f64(),
        n as f64 / spawn_elapsed.as_secs_f64().max(0.001)
    );
    Ok(())
}
