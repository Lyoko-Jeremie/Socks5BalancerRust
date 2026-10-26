//! HTTP 代理协议解析（M2：CONNECT 与 absolute-form 正向代理）。
//!
//! 对齐 docs/03 §6.2 的语义：
//! - **CONNECT**：丢弃整个请求头，回 `200 Connection established`，之后双向透传；
//!   请求头之后若已有 early data（客户端粘包），必须原样转发给目标（前缀回放）。
//! - **absolute-form**（`GET http://host/path HTTP/1.1`）：**不回 200**，把请求转给目标；
//!   本实现改写为 origin-form 并去掉 `Proxy-*` 逐跳头（比 C++ 原样转发更标准，
//!   RFC 7230 §5.3.2 允许服务器接受 absolute-form，但改写兼容性更好）。
//! - 非 CONNECT 且非 absolute-form → `400`（C++ 分支A 会把明文塞进隧道，属缺陷 H-1）。
//!
//! 解析用 `httparse`（增量、零分配）；不读取 body，头部之后的字节全部作为前缀回放。

use std::io;
use std::net::{IpAddr, SocketAddr};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::config::AuthUser;
use crate::handshake::TargetAddr;

/// HTTP 请求头最大字节数（防慢速/超大头部攻击）。
pub const MAX_HEAD_BYTES: usize = 16 * 1024;

/// 解析出的请求类型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestKind {
    /// `CONNECT host:port HTTP/1.1`
    Connect(TargetAddr),
    /// `GET http://host/path HTTP/1.1`（转入 origin-form 后转发给目标）
    Forward {
        method: String,
        /// 隧道目标（由 absolute-form 的 authority 得出）
        target: TargetAddr,
        /// 改写后的完整请求头（含早期数据之前的头；prefix 由调用方拼接）
        rewritten_head: Vec<u8>,
    },
}

impl RequestKind {
    /// 日志/统计用摘要（M3 控制面会复用）。
    #[allow(dead_code)]
    pub fn describe(&self) -> String {
        match self {
            Self::Connect(target) => format!("CONNECT {target}"),
            Self::Forward { method, target, .. } => {
                format!("{method} {target} (absolute-form)")
            }
        }
    }
}

/// HTTP 解析错误。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O 错误: {0}")]
    Io(#[from] io::Error),
    #[error("HTTP 头部超过上限（{0} 字节）")]
    HeadTooLarge(usize),
    #[error("HTTP 头不完整")]
    Incomplete,
    #[error("非法的 HTTP 请求: {0}")]
    BadRequest(&'static str),
}

/// 读取 HTTP 请求头（直到 `\r\n\r\n`，含结尾空行）。
///
/// 返回 `(buf, head_len)`：`buf[..head_len]` 是头部，`buf[head_len..]` 是读到的 early data
/// （调用方必须作为前缀回放给目标）。
pub async fn read_head<S>(stream: &mut S, max_bytes: usize) -> Result<(Vec<u8>, usize), Error>
where
    S: AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(1024);
    loop {
        if let Some(pos) = find_head_end(&buf) {
            return Ok((buf, pos));
        }
        if buf.len() >= max_bytes {
            return Err(Error::HeadTooLarge(max_bytes));
        }
        let mut chunk = [0u8; 1024];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(Error::Incomplete);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// 查找 `\r\n\r\n` 结束位置（返回头部长度，含尾部空行）；兼容只用 `\n\n` 的客户端。
fn find_head_end(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len() {
        if buf[i..].starts_with(b"\r\n\r\n") {
            return Some(i + 4);
        }
        if buf[i..].starts_with(b"\n\n") {
            return Some(i + 2);
        }
    }
    None
}

/// 入站 HTTP 代理认证（对齐 C++ 增强模式）：校验 `Proxy-Authorization` / `Authorization` 的 Basic 凭据。
///
/// `auth` 为空表示不需要认证。返回 `false` 时调用方应回 407 + `Proxy-Authenticate` 并关闭连接。
pub fn check_proxy_auth(head: &[u8], auth: &[AuthUser]) -> bool {
    if auth.is_empty() {
        return true;
    }
    let Some((user, pwd)) = proxy_basic_credentials(head) else {
        return false;
    };
    auth.iter().any(|a| a.user == user && a.pwd == pwd)
}

/// 从请求头中提取 Basic 凭据；优先 `Proxy-Authorization`，其次 `Authorization`。
fn proxy_basic_credentials(head: &[u8]) -> Option<(String, String)> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    req.parse(head).ok()?;
    let mut plain = None;
    for h in req.headers.iter() {
        if h.name.eq_ignore_ascii_case("proxy-authorization") {
            // Proxy-Authorization 优先；解析失败时仍允许回落到 Authorization
            if let Some(v) = parse_basic(h.value) {
                return Some(v);
            }
        } else if h.name.eq_ignore_ascii_case("authorization") {
            plain = parse_basic(h.value);
        }
    }
    plain
}

/// 解析 `Basic <base64>`（scheme 大小写不敏感，对齐 RFC 与 docs/03 D11）。
fn parse_basic(value: &[u8]) -> Option<(String, String)> {
    let text = std::str::from_utf8(value).ok()?.trim();
    let (scheme, token) = text.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = decode_base64(token.trim())?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, pwd) = text.split_once(':')?;
    Some((user.to_string(), pwd.to_string()))
}

