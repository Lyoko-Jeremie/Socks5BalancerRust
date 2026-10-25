//! 连接转发（M0：纯 TCP 透传）。
//!
//! 关键设计（docs/02-relay-core.md §3、§8 与 docs/07 ADR-005）：
//! - 双向缓冲在 accept 时一次性预分配，运行期零分配；
//! - 每方向"单在途 read + 单在途 write"，天然背压；
//! - 上游建连带超时（C++ 版 `connectTimeout` 是死配置，这里真正落地）；
//! - 全局取消令牌可中断阻塞中的读写，保证优雅关闭。

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use crate::app::AppCtx;
use crate::stats::{ActiveGuard, Stats};

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
///
/// `_guard` 持有活跃连接名额，函数返回时自动归还（Drop），因此所有返回路径都无需手动计数。
pub async fn handle_connection(
    ctx: Arc<AppCtx>,
    downstream: TcpStream,
    peer: SocketAddr,
    local: SocketAddr,
    bufs: Buffers,
    _guard: ActiveGuard,
) {
    let Some(upstream_cfg) = ctx.selector.next().cloned() else {
        // 理论上启动时已保证非空；运行期若被清空则拒绝。
        ctx.stats.rejected.fetch_add(1, Ordering::Relaxed);
        debug!(%peer, "无可用上游，关闭连接");
        return;
    };

    // 连接上游：带超时（ADR-002/06 §7.5）。
    let connect = TcpStream::connect(upstream_cfg.addr);
    let upstream = match tokio::time::timeout(ctx.cfg.connect_timeout, connect).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            ctx.stats.connect_failed.fetch_add(1, Ordering::Relaxed);
            debug!(%peer, upstream = %upstream_cfg.name, addr = %upstream_cfg.addr, error = %e, "连接上游失败");
            return;
        }
        Err(_) => {
            ctx.stats.connect_failed.fetch_add(1, Ordering::Relaxed);
            debug!(%peer, upstream = %upstream_cfg.name, addr = %upstream_cfg.addr, timeout_ms = ctx.cfg.connect_timeout.as_millis() as u64, "连接上游超时");
            return;
        }
    };
    if let Err(e) = upstream.set_nodelay(true) {
        debug!(error = %e, "上游 set_nodelay 失败");
    }

    trace!(%peer, %local, upstream = %upstream_cfg.name, addr = %upstream_cfg.addr, "开始透传");

    let (down_r, down_w) = downstream.into_split();
    let (up_r, up_w) = upstream.into_split();

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
        Ok(()) => trace!(%peer, direction, "透传结束"),
        Err(e) => {
            stats.relay_errors.fetch_add(1, Ordering::Relaxed);
            debug!(%peer, direction, error = %e, "透传异常结束");
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
            // 上游/下游正常半关闭：把 FIN 传递给对端后退出。
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
}
