//! 入站协议处理。
//!
//! M1：SOCKS5 服务端协商（0x00/0x02 + RFC1929 + CONNECT）。
//! M2：首包嗅探分派 + SOCKS4/4a + HTTP CONNECT / absolute-form 正向代理。
//!
//! 与 C++ 默认模式的关键差异（见 docs/07 ADR-004 与 docs/03 §6）：
//! - C++ 默认只认首包恰为 `05 01 00` 并整段透传；Rust 版**解析**方法协商，支持
//!   `NMETHODS>1`（例如 `05 02 00 02`），并在需要认证时作为服务端终结认证；
//! - CONNECT 请求由本进程解析出目标地址，再以 SOCKS5 客户端身份向上游发起 CONNECT，
//!   因此域名可交给上游解析（socks5h 语义）；
//! - **成功/失败应答都由本进程写回**，且在"上游握手成功之后"才写成功应答；
//! - 支持混合端口：同一监听端口按首包分派 SOCKS5 / SOCKS4 / HTTP（M2）。

pub mod http;
pub mod socks4;
pub mod socks5;

pub use socks5::TargetAddr;

/// 入站协议类型（首包嗅探结果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Socks5,
    Socks4,
    Http,
}

impl Protocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Socks5 => "socks5",
            Self::Socks4 => "socks4",
            Self::Http => "http",
        }
    }
}

/// 根据首字节判断协议。
///
/// - `0x05` → SOCKS5
/// - `0x04` → SOCKS4/4a
/// - 其它 → HTTP（C++ 用 `P/G/H/D/O/T` 等首字母判断，这里更宽松，交给 HTTP 解析器拒绝）
pub fn sniff_first_byte(first: u8) -> Protocol {
    match first {
        0x05 => Protocol::Socks5,
        0x04 => Protocol::Socks4,
        _ => Protocol::Http,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 首字节分派() {
        assert_eq!(sniff_first_byte(0x05), Protocol::Socks5);
        assert_eq!(sniff_first_byte(0x04), Protocol::Socks4);
        assert_eq!(sniff_first_byte(b'G'), Protocol::Http);
        assert_eq!(sniff_first_byte(b'C'), Protocol::Http);
        assert_eq!(sniff_first_byte(0x16), Protocol::Http); // TLS ClientHello → 交给 HTTP 解析器拒绝
    }
}
