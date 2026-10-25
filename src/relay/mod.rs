//! 连接处理：SOCKS5 终结转发（M1）与纯 TCP 透传（`traditionTcpRelay`）。
//!
//! 关键设计（docs/02 §3/§8、docs/07 ADR-005）：
//! - 双向缓冲在 accept 时一次性预分配，运行期零分配；
//! - 每方向"单在途 read + 单在途 write"，天然背压；
//! - 上游 TCP 建连与 SOCKS5 握手都带超时；
//! - 握手成功后才向客户端写 SOCKS5 成功应答，失败时写错误码（修正 C++ 缺陷 B-4）；
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
use crate::handshake::{self, socks5};
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

/// 处理一条下游连接。
pub async fn handle_connection(
    ctx: Arc<AppCtx>,
    downstream: TcpStream,
    peer: SocketAddr,
    local: SocketAddr,
    bufs: Buffers,
    guard: ActiveGuard,
) {
    if ctx.cfg.tradition_tcp_relay {
        // 透传模式：先选上游（无可用上游只能关闭连接）
        let Some(upstream_cfg) = ctx.pick_upstream().cloned() else {
            ctx.stats.rejected.fetch_add(1, Ordering::Relaxed);
            debug!(%peer, "无可用上游，关闭连接");
            return;
        };
        handle_tradition(ctx, downstream, upstream_cfg, peer, local, bufs, guard).await;
    } else {
        // SOCKS5 终结模式：先完成入站协商拿到 target，再选上游（失败可回 SOCKS5 错误码）
        handle_socks5(ctx, downstream, peer, local, bufs, guard).await;
    }
}

/// 纯 TCP 透传：不做任何协议解析，下游字节直接交给上游（`traditionTcpRelay=true`）。
async fn handle_tradition(
    ctx: Arc<AppCtx>,
    downstream: TcpStream,
    upstream_cfg: Upstream,
    peer: SocketAddr,
    local: SocketAddr,
    bufs: Buffers,
    _guard: ActiveGuard,
) {
    let upstream = match connect_upstream(&ctx, &upstream_cfg, peer).await {
        Some(s) => s,
        None => return,
    };
    trace!(%peer, %local, upstream = %upstream_cfg.name, addr = %upstream_cfg.addr, "开始透传");
    relay_streams(ctx, downstream, upstream, bufs, peer, "tradition").await;
}

/// SOCKS5 终结转发：解析入站请求 → 以上游客户端身份建立隧道 → 双向转发。
async fn handle_socks5(
    ctx: Arc<AppCtx>,
    mut downstream: TcpStream,
    peer: SocketAddr,
    local: SocketAddr,
    bufs: Buffers,
    _guard: ActiveGuard,
) {
    // 1) 首包探测（防"哑连接"：M0/C++ 的永久泄漏问题在此消除）。
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
            // 显式 FIN，避免直接 drop 触发 RST 干扰客户端诊断
            drain_and_close(downstream).await;
            return;
        }
    }
    if first[0] != socks5::VERSION {
        ctx.stats.protocol_errors.fetch_add(1, Ordering::Relaxed);
        debug!(%peer, first_byte = first[0], "M1 仅支持 SOCKS5 入站（SOCKS4/HTTP 将在 M2 实现）");
        drain_and_close(downstream).await;
        return;
    }

    // 2) 入站协商（方法选择 / 认证 / CONNECT 解析）
    let target = match tokio::time::timeout(
        ctx.cfg.handshake_timeout,
        socks5::negotiate(&mut downstream, &ctx.cfg.auth),
    )
    .await
    {
        Ok(Ok(target)) => target,
        Ok(Err(e)) => {
            // negotiate 内部已尽力写回 SOCKS5 错误码
            ctx.stats.handshake_failed.fetch_add(1, Ordering::Relaxed);
            debug!(%peer, error = %e, "入站 SOCKS5 协商失败");
            drain_and_close(downstream).await;
            return;
        }
        Err(_) => {
            ctx.stats.handshake_failed.fetch_add(1, Ordering::Relaxed);
            let _ = socks5::write_error_reply(&mut downstream, socks5::REP_TTL_EXPIRED).await;
            debug!(%peer, "入站 SOCKS5 握手超时");
            drain_and_close(downstream).await;
            return;
        }
    };
    trace!(%peer, target = %target, "收到 SOCKS5 CONNECT");

    // 3) 选择可用上游（全部 DOWN → 明确拒绝，避免黑洞）
    let Some(upstream_cfg) = ctx.pick_upstream().cloned() else {
        ctx.stats.rejected.fetch_add(1, Ordering::Relaxed);
        let _ = socks5::write_error_reply(&mut downstream, socks5::REP_GENERAL_FAILURE).await;
        debug!(
            %peer,
            available = ctx.health.available_count(),
            total = ctx.health.len(),
            "所有上游不可用，拒绝 CONNECT"
        );
        drain_and_close(downstream).await;
        return;
    };
    let up_name = upstream_cfg.name.clone();

    // 4) 连接上游 TCP
    let Some(upstream_raw) = connect_upstream(&ctx, &upstream_cfg, peer).await else {
        let _ = socks5::write_error_reply(&mut downstream, socks5::REP_HOST_UNREACHABLE).await;
        return;
    };

    // 5) 以上游 SOCKS5 客户端身份握手（含上游认证）；域名交给上游解析（socks5h）
    let up_target = to_socks_target(&target);
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

    let handshake = async move {
        match creds {
            Some((user, pwd)) => {
                Socks5Stream::connect_with_password_and_socket(upstream_raw, up_target, &user, &pwd)
                    .await
            }
            None => Socks5Stream::connect_with_socket(upstream_raw, up_target).await,
        }
    };
    let upstream = match tokio::time::timeout(ctx.cfg.handshake_timeout, handshake).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            ctx.stats
                .upstream_handshake_failed
                .fetch_add(1, Ordering::Relaxed);
            // 只有"上游自身故障"才降级；目标不可达/认证配置错等不应误伤上游
            if is_upstream_fault(&e) {
                ctx.health
                    .mark_degraded(upstream_cfg.health_index, "上游握手失败");
            }
            let rep = rep_from_upstream_error(&e);
            let _ = socks5::write_error_reply(&mut downstream, rep).await;
            debug!(%peer, upstream = %up_name, error = %e, rep, "上游 SOCKS5 握手失败");
            return;
        }
        Err(_) => {
            ctx.stats
                .upstream_handshake_failed
                .fetch_add(1, Ordering::Relaxed);
            ctx.health
                .mark_degraded(upstream_cfg.health_index, "上游握手超时");
            let _ = socks5::write_error_reply(&mut downstream, socks5::REP_TTL_EXPIRED).await;
            debug!(%peer, upstream = %up_name, "上游 SOCKS5 握手超时");
            return;
        }
    };

    // 6) 上游成功后，才回复客户端成功
    if let Err(e) = socks5::write_success_reply(&mut downstream).await {
        debug!(%peer, error = %e, "写 SOCKS5 成功应答失败");
        return;
    }
    trace!(%peer, %local, upstream = %up_name, target = %target, "隧道建立，开始转发");

    // 7) 双向转发
    relay_streams(ctx, downstream, upstream, bufs, peer, "socks5").await;
}

