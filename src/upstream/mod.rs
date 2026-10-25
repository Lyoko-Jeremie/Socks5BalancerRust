//! 上游池与选择策略。
//!
//! M0 范围：**启动期解析一次 + round-robin**。
//! - 解析移出热路径（对应 docs/02 §5.2 的 DNS 瓶颈）；
//! - 选择用原子游标，无锁（对应 docs/02 §7 的全局锁瓶颈）；
//! - M1 起替换为 5 条规则 + 健康状态（docs/07 ADR-002/003 与 04 §4）。

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};

use tracing::{info, warn};

use crate::config::UpstreamConfig;

/// 已解析的上游。
#[derive(Debug, Clone)]
pub struct Upstream {
    pub name: String,
    pub addr: SocketAddr,
    /// 上游 SOCKS5 认证（M1 起真正使用；None 表示无认证）。
    pub auth_user: Option<String>,
    pub auth_pwd: Option<String>,
}

/// 上游选择器：当前实现为无锁 round-robin。
#[derive(Debug)]
pub struct UpstreamSelector {
    upstreams: Vec<Upstream>,
    cursor: AtomicUsize,
}

impl UpstreamSelector {
    pub fn new(upstreams: Vec<Upstream>) -> Self {
        Self {
            upstreams,
            cursor: AtomicUsize::new(0),
        }
    }

    pub fn len(&self) -> usize {
        self.upstreams.len()
    }

    pub fn is_empty(&self) -> bool {
        self.upstreams.is_empty()
    }

    /// 取下一个上游（round-robin）。
    pub fn next(&self) -> Option<&Upstream> {
        if self.is_empty() {
            return None;
        }
        let idx = self.cursor.fetch_add(1, Ordering::Relaxed) % self.upstreams.len();
        self.upstreams.get(idx)
    }
}

/// 启动期解析全部启用的上游。
///
/// 返回 `(解析成功的上游, 失败说明)`；域名解析失败不阻止启动（其它上游可用），
/// 但全部失败时由调用方决定是否退出。
pub async fn resolve_all(cfgs: &[UpstreamConfig]) -> (Vec<Upstream>, Vec<String>) {
    let mut ok = Vec::new();
    let mut failed = Vec::new();

    for c in cfgs.iter().filter(|c| c.enabled) {
        match resolve_one(&c.host, c.port).await {
            Ok(addr) => {
                info!(upstream = %c.name, host = %c.host, port = c.port, addr = %addr, "上游解析成功");
                ok.push(Upstream {
                    name: c.name.clone(),
                    addr,
                    auth_user: c.auth_user.clone(),
                    auth_pwd: c.auth_pwd.clone(),
                });
            }
            Err(e) => {
                let msg = format!("上游 {} ({}:{}) 解析失败: {e}", c.name, c.host, c.port);
                warn!("{msg}");
                failed.push(msg);
            }
        }
    }

    (ok, failed)
}

async fn resolve_one(host: &str, port: u16) -> io::Result<SocketAddr> {
    // IP 字面量直接返回，避免任何解析调用（热路径无 DNS 的前提之一）。
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    let mut addrs = tokio::net::lookup_host((host, port)).await?;
    addrs.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("DNS 未返回任何地址: {host}:{port}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(name: &str, port: u16) -> Upstream {
        Upstream {
            name: name.to_string(),
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            auth_user: None,
            auth_pwd: None,
        }
    }

    #[test]
    fn round_robin_顺序正确() {
        let s = UpstreamSelector::new(vec![u("a", 1), u("b", 2), u("c", 3)]);
        let names: Vec<&str> = (0..7).map(|_| s.next().unwrap().name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c", "a", "b", "c", "a"]);
    }

    #[test]
    fn 空池返回_none() {
        let s = UpstreamSelector::new(vec![]);
        assert!(s.is_empty());
        assert!(s.next().is_none());
    }

    #[tokio::test]
    async fn ip_字面量无需_dns() {
        let addr = resolve_one("127.0.0.1", 8080).await.unwrap();
        assert_eq!(addr.port(), 8080);
        assert!(addr.ip().is_loopback());
    }

    #[tokio::test]
    async fn localhost_可解析() {
        let addr = resolve_one("localhost", 8080).await;
        assert!(addr.is_ok(), "localhost 应可解析: {addr:?}");
    }
}
