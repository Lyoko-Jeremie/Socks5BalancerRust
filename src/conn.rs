//! 连接注册表与监测（三维度计数 + 每连接明细 + 按维度中断）。
//!
//! 设计（docs/04 §14.4 端点、docs/11 已知限制的后续补齐）：
//! - **默认开启明细**（`connectionTracking.detail=true`），与 C++ 的会话表行为接近；
//!   可配置 `detail=false` 降级为只聚合计数（内存更省）；
//! - 每连接一个 [`ConnMeta`]：原子字段，热路径只读；持有一个**子 `CancellationToken`**
//!   （由全局取消令牌派生），中断该连接只需 `cancel()`，全局优雅退出仍能级联；
//! - 明细表按连接 id 分片（`Mutex<HashMap>`），注册/注销各一次锁；
//! - 三维度计数（上游/客户端/监听）单独用原子与 `HashMap` 维护，`summary` 直接读取；
//! - 明细上限 `detailLimit`：达到后新连接**只计数、不登记明细**，避免无界增长；
//! - `enabled=false` 时完全不记录（只返回原始 token，控制面 `/connections` 会说明未启用）。

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::config::ConnTrackingConfig;
use crate::handshake::Protocol;

/// 连接 ID（单调递增）。
pub type ConnId = u64;

/// 连接阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState {
    /// 已接受，等待首包。
    FirstPacket,
    /// 协议握手（SOCKS5/4、HTTP）进行中。
    Handshaking,
    /// 正在连接/握手上游。
    ConnectingUpstream,
    /// 双向转发中。
    Relaying,
    /// 关闭中。
    Closing,
}

impl ConnState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FirstPacket => "first_packet",
            Self::Handshaking => "handshaking",
            Self::ConnectingUpstream => "connecting_upstream",
            Self::Relaying => "relaying",
            Self::Closing => "closing",
        }
    }

    fn as_u8(&self) -> u8 {
        match self {
            Self::FirstPacket => 0,
            Self::Handshaking => 1,
            Self::ConnectingUpstream => 2,
            Self::Relaying => 3,
            Self::Closing => 4,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Handshaking,
            2 => Self::ConnectingUpstream,
            3 => Self::Relaying,
            4 => Self::Closing,
            _ => Self::FirstPacket,
        }
    }
}

/// 表示"尚未选择上游"的哨兵值。
const UPSTREAM_NONE: usize = usize::MAX;
/// 分片数（按连接 id 取模）。
const SHARDS: usize = 16;
/// 客户端维度（`per_client`）表容量上限；超过后整体清空（同 hint 表的简单有界策略）。
const MAX_CLIENT_ENTRIES: usize = 4096;

fn protocol_to_u8(p: Option<Protocol>) -> u8 {
    match p {
        None => 0,
        Some(Protocol::Socks5) => 1,
        Some(Protocol::Socks4) => 2,
        Some(Protocol::Http) => 3,
    }
}

fn protocol_from_u8(v: u8) -> Option<Protocol> {
    match v {
        1 => Some(Protocol::Socks5),
        2 => Some(Protocol::Socks4),
        3 => Some(Protocol::Http),
        _ => None,
    }
}

fn protocol_str(p: Option<Protocol>) -> &'static str {
    match p {
        None => "raw",
        Some(Protocol::Socks5) => "socks5",
        Some(Protocol::Socks4) => "socks4",
        Some(Protocol::Http) => "http",
    }
}

/// 单个连接的监测数据（原子字段，供并发读写）。
#[derive(Debug)]
pub struct ConnMeta {
    id: ConnId,
    peer: SocketAddr,
    local: SocketAddr,
    /// 协议编码（0 = 未知/纯透传）。
    protocol: AtomicUsize,
    /// 上游池下标（`UPSTREAM_NONE` = 未选）。
    upstream: AtomicUsize,
    state: AtomicUsize,
    started: Instant,
    bytes_up: AtomicU64,
    bytes_down: AtomicU64,
    /// 中断该连接的子取消令牌（由全局令牌派生）。
    token: CancellationToken,
}

