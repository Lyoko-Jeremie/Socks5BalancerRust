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
use std::sync::Mutex;
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
    /// 上游地址（与 `Upstream` 共享，DNS TTL 刷新会原地更新）。
    addr: Arc<Mutex<SocketAddr>>,
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
    /// 最近一次探测的 RTT（毫秒；0 = 尚未测到）。
    last_rtt_ms: AtomicU64,
    /// 探测提前唤醒（控制面 `check_upstream`）。
    probe_notify: tokio::sync::Notify,
}

impl Entry {
    /// 读取当前上游地址（DNS TTL 刷新后可能变化）。
    fn addr(&self) -> SocketAddr {
        *self.addr.lock().unwrap_or_else(|e| e.into_inner())
    }
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
    /// 最近一次探测的 RTT（毫秒；0 = 尚未测到）。
    pub rtt_ms: u64,
}

/// 上游健康注册表。
#[derive(Debug)]
pub struct HealthRegistry {
    cfg: HealthConfig,
    started: Instant,
    entries: Vec<Entry>,
    /// 最近一次下游活动时间（相对 `started` 的毫秒；`sleepTime` 空闲暂停用）。
    last_activity_ms: AtomicU64,
    /// 新连接到来时通知探测任务从"空闲暂停"恢复。
    activity_notify: tokio::sync::Notify,
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
                addr: Arc::clone(&u.addr),
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
                last_rtt_ms: AtomicU64::new(0),
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
            last_activity_ms: AtomicU64::new(0),
            activity_notify: tokio::sync::Notify::new(),
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

    /// 记录一次下游活动（接入了新连接），唤醒处于 `sleepTime` 空闲暂停的探测任务。
    ///
    /// 用 `notify_one`（而不是 `notify_waiters`）：前者会把许可存下来，即使此刻没有任务
    /// 在等待，下一次 `notified()` 也会立刻返回，不会丢失唤醒；这样休眠循环里就不需要
    /// 再靠 1s 轮询兜底。
    pub fn note_activity(&self) {
        if self.cfg.sleep_time.is_none() {
            return;
        }
        self.last_activity_ms
            .store(self.now_ms(), Ordering::Relaxed);
        self.activity_notify.notify_one();
    }