/// RFC 4648 标准 base64 解码（仅用于认证头，输入很小；不引入额外依赖）。
fn decode_base64(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &c in input.as_bytes() {
        if c == b'=' {
            break;
        }
        if c.is_ascii_whitespace() {
            continue;
        }
        let v = val(c)?;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1u32 << bits) - 1;
        }
    }
    Some(out)
}

/// 写 `407 Proxy Authentication Required` 响应并关闭连接。
///
/// 不做重试循环：C++ 曾在 407 后未消费请求导致单连接紧循环，这里一次性回 407 即关闭。
pub async fn write_proxy_auth_required<S: AsyncWrite + Unpin>(stream: &mut S) -> io::Result<()> {
    let body = "HTTP/1.1 407 Proxy Authentication Required\r\n\
                Proxy-Authenticate: Basic realm=\"Socks5BalancerRust\"\r\n\
                Content-Length: 0\r\n\
                Connection: close\r\n\r\n";
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await
}

/// 解析请求头（`head` 必须恰好是头部，不含 early data）。
pub fn parse(head: &[u8]) -> Result<RequestKind, Error> {
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    let status = req
        .parse(head)
        .map_err(|_| Error::BadRequest("httparse 解析失败"))?;
    if status.is_partial() {
        return Err(Error::Incomplete);
    }
    let method = req
        .method
        .ok_or(Error::BadRequest("缺少请求方法"))?
        .to_string();
    let path = req.path.ok_or(Error::BadRequest("缺少请求路径"))?;
    let version = req.version.unwrap_or(1);

    if method.eq_ignore_ascii_case("CONNECT") {
        let target = parse_authority(path)?;
        return Ok(RequestKind::Connect(target));
    }

    // 仅支持 absolute-form（正向代理）；非绝对 URI 视为非法
    let uri: http::Uri = path
        .parse()
        .map_err(|_| Error::BadRequest("无法解析绝对 URI"))?;
    if uri.scheme_str() != Some("http") {
        return Err(Error::BadRequest("仅支持 http:// absolute-form"));
    }
    let authority = uri
        .authority()
        .ok_or(Error::BadRequest("absolute-form 缺少 authority"))?;
    let host = authority.host().to_string();
    if host.is_empty() {
        return Err(Error::BadRequest("authority 缺少 host"));
    }
    let port = authority.port_u16().unwrap_or(80);
    if port == 0 {
        return Err(Error::BadRequest("authority 端口为 0"));
    }
    let target = match host.parse::<IpAddr>() {
        Ok(ip) => TargetAddr::Ip(SocketAddr::new(ip, port)),
        Err(_) => TargetAddr::Domain { host, port },
    };

    // 改写为 origin-form，并去掉 Proxy-* 逐跳头
    let origin = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let rewritten = rewrite_forward(version, &method, origin, &mut headers);
    Ok(RequestKind::Forward {
        method,
        target,
        rewritten_head: rewritten,
    })
}

