//! 上游池与选择策略。
//!
//! - 解析在启动期完成一次（域名 → `SocketAddr`），热路径不做 DNS；
//! - 初始 `disable: true` 的上游**保留在池中**（`enabled=false`），控制面可见并可启用；
//! - 选择策略对齐 C++ 版 5 条规则（`loop` / `random` / `one_by_one` /
//!   `change_by_time` / `force_only_one`），状态用轻量 `Mutex` 保护
//!   （每连接只选一次，不在转发热路径上）；
//! - `is_available` 回调为 M1.4 的健康状态接入点。

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::config::UpstreamConfig;
use tracing::{info, warn};

/// 已解析的上游。
#[derive(Debug, Clone)]
pub struct Upstream {
    pub name: String,
    /// 配置中的原始 host（控制面展示用；`addr` 是解析后的地址）。
    pub host: String,
    /// 配置端口。
    pub port: u16,
    /// 解析后的上游地址；DNS TTL 刷新任务会原地更新（`Arc` 与所有持有者共享）。
    pub addr: Arc<Mutex<SocketAddr>>,
    /// 上游 SOCKS5 认证（M1 起真正使用；None 表示无认证）。
    pub auth_user: Option<String>,
    pub auth_pwd: Option<String>,
    /// 在健康注册表中的下标（与"解析成功上游"的顺序一致）。
    pub health_index: usize,
    /// 配置是否启用；作为人工启停的初始值（控制面可再改）。
    pub enabled: bool,
}

impl Upstream {
    /// 读取当前解析地址（DNS 刷新后可能变化）。
    pub fn addr(&self) -> SocketAddr {
        *self.addr.lock().unwrap_or_else(|e| e.into_inner())
    }
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
            // C++ 的 `inherit` 表示"继承上层规则"（全局配置用它 = 交给按 client/listen 的
            // 维度规则）。Rust 版没有"继承"概念：维度覆盖由控制面
            // `set_rule(scope=client|listen, target, index)` 实现，因此这里按 random 处理，
            // 由 config 层打印提示。这样 C++ 旧配置迁移不会启动失败。
            "inherit" => Some(Self::Random),
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

    /// 稳定编码（供控制面运行期改规则时用原子变量存储）。
    pub fn as_u8(&self) -> u8 {
        match self {
            Self::Loop => 0,
            Self::Random => 1,
            Self::OneByOne => 2,
            Self::ChangeByTime => 3,
            Self::ForceOnlyOne => 4,
        }
    }

    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Loop),
            1 => Some(Self::Random),
            2 => Some(Self::OneByOne),
            3 => Some(Self::ChangeByTime),
            4 => Some(Self::ForceOnlyOne),
            _ => None,
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
    /// 人工启停（M3 控制面 `enable_upstream` / `disable_upstream`）；索引与上游一致。
    enabled: Vec<bool>,
    /// 每客户端粘性 hint（客户端 IP → 上次成功的上游下标）；只对粘性型规则生效。
    hints: std::collections::HashMap<IpAddr, usize>,
    /// 全局强制上游（控制面 `set_rule scope=global`；只对粘性型规则生效）。
    global_override: Option<usize>,
    /// 每客户端强制上游（控制面 `set_rule scope=client target=<ip>`）。
    client_overrides: std::collections::HashMap<IpAddr, usize>,
    /// 每监听地址强制上游（控制面 `set_rule scope=listen target=<ip:port>`）。
    listen_overrides: std::collections::HashMap<SocketAddr, usize>,
}

/// per-client hint / 覆盖表容量上限（防止被大量客户端 IP 撑爆内存）。
const MAX_HINTS: usize = 4096;
/// per-listen 覆盖表容量上限（正常部署监听数量很少，这里仅防御性限制）。
const MAX_LISTEN_OVERRIDES: usize = 256;

