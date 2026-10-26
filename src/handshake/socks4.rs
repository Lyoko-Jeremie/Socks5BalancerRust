//! SOCKS4 / SOCKS4a 服务端协议（M2）。
//!
//! 协议要点（对齐 docs/03 §5）：
//! - 请求头固定 8 字节：`VN(4) CD DSTPORT(2) DSTIP(4)`，随后是 `USERID\0`；
//! - SOCKS4a：DSTIP 为 `0.0.0.x`（x≠0）时，USERID 之后再跟 `DOMAIN\0`；
//! - 只支持 CD=1（CONNECT），其它命令回 `0x5B`；
//! - 应答固定 8 字节：`00 5A 0000 00000000`（成功）或 `00 5B ...`（失败）。
//!
//! 与 SOCKS5 一致：解析出目标后由调用方通过上游 SOCKS5 建立隧道。

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::config::AuthUser;
use crate::handshake::TargetAddr;

pub const VERSION: u8 = 0x04;
pub const CMD_CONNECT: u8 = 0x01;
pub const REP_GRANTED: u8 = 0x5A;
pub const REP_REJECTED: u8 = 0x5B;
/// USERID 认证失败（C++ 增强模式：`00 5C ...`）。
pub const REP_AUTH_FAILED: u8 = 0x5C;

/// USERID / 域名的最大长度（防滥用）。
const MAX_FIELD: usize = 1024;

/// 读取 SOCKS4/4a 请求，返回 CONNECT 目标。
///
/// `auth` 非空时按 C++ 增强模式校验 USERID（SOCKS4 无密码，只校验用户名）。
pub async fn negotiate<S>(stream: &mut S, auth: &[AuthUser]) -> Result<TargetAddr, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut head = [0u8; 8]; // VN CD DSTPORT DSTIP
    stream.read_exact(&mut head).await?;
    if head[0] != VERSION {
        return Err(Error::BadVersion(head[0]));
    }
    let cmd = head[1];
    let port = u16::from_be_bytes([head[2], head[3]]);
    let ip = Ipv4Addr::new(head[4], head[5], head[6], head[7]);

    // USERID：C++ 增强模式用它当用户名做认证（只校验用户名，不校验密码）
    let userid = read_cstr(stream).await?;
    if !auth.is_empty() {
        let user = String::from_utf8_lossy(&userid);
        let ok = !user.is_empty() && auth.iter().any(|a| a.user == user);
        if !ok {
            write_reply_code(stream, REP_AUTH_FAILED).await?;
            return Err(Error::AuthFailed);
        }
    }

    if cmd != CMD_CONNECT {
        write_reply(stream, false).await?;
        return Err(Error::UnsupportedCommand(cmd));
    }
    if port == 0 {
        write_reply(stream, false).await?;
        return Err(Error::BadRequest("端口不能为 0"));
    }

    // SOCKS4a：0.0.0.x（x≠0）表示后面跟域名
    let target = if ip.octets()[..3] == [0, 0, 0] && ip.octets()[3] != 0 {
        let domain = read_cstr(stream).await?;
        if domain.is_empty() {
            write_reply(stream, false).await?;
            return Err(Error::BadRequest("SOCKS4a 域名为空"));
        }
        let host = String::from_utf8(domain)
            .map_err(|_| Error::BadRequest("SOCKS4a 域名不是合法 UTF-8"))?;
        TargetAddr::Domain { host, port }
    } else if ip.is_unspecified() {
        // 0.0.0.0 在 SOCKS4 中表示无效目标（4a 的 x=0 变体不支持）
        write_reply(stream, false).await?;
        return Err(Error::BadRequest("SOCKS4 目标 IP 为 0.0.0.0"));
    } else {
        TargetAddr::Ip(SocketAddr::new(IpAddr::V4(ip), port))
    };

    Ok(target)
}

/// 写 SOCKS4 应答（8 字节）。
pub async fn write_reply<S: AsyncWrite + Unpin>(stream: &mut S, granted: bool) -> io::Result<()> {
    let rep = if granted { REP_GRANTED } else { REP_REJECTED };
    write_reply_code(stream, rep).await
}

/// 写指定 CD 的 SOCKS4 应答（8 字节）；`0x5C` 用于 USERID 认证失败。
pub async fn write_reply_code<S: AsyncWrite + Unpin>(
    stream: &mut S,
    rep: u8,
) -> io::Result<()> {
    stream
        .write_all(&[0x00, rep, 0x00, 0x00, 0, 0, 0, 0])
        .await?;
    stream.flush().await
}

/// 读取以 `\0` 结尾的字段（最多 [`MAX_FIELD`] 字节）。
async fn read_cstr<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).await?;
        if byte[0] == 0 {
            return Ok(out);
        }
        out.push(byte[0]);
        if out.len() > MAX_FIELD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "SOCKS4 字段过长",
            ));
        }
    }
}

