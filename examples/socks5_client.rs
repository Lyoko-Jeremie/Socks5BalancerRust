//! SOCKS5 客户端（仅用于测试）：完成握手 → CONNECT → 发 payload → 读回显。
//!
//! 用法：
//!   `socks5_client <proxy> <target> [payload] [--auth user:pass]`
//!   - proxy：如 `127.0.0.1:5100`
//!   - target：如 `127.0.0.1:6100`（域名也可，交给代理解析）
//!   - `--auth user:pass`：启用 0x02 用户名密码认证
//!   - 成功时打印 `OK <回显>` 并以 0 退出；失败打印 `FAIL ...` 并以 1 退出。
//!
//! 客户端协议栈直接复用 tokio-socks（与 balancer 上游实现是同一套库，便于交叉验证）。

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_socks::tcp::Socks5Stream;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("FAIL {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        return Err("用法: socks5_client <proxy> <target> [payload] [--auth user:pass]".into());
    }
    let proxy: std::net::SocketAddr = args[0].parse()?;
    let (host, port) = split_target(&args[1])?;

    // 位置参数：payload；选项：--auth user:pass
    let mut payload = "hello-m1".to_string();
    let mut creds_raw: Option<String> = None;
    let mut it = args.iter().skip(2);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--auth" => {
                creds_raw = Some(it.next().ok_or("--auth 缺少 user:pass")?.clone());
            }
            other => payload = other.to_string(),
        }
    }

    // 先解析凭据（错误类型统一为 Box<dyn Error>），再进入握手 future
    let creds: Option<(String, String)> = match creds_raw.as_deref() {
        Some(c) => {
            let (user, pwd) = c.split_once(':').ok_or("凭据格式应为 user:pass")?;
            Some((user.to_string(), pwd.to_string()))
        }
        None => None,
    };

    let handshake = async {
        match creds {
            Some((user, pwd)) => {
                Socks5Stream::connect_with_password(proxy, (host.as_str(), port), &user, &pwd).await
            }
            None => Socks5Stream::connect(proxy, (host.as_str(), port)).await,
        }
    };
    let mut stream = tokio::time::timeout(Duration::from_secs(5), handshake)
        .await
        .map_err(|_| "握手超时")??;

    stream.write_all(payload.as_bytes()).await?;
    stream.flush().await?;
    let mut buf = vec![0u8; payload.len().max(64)];
    let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .map_err(|_| "读取回显超时")??;
    let echoed = String::from_utf8_lossy(&buf[..n]).to_string();
    if echoed == payload {
        println!("OK {echoed}");
        Ok(())
    } else {
        Err(format!("回显不一致: 期望 {payload:?} 实际 {echoed:?}").into())
    }
}

fn split_target(target: &str) -> Result<(String, u16), Box<dyn std::error::Error>> {
    // 支持 IPv6 字面量 [::1]:8080 与普通 host:port
    if let Ok(addr) = target.parse::<std::net::SocketAddr>() {
        return Ok((addr.ip().to_string(), addr.port()));
    }
    let (host, port) = target.rsplit_once(':').ok_or("target 必须是 host:port")?;
    Ok((host.to_string(), port.parse()?))
}
