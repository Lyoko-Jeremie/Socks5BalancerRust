//! 上游健康检查（M1.4）。
//!
//! 设计严格对齐 docs/07 ADR-002：
//! - **fail-fast**：一次探测失败即判 DOWN，不做失败计数/重试/防抖；
//! - **快速切换**：DOWN 的上游立即从选择池中排除（由 `is_available` 回调提供）；
//! - **避免频繁打扰**：DOWN 后按指数退避安排重探（initial→×multiplier→cap max，带抖动），
//!   给后端留出自恢复时间；
//! - **数据面失败立即降级**：连接/握手失败调用 [`HealthRegistry::mark_degraded`]，
//!   1 秒后快速重探确认，探测成功即恢复。
//!
//! 每个上游一个探测任务，互不阻塞；探测只做 TCP connect（M1.5 再加 TLS + HTTP readiness）。

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::HealthConfig;
use crate::upstream::Upstream;

/// 数据面失败后的快速重探延迟。
const QUICK_RECHECK_MS: u64 = 1_000;

/// 单个上游的健康状态（全部原子，热路径只读）。
#[derive(Debug)]
struct Entry {
    name: String,
    addr: SocketAddr,
    /// 是否可用；启动时乐观为 true，首个探测校正。
    available: AtomicBool,
    /// 下次探测时间（相对 registry 启动的毫秒）。
    next_probe_ms: AtomicU64,
    /// 当前退避时长（毫秒）。
    backoff_ms: AtomicU64,
    /// 连续失败次数（仅用于日志/快照）。
    failures: AtomicU64,
}

/// 健康快照（控制面/日志用；M3 会序列化为 JSON）。
#[derive(Debug, Clone)]
pub struct HealthSnapshot {
    pub name: String,
    pub addr: SocketAddr,
    pub available: bool,
    pub failures: u64,
    pub backoff_ms: u64,
}

/// 上游健康注册表。
#[derive(Debug)]
pub struct HealthRegistry {
    cfg: HealthConfig,
    started: Instant,
    entries: Vec<Entry>,
}

