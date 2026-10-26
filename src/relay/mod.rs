//! 连接处理：混合端口协议分派（SOCKS5 / SOCKS4 / HTTP）与纯 TCP 透传。
//!
//! M2 起同一监听端口按首包分派：
//! - SOCKS5（0x05）→ 方法协商/CONNECT（M1）；
//! - SOCKS4/4a（0x04）→ CONNECT（M2）；
//! - 其它 → HTTP CONNECT / absolute-form 正向代理（M2，`httparse` 解析）。
//!
//! 统一模型：各协议先解析出 `Tunnel { target, prefix, reply }`，再走同一条
//! "选上游 → 连上游 → 上游 SOCKS5 握手 → 回成功应答 → 写前缀 → 双向转发" 流程：
//! - `prefix` 用于 HTTP 的 early data（CONNECT 粘包）与改写后的请求头；
//! - `reply` 决定成功/失败应答格式（SOCKS5 REP / SOCKS4 5A-5B / HTTP 200-502）。
//!
//! 关键设计（docs/02 §3/§8、docs/07 ADR-005）：
//! - 双向缓冲在 accept 时一次性预分配，运行期零分配；
//! - 每方向"单在途 read + 单在途 write"，天然背压；
//! - 上游 TCP 建连与 SOCKS5 握手都带超时；
//! - 成功应答在"上游握手成功之后"才写，失败回具体错误码；
//! - 全局取消令牌可中断阻塞中的读写，保证优雅关闭。

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_socks::tcp::Socks5Stream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use crate::app::AppCtx;
use crate::handshake::{self, Protocol, TargetAddr, http, socks4, socks5};
use crate::stats::{ActiveGuard, Stats};
use crate::upstream::Upstream;

/// 双向转发缓冲（ADR-005：会话建立时预分配）。
#[derive(Debug)]
pub struct Buffers {
    pub down_to_up: Box<[u8]>,
    pub up_to_down: Box<[u8]>,
}

impl Buffers {
    /// 分配两块 `size` 字节的缓冲。只在 accept 时调用一次。
    pub fn alloc(size: usize) -> Self {
        Self {
            down_to_up: vec![0u8; size].into_boxed_slice(),
            up_to_down: vec![0u8; size].into_boxed_slice(),
        }
    }
}

/// 隧道建立成功后的应答方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reply {
    Socks5,
    Socks4,
    HttpConnect,
    /// absolute-form：不写应答，直接转发改写后的请求
    None,
}

/// 协议解析完成、等待建立隧道。
struct Tunnel {
    protocol: Protocol,
    target: TargetAddr,
    /// 必须先写给目标的前缀字节（HTTP early data / 改写后的请求头）
    prefix: Vec<u8>,
    reply: Reply,
}

/// 处理一条下游连接。
pub async fn handle_connection(
    ctx: Arc<AppCtx>,
    downstream: TcpStream,
    peer: SocketAddr,
    local: SocketAddr,
    bufs: Buffers,
    guard: ActiveGuard,
) {
    // sleepTime：记录"有新连接"的活动时间，健康检查会据此暂停/恢复常规探测
    ctx.health.note_activity();
    if ctx.cfg.tradition_tcp_relay {
        handle_tradition(ctx, downstream, peer, local, bufs, guard).await;
    } else {
        handle_smart(ctx, downstream, peer, local, bufs, guard).await;
    }
}

/// 纯 TCP 透传（`traditionTcpRelay=true`）。
async fn handle_tradition(
    ctx: Arc<AppCtx>,
    downstream: TcpStream,
    peer: SocketAddr,
    local: SocketAddr,
    bufs: Buffers,
    _guard: ActiveGuard,
) {
    // ADR-002 #3：建连失败立即切下一个可用上游（最多 retryTimes+1 次）。
    let Some((upstream_cfg, upstream)) = connect_upstream_with_retry(&ctx, peer, local).await
    else {
        return;
    };
    trace!(%peer, %local, upstream = %upstream_cfg.name, addr = %upstream_cfg.addr(), "开始透传");
    relay_streams(ctx, downstream, upstream, bufs, peer, "tradition").await;
}