impl ConnMeta {
    pub fn id(&self) -> ConnId {
        self.id
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    pub fn local(&self) -> SocketAddr {
        self.local
    }

    pub fn protocol(&self) -> Option<Protocol> {
        protocol_from_u8(self.protocol.load(Ordering::Relaxed) as u8)
    }

    pub fn set_protocol(&self, p: Protocol) {
        self.protocol
            .store(protocol_to_u8(Some(p)) as usize, Ordering::Relaxed);
    }

    pub fn state(&self) -> ConnState {
        ConnState::from_u8(self.state.load(Ordering::Relaxed) as u8)
    }

    pub fn set_state(&self, s: ConnState) {
        self.state.store(s.as_u8() as usize, Ordering::Relaxed);
    }

    pub fn upstream(&self) -> Option<usize> {
        let v = self.upstream.load(Ordering::Relaxed);
        (v != UPSTREAM_NONE).then_some(v)
    }

    pub fn token(&self) -> &CancellationToken {
        &self.token
    }

    /// 中断该连接（仅取消本连接，不影响其它连接）。
    pub fn cancel(&self) {
        self.token.cancel();
    }

    pub fn add_bytes(&self, up: bool, n: usize) {
        let counter = if up { &self.bytes_up } else { &self.bytes_down };
        counter.fetch_add(n as u64, Ordering::Relaxed);
    }

    fn uptime_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    fn snapshot(&self) -> ConnSnapshot {
        ConnSnapshot {
            id: self.id,
            peer: self.peer,
            local: self.local,
            protocol: protocol_str(self.protocol()),
            upstream: self.upstream(),
            state: self.state().as_str(),
            uptime_ms: self.uptime_ms(),
            bytes_up: self.bytes_up.load(Ordering::Relaxed),
            bytes_down: self.bytes_down.load(Ordering::Relaxed),
        }
    }
}

/// 连接快照（控制面 JSON 用；不含任何凭据）。
#[derive(Debug, Clone)]
pub struct ConnSnapshot {
    pub id: ConnId,
    pub peer: SocketAddr,
    pub local: SocketAddr,
    pub protocol: &'static str,
    pub upstream: Option<usize>,
    pub state: &'static str,
    pub uptime_ms: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
}

/// 连接过滤条件（`/connections` 查询与 `close_conn` 命令共用）。
#[derive(Debug, Clone, Default)]
pub struct ConnFilter {
    pub id: Option<ConnId>,
    pub client: Option<IpAddr>,
    pub listen: Option<SocketAddr>,
    pub upstream: Option<usize>,
}

impl ConnFilter {
    fn matches(&self, m: &ConnMeta) -> bool {
        if let Some(id) = self.id
            && m.id != id
        {
            return false;
        }
        if let Some(ip) = self.client
            && m.peer.ip() != ip
        {
            return false;
        }
        if let Some(l) = self.listen
            && m.local != l
        {
            return false;
        }
        if let Some(u) = self.upstream
            && m.upstream() != Some(u)
        {
            return false;
        }
        true
    }
}

/// 三维度摘要（`/api/v1/connections/summary` 用）。
#[derive(Debug, Clone, Default)]
pub struct ConnSummary {
    pub total: u64,
    /// (上游下标, 连接数)；只含存在连接的上游。
    pub by_upstream: Vec<(usize, u64)>,
    /// (监听地址, 连接数)。
    pub by_listen: Vec<(SocketAddr, u64)>,
    /// (客户端 IP, 连接数)，按数量降序、截断到 `clientTopLimit`。
    pub by_client: Vec<(IpAddr, u64)>,
}

/// 连接注册表（进程级单例）。
#[derive(Debug)]
pub struct ConnRegistry {
    cfg: ConnTrackingConfig,
    shards: Vec<Mutex<HashMap<ConnId, Arc<ConnMeta>>>>,
    next_id: AtomicU64,
    active: AtomicU64,
    detail_count: AtomicU64,
    per_upstream: Vec<AtomicU64>,
    per_listen: Mutex<HashMap<SocketAddr, u64>>,
    per_client: Mutex<HashMap<IpAddr, u64>>,
    overflow_warned: AtomicBool,
}

impl ConnRegistry {
    pub fn new(cfg: ConnTrackingConfig, upstream_count: usize) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            shards: (0..SHARDS).map(|_| Mutex::new(HashMap::new())).collect(),
            next_id: AtomicU64::new(1),
            active: AtomicU64::new(0),
            detail_count: AtomicU64::new(0),
            per_upstream: (0..upstream_count).map(|_| AtomicU64::new(0)).collect(),
            per_listen: Mutex::new(HashMap::new()),
            per_client: Mutex::new(HashMap::new()),
            overflow_warned: AtomicBool::new(false),
        })
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// 是否登记每连接明细（`enabled && detail`）。
    pub fn detail_enabled(&self) -> bool {
        self.cfg.enabled && self.cfg.detail
    }

    pub fn cfg(&self) -> &ConnTrackingConfig {
        &self.cfg
    }

    /// 当前活跃连接数（无论是否登记明细）。
    pub fn active(&self) -> u64 {
        self.active.load(Ordering::Relaxed)
    }

    /// 注册一条连接。
    ///
    /// 返回 `(guard, token)`：`guard` 在 drop 时自动注销并回退计数；`token` 由全局取消令牌派生，
    /// 关闭该连接只需 `token.cancel()`（全局退出会级联取消）。
    pub fn register(
        self: &Arc<Self>,
        peer: SocketAddr,
        local: SocketAddr,
        parent: &CancellationToken,
    ) -> (ConnGuard, CancellationToken) {
        let token = parent.child_token();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let meta = Arc::new(ConnMeta {
            id,
            peer,
            local,
            protocol: AtomicUsize::new(0),
            upstream: AtomicUsize::new(UPSTREAM_NONE),
            state: AtomicUsize::new(ConnState::FirstPacket.as_u8() as usize),
            started: Instant::now(),
            bytes_up: AtomicU64::new(0),
            bytes_down: AtomicU64::new(0),
            token: token.clone(),
        });

        let mut counted = false;
        let mut detailed = false;
        if self.cfg.enabled {
            counted = true;
            self.active.fetch_add(1, Ordering::Relaxed);
            {
                let mut l = self.per_listen.lock().unwrap_or_else(|e| e.into_inner());
                *l.entry(local).or_default() += 1;
            }
            {
                let mut c = self.per_client.lock().unwrap_or_else(|e| e.into_inner());
                let ip = peer.ip();
                if c.len() >= MAX_CLIENT_ENTRIES && !c.contains_key(&ip) {
                    c.clear();
                }
                *c.entry(ip).or_default() += 1;
            }
            if self.cfg.detail {
                let current = self.detail_count.load(Ordering::Relaxed);
                if current < self.cfg.detail_limit as u64 {
                    let shard = (id as usize) % SHARDS;
                    let mut map = self.shards[shard].lock().unwrap_or_else(|e| e.into_inner());
                    map.insert(id, Arc::clone(&meta));
                    self.detail_count.fetch_add(1, Ordering::Relaxed);
                    detailed = true;
                } else if !self.overflow_warned.swap(true, Ordering::Relaxed) {
                    tracing::warn!(
                        limit = self.cfg.detail_limit,
                        "连接明细达到上限：后续连接只计数、不登记明细"
                    );
                }
            }
        }

        let guard = ConnGuard {
            reg: Arc::clone(self),
            meta,
            counted,
            detailed,
        };
        (guard, token)
    }

    /// 设置某连接的上游下标（同步维护上游维度计数）。
    fn set_upstream(&self, meta: &Arc<ConnMeta>, idx: usize) {
        let old = meta.upstream.swap(idx, Ordering::Relaxed);
        if old != UPSTREAM_NONE
            && let Some(c) = self.per_upstream.get(old)
        {
            c.fetch_sub(1, Ordering::Relaxed);
        }
        if let Some(c) = self.per_upstream.get(idx) {
            c.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 全部明细快照（按 id 升序；`detail=false` 时为空）。
    pub fn snapshot(&self) -> Vec<ConnSnapshot> {
        let mut out = Vec::new();
        for shard in &self.shards {
            let map = shard.lock().unwrap_or_else(|e| e.into_inner());
            out.extend(map.values().map(|m| m.snapshot()));
        }
        out.sort_by_key(|s| s.id);
        out
    }

    /// 过滤 + 游标分页：`after` 为上一页最后一条 id（不包含），返回 `(本页, 下一页游标)`。
    pub fn page(
        &self,
        filter: &ConnFilter,
        after: Option<ConnId>,
        limit: usize,
    ) -> (Vec<ConnSnapshot>, Option<ConnId>) {
        let limit = limit.clamp(1, 1000);
        let mut items = Vec::new();
        for shard in &self.shards {
            let map = shard.lock().unwrap_or_else(|e| e.into_inner());
            for m in map.values() {
                if let Some(a) = after
                    && m.id <= a
                {
                    continue;
                }
                if !filter.matches(m) {
                    continue;
                }
                items.push(m.snapshot());
            }
        }
        items.sort_by_key(|s| s.id);
        items.truncate(limit);
        let next = items.last().map(|s| s.id);
        (items, next)
    }

    /// 三维度摘要。
    pub fn summary(&self) -> ConnSummary {
        let mut by_upstream: Vec<(usize, u64)> = self
            .per_upstream
            .iter()
            .enumerate()
            .map(|(i, c)| (i, c.load(Ordering::Relaxed)))
            .filter(|(_, c)| *c > 0)
            .collect();
        by_upstream.sort_by_key(|(i, _)| *i);

        let mut by_listen: Vec<(SocketAddr, u64)> = self
            .per_listen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (*k, *v))
            .filter(|(_, v)| *v > 0)
            .collect();
        by_listen.sort_by_key(|(k, _)| *k);

        let mut by_client: Vec<(IpAddr, u64)> = self
            .per_client
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(k, v)| (*k, *v))
            .filter(|(_, v)| *v > 0)
            .collect();
        by_client.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        by_client.truncate(self.cfg.client_top_limit);

        ConnSummary {
            total: self.active(),
            by_upstream,
            by_listen,
            by_client,
        }
    }

    /// 取消匹配的全部连接，返回匹配数量（`detail=false` 时无法定位连接，返回 0）。
    pub fn close(&self, filter: &ConnFilter) -> usize {
        if !self.detail_enabled() {
            return 0;
        }
        let mut closed = 0;
        for shard in &self.shards {
            let map = shard.lock().unwrap_or_else(|e| e.into_inner());
            for m in map.values() {
                if filter.matches(m) {
                    m.cancel();
                    closed += 1;
                }
            }
        }
        closed
    }
}