/// 解析 `host:port`（支持 `[::1]:443` 与裸 IPv6）。
fn parse_authority(authority: &str) -> Result<TargetAddr, Error> {
    if let Some(rest) = authority.strip_prefix('[') {
        // [::1]:443
        let (host, tail) = rest
            .split_once(']')
            .ok_or(Error::BadRequest("IPv6 authority 缺少 ]"))?;
        let port = tail
            .strip_prefix(':')
            .ok_or(Error::BadRequest("CONNECT 缺少端口"))?
            .parse::<u16>()
            .map_err(|_| Error::BadRequest("端口非法"))?;
        return Ok(TargetAddr::Ip(SocketAddr::new(
            host.parse::<IpAddr>()
                .map_err(|_| Error::BadRequest("IPv6 地址非法"))?,
            port,
        )));
    }
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or(Error::BadRequest("CONNECT 缺少端口"))?;
    let port = port
        .parse::<u16>()
        .map_err(|_| Error::BadRequest("端口非法"))?;
    if port == 0 {
        return Err(Error::BadRequest("端口不能为 0"));
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => Ok(TargetAddr::Ip(SocketAddr::new(ip, port))),
        Err(_) => Ok(TargetAddr::Domain {
            host: host.to_string(),
            port,
        }),
    }
}