/// 混合端口协议分派。
async fn handle_smart(
    ctx: Arc<AppCtx>,
    mut downstream: TcpStream,
    peer: SocketAddr,
    local: SocketAddr,
    bufs: Buffers,
    _guard: ActiveGuard,
) {
    // 1) 首包探测（防"哑连接"）
    let mut first = [0u8; 1];
    match tokio::time::timeout(ctx.cfg.first_packet_timeout, downstream.peek(&mut first)).await {
        Ok(Ok(0)) => {
            trace!(%peer, "对端在首包前关闭");
            return;
        }
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            debug!(%peer, error = %e, "首包读取失败");
            return;
        }
        Err(_) => {
            ctx.stats
                .first_packet_timeout
                .fetch_add(1, Ordering::Relaxed);
            debug!(%peer, timeout_ms = ctx.cfg.first_packet_timeout.as_millis() as u64, "首包超时");
            drain_and_close(downstream).await;
            return;
        }
    }
    let protocol = handshake::sniff_first_byte(first[0]);
    trace!(%peer, protocol = protocol.as_str(), "协议分派");

    // 2) 各协议解析出目标 + 前缀 + 应答方式
    let tunnel = match protocol {
        Protocol::Socks5 => socks5_tunnel(&ctx, &mut downstream, peer).await,
        Protocol::Socks4 => socks4_tunnel(&ctx, &mut downstream, peer).await,
        Protocol::Http => http_tunnel(&ctx, &mut downstream, peer).await,
    };
    let Some(tunnel) = tunnel else {
        // 各 flow 已尽力写错误应答，这里统一优雅关闭
        drain_and_close(downstream).await;
        return;
    };
    trace!(%peer, protocol = tunnel.protocol.as_str(), target = %tunnel.target, "已解析目标");

    // 3) 统一建立隧道并转发
    establish(ctx, downstream, tunnel, peer, local, bufs).await;
}

/// SOCKS5：方法协商 + 可选认证 + CONNECT 解析（错误码由 negotiate 尽力写回）。
async fn socks5_tunnel(
    ctx: &Arc<AppCtx>,
    stream: &mut TcpStream,
    peer: SocketAddr,
) -> Option<Tunnel> {
    let target = match tokio::time::timeout(
        ctx.cfg.handshake_timeout,
        socks5::negotiate(stream, &ctx.cfg.auth),
    )
    .await
    {
        Ok(Ok(target)) => target,
        Ok(Err(e)) => {
            ctx.stats.handshake_failed.fetch_add(1, Ordering::Relaxed);
            debug!(%peer, error = %e, "入站 SOCKS5 协商失败");
            return None;
        }
        Err(_) => {
            ctx.stats.handshake_failed.fetch_add(1, Ordering::Relaxed);
            let _ = socks5::write_error_reply(stream, socks5::REP_TTL_EXPIRED).await;
            debug!(%peer, "入站 SOCKS5 握手超时");
            return None;
        }
    };
    Some(Tunnel {
        protocol: Protocol::Socks5,
        target,
        prefix: Vec::new(),
        reply: Reply::Socks5,
    })
}

/// SOCKS4/4a：解析 CONNECT 请求（禁用时回 5B）。
async fn socks4_tunnel(
    ctx: &Arc<AppCtx>,
    stream: &mut TcpStream,
    peer: SocketAddr,
) -> Option<Tunnel> {
    if ctx.cfg.disable_socks4 {
        ctx.stats.protocol_errors.fetch_add(1, Ordering::Relaxed);
        let _ = socks4::write_reply(stream, false).await;
        debug!(%peer, "SOCKS4 已禁用（disableSocks4=true）");
        return None;
    }
    let target = match tokio::time::timeout(
        ctx.cfg.handshake_timeout,
        socks4::negotiate(stream, &ctx.cfg.auth),
    )
    .await
    {
        Ok(Ok(target)) => target,
        Ok(Err(e)) => {
            ctx.stats.handshake_failed.fetch_add(1, Ordering::Relaxed);
            debug!(%peer, error = %e, "入站 SOCKS4 协商失败");
            return None;
        }
        Err(_) => {
            ctx.stats.handshake_failed.fetch_add(1, Ordering::Relaxed);
            let _ = socks4::write_reply(stream, false).await;
            debug!(%peer, "入站 SOCKS4 握手超时");
            return None;
        }
    };
    Some(Tunnel {
        protocol: Protocol::Socks4,
        target,
        prefix: Vec::new(),
        reply: Reply::Socks4,
    })
}