/// 连接守卫：drop 时自动注销并回退计数。
///
/// 方法委托给 [`ConnMeta`]，并负责同步上游维度计数。
#[derive(Debug)]
pub struct ConnGuard {
    reg: Arc<ConnRegistry>,
    meta: Arc<ConnMeta>,
    /// 是否已计入 `active`/维度计数（`enabled=false` 时不计）。
    counted: bool,
    /// 是否已登记明细（`detail=false` 或达到上限时为 false）。
    detailed: bool,
}

impl ConnGuard {
    pub fn meta(&self) -> &Arc<ConnMeta> {
        &self.meta
    }

    pub fn token(&self) -> &CancellationToken {
        &self.meta.token
    }

    pub fn set_protocol(&self, p: Protocol) {
        self.meta.set_protocol(p);
    }

    pub fn set_state(&self, s: ConnState) {
        self.meta.set_state(s);
    }

    pub fn set_upstream(&self, idx: usize) {
        if self.counted {
            self.reg.set_upstream(&self.meta, idx);
        }
    }

    pub fn add_bytes(&self, up: bool, n: usize) {
        self.meta.add_bytes(up, n);
    }

    pub fn cancel(&self) {
        self.meta.cancel();
    }
}

impl Drop for ConnGuard {
    fn drop(&mut self) {
        if !self.counted {
            return;
        }
        self.reg.active.fetch_sub(1, Ordering::Relaxed);

        let local = self.meta.local;
        {
            let mut l = self
                .reg
                .per_listen
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(c) = l.get_mut(&local) {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    l.remove(&local);
                }
            }
        }
        let ip = self.meta.peer.ip();
        {
            let mut c = self
                .reg
                .per_client
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(v) = c.get_mut(&ip) {
                *v = v.saturating_sub(1);
                if *v == 0 {
                    c.remove(&ip);
                }
            }
        }
        if let Some(u) = self.meta.upstream()
            && let Some(c) = self.reg.per_upstream.get(u)
        {
            c.fetch_sub(1, Ordering::Relaxed);
        }
        if self.detailed {
            let shard = (self.meta.id as usize) % SHARDS;
            let mut map = self.reg.shards[shard]
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if map.remove(&self.meta.id).is_some() {
                self.reg.detail_count.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(detail: bool, limit: usize) -> Arc<ConnRegistry> {
        ConnRegistry::new(
            ConnTrackingConfig {
                enabled: true,
                detail,
                detail_limit: limit,
                client_top_limit: 10,
            },
            3,
        )
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// 构造不同客户端 IP 的地址（用于 client 维度过滤测试）。
    fn sock(last: u8, port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, last], port))
    }

