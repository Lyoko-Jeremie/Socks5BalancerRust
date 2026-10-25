//! 上游池与选择策略。
//!
//! - 解析在启动期完成一次（域名 → `SocketAddr`），热路径不做 DNS；
//! - 选择策略对齐 C++ 版 5 条规则（`loop` / `random` / `one_by_one` /
//!   `change_by_time` / `force_only_one`），状态用轻量 `Mutex` 保护
//!   （每连接只选一次，不在转发热路径上）；
//! - `is_available` 回调为 M1.4 的健康状态接入点；M1.3 阶段恒为可用。

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
    /// 在健康注册表中的下标（与启用上游的解析顺序一致）。
    pub health_index: usize,
}

/// 上游选择规则（与 C++ `upstreamSelectRule` 同名）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectRule {
    /// 轮询。
    Loop,
    /// 随机。
    Random,
    /// 固定一个（当前不可用才顺延）。
    OneByOne,
    /// 固定一个，超过 `serverChangeTime` 才换下一个。
    ChangeByTime,
    /// 强制只用当前一个（不可用则拒绝）。
    ForceOnlyOne,
}

impl SelectRule {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "loop" => Some(Self::Loop),
            "random" => Some(Self::Random),
            "one_by_one" | "onebyone" => Some(Self::OneByOne),
            "change_by_time" | "changebytime" => Some(Self::ChangeByTime),
            "force_only_one" | "forceonlyone" => Some(Self::ForceOnlyOne),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Loop => "loop",
            Self::Random => "random",
            Self::OneByOne => "one_by_one",
            Self::ChangeByTime => "change_by_time",
            Self::ForceOnlyOne => "force_only_one",
        }
    }
}

/// 选择器内部状态（只在选择时短暂加锁）。
#[derive(Debug)]
struct SelectState {
    /// loop 规则的下一个起点。
    cursor: usize,
    /// one_by_one / change_by_time / force_only_one 的当前上游下标。
    current: usize,
    /// change_by_time 上次切换时间。
    last_change: Instant,
}

/// 上游选择器。
#[derive(Debug)]
pub struct UpstreamSelector {
    upstreams: Vec<Upstream>,
    rule: SelectRule,
    change_interval: Duration,
    state: Mutex<SelectState>,
}

impl UpstreamSelector {
    pub fn new(upstreams: Vec<Upstream>, rule: SelectRule, change_interval: Duration) -> Self {
        Self {
            upstreams,
            rule,
            change_interval,
            state: Mutex::new(SelectState {
                cursor: 0,
                current: 0,
                last_change: Instant::now(),
            }),
        }
    }

    pub fn len(&self) -> usize {
        self.upstreams.len()
    }

    pub fn is_empty(&self) -> bool {
        self.upstreams.is_empty()
    }

    pub fn rule(&self) -> SelectRule {
        self.rule
    }

    /// 按规则选择一个可用上游；全部不可用返回 `None`（调用方应快速拒绝）。
    ///
    /// `is_available` 当前恒为 true（M1.4 接入健康状态）。
    pub fn next(&self, is_available: &dyn Fn(usize) -> bool) -> Option<&Upstream> {
        if self.is_empty() {
            return None;
        }
        let n = self.upstreams.len();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());

        let idx = match self.rule {
            SelectRule::Loop => {
                let start = state.cursor % n;
                let mut chosen = None;
                for k in 0..n {
                    let i = (start + k) % n;
                    if is_available(i) {
                        chosen = Some(i);
                        break;
                    }
                }
                let i = chosen?;
                state.cursor = (i + 1) % n;
                i
            }
            SelectRule::Random => {
                let available: Vec<usize> = (0..n).filter(|i| is_available(*i)).collect();
                if available.is_empty() {
                    return None;
                }
                available[fastrand::usize(..available.len())]
            }
            SelectRule::OneByOne => {
                if is_available(state.current) {
                    state.current
                } else {
                    // 顺延到"当前之后"的第一个可用上游（绕回）
                    let i = (0..n)
                        .map(|k| (state.current + 1 + k) % n)
                        .find(|i| is_available(*i))?;
                    state.current = i;
                    i
                }
            }
            SelectRule::ChangeByTime => {
                if is_available(state.current) && state.last_change.elapsed() < self.change_interval
                {
                    state.current
                } else {
                    // 到期（或当前不可用）：切到"当前之后"的第一个可用上游
                    let i = (0..n)
                        .map(|k| (state.current + 1 + k) % n)
                        .find(|i| is_available(*i))?;
                    state.current = i;
                    state.last_change = Instant::now();
                    i
                }
            }
            SelectRule::ForceOnlyOne => {
                let i = state.current.min(n - 1);
                if is_available(i) {
                    state.current = i;
                    i
                } else {
                    return None;
                }
            }
        };

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
                // 健康注册表基于"解析成功"的列表，下标即 ok.len()
                let health_index = ok.len();
                info!(upstream = %c.name, host = %c.host, port = c.port, addr = %addr, "上游解析成功");
                ok.push(Upstream {
                    name: c.name.clone(),
                    addr,
                    auth_user: c.auth_user.clone(),
                    auth_pwd: c.auth_pwd.clone(),
                    health_index,
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
            health_index: 0,
        }
    }