/// HTTP：读取请求头 → CONNECT 或 absolute-form。
async fn http_tunnel(
    ctx: &Arc<AppCtx>,
    stream: &mut TcpStream,
    peer: SocketAddr,
) -> Option<Tunnel> {
    let (buf, head_len) = match tokio::time::timeout(
        ctx.cfg.handshake_timeout,
        http::read_head(stream, http::MAX_HEAD_BYTES),
    )
    .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            ctx.stats.protocol_errors.fetch_add(1, Ordering::Relaxed);
            let _ = http::write_error(stream, 400, "Bad Request").await;
            debug!(%peer, error = %e, "HTTP 请求头读取失败");
            return None;
        }
        Err(_) => {
            ctx.stats.protocol_errors.fetch_add(1, Ordering::Relaxed);
            let _ = http::write_error(stream, 408, "Request Timeout").await;
            debug!(%peer, "HTTP 请求头读取超时");
            return None;
        }
    };

    let kind = match http::parse(&buf[..head_len]) {
        Ok(kind) => kind,
        Err(e) => {
            ctx.stats.protocol_errors.fetch_add(1, Ordering::Relaxed);
            let _ = http::write_error(stream, 400, "Bad Request").await;
            debug!(%peer, error = %e, "HTTP 请求解析失败");
            return None;
        }
    };

    // 1) 功能禁用优先：absolute-form 正向代理直接回 501 告知客户端不支持（CONNECT 不受影响）。
    if ctx.cfg.disable_http_forward && matches!(&kind, http::RequestKind::Forward { .. }) {
        ctx.stats.protocol_errors.fetch_add(1, Ordering::Relaxed);
        let _ = http::write_not_implemented(
            stream,
            "HTTP forward proxy (absolute-form) is disabled; use CONNECT or SOCKS5.",
        )
        .await;
        debug!(%peer, "HTTP 正向代理已禁用（disableHttpForward=true），已回 501");
        return None;
    }

    // 2) 入站 HTTP 认证（对齐 C++ 增强模式）：配置了 authClientInfo 时校验 Basic 凭据。
    // 失败一次性回 407 并关闭，避免 C++ 的 407 后紧循环问题。
    if !ctx.cfg.auth.is_empty() && !http::check_proxy_auth(&buf[..head_len], &ctx.cfg.auth) {
        ctx.stats.handshake_failed.fetch_add(1, Ordering::Relaxed);
        let _ = http::write_proxy_auth_required(stream).await;
        debug!(%peer, "HTTP 代理认证失败，已回 407 并关闭");
        return None;
    }

    let early_data = buf[head_len..].to_vec();

    match kind {
        http::RequestKind::Connect(target) => Some(Tunnel {
            protocol: Protocol::Http,
            target,
            // CONNECT 的请求头丢弃，early data 必须回放给目标（修正 C++ 粘包失败）
            prefix: early_data,
            reply: Reply::HttpConnect,
        }),
        http::RequestKind::Forward {
            target,
            mut rewritten_head,
            ..
        } => {
            // absolute-form：把改写后的请求头 + early data 一起发给目标
            rewritten_head.extend_from_slice(&early_data);
            Some(Tunnel {
                protocol: Protocol::Http,
                target,
                prefix: rewritten_head,
                reply: Reply::None,
            })
        }
    }
}