    #[test]
    fn 注册与注销维护计数与明细() {
        let reg = registry(true, 100);
        let cancel = CancellationToken::new();
        let (guard, _token) = reg.register(addr(40001), addr(5000), &cancel);
        assert_eq!(reg.active(), 1);
        assert_eq!(reg.snapshot().len(), 1);
        guard.set_upstream(1);
        guard.set_protocol(Protocol::Socks5);
        guard.set_state(ConnState::Relaying);
        guard.add_bytes(true, 10);
        guard.add_bytes(false, 20);

        let snap = reg.snapshot().pop().unwrap();
        assert_eq!(snap.protocol, "socks5");
        assert_eq!(snap.upstream, Some(1));
        assert_eq!(snap.state, "relaying");
        assert_eq!(snap.bytes_up, 10);
        assert_eq!(snap.bytes_down, 20);

        let sum = reg.summary();
        assert_eq!(sum.total, 1);
        assert_eq!(sum.by_upstream, vec![(1, 1)]);
        assert_eq!(sum.by_listen, vec![(addr(5000), 1)]);
        assert_eq!(sum.by_client, vec![(addr(40001).ip(), 1)]);

        drop(guard);
        assert_eq!(reg.active(), 0);
        assert!(reg.snapshot().is_empty());
        assert!(reg.summary().by_upstream.is_empty());
    }