/// 上游选择器。
#[derive(Debug)]
pub struct UpstreamSelector {
    upstreams: Vec<Upstream>,
    /// 当前规则（M3 起可运行期修改）。
    rule: AtomicU8,
    change_interval: Duration,
    state: Mutex<SelectState>,
}

impl UpstreamSelector {
    pub fn new(upstreams: Vec<Upstream>, rule: SelectRule, change_interval: Duration) -> Self {
        // 人工启停掩码的初始值来自配置（`disable: true` 的上游初始不可选，但可被控制面启用）。
        let enabled: Vec<bool> = upstreams.iter().map(|u| u.enabled).collect();
        Self {
            upstreams,
            rule: AtomicU8::new(rule.as_u8()),
            change_interval,
            state: Mutex::new(SelectState {
                cursor: 0,
                current: 0,
                last_change: Instant::now(),
                enabled,
                hints: std::collections::HashMap::new(),
                global_override: None,
                client_overrides: std::collections::HashMap::new(),
                listen_overrides: std::collections::HashMap::new(),
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
        SelectRule::from_u8(self.rule.load(Ordering::Relaxed)).unwrap_or(SelectRule::Random)
    }

    /// 运行期修改选路规则（M3 控制面 `set_rule`）。
    pub fn set_rule(&self, rule: SelectRule) {
        self.rule.store(rule.as_u8(), Ordering::Relaxed);
    }

    /// 上游是否被人工启用（M3 控制面）。
    pub fn manual_enabled(&self, idx: usize) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.enabled.get(idx).copied().unwrap_or(false)
    }

    /// 人工启用/禁用一个上游；返回 false 表示下标越界。
    pub fn set_manual_enabled(&self, idx: usize, enabled: bool) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.enabled.get_mut(idx) {
            Some(slot) => {
                *slot = enabled;
                true
            }
            None => false,
        }
    }

    /// 上游列表（控制面 `/api/v1/upstreams` 用；响应不得包含密码）。
    pub fn upstreams(&self) -> &[Upstream] {
        &self.upstreams
    }

    /// 按规则选择一个可用上游；全部不可用返回 `None`（调用方应快速拒绝）。
    ///
    /// 可用 = 人工启用 && `is_available`（健康状态）。
    pub fn next(&self, is_available: &dyn Fn(usize) -> bool) -> Option<&Upstream> {
        if self.is_empty() {
            return None;
        }
        let n = self.upstreams.len();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // 可用判定：人工启停 + 健康状态
        let is_ok = |st: &SelectState, i: usize| -> bool {
            st.enabled.get(i).copied().unwrap_or(false) && is_available(i)
        };
        let rule =
            SelectRule::from_u8(self.rule.load(Ordering::Relaxed)).unwrap_or(SelectRule::Random);

        let idx = match rule {
            SelectRule::Loop => {
                let start = state.cursor % n;
                let mut chosen = None;
                for k in 0..n {
                    let i = (start + k) % n;
                    if is_ok(&state, i) {
                        chosen = Some(i);
                        break;
                    }
                }
                let i = chosen?;
                state.cursor = (i + 1) % n;
                i
            }
            SelectRule::Random => {
                let available: Vec<usize> = (0..n).filter(|i| is_ok(&state, *i)).collect();
                if available.is_empty() {
                    return None;
                }
                available[fastrand::usize(..available.len())]
            }
            SelectRule::OneByOne => {
                if is_ok(&state, state.current) {
                    state.current
                } else {
                    // 顺延到"当前之后"的第一个可用上游（绕回）
                    let i = (0..n)
                        .map(|k| (state.current + 1 + k) % n)
                        .find(|i| is_ok(&state, *i))?;
                    state.current = i;
                    i
                }
            }
            SelectRule::ChangeByTime => {
                if is_ok(&state, state.current)
                    && state.last_change.elapsed() < self.change_interval
                {
                    state.current
                } else {
                    // 到期（或当前不可用）：切到"当前之后"的第一个可用上游
                    let i = (0..n)
                        .map(|k| (state.current + 1 + k) % n)
                        .find(|i| is_ok(&state, *i))?;
                    state.current = i;
                    state.last_change = Instant::now();
                    i
                }
            }
            SelectRule::ForceOnlyOne => {
                let i = state.current.min(n - 1);
                if is_ok(&state, i) {
                    state.current = i;
                    i
                } else {
                    return None;
                }
            }
        };

        self.upstreams.get(idx)
    }

    /// 与 [`Self::next`] 相同，但跳过 `exclude` 中的上游（数据面失败重试时使用）。
    ///
    /// 语义：把排除项视为不可用；规则状态推进保持不变。
    pub fn next_excluding(
        &self,
        is_available: &dyn Fn(usize) -> bool,
        exclude: &[usize],
    ) -> Option<&Upstream> {
        if exclude.is_empty() {
            return self.next(is_available);
        }
        let filtered = |i: usize| !exclude.contains(&i) && is_available(i);
        self.next(&filtered)
    }

    /// 按客户端 hint 选择上游。
    ///
    /// 粘性型规则（`one_by_one` / `change_by_time`）下，若该客户端上次成功的上游仍可用，
    /// 直接复用它（对齐 C++ `hintUpstreamIndex` 的每客户端粘性）；其他规则保持原语义。
    pub fn next_for_client(
        &self,
        client: IpAddr,
        is_available: &dyn Fn(usize) -> bool,
    ) -> Option<&Upstream> {
        if matches!(self.rule(), SelectRule::OneByOne | SelectRule::ChangeByTime) {
            let hint = {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state
                    .hints
                    .get(&client)
                    .copied()
                    .filter(|idx| state.enabled.get(*idx).copied().unwrap_or(false))
            };
            if let Some(idx) = hint
                && is_available(idx)
            {
                return self.upstreams.get(idx);
            }
        }
        self.next(is_available)
    }

    /// 带客户端/监听维度的选路：先按维度强制覆盖，再按 hint/规则。
    ///
    /// 覆盖优先级：client > listen > global；与 C++ `forceNowUseServer` 一致，
    /// 只对粘性型规则（`one_by_one` / `change_by_time`）生效；强制目标不可用时回落到常规选路。
    pub fn next_scoped(
        &self,
        client: IpAddr,
        local: SocketAddr,
        is_available: &dyn Fn(usize) -> bool,
    ) -> Option<&Upstream> {
        if matches!(self.rule(), SelectRule::OneByOne | SelectRule::ChangeByTime) {
            let forced = {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state
                    .client_overrides
                    .get(&client)
                    .or_else(|| state.listen_overrides.get(&local))
                    .or(state.global_override.as_ref())
                    .copied()
                    .filter(|idx| state.enabled.get(*idx).copied().unwrap_or(false))
            };
            if let Some(idx) = forced
                && is_available(idx)
            {
                return self.upstreams.get(idx);
            }
        }
        self.next_for_client(client, is_available)
    }

    /// 设置/清除维度强制覆盖（控制面 `set_rule` 的 `scope`/`target`/`index`）。
    ///
    /// - `scope`：`global` / `client` / `listen`；
    /// - `target`：client 为本机看到的客户端 IP，listen 为监听地址 `ip:port`（global 忽略）；
    /// - `index`：`Some(i)` 设置强制，`None` 清除；
    /// - 返回人类可读说明；参数非法/下标越界返回 `Err`。
    pub fn set_scope_override(
        &self,
        scope: &str,
        target: Option<&str>,
        index: Option<usize>,
    ) -> Result<String, String> {
        if let Some(i) = index
            && i >= self.upstreams.len()
        {
            return Err(format!(
                "index 越界: {i}（共 {} 个上游）",
                self.upstreams.len()
            ));
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match scope {
            "global" => match index {
                Some(i) => {
                    state.global_override = Some(i);
                    Ok(format!("已设置全局强制上游 {i}"))
                }
                None => {
                    let was = state.global_override.take().is_some();
                    Ok(if was {
                        "已清除全局强制上游".to_string()
                    } else {
                        "全局没有强制上游".to_string()
                    })
                }
            },
            "client" => {
                let raw =
                    target.ok_or_else(|| "scope=client 需要 target=<客户端IP>".to_string())?;
                let ip: IpAddr = raw
                    .parse()
                    .map_err(|_| format!("target 不是合法 IP: {raw:?}"))?;
                match index {
                    Some(i) => {
                        if state.client_overrides.len() >= MAX_HINTS
                            && !state.client_overrides.contains_key(&ip)
                        {
                            state.client_overrides.clear();
                        }
                        state.client_overrides.insert(ip, i);
                        Ok(format!("已设置客户端 {ip} 强制上游 {i}"))
                    }
                    None => {
                        let was = state.client_overrides.remove(&ip).is_some();
                        Ok(if was {
                            format!("已清除客户端 {ip} 的强制上游")
                        } else {
                            format!("客户端 {ip} 没有强制上游")
                        })
                    }
                }
            }
            "listen" => {
                let raw = target.ok_or_else(|| "scope=listen 需要 target=<ip:port>".to_string())?;
                let addr: SocketAddr = raw
                    .parse()
                    .map_err(|_| format!("target 不是合法 ip:port: {raw:?}"))?;
                match index {
                    Some(i) => {
                        if state.listen_overrides.len() >= MAX_LISTEN_OVERRIDES
                            && !state.listen_overrides.contains_key(&addr)
                        {
                            state.listen_overrides.clear();
                        }
                        state.listen_overrides.insert(addr, i);
                        Ok(format!("已设置监听 {addr} 强制上游 {i}"))
                    }
                    None => {
                        let was = state.listen_overrides.remove(&addr).is_some();
                        Ok(if was {
                            format!("已清除监听 {addr} 的强制上游")
                        } else {
                            format!("监听 {addr} 没有强制上游")
                        })
                    }
                }
            }
            other => Err(format!(
                "未知 scope: {other:?}（支持 global/client/listen）"
            )),
        }
    }

    /// 记录某客户端成功使用的上游（供下次 `next_for_client` 粘性选择）。
    ///
    /// hint 表有容量上限；超过时整体清空（最简单且有界，避免无界增长）。
    pub fn record_hint(&self, client: IpAddr, idx: usize) {
        if idx >= self.upstreams.len() {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.hints.len() >= MAX_HINTS && !state.hints.contains_key(&client) {
            state.hints.clear();
        }
        state.hints.insert(client, idx);
    }
}

/// 启动期解析全部启用的上游。
///
/// 返回 `(解析成功的上游, 失败说明)`；域名解析失败不阻止启动（其它上游可用），
/// 但全部失败时由调用方决定是否退出。
pub async fn resolve_all(cfgs: &[UpstreamConfig]) -> (Vec<Upstream>, Vec<String>) {
    let mut ok = Vec::new();
    let mut failed = Vec::new();

    // 注意：这里**不过滤** disabled 上游——它们保留在池中（`enabled=false`），
    // 以便控制面 `/api/v1/upstreams` 能看到并 `enable_upstream` 启用。
    for c in cfgs.iter() {
        match resolve_one(&c.host, c.port).await {
            Ok(addr) => {
                // 健康注册表基于"解析成功"的列表，下标即 ok.len()
                let health_index = ok.len();
                info!(
                    upstream = %c.name,
                    host = %c.host,
                    port = c.port,
                    addr = %addr,
                    enabled = c.enabled,
                    "上游解析成功"
                );
                ok.push(Upstream {
                    name: c.name.clone(),
                    host: c.host.clone(),
                    port: c.port,
                    addr: Arc::new(Mutex::new(addr)),
                    auth_user: c.auth_user.clone(),
                    auth_pwd: c.auth_pwd.clone(),
                    health_index,
                    enabled: c.enabled,
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
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
    if addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("DNS 未返回任何地址: {host}:{port}"),
        ));
    }
    // 优先 IPv4：双栈机器上常见"IPv6 能建连但数据被 RST"的环境问题，
    // 而 C++ 版直接取 resolver 首项同样会踩坑（见 docs/03 端口陷阱）。
    Ok(addrs
        .iter()
        .find(|a| a.is_ipv4())
        .copied()
        .unwrap_or(addrs[0]))
}

/// 启动 DNS TTL 刷新任务：定期重新解析上游域名并原地更新地址（IP 字面量跳过）。
///
/// 解析失败时保留旧地址（避免一次 DNS 抖动把上游打挂）；地址变化会打印 info 日志。
pub fn spawn_dns_refresher(
    selector: Arc<UpstreamSelector>,
    ttl: Duration,
    cancel: tokio_util::sync::CancellationToken,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(ttl);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // 跳过立即触发
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticker.tick() => {
                    for u in selector.upstreams() {
                        if u.host.parse::<IpAddr>().is_ok() {
                            continue; // IP 字面量无需刷新
                        }
                        match resolve_one(&u.host, u.port).await {
                            Ok(addr) => {
                                let mut guard =
                                    u.addr.lock().unwrap_or_else(|e| e.into_inner());
                                if *guard != addr {
                                    info!(
                                        upstream = %u.name,
                                        host = %u.host,
                                        old = %*guard,
                                        new = %addr,
                                        "上游 DNS 地址已刷新"
                                    );
                                    *guard = addr;
                                }
                            }
                            Err(e) => warn!(
                                upstream = %u.name,
                                host = %u.host,
                                error = %e,
                                "上游 DNS 刷新失败，保留旧地址"
                            ),
                        }
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(name: &str, port: u16) -> Upstream {
        Upstream {
            name: name.to_string(),
            host: "127.0.0.1".to_string(),
            port,
            addr: Arc::new(Mutex::new(SocketAddr::from(([127, 0, 0, 1], port)))),
            auth_user: None,
            auth_pwd: None,
            health_index: 0,
            enabled: true,
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

    #[test]
    fn 排除已尝试的上游() {
        let s = selector(SelectRule::Loop, Duration::from_secs(60));
        // 排除下标 0（"a"）后应选中其它上游
        let first = s.next_excluding(&all_available, &[0]).unwrap();
        assert_ne!(first.name, "a");
        // 全部排除 → 无可用上游
        assert!(s.next_excluding(&all_available, &[0, 1, 2]).is_none());
    }

    #[test]
    fn 初始禁用的上游可被控制面启用() {
        let mut a = u("a", 1);
        let b = u("b", 2);
        a.enabled = false;
        let s = UpstreamSelector::new(vec![a, b], SelectRule::Loop, Duration::from_secs(60));
        // 初始 disabled：不可选，但仍保留在池中（下标 0 可见）
        assert!(!s.manual_enabled(0));
        assert_eq!(s.next(&all_available).unwrap().name, "b");
        // 控制面启用后可被选中
        assert!(s.set_manual_enabled(0, true));
        let names: Vec<&str> = (0..3)
            .map(|_| s.next(&all_available).unwrap().name.as_str())
            .collect();
        assert!(names.contains(&"a"), "启用后应可选中: {names:?}");
    }

    #[test]
    fn 粘性规则按客户端_hint_选择上游() {
        let s = selector(SelectRule::OneByOne, Duration::from_secs(60));
        let c1: IpAddr = "10.0.0.1".parse().unwrap();
        let c2: IpAddr = "10.0.0.2".parse().unwrap();
        s.record_hint(c1, 1);
        s.record_hint(c2, 2);
        assert_eq!(s.next_for_client(c1, &all_available).unwrap().name, "b");
        assert_eq!(s.next_for_client(c2, &all_available).unwrap().name, "c");
        // hint 上游不可用 → 回落到常规规则
        let only_a = |i: usize| i == 0;
        assert_eq!(s.next_for_client(c1, &only_a).unwrap().name, "a");
    }

    #[test]
    fn 上游地址可原地刷新() {
        let s = selector(SelectRule::Loop, Duration::from_secs(60));
        let u = &s.upstreams()[0];
        let old = u.addr();
        let new: SocketAddr = "127.0.0.2:9".parse().unwrap();
        *u.addr.lock().unwrap() = new;
        assert_eq!(u.addr(), new, "DNS 刷新写入应被读取方看到");
        assert_ne!(old, new);
    }

    #[test]
    fn 维度覆盖按客户端与监听生效() {
        let s = selector(SelectRule::OneByOne, Duration::from_secs(60));
        let client: IpAddr = "10.0.0.7".parse().unwrap();
        let other: IpAddr = "10.0.0.8".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:5000".parse().unwrap();

        // client 覆盖优先
        s.set_scope_override("client", Some("10.0.0.7"), Some(1))
            .unwrap();
        assert_eq!(
            s.next_scoped(client, local, &all_available).unwrap().name,
            "b"
        );
        // 未命中的客户端走常规规则
        assert_eq!(
            s.next_scoped(other, local, &all_available).unwrap().name,
            "a"
        );
        // listen 覆盖对该监听的所有客户端生效
        s.set_scope_override("listen", Some("127.0.0.1:5000"), Some(2))
            .unwrap();
        assert_eq!(
            s.next_scoped(other, local, &all_available).unwrap().name,
            "c"
        );
        // 清除 listen 覆盖后回落
        s.set_scope_override("listen", Some("127.0.0.1:5000"), None)
            .unwrap();
        assert_eq!(
            s.next_scoped(other, local, &all_available).unwrap().name,
            "a"
        );
    }

    #[test]
    fn 维度覆盖不可用时回落() {
        let s = selector(SelectRule::ChangeByTime, Duration::from_secs(60));
        let client: IpAddr = "10.0.0.7".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        s.set_scope_override("client", Some("10.0.0.7"), Some(1))
            .unwrap();
        // 强制上游 b 不可用 → 回落到可用上游
        let only_a = |i: usize| i == 0;
        assert_eq!(s.next_scoped(client, local, &only_a).unwrap().name, "a");
    }

    #[test]
    fn 非粘性规则忽略维度覆盖() {
        let s = selector(SelectRule::Loop, Duration::from_secs(60));
        let client: IpAddr = "10.0.0.7".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        s.set_scope_override("client", Some("10.0.0.7"), Some(2))
            .unwrap();
        // Loop 规则无视覆盖，按轮询从 a 开始
        assert_eq!(
            s.next_scoped(client, local, &all_available).unwrap().name,
            "a"
        );
    }

    #[test]
    fn 维度覆盖参数校验与全局覆盖() {
        let s = selector(SelectRule::OneByOne, Duration::from_secs(60));
        assert!(s.set_scope_override("client", None, Some(0)).is_err());
        assert!(
            s.set_scope_override("client", Some("not-an-ip"), Some(0))
                .is_err()
        );
        assert!(
            s.set_scope_override("listen", Some("127.0.0.1"), Some(0))
                .is_err()
        );
        assert!(
            s.set_scope_override("bogus", Some("10.0.0.1"), Some(0))
                .is_err()
        );
        assert!(s.set_scope_override("global", None, Some(99)).is_err());
        assert!(s.set_scope_override("global", None, Some(1)).is_ok());
        let client: IpAddr = "10.0.0.1".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        assert_eq!(
            s.next_scoped(client, local, &all_available).unwrap().name,
            "b"
        );
        assert!(s.set_scope_override("global", None, None).is_ok());
    }
}