/// 建立到目标的隧道（经上游 SOCKS5），成功后写应答与前缀，再双向转发。
async fn establish(
    ctx: Arc<AppCtx>,
    mut downstream: TcpStream,
    tunnel: Tunnel,
    peer: SocketAddr,
    local: SocketAddr,
    bufs: Buffers,
) {
    // 1) 选择上游并建立到上游的连接（ADR-002 #3：失败立即切下一个可用后端）
    //    最多尝试 `retryTimes + 1` 个不同上游；目标侧错误（如 ConnectionRefused）也会换一个
    //    出口重试，因为不同上游的出口网络可能不同。
    let max_attempts = ctx.cfg.retry_times.saturating_add(1).max(1);
    let mut tried: Vec<usize> = Vec::with_capacity(max_attempts);
    let mut last_rep = socks5::REP_GENERAL_FAILURE;
    let mut established = None;
    let mut up_name = String::new();

    for _ in 0..max_attempts {
        let Some(upstream_cfg) = ctx
            .pick_upstream_for_client(peer.ip(), local, &tried)
            .cloned()
        else {
            break;
        };
        tried.push(upstream_cfg.health_index);

        // 连接上游 TCP（失败时已 mark_degraded）
        let Some(upstream_raw) = connect_upstream(&ctx, &upstream_cfg, peer).await else {
            last_rep = socks5::REP_HOST_UNREACHABLE;
            continue;
        };

        // 以上游 SOCKS5 客户端身份握手（含上游认证；域名交给上游解析）
        let up_target = to_socks_target(&tunnel.target);
        let creds = upstream_cfg
            .auth_user
            .as_deref()
            .filter(|u| !u.trim().is_empty())
            .map(|u| {
                (
                    u.to_string(),
                    upstream_cfg.auth_pwd.clone().unwrap_or_default(),
                )
            });
        let handshake_fut = async move {
            match creds {
                Some((user, pwd)) => {
                    Socks5Stream::connect_with_password_and_socket(
                        upstream_raw,
                        up_target,
                        &user,
                        &pwd,
                    )
                    .await
                }
                None => Socks5Stream::connect_with_socket(upstream_raw, up_target).await,
            }
        };
        match tokio::time::timeout(ctx.cfg.handshake_timeout, handshake_fut).await {
            Ok(Ok(stream)) => {
                up_name = upstream_cfg.name.clone();
                // 记录每客户端粘性 hint（下次同客户端优先复用该上游）
                ctx.record_hint(peer.ip(), upstream_cfg.health_index);
                established = Some(stream);
                break;
            }
            Ok(Err(e)) => {
                ctx.stats
                    .upstream_handshake_failed
                    .fetch_add(1, Ordering::Relaxed);
                if is_upstream_fault(&e) {
                    ctx.health
                        .mark_degraded(upstream_cfg.health_index, "上游握手失败");
                }
                last_rep = rep_from_upstream_error(&e);
                debug!(
                    %peer,
                    upstream = %upstream_cfg.name,
                    error = %e,
                    rep = last_rep,
                    "上游 SOCKS5 握手失败，尝试下一个上游"
                );
            }
            Err(_) => {
                ctx.stats
                    .upstream_handshake_failed
                    .fetch_add(1, Ordering::Relaxed);
                ctx.health
                    .mark_degraded(upstream_cfg.health_index, "上游握手超时");
                last_rep = socks5::REP_TTL_EXPIRED;
                debug!(%peer, upstream = %upstream_cfg.name, "上游 SOCKS5 握手超时，尝试下一个上游");
            }
        }
    }

    let Some(mut upstream) = established else {
        if tried.is_empty() {
            // 一个可用上游都没有：快速拒绝
            ctx.stats.rejected.fetch_add(1, Ordering::Relaxed);
            let _ = write_failure(&mut downstream, tunnel.reply, socks5::REP_GENERAL_FAILURE).await;
            debug!(
                %peer,
                available = ctx.health.available_count(),
                total = ctx.health.len(),
                "所有上游不可用，拒绝请求"
            );
            drain_and_close(downstream).await;
        } else {
            // 尝试过的上游全部失败：回最后一次的具体错误码
            let _ = write_failure(&mut downstream, tunnel.reply, last_rep).await;
            debug!(%peer, tried = tried.len(), rep = last_rep, "所有尝试的上游均失败");
        }
        return;
    };

    // 4) 上游成功后，才回复客户端成功
    let reply_result = match tunnel.reply {
        Reply::Socks5 => socks5::write_success_reply(&mut downstream).await,
        Reply::Socks4 => socks4::write_reply(&mut downstream, true).await,
        Reply::HttpConnect => http::write_connect_ok(&mut downstream).await,
        Reply::None => Ok(()),
    };
    if let Err(e) = reply_result {
        debug!(%peer, error = %e, "写成功应答失败");
        return;
    }

    // 5) 回放前缀（HTTP early data / 改写后的请求头）
    if !tunnel.prefix.is_empty()
        && let Err(e) = upstream.write_all(&tunnel.prefix).await
    {
        debug!(%peer, error = %e, "写前缀数据失败");
        return;
    }

    trace!(
        %peer,
        %local,
        protocol = tunnel.protocol.as_str(),
        upstream = %up_name,
        target = %tunnel.target,
        "隧道建立，开始转发"
    );

    // 6) 双向转发
    relay_streams(ctx, downstream, upstream, bufs, peer, "smart").await;
}

