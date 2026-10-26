//! 连接统计路径并发基准（开发/调优用，不参与功能验收）。
//!
//! 用法：`cargo run --release --example conn_bench -- [threads] [iters_per_thread]`
//!   threads  并发线程数（默认 = CPU 核数）
//!   iters    每线程迭代次数（默认 200000）
//!
//! 目的：量化 `ConnRegistry` / `ConnMeta` / `CancellationToken` 子令牌 / 全局原子
//! 这几条"连接统计"路径的并发上限，用来判断是否值得改造成无锁或消息架构。
//!
//! 分组：
//!   A) register+unregister，detail=true —— 完整路径（per_listen/per_client 全局锁
//!      + 分片锁 + 子令牌树锁 + 原子计数）
//!   B) detail=false —— 少一次分片锁
//!   C) enabled=false —— 只建 ConnMeta 与子令牌，不做任何计数（隔离"最小成本"）
//!   D) 纯 CancellationToken::child_token + drop —— 隔离令牌树上的全局锁
//!   E) Stats 全局原子（3 次/op）—— 无锁原子路径的对照上限
//!   F) tokio mpsc `try_send`（有界 64K + 单消费者 drain）—— 消息架构的热路径成本
//!   G) A1 无锁原型（per_listen 原子数组 + per_client 64 分片 + active 分片 + 明细 16 分片）——
//!      不改架构、只换数据结构时的上限估算

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use socks5_balancer_rust::config::ConnTrackingConfig;
use socks5_balancer_rust::conn::{ConnRegistry, ConnState};
use socks5_balancer_rust::handshake::Protocol;
use socks5_balancer_rust::stats::Stats;
use tokio_util::sync::CancellationToken;

fn report(label: &str, total: u64, secs: f64) {
    let ops = total as f64;
    println!(
        "{label:<34} {ops:>10.0} ops {secs:>9.3}s {:>12.0} ops/s {:>8.1} ns/op",
        ops / secs.max(1e-9),
        secs * 1e9 / ops.max(1.0)
    );
}

fn bench<F: Fn(u64) + Send + Sync + 'static>(label: &str, threads: usize, iters: u64, f: Arc<F>) {
    let start = Instant::now();
    let mut handles = Vec::with_capacity(threads);
    for t in 0..threads {
        let f = Arc::clone(&f);
        let base = t as u64 * iters;
        handles.push(std::thread::spawn(move || {
            for i in 0..iters {
                f(base + i);
            }
        }));
    }
    for h in handles {
        h.join().expect("bench 线程 panicked");
    }
    report(label, threads as u64 * iters, start.elapsed().as_secs_f64());
}

fn bench_registry(label: &str, enabled: bool, detail: bool, threads: usize, iters: u64) {
    let reg = ConnRegistry::new(
        ConnTrackingConfig {
            enabled,
            detail,
            detail_limit: 100_000,
            client_top_limit: 200,
        },
        4,
    );
    let parent = CancellationToken::new();
    let f = Arc::new(move |n: u64| {
        // 模拟不同客户端 IP / 同一监听地址，触发 per_client 与 per_listen 的锁路径。
        let peer = SocketAddr::from(([127, 0, 0, (n % 250 + 1) as u8], (n % 60_000 + 1) as u16));
        let local = SocketAddr::from(([127, 0, 0, 1], 5700u16));
        let (guard, _token) = reg.register(peer, local, &parent);
        guard.set_protocol(Protocol::Socks5);
        guard.set_state(ConnState::Relaying);
        guard.set_upstream((n % 4) as usize);
        guard.add_bytes(true, 1024);
        guard.add_bytes(false, 2048);
        drop(guard);
    });
    bench(label, threads, iters, f);
}

