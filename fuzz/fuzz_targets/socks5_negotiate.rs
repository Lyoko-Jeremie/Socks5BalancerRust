//! SOCKS5 入站协商 fuzz 目标。
//!
//! 覆盖 `handshake::socks5::negotiate`：版本/方法列表、RFC1929 用户名密码、
//! CONNECT 请求的 ATYP（IPv4 / 域名 / IPv6）与错误应答路径。
//!
//! 输入约定：首字节最低位决定是否启用客户端认证（0=无认证，1=用户名密码），
//! 其余字节原样作为网络字节流喂给解析器。断言目标（ADR-008）：**任何输入都不 panic**。

#![no_main]

use libfuzzer_sys::fuzz_target;
use socks5_balancer_rust::config::AuthUser;
use socks5_balancer_rust::handshake::socks5;

#[path = "common.rs"]
mod common;

fuzz_target!(|data: &[u8]| {
    let Some((&selector, payload)) = data.split_first() else {
        return;
    };

    // 一半输入走"无认证"，一半走"用户名密码"分支，两个分支都要覆盖
    let auth: &[AuthUser] = if selector & 1 == 1 {
        common::auth_users()
    } else {
        &[]
    };

    common::runtime().block_on(async {
        let mut stream = common::server_with(payload).await;
        // 结果本身（Ok/Err）不做判断，只要求解析过程不 panic
        let _ = socks5::negotiate(&mut stream, auth).await;
    });
});