/// 握手失败后的优雅关闭：先半关闭写端，再短暂丢弃剩余输入。
///
/// 目的：避免"带着未读输入直接 drop"触发 RST，导致客户端可能收不到刚刚写回的
/// SOCKS5 错误码（例如 BIND 请求的地址字段还没读，内核缓冲里仍有数据）。
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

/// 连接上游 TCP（带超时），成功后关闭 Nagle。
async fn connect_upstream(
    ctx: &Arc<AppCtx>,
    upstream_cfg: &Upstream,
    peer: SocketAddr,
) -> Option<TcpStream> {
    match tokio::time::timeout(
        ctx.cfg.connect_timeout,
        TcpStream::connect(upstream_cfg.addr),
    )
    .await
    {
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
            debug!(%peer, upstream = %upstream_cfg.name, addr = %upstream_cfg.addr, error = %e, "连接上游失败");
            None
        }
        Err(_) => {
            ctx.stats.connect_failed.fetch_add(1, Ordering::Relaxed);
            ctx.health
                .mark_degraded(upstream_cfg.health_index, "连接上游超时");
            debug!(
                %peer,
                upstream = %upstream_cfg.name,
                addr = %upstream_cfg.addr,
                timeout_ms = ctx.cfg.connect_timeout.as_millis() as u64,
                "连接上游超时"
            );
            None
        }
    }
}

/// 把入站目标地址转成 tokio-socks 的目标地址类型（域名保持域名，交给上游解析）。
fn to_socks_target(target: &handshake::TargetAddr) -> tokio_socks::TargetAddr<'static> {
    match target {
        handshake::TargetAddr::Ip(addr) => tokio_socks::TargetAddr::Ip(*addr),
        handshake::TargetAddr::Domain { host, port } => {
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

/// 双向转发（对任意 AsyncRead+AsyncWrite 的流，便于 tradition/SOCKS5 复用）。
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

    // 任一方向结束（EOF/错误/取消）即关闭整条连接，与 C++ 版行为一致。
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
            // 正常半关闭：把 FIN 传递给对端后退出。
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

        // 两个 duplex 端点分别代表"读来源"和"写目标"，中间跑 pump。
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

        source_writer.write_all(b"hello-m0").await.unwrap();
        let mut out = [0u8; 8];
        sink_reader.read_exact(&mut out).await.unwrap();
        assert_eq!(&out, b"hello-m0");
        assert_eq!(stats.bytes_down_to_up.load(Ordering::Relaxed), 8);

        // 关闭写端 → pump 读到 EOF → 正常返回 Ok
        drop(source_writer);
        task.await.unwrap().unwrap();

        // 取消令牌可中断阻塞中的读
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
        let t = handshake::TargetAddr::Domain {
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
}
