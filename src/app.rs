//! 进程内共享上下文。
//!
//! M0 只包含"转发所需"的最小状态：配置、上游选择器、统计、取消令牌。
//! M1 起会在这里挂健康检查快照、控制面快照等（见 docs/06 §6 模块划分）。

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::stats::Stats;
use crate::upstream::UpstreamSelector;

/// 所有任务共享的只读上下文。
#[derive(Debug)]
pub struct AppCtx {
    pub cfg: Arc<Config>,
    pub selector: Arc<UpstreamSelector>,
    pub stats: Arc<Stats>,
    /// 全局取消令牌：收到退出信号时取消，accept 循环与 relay 任务都会响应。
    pub cancel: CancellationToken,
}

impl AppCtx {
    pub fn new(
        cfg: Arc<Config>,
        selector: Arc<UpstreamSelector>,
        stats: Arc<Stats>,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            selector,
            stats,
            cancel,
        })
    }
}
