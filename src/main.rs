//! Socks5BalancerRust —— 多上游 SOCKS 代理负载均衡器（二进制入口）。
//!
//! 当前能力（阶段与验收状态见 docs/06 §10）：
//! - 配置解析（兼容 C++ `config.json`，支持新字段）与校验；
//! - `tracing` 异步日志；
//! - tokio runtime，worker/blocking/栈大小全部可配（ADR-003）；
//! - 多监听（每目标解析出的全部地址，`::` 支持双栈）；
//! - 混合端口按首包分派：SOCKS5 / SOCKS4 / SOCKS4a / HTTP CONNECT 与 absolute-form；
//! - 上游 SOCKS5 池与选路规则、健康检查（TCP 探活 + fail-fast + DOWN 退避 + TLS 探测）；
//! - 纯 TCP 透传模式（`traditionTcpRelay=true`）；
//! - 控制面/监控面只读端点、连接监测、优雅关闭（含 TCP 半关闭传播）。

// 模块定义在库 crate（src/lib.rs）中，二进制只做装配与启动
use socks5_balancer_rust::{app, config, conn, control, health, logging, net, stats, upstream};

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use mimalloc::MiMalloc;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

/// 全局分配器：多线程下降低分配竞争与碎片（ADR-007）。
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// mimalloc v3 运行期选项索引。
///
/// 与 `libmimalloc-sys` 内置 mimalloc **v3** 的 `include/mimalloc.h` 中 `mi_option_e`
/// 枚举顺序一致（0 起始）：`mi_option_arena_eager_commit` 是第 5 个 **= 4**、
/// `mi_option_purge_delay` 是第 16 个 **= 15**。
///
/// 注意：`libmimalloc-sys` 的 Rust 绑定只导出了部分旧选项名，这两个 v3 选项要按枚举值调用；
/// 升级 `libmimalloc-sys` 时需重新核对该枚举顺序。
const MI_OPTION_ARENA_EAGER_COMMIT: std::ffi::c_int = 4;
const MI_OPTION_PURGE_DELAY: std::ffi::c_int = 15;

/// 把 mimalloc 的低 RSS 选项设为默认值（M4：10K 连接 ≤100MB 口径，见 docs/12 实测）。
///
/// - `arena_eager_commit=0`：arena 不预先提交物理页，按需提交；
/// - `purge_delay=0`：空闲页**立即**归还 OS（mimalloc 默认延迟 10ms 且按比例保留，
///   高并发短连接场景会让 RSS 长期停在峰值附近）。
///
/// **必须在 `main` 之前调用**：mimalloc 在首次分配时初始化、arena 在创建时读取
/// `arena_eager_commit`；实测在 `main` 里设置（读回确实是 0）对已创建的 arena 无效——
/// Linux 10K 峰值仍是 110 MB，而在初始化前设置能到 93 MB。因此下面用 C 运行时初始化段
/// （Linux `.init_array` / Windows `.CRT$XCU`）在 `main` 前执行。
///
/// 运维仍可用环境变量 `MIMALLOC_ARENA_EAGER_COMMIT` / `MIMALLOC_PURGE_DELAY` 覆盖：
/// mimalloc 初始化时先读环境变量，这里仅在**未显式设置**时套默认值（不覆盖用户选择）。
fn init_mimalloc_low_rss_defaults() {
    let has_env = |name: &str| std::env::var_os(name).is_some();
    // SAFETY: `mi_option_set` 是 mimalloc 的 C API，在 C 运行时初始化段单线程调用，
    // 选项索引与内置 mimalloc v3 头文件一致（见上方常量注释）。
    unsafe {
        if !has_env("MIMALLOC_ARENA_EAGER_COMMIT") {
            libmimalloc_sys::mi_option_set(MI_OPTION_ARENA_EAGER_COMMIT, 0);
        }
        if !has_env("MIMALLOC_PURGE_DELAY") {
            libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, 0);
        }
    }
}

extern "C" fn mimalloc_low_rss_ctor() {
    init_mimalloc_low_rss_defaults();
}