    fn selector(rule: SelectRule, interval: Duration) -> UpstreamSelector {
        UpstreamSelector::new(vec![u("a", 1), u("b", 2), u("c", 3)], rule, interval)
    }

    fn all_available(_: usize) -> bool {
        true
    }

    #[test]
    fn loop_规则轮询且跳过不可用() {
        let s = selector(SelectRule::Loop, Duration::from_secs(60));
        let names: Vec<&str> = (0..7)
            .map(|_| s.next(&all_available).unwrap().name.as_str())
            .collect();
        assert_eq!(names, ["a", "b", "c", "a", "b", "c", "a"]);

        // 把 b 标为不可用：序列中不应再出现 b
        let avail = |i: usize| i != 1;
        let names: Vec<&str> = (0..6)
            .map(|_| s.next(&avail).unwrap().name.as_str())
            .collect();
        assert!(names.iter().all(|n| *n != "b"), "实际 {names:?}");
    }

    #[test]
    fn random_规则只返回可用项() {
        let s = selector(SelectRule::Random, Duration::from_secs(60));
        let avail = |i: usize| i == 2;
        for _ in 0..50 {
            assert_eq!(s.next(&avail).unwrap().name, "c");
        }
    }

    #[test]
    fn one_by_one_粘住当前上游() {
        let s = selector(SelectRule::OneByOne, Duration::from_secs(60));
        assert_eq!(s.next(&all_available).unwrap().name, "a");
        assert_eq!(s.next(&all_available).unwrap().name, "a");
        // 当前不可用 → 顺延到下一个可用
        let avail = |i: usize| i != 0;
        assert_eq!(s.next(&avail).unwrap().name, "b");
        assert_eq!(s.next(&avail).unwrap().name, "b");
    }

    #[test]
    fn change_by_time_到期才切换() {
        let s = selector(SelectRule::ChangeByTime, Duration::from_millis(50));
        assert_eq!(s.next(&all_available).unwrap().name, "a");
        assert_eq!(s.next(&all_available).unwrap().name, "a");
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(s.next(&all_available).unwrap().name, "b");
        assert_eq!(s.next(&all_available).unwrap().name, "b");
    }

    #[test]
    fn force_only_one_不可用则拒绝() {
        let s = selector(SelectRule::ForceOnlyOne, Duration::from_secs(60));
        assert_eq!(s.next(&all_available).unwrap().name, "a");
        // 当前上游不可用 → 返回 None（不切换到其它上游）
        let avail = |i: usize| i != 0;
        assert!(s.next(&avail).is_none());
    }

    #[test]
    fn 全部不可用返回_none() {
        let s = selector(SelectRule::Loop, Duration::from_secs(60));
        let none = |_: usize| false;
        assert!(s.next(&none).is_none());
    }

    #[test]
    fn 规则解析与序列化一致() {
        for rule in [
            SelectRule::Loop,
            SelectRule::Random,
            SelectRule::OneByOne,
            SelectRule::ChangeByTime,
            SelectRule::ForceOnlyOne,
        ] {
            assert_eq!(SelectRule::parse(rule.as_str()), Some(rule));
        }
        assert_eq!(SelectRule::parse("LOOP"), Some(SelectRule::Loop));
        assert_eq!(SelectRule::parse("one-by-one"), None);
    }

    #[test]
    fn 空池返回_none() {
        let s = UpstreamSelector::new(vec![], SelectRule::Loop, Duration::from_secs(60));
        assert!(s.is_empty());
        assert!(s.next(&all_available).is_none());
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
