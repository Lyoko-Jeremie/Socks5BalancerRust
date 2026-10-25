//! 本地 HTTPS 测试服务（仅用于测试）：自签证书 + 最小 HTTP 响应。
//!
//! 用途：M1.5 TLS/HTTP readiness 探测的可复现验收，不依赖外网。
//! 启动时生成自签证书并把证书 PEM 写到 `ca_out`，balancer 通过 `healthCheck.target.caFile` 信任它。
//!
//! 用法：`tls_echo_server <listen> [ca_out]`
//!   - listen：如 `127.0.0.1:18443`
//!   - ca_out：证书 PEM 输出路径（默认 `target/m1-tls-logs/cert.pem`）
//!
//! 注意：这是测试基础设施，不是产品代码。

use std::io;
use std::sync::Arc;

use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let listen = args
        .first()
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:18443".to_string());
    let ca_out = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| "target/m1-tls-logs/cert.pem".to_string());

    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(vec!["localhost".to_string()])
            .map_err(|e| io::Error::other(format!("生成自签证书失败: {e}")))?;
    if let Some(parent) = std::path::Path::new(&ca_out).parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&ca_out, cert.pem())?;
    println!("tls echo server: 证书已写入 {ca_out}");

    let certs = vec![CertificateDer::from(cert.der().to_vec())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));
    let server_config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| io::Error::other(format!("构建 ServerConfig 失败: {e}")))?;
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let listener = TcpListener::bind(&listen).await?;
    println!("tls echo server listening on {}", listener.local_addr()?);
    loop {
        let (stream, peer) = listener.accept().await?;
        let acceptor = acceptor.clone();
        tokio::spawn(async move {
            let Ok(mut tls) = acceptor.accept(stream).await else {
                return;
            };
            // 读到请求头结束（最多 4KB）
            let mut buf = vec![0u8; 4096];
            let mut filled = 0usize;
            loop {
                match tls.read(&mut buf[filled..]).await {
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
            let first_line = String::from_utf8_lossy(&buf[..filled])
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            let body = "ok\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = tls.write_all(response.as_bytes()).await;
            let _ = tls.shutdown().await;
            println!("tls echo server: {peer} 请求: {first_line}");
        });
    }
}