/// Linux/Android：ELF `.init_array` 在 `main` 之前由 C 运行时调用。
#[cfg(any(target_os = "linux", target_os = "android"))]
#[used]
#[unsafe(link_section = ".init_array")]
static MIMALLOC_LOW_RSS_CTOR: extern "C" fn() = mimalloc_low_rss_ctor;

/// Windows：MSVC CRT 的 `.CRT$XCU` 段在 `main` 之前执行。
#[cfg(windows)]
#[used]
#[unsafe(link_section = ".CRT$XCU")]
static MIMALLOC_LOW_RSS_CTOR: extern "C" fn() = mimalloc_low_rss_ctor;

/// 命令行参数（与 C++ 版 `-c/--config`、`--help`、`--version` 兼容）。
#[derive(Parser, Debug)]
#[command(
    name = "Socks5BalancerRust",
    version,
    about = "多上游 SOCKS 代理负载均衡器（SOCKS5 / SOCKS4 / SOCKS4a / HTTP CONNECT，Rust 移植）"
)]
struct Cli {
    /// 配置文件路径
    #[arg(short, long, value_name = "CONFIG", default_value = "config.json")]
    config: PathBuf,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let cfg = match config::load(&cli.config) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("配置加载失败（{}）: {e}", cli.config.display());
            return ExitCode::FAILURE;
        }
    };

    // 日志 guard 必须活到进程结束。
    let _log_guard = logging::init(&cfg.log_level);
    info!("{}", logging::version_info());
    info!(config = %cli.config.display(), "配置已加载");
    for note in &cfg.notes {
        warn!("配置提示: {note}");
    }
    if !cfg.tradition_tcp_relay {
        info!(
            "入站模式：混合端口按首包分派（SOCKS5 / SOCKS4 / SOCKS4a / HTTP CONNECT 与 absolute-form）"
        );
        info!("SOCKS5 支持 0x00 无认证 / 0x02 用户名密码（ADR-004）");
        if cfg.auth.is_empty() {
            info!("未配置 AuthClientInfo：客户端将协商为 0x00 无认证");
        } else {
            info!(
                users = cfg.auth.len(),
                "已启用客户端认证（SOCKS5 0x02 / SOCKS4 userid / HTTP Basic）"
            );
        }
        if cfg.disable_socks4 {
            info!("SOCKS4/4a 已禁用（disableSocks4=true）");
        }
        if cfg.disable_http_forward {
            info!("HTTP absolute-form 正向代理已禁用（disableHttpForward=true），CONNECT 不受影响");
        }
    } else {
        info!("入站模式：纯 TCP 透传（traditionTcpRelay=true），不做协议解析");
    }

    let workers = config::effective_worker_threads(&cfg.thread);
    // ADR-003：`workerThreads` 允许超过核数（特殊场景），但超订阅无收益，启动时明确告警。
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    if workers > cores {
        warn!(
            workers,
            cores, "worker 线程数超过可用核数（ADR-003：超订阅无收益，仅用于特殊场景）"
        );
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .max_blocking_threads(cfg.thread.max_blocking_threads)
        .thread_stack_size(cfg.thread.stack_size)
        .thread_name("s5b")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!("构建 tokio runtime 失败: {e}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(cfg, workers, cores)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("运行期错误: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(
    cfg: config::Config,
    workers: usize,
    cores: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Arc::new(cfg);
    info!(
        workers,
        available_cores = cores,
        max_blocking_threads = cfg.thread.max_blocking_threads,
        thread_stack_kb = cfg.thread.stack_size / 1024,
        buffer_size = cfg.buffer_size,
        max_connections = cfg.max_connections,
        connect_timeout_ms = cfg.connect_timeout.as_millis() as u64,
        first_packet_timeout_ms = cfg.first_packet_timeout.as_millis() as u64,
        handshake_timeout_ms = cfg.handshake_timeout.as_millis() as u64,
        auth_users = cfg.auth.len(),
        retry_times = cfg.retry_times,
        tradition_tcp_relay = cfg.tradition_tcp_relay,
        disable_socks4 = cfg.disable_socks4,
        disable_http_forward = cfg.disable_http_forward,
        listen_targets = cfg.listen.len(),
        upstreams = cfg.upstreams.iter().filter(|u| u.enabled).count(),
        health_period_ms = cfg.health.period.as_millis() as u64,
        health_timeout_ms = cfg.health.timeout.as_millis() as u64,
        "运行时配置生效"
    );

    let (upstreams, failures) = upstream::resolve_all(&cfg.upstreams).await;
    if !failures.is_empty() {
        warn!(failed = failures.len(), "部分上游解析失败，已从池中剔除");
    }
    if upstreams.is_empty() {
        return Err("所有上游均解析失败，无法启动".into());
    }

    let health = health::HealthRegistry::new(&upstreams, cfg.health.clone());
    let selector = Arc::new(upstream::UpstreamSelector::new(
        upstreams,
        cfg.select_rule,
        cfg.server_change_time,
    ));
    info!(
        count = selector.len(),
        rule = selector.rule().as_str(),
        server_change_time_ms = cfg.server_change_time.as_millis() as u64,
        "上游池就绪"
    );
    info!(
        period_ms = health.cfg().period.as_millis() as u64,
        timeout_ms = health.cfg().timeout.as_millis() as u64,
        backoff_initial_ms = health.cfg().backoff_initial.as_millis() as u64,
        backoff_multiplier = health.cfg().backoff_multiplier,
        backoff_max_ms = health.cfg().backoff_max.as_millis() as u64,
        probe_jitter = health.cfg().probe_jitter,
        sleep_time_ms = health
            .cfg()
            .sleep_time
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        addition_check_period_ms = health
            .cfg()
            .addition_check_period
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        "健康检查已启用：TCP 探活 + fail-fast + DOWN 指数退避"
    );
    if cfg.health.enabled {
        match &cfg.health.tls {
            Some(tls) => info!(
                host = %tls.host,
                port = tls.port,
                sni = %tls.sni,
                http_path = %tls.http_path,
                period_ms = tls.period.as_millis() as u64,
                timeout_ms = tls.timeout.as_millis() as u64,
                max_concurrency = tls.max_concurrency,
                "TLS/HTTP readiness 探测已启用（ADR-006）"
            ),
            None => info!("未配置 healthCheck.target：仅做 TCP liveness 探测"),
        }
    } else {
        info!("健康检查已禁用（disableConnectTest=true）：不探测、数据面失败不降级，仅按人工启停");
    }
    let stats = stats::Stats::new();
    let cancel = CancellationToken::new();
    // 连接监测（默认开启明细，与 C++ 会话表行为接近）
    let conns = conn::ConnRegistry::new(cfg.conn_tracking.clone(), selector.len());
    if cfg.conn_tracking.enabled {
        info!(
            detail = cfg.conn_tracking.detail,
            detail_limit = cfg.conn_tracking.detail_limit,
            client_top_limit = cfg.conn_tracking.client_top_limit,
            "连接监测已启用"
        );
    } else {
        info!("连接监测已禁用（connectionTracking.enabled=false）");
    }
    if cfg.health.enabled {
        health.spawn_probes(cancel.clone());
    }
    // 上游域名 DNS TTL 刷新（可选）：IP 字面量不刷新，解析失败保留旧地址
    if let Some(ttl) = cfg.dns_ttl {
        upstream::spawn_dns_refresher(Arc::clone(&selector), ttl, cancel.clone());
        info!(
            dns_ttl_ms = ttl.as_millis() as u64,
            "上游域名 DNS 周期刷新已启用"
        );
    }
    let ctx = app::AppCtx::new(
        Arc::clone(&cfg),
        selector,
        Arc::clone(&health),
        Arc::clone(&stats),
        Arc::clone(&conns),
        cancel.clone(),
    );

    // per-core accept（docs/02 §2）：Linux 上每个监听地址按核数复制 listener（SO_REUSEPORT
    // 由内核分发，降低单 accept 队列锁竞争）；Windows 不支持且会带来端口劫持风险，固定 1 个。
    // 上限 4：tokio 多线程下 accept 本身已可并行，副本过多只会增加 socket 数量。
    let replicas = if cfg!(unix) { workers.clamp(1, 4) } else { 1 };
    let listeners = net::listener::bind_all(
        &cfg.listen,
        cfg.socket_recv_buffer,
        cfg.socket_send_buffer,
        replicas,
    )
    .await?;
    {
        let mut by_addr: std::collections::BTreeMap<SocketAddr, usize> =
            std::collections::BTreeMap::new();
        for (_, local) in &listeners {
            *by_addr.entry(*local).or_default() += 1;
        }
        for (local, n) in &by_addr {
            info!(%local, replicas = n, "监听已就绪");
        }
    }

    let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    for (listener, local) in listeners {
        let ctx = Arc::clone(&ctx);
        tasks.spawn(async move {
            net::listener::accept_loop(ctx, listener, local).await;
        });
    }

    let stats_task = spawn_stats_logger(Arc::clone(&stats), Arc::clone(&health), cancel.clone());

    // 控制面（M3，ADR-001）：无鉴权、默认回环；绑定失败只告警，不影响主服务。
    if let Some(state_bind) = cfg.state_server.clone() {
        let ctx = Arc::clone(&ctx);
        let max_response = cfg.state_max_response_bytes;
        let cancel = cancel.clone();
        tokio::spawn(async move {
            control::serve(ctx, state_bind, max_response, cancel).await;
        });
    } else {
        info!("控制面未启用（stateServerPort=0）");
    }

    // 监控面（可选独立监听，配置 monitorServerHost/monitorServerPort）：
    // 只读端点（含连接明细/摘要），不提供 /api/v1/control 写命令。
    if let Some(monitor_bind) = cfg.monitor_server.clone() {
        let ctx = Arc::clone(&ctx);
        let max_response = cfg.state_max_response_bytes;
        let cancel = cancel.clone();
        tokio::spawn(async move {
            control::serve_monitor(ctx, monitor_bind, max_response, cancel).await;
        });
    } else if cfg.state_server.is_some() {
        info!("监控端点复用控制面监听（未配置 monitorServerPort）");
    }

    let signal = wait_signal().await;
    info!(signal, "收到退出信号，开始优雅关闭");
    cancel.cancel();

    // 有界收尾：最多等 12 秒，之后强制中止剩余任务。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                warn!("等待连接收尾超时，强制中止");
                tasks.shutdown().await;
                break;
            }
            joined = tasks.join_next() => {
                if joined.is_none() {
                    break;
                }
            }
        }
    }

    stats_task.abort();
    info!(target: "stats", "退出统计: {}", stats.snapshot().summary());
    Ok(())
}