    /// 是否已进入 `sleepTime` 空闲休眠（最近 `sleepTime` 内无任何新连接）。
    ///
    /// 休眠期间**所有自动探测都暂停**（TCP 周期探测、readiness 周期探测、addition 加速
    /// 探测），对齐 C++：C++ 的 tcp/connect/addition 三个定时器都以
    /// `now - lastConnectComeTime <= sleepTime` 为门控，超过即整轮跳过。
    ///
    /// 这样做的原因不是省流量，而是避免"长时间无人使用"时后端链路上持续出现单调、有规律
    /// 的探测流量，被链路状态检测/防火墙判为异常通信而把后端拉黑。
    /// 恢复条件：新连接到来（[`Self::note_activity`]）或控制面人工探测（[`Self::probe_now`]）。
    fn idle_sleeping(&self) -> bool {
        let Some(sleep) = self.cfg.sleep_time else {
            return false;
        };
        let sleep_ms = sleep.as_millis() as u64;
        if sleep_ms == 0 {
            return false;
        }
        let last = self.last_activity_ms.load(Ordering::Relaxed);
        self.now_ms().saturating_sub(last) >= sleep_ms
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
                    addr: e.addr(),
                    available: liveness && readiness,
                    liveness,
                    readiness,
                    failures: e.tcp_failures.load(Ordering::Relaxed),
                    tls_failures: e.tls_failures.load(Ordering::Relaxed),
                    backoff_ms: e.tcp_backoff_ms.load(Ordering::Relaxed),
                    rtt_ms: e.last_rtt_ms.load(Ordering::Relaxed),
                }
            })
            .collect()
    }

    /// 立即安排一次探测（M3 控制面 `check_upstream`）：只探测一次、不重试。
    ///
    /// 通过 `Notify` 唤醒正在等待下次探测的探测任务；返回 false 表示下标越界
    /// 或健康检查已被 `disableConnectTest` 禁用。
    ///
    /// 人工探测**视为一次下游活动**：会绕过 `sleepTime` 空闲休眠并唤醒休眠中的探测任务。
    pub fn probe_now(&self, idx: usize) -> bool {
        if !self.cfg.enabled {
            return false;
        }
        self.last_activity_ms
            .store(self.now_ms(), Ordering::Relaxed);
        self.activity_notify.notify_one();
        self.arm_probe(idx)
    }

    /// 为某个上游安排"立刻探测一次"（不刷新活动时间，因而不算下游活动）。
    ///
    /// `additionCheckPeriod` 加速探测用它：若这里也刷新 `last_activity_ms`，
    /// "全池 DOWN 且长时间无人使用"的场景会每 10s 被自己刷新一次，永远进不了
    /// `sleepTime` 休眠，后端就会被持续的规律探测流量打黑——正是要避免的情况。
    fn arm_probe(&self, idx: usize) -> bool {
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
        // `connectFailMarkDegraded=false`：数据面失败只累加 relayErrors（调用方负责），
        // 不改变可用性——可用性完全交给周期探测判定。
        if !self.cfg.connect_fail_mark_degraded {
            debug!(
                idx,
                reason, "数据面失败（connectFailMarkDegraded=false，不标记不可用）"
            );
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
                addr = %entry.addr(),
                reason,
                "数据面失败：标记为不可用，1s 后快速重探"
            );
        } else {
            debug!(
                upstream = %entry.name,
                addr = %entry.addr(),
                reason,
                "数据面再次失败（已处于不可用状态）"
            );
        }
    }

    /// 记录一次 TCP liveness 探测结果（`rtt_ms` 为连接耗时）。
    fn record_tcp(&self, idx: usize, ok: bool, rtt_ms: u64) {
        let Some(entry) = self.entries.get(idx) else {
            return;
        };
        if ok {
            let was_available = self.is_available(idx);
            entry.liveness.store(true, Ordering::Release);
            if rtt_ms > 0 {
                entry.last_rtt_ms.store(rtt_ms, Ordering::Relaxed);
            }
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
                info!(upstream = %entry.name, addr = %entry.addr(), "上游 liveness 恢复");
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
                addr = %entry.addr(),
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

    /// 记录一次 readiness（TLS + HTTP）探测结果（`rtt_ms` 为 HTTP 往返耗时）。
    fn record_readiness(&self, idx: usize, ok: bool, rtt_ms: u64) {
        let Some(entry) = self.entries.get(idx) else {
            return;
        };
        if ok {
            let was_available = self.is_available(idx);
            entry.readiness.store(true, Ordering::Release);
            if rtt_ms > 0 {
                entry.last_rtt_ms.store(rtt_ms, Ordering::Relaxed);
            }
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
                info!(upstream = %entry.name, addr = %entry.addr(), "上游 readiness 恢复");
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
                addr = %entry.addr(),
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
        self.spawn_addition_checker(cancel.clone());
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
                // sleepTime：空闲休眠期间暂停**全部**自动探测（对齐 C++：DOWN 后端的
                // 退避重探同样暂停），避免无人使用时仍在后端链路上留下有规律的探测流量。
                // 新连接（note_activity）或控制面人工探测（probe_now）会唤醒并恢复。
                if reg.idle_sleeping() {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = reg.activity_notify.notified() => continue,
                    }
                }
                let addr = reg.entries[idx].addr();
                let started = Instant::now();
                let ok = matches!(
                    tokio::time::timeout(reg.cfg.timeout, TcpStream::connect(addr)).await,
                    Ok(Ok(_))
                );
                let rtt_ms = if ok {
                    started.elapsed().as_millis() as u64
                } else {
                    0
                };
                reg.record_tcp(idx, ok, rtt_ms);
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
                // sleepTime：空闲休眠期间暂停 readiness 探测（与 TCP 探测一致）。
                if reg.idle_sleeping() {
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = reg.activity_notify.notified() => continue,
                    }
                }
                if !reg.entries[idx].liveness.load(Ordering::Relaxed) {
                    // TCP 都不通，无需做 readiness 探测
                    reg.record_readiness(idx, false, 0);
                    continue;
                }
                let current_addr = reg.entries[idx].addr();
                let upstream = Upstream {
                    name: reg.entries[idx].name.clone(),
                    host: current_addr.ip().to_string(),
                    port: current_addr.port(),
                    addr: Arc::clone(&reg.entries[idx].addr),
                    auth_user: reg.entries[idx].auth_user.clone(),
                    auth_pwd: reg.entries[idx].auth_pwd.clone(),
                    health_index: idx,
                    enabled: true,
                };
                let (ok, rtt_ms) = {
                    let _permit = reg.probe_permits.acquire().await;
                    match tokio::time::timeout(
                        tls_cfg.timeout,
                        tls_probe::probe(&upstream, &tls_cfg),
                    )
                    .await
                    {
                        Ok(Ok(outcome)) => (
                            tls_probe::status_acceptable(outcome.status, &tls_cfg.expect_status),
                            outcome.rtt.as_millis() as u64,
                        ),
                        Ok(Err(e)) => {
                            debug!(upstream = %upstream.name, error = %e, "readiness 探测失败");
                            (false, 0)
                        }
                        Err(_) => {
                            debug!(
                                upstream = %upstream.name,
                                timeout_ms = tls_cfg.timeout.as_millis() as u64,
                                "readiness 探测超时"
                            );
                            (false, 0)
                        }
                    }
                };
                reg.record_readiness(idx, ok, rtt_ms);
            }
        });
    }

    /// `additionCheckPeriod`：全池不可用且**不在空闲休眠中**时，按该周期额外触发一次全池探测。
    ///
    /// 对齐 C++ `UpstreamPool.cpp:419-423`：`isAllDown && now - lastConnectComeTime <= sleepTime`
    /// 才追加探测。休眠期间必须停——否则"全池 DOWN 且无人使用"会变成持续规律探测，
    /// 正好触发链路检测防火墙的黑名单。
    fn spawn_addition_checker(self: &Arc<Self>, cancel: CancellationToken) {
        let Some(period) = self.cfg.addition_check_period else {
            return;
        };
        let reg = Arc::clone(self);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            ticker.tick().await; // 跳过第一次立即触发（首轮在 period 之后，与 C++ 的延迟启动一致）
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = ticker.tick() => {
                        if !reg.idle_sleeping()
                            && !reg.entries.is_empty()
                            && reg.available_count() == 0
                        {
                            debug!(count = reg.entries.len(), "全池不可用：按 additionCheckPeriod 加速探测");
                            for i in 0..reg.entries.len() {
                                // 用 arm_probe 而不是 probe_now：加速探测不能刷新活动时间，
                                // 否则会自己阻止进入 sleepTime 休眠。
                                reg.arm_probe(i);
                            }
                        }
                    }
                }
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
            sleep_time: None,
            addition_check_period: None,
            connect_fail_mark_degraded: true,
        }
    }

    fn upstream(name: &str, port: u16) -> Upstream {
        Upstream {
            name: name.to_string(),
            host: "127.0.0.1".to_string(),
            port,
            addr: Arc::new(Mutex::new(SocketAddr::from(([127, 0, 0, 1], port)))),
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
        assert!(reg.is_available(0), "禁用健康检查时数据面失败不应标记降级");
    }

    #[test]
    fn connect_fail_mark_degraded_为_false_时数据面失败不降级() {
        let mut c = cfg();
        c.connect_fail_mark_degraded = false;
        let reg = HealthRegistry::new(&[upstream("a", 1)], c);
        reg.mark_degraded(0, "connect failed");
        assert!(
            reg.is_available(0),
            "connectFailMarkDegraded=false 时数据面失败不应改变可用性"
        );
        assert_eq!(reg.snapshot()[0].failures, 0, "也不应累加失败计数");
    }

    #[test]
    fn 加速探测不刷新活动时间因而不会阻止休眠() {
        let mut c = cfg();
        c.sleep_time = Some(Duration::from_millis(50));
        let reg = HealthRegistry::new(&[upstream("a", 1)], c);
        std::thread::sleep(Duration::from_millis(80));
        assert!(reg.idle_sleeping());
        // additionCheckPeriod 内部走 arm_probe：不应刷新 last_activity
        for i in 0..3 {
            let before = reg.last_activity_ms.load(Ordering::Relaxed);
            assert!(reg.arm_probe(0), "arm_probe 应成功（idx={i}）");
            assert_eq!(
                reg.last_activity_ms.load(Ordering::Relaxed),
                before,
                "加速探测不能刷新活动时间"
            );
        }
        assert!(reg.idle_sleeping(), "arm_probe 之后仍应处于休眠");
        // 人工探测则算活动，会退出休眠
        assert!(reg.probe_now(0));
        assert!(!reg.idle_sleeping(), "probe_now 应视为活动并退出休眠");
    }

    #[test]
    fn 探测结果记录_rtt() {
        let reg = HealthRegistry::new(&[upstream("a", 1)], cfg());
        reg.record_tcp(0, true, 12);
        assert_eq!(reg.snapshot()[0].rtt_ms, 12);
        reg.record_readiness(0, true, 34);
        assert_eq!(reg.snapshot()[0].rtt_ms, 34);
    }

    #[test]
    fn sleep_空闲窗口暂停探测() {
        let mut c = cfg();
        c.sleep_time = Some(Duration::from_millis(50));
        let reg = HealthRegistry::new(&[upstream("a", 1)], c);
        // 启动初期不算空闲（先让首轮探测跑起来）
        assert!(!reg.idle_sleeping());
        std::thread::sleep(Duration::from_millis(80));
        // 超过 sleepTime 无新连接 → 暂停常规探测
        assert!(reg.idle_sleeping());
        // 新连接到来 → 恢复
        reg.note_activity();
        assert!(!reg.idle_sleeping());
        // 人工探测也视为活动
        std::thread::sleep(Duration::from_millis(80));
        assert!(reg.idle_sleeping());
        assert!(reg.probe_now(0));
        assert!(!reg.idle_sleeping());
    }

    #[test]
    fn fail_fast_一次失败立即_down() {
        let reg = HealthRegistry::new(&[upstream("a", 1)], cfg());
        reg.record_tcp(0, false, 0);
        assert!(!reg.is_available(0));
        assert_eq!(reg.available_count(), 0);
        assert_eq!(reg.snapshot()[0].failures, 1);
        assert!(!reg.snapshot()[0].liveness);
    }

    #[test]
    fn 失败后指数退避并封顶() {
        let reg = HealthRegistry::new(&[upstream("a", 1)], cfg());
        reg.record_tcp(0, false, 0);
        assert_eq!(reg.snapshot()[0].backoff_ms, 200);
        reg.record_tcp(0, false, 0);
        assert_eq!(reg.snapshot()[0].backoff_ms, 400);
        reg.record_tcp(0, false, 0);
        assert_eq!(reg.snapshot()[0].backoff_ms, 500);
        reg.record_tcp(0, false, 0);
        assert_eq!(reg.snapshot()[0].backoff_ms, 500);
    }

    #[test]
    fn tcp_探测成功恢复并重置退避() {
        let reg = HealthRegistry::new(&[upstream("a", 1)], cfg());
        reg.record_tcp(0, false, 0);
        reg.record_tcp(0, false, 0);
        assert!(!reg.is_available(0));
        reg.record_tcp(0, true, 0);
        assert!(reg.is_available(0));
        assert_eq!(reg.snapshot()[0].failures, 0);
        assert_eq!(reg.snapshot()[0].backoff_ms, 100);
    }

    #[test]
    fn readiness_失败导致不可用_liveness_仍为真() {
        let reg = HealthRegistry::new(&[upstream("a", 1)], cfg());
        reg.record_readiness(0, false, 0);
        let s = reg.snapshot().into_iter().next().unwrap();
        assert!(!s.available);
        assert!(s.liveness);
        assert!(!s.readiness);
        assert_eq!(s.tls_failures, 1);
        // readiness 恢复 → 整体恢复
        reg.record_readiness(0, true, 0);
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
            addr: Arc::new(Mutex::new(addr)),
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

    /// 端到端验证 `sleepTime`：空闲后探测任务真的停了，新连接到来后恢复。
    ///
    /// 用 accept 计数观测探测：TCP 探活 = 一次 connect，目标 listener 每接受一次即 +1。
    #[tokio::test]
    async fn 休眠期间停止探测_新连接后恢复() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepts = Arc::new(AtomicU64::new(0));
        let acc = Arc::clone(&accepts);
        let acceptor = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((_s, _)) => {
                        acc.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => return,
                }
            }
        });

        let up = Upstream {
            name: "sleep-target".to_string(),
            host: "127.0.0.1".to_string(),
            port: addr.port(),
            addr: Arc::new(Mutex::new(addr)),
            auth_user: None,
            auth_pwd: None,
            health_index: 0,
            enabled: true,
        };
        let mut c = cfg();
        c.period = Duration::from_millis(50);
        c.timeout = Duration::from_millis(20);
        c.sleep_time = Some(Duration::from_millis(50));
        let reg = HealthRegistry::new(&[up], c);
        let cancel = CancellationToken::new();
        reg.spawn_probes(cancel.clone());

        // 启动后先跑探测，随后 50ms 无连接 → 进入休眠
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(reg.idle_sleeping(), "启动 120ms 后应已进入休眠");
        let a1 = accepts.load(Ordering::Relaxed);
        assert!(a1 >= 1, "启动后应至少探测过一次，实际 {a1}");

        // 休眠期：不应再有探测
        tokio::time::sleep(Duration::from_millis(300)).await;
        let a2 = accepts.load(Ordering::Relaxed);
        assert_eq!(a2, a1, "休眠期间不应继续探测（{a1} → {a2}）");

        // 新连接到来 → 立即恢复常规探测
        reg.note_activity();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let a3 = accepts.load(Ordering::Relaxed);
        assert!(a3 > a2, "新连接后应恢复探测（{a2} → {a3}）");

        cancel.cancel();
        acceptor.abort();
    }
}
