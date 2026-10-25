//! SOCKS5 服务端协议（RFC1928 / RFC1929）。
//!
//! 设计要点：
//! - 全部用 `read_exact` 按**帧边界**读取，不做 `read_some`，因此天然不会"超读"，
//!   也就不需要 C++ 版的首包回放机制（HTTP 场景 M2 才需要 read_until + 回放）；
//! - 认证方法只支持 0x00（无认证）与 0x02（用户名密码），其余一律 `05 FF`（ADR-004）；
//! - 命令只支持 CONNECT；BIND/UDP ASSOCIATE 回 `0x07`；
//! - 所有失败路径都尽力写回 SOCKS5 错误码，便于客户端快速失败。

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::config::AuthUser;

pub const VERSION: u8 = 0x05;

/// 认证方法：无认证。
pub const METHOD_NO_AUTH: u8 = 0x00;
/// 认证方法：用户名密码（RFC1929）。
pub const METHOD_USER_PASS: u8 = 0x02;
/// 认证方法：无可接受方法。
pub const METHOD_NO_ACCEPTABLE: u8 = 0xFF;

/// 认证子协商版本（RFC1929）。
pub const AUTH_VERSION: u8 = 0x01;

// ===== REP 应答码（RFC1928 §6）=====
pub const REP_SUCCEEDED: u8 = 0x00;
pub const REP_GENERAL_FAILURE: u8 = 0x01;
pub const REP_NOT_ALLOWED: u8 = 0x02;
pub const REP_NETWORK_UNREACHABLE: u8 = 0x03;
pub const REP_HOST_UNREACHABLE: u8 = 0x04;
pub const REP_CONNECTION_REFUSED: u8 = 0x05;
pub const REP_TTL_EXPIRED: u8 = 0x06;
pub const REP_COMMAND_NOT_SUPPORTED: u8 = 0x07;
pub const REP_ADDRESS_TYPE_NOT_SUPPORTED: u8 = 0x08;

// ===== 命令 =====
pub const CMD_CONNECT: u8 = 0x01;

// ===== 地址类型 =====
pub const ATYP_IPV4: u8 = 0x01;
pub const ATYP_DOMAIN: u8 = 0x03;
pub const ATYP_IPV6: u8 = 0x04;

/// CONNECT 的目标地址（域名保持为字符串，交给上游解析）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetAddr {
    Ip(SocketAddr),
    Domain { host: String, port: u16 },
}

impl TargetAddr {
    /// 便于日志展示；域名不加引号。
    pub fn display(&self) -> String {
        match self {
            TargetAddr::Ip(addr) => addr.to_string(),
            TargetAddr::Domain { host, port } => format!("{host}:{port}"),
        }
    }
}

impl std::fmt::Display for TargetAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display())
    }
}

