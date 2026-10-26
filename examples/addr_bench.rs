//! 临时微基准（不提交）：上游 addr 字段几种存储方案的读取成本对比。
//!
//! 用法：`cargo run --release --example addr_bench -- [threads] [iters]`
use std::hint::black_box;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

fn sock(port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), port))
}

fn bench<F: Fn(u64) -> u64 + Send + Sync + 'static>(
    label: &str,
    threads: usize,
    iters: u64,
    f: Arc<F>,
) {
    let start = Instant::now();
    let mut hs = Vec::new();
    for _ in 0..threads {
        let f = Arc::clone(&f);
        hs.push(std::thread::spawn(move || {
            let mut acc = 0u64;
            for i in 0..iters {
                acc = acc.wrapping_add(f(i));
            }
            acc
        }));
    }
    let mut sum = 0u64;
    for h in hs {
        sum = sum.wrapping_add(h.join().unwrap());
    }
    let elapsed = start.elapsed();
    let total = (threads as u64 * iters) as f64;
    let secs = elapsed.as_secs_f64();
    println!(
        "{label:<38} chk={sum:<3} {total:>10.0} ops {secs:>8.3}s {:>12.0} ops/s {:>7.1} ns/op",
        total / secs.max(1e-9),
        secs * 1e9 / total
    );
}

fn main() {
    let threads: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
        .max(1);
    let iters: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000_000);
    println!("addr_bench: threads={threads} iters={iters}（只读场景；混合写另测）");

    // 1) 现状：Arc<Mutex<SocketAddr>> + 读
    let m = Arc::new(Mutex::new(sock(1080)));
    let m2 = Arc::clone(&m);
    bench(
        "1) Arc<Mutex<SocketAddr>> 读",
        threads,
        iters,
        Arc::new(move |_i: u64| {
            let a = *m2.lock().unwrap_or_else(|e| e.into_inner());
            a.port() as u64
        }),
    );

    // 2) RwLock 读
    let r = Arc::new(RwLock::new(sock(1080)));
    let r2 = Arc::clone(&r);
    bench(
        "2) Arc<RwLock<SocketAddr>> 读",
        threads,
        iters,
        Arc::new(move |_i: u64| {
            let a = *r2.read().unwrap_or_else(|e| e.into_inner());
            a.port() as u64
        }),
    );

    // 3) AtomicU64（IPv4 地址<<16 | 端口）load
    let packed = (u32::from_be_bytes([127, 0, 0, 1]) as u64) << 16 | 1080;
    let a = Arc::new(AtomicU64::new(packed));
    let a2 = Arc::clone(&a);
    bench(
        "3) AtomicU64 打包 IPv4 load",
        threads,
        iters,
        Arc::new(move |_i: u64| black_box(a2.load(Ordering::Relaxed)) & 0xFFFF),
    );

    // 4) 混合读写：1/1000 次写（模拟 DNS 刷新与读交错）
    let mw = Arc::clone(&m);
    bench(
        "4) Mutex 混合 999 读 : 1 写",
        threads,
        iters / 1000,
        Arc::new(move |i: u64| {
            let mut acc = 0u64;
            for _ in 0..999 {
                acc = acc.wrapping_add(mw.lock().unwrap_or_else(|e| e.into_inner()).port() as u64);
            }
            *mw.lock().unwrap_or_else(|e| e.into_inner()) = sock(2000 + (i % 500) as u16);
            acc
        }),
    );

    let rw = Arc::clone(&r);
    bench(
        "5) RwLock 混合 999 读 : 1 写",
        threads,
        iters / 1000,
        Arc::new(move |i: u64| {
            let mut acc = 0u64;
            for _ in 0..999 {
                acc = acc.wrapping_add(rw.read().unwrap_or_else(|e| e.into_inner()).port() as u64);
            }
            let _ = rw.write().map(|mut g| *g = sock(2000 + (i % 500) as u16));
            acc
        }),
    );
}
