//! 多监听与 accept 循环。
//!
//! 与 C++ 版 `TcpRelayServer` 的差异（见 docs/02 §2、docs/07 ADR-001）：
//! - 支持每个监听目标解析出的**全部**地址（`::` 等场景）；
//! - accept 错误不再无脑立即重试：`EMFILE/ENFILE/WSAEMFILE` 指数退避，避免自旋；
//! - 每个连接在 accept 时**一次性预分配**双向缓冲（ADR-005）；
//! - 连接数上限用 CAS 精确控制（`ActiveGuard`），超限直接 RST/drop，不黑洞。

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::app::AppCtx;
use crate::config::ListenTarget;
use crate::relay;
use crate::stats::ActiveGuard;

/// accept 队列长度（与 C++ 的默认 backlog 相比放大，降低突发建连丢包）。
const LISTEN_BACKLOG: i32 = 1024;

/// 绑定全部监听目标，返回 `(listener, 本地地址)`。
pub async fn bind_all(
    targets: &[ListenTarget],
    socket_recv_buffer: usize,
    socket_send_buffer: usize,
) -> io::Result<Vec<(TcpListener, SocketAddr)>> {
    let mut out: Vec<(TcpListener, SocketAddr)> = Vec::new();
    let mut last_err: Option<io::Error> = None;

    for t in targets {
        let addrs = match tokio::net::lookup_host((t.host.as_str(), t.port)).await {
            Ok(it) => it.collect::<Vec<_>>(),
            Err(e) => {
                warn!(target = %t, error = %e, "监听地址解析失败");
                last_err = Some(e);
                continue;
            }
        };
        if addrs.is_empty() {
            warn!(target = %t, "监听地址解析结果为空");
            continue;
        }
        for addr in addrs {
            if out.iter().any(|(_, local)| *local == addr) {
                continue;
            }
            match bind_one(addr, socket_recv_buffer, socket_send_buffer) {
                Ok(listener) => {
                    let local = listener.local_addr()?;
                    out.push((listener, local));
                }
                Err(e) => {
                    warn!(%addr, error = %e, "绑定失败");
                    last_err = Some(e);
                }
            }
        }
    }

    if out.is_empty() {
        return Err(last_err.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::AddrNotAvailable, "没有任何监听地址绑定成功")
        }));
    }
    Ok(out)
}

fn bind_one(
    addr: SocketAddr,
    socket_recv_buffer: usize,
    socket_send_buffer: usize,
) -> io::Result<TcpListener> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let sock = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    // SO_REUSEADDR 的语义在 Windows 与 Unix 上**完全不同**：
    // - Unix：允许在 TIME_WAIT 期间重新 bind 同一地址，是服务端快速重启的标准做法；
    // - Windows：它允许**其它进程抢占同一个端口**（端口劫持），不会报错，连接可能被投递到另一个进程。
    //   代理服务端不应接受这种语义；Windows 上保持默认（不设置），端口被占用时直接 bind 失败更安全。
    #[cfg(unix)]
    sock.set_reuse_address(true)?;
    // Linux：每个 worker 可各自 accept（M1 起配合多 listener 使用；此处先打开不影响语义）
    #[cfg(all(unix, not(target_os = "solaris"), not(target_os = "illumos")))]
    {
        if let Err(e) = sock.set_reuse_port(true) {
            debug!(error = %e, "SO_REUSEPORT 设置失败（忽略）");
        }
    }
    // IPv6 只监听 v6，避免与同端口的 v4 绑定冲突（等价于 C++ 分别 bind 两个 endpoint）。
    if addr.is_ipv6() {
        let _ = sock.set_only_v6(true);
    }
    // 显式 socket 缓冲（0 = 用系统默认）。注意：Linux 上 accepted socket 会继承 listener 的缓冲设置，
    // Windows 不继承；M1 会在 accepted 连接上再设置一次。
    if socket_recv_buffer > 0 {
        sock.set_recv_buffer_size(socket_recv_buffer)?;
    }
    if socket_send_buffer > 0 {
        sock.set_send_buffer_size(socket_send_buffer)?;
    }
    sock.set_nonblocking(true)?;
    sock.bind(&addr.into())?;
    sock.listen(LISTEN_BACKLOG)?;
    let std_listener: std::net::TcpListener = sock.into();
    TcpListener::from_std(std_listener)
}