    #[test]
    fn 明细关闭时只保留聚合计数() {
        let reg = registry(false, 100);
        let cancel = CancellationToken::new();
        let (guard, _token) = reg.register(addr(40001), addr(5000), &cancel);
        assert_eq!(reg.active(), 1);
        assert_eq!(reg.summary().total, 1);
        assert!(reg.snapshot().is_empty());
        assert_eq!(reg.close(&ConnFilter::default()), 0, "无明细时无法关闭");
        drop(guard);
        assert_eq!(reg.active(), 0);
    }

    #[test]
    fn 明細上限生效() {
        let reg = registry(true, 2);
        let cancel = CancellationToken::new();
        let guards: Vec<_> = (1u16..=3)
            .map(|i| reg.register(addr(40000 + i), addr(5000), &cancel).0)
            .collect();
        assert_eq!(reg.active(), 3);
        assert_eq!(reg.snapshot().len(), 2, "超过 detailLimit 的连接只计数");
        drop(guards);
        assert_eq!(reg.active(), 0);
        assert!(reg.snapshot().is_empty());
    }

    #[test]
    fn 过滤分页与按维度关闭() {
        let reg = registry(true, 100);
        let cancel = CancellationToken::new();
        let (g1, t1) = reg.register(sock(1, 41001), addr(5000), &cancel);
        let (g2, t2) = reg.register(sock(2, 41002), addr(5001), &cancel);
        let (g3, t3) = reg.register(sock(1, 41001), addr(5001), &cancel);
        g1.set_upstream(0);
        g2.set_upstream(1);
        g3.set_upstream(1);
        assert!(!t1.is_cancelled() && !t2.is_cancelled() && !t3.is_cancelled());

        // 分页：limit=2，第二页不应重复
        let (page1, next) = reg.page(&ConnFilter::default(), None, 2);
        assert_eq!(page1.len(), 2);
        let (page2, _) = reg.page(&ConnFilter::default(), next, 2);
        assert_eq!(page2.len(), 1);
        assert!(page1.iter().all(|s| page2.iter().all(|t| t.id != s.id)));

        // 过滤
        let (by_client, _) = reg.page(
            &ConnFilter {
                client: Some(sock(1, 41001).ip()),
                ..Default::default()
            },
            None,
            100,
        );
        assert_eq!(by_client.len(), 2);
        let (by_listen, _) = reg.page(
            &ConnFilter {
                listen: Some(addr(5001)),
                ..Default::default()
            },
            None,
            100,
        );
        assert_eq!(by_listen.len(), 2);
        let (by_upstream, _) = reg.page(
            &ConnFilter {
                upstream: Some(1),
                ..Default::default()
            },
            None,
            100,
        );
        assert_eq!(by_upstream.len(), 2);

        // 按监听关闭：应取消 5001 上的两条连接
        assert_eq!(
            reg.close(&ConnFilter {
                listen: Some(addr(5001)),
                ..Default::default()
            }),
            2
        );
        assert!(!t1.is_cancelled(), "未命中的连接不应被取消");
        assert!(t2.is_cancelled() && t3.is_cancelled(), "命中的连接应被取消");
        assert_eq!(reg.close(&ConnFilter::default()), 3);
        drop((g1, g2, g3));
        assert_eq!(reg.active(), 0);
    }

    #[test]
    fn 禁用连接监测时不记录() {
        let reg = ConnRegistry::new(
            ConnTrackingConfig {
                enabled: false,
                ..Default::default()
            },
            1,
        );
        let cancel = CancellationToken::new();
        let (guard, _token) = reg.register(addr(40001), addr(5000), &cancel);
        assert!(!reg.enabled());
        assert_eq!(reg.active(), 0);
        assert!(reg.snapshot().is_empty());
        assert_eq!(reg.close(&ConnFilter::default()), 0);
        drop(guard);
        assert_eq!(reg.active(), 0);
    }
}