/// 重建 origin-form 请求头：请求行使用 path，头字段保留（去掉 Proxy-*）。
fn rewrite_forward(
    version: u8,
    method: &str,
    path: &str,
    headers: &mut [httparse::Header<'_>],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(512);
    out.extend_from_slice(method.as_bytes());
    out.push(b' ');
    out.extend_from_slice(path.as_bytes());
    out.extend_from_slice(format!(" HTTP/1.{version}\r\n").as_bytes());
    for h in headers.iter() {
        if h.name.is_empty() {
            continue;
        }
        if h.name.eq_ignore_ascii_case("Proxy-Connection")
            || h.name.eq_ignore_ascii_case("Proxy-Authorization")
        {
            continue;
        }
        out.extend_from_slice(h.name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(h.value);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// 写 CONNECT 成功应答。
pub async fn write_connect_ok<S: tokio::io::AsyncWrite + Unpin>(stream: &mut S) -> io::Result<()> {
    stream
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await?;
    tokio::io::AsyncWriteExt::flush(stream).await
}

/// 写错误应答（CONNECT/forward 通用）。
pub async fn write_error<S: tokio::io::AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    reason: &str,
) -> io::Result<()> {
    let resp =
        format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    stream.write_all(resp.as_bytes()).await?;
    tokio::io::AsyncWriteExt::flush(stream).await
}

/// 供日志使用的请求摘要（M3 控制面复用）。
#[allow(dead_code)]
pub fn describe(kind: &RequestKind) -> String {
    kind.describe()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    #[test]
    fn 查找头部结束位置() {
        let buf = head("GET / HTTP/1.1\r\nHost: a\r\n\r\nBODY");
        assert_eq!(
            find_head_end(&buf),
            Some(head("GET / HTTP/1.1\r\nHost: a\r\n\r\n").len())
        );
        assert_eq!(find_head_end(&head("GET / HTTP/1.1\r\nHost: a\r\n")), None);
        // 只带 \n 的兼容
        let buf = head("GET / HTTP/1.0\nHost: a\n\nBODY");
        assert_eq!(find_head_end(&buf), Some(buf.len() - 4));
    }

    #[test]
    fn 解析_connect_ipv4() {
        let h = head("CONNECT 127.0.0.1:6100 HTTP/1.1\r\nHost: 127.0.0.1:6100\r\n\r\n");
        let kind = parse(&h).unwrap();
        match kind {
            RequestKind::Connect(TargetAddr::Ip(addr)) => {
                assert_eq!(addr.to_string(), "127.0.0.1:6100");
            }
            other => panic!("应为 IP 目标: {other:?}"),
        }
    }

    #[test]
    fn 解析_connect_ipv6() {
        let h = head("CONNECT [::1]:443 HTTP/1.1\r\n\r\n");
        match parse(&h).unwrap() {
            RequestKind::Connect(TargetAddr::Ip(addr)) => assert_eq!(addr.port(), 443),
            other => panic!("应为 IPv6 目标: {other:?}"),
        }
    }

    #[test]
    fn 解析_connect_域名() {
        let h = head("CONNECT example.com:443 HTTP/1.1\r\n\r\n");
        assert_eq!(
            parse(&h).unwrap(),
            RequestKind::Connect(TargetAddr::Domain {
                host: "example.com".into(),
                port: 443
            })
        );
    }

    #[test]
    fn 解析_forward_改写为_origin_form() {
        let h = head(
            "GET http://example.com/a/b?x=1 HTTP/1.1\r\nHost: example.com\r\nProxy-Connection: keep-alive\r\nProxy-Authorization: Basic xx\r\nUser-Agent: t\r\n\r\n",
        );
        let kind = parse(&h).unwrap();
        match kind {
            RequestKind::Forward {
                method,
                target,
                rewritten_head,
            } => {
                assert_eq!(method, "GET");
                assert_eq!(
                    target,
                    TargetAddr::Domain {
                        host: "example.com".into(),
                        port: 80
                    }
                );
                let text = String::from_utf8(rewritten_head).unwrap();
                assert!(
                    text.starts_with("GET /a/b?x=1 HTTP/1.1\r\n"),
                    "实际: {text}"
                );
                assert!(text.contains("Host: example.com\r\n"));
                assert!(text.contains("User-Agent: t\r\n"));
                assert!(!text.contains("Proxy-"), "Proxy-* 头应被移除: {text}");
                assert!(text.ends_with("\r\n\r\n"));
            }
            other => panic!("应为 forward: {other:?}"),
        }
    }

    #[test]
    fn 非_absolute_form_报错() {
        let h = head("GET /index.html HTTP/1.1\r\nHost: a\r\n\r\n");
        assert!(matches!(parse(&h), Err(Error::BadRequest(_))));
    }

    #[test]
    fn 非_http_scheme_报错() {
        let h = head("GET https://example.com/ HTTP/1.1\r\nHost: a\r\n\r\n");
        assert!(matches!(parse(&h), Err(Error::BadRequest(_))));
    }

    #[test]
    fn 缺少端口报错() {
        let h = head("CONNECT example.com HTTP/1.1\r\n\r\n");
        assert!(matches!(parse(&h), Err(Error::BadRequest(_))));
    }

    #[test]
    fn 代理认证_basic_校验() {
        let auth = vec![AuthUser {
            user: "alice".into(),
            pwd: "secret".into(),
        }];
        // base64("alice:secret") = "YWxpY2U6c2VjcmV0"
        let mut h = Vec::new();
        h.extend_from_slice(
            b"GET http://x/ HTTP/1.1\r\nHost: x\r\nProxy-Authorization: Basic YWxpY2U6c2VjcmV0\r\n\r\n",
        );
        assert!(check_proxy_auth(&h, &auth), "正确凭据应通过");
        // scheme 大小写不敏感（docs/03 D11）
        let mut h = Vec::new();
        h.extend_from_slice(
            b"GET http://x/ HTTP/1.1\r\nHost: x\r\nProxy-Authorization: basic YWxpY2U6c2VjcmV0\r\n\r\n",
        );
        assert!(check_proxy_auth(&h, &auth), "basic 小写应通过");
        // 错误凭据
        let mut h = Vec::new();
        h.extend_from_slice(
            b"GET http://x/ HTTP/1.1\r\nHost: x\r\nProxy-Authorization: Basic YWxpY2U6d3Jvbmc=\r\n\r\n",
        );
        assert!(!check_proxy_auth(&h, &auth), "错误凭据应拒绝");
        // 无认证头
        let mut h = Vec::new();
        h.extend_from_slice(b"GET http://x/ HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(!check_proxy_auth(&h, &auth), "缺少认证头应拒绝");
        assert!(check_proxy_auth(&h, &[]), "未配置认证时应通过");
    }

    #[test]
    fn base64_解码() {
        assert_eq!(decode_base64("YWxpY2U6c2VjcmV0").unwrap(), b"alice:secret");
        assert_eq!(decode_base64("YQ==").unwrap(), b"a");
        assert_eq!(decode_base64("").unwrap(), b"");
        assert!(decode_base64("!!").is_none());
    }

    #[tokio::test]
    async fn 读取头部并保留早期数据() {
        use tokio::io::AsyncWriteExt;
        let (mut client, mut server) = tokio::io::duplex(4096);
        let payload = b"CONNECT a:1 HTTP/1.1\r\n\r\nEARLY-DATA";
        client.write_all(payload).await.unwrap();
        let (buf, head_len) = read_head(&mut server, MAX_HEAD_BYTES).await.unwrap();
        assert_eq!(&buf[..head_len], b"CONNECT a:1 HTTP/1.1\r\n\r\n");
        assert_eq!(&buf[head_len..], b"EARLY-DATA");
    }
}
