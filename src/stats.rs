//! 极简运行统计（M0 版）。
//!
//! 设计遵循 ADR-003 / 02-relay-core §8.3：热路径只用原子操作，不做全局锁；
//! 1 秒级聚合与 JSON 快照留给 M3（控制面）实现，这里先提供累计值与连接数。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// 进程级统计。所有字段用 Relaxed 原子，避免热路径上的内存序开销。
#[derive(Debug)]
pub struct Stats {
    pub started_at: Instant,
    /// 成功 accept 并进入处理的连接总数（含后续建上游失败的）。
    pub accepted: AtomicU64,
    /// 因达到连接上限 / 无可用上游而被拒绝的连接数。
    pub rejected: AtomicU64,
    /// 当前活跃连接数。
    pub active: AtomicU64,
    /// 下游 → 上游的字节数。
    pub bytes_down_to_up: AtomicU64,
    /// 上游 → 下游的字节数。
    pub bytes_up_to_down: AtomicU64,
    /// 连接上游失败次数。
    pub connect_failed: AtomicU64,
    /// relay 过程中的 I/O 错误次数。
    pub relay_errors: AtomicU64,
}

impl Stats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            started_at: Instant::now(),
            accepted: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            active: AtomicU64::new(0),
            bytes_down_to_up: AtomicU64::new(0),
            bytes_up_to_down: AtomicU64::new(0),
            connect_failed: AtomicU64::new(0),
            relay_errors: AtomicU64::new(0),
        })
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            uptime_secs: self.started_at.elapsed().as_secs(),
            accepted: self.accepted.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            active: self.active.load(Ordering::Relaxed),
            bytes_down_to_up: self.bytes_down_to_up.load(Ordering::Relaxed),
            bytes_up_to_down: self.bytes_up_to_down.load(Ordering::Relaxed),
            connect_failed: self.connect_failed.load(Ordering::Relaxed),
            relay_errors: self.relay_errors.load(Ordering::Relaxed),
        }
    }
}

/// 统计快照（可安全跨任务传递/序列化）。
#[derive(Debug, Clone, Copy, Default)]
pub struct StatsSnapshot {
    pub uptime_secs: u64,
    pub accepted: u64,
    pub rejected: u64,
    pub active: u64,
    pub bytes_down_to_up: u64,
    pub bytes_up_to_down: u64,
    pub connect_failed: u64,
    pub relay_errors: u64,
}

impl StatsSnapshot {
    /// 人类可读的单行摘要（日志用）。
    pub fn summary(&self) -> String {
        format!(
            "uptime={}s accepted={} active={} rejected={} connect_failed={} up={}B down={}B relay_err={}",
            self.uptime_secs,
            self.accepted,
            self.active,
            self.rejected,
            self.connect_failed,
            self.bytes_down_to_up,
            self.bytes_up_to_down,
            self.relay_errors
        )
    }
}

/// 活跃连接计数守卫：创建时 CAS 占用一个名额，Drop 时归还。
///
/// 用 CAS 而不是 "先读再 +1"，避免高并发 accept 下超过 `max_connections`。
pub struct ActiveGuard {
    stats: Arc<Stats>,
}

impl ActiveGuard {
    /// 尝试占用一个活跃连接名额；超过上限返回 None。
    pub fn try_acquire(stats: &Arc<Stats>, max_connections: usize) -> Option<Self> {
        let max = max_connections as u64;
        let mut cur = stats.active.load(Ordering::Relaxed);
        loop {
            if cur >= max {
                return None;
            }
            match stats.active.compare_exchange_weak(
                cur,
                cur + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(Self {
                        stats: Arc::clone(stats),
                    });
                }
                Err(next) => cur = next,
            }
        }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.stats.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 活跃连接上限生效() {
        let stats = Stats::new();
        let g1 = ActiveGuard::try_acquire(&stats, 2).unwrap();
        let _g2 = ActiveGuard::try_acquire(&stats, 2).unwrap();
        assert!(ActiveGuard::try_acquire(&stats, 2).is_none());
        assert_eq!(stats.active.load(Ordering::Relaxed), 2);
        drop(g1);
        assert!(ActiveGuard::try_acquire(&stats, 2).is_some());
    }

    #[test]
    fn 快照字段自洽() {
        let stats = Stats::new();
        stats.accepted.store(3, Ordering::Relaxed);
        stats.bytes_down_to_up.store(10, Ordering::Relaxed);
        let s = stats.snapshot();
        assert_eq!(s.accepted, 3);
        assert_eq!(s.bytes_down_to_up, 10);
        assert!(s.summary().contains("accepted=3"));
    }
}