impl HealthRegistry {
    pub fn new(upstreams: &[Upstream], cfg: HealthConfig) -> Arc<Self> {
        let initial_backoff = cfg.backoff_initial.as_millis() as u64;
        let entries = upstreams
            .iter()
            .map(|u| Entry {
                name: u.name.clone(),
                addr: u.addr,
                available: AtomicBool::new(true),
                next_probe_ms: AtomicU64::new(0),
                backoff_ms: AtomicU64::new(initial_backoff.max(1)),
                failures: AtomicU64::new(0),
            })
            .collect();
        Arc::new(Self {
            cfg,
            started: Instant::now(),
            entries,
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 与 [`Self::len`] 配对（clippy `len_without_is_empty`）；上游池启动时保证非空。
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn cfg(&self) -> &HealthConfig {
        &self.cfg
    }

    /// 上游当前是否可用（供选择器回调使用）。
    pub fn is_available(&self, idx: usize) -> bool {
        self.entries
            .get(idx)
            .map(|e| e.available.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    /// 当前可用上游数量。
    pub fn available_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.available.load(Ordering::Relaxed))
            .count()
    }

    pub fn snapshot(&self) -> Vec<HealthSnapshot> {
        self.entries
            .iter()
            .map(|e| HealthSnapshot {
                name: e.name.clone(),
                addr: e.addr,
                available: e.available.load(Ordering::Relaxed),
                failures: e.failures.load(Ordering::Relaxed),
                backoff_ms: e.backoff_ms.load(Ordering::Relaxed),
            })
            .collect()
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// 数据面失败：立即标记不可用并安排 1s 后快速重探。
    ///
    /// 这是"快速切换"的关键：失败连接的下一个请求不会再被分配到该上游。
    pub fn mark_degraded(&self, idx: usize, reason: &str) {
        let Some(entry) = self.entries.get(idx) else {
            return;
        };
        let was_available = entry.available.swap(false, Ordering::AcqRel);
        entry.failures.fetch_add(1, Ordering::Relaxed);
        entry.next_probe_ms.store(
            self.now_ms().saturating_add(QUICK_RECHECK_MS),
            Ordering::Release,
        );
        if was_available {
            warn!(
                upstream = %entry.name,
                addr = %entry.addr,
                reason,
                "数据面失败：标记为不可用，1s 后快速重探"
            );
        } else {
            debug!(
                upstream = %entry.name,
                addr = %entry.addr,
                reason,
                "数据面再次失败（已处于不可用状态）"
            );
        }
    }

    /// 记录一次探测结果。
    fn record_probe(&self, idx: usize, ok: bool) {
        let Some(entry) = self.entries.get(idx) else {
            return;
        };
        if ok {
            let was_available = entry.available.swap(true, Ordering::AcqRel);
            entry.failures.store(0, Ordering::Relaxed);
            entry.backoff_ms.store(
                self.cfg.backoff_initial.as_millis() as u64,
                Ordering::Relaxed,
            );
            let next = self.now_ms().saturating_add(jittered(
                self.cfg.period.as_millis() as u64,
                self.cfg.probe_jitter,
            ));
            entry.next_probe_ms.store(next, Ordering::Release);
            if !was_available {
                info!(upstream = %entry.name, addr = %entry.addr, "上游恢复可用");
            } else {
                debug!(upstream = %entry.name, "上游探活成功");
            }
            return;
        }

        // fail-fast：一次失败立即 DOWN
        let was_available = entry.available.swap(false, Ordering::AcqRel);
        let failures = entry.failures.fetch_add(1, Ordering::Relaxed) + 1;
        let current_backoff = entry.backoff_ms.load(Ordering::Relaxed).max(1);
        // 按"当前退避"等待，然后增长（cap 到 max）
        let wait_ms = jittered(current_backoff, self.cfg.backoff_jitter);
        let next_backoff = ((current_backoff as f64) * self.cfg.backoff_multiplier)
            .min(self.cfg.backoff_max.as_millis() as f64)
            .max(1.0) as u64;
        entry.backoff_ms.store(next_backoff, Ordering::Relaxed);
        entry
            .next_probe_ms
            .store(self.now_ms().saturating_add(wait_ms), Ordering::Release);

        if was_available {
            warn!(
                upstream = %entry.name,
                addr = %entry.addr,
                wait_ms,
                next_backoff_ms = next_backoff,
                "上游探活失败：标记 DOWN（fail-fast），按退避延迟重探"
            );
        } else {
            debug!(
                upstream = %entry.name,
                failures,
                wait_ms,
                next_backoff_ms = next_backoff,
                "上游仍不可用"
            );
        }
    }

    /// 为每个上游启动独立探测任务（随 `cancel` 退出）。
    pub fn spawn_probes(self: &Arc<Self>, cancel: CancellationToken) {
        for idx in 0..self.entries.len() {
            let reg = Arc::clone(self);
            let cancel = cancel.clone();
            tokio::spawn(async move {
                let start_delay =
                    reg.cfg
                        .start_delay
                        .saturating_add(Duration::from_millis(jittered(
                            reg.cfg.period.as_millis() as u64 / 4,
                            reg.cfg.probe_jitter,
                        )));
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(start_delay) => {}
                }

                loop {
                    // 等到下次探测时间（DOWN 期间可能是很长的退避）
                    loop {
                        if cancel.is_cancelled() {
                            return;
                        }
                        let now = reg.now_ms();
                        let next = reg.entries[idx].next_probe_ms.load(Ordering::Acquire);
                        if now >= next {
                            break;
                        }
                        let wait = Duration::from_millis(next - now);
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            _ = tokio::time::sleep(wait) => {}
                        }
                    }

                    let addr = reg.entries[idx].addr;
                    let ok = matches!(
                        tokio::time::timeout(reg.cfg.timeout, TcpStream::connect(addr)).await,
                        Ok(Ok(_))
                    );
                    reg.record_probe(idx, ok);
                }
            });
        }
    }
}

/// 对 `base_ms` 施加 ±ratio 的随机抖动（ratio 取 0..=1）。
fn jittered(base_ms: u64, ratio: f64) -> u64 {
    if ratio <= 0.0 || base_ms == 0 {
        return base_ms;
    }
    let factor = 1.0 + (fastrand::f64() * 2.0 - 1.0) * ratio;
    ((base_ms as f64) * factor).max(1.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn cfg() -> HealthConfig {
        HealthConfig {
            period: Duration::from_millis(50),
            timeout: Duration::from_millis(20),
            start_delay: Duration::from_millis(0),
            backoff_initial: Duration::from_millis(100),
            backoff_multiplier: 2.0,
            backoff_max: Duration::from_millis(500),
            probe_jitter: 0.0,
            backoff_jitter: 0.0,
        }
    }

    fn upstream(name: &str, port: u16) -> Upstream {
        Upstream {
            name: name.to_string(),
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            auth_user: None,
            auth_pwd: None,
            health_index: 0,
        }
    }

    #[test]
    fn 启动时乐观可用() {
        let reg = HealthRegistry::new(&[upstream("a", 1)], cfg());
        assert!(reg.is_available(0));
        assert_eq!(reg.available_count(), 1);
    }

    #[test]
    fn fail_fast_一次失败立即_down() {
        let reg = HealthRegistry::new(&[upstream("a", 1)], cfg());
        reg.record_probe(0, false);
        assert!(!reg.is_available(0));
        assert_eq!(reg.available_count(), 0);
        assert_eq!(reg.snapshot()[0].failures, 1);
    }

    #[test]
    fn 失败后指数退避并封顶() {
        let reg = HealthRegistry::new(&[upstream("a", 1)], cfg());
        // 第一次失败：等待 100ms，退避 → 200ms
        reg.record_probe(0, false);
        assert_eq!(reg.snapshot()[0].backoff_ms, 200);
        // 连续失败：200 → 400 → 500(cap)
        reg.record_probe(0, false);
        assert_eq!(reg.snapshot()[0].backoff_ms, 400);
        reg.record_probe(0, false);
        assert_eq!(reg.snapshot()[0].backoff_ms, 500);
        reg.record_probe(0, false);
        assert_eq!(reg.snapshot()[0].backoff_ms, 500);
    }

    #[test]
    fn 探测成功恢复并重置退避() {
        let reg = HealthRegistry::new(&[upstream("a", 1)], cfg());
        reg.record_probe(0, false);
        reg.record_probe(0, false);
        assert!(!reg.is_available(0));
        reg.record_probe(0, true);
        assert!(reg.is_available(0));
        assert_eq!(reg.snapshot()[0].failures, 0);
        assert_eq!(reg.snapshot()[0].backoff_ms, 100);
    }

    #[test]
    fn 数据面降级立即不可用并安排快速重探() {
        let reg = HealthRegistry::new(&[upstream("a", 1)], cfg());
        let before = reg.now_ms();
        reg.mark_degraded(0, "connect failed");
        assert!(!reg.is_available(0));
        let next = reg.entries[0].next_probe_ms.load(Ordering::Relaxed);
        assert!(
            next >= before + QUICK_RECHECK_MS,
            "next={next} before={before}"
        );
        // 退避不变（保持 initial），快速重探失败后再进入退避
        assert_eq!(reg.snapshot()[0].backoff_ms, 100);
    }

    #[test]
    fn 越界索引安全() {
        let reg = HealthRegistry::new(&[upstream("a", 1)], cfg());
        assert!(!reg.is_available(9));
        reg.mark_degraded(9, "out of range"); // 不 panic
    }

    #[tokio::test]
    async fn 探测任务能感知上游起落() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let up = Upstream {
            name: "probe-target".to_string(),
            addr,
            auth_user: None,
            auth_pwd: None,
            health_index: 0,
        };
        let reg = HealthRegistry::new(&[up], cfg());
        let cancel = CancellationToken::new();
        reg.spawn_probes(cancel.clone());

        // 首个探测成功后应保持可用
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(reg.is_available(0), "监听中应可用");

        // 关闭监听 → 探测失败 → DOWN
        drop(listener);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!reg.is_available(0), "监听关闭后应 DOWN");

        // 重新监听同一地址 → 退避后恢复（test cfg 退避 100ms 起）
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(reg.is_available(0), "重新监听后应恢复可用");
        cancel.cancel();
        drop(listener);
    }
}
