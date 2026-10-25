//! 日志初始化。
//!
//! 决策依据：ADR-007（tracing 三件套 + 异步 writer）、02-relay-core §7（日志必须移出热路径）。
//! - 级别来源：`RUST_LOG` 环境变量优先，否则用配置里的 `logLevel`（默认 `info`）；
//! - 使用 `tracing_appender::non_blocking`，避免同步写 stdout 阻塞 I/O 线程；
//! - 关闭 ANSI（Windows 重定向到文件时不会出现转义符）。

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

/// 初始化全局日志，返回的 guard 必须活到进程结束（否则最后一批日志会丢）。
pub fn init(level: &str) -> WorkerGuard {
    let fallback = if level.trim().is_empty() {
        "info"
    } else {
        level
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(fallback));

    let (writer, guard) = tracing_appender::non_blocking(std::io::stdout());

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_target(false)
        .with_ansi(false)
        .init();

    guard
}

/// 版本信息（与 C++ 版 `versionInfo()` 对齐的简化版）。
pub fn version_info() -> String {
    format!(
        "Socks5BalancerRust {}\n  profile: {}\n  target: {}-{}\n",
        env!("CARGO_PKG_VERSION"),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}
