//! fuzz target 共用工具。
//!
//! 每个 fuzz target 在 cargo-fuzz 里都是独立的 crate root，彼此不能互相依赖，
//! 因此用 `#[path = "common.rs"] mod common;` 复用这一份辅助代码（不会被当成 target）。

use std::sync::OnceLock;

use socks5_balancer_rust::config::AuthUser;
use tokio::io::{AsyncWriteExt, DuplexStream};
use tokio::runtime::{Builder, Runtime};

/// 进程级共享的 current-thread runtime。
///
/// fuzz 每秒会执行数万次输入，如果每个输入都新建 runtime，开销会完全盖住被测代码，
/// 所以只创建一次并复用。
pub fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("创建 tokio current-thread runtime 失败")
    })
}

/// 认证用户池（非空时 handler 走"需要认证"分支）。
pub fn auth_users() -> &'static [AuthUser] {
    static AUTH: OnceLock<Vec<AuthUser>> = OnceLock::new();
    AUTH.get_or_init(|| {
        vec![AuthUser {
            user: "u".to_string(),
            pwd: "p".to_string(),
        }]
    })
}

/// 把随机字节写进内存 duplex，返回**服务端**一侧给协议解析函数读取。
///
/// 两个关键点：
/// 1. 容量按输入长度 +64 分配，保证写端在解析器尚未读取时也能一次写完，不会互相等待；
/// 2. 写完关闭写端（EOF），解析器就不会因为"等更多字节"而永久阻塞，从而覆盖
///    "输入截断/不完整"的错误分支。
pub async fn server_with(payload: &[u8]) -> DuplexStream {
    let (mut client, server) = tokio::io::duplex(payload.len() + 64);
    let _ = client.write_all(payload).await;
    let _ = client.shutdown().await;
    drop(client);
    server
}