/// SOCKS4 解析错误。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O 错误: {0}")]
    Io(#[from] io::Error),
    #[error("不是 SOCKS4 协议（版本 0x{0:02x}）")]
    BadVersion(u8),
    #[error("不支持的 SOCKS4 命令: 0x{0:02x}")]
    UnsupportedCommand(u8),
    #[error("请求非法: {0}")]
    BadRequest(&'static str),
    #[error("USERID 认证失败")]
    AuthFailed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    async fn run_session(payload: Vec<u8>) -> (Result<TargetAddr, Error>, Vec<u8>) {
        run_session_auth(payload, &[]).await
    }

    async fn run_session_auth(
        payload: Vec<u8>,
        auth: &[AuthUser],
    ) -> (Result<TargetAddr, Error>, Vec<u8>) {
        let (mut client, mut server) = duplex(4096);
        let auth = auth.to_vec();
        let handle = tokio::spawn(async move { negotiate(&mut server, &auth).await });
        {
            use tokio::io::AsyncWriteExt;
            client.write_all(&payload).await.unwrap();
        }
        let target = handle.await.unwrap();
        // 读取可能的应答（非阻塞，读不到也算空）
        let mut reply = Vec::new();
        let mut buf = [0u8; 16];
        if let Ok(Ok(n)) =
            tokio::time::timeout(std::time::Duration::from_millis(50), client.read(&mut buf)).await
        {
            reply.extend_from_slice(&buf[..n]);
        }
        (target, reply)
    }

    #[tokio::test]
    async fn 标准_socks4_ip_目标() {
        // VN=4 CD=1 port=6100 ip=127.0.0.1 userid="u" 0
        let mut p = vec![0x04, 0x01, 0x17, 0xD4, 127, 0, 0, 1];
        p.extend_from_slice(b"u\0");
        let (target, reply) = run_session(p).await;
        assert_eq!(
            target.unwrap(),
            TargetAddr::Ip("127.0.0.1:6100".parse().unwrap())
        );
        assert!(reply.is_empty(), "SOCKS4 成功应答由调用方在隧道建立后写");
    }

    #[tokio::test]
    async fn socks4a_域名目标() {
        // ip=0.0.0.1 表示 4a，userid="", domain="localhost"
        let mut p = vec![0x04, 0x01, 0x01, 0xBB, 0, 0, 0, 1];
        p.extend_from_slice(b"\0");
        p.extend_from_slice(b"localhost\0");
        let (target, _) = run_session(p).await;
        assert_eq!(
            target.unwrap(),
            TargetAddr::Domain {
                host: "localhost".into(),
                port: 443
            }
        );
    }

    #[tokio::test]
    async fn bind_命令回_5b() {
        let mut p = vec![0x04, 0x02, 0x00, 0x50, 1, 2, 3, 4];
        p.extend_from_slice(b"\0");
        let (target, reply) = run_session(p).await;
        assert!(matches!(target, Err(Error::UnsupportedCommand(0x02))));
        assert_eq!(reply, vec![0x00, 0x5B, 0x00, 0x00, 0, 0, 0, 0]);
    }

    #[tokio::test]
    async fn 版本错误被拒绝() {
        let mut p = vec![0x05, 0x01, 0x00, 0x50, 127, 0, 0, 1];
        p.extend_from_slice(b"\0");
        let (target, _) = run_session(p).await;
        assert!(matches!(target, Err(Error::BadVersion(0x05))));
    }

    #[tokio::test]
    async fn 应答格式八字节() {
        let (mut c, mut s) = duplex(32);
        write_reply(&mut s, true).await.unwrap();
        let mut buf = [0u8; 8];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, [0x00, 0x5A, 0x00, 0x00, 0, 0, 0, 0]);
    }

    #[tokio::test]
    async fn userid_认证只校验用户名() {
        let auth = vec![AuthUser {
            user: "u".into(),
            pwd: "ignored".into(),
        }];
        // USERID 匹配 → 通过
        let mut p = vec![0x04, 0x01, 0x17, 0xD4, 127, 0, 0, 1];
        p.extend_from_slice(b"u\0");
        let (target, _) = run_session_auth(p, &auth).await;
        assert!(target.is_ok());

        // USERID 不匹配 → 5C
        let mut p = vec![0x04, 0x01, 0x17, 0xD4, 127, 0, 0, 1];
        p.extend_from_slice(b"x\0");
        let (target, reply) = run_session_auth(p, &auth).await;
        assert!(matches!(target, Err(Error::AuthFailed)));
        assert_eq!(reply, vec![0x00, 0x5C, 0x00, 0x00, 0, 0, 0, 0]);

        // USERID 为空 → 5C
        let mut p = vec![0x04, 0x01, 0x17, 0xD4, 127, 0, 0, 1];
        p.extend_from_slice(b"\0");
        let (target, reply) = run_session_auth(p, &auth).await;
        assert!(matches!(target, Err(Error::AuthFailed)));
        assert_eq!(reply[1], 0x5C);
    }
}
