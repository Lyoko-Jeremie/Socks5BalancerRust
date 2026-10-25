//! 最小 SOCKS5 代理（仅用于测试）：把 CONNECT 目标真实连起来做双向转发。
//!
//! 用法：`cargo run --release --example mock_socks5_upstream -- 127.0.0.1:6200 [user:pass]`
//!   - 不传 `user:pass`：只接受 0x00 无认证；
//!   - 传 `user:pass`：只接受 0x02 用户名密码，校验失败回 `01 01`。
//!
//! 注意：这是测试基础设施，不是产品代码；产品侧解析实现见 `src/handshake/socks5.rs`。

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let addr = args
        .first()
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:6200".to_string());
    let creds = args.get(1).cloned();
    let listener = TcpListener::bind(&addr).await?;
    println!(
        "mock socks5 upstream listening on {} (auth: {:?})",
        listener.local_addr()?,
        creds
    );
    loop {
        let (stream, peer) = listener.accept().await?;
        let creds = creds.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, creds).await {
                eprintln!("mock upstream: {peer} 处理失败: {e}");
            }
        });
    }
}

async fn handle(mut stream: TcpStream, creds: Option<String>) -> io::Result<()> {
    // 1) 方法协商
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).await?;
    if head[0] != 0x05 || head[1] == 0 {
        return Ok(());
    }
    let mut methods = vec![0u8; head[1] as usize];
    stream.read_exact(&mut methods).await?;
    let method = if creds.is_some() {
        if methods.contains(&0x02) { 0x02 } else { 0xFF }
    } else if methods.contains(&0x00) {
        0x00
    } else {
        0xFF
    };
    stream.write_all(&[0x05, method]).await?;
    if method == 0xFF {
        return Ok(());
    }

    // 2) 用户名密码（RFC1929）
    if method == 0x02 {
        let mut b = [0u8; 2];
        stream.read_exact(&mut b).await?;
        let mut user = vec![0u8; b[1] as usize];
        stream.read_exact(&mut user).await?;
        let mut plen = [0u8; 1];
        stream.read_exact(&mut plen).await?;
        let mut pwd = vec![0u8; plen[0] as usize];
        stream.read_exact(&mut pwd).await?;
        let got = format!(
            "{}:{}",
            String::from_utf8_lossy(&user),
            String::from_utf8_lossy(&pwd)
        );
        let ok = creds.as_deref() == Some(got.as_str());
        stream
            .write_all(&[0x01, if ok { 0x00 } else { 0x01 }])
            .await?;
        if !ok {
            return Ok(());
        }
    }

    // 3) 请求
    let mut req = [0u8; 4];
    stream.read_exact(&mut req).await?;
    if req[1] != 0x01 {
        // 只支持 CONNECT
        stream
            .write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await?;
        return Ok(());
    }
    let target: Target = match req[3] {
        0x01 => {
            let mut ip = [0u8; 4];
            stream.read_exact(&mut ip).await?;
            let port = read_port(&mut stream).await?;
            Target::Ip(SocketAddr::from((Ipv4Addr::from(ip), port)))
        }
        0x04 => {
            let mut ip = [0u8; 16];
            stream.read_exact(&mut ip).await?;
            let port = read_port(&mut stream).await?;
            Target::Ip(SocketAddr::from((Ipv6Addr::from(ip), port)))
        }
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            let mut host = vec![0u8; len[0] as usize];
            stream.read_exact(&mut host).await?;
            let port = read_port(&mut stream).await?;
            Target::Domain(String::from_utf8_lossy(&host).to_string(), port)
        }
        _ => {
            stream
                .write_all(&[0x05, 0x08, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await?;
            return Ok(());
        }
    };

    // 4) 真实连接目标
    let connected = match &target {
        Target::Ip(addr) => TcpStream::connect(*addr).await,
        Target::Domain(host, port) => TcpStream::connect((host.as_str(), *port)).await,
    };
    match connected {
        Ok(mut upstream) => {
            stream
                .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await?;
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
        }
        Err(e) => {
            let rep = if e.kind() == io::ErrorKind::ConnectionRefused {
                0x05
            } else {
                0x01
            };
            stream
                .write_all(&[0x05, rep, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await?;
            eprintln!("mock upstream: 连接目标 {target:?} 失败: {e}");
        }
    }
    Ok(())
}

#[derive(Debug)]
enum Target {
    Ip(SocketAddr),
    Domain(String, u16),
}

async fn read_port(stream: &mut TcpStream) -> io::Result<u16> {
    let mut b = [0u8; 2];
    stream.read_exact(&mut b).await?;
    Ok(u16::from_be_bytes(b))
}
