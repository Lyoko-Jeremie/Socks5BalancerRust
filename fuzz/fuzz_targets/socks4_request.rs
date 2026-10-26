//! SOCKS4 / SOCKS4a 请求 fuzz 目标。
//!
//! 覆盖 `handshake::socks4::negotiate`：8 字节定长头、USERID（增强认证模式）、
//! SOCKS4a 域名、非法命令/端口/版本的错误应答路径。
//!
//! 输入约定：首字节最低位决定是否启用 USERID 增强认证（0=不校验，1=校验），
//! 其余字节原样作为网络字节流喂给解析器。断言目标（ADR-008）：**任何输入都不 panic**。

#![no_main]

use libfuzzer_sys::fuzz_target;
use socks5_balancer_rust::config::AuthUser;
use socks5_balancer_rust::handshake::socks4;

#[path = "common.rs"]
mod common;

fuzz_target!(|data: &[u8]| {
    let Some((&selector, payload)) = data.split_first() else {
        return;
    };

    let auth: &[AuthUser] = if selector & 1 == 1 {
        common::auth_users()
    } else {
        &[]
    };

    common::runtime().block_on(async {
        let mut stream = common::server_with(payload).await;
        // 解析结果不做判断，只要求不 panic（失败路径会尝试写回 8 字节应答）
        let _ = socks4::negotiate(&mut stream, auth).await;
    });
});
