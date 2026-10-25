//! 进程内共享上下文。
//!
//! 包含"转发 + 选路 + 健康"所需的最小状态：配置、上游选择器、健康注册表、统计、取消令牌。
//! M3 起会在这里挂控制面快照等（见 docs/06 §6 模块划分）。

use std::sync::Arc;

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
            cancel,
        })
    }

    /// 选一个当前可用的上游（健康状态由 M1.4 的注册表提供）。
    pub fn pick_upstream(&self) -> Option<&crate::upstream::Upstream> {
        self.selector.next(&|idx| self.health.is_available(idx))
    }
}
