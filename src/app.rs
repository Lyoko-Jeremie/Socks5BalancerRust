//! 进程内共享上下文。
//!
//! 包含"转发 + 选路 + 健康"所需的最小状态：配置、上游选择器、健康注册表、统计、取消令牌。
//! M3 起会在这里挂控制面快照等（见 docs/06 §6 模块划分）。

use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::health::HealthRegistry;
use crate::stats::Stats;
use crate::upstream::UpstreamSelector;

/// 所有任务共享的只读上下文。
#[derive(Debug)]
pub struct AppCtx {
    pub cfg: Arc<Config>,
    pub selector: Arc<UpstreamSelector>,
    pub health: Arc<HealthRegistry>,
    pub stats: Arc<Stats>,
    /// 进程启动时刻（控制面 `uptimeSeconds` 用）。
    pub started: Instant,
    /// 全局取消令牌：收到退出信号时取消，accept 循环、relay 与探测任务都会响应。
    pub cancel: CancellationToken,
}

impl AppCtx {
    pub fn new(
        cfg: Arc<Config>,
        selector: Arc<UpstreamSelector>,
        health: Arc<HealthRegistry>,
        stats: Arc<Stats>,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            selector,
            health,
            stats,
            started: Instant::now(),
            cancel,
        })
    }

    /// 选一个可用上游，但跳过 `excluded` 中已尝试过的下标。
    ///
    /// 用于数据面建连失败后的快速切换（ADR-002 #3：当前连接立即切下一个可用后端）。
    pub fn pick_upstream_excluding(
        &self,
        excluded: &[usize],
    ) -> Option<&crate::upstream::Upstream> {
        self.selector
            .next_excluding(&|idx| self.health.is_available(idx), excluded)
    }

    /// 按客户端 hint 选择上游（首次尝试）；`excluded` 非空时按排除规则重试。
    pub fn pick_upstream_for_client(
        &self,
        client: std::net::IpAddr,
        excluded: &[usize],
    ) -> Option<&crate::upstream::Upstream> {
        if excluded.is_empty() {
            self.selector
                .next_for_client(client, &|idx| self.health.is_available(idx))
        } else {
            self.pick_upstream_excluding(excluded)
        }
    }

    /// 记录客户端成功使用的上游（供下次按 hint 粘性选择）。
    pub fn record_hint(&self, client: std::net::IpAddr, idx: usize) {
        self.selector.record_hint(client, idx);
    }
}