/// 单个 listener 的 accept 循环。
pub async fn accept_loop(ctx: Arc<AppCtx>, listener: TcpListener, local: SocketAddr) {
    let mut tasks: JoinSet<()> = JoinSet::new();
    let mut backoff_ms: u64 = 0;

    loop {
        // 及时回收已结束的连接任务，避免 JoinSet 无限增长。
        while tasks.try_join_next().is_some() {}

        let accepted = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => break,
            r = listener.accept() => r,
        };

        match accepted {
            Ok((stream, peer)) => {
                backoff_ms = 0;
                on_accept(&ctx, stream, peer, local, &mut tasks);
            }
            Err(e) => {
                if ctx.cancel.is_cancelled() {
                    break;
                }
                if is_fd_exhausted(&e) {
                    backoff_ms = if backoff_ms == 0 {
                        100
                    } else {
                        (backoff_ms * 2).min(1000)
                    };
                    warn!(error = %e, backoff_ms, "文件描述符耗尽，退避后重试 accept");
                    tokio::select! {
                        _ = ctx.cancel.cancelled() => break,
                        _ = tokio::time::sleep(Duration::from_millis(backoff_ms)) => {}
                    }
                } else {
                    // 连接级错误（如对端在 accept 前 RST）不影响后续，短暂让出后继续。
                    warn!(error = %e, "accept 失败");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        }
    }

    info!(%local, "accept 循环退出，等待在途连接收尾");
    let drain = async { while tasks.join_next().await.is_some() {} };
    if tokio::time::timeout(Duration::from_secs(5), drain)
        .await
        .is_err()
    {
        warn!(%local, "连接收尾超时，强制中止剩余任务");
        tasks.shutdown().await;
    }
}

fn on_accept(
    ctx: &Arc<AppCtx>,
    stream: TcpStream,
    peer: SocketAddr,
    local: SocketAddr,
    tasks: &mut JoinSet<()>,
) {
    // 代理转发对延迟敏感：必须关闭 Nagle（C++ 版全仓库没有设置，见 docs/02 §7）。
    if let Err(e) = stream.set_nodelay(true) {
        debug!(%peer, error = %e, "set_nodelay 失败");
    }

    // 连接数上限：CAS 抢占名额，超限直接丢弃（RST），不进入转发。
    let guard = match ActiveGuard::try_acquire(&ctx.stats, ctx.cfg.max_connections) {
        Some(g) => g,
        None => {
            ctx.stats
                .rejected
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            debug!(%peer, %local, "拒绝连接：达到 maxConnections");
            return;
        }
    };
    ctx.stats
        .accepted
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // ADR-005：会话建立时一次性预分配双向缓冲（运行期零分配）。
    let bufs = relay::Buffers::alloc(ctx.cfg.buffer_size);

    let ctx = Arc::clone(ctx);
    tasks.spawn(async move {
        relay::handle_connection(ctx, stream, peer, local, bufs, guard).await;
    });
}

/// 文件描述符耗尽类错误（需要退避，不能自旋）。
fn is_fd_exhausted(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        // Linux EMFILE / ENFILE
        Some(24) | Some(23)
        // Windows WSAEMFILE
        | Some(10024)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emfile_被识别为需要退避() {
        let e = io::Error::from_raw_os_error(24);
        assert!(is_fd_exhausted(&e));
        let e = io::Error::from_raw_os_error(10024);
        assert!(is_fd_exhausted(&e));
        let e = io::Error::from_raw_os_error(9); // EBADF 不是耗尽
        assert!(!is_fd_exhausted(&e));
    }
}