/// 每 30 秒输出一次统计摘要与不可用上游（M3 换成控制面快照）。
fn spawn_stats_logger(
    stats: Arc<stats::Stats>,
    health: Arc<health::HealthRegistry>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        interval.tick().await; // 跳过首次立即触发的 tick
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    let snapshot = stats.snapshot();
                    if snapshot.accepted > 0 || snapshot.active > 0 {
                        info!(target: "stats", "{}", snapshot.summary());
                    }
                    let unavailable: Vec<String> = health
                        .snapshot()
                        .into_iter()
                        .filter(|s| !s.available)
                        .map(|s| {
                            format!(
                                "{}({},tcp={},tls={},fail={},tls_fail={},backoff={}ms)",
                                s.name,
                                s.addr,
                                s.liveness,
                                s.readiness,
                                s.failures,
                                s.tls_failures,
                                s.backoff_ms
                            )
                        })
                        .collect();
                    if !unavailable.is_empty() {
                        info!(
                            target: "health",
                            available = health.available_count(),
                            total = health.len(),
                            upstreams = ?unavailable,
                            "上游健康状态"
                        );
                    }
                }
            }
        }
    })
}

/// 等待退出信号（Linux: SIGINT/SIGTERM；Windows: Ctrl-C）。
async fn wait_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    r = tokio::signal::ctrl_c() => {
                        if let Err(e) = r {
                            warn!("等待 SIGINT 失败: {e}");
                        }
                        "SIGINT"
                    }
                    _ = sigterm.recv() => "SIGTERM",
                }
            }
            Err(e) => {
                warn!("注册 SIGTERM 失败: {e}");
                if let Err(e) = tokio::signal::ctrl_c().await {
                    warn!("等待 Ctrl-C 失败: {e}");
                }
                "CTRL_C"
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(e) = tokio::signal::ctrl_c().await {
            warn!("等待 Ctrl-C 失败: {e}");
        }
        "CTRL_C"
    }
}