/// 入站握手错误。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O 错误: {0}")]
    Io(#[from] io::Error),
    #[error("不支持的 SOCKS 版本: 0x{0:02x}")]
    UnsupportedVersion(u8),
    #[error("没有可接受的认证方法")]
    NoAcceptableMethod,
    #[error("认证失败")]
    AuthFailed,
    #[error("不支持的认证子协商版本: 0x{0:02x}")]
    UnsupportedAuthVersion(u8),
    #[error("不支持的地址类型: 0x{0:02x}")]
    UnsupportedAddressType(u8),
    #[error("不支持的 SOCKS5 命令: 0x{0:02x}")]
    UnsupportedCommand(u8),
    #[error("请求格式错误: {0}")]
    BadRequest(&'static str),
}

/// 执行 SOCKS5 服务端协商：方法选择 → 可选认证 → 解析 CONNECT 请求。
///
/// 成功后返回目标地址；此时**尚未**写最终应答（由调用方在连上上游后写
/// [`write_success_reply`] 或 [`write_error_reply`]）。
pub async fn negotiate<S>(stream: &mut S, auth: &[AuthUser]) -> Result<TargetAddr, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // 1) 版本 + 方法列表
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).await?;
    if head[0] != VERSION {
        return Err(Error::UnsupportedVersion(head[0]));
    }
    let nmethods = head[1] as usize;
    if nmethods == 0 {
        let _ = stream.write_all(&[VERSION, METHOD_NO_ACCEPTABLE]).await;
        return Err(Error::NoAcceptableMethod);
    }
    let mut methods = vec![0u8; nmethods];
    stream.read_exact(&mut methods).await?;

    // 2) 方法选择（ADR-004：仅 0x00 / 0x02）
    let need_auth = !auth.is_empty();
    let method = if need_auth {
        if methods.contains(&METHOD_USER_PASS) {
            METHOD_USER_PASS
        } else {
            METHOD_NO_ACCEPTABLE
        }
    } else if methods.contains(&METHOD_NO_AUTH) {
        METHOD_NO_AUTH
    } else {
        METHOD_NO_ACCEPTABLE
    };
    stream.write_all(&[VERSION, method]).await?;
    stream.flush().await?;
    if method == METHOD_NO_ACCEPTABLE {
        return Err(Error::NoAcceptableMethod);
    }

    // 3) RFC1929 用户名密码认证
    if method == METHOD_USER_PASS {
        let mut auth_head = [0u8; 2]; // 子协商 VER + ULEN
        stream.read_exact(&mut auth_head).await?;
        if auth_head[0] != AUTH_VERSION {
            write_auth_reply(stream, false).await?;
            return Err(Error::UnsupportedAuthVersion(auth_head[0]));
        }
        let ulen = auth_head[1] as usize;
        let mut user = vec![0u8; ulen];
        stream.read_exact(&mut user).await?;
        let mut plen_buf = [0u8; 1];
        stream.read_exact(&mut plen_buf).await?;
        let mut pwd = vec![0u8; plen_buf[0] as usize];
        stream.read_exact(&mut pwd).await?;

        // 用字节比较，避免客户端发送非 UTF-8 凭据时的行为差异；配置侧统一 UTF-8。
        let ok = auth
            .iter()
            .any(|u| u.user.as_bytes() == user.as_slice() && u.pwd.as_bytes() == pwd.as_slice());
        write_auth_reply(stream, ok).await?;
        if !ok {
            return Err(Error::AuthFailed);
        }
    }

    // 4) 请求行
    let mut req = [0u8; 4]; // VER CMD RSV ATYP
    stream.read_exact(&mut req).await?;
    if req[0] != VERSION {
        return Err(Error::UnsupportedVersion(req[0]));
    }
    let cmd = req[1];
    if cmd != CMD_CONNECT {
        let _ = write_reply(stream, REP_COMMAND_NOT_SUPPORTED).await;
        return Err(Error::UnsupportedCommand(cmd));
    }
    if req[2] != 0x00 {
        let _ = write_reply(stream, REP_GENERAL_FAILURE).await;
        return Err(Error::BadRequest("RSV 必须为 0"));
    }

    // 5) 地址
    let target = match req[3] {
        ATYP_IPV4 => {
            let mut b = [0u8; 4];
            stream.read_exact(&mut b).await?;
            let port = read_port(stream).await?;
            TargetAddr::Ip(SocketAddr::from((Ipv4Addr::from(b), port)))
        }
        ATYP_IPV6 => {
            let mut b = [0u8; 16];
            stream.read_exact(&mut b).await?;
            let port = read_port(stream).await?;
            TargetAddr::Ip(SocketAddr::from((Ipv6Addr::from(b), port)))
        }
        ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            if len[0] == 0 {
                let _ = write_reply(stream, REP_GENERAL_FAILURE).await;
                return Err(Error::BadRequest("域名为空"));
            }
            let mut host = vec![0u8; len[0] as usize];
            stream.read_exact(&mut host).await?;
            let port = read_port(stream).await?;
            let host =
                String::from_utf8(host).map_err(|_| Error::BadRequest("域名不是合法 UTF-8"))?;
            TargetAddr::Domain { host, port }
        }
        other => {
            let _ = write_reply(stream, REP_ADDRESS_TYPE_NOT_SUPPORTED).await;
            return Err(Error::UnsupportedAddressType(other));
        }
    };

    Ok(target)
}

async fn read_port<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<u16> {
    let mut b = [0u8; 2];
    stream.read_exact(&mut b).await?;
    Ok(u16::from_be_bytes(b))
}

async fn write_auth_reply<S: AsyncWrite + Unpin>(stream: &mut S, ok: bool) -> io::Result<()> {
    stream
        .write_all(&[AUTH_VERSION, if ok { 0x00 } else { 0x01 }])
        .await?;
    stream.flush().await
}

/// 写 SOCKS5 应答（BND.ADDR/BND.PORT 固定为 0.0.0.0:0，与 C++ 版一致）。
pub async fn write_reply<S: AsyncWrite + Unpin>(stream: &mut S, rep: u8) -> io::Result<()> {
    stream
        .write_all(&[VERSION, rep, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0])
        .await?;
    stream.flush().await
}

