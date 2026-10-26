//! 上游健康检查（M1.4 TCP liveness + M1.5 TLS/HTTP readiness）。
//!
//! 设计严格对齐 docs/07 ADR-002 与 ADR-006：
//! - **fail-fast**：一次探测失败即判 DOWN，不做失败计数/重试/防抖；
//! - **快速切换**：不可用的上游立即从选择池中排除；
//! - **避免频繁打扰**：DOWN 后按指数退避安排重探（initial→×multiplier→cap max，带抖动），
//!   给后端留出自恢复时间；
//! - **数据面失败立即降级**：连接/握手 I/O 失败调用 [`HealthRegistry::mark_degraded`]，
//!   1 秒后快速重探确认；
//! - **liveness 与 readiness 同时满足才可用**：TCP 探活 + （可选）TLS/HTTP 探测都通过；
//! - 探测并发受 `maxProbeConcurrency` 限制（信号量）。

pub mod tls_probe;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::sync::Semaphore;
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
    auth_user: Option<String>,
    auth_pwd: Option<String>,
    /// TCP liveness。
    liveness: AtomicBool,
    /// TLS/HTTP readiness；未启用 readiness 探测时恒 true。
    readiness: AtomicBool,
    /// TCP 下次探测时间（相对 registry 启动的毫秒）。
    tcp_next_probe_ms: AtomicU64,
    /// TCP 当前退避时长（毫秒）。
    tcp_backoff_ms: AtomicU64,
    /// TCP 连续失败次数。
    tcp_failures: AtomicU64,
    /// readiness 下次探测时间。
    tls_next_probe_ms: AtomicU64,
    /// readiness 当前退避时长。
    tls_backoff_ms: AtomicU64,
    /// readiness 连续失败次数。
    tls_failures: AtomicU64,
    /// 探测提前唤醒（控制面 `check_upstream`）。
    probe_notify: tokio::sync::Notify,
}

/// 健康快照（控制面/日志用；M3 会序列化为 JSON）。
#[derive(Debug, Clone)]
pub struct HealthSnapshot {
    pub name: String,
    pub addr: SocketAddr,
    /// liveness && readiness。
    pub available: bool,
    pub liveness: bool,
    pub readiness: bool,
    /// TCP 连续失败次数。
    pub failures: u64,
    /// readiness 连续失败次数。
    pub tls_failures: u64,
    /// TCP 当前退避。
    pub backoff_ms: u64,
}

/// 上游健康注册表。
#[derive(Debug)]
pub struct HealthRegistry {
    cfg: HealthConfig,
    started: Instant,
    entries: Vec<Entry>,
    /// readiness 探测并发上限。
    probe_permits: Arc<Semaphore>,
}

