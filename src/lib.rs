//! Socks5BalancerRust 库入口。
//!
//! 把核心模块暴露为库，供 fuzz / 集成测试复用**真实**解析代码（避免与二进制分叉）；
//! 可执行入口见 `src/main.rs`。
//!
//! 模块划分与 docs/06 §6 一致：配置、日志、网络、协议、转发、上游池、健康、统计、控制面。

pub mod app;
pub mod config;
pub mod conn;
pub mod control;
pub mod handshake;
pub mod health;
pub mod logging;
pub mod net;
pub mod relay;
pub mod stats;
pub mod upstream;