/// 写成功应答（REP=0x00）。
pub async fn write_success_reply<S: AsyncWrite + Unpin>(stream: &mut S) -> io::Result<()> {
    write_reply(stream, REP_SUCCEEDED).await
}

/// 写失败应答（尽力而为：写失败也不影响主流程返回错误）。
pub async fn write_error_reply<S: AsyncWrite + Unpin>(stream: &mut S, rep: u8) -> io::Result<()> {
    write_reply(stream, rep).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    fn auth_user(user: &str, pwd: &str) -> AuthUser {
        AuthUser {
            user: user.to_string(),
            pwd: pwd.to_string(),
        }
    }

    /// 启动协商任务，返回 (客户端端, 任务句柄)。
    async fn spawn_negotiate(
        auth: Vec<AuthUser>,
    ) -> (
        tokio::io::DuplexStream,
        tokio::task::JoinHandle<Result<TargetAddr, Error>>,
    ) {
        let (client, mut server) = duplex(256);
        let handle = tokio::spawn(async move { negotiate(&mut server, &auth).await });
        (client, handle)
    }

    #[tokio::test]
    async fn 无认证_域名连接成功() {
        let (mut c, h) = spawn_negotiate(vec![]).await;
        c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut resp = [0u8; 2];
        c.read_exact(&mut resp).await.unwrap();
        assert_eq!(resp, [0x05, 0x00]);
        c.write_all(&[0x05, 0x01, 0x00, 0x03, 11]).await.unwrap();
        c.write_all(b"example.com").await.unwrap();
        c.write_all(&443u16.to_be_bytes()).await.unwrap();
        let target = h.await.unwrap().unwrap();
        assert_eq!(
            target,
            TargetAddr::Domain {
                host: "example.com".into(),
                port: 443
            }
        );
    }

    #[tokio::test]
    async fn 无认证_ipv4_与_ipv6() {
        let (mut c, h) = spawn_negotiate(vec![]).await;
        c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut resp = [0u8; 2];
        c.read_exact(&mut resp).await.unwrap();
        c.write_all(&[0x05, 0x01, 0x00, 0x01, 127, 0, 0, 1])
            .await
            .unwrap();
        c.write_all(&8080u16.to_be_bytes()).await.unwrap();
        assert_eq!(
            h.await.unwrap().unwrap(),
            TargetAddr::Ip("127.0.0.1:8080".parse().unwrap())
        );

        let (mut c, h) = spawn_negotiate(vec![]).await;
        c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut resp = [0u8; 2];
        c.read_exact(&mut resp).await.unwrap();
        let mut frame = vec![0x05, 0x01, 0x00, 0x04];
        frame.extend_from_slice(&[0u8; 15]);
        frame.push(1); // ::1
        c.write_all(&frame).await.unwrap();
        c.write_all(&80u16.to_be_bytes()).await.unwrap();
        assert_eq!(
            h.await.unwrap().unwrap(),
            TargetAddr::Ip("[::1]:80".parse().unwrap())
        );
    }

    #[tokio::test]
    async fn 多方法首包_优先无认证() {
        // C++ 默认模式会挂死；Rust 版必须正确协商（ADR-004）
        let (mut c, h) = spawn_negotiate(vec![]).await;
        c.write_all(&[0x05, 0x02, 0x00, 0x02]).await.unwrap();
        let mut resp = [0u8; 2];
        c.read_exact(&mut resp).await.unwrap();
        assert_eq!(resp, [0x05, 0x00]);
        c.write_all(&[0x05, 0x01, 0x00, 0x01, 10, 0, 0, 1])
            .await
            .unwrap();
        c.write_all(&53u16.to_be_bytes()).await.unwrap();
        assert!(h.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn 配置了认证_必须选_0x02() {
        let (mut c, h) = spawn_negotiate(vec![auth_user("u", "p")]).await;
        // 客户端只提供 0x00 → 服务端应回 FF
        c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut resp = [0u8; 2];
        c.read_exact(&mut resp).await.unwrap();
        assert_eq!(resp, [0x05, 0xFF]);
        assert!(matches!(
            h.await.unwrap().unwrap_err(),
            Error::NoAcceptableMethod
        ));
    }

    #[tokio::test]
    async fn 用户名密码认证成功() {
        let (mut c, h) = spawn_negotiate(vec![auth_user("alice", "secret")]).await;
        c.write_all(&[0x05, 0x02, 0x00, 0x02]).await.unwrap();
        let mut resp = [0u8; 2];
        c.read_exact(&mut resp).await.unwrap();
        assert_eq!(resp, [0x05, 0x02]);
        // RFC1929：VER=1, ULEN, user, PLEN, pwd
        c.write_all(&[0x01, 5]).await.unwrap();
        c.write_all(b"alice").await.unwrap();
        c.write_all(&[6]).await.unwrap();
        c.write_all(b"secret").await.unwrap();
        let mut ar = [0u8; 2];
        c.read_exact(&mut ar).await.unwrap();
        assert_eq!(ar, [0x01, 0x00]);
        // CONNECT
        c.write_all(&[0x05, 0x01, 0x00, 0x03, 1, b'x'])
            .await
            .unwrap();
        c.write_all(&443u16.to_be_bytes()).await.unwrap();
        let target = h.await.unwrap().unwrap();
        assert_eq!(
            target,
            TargetAddr::Domain {
                host: "x".into(),
                port: 443
            }
        );
    }

    #[tokio::test]
    async fn 用户名密码认证失败() {
        let (mut c, h) = spawn_negotiate(vec![auth_user("alice", "secret")]).await;
        c.write_all(&[0x05, 0x01, 0x02]).await.unwrap();
        let mut resp = [0u8; 2];
        c.read_exact(&mut resp).await.unwrap();
        assert_eq!(resp, [0x05, 0x02]);
        c.write_all(&[0x01, 5]).await.unwrap();
        c.write_all(b"alice").await.unwrap();
        c.write_all(&[5]).await.unwrap();
        c.write_all(b"wrong").await.unwrap();
        let mut ar = [0u8; 2];
        c.read_exact(&mut ar).await.unwrap();
        assert_eq!(ar, [0x01, 0x01]);
        assert!(matches!(h.await.unwrap().unwrap_err(), Error::AuthFailed));
    }

    #[tokio::test]
    async fn bind_命令回_0x07() {
        let (mut c, h) = spawn_negotiate(vec![]).await;
        c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut resp = [0u8; 2];
        c.read_exact(&mut resp).await.unwrap();
        c.write_all(&[0x05, 0x02, 0x00, 0x01, 0, 0, 0, 0])
            .await
            .unwrap();
        c.write_all(&0u16.to_be_bytes()).await.unwrap();
        let mut reply = [0u8; 10];
        c.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], 0x05);
        assert_eq!(reply[1], REP_COMMAND_NOT_SUPPORTED);
        assert!(matches!(
            h.await.unwrap().unwrap_err(),
            Error::UnsupportedCommand(0x02)
        ));
    }

    #[tokio::test]
    async fn 不支持的地址类型回_0x08() {
        let (mut c, h) = spawn_negotiate(vec![]).await;
        c.write_all(&[0x05, 0x01, 0x00]).await.unwrap();
        let mut resp = [0u8; 2];
        c.read_exact(&mut resp).await.unwrap();
        c.write_all(&[0x05, 0x01, 0x00, 0x09]).await.unwrap();
        let mut reply = [0u8; 10];
        c.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], REP_ADDRESS_TYPE_NOT_SUPPORTED);
        assert!(matches!(
            h.await.unwrap().unwrap_err(),
            Error::UnsupportedAddressType(0x09)
        ));
    }

    #[tokio::test]
    async fn 逐字节拆包也能正确解析() {
        let (mut c, h) = spawn_negotiate(vec![]).await;
        let frame = [
            0x05, 0x01, 0x00, // 方法协商
            0x05, 0x01, 0x00, 0x03, 3, b'a', b'b', b'c', // CONNECT 域名
            0x01, 0xBB, // 443
        ];
        for byte in frame {
            c.write_all(&[byte]).await.unwrap();
            tokio::task::yield_now().await;
        }
        let target = h.await.unwrap().unwrap();
        assert_eq!(
            target,
            TargetAddr::Domain {
                host: "abc".into(),
                port: 443
            }
        );
    }

    #[tokio::test]
    async fn 非_socks5_版本被拒绝() {
        let (mut c, h) = spawn_negotiate(vec![]).await;
        c.write_all(&[0x04, 0x01, 0x00]).await.unwrap();
        assert!(matches!(
            h.await.unwrap().unwrap_err(),
            Error::UnsupportedVersion(0x04)
        ));
    }

    #[tokio::test]
    async fn 应答格式固定十字节() {
        let (mut c, mut s) = duplex(64);
        write_success_reply(&mut s).await.unwrap();
        let mut reply = [0u8; 10];
        c.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]);
        write_error_reply(&mut s, REP_CONNECTION_REFUSED)
            .await
            .unwrap();
        c.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[1], REP_CONNECTION_REFUSED);
    }
}