impl HealthRegistry {
    pub fn new(upstreams: &[Upstream], cfg: HealthConfig) -> Arc<Self> {
        let initial_backoff = cfg.backoff_initial.as_millis() as u64;
        let entries = upstreams
            .iter()
            .map(|u| Entry {
                name: u.name.clone(),
                addr: u.addr,
                auth_user: u.auth_user.clone(),
                auth_pwd: u.auth_pwd.clone(),
                liveness: AtomicBool::new(true),
                readiness: AtomicBool::new(true),
                tcp_next_probe_ms: AtomicU64::new(0),
                tcp_backoff_ms: AtomicU64::new(initial_backoff.max(1)),
                tcp_failures: AtomicU64::new(0),
                tls_next_probe_ms: AtomicU64::new(0),
                tls_backoff_ms: AtomicU64::new(initial_backoff.max(1)),
                tls_failures: AtomicU64::new(0),
                probe_notify: tokio::sync::Notify::new(),
            })
            .collect();
        let permits = cfg
            .tls
            .as_ref()
            .map(|t| t.max_concurrency.max(1))
            .unwrap_or(1);
        Arc::new(Self {
            cfg,
            started: Instant::now(),
            entries,
            probe_permits: Arc::new(Semaphore::new(permits)),
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

    /// 健康检查是否启用（`disableConnectTest=true` 时为 false）。
    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// 上游当前是否可用 = liveness && readiness（供选择器回调使用）。
    pub fn is_available(&self, idx: usize) -> bool {
        self.entries
            .get(idx)
            .map(|e| e.liveness.load(Ordering::Relaxed) && e.readiness.load(Ordering::Relaxed))
            .unwrap_or(false)
    }

    /// 当前可用上游数量。
    pub fn available_count(&self) -> usize {
        (0..self.entries.len())
            .filter(|i| self.is_available(*i))
            .count()
    }

    pub fn snapshot(&self) -> Vec<HealthSnapshot> {
        self.entries
            .iter()
            .map(|e| {
                let liveness = e.liveness.load(Ordering::Relaxed);
                let readiness = e.readiness.load(Ordering::Relaxed);
                HealthSnapshot {
                    name: e.name.clone(),
                    addr: e.addr,
                    available: liveness && readiness,
                    liveness,
                    readiness,
                    failures: e.tcp_failures.load(Ordering::Relaxed),
                    tls_failures: e.tls_failures.load(Ordering::Relaxed),
                    backoff_ms: e.tcp_backoff_ms.load(Ordering::Relaxed),
                }
            })
            .collect()
    }

    /// 立即安排一次探测（M3 控制面 `check_upstream`）：只探测一次、不重试。
    ///
    /// 通过 `Notify` 唤醒正在等待下次探测的探测任务；返回 false 表示下标越界
    /// 或健康检查已被 `disableConnectTest` 禁用。
    pub fn probe_now(&self, idx: usize) -> bool {
        if !self.cfg.enabled {
            return false;
        }
        let Some(entry) = self.entries.get(idx) else {
            return false;
        };
        let now = self.now_ms();
        entry.tcp_next_probe_ms.store(now, Ordering::Release);
        entry.tls_next_probe_ms.store(now, Ordering::Release);
        entry.probe_notify.notify_one();
        true
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// 数据面失败：立即标记不可用并安排 1s 后快速重探。
    ///
    /// 健康检查被禁用（`disableConnectTest=true`）时不做任何标记：
    /// 此时上游可用性完全由人工启停决定，避免"失败后再无探测恢复"的永久 DOWN。
    pub fn mark_degraded(&self, idx: usize, reason: &str) {
        if !self.cfg.enabled {
            return;
        }
        let Some(entry) = self.entries.get(idx) else {
            return;
        };
        let was_available = self.is_available(idx);
        entry.liveness.store(false, Ordering::Release);
        entry.readiness.store(false, Ordering::Release);
        entry.tcp_failures.fetch_add(1, Ordering::Relaxed);
        let quick = self.now_ms().saturating_add(QUICK_RECHECK_MS);
        entry.tcp_next_probe_ms.store(quick, Ordering::Release);
        entry.tls_next_probe_ms.store(quick, Ordering::Release);
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

    /// 记录一次 TCP liveness 探测结果。
    fn record_tcp(&self, idx: usize, ok: bool) {
        let Some(entry) = self.entries.get(idx) else {
            return;
        };
        if ok {
            let was_available = self.is_available(idx);
            entry.liveness.store(true, Ordering::Release);
            // 未启用 readiness 探测时，readiness 不适用，视为通过
            if self.cfg.tls.is_none() {
                entry.readiness.store(true, Ordering::Release);
            }
            entry.tcp_failures.store(0, Ordering::Relaxed);
            entry.tcp_backoff_ms.store(
                self.cfg.backoff_initial.as_millis() as u64,
                Ordering::Relaxed,
            );
            let next = self.now_ms().saturating_add(jittered(
                self.cfg.period.as_millis() as u64,
                self.cfg.probe_jitter,
            ));
            entry.tcp_next_probe_ms.store(next, Ordering::Release);
            if !was_available {
                info!(upstream = %entry.name, addr = %entry.addr, "上游 liveness 恢复");
            } else {
                debug!(upstream = %entry.name, "TCP 探活成功");
            }
            return;
        }

        // fail-fast：一次失败立即 DOWN（readiness 自然也不满足）
        let was_available = self.is_available(idx);
        entry.liveness.store(false, Ordering::Release);
        entry.readiness.store(false, Ordering::Release);
        let failures = entry.tcp_failures.fetch_add(1, Ordering::Relaxed) + 1;
        let current_backoff = entry.tcp_backoff_ms.load(Ordering::Relaxed).max(1);
        let wait_ms = jittered(current_backoff, self.cfg.backoff_jitter);
        let next_backoff = ((current_backoff as f64) * self.cfg.backoff_multiplier)
            .min(self.cfg.backoff_max.as_millis() as f64)
            .max(1.0) as u64;
        entry.tcp_backoff_ms.store(next_backoff, Ordering::Relaxed);
        entry
            .tcp_next_probe_ms
            .store(self.now_ms().saturating_add(wait_ms), Ordering::Release);

        if was_available {
            warn!(
                upstream = %entry.name,
                addr = %entry.addr,
                wait_ms,
                next_backoff_ms = next_backoff,
                "TCP 探活失败：标记 DOWN（fail-fast），按退避延迟重探"
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

    /// 记录一次 readiness（TLS + HTTP）探测结果。
    fn record_readiness(&self, idx: usize, ok: bool) {
        let Some(entry) = self.entries.get(idx) else {
            return;
        };
        if ok {
            let was_available = self.is_available(idx);
            entry.readiness.store(true, Ordering::Release);
            entry.tls_failures.store(0, Ordering::Relaxed);
            entry.tls_backoff_ms.store(
                self.cfg.backoff_initial.as_millis() as u64,
                Ordering::Relaxed,
            );
            let period = self
                .cfg
                .tls
                .as_ref()
                .map(|t| t.period.as_millis() as u64)
                .unwrap_or(60_000);
            let next = self
                .now_ms()
                .saturating_add(jittered(period, self.cfg.probe_jitter));
            entry.tls_next_probe_ms.store(next, Ordering::Release);
            if !was_available {
                info!(upstream = %entry.name, addr = %entry.addr, "上游 readiness 恢复");
            } else {
                debug!(upstream = %entry.name, "TLS/HTTP 探测成功");
            }
            return;
        }

        let was_available = self.is_available(idx);
        entry.readiness.store(false, Ordering::Release);
        let failures = entry.tls_failures.fetch_add(1, Ordering::Relaxed) + 1;
        let current_backoff = entry.tls_backoff_ms.load(Ordering::Relaxed).max(1);
        let wait_ms = jittered(current_backoff, self.cfg.backoff_jitter);
        let next_backoff = ((current_backoff as f64) * self.cfg.backoff_multiplier)
            .min(self.cfg.backoff_max.as_millis() as f64)
            .max(1.0) as u64;
        entry.tls_backoff_ms.store(next_backoff, Ordering::Relaxed);
        entry
            .tls_next_probe_ms
            .store(self.now_ms().saturating_add(wait_ms), Ordering::Release);

        if was_available {
            warn!(
                upstream = %entry.name,
                addr = %entry.addr,
                wait_ms,
                next_backoff_ms = next_backoff,
                "readiness 探测失败（TLS/HTTP）：标记不可用，按退避延迟重探"
            );
        } else {
            debug!(
                upstream = %entry.name,
                failures,
                wait_ms,
                "上游 readiness 仍不满足"
            );
        }
    }

    /// 为每个上游启动探测任务（随 `cancel` 退出）。
    pub fn spawn_probes(self: &Arc<Self>, cancel: CancellationToken) {
        if !self.cfg.enabled {
            info!("健康检查已禁用（disableConnectTest=true）：不启动探测任务");
            return;
        }
        for idx in 0..self.entries.len() {
            self.spawn_tcp_probe(idx, cancel.clone());
            if self.cfg.tls.is_some() {
                self.spawn_readiness_probe(idx, cancel.clone());
            }
        }
    }

    fn spawn_tcp_probe(self: &Arc<Self>, idx: usize, cancel: CancellationToken) {
        let reg = Arc::clone(self);
        tokio::spawn(async move {
            let start_delay = reg
                .cfg
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
                if !wait_until(&reg, idx, true, &cancel).await {
                    return;
                }
                let addr = reg.entries[idx].addr;
                let ok = matches!(
                    tokio::time::timeout(reg.cfg.timeout, TcpStream::connect(addr)).await,
                    Ok(Ok(_))
                );
                reg.record_tcp(idx, ok);
            }
        });
    }

    fn spawn_readiness_probe(self: &Arc<Self>, idx: usize, cancel: CancellationToken) {
        let Some(tls_cfg) = self.cfg.tls.clone() else {
            return;
        };
        let reg = Arc::clone(self);
        tokio::spawn(async move {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(tls_cfg.start_delay) => {}
            }
            loop {
                if !wait_until(&reg, idx, false, &cancel).await {
                    return;
                }
                if !reg.entries[idx].liveness.load(Ordering::Relaxed) {
                    // TCP 都不通，无需做 readiness 探测
                    reg.record_readiness(idx, false);
                    continue;
                }
                let upstream = Upstream {
                    name: reg.entries[idx].name.clone(),
                    host: reg.entries[idx].addr.ip().to_string(),
                    port: reg.entries[idx].addr.port(),
                    addr: reg.entries[idx].addr,
                    auth_user: reg.entries[idx].auth_user.clone(),
                    auth_pwd: reg.entries[idx].auth_pwd.clone(),
                    health_index: idx,
                    enabled: true,
                };
                let ok = {
                    let _permit = reg.probe_permits.acquire().await;
                    match tokio::time::timeout(
                        tls_cfg.timeout,
                        tls_probe::probe(&upstream, &tls_cfg),
                    )
                    .await
                    {
                        Ok(Ok(status)) => {
                            tls_probe::status_acceptable(status, &tls_cfg.expect_status)
                        }
                        Ok(Err(e)) => {
                            debug!(upstream = %upstream.name, error = %e, "readiness 探测失败");
                            false
                        }
                        Err(_) => {
                            debug!(
                                upstream = %upstream.name,
                                timeout_ms = tls_cfg.timeout.as_millis() as u64,
                                "readiness 探测超时"
                            );
                            false
                        }
                    }
                };
                reg.record_readiness(idx, ok);
            }
        });
    }
}

/// 等待某个上游的下次探测时间；返回 false 表示已取消。
async fn wait_until(
    reg: &Arc<HealthRegistry>,
    idx: usize,
    tcp: bool,
    cancel: &CancellationToken,
) -> bool {
    loop {
        if cancel.is_cancelled() {
            return false;
        }
        let now = reg.now_ms();
        let entry = &reg.entries[idx];
        let next = if tcp {
            entry.tcp_next_probe_ms.load(Ordering::Acquire)
        } else {
            entry.tls_next_probe_ms.load(Ordering::Acquire)
        };
        if now >= next {
            return true;
        }
        let wait = Duration::from_millis(next - now);
        tokio::select! {
            _ = cancel.cancelled() => return false,
            _ = tokio::time::sleep(wait) => {}
            // 控制面 `check_upstream`：提前唤醒，立即探测一次
            _ = entry.probe_notify.notified() => {}
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
            enabled: true,
            period: Duration::from_millis(50),
            timeout: Duration::from_millis(20),
            start_delay: Duration::from_millis(0),
            backoff_initial: Duration::from_millis(100),
            backoff_multiplier: 2.0,
            backoff_max: Duration::from_millis(500),
            probe_jitter: 0.0,
            backoff_jitter: 0.0,
            tls: None,
        }
    }

    fn upstream(name: &str, port: u16) -> Upstream {
        Upstream {
            name: name.to_string(),
            host: "127.0.0.1".to_string(),
            port,
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            auth_user: None,
            auth_pwd: None,
            health_index: 0,
            enabled: true,
        }
    }

    #[test]
    fn 启动时乐观可用() {
        let reg = HealthRegistry::new(&[upstream("a", 1)], cfg());
        assert!(reg.is_available(0));
        assert_eq!(reg.available_count(), 1);
    }

    #[test]
    fn 禁用健康检查时不标记降级() {
        let mut c = cfg();
        c.enabled = false;
        let reg = HealthRegistry::new(&[upstream("a", 1)], c);
        assert!(!reg.enabled());
        assert!(!reg.probe_now(0), "禁用时不应安排探测");
        reg.mark_degraded(0, "测试");
        assert!(
            reg.is_available(0),
            "禁用健康检查时数据面失败不应标记降级"
        );
    }

    #[test]
    fn fail_fast_一次失败立即_down() {
        let reg = HealthRegistry::new(&[upstream("a", 1)], cfg());
        reg.record_tcp(0, false);
        assert!(!reg.is_available(0));
        assert_eq!(reg.available_count(), 0);
        assert_eq!(reg.snapshot()[0].failures, 1);
        assert!(!reg.snapshot()[0].liveness);
    }

    #[test]
    fn 失败后指数退避并封顶() {
        let reg = HealthRegistry::new(&[upstream("a", 1)], cfg());
        reg.record_tcp(0, false);
        assert_eq!(reg.snapshot()[0].backoff_ms, 200);
        reg.record_tcp(0, false);
        assert_eq!(reg.snapshot()[0].backoff_ms, 400);
        reg.record_tcp(0, false);
        assert_eq!(reg.snapshot()[0].backoff_ms, 500);
        reg.record_tcp(0, false);
        assert_eq!(reg.snapshot()[0].backoff_ms, 500);
    }

    #[test]
    fn tcp_探测成功恢复并重置退避() {
        let reg = HealthRegistry::new(&[upstream("a", 1)], cfg());
        reg.record_tcp(0, false);
        reg.record_tcp(0, false);
        assert!(!reg.is_available(0));
        reg.record_tcp(0, true);
        assert!(reg.is_available(0));
        assert_eq!(reg.snapshot()[0].failures, 0);
        assert_eq!(reg.snapshot()[0].backoff_ms, 100);
    }

    #[test]
    fn readiness_失败导致不可用_liveness_仍为真() {
        let reg = HealthRegistry::new(&[upstream("a", 1)], cfg());
        reg.record_readiness(0, false);
        let s = reg.snapshot().into_iter().next().unwrap();
        assert!(!s.available);
        assert!(s.liveness);
        assert!(!s.readiness);
        assert_eq!(s.tls_failures, 1);
        // readiness 恢复 → 整体恢复
        reg.record_readiness(0, true);
        assert!(reg.is_available(0));
        assert_eq!(reg.snapshot()[0].tls_failures, 0);
    }

    #[test]
    fn 数据面降级立即不可用并安排快速重探() {
        let reg = HealthRegistry::new(&[upstream("a", 1)], cfg());
        let before = reg.now_ms();
        reg.mark_degraded(0, "connect failed");
        assert!(!reg.is_available(0));
        let next = reg.entries[0].tcp_next_probe_ms.load(Ordering::Relaxed);
        assert!(
            next >= before + QUICK_RECHECK_MS,
            "next={next} before={before}"
        );
        assert_eq!(reg.snapshot()[0].backoff_ms, 100);
    }

    #[test]
    fn 越界索引安全() {
        let reg = HealthRegistry::new(&[upstream("a", 1)], cfg());
        assert!(!reg.is_available(9));
        reg.mark_degraded(9, "out of range"); // 不 panic
    }

    #[tokio::test]
    async fn tcp_探测任务能感知上游起落() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let up = Upstream {
            name: "probe-target".to_string(),
            host: "127.0.0.1".to_string(),
            port: addr.port(),
            addr,
            auth_user: None,
            auth_pwd: None,
            health_index: 0,
            enabled: true,
        };
        let reg = HealthRegistry::new(&[up], cfg());
        let cancel = CancellationToken::new();
        reg.spawn_probes(cancel.clone());

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(reg.is_available(0), "监听中应可用");

        drop(listener);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(!reg.is_available(0), "监听关闭后应 DOWN");

        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(reg.is_available(0), "重新监听后应恢复可用");
        cancel.cancel();
        drop(listener);
    }
}
