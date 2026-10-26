//! 最小 HTTP 目标服务（仅用于测试）：返回 200 并在 body 中回显请求首行。
//!
//! 用途：验证 HTTP absolute-form 正向代理是否把请求正确改写为 origin-form
//! （客户端经代理请求 `http://127.0.0.1:6101/path`，body 应为 `OK GET /path HTTP/1.1`）。
//!
//! 用法：`cargo run --release --example http_target -- 127.0.0.1:6101`

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:6101".to_string());
    let listener = TcpListener::bind(&addr).await?;
    println!("http target listening on {}", listener.local_addr()?);
    loop {
        let (mut stream, peer) = listener.accept().await?;
        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            let mut filled = 0usize;
            loop {
                match stream.read(&mut buf[filled..]).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        filled += n;
                        if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") || filled == buf.len()
                        {
                            break;
                        }
                    }
                }
            }
            let head = String::from_utf8_lossy(&buf[..filled]).to_string();
            let first_line = head.lines().next().unwrap_or("").to_string();
            println!("http target: {peer} {first_line}");
            let body = format!("OK {first_line}");
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
    }
}