/// 按协议写失败应答（SOCKS5 REP / SOCKS4 5B / HTTP 502）。
async fn write_failure<S: AsyncWrite + Unpin>(
    stream: &mut S,
    reply: Reply,
    rep: u8,
) -> io::Result<()> {
    match reply {
        Reply::Socks5 => socks5::write_error_reply(stream, rep).await,
        Reply::Socks4 => socks4::write_reply(stream, false).await,
        Reply::HttpConnect | Reply::None => http::write_error(stream, 502, "Bad Gateway").await,
    }
}

/// 握手失败后的优雅关闭：先半关闭写端，再短暂丢弃剩余输入，避免 RST 丢错误码。
async fn drain_and_close(mut stream: TcpStream) {
    let _ = stream.shutdown().await;
    let mut buf = [0u8; 512];
    let _ = tokio::time::timeout(std::time::Duration::from_millis(200), async {
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await;
}

/// 纯 TCP 模式：按 `retryTimes` 依次尝试不同上游建立 TCP 连接。
///
/// 每次失败都会 `mark_degraded`（由 [`connect_upstream`] 完成），并换下一个可用上游，
/// 最多尝试 `retryTimes + 1` 个，全部失败返回 `None`。
async fn connect_upstream_with_retry(
    ctx: &Arc<AppCtx>,
    peer: SocketAddr,
    local: SocketAddr,
) -> Option<(Upstream, TcpStream)> {
    let max_attempts = ctx.cfg.retry_times.saturating_add(1).max(1);
    let mut tried: Vec<usize> = Vec::with_capacity(max_attempts);
    for _ in 0..max_attempts {
        let Some(cfg) = ctx
            .pick_upstream_for_client(peer.ip(), local, &tried)
            .cloned()
        else {
            break;
        };
        tried.push(cfg.health_index);
        if let Some(stream) = connect_upstream(ctx, &cfg, peer).await {
            ctx.record_hint(peer.ip(), cfg.health_index);
            return Some((cfg, stream));
        }
    }
    if tried.is_empty() {
        // 一个可用上游都没有：按拒绝统计（与协议路径口径一致）
        ctx.stats.rejected.fetch_add(1, Ordering::Relaxed);
        debug!(%peer, "无可用上游，关闭连接");
    } else {
        debug!(%peer, tried = tried.len(), "所有尝试的上游均连接失败");
    }
    None
}

/// 连接上游 TCP（带超时），成功后关闭 Nagle；失败时标记上游降级。
async fn connect_upstream(
    ctx: &Arc<AppCtx>,
    upstream_cfg: &Upstream,
    peer: SocketAddr,
) -> Option<TcpStream> {
    // 每次建连都读当前地址：DNS TTL 刷新可能已更新（不在热路径上，每连接一次）
    let addr = upstream_cfg.addr();
    match tokio::time::timeout(ctx.cfg.connect_timeout, TcpStream::connect(addr)).await {
        Ok(Ok(stream)) => {
            if let Err(e) = stream.set_nodelay(true) {
                debug!(%peer, upstream = %upstream_cfg.name, error = %e, "上游 set_nodelay 失败");
            }
            Some(stream)
        }
        Ok(Err(e)) => {
            ctx.stats.connect_failed.fetch_add(1, Ordering::Relaxed);
            ctx.health
                .mark_degraded(upstream_cfg.health_index, "连接上游失败");
            debug!(%peer, upstream = %upstream_cfg.name, %addr, error = %e, "连接上游失败");
            None
        }
        Err(_) => {
            ctx.stats.connect_failed.fetch_add(1, Ordering::Relaxed);
            ctx.health
                .mark_degraded(upstream_cfg.health_index, "连接上游超时");
            debug!(
                %peer,
                upstream = %upstream_cfg.name,
                %addr,
                timeout_ms = ctx.cfg.connect_timeout.as_millis() as u64,
                "连接上游超时"
            );
            None
        }
    }
}

/// 把入站目标地址转成 tokio-socks 的目标地址类型（域名保持域名，交给上游解析）。
fn to_socks_target(target: &TargetAddr) -> tokio_socks::TargetAddr<'static> {
    match target {
        TargetAddr::Ip(addr) => tokio_socks::TargetAddr::Ip(*addr),
        TargetAddr::Domain { host, port } => {
            tokio_socks::TargetAddr::Domain(host.clone().into(), *port)
        }
    }
}

/// 判断上游错误是否表示"上游自身故障"（需要降级）。
///
/// 目标侧错误（HostUnreachable / ConnectionRefused / NetworkUnreachable 等）
/// 说明上游工作正常、只是目标不可达，不应把上游标记为 DOWN；
/// 认证失败属于配置问题，同样不降级（否则所有上游会被同时误标）。
fn is_upstream_fault(e: &tokio_socks::Error) -> bool {
    use tokio_socks::Error as E;
    matches!(
        e,
        E::Io(_)
            | E::ProxyServerUnreachable
            | E::InvalidResponseVersion
            | E::UnknownAuthMethod
            | E::InvalidReservedByte
    )
}

/// 把上游 SOCKS5 错误映射为回给下游的 REP 码（尽量保留具体原因）。
fn rep_from_upstream_error(e: &tokio_socks::Error) -> u8 {
    use tokio_socks::Error as E;
    match e {
        E::GeneralSocksServerFailure => socks5::REP_GENERAL_FAILURE,
        E::ConnectionNotAllowedByRuleset => socks5::REP_NOT_ALLOWED,
        E::NetworkUnreachable => socks5::REP_NETWORK_UNREACHABLE,
        E::HostUnreachable | E::ProxyServerUnreachable => socks5::REP_HOST_UNREACHABLE,
        E::ConnectionRefused => socks5::REP_CONNECTION_REFUSED,
        E::TtlExpired => socks5::REP_TTL_EXPIRED,
        E::CommandNotSupported => socks5::REP_COMMAND_NOT_SUPPORTED,
        E::AddressTypeNotSupported | E::UnknownAddressType => {
            socks5::REP_ADDRESS_TYPE_NOT_SUPPORTED
        }
        _ => socks5::REP_GENERAL_FAILURE,
    }
}

/// 双向转发（对任意 AsyncRead+AsyncWrite 的流）。
async fn relay_streams<D, U>(
    ctx: Arc<AppCtx>,
    downstream: D,
    upstream: U,
    bufs: Buffers,
    peer: SocketAddr,
    mode: &'static str,
) where
    D: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    let (down_r, down_w) = tokio::io::split(downstream);
    let (up_r, up_w) = tokio::io::split(upstream);

    let stats = Arc::clone(&ctx.stats);
    let cancel = ctx.cancel.clone();

    let up_flow = pump(
        down_r,
        up_w,
        bufs.down_to_up,
        Direction::DownToUp,
        Arc::clone(&stats),
        cancel.clone(),
    );
    let down_flow = pump(
        up_r,
        down_w,
        bufs.up_to_down,
        Direction::UpToDown,
        Arc::clone(&stats),
        cancel,
    );

    // 任一方向结束（EOF/错误/取消）即关闭整条连接。
    let (direction, result) = tokio::select! {
        r = up_flow => ("down->up", r),
        r = down_flow => ("up->down", r),
    };

    match result {
        Ok(()) => trace!(%peer, mode, direction, "转发结束"),
        Err(e) => {
            stats.relay_errors.fetch_add(1, Ordering::Relaxed);
            debug!(%peer, mode, direction, error = %e, "转发异常结束");
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Direction {
    DownToUp,
    UpToDown,
}

/// 单向搬运循环：读满 → 写完 → 再读，单在途 read/write。
async fn pump<R, W>(
    mut reader: R,
    mut writer: W,
    mut buf: Box<[u8]>,
    direction: Direction,
    stats: Arc<Stats>,
    cancel: CancellationToken,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let n = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(()),
            r = reader.read(&mut buf) => r?,
        };
        if n == 0 {
            let _ = writer.shutdown().await;
            return Ok(());
        }
        match direction {
            Direction::DownToUp => stats
                .bytes_down_to_up
                .fetch_add(n as u64, Ordering::Relaxed),
            Direction::UpToDown => stats
                .bytes_up_to_down
                .fetch_add(n as u64, Ordering::Relaxed),
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok(()),
            r = writer.write_all(&buf[..n]) => r?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 缓冲按配置大小分配() {
        for size in [2048, 4096, 8192] {
            let b = Buffers::alloc(size);
            assert_eq!(b.down_to_up.len(), size);
            assert_eq!(b.up_to_down.len(), size);
        }
    }

    #[tokio::test]
    async fn 透传回环正确() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

        let (mut source_writer, pump_reader) = duplex(1024);
        let (pump_writer, mut sink_reader) = duplex(1024);

        let stats = Stats::new();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(pump(
            pump_reader,
            pump_writer,
            vec![0u8; 64].into_boxed_slice(),
            Direction::DownToUp,
            Arc::clone(&stats),
            cancel.clone(),
        ));

        source_writer.write_all(b"hello-m2").await.unwrap();
        let mut out = [0u8; 8];
        sink_reader.read_exact(&mut out).await.unwrap();
        assert_eq!(&out, b"hello-m2");
        assert_eq!(stats.bytes_down_to_up.load(Ordering::Relaxed), 8);

        drop(source_writer);
        task.await.unwrap().unwrap();

        let (mut source2, pump_reader2) = duplex(1024);
        let (pump_writer2, _sink2) = duplex(1024);
        let cancel2 = CancellationToken::new();
        let task2 = tokio::spawn(pump(
            pump_reader2,
            pump_writer2,
            vec![0u8; 64].into_boxed_slice(),
            Direction::UpToDown,
            Arc::clone(&stats),
            cancel2.clone(),
        ));
        source2.write_all(b"x").await.unwrap();
        cancel2.cancel();
        task2.await.unwrap().unwrap();
    }

    #[test]
    fn 上游错误映射为具体_rep() {
        use tokio_socks::Error as E;
        assert_eq!(
            rep_from_upstream_error(&E::ConnectionRefused),
            socks5::REP_CONNECTION_REFUSED
        );
        assert_eq!(
            rep_from_upstream_error(&E::HostUnreachable),
            socks5::REP_HOST_UNREACHABLE
        );
        assert_eq!(
            rep_from_upstream_error(&E::GeneralSocksServerFailure),
            socks5::REP_GENERAL_FAILURE
        );
        assert_eq!(
            rep_from_upstream_error(&E::CommandNotSupported),
            socks5::REP_COMMAND_NOT_SUPPORTED
        );
    }

    #[test]
    fn 目标地址转换保持域名() {
        let t = TargetAddr::Domain {
            host: "example.com".into(),
            port: 443,
        };
        match to_socks_target(&t) {
            tokio_socks::TargetAddr::Domain(host, port) => {
                assert_eq!(&*host, "example.com");
                assert_eq!(port, 443);
            }
            other => panic!("应为域名目标，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn 失败应答格式按协议区分() {
        use tokio::io::AsyncReadExt;
        // SOCKS5
        let (mut c, mut s) = tokio::io::duplex(64);
        write_failure(&mut s, Reply::Socks5, socks5::REP_CONNECTION_REFUSED)
            .await
            .unwrap();
        let mut buf = [0u8; 10];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf[0], 0x05);
        assert_eq!(buf[1], 0x05);

        // SOCKS4
        let (mut c, mut s) = tokio::io::duplex(64);
        write_failure(&mut s, Reply::Socks4, 0).await.unwrap();
        let mut buf = [0u8; 8];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, [0x00, 0x5B, 0, 0, 0, 0, 0, 0]);

        // HTTP
        let (mut c, mut s) = tokio::io::duplex(128);
        write_failure(&mut s, Reply::HttpConnect, 0).await.unwrap();
        let mut chunk = [0u8; 128];
        let n = c.read(&mut chunk).await.unwrap();
        let text = String::from_utf8_lossy(&chunk[..n]);
        assert!(text.starts_with("HTTP/1.1 502"), "实际: {text}");
    }
}
