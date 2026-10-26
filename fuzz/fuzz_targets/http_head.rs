//! HTTP 代理请求头 fuzz 目标。
//!
//! 覆盖三类解析入口：
//! 1. `handshake::http::parse`：CONNECT authority、IPv6、absolute-form 改写、非法请求；
//! 2. `handshake::http::check_proxy_auth`：`Proxy-Authorization: Basic` 解码；
//! 3. `handshake::http::read_head`：增量读头（含 `HeadTooLarge` 与 `Incomplete` 分支）。
//!
//! 断言目标（ADR-008）：**任何输入都不 panic、不越界、不永久阻塞**。

#![no_main]

use libfuzzer_sys::fuzz_target;
use socks5_balancer_rust::handshake::http;

#[path = "common.rs"]
mod common;

fuzz_target!(|data: &[u8]| {
    // 1) 原始随机字节直接当"请求头"喂给解析器（绝大多数是畸形输入，正好测拒绝路径）
    let _ = http::parse(data);

    // 2) 代理认证头解析
    let _ = http::check_proxy_auth(data, common::auth_users());

    // 3) 模拟正常切分：只取到第一个 \r\n\r\n（含结尾空行）的头
    if let Some(pos) = data.windows(4).position(|w| w == b"\r\n\r\n") {
        let _ = http::parse(&data[..pos + 4]);
    }

    // 4) 异步增量读头：用输入首字节扰动 max_bytes，覆盖 "头部超限" 与 "读到 EOF" 两条错误路径
    let max_bytes = if data.len() > 1 {
        (data[0] as usize).max(1)
    } else {
        http::MAX_HEAD_BYTES
    };
    common::runtime().block_on(async {
        let mut stream = common::server_with(data).await;
        let _ = http::read_head(&mut stream, max_bytes).await;
    });
});
