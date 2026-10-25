//! 本地 TCP echo 服务，用于 M0 纯透传的端到端验证。
//!
//! 用法：`cargo run --example echo_upstream -- 127.0.0.1:6100`

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:6100".to_string());
    let listener = TcpListener::bind(&addr).await?;
    println!("echo upstream listening on {}", listener.local_addr()?);
    loop {
        let (stream, peer) = listener.accept().await?;
        tokio::spawn(async move {
            let (mut reader, mut writer) = stream.into_split();
            let mut buf = vec![0u8; 8192];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if writer.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
            println!("echo: {peer} 连接关闭");
        });
    }
}