fn main() {
    let mut args = std::env::args().skip(1);
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let threads: usize = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(cores)
        .max(1);
    let iters: u64 = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000)
        .max(1);

    println!("conn_bench: threads={threads} iters/thread={iters} cores={cores}");
    println!("（A/B/C = 每连接完整生命周期；D = 仅子令牌；E = 全局原子对照）");

    bench_registry(
        "A) register+unregister detail=true",
        true,
        true,
        threads,
        iters,
    );
    bench_registry(
        "B) register+unregister detail=false",
        true,
        false,
        threads,
        iters,
    );
    bench_registry(
        "C) register+unregister enabled=false",
        false,
        true,
        threads,
        iters,
    );

    let parent = CancellationToken::new();
    let token_f = Arc::new(move |_n: u64| {
        let child = parent.child_token();
        drop(child);
    });
    bench("D) child_token + drop", threads, iters, token_f);

    let stats = Stats::new();
    let stats_f = Arc::new(move |_n: u64| {
        stats.bytes_down_to_up.fetch_add(1024, Ordering::Relaxed);
        stats.bytes_up_to_down.fetch_add(2048, Ordering::Relaxed);
        stats.active.fetch_add(1, Ordering::Relaxed);
        stats.active.fetch_sub(1, Ordering::Relaxed);
    });
    bench("E) Stats 全局原子 x4", threads, iters, stats_f);

    // F) 消息架构：有界 mpsc 的 try_send（含单消费者 drain，避免队列满即丢弃）。
    let (tx, mut rx) = tokio::sync::mpsc::channel::<u64>(65_536);
    let consumer = std::thread::spawn(move || {
        let mut n = 0u64;
        while rx.blocking_recv().is_some() {
            n += 1;
        }
        n
    });
    bench(
        "F) mpsc try_send (64K)",
        threads,
        iters,
        Arc::new(move |n: u64| {
            let _ = tx.try_send(n);
        }),
    );
    let _ = consumer; // tx 已在 bench 返回时释放，消费者线程自行退出

    // G) A1 原型：只换数据结构（不改架构）时的上限估算。
    struct ProtoA1 {
        listen: AtomicU64,
        client: Vec<Mutex<HashMap<u8, u64>>>,
        active: Vec<AtomicU64>,
        next_id: AtomicU64,
        shards: Vec<Mutex<HashMap<u64, u64>>>,
    }
    impl ProtoA1 {
        fn new() -> Self {
            Self {
                listen: AtomicU64::new(0),
                client: (0..64).map(|_| Mutex::new(HashMap::new())).collect(),
                active: (0..32).map(|_| AtomicU64::new(0)).collect(),
                next_id: AtomicU64::new(1),
                shards: (0..16).map(|_| Mutex::new(HashMap::new())).collect(),
            }
        }
        fn register(&self, ip: u8) {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            self.listen.fetch_add(1, Ordering::Relaxed);
            {
                let mut m = self.client[(ip as usize) % 64]
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *m.entry(ip).or_insert(0) += 1;
            }
            self.active[(id as usize) % 32].fetch_add(1, Ordering::Relaxed);
            {
                let mut s = self.shards[(id as usize) % 16]
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                s.insert(id, 0);
            }
        }
        fn unregister(&self, ip: u8, id: u64) {
            self.listen.fetch_sub(1, Ordering::Relaxed);
            {
                let mut m = self.client[(ip as usize) % 64]
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(v) = m.get_mut(&ip) {
                    *v = v.saturating_sub(1);
                    if *v == 0 {
                        m.remove(&ip);
                    }
                }
            }
            self.active[(id as usize) % 32].fetch_sub(1, Ordering::Relaxed);
            let mut s = self.shards[(id as usize) % 16]
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            s.remove(&id);
        }
    }
    let proto = Arc::new(ProtoA1::new());
    let proto_f = Arc::clone(&proto);
    bench(
        "G) A1 原型（原子/分片，无令牌）",
        threads,
        iters,
        Arc::new(move |n: u64| {
            let ip = (n % 250 + 1) as u8;
            proto_f.register(ip);
            proto_f.unregister(ip, n + 1);
        }),
    );
}
