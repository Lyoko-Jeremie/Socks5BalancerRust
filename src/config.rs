//! 配置加载与校验。
//!
//! 设计要点（见 docs/07-decisions.md）：
//! - 完整兼容 C++ 版 `config.json` 的字段名（camelCase，部分字段首字母大写需 alias）；
//! - 新增字段一律可选并带默认值：`bufferSize` / `workerThreads` / `maxBlockingThreads` /
//!   `threadStackSize` / `maxConnections` / `logLevel` / `socketRecvBuffer` / `socketSendBuffer`；
//! - `internalBehavior.*` 在 C++ 中从未被解析（死配置），M0 只把其中的 `connectTimeout`
//!   作为顶层字段缺失时的兜底，其余仅提示忽略；
//! - 解析后统一做范围校验，错误信息必须能让运维一次改对。

use std::fs;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::upstream::SelectRule;

/// 转发缓冲默认值（ADR-005：默认双向 8KB 预分配）。
pub const DEFAULT_BUFFER_SIZE: usize = 8192;
/// 允许的缓冲块大小（方案 B：2048/4096/8192）。
pub const ALLOWED_BUFFER_SIZES: [usize; 3] = [2048, 4096, 8192];
/// 上游建连默认超时（C++ 版 `connectTimeout` 默认 2000ms，但从未被使用；这里真正落地）。
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 2000;
/// 默认连接数上限（C10K 目标）。
pub const DEFAULT_MAX_CONNECTIONS: usize = 10_000;
/// 首包默认超时：下游连上后 15s 不发数据即关闭。
pub const DEFAULT_FIRST_PACKET_TIMEOUT_MS: u64 = 15_000;
/// 握手默认超时（方法/认证协商、上游 SOCKS5 握手）。
pub const DEFAULT_HANDSHAKE_TIMEOUT_MS: u64 = 10_000;
/// `spawn_blocking` 线程默认上限。
pub const DEFAULT_MAX_BLOCKING_THREADS: usize = 2;
/// 线程栈默认 512KB（ADR-003）。
pub const DEFAULT_THREAD_STACK_SIZE: usize = 512 * 1024;
/// worker 线程数硬上限，防止手滑把机器打爆。
pub const MAX_WORKER_THREADS: usize = 256;
/// `sleepTime` 默认值：1800000ms = 30min（对齐 C++ `ConfigLoader.cpp:226`）。
///
/// 语义：**最近该时长内没有任何新连接**就进入空闲休眠，暂停全部自动探测。目的是避免
/// 长时间无人使用时，周期探测在后端链路上留下单调、有规律的流量，被链路状态检测/防火墙
/// 判为异常通信而拉黑后端（C++ README "Full configuration" 与作者说明）。
pub const DEFAULT_SLEEP_TIME_MS: u64 = 30 * 60 * 1000;
/// `additionCheckPeriod` 默认值：10000ms = 10s（对齐 C++ `ConfigLoader.cpp:239`）。
/// 全池不可用且不在空闲休眠时，按该周期强制探测全池。
pub const DEFAULT_ADDITION_CHECK_PERIOD_MS: u64 = 10 * 1000;
/// `tcpCheckStart` / `connectCheckStart` 默认值：1000ms（对齐 C++）。
/// 启动后等待该时长再做首轮探测，给"后端与本程序同时启动、后端起得更慢"的场景留窗口。
pub const DEFAULT_CHECK_START_MS: u64 = 1_000;

/// 配置错误。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("读取配置文件失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("配置文件不是合法 UTF-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),
    #[error("配置 JSON 解析失败: {0}")]
    Json(#[from] serde_json::Error),
    #[error("配置非法: {0}")]
    Invalid(String),
}

fn err(msg: impl Into<String>) -> ConfigError {
    ConfigError::Invalid(msg.into())
}

/// 判断监听 host 是否为回环地址（用于控制面安全提示）。
fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host
        .trim_matches(|c| c == '[' || c == ']')
        .parse::<std::net::IpAddr>()
    {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

fn default_listen_host() -> String {
    "127.0.0.1".to_string()
}

fn default_listen_port() -> u16 {
    5000
}

fn default_retry_times() -> usize {
    3
}

fn default_max_blocking_threads() -> usize {
    DEFAULT_MAX_BLOCKING_THREADS
}

fn default_log_level() -> String {
    "info".to_string()
}

/// `disableSocks4` 的默认值：与 C++ 一致（默认 **true**，即默认禁用 SOCKS4/4a）。
///
/// 用途：C++ 作者默认关闭 SOCKS4，是因为不少陈旧客户端对 SOCKS4a 的实现并不正确
/// （域名长度/结尾 NUL 处理、USERID 截断等），放开会带来难排查的异常请求。需要兼容旧
/// 客户端时必须显式写 `"disableSocks4": false`（Rust 版保留了完整的 SOCKS4/4a 支持）。
fn default_true() -> bool {
    true
}

/// 一个监听目标（host + port）。
#[derive(Debug, Clone, Deserialize)]
pub struct ListenAddr {
    #[serde(default = "default_listen_host")]
    pub host: String,
    #[serde(default = "default_listen_port")]
    pub port: u16,
}

/// 原始上游条目（字段名兼容 C++ 配置）。
///
/// `slow_impl` / `auth_*` 等字段在 M0 仅解析，M2（握手兼容/上游认证）启用；
/// 这里保留定义以保证旧配置可原样读取且字段含义有文档。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawUpstream {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub auth_user: Option<String>,
    #[serde(default)]
    pub auth_pwd: Option<String>,
    /// C++ 的 `slowImpl`，M2（握手兼容）才会使用；M0 只记录。
    #[serde(default)]
    pub slow_impl: bool,
    /// C++ 里可能是 bool 也可能是 0/1 数字，用 Value 兼容。
    #[serde(default)]
    pub disable: Option<serde_json::Value>,
}

impl RawUpstream {
    /// C++ 语义：`disable` 为 true 或非 0 数字即禁用。
    fn is_disabled(&self) -> bool {
        match &self.disable {
            None | Some(serde_json::Value::Null) => false,
            Some(serde_json::Value::Bool(b)) => *b,
            Some(serde_json::Value::Number(n)) => n.as_i64().map(|v| v != 0).unwrap_or(false),
            Some(serde_json::Value::String(s)) => {
                matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes")
            }
            Some(_) => false,
        }
    }
}

/// C++ 的 `internalBehavior`（整节在 C++ `ConfigLoader.cpp` 中从未被解析，属死配置）。
///
/// Rust 侧只把其中的 `connectTimeout` 作为顶层 `connectTimeout` 缺省时的兜底
/// （见 `from_value`）；其余字段仅保留定义，让旧配置"不丢字段"且能在启动时明确提示忽略，
/// 因此这里豁免 `dead_code`。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct InternalBehavior {
    #[serde(default)]
    pub connect_timeout: Option<u64>,
    #[serde(default)]
    pub socks_test_timeout: Option<u64>,
    #[serde(default)]
    pub moment_locale: Option<String>,
    #[serde(default)]
    pub statistical_timer_period: Option<u64>,
}

/// 客户端认证用户（SOCKS5 0x02 / M2 的 HTTP Basic）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct AuthUser {
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub pwd: String,
}

/// 健康检查原始配置（`healthCheck` 节，字段与 docs/07 ADR-002 对齐）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawHealthConfig {
    #[serde(default)]
    pub tcp: Option<RawPeriodicProbe>,
    #[serde(default)]
    pub connect: Option<RawPeriodicProbe>,
    #[serde(default)]
    pub target: Option<RawProbeTarget>,
    /// 失败即判 DOWN（默认 true；ADR-002 明确不做失败计数/重试）。
    #[serde(default)]
    pub fail_fast: Option<bool>,
    #[serde(default)]
    pub down_backoff: Option<RawBackoff>,
    #[serde(default)]
    pub probe_jitter: Option<f64>,
    /// readiness 探测并发上限（默认 4）。
    #[serde(default)]
    pub max_probe_concurrency: Option<usize>,
    /// 数据面 connect/握手失败是否立即把上游标为不可用（默认 true，ADR-002 #3）。
    /// 置 false 时数据面失败只记日志/`relayErrors`，不改变可用性（仅靠周期探测判定）。
    #[serde(default)]
    pub connect_fail_mark_degraded: Option<bool>,
}

/// 周期探测原始配置（tcp / connect 共用形状）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawPeriodicProbe {
    #[serde(default)]
    pub period: Option<u64>,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub start_delay: Option<u64>,
}

/// 探测目标（TLS + HTTP readiness 探测）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawProbeTarget {
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub sni: Option<String>,
    #[serde(default)]
    pub http_path: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub expect_status: Vec<u16>,
    /// 自定义信任根 PEM（企业自签 CA / 测试用）。
    #[serde(default)]
    pub ca_file: Option<String>,
}

/// DOWN 后的指数退避原始配置。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawBackoff {
    #[serde(default)]
    pub initial: Option<u64>,
    #[serde(default)]
    pub multiplier: Option<f64>,
    #[serde(default)]
    pub max: Option<u64>,
    #[serde(default)]
    pub jitter: Option<f64>,
}

/// TLS + HTTP readiness 探测有效配置（M1.5，ADR-006：对齐 C++ `ConnectTestHttps`）。
#[derive(Debug, Clone)]
pub struct TlsProbeConfig {
    /// 探测周期。
    pub period: Duration,
    /// 单次探测超时（含 SOCKS5 CONNECT + TLS 握手 + HTTP 响应）。
    pub timeout: Duration,
    /// 启动后首次探测延迟。
    pub start_delay: Duration,
    /// 探测目标 host（交给上游解析，socks5h）。
    pub host: String,
    /// 探测目标端口（默认 443）。
    pub port: u16,
    /// TLS SNI（默认同 host）。
    pub sni: String,
    /// HTTP 请求路径（默认 `/`）。
    pub http_path: String,
    /// User-Agent。
    pub user_agent: String,
    /// 期望状态码；**为空表示不校验**（只要完成一次 HTTP 往返即视为成功，对齐 C++
    /// `ConnectTestHttps` 的"只看能否完成往返、不看状态码"语义）。
    pub expect_status: Vec<u16>,
    /// 自定义信任根（PEM 文件）；None 表示使用操作系统根证书。
    pub ca_file: Option<std::path::PathBuf>,
    /// 探测并发上限（上游很多时避免同时打满）。
    pub max_concurrency: usize,
}

/// 健康检查有效配置（M1.4：TCP 探活；M1.5：+ TLS/HTTP readiness）。
#[derive(Debug, Clone)]
pub struct HealthConfig {
    /// 是否启用健康检查。`disableConnectTest=true` 时为 false：不探测、数据面失败也不降级，
    /// 完全按人工启停（C++ `disableConnectTest` 语义）。
    pub enabled: bool,
    /// 探活周期。
    pub period: Duration,
    /// 单次 TCP 探活超时。
    pub timeout: Duration,
    /// 启动后首次探测的延迟（错峰，避免同时打所有上游）。
    pub start_delay: Duration,
    /// DOWN 后首次重探延迟。
    pub backoff_initial: Duration,
    /// 退避倍数。
    pub backoff_multiplier: f64,
    /// 退避上限。
    pub backoff_max: Duration,
    /// 常规探测周期的随机抖动比例（0..=1）。
    pub probe_jitter: f64,
    /// 退避时长的随机抖动比例（0..=1）。
    pub backoff_jitter: f64,
    /// TLS + HTTP readiness 探测；None 表示只做 TCP liveness。
    pub tls: Option<TlsProbeConfig>,
    /// `sleepTime`（默认 1800000ms = 30min，对齐 C++）：**最近该时长内无任何新连接**
    /// 就进入空闲休眠，**暂停全部自动探测**（TCP 周期探测、readiness 周期探测、
    /// `additionCheckPeriod` 加速探测都停）。
    ///
    /// 目的（C++ README "Full configuration" + 作者说明）：长时间完全没人用时，周期探测
    /// 会在后端链路上留下**单调且有规律**的请求，容易被链路状态检测/防火墙判为异常通信
    /// 而拉黑后端。休眠期间只在**新连接到来**或**控制面 `check_upstream`** 时恢复探测。
    ///
    /// `None`/0 表示不启用休眠（始终周期探测）。
    pub sleep_time: Option<Duration>,
    /// `additionCheckPeriod`（默认 10000ms，对齐 C++）：**全池不可用**且不在
    /// `sleepTime` 休眠中时，按该周期额外强制探测全池，把恢复窗口从"一个探测周期"
    /// 压到"约一个 additionCheckPeriod"。`None`/0 表示不启用加速。
    pub addition_check_period: Option<Duration>,
    /// 数据面 connect/握手失败是否立即把上游标为不可用（默认 true，ADR-002 #3）。
    /// `connectFailMarkDegraded=false` 时数据面失败不影响可用性，只累加 `relayErrors`。
    pub connect_fail_mark_degraded: bool,
}

/// 连接监测原始配置（`connectionTracking` 节）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawConnectionTracking {
    /// 是否启用连接监测（聚合计数 + 明细注册）。
    #[serde(default)]
    pub enabled: Option<bool>,
    /// 是否登记每连接明细（默认 true，与 C++ 会话表行为接近）。
    #[serde(default)]
    pub detail: Option<bool>,
    /// 明细上限（达到后新连接只计数、不登记明细）。
    #[serde(default)]
    pub detail_limit: Option<usize>,
    /// `/connections/summary` 返回的客户端 Top N。
    #[serde(default)]
    pub client_top_limit: Option<usize>,
}

/// 连接监测有效配置。
#[derive(Debug, Clone)]
pub struct ConnTrackingConfig {
    /// 是否启用连接监测。
    pub enabled: bool,
    /// 是否登记每连接明细（关闭时只保留聚合计数）。
    pub detail: bool,
    /// 明细上限（达到后新连接只计数、不登记明细）。
    pub detail_limit: usize,
    /// 客户端维度 Top N（`/connections/summary`）。
    pub client_top_limit: usize,
}

impl Default for ConnTrackingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            detail: true,
            detail_limit: 10_000,
            client_top_limit: 200,
        }
    }
}

/// 原始配置：字段名与 C++ `config.json` 对齐。
///
/// 所有字段都会被 `from_value` 读取或映射（含旧字段兼容映射），因此不再需要
/// `allow(dead_code)`——一旦某个字段被误改成"只解析不生效"，编译器会立刻报出来。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawConfig {
    #[serde(default = "default_listen_host")]
    pub listen_host: String,
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
    #[serde(default)]
    pub multi_listen: Vec<ListenAddr>,

    #[serde(default)]
    pub upstream: Vec<RawUpstream>,

    #[serde(default = "default_retry_times")]
    pub retry_times: usize,
    /// 毫秒；缺省时回落到 `internalBehavior.connectTimeout`，再缺省 2000ms。
    #[serde(default)]
    pub connect_timeout: Option<u64>,
    /// 首包超时（毫秒）：下游连上后迟迟不发数据则关闭，防止"哑连接"泄漏。
    #[serde(default)]
    pub first_packet_timeout: Option<u64>,
    /// 协议握手超时（毫秒）：方法协商/认证/上游 SOCKS5 握手共用。
    #[serde(default)]
    pub handshake_timeout: Option<u64>,

    // ===== 线程（ADR-003）=====
    /// 最大 worker 线程数上限；0 = 自动（= 可用核数）。
    #[serde(default)]
    pub thread_num: usize,
    /// 精确指定 worker 线程数；0 = 不指定。
    #[serde(default)]
    pub worker_threads: usize,
    #[serde(default = "default_max_blocking_threads")]
    pub max_blocking_threads: usize,
    #[serde(default)]
    pub thread_stack_size: Option<usize>,

    // ===== 转发缓冲（ADR-005 / U1=方案 B）=====
    #[serde(default)]
    pub buffer_size: Option<usize>,

    // ===== 连接与平台 =====
    #[serde(default)]
    pub max_connections: Option<usize>,
    #[serde(default)]
    pub socket_recv_buffer: Option<usize>,
    #[serde(default)]
    pub socket_send_buffer: Option<usize>,
    #[serde(default)]
    pub log_level: Option<String>,

    // ===== 行为开关（部分 M1/M2 生效）=====
    /// 纯 TCP 透传（M0 唯一模式；M1 起为 false 时启用首包识别）。
    #[serde(default)]
    pub tradition_tcp_relay: bool,
    /// 禁用 SOCKS4/4a 入站。默认 **true**（对齐 C++ `ConfigLoader.cpp:242`：默认禁用，
    /// 避免陈旧的不正确实现客户端触发的异常请求）。
    #[serde(default = "default_true")]
    pub disable_socks4: bool,
    /// 禁用 HTTP 正向代理（absolute-form）；CONNECT 不受影响。默认 false。
    #[serde(default)]
    pub disable_http_forward: bool,

    // ===== 控制面（M3 生效，M0 仅兼容解析）=====
    #[serde(default)]
    pub state_server_host: Option<String>,
    #[serde(default)]
    pub state_server_port: Option<u16>,
    #[serde(default)]
    pub state_max_response_bytes: Option<usize>,

    // ===== 监控面（连接监测端点；可选独立监听）=====
    /// 监控端点独立监听的 host（缺省 127.0.0.1；仅当 monitorServerPort>0 时生效）。
    #[serde(default)]
    pub monitor_server_host: Option<String>,
    /// 监控端点独立监听端口；0/缺省 = 与控制面共用 stateServer 监听。
    #[serde(default)]
    pub monitor_server_port: Option<u16>,
    /// 连接监测配置（默认开启明细）。
    #[serde(default)]
    pub connection_tracking: Option<RawConnectionTracking>,

    // ===== 认证（ADR-004：SOCKS5 0x00/0x02）=====
    #[serde(default, alias = "AuthClientInfo")]
    pub auth_client_info: Vec<AuthUser>,

    // ===== 兼容性字段：解析但暂未使用（M2/M3 阶段）=====
    #[serde(default)]
    pub internal_behavior: Option<InternalBehavior>,
    #[serde(default, alias = "EmbedWebServerConfig")]
    pub embed_web_server_config: Option<serde_json::Value>,
    #[serde(default)]
    pub health_check: Option<RawHealthConfig>,
    #[serde(default)]
    pub upstream_select_rule: Option<String>,
    #[serde(default)]
    pub server_change_time: Option<u64>,
    /// 上游域名 DNS 刷新周期（毫秒）；0/缺省 = 不刷新（沿用启动期解析结果）。
    #[serde(default)]
    pub dns_ttl: Option<u64>,
    #[serde(default)]
    pub sleep_time: Option<u64>,
    #[serde(default)]
    pub disable_connect_test: bool,
    #[serde(default)]
    pub disable_connection_tracker: bool,
    #[serde(default)]
    pub tcp_check_period: Option<u64>,
    #[serde(default)]
    pub tcp_check_start: Option<u64>,
    #[serde(default)]
    pub connect_check_period: Option<u64>,
    #[serde(default)]
    pub connect_check_start: Option<u64>,
    #[serde(default)]
    pub addition_check_period: Option<u64>,
    #[serde(default)]
    pub test_remote_host: Option<String>,
    #[serde(default)]
    pub test_remote_port: Option<u16>,
}

/// 监听目标（已校验）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenTarget {
    pub host: String,
    pub port: u16,
}

impl std::fmt::Display for ListenTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

/// 上游（已校验）。
#[derive(Debug, Clone)]
pub struct UpstreamConfig {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub enabled: bool,
    pub auth_user: Option<String>,
    pub auth_pwd: Option<String>,
}

/// 线程配置（ADR-003）。
#[derive(Debug, Clone, Copy)]
pub struct ThreadConfig {
    /// 0 表示交给启动逻辑按 `threadNum`/核数推导。
    pub worker_threads: usize,
    pub thread_num: usize,
    pub max_blocking_threads: usize,
    pub stack_size: usize,
}

/// 有效配置（解析 + 校验后的结果）。
#[derive(Debug, Clone)]
pub struct Config {
    pub listen: Vec<ListenTarget>,
    pub upstreams: Vec<UpstreamConfig>,
    pub retry_times: usize,
    pub connect_timeout: Duration,
    pub buffer_size: usize,
    pub max_connections: usize,
    pub log_level: String,
    pub thread: ThreadConfig,
    pub socket_recv_buffer: usize,
    pub socket_send_buffer: usize,
    /// 客户端认证用户；为空表示不需要认证（SOCKS5 选 0x00）。
    pub auth: Vec<AuthUser>,
    /// 下游首包超时（防"哑连接"占用内存）。
    pub first_packet_timeout: Duration,
    /// 协议握手超时。
    pub handshake_timeout: Duration,
    /// 上游选择规则（M1.3）。
    pub select_rule: SelectRule,
    /// `change_by_time` 规则的切换周期。
    pub server_change_time: Duration,
    /// 上游域名 DNS 刷新周期；None 表示不刷新（沿用启动期解析结果）。
    pub dns_ttl: Option<Duration>,
    /// 健康检查配置（M1.4：TCP 探活 + fail-fast + DOWN 退避）。
    pub health: HealthConfig,
    pub tradition_tcp_relay: bool,
    pub disable_socks4: bool,
    /// 禁用 HTTP 正向代理（absolute-form）；CONNECT 不受影响。
    pub disable_http_forward: bool,
    /// 控制面监听地址（M3，ADR-001）；None 表示禁用。
    pub state_server: Option<ListenTarget>,
    /// 控制面单次响应字节上限（默认 65536）。
    pub state_max_response_bytes: usize,
    /// 监控面独立监听地址；None 表示监控端点与控制面共用监听。
    pub monitor_server: Option<ListenTarget>,
    /// 连接监测配置（默认开启明细）。
    pub conn_tracking: ConnTrackingConfig,
    /// 解析过程中发现的"被忽略/降级"的配置项说明，启动时以 warning 打印。
    pub notes: Vec<String>,
}

// ===== 字段兼容与拼写检查（ADR-002、docs/07 §5.2）=====

/// 顶层已知字段（与 `RawConfig` 的 serde 名称保持一致）。
const KNOWN_TOP_LEVEL_KEYS: &[&str] = &[
    "listenHost",
    "listenPort",
    "multiListen",
    "upstream",
    "retryTimes",
    "connectTimeout",
    "firstPacketTimeout",
    "handshakeTimeout",
    "threadNum",
    "workerThreads",
    "maxBlockingThreads",
    "threadStackSize",
    "bufferSize",
    "maxConnections",
    "socketRecvBuffer",
    "socketSendBuffer",
    "logLevel",
    "traditionTcpRelay",
    "disableSocks4",
    "disableHttpForward",
    "stateServerHost",
    "stateServerPort",
    "stateMaxResponseBytes",
    "monitorServerHost",
    "monitorServerPort",
    "connectionTracking",
    "authClientInfo",
    "AuthClientInfo",
    "internalBehavior",
    "embedWebServerConfig",
    "EmbedWebServerConfig",
    "healthCheck",
    "upstreamSelectRule",
    "serverChangeTime",
    "dnsTtl",
    "sleepTime",
    "disableConnectTest",
    "disableConnectionTracker",
    "tcpCheckPeriod",
    "tcpCheckStart",
    "connectCheckPeriod",
    "connectCheckStart",
    "additionCheckPeriod",
    "testRemoteHost",
    "testRemotePort",
];

/// `upstream[]` 条目的已知字段。
const KNOWN_UPSTREAM_KEYS: &[&str] = &[
    "name", "host", "port", "authUser", "authPwd", "slowImpl", "disable",
];
/// `healthCheck` 节的已知字段。
const KNOWN_HEALTH_KEYS: &[&str] = &[
    "tcp",
    "connect",
    "target",
    "failFast",
    "downBackoff",
    "probeJitter",
    "maxProbeConcurrency",
    "connectFailMarkDegraded",
];
/// `healthCheck.tcp` / `healthCheck.connect` 的已知字段。
const KNOWN_PERIODIC_KEYS: &[&str] = &["period", "timeout", "startDelay"];
/// `healthCheck.target` 的已知字段。
const KNOWN_TARGET_KEYS: &[&str] = &[
    "host",
    "port",
    "sni",
    "httpPath",
    "userAgent",
    "expectStatus",
    "caFile",
];
/// `healthCheck.downBackoff` 的已知字段。
const KNOWN_BACKOFF_KEYS: &[&str] = &["initial", "multiplier", "max", "jitter"];
/// `internalBehavior` 的已知字段（C++ 中除 connectTimeout 外均为死配置）。
const KNOWN_INTERNAL_KEYS: &[&str] = &[
    "connectTimeout",
    "socksTestTimeout",
    "momentLocale",
    "statisticalTimerPeriod",
];
/// `multiListen[]` 的已知字段。
const KNOWN_LISTEN_KEYS: &[&str] = &["host", "port"];
/// `authClientInfo[]` 的已知字段。
const KNOWN_AUTH_KEYS: &[&str] = &["user", "pwd"];
/// `connectionTracking` 节的已知字段。
const KNOWN_TRACKING_KEYS: &[&str] = &["enabled", "detail", "detailLimit", "clientTopLimit"];

/// 记录 `value` 对象中本版本不认识的字段（防止 `enabled`/`threadStackSizeKb` 之类拼写错误被静默忽略）。
///
/// serde 默认忽略未知字段；这里显式告警，避免"配置写了但完全不生效"的排查黑洞。
fn note_unknown_keys(
    value: &serde_json::Value,
    known: &[&str],
    path: &str,
    notes: &mut Vec<String>,
) {
    let Some(obj) = value.as_object() else {
        return;
    };
    let mut unknown: Vec<&str> = obj
        .keys()
        .map(String::as_str)
        .filter(|k| !known.contains(k))
        .collect();
    if unknown.is_empty() {
        return;
    }
    unknown.sort_unstable();
    notes.push(format!(
        "{path} 中的字段本版本不识别，已忽略（请检查拼写或对照文档）: {}",
        unknown.join(", ")
    ));
}

/// 扫描整份配置的未知字段（顶层 + 已知子结构）。
fn scan_unknown_keys(value: &serde_json::Value, notes: &mut Vec<String>) {
    note_unknown_keys(value, KNOWN_TOP_LEVEL_KEYS, "配置", notes);

    if let Some(arr) = value.get("upstream").and_then(serde_json::Value::as_array) {
        for (i, item) in arr.iter().enumerate() {
            note_unknown_keys(item, KNOWN_UPSTREAM_KEYS, &format!("upstream[{i}]"), notes);
        }
    }
    if let Some(arr) = value
        .get("multiListen")
        .and_then(serde_json::Value::as_array)
    {
        for (i, item) in arr.iter().enumerate() {
            note_unknown_keys(item, KNOWN_LISTEN_KEYS, &format!("multiListen[{i}]"), notes);
        }
    }
    for key in ["authClientInfo", "AuthClientInfo"] {
        if let Some(arr) = value.get(key).and_then(serde_json::Value::as_array) {
            for (i, item) in arr.iter().enumerate() {
                note_unknown_keys(item, KNOWN_AUTH_KEYS, &format!("{key}[{i}]"), notes);
            }
        }
    }
    if let Some(h) = value.get("healthCheck") {
        note_unknown_keys(h, KNOWN_HEALTH_KEYS, "healthCheck", notes);
        for (key, known) in [
            ("tcp", KNOWN_PERIODIC_KEYS),
            ("connect", KNOWN_PERIODIC_KEYS),
            ("target", KNOWN_TARGET_KEYS),
            ("downBackoff", KNOWN_BACKOFF_KEYS),
        ] {
            if let Some(sub) = h.get(key) {
                note_unknown_keys(sub, known, &format!("healthCheck.{key}"), notes);
            }
        }
    }
    if let Some(ib) = value.get("internalBehavior") {
        note_unknown_keys(ib, KNOWN_INTERNAL_KEYS, "internalBehavior", notes);
    }
    if let Some(t) = value.get("connectionTracking") {
        note_unknown_keys(t, KNOWN_TRACKING_KEYS, "connectionTracking", notes);
    }
}

/// 把 C++ 旧探测字段映射进 `healthCheck`（ADR-002；docs/07 §5.2 兼容映射表）。
///
/// 规则：**新字段优先**；旧字段只在对应新字段缺省时生效；两者都配置时打印提示。
fn legacy_health_config(raw: &RawConfig, notes: &mut Vec<String>) -> RawHealthConfig {
    let mut h = raw.health_check.clone().unwrap_or_default();
    let mut mapped: Vec<&str> = Vec::new();

    // tcpCheckPeriod / tcpCheckStart → healthCheck.tcp.{period,startDelay}
    if raw.tcp_check_period.is_some() || raw.tcp_check_start.is_some() {
        let tcp = h.tcp.get_or_insert_with(RawPeriodicProbe::default);
        if let Some(v) = raw.tcp_check_period.filter(|v| *v > 0) {
            if tcp.period.is_none() {
                tcp.period = Some(v);
                mapped.push("tcpCheckPeriod");
            } else {
                notes.push("tcpCheckPeriod 与 healthCheck.tcp.period 同时配置，新字段优先".into());
            }
        }
        if let Some(v) = raw.tcp_check_start {
            if tcp.start_delay.is_none() {
                tcp.start_delay = Some(v);
                mapped.push("tcpCheckStart");
            } else {
                notes.push(
                    "tcpCheckStart 与 healthCheck.tcp.startDelay 同时配置，新字段优先".into(),
                );
            }
        }
    }

    // connectCheckPeriod / connectCheckStart → healthCheck.connect.{period,startDelay}
    if raw.connect_check_period.is_some() || raw.connect_check_start.is_some() {
        let connect = h.connect.get_or_insert_with(RawPeriodicProbe::default);
        if let Some(v) = raw.connect_check_period.filter(|v| *v > 0) {
            if connect.period.is_none() {
                connect.period = Some(v);
                mapped.push("connectCheckPeriod");
            } else {
                notes.push(
                    "connectCheckPeriod 与 healthCheck.connect.period 同时配置，新字段优先".into(),
                );
            }
        }
        if let Some(v) = raw.connect_check_start {
            if connect.start_delay.is_none() {
                connect.start_delay = Some(v);
                mapped.push("connectCheckStart");
            } else {
                notes.push(
                    "connectCheckStart 与 healthCheck.connect.startDelay 同时配置，新字段优先"
                        .into(),
                );
            }
        }
    }

    // testRemoteHost / testRemotePort → healthCheck.target.{host,port}
    if raw.test_remote_host.is_some() || raw.test_remote_port.is_some() {
        let target = h.target.get_or_insert_with(RawProbeTarget::default);
        if let Some(host) = raw
            .test_remote_host
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            let has_new = target
                .host
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_some();
            if has_new {
                notes.push("testRemoteHost 与 healthCheck.target.host 同时配置，新字段优先".into());
            } else {
                target.host = Some(host.trim().to_string());
                mapped.push("testRemoteHost");
            }
        }
        if let Some(port) = raw.test_remote_port.filter(|v| *v > 0) {
            if target.port.is_none() {
                target.port = Some(port);
                mapped.push("testRemotePort");
            } else {
                notes.push("testRemotePort 与 healthCheck.target.port 同时配置，新字段优先".into());
            }
        }
    }

    if !mapped.is_empty() {
        notes.push(format!(
            "已按旧版字段映射到 healthCheck: {}（建议改用 healthCheck.* 新字段）",
            mapped.join(", ")
        ));
    }
    h
}

/// 从文件加载配置。
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let bytes = fs::read(path)?;
    let text = String::from_utf8(bytes)?;
    load_str(&text)
}

/// 从字符串加载配置（便于单测）。
///
/// 容忍 UTF-8 BOM（项目规范是无 BOM，但为了能用编辑器直接保存的旧文件，这里宽容处理）。
pub fn load_str(text: &str) -> Result<Config, ConfigError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let value: serde_json::Value = serde_json::from_str(text)?;
    from_value(value)
}

/// 从 JSON Value 构建有效配置。
pub fn from_value(value: serde_json::Value) -> Result<Config, ConfigError> {
    let mut notes = Vec::new();

    // 先做"未知字段"告警（serde 会静默忽略；拼错的字段往往意味着配置完全不生效）。
    scan_unknown_keys(&value, &mut notes);

    // 先做"被忽略配置"的探测（serde 会静默忽略未知字段，这里显式告知运维）。
    if value.get("internalBehavior").is_some() {
        notes.push(
            "internalBehavior 整节在 C++ 中未被解析；M0 仅用其 connectTimeout 作兜底，其余忽略"
                .into(),
        );
    }
    if value.get("AuthClientInfo").is_some() || value.get("authClientInfo").is_some() {
        notes
            .push("AuthClientInfo 已读取（非空时启用 SOCKS5 0x02 用户名密码认证，ADR-004）".into());
    }
    if value.get("EmbedWebServerConfig").is_some() || value.get("embedWebServerConfig").is_some() {
        notes.push("EmbedWebServerConfig 已解析但 Rust 版不实现内嵌 Web 服务器（ADR-001）".into());
    }
    if value.get("upstreamSelectRule").is_some() || value.get("serverChangeTime").is_some() {
        notes.push("upstreamSelectRule/serverChangeTime 已启用（M1.3）".into());
    }

    let raw: RawConfig = serde_json::from_value(value)?;

    // ===== 监听 =====
    let mut listen = Vec::new();
    let push_listen = |host: &str, port: u16, listen: &mut Vec<ListenTarget>| {
        let host = host.trim();
        if host.is_empty() {
            return;
        }
        let target = ListenTarget {
            host: host.to_string(),
            port,
        };
        if !listen.contains(&target) {
            listen.push(target);
        }
    };
    if raw.listen_port == 0 {
        return Err(err("listenPort 不能为 0"));
    }
    push_listen(&raw.listen_host, raw.listen_port, &mut listen);
    for m in &raw.multi_listen {
        if m.port == 0 {
            return Err(err("multiListen[].port 不能为 0"));
        }
        let host = if m.host.trim().is_empty() {
            raw.listen_host.as_str()
        } else {
            m.host.as_str()
        };
        push_listen(host, m.port, &mut listen);
    }
    if listen.is_empty() {
        return Err(err("没有任何有效监听地址"));
    }

    // ===== 上游 =====
    let mut upstreams = Vec::with_capacity(raw.upstream.len());
    for (idx, u) in raw.upstream.iter().enumerate() {
        if u.host.trim().is_empty() {
            return Err(err(format!("upstream[{idx}].host 为空")));
        }
        if u.port == 0 {
            return Err(err(format!("upstream[{idx}].port 为 0")));
        }
        upstreams.push(UpstreamConfig {
            name: u
                .name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| format!("upstream-{idx}")),
            host: u.host.trim().to_string(),
            port: u.port,
            enabled: !u.is_disabled(),
            auth_user: u.auth_user.clone(),
            auth_pwd: u.auth_pwd.clone(),
        });
        if u.slow_impl {
            notes.push(format!(
                "upstream[{idx}] slowImpl=true：M0 纯透传不涉及（M2 握手兼容模式生效）"
            ));
        }
    }
    if upstreams.is_empty() {
        return Err(err("upstream 列表为空，至少需要一个上游"));
    }
    if !upstreams.iter().any(|u| u.enabled) {
        // 允许以"全禁用"状态启动（对齐 docs/13 §5.2：上游保留在池中 enabled=false，
        // 运维可经控制面 `enable_upstream` 启用），不再直接启动失败。
        // 该状态下新连接一律拒绝（SOCKS5 REP 0x01 / HTTP 502），不会黑洞。
        notes.push(
            "所有上游都被禁用（disable=true）：服务正常启动但拒绝新连接，请用控制面 enable_upstream 启用"
                .into(),
        );
    }
    if let Some(u) = upstreams
        .iter()
        .find(|u| u.auth_user.is_some() || u.auth_pwd.is_some())
    {
        notes.push(format!(
            "upstream[{}] 配置了 SOCKS5 认证：将在上游握手时使用（0x02）",
            u.name
        ));
    }

    // ===== 缓冲（ADR-005 / 方案 B）=====
    let buffer_size = raw.buffer_size.unwrap_or(DEFAULT_BUFFER_SIZE);
    if !ALLOWED_BUFFER_SIZES.contains(&buffer_size) {
        return Err(err(format!(
            "bufferSize={buffer_size} 非法，允许值: {ALLOWED_BUFFER_SIZES:?}"
        )));
    }

    // ===== 超时 =====
    let connect_timeout_ms = raw
        .connect_timeout
        .or_else(|| {
            raw.internal_behavior
                .as_ref()
                .and_then(|i| i.connect_timeout)
        })
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_CONNECT_TIMEOUT_MS);
    if connect_timeout_ms > 300_000 {
        return Err(err("connectTimeout 过大（>300000ms）"));
    }
    let first_packet_timeout_ms = raw
        .first_packet_timeout
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_FIRST_PACKET_TIMEOUT_MS);
    if first_packet_timeout_ms > 600_000 {
        return Err(err("firstPacketTimeout 过大（>600000ms）"));
    }
    let handshake_timeout_ms = raw
        .handshake_timeout
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_HANDSHAKE_TIMEOUT_MS);
    if handshake_timeout_ms > 600_000 {
        return Err(err("handshakeTimeout 过大（>600000ms）"));
    }

    // ===== 上游选择规则（M1.3）=====
    let select_rule = match raw.upstream_select_rule.as_deref() {
        None => SelectRule::Random,
        Some(s) => {
            if s.trim().eq_ignore_ascii_case("inherit") {
                notes.push(
                    "upstreamSelectRule=\"inherit\"：Rust 无\"继承上层规则\"语义，按 random 处理；按维度强制上游请用控制面 set_rule(scope=client|listen)"
                        .into(),
                );
            }
            SelectRule::parse(s).ok_or_else(|| {
                err(format!(
                    "upstreamSelectRule={s:?} 非法，允许值: loop/random/one_by_one/change_by_time/force_only_one/inherit"
                ))
            })?
        }
    };
    let server_change_time =
        Duration::from_millis(raw.server_change_time.filter(|v| *v > 0).unwrap_or(60_000));
    let dns_ttl = match raw.dns_ttl.filter(|v| *v > 0) {
        None => None,
        Some(ms) if ms < 1_000 => {
            return Err(err("dnsTtl 不能小于 1000ms（0 表示不刷新）"));
        }
        Some(ms) if ms > 3_600_000 => {
            return Err(err("dnsTtl 过大（>3600000ms）"));
        }
        Some(ms) => Some(Duration::from_millis(ms)),
    };

    // ===== 健康检查（ADR-002：fail-fast，不重试/不防抖）=====
    // 旧版探测字段先按 ADR 映射进 healthCheck（新字段优先），再构建有效配置。
    let health_raw = legacy_health_config(&raw, &mut notes);
    if health_raw.connect.is_some() || health_raw.target.is_some() {
        notes.push("healthCheck.connect/target 已启用：TLS + HTTP readiness 探测（M1.5）".into());
    }
    let health = build_health_config(&health_raw, raw.disable_connect_test)?;

    // ===== HTTP 正向代理开关 =====
    if raw.disable_http_forward {
        if raw.tradition_tcp_relay {
            notes.push("traditionTcpRelay=true 时 disableHttpForward 无效果（不解析协议）".into());
        } else {
            notes.push(
                "disableHttpForward=true：HTTP absolute-form 正向代理将回 501（CONNECT 仍可用）"
                    .into(),
            );
        }
    }

    // ===== sleepTime / additionCheckPeriod（对齐 C++ README "Full configuration"）=====
    // C++ 默认：sleepTime=1800000ms(30min)、additionCheckPeriod=10000ms。
    //
    // sleepTime 不是"省流量"这么简单：长时间完全没人用时，5s 一轮的周期探测会在后端链路
    // 上留下单调、有规律的请求，容易被链路状态检测/防火墙视为异常通信并把后端拉黑。
    // 因此这里对齐 C++：进入休眠后**全部自动探测都暂停**（TCP、readiness、addition 加速
    // 探测），只有"新连接到来"或控制面 `check_upstream` 才恢复。
    // 显式写 `"sleepTime": 0` 可关闭休眠（始终周期探测）。
    let mut health = health;
    if health.enabled {
        let sleep_ms = raw.sleep_time.unwrap_or(DEFAULT_SLEEP_TIME_MS);
        if sleep_ms > 86_400_000 {
            return Err(err("sleepTime 过大（>86400000ms）"));
        }
        if sleep_ms > 0 {
            health.sleep_time = Some(Duration::from_millis(sleep_ms));
            notes.push(format!(
                "sleepTime={sleep_ms}ms：最近该时长内无新连接则暂停全部自动探测（新连接或 check_upstream 恢复；显式 0 关闭）"
            ));
        } else {
            notes.push("sleepTime=0：不启用空闲休眠，始终周期探测".into());
        }

        let addition_ms = raw
            .addition_check_period
            .unwrap_or(DEFAULT_ADDITION_CHECK_PERIOD_MS);
        if addition_ms > 0 {
            if addition_ms < 1_000 {
                return Err(err("additionCheckPeriod 不能小于 1000ms"));
            }
            if addition_ms > 3_600_000 {
                return Err(err("additionCheckPeriod 过大（>3600000ms）"));
            }
            health.addition_check_period = Some(Duration::from_millis(addition_ms));
            notes.push(format!(
                "additionCheckPeriod={addition_ms}ms：全池不可用时加速探测（休眠中不启用）"
            ));
        } else {
            notes.push("additionCheckPeriod=0：全池不可用时不加速探测".into());
        }
    }

    if !health.connect_fail_mark_degraded {
        notes.push(
            "healthCheck.connectFailMarkDegraded=false：数据面失败只计数，不立即标记上游不可用"
                .into(),
        );
    }

    // ===== 客户端认证（ADR-004）=====
    let mut auth: Vec<AuthUser> = Vec::new();
    for (idx, u) in raw.auth_client_info.iter().enumerate() {
        if u.user.is_empty() {
            // 与 C++ 一致：跳过空用户名条目
            notes.push(format!("AuthClientInfo[{idx}] 用户名为空，已忽略"));
            continue;
        }
        if u.pwd.is_empty() {
            notes.push(format!(
                "AuthClientInfo[{}] 密码为空（等价于只校验用户名）",
                u.user
            ));
        }
        auth.push(u.clone());
    }
    if !auth.is_empty() {
        notes.push(format!(
            "已启用 SOCKS5 用户名密码认证（0x02），用户数 {}",
            auth.len()
        ));
    }

    // ===== 线程（ADR-003）=====
    if raw.worker_threads > MAX_WORKER_THREADS {
        return Err(err(format!(
            "workerThreads={} 超过上限 {MAX_WORKER_THREADS}",
            raw.worker_threads
        )));
    }
    if raw.max_blocking_threads == 0 {
        return Err(err("maxBlockingThreads 不能为 0"));
    }
    if raw.max_blocking_threads > 64 {
        return Err(err("maxBlockingThreads 过大（>64）"));
    }
    let stack_size = raw.thread_stack_size.unwrap_or(DEFAULT_THREAD_STACK_SIZE);
    if !(64 * 1024..=64 * 1024 * 1024).contains(&stack_size) {
        return Err(err(format!(
            "threadStackSize={stack_size} 超出允许范围 [65536, 67108864]"
        )));
    }

    // ===== 连接数上限 =====
    let max_connections = raw.max_connections.unwrap_or(DEFAULT_MAX_CONNECTIONS);
    if max_connections == 0 {
        return Err(err("maxConnections 不能为 0"));
    }

    // ===== socket 缓冲（0 = 使用系统默认）=====
    let socket_recv_buffer = raw.socket_recv_buffer.unwrap_or(0);
    let socket_send_buffer = raw.socket_send_buffer.unwrap_or(0);

    // ===== 控制面（M3，ADR-001：4 端点、无鉴权、默认仅回环）=====
    // 默认端口与 C++ 版一致（5110）；显式配置 0 表示禁用。
    let state_port = raw.state_server_port.unwrap_or(5110);
    let state_server = if state_port > 0 {
        Some(ListenTarget {
            host: raw
                .state_server_host
                .clone()
                .filter(|h| !h.trim().is_empty())
                .unwrap_or_else(|| "127.0.0.1".to_string()),
            port: state_port,
        })
    } else {
        None
    };
    if let Some(state) = &state_server
        && !is_loopback_host(&state.host)
    {
        notes.push(format!(
            "控制面无鉴权且监听非回环地址 {}：存在被未授权访问的风险（ADR-001）",
            state
        ));
    }
    let state_max_response_bytes = raw
        .state_max_response_bytes
        .filter(|v| *v > 0)
        .unwrap_or(65_536)
        .min(1024 * 1024);

    // ===== 监控面（可选独立监听；缺省与控制面共用）=====
    let monitor_server = match raw.monitor_server_port.filter(|p| *p > 0) {
        None => None,
        Some(port) => {
            let bind = ListenTarget {
                host: raw
                    .monitor_server_host
                    .clone()
                    .filter(|h| !h.trim().is_empty())
                    .unwrap_or_else(|| "127.0.0.1".to_string()),
                port,
            };
            if !is_loopback_host(&bind.host) {
                notes.push(format!(
                    "监控面无鉴权且监听非回环地址 {}：会暴露客户端 IP/连接明细，建议仅回环",
                    bind
                ));
            }
            if state_server.as_ref() == Some(&bind) {
                notes.push("monitorServerHost/Port 与控制面相同：监控端点复用控制面监听".into());
                None
            } else {
                Some(bind)
            }
        }
    };

    // ===== 连接监测（默认开启明细，与 C++ 会话表行为接近）=====
    let tracking_raw = raw.connection_tracking.clone().unwrap_or_default();
    let mut conn_tracking = ConnTrackingConfig {
        detail_limit: max_connections.max(1),
        ..ConnTrackingConfig::default()
    };
    // 旧字段 `disableConnectionTracker`（C++：关闭连接分析）映射到 `connectionTracking.enabled`。
    // 优先级：新字段 `connectionTracking.enabled` > 旧字段 > 默认（启用）。
    if let Some(v) = tracking_raw.enabled {
        conn_tracking.enabled = v;
    } else if raw.disable_connection_tracker {
        conn_tracking.enabled = false;
        notes.push(
            "disableConnectionTracker=true：已映射为 connectionTracking.enabled=false（不记录连接监测）"
                .into(),
        );
    }
    if let Some(v) = tracking_raw.detail {
        conn_tracking.detail = v;
    }
    if let Some(v) = tracking_raw.detail_limit.filter(|v| *v > 0) {
        if v > 1_000_000 {
            return Err(err("connectionTracking.detailLimit 过大（>1000000）"));
        }
        conn_tracking.detail_limit = v;
    }
    if let Some(v) = tracking_raw.client_top_limit.filter(|v| *v > 0) {
        conn_tracking.client_top_limit = v.min(10_000);
    }
    if !conn_tracking.enabled {
        notes
            .push("connectionTracking.enabled=false：不记录连接监测（/connections 不可用）".into());
    } else if !conn_tracking.detail {
        notes.push(
            "connectionTracking.detail=false：只保留三维度计数，/connections 明细为空".into(),
        );
    } else {
        notes.push(format!(
            "连接监测：明细开启（上限 {}，客户端 Top {}）",
            conn_tracking.detail_limit, conn_tracking.client_top_limit
        ));
    }

    Ok(Config {
        listen,
        upstreams,
        retry_times: raw.retry_times.max(1),
        connect_timeout: Duration::from_millis(connect_timeout_ms),
        first_packet_timeout: Duration::from_millis(first_packet_timeout_ms),
        handshake_timeout: Duration::from_millis(handshake_timeout_ms),
        select_rule,
        server_change_time,
        dns_ttl,
        health,
        auth,
        buffer_size,
        max_connections,
        log_level: raw.log_level.unwrap_or_else(default_log_level),
        thread: ThreadConfig {
            worker_threads: raw.worker_threads,
            thread_num: raw.thread_num,
            max_blocking_threads: raw.max_blocking_threads,
            stack_size,
        },
        socket_recv_buffer,
        socket_send_buffer,
        tradition_tcp_relay: raw.tradition_tcp_relay,
        disable_socks4: raw.disable_socks4,
        disable_http_forward: raw.disable_http_forward,
        state_server,
        state_max_response_bytes,
        monitor_server,
        conn_tracking,
        notes,
    })
}

/// 计算实际 worker 线程数（ADR-003）。
///
/// 规则：`workerThreads > 0` 时精确使用；否则 `threadNum > 0` 时取 `min(核数, threadNum)`；
/// 否则等于核数（最少 1）。
pub fn effective_worker_threads(thread: &ThreadConfig) -> usize {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .max(1);
    let n = if thread.worker_threads > 0 {
        thread.worker_threads
    } else if thread.thread_num > 0 {
        cores.min(thread.thread_num)
    } else {
        cores
    };
    n.clamp(1, MAX_WORKER_THREADS)
}

/// 构建健康检查有效配置（M1.4：TCP 探活；M1.5：+ TLS/HTTP readiness）。
fn build_health_config(
    raw: &RawHealthConfig,
    disable_connect_test: bool,
) -> Result<HealthConfig, ConfigError> {
    const DEFAULT_PERIOD_MS: u64 = 5_000;
    const DEFAULT_TIMEOUT_MS: u64 = 2_000;
    const DEFAULT_START_DELAY_MS: u64 = 1_000;
    const DEFAULT_BACKOFF_INITIAL_MS: u64 = 30_000;
    const DEFAULT_BACKOFF_MAX_MS: u64 = 600_000;
    const DEFAULT_MULTIPLIER: f64 = 2.0;
    const DEFAULT_JITTER: f64 = 0.2;
    // C++ 默认 ConnectTest 周期 300000ms、超时 11000ms
    const DEFAULT_TLS_PERIOD_MS: u64 = 300_000;
    const DEFAULT_TLS_TIMEOUT_MS: u64 = 11_000;

    // C++ 语义（docs/03 §7.5、docs/07 ADR-002 #5）：`disableConnectTest=true` 表示
    // 完全跳过健康检查——不探测、数据面失败也不降级，完全按人工启停。
    if disable_connect_test {
        return Ok(HealthConfig {
            enabled: false,
            period: Duration::from_millis(DEFAULT_PERIOD_MS),
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            start_delay: Duration::from_millis(DEFAULT_START_DELAY_MS),
            backoff_initial: Duration::from_millis(DEFAULT_BACKOFF_INITIAL_MS),
            backoff_multiplier: DEFAULT_MULTIPLIER,
            backoff_max: Duration::from_millis(DEFAULT_BACKOFF_MAX_MS),
            probe_jitter: DEFAULT_JITTER,
            backoff_jitter: DEFAULT_JITTER,
            tls: None,
            sleep_time: None,
            addition_check_period: None,
            // 禁用探测时不使用降级开关（`mark_degraded` 先判 enabled）
            connect_fail_mark_degraded: true,
        });
    }

    // fail-fast 是 ADR-002 的既定策略，不接受关闭
    if raw.fail_fast == Some(false) {
        return Err(err(
            "healthCheck.failFast=false 不受支持：健康检查采用 fail-fast（ADR-002）",
        ));
    }

    let tcp = raw.tcp.clone().unwrap_or_default();
    let period_ms = tcp.period.filter(|v| *v > 0).unwrap_or(DEFAULT_PERIOD_MS);
    let timeout_ms = tcp.timeout.filter(|v| *v > 0).unwrap_or(DEFAULT_TIMEOUT_MS);
    let start_delay_ms = tcp.start_delay.unwrap_or(DEFAULT_START_DELAY_MS);
    // 首次探测延迟（C++ `tcpCheckStart`）：给"后端与本程序同时启动、后端起得更慢"留出窗口。
    if start_delay_ms > 3_600_000 {
        return Err(err("healthCheck.tcp.startDelay 过大（>3600000ms）"));
    }
    if period_ms < 100 {
        return Err(err("healthCheck.tcp.period 不能小于 100ms"));
    }
    if timeout_ms > 60_000 {
        return Err(err("healthCheck.tcp.timeout 过大（>60000ms）"));
    }
    if timeout_ms >= period_ms {
        return Err(err("healthCheck.tcp.timeout 必须小于 period"));
    }

    let backoff = raw.down_backoff.clone().unwrap_or_default();
    let initial_ms = backoff
        .initial
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_BACKOFF_INITIAL_MS);
    let max_ms = backoff
        .max
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_BACKOFF_MAX_MS);
    if initial_ms > max_ms {
        return Err(err("healthCheck.downBackoff.initial 不能大于 max"));
    }
    let multiplier = backoff.multiplier.unwrap_or(DEFAULT_MULTIPLIER);
    if !(1.0..=10.0).contains(&multiplier) {
        return Err(err("healthCheck.downBackoff.multiplier 必须在 [1, 10]"));
    }
    let jitter = raw.probe_jitter.unwrap_or(DEFAULT_JITTER);
    if !(0.0..=1.0).contains(&jitter) {
        return Err(err("healthCheck.probeJitter 必须在 [0, 1]"));
    }
    let backoff_jitter = backoff.jitter.unwrap_or(DEFAULT_JITTER);
    if !(0.0..=1.0).contains(&backoff_jitter) {
        return Err(err("healthCheck.downBackoff.jitter 必须在 [0, 1]"));
    }

    // ===== TLS + HTTP readiness（M1.5，ADR-006）=====
    let tls = if disable_connect_test {
        None
    } else if raw.connect.is_some() || raw.target.is_some() {
        let connect = raw.connect.clone().unwrap_or_default();
        let target = raw.target.clone().unwrap_or_default();
        let host = target
            .host
            .clone()
            .filter(|h| !h.trim().is_empty())
            .ok_or_else(|| {
                err("healthCheck.connect 需要同时配置 healthCheck.target.host（ADR-006）")
            })?;
        let port = target.port.unwrap_or(443);
        if port == 0 {
            return Err(err("healthCheck.target.port 不能为 0"));
        }
        let http_period_ms = connect
            .period
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_TLS_PERIOD_MS);
        let http_timeout_ms = connect
            .timeout
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_TLS_TIMEOUT_MS);
        let connect_start_delay_ms = connect.start_delay.unwrap_or(DEFAULT_START_DELAY_MS);
        if connect_start_delay_ms > 3_600_000 {
            return Err(err("healthCheck.connect.startDelay 过大（>3600000ms）"));
        }
        if http_period_ms < 1_000 {
            return Err(err("healthCheck.connect.period 不能小于 1000ms"));
        }
        if http_timeout_ms > 120_000 {
            return Err(err("healthCheck.connect.timeout 过大（>120000ms）"));
        }
        let max_concurrency = raw.max_probe_concurrency.filter(|v| *v > 0).unwrap_or(4);
        Some(TlsProbeConfig {
            period: Duration::from_millis(http_period_ms),
            timeout: Duration::from_millis(http_timeout_ms),
            start_delay: Duration::from_millis(connect_start_delay_ms),
            host,
            port,
            sni: target
                .sni
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| target.host.clone().unwrap_or_default()),
            http_path: target
                .http_path
                .clone()
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| "/".to_string()),
            user_agent: target
                .user_agent
                .clone()
                .unwrap_or_else(|| format!("Socks5BalancerRust/{}", env!("CARGO_PKG_VERSION"))),
            expect_status: target.expect_status.clone(),
            ca_file: target
                .ca_file
                .clone()
                .filter(|p| !p.trim().is_empty())
                .map(std::path::PathBuf::from),
            max_concurrency,
        })
    } else {
        None
    };

    Ok(HealthConfig {
        enabled: true,
        period: Duration::from_millis(period_ms),
        timeout: Duration::from_millis(timeout_ms),
        start_delay: Duration::from_millis(start_delay_ms),
        backoff_initial: Duration::from_millis(initial_ms),
        backoff_multiplier: multiplier,
        backoff_max: Duration::from_millis(max_ms),
        probe_jitter: jitter,
        backoff_jitter,
        tls,
        // 由 `from_value` 根据 RawConfig 的旧字段填充
        sleep_time: None,
        addition_check_period: None,
        connect_fail_mark_degraded: raw.connect_fail_mark_degraded.unwrap_or(true),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> &'static str {
        r#"{
            "listenHost": "127.0.0.1",
            "listenPort": 5100,
            "upstream": [ { "host": "127.0.0.1", "port": 6100 } ]
        }"#
    }

    #[test]
    fn 最小配置使用默认值() {
        let c = load_str(minimal()).unwrap();
        assert_eq!(c.listen.len(), 1);
        assert_eq!(c.listen[0].port, 5100);
        assert_eq!(c.buffer_size, DEFAULT_BUFFER_SIZE);
        assert_eq!(c.connect_timeout, Duration::from_millis(2000));
        assert_eq!(c.max_connections, DEFAULT_MAX_CONNECTIONS);
        assert_eq!(c.thread.max_blocking_threads, DEFAULT_MAX_BLOCKING_THREADS);
        assert_eq!(c.thread.stack_size, DEFAULT_THREAD_STACK_SIZE);
        assert!(c.upstreams[0].enabled);
    }

    #[test]
    fn 兼容_cpp_完整配置() {
        // 取自 C++ 仓库 config.json（含首字母大写的键、数字 disable、internalBehavior）
        let text = r#"{
            "listenHost": "127.0.0.1",
            "listenPort": 5100,
            "retryTimes": 3,
            "testRemoteHost": "www.google.com",
            "testRemotePort": 443,
            "tcpCheckPeriod": 5000,
            "tcpCheckStart": 1000,
            "connectCheckPeriod": 300000,
            "connectCheckStart": 1000,
            "additionCheckPeriod": 10000,
            "upstreamSelectRule": "loop",
            "sleepTime": 1800000,
            "threadNum": 11,
            "serverChangeTime": 5000,
            "stateServerHost": "127.0.0.1",
            "stateServerPort": 5110,
            "disableConnectTest": false,
            "disableConnectionTracker": false,
            "traditionTcpRelay": false,
            "disableSocks4": false,
            "multiListen": [
                { "host": "127.0.0.1", "port": 6600 },
                { "host": "127.0.0.1", "port": 6600 }
            ],
            "upstream": [
                { "name": "serverTest", "host": "127.0.0.1", "port": 6100, "disable": 1 },
                { "name": "serverA", "host": "127.0.0.1", "port": 26702 }
            ],
            "AuthClientInfo": [ { "user": "123", "pwd": "321" } ],
            "internalBehavior": { "connectTimeout": 1234 },
            "EmbedWebServerConfig": { "enable": true, "host": "127.0.0.1", "port": 5102 }
        }"#;
        let c = load_str(text).unwrap();
        // 去重后 2 个监听
        assert_eq!(c.listen.len(), 2);
        // 数字 disable=1 视为禁用
        assert!(!c.upstreams[0].enabled);
        assert!(c.upstreams[1].enabled);
        // 顶层无 connectTimeout，回落 internalBehavior
        assert_eq!(c.connect_timeout, Duration::from_millis(1234));
        // 被忽略项有提示
        assert!(c.notes.iter().any(|n| n.contains("AuthClientInfo")));
        assert!(c.notes.iter().any(|n| n.contains("EmbedWebServer")));
    }

    #[test]
    fn buffer_size_只允许三档() {
        for ok in [2048, 4096, 8192] {
            let text = format!(
                r#"{{"listenPort":5000,"bufferSize":{ok},"upstream":[{{"host":"1.2.3.4","port":1}}]}}"#
            );
            assert!(load_str(&text).is_ok(), "bufferSize={ok} 应被接受");
        }
        let text =
            r#"{"listenPort":5000,"bufferSize":3000,"upstream":[{"host":"1.2.3.4","port":1}]}"#;
        let e = load_str(text).unwrap_err();
        assert!(e.to_string().contains("bufferSize"));
    }

    #[test]
    fn disable_支持布尔与数字() {
        let text = r#"{"listenPort":5000,"upstream":[
            {"host":"a","port":1,"disable":true},
            {"host":"b","port":2,"disable":1},
            {"host":"c","port":3,"disable":0},
            {"host":"d","port":4}
        ]}"#;
        let c = load_str(text).unwrap();
        assert!(!c.upstreams[0].enabled);
        assert!(!c.upstreams[1].enabled);
        assert!(c.upstreams[2].enabled);
        assert!(c.upstreams[3].enabled);
    }

    #[test]
    fn 全部上游禁用_允许启动并提示() {
        let text = r#"{"listenPort":5000,"upstream":[{"host":"a","port":1,"disable":1}]}"#;
        // 对齐 docs/13 §5.2：全禁用不再启动失败，而是保留在池中（enabled=false），
        // 由控制面 enable_upstream 启用；运行期新连接会被拒绝。
        let c = load_str(text).unwrap();
        assert_eq!(c.upstreams.len(), 1);
        assert!(!c.upstreams[0].enabled);
        assert!(
            c.notes.iter().any(|n| n.contains("enable_upstream")),
            "应提示用控制面启用: {:?}",
            c.notes
        );
    }

    #[test]
    fn 空上游_报错() {
        let text = r#"{"listenPort":5000}"#;
        assert!(load_str(text).is_err());
    }

    #[test]
    fn 线程数量_校验() {
        let text = r#"{"listenPort":5000,"workerThreads":999,"upstream":[{"host":"a","port":1}]}"#;
        assert!(load_str(text).is_err());
        let text =
            r#"{"listenPort":5000,"maxBlockingThreads":0,"upstream":[{"host":"a","port":1}]}"#;
        assert!(load_str(text).is_err());
        let text =
            r#"{"listenPort":5000,"threadStackSize":1024,"upstream":[{"host":"a","port":1}]}"#;
        assert!(load_str(text).is_err());
    }

    #[test]
    fn 计算实际线程数() {
        let t = ThreadConfig {
            worker_threads: 3,
            thread_num: 0,
            max_blocking_threads: 2,
            stack_size: DEFAULT_THREAD_STACK_SIZE,
        };
        assert_eq!(effective_worker_threads(&t), 3);
        let t = ThreadConfig {
            worker_threads: 0,
            thread_num: 1,
            ..t
        };
        assert_eq!(effective_worker_threads(&t), 1);
        let t = ThreadConfig {
            worker_threads: 0,
            thread_num: 0,
            ..t
        };
        assert!(effective_worker_threads(&t) >= 1);
    }

    #[test]
    fn 带_bom_的配置可加载() {
        let text = "\u{feff}{\"listenPort\":5000,\"upstream\":[{\"host\":\"a\",\"port\":1}]}";
        assert!(load_str(text).is_ok());
    }

    #[test]
    fn 客户端认证解析_跳过空用户名() {
        let text = r#"{
            "listenPort": 5000,
            "upstream": [ { "host": "127.0.0.1", "port": 6100 } ],
            "AuthClientInfo": [
                { "user": "alice", "pwd": "secret" },
                { "user": "", "pwd": "ignored" },
                { "user": "bob" }
            ]
        }"#;
        let c = load_str(text).unwrap();
        assert_eq!(c.auth.len(), 2);
        assert_eq!(c.auth[0].user, "alice");
        assert_eq!(c.auth[0].pwd, "secret");
        assert_eq!(c.auth[1].user, "bob");
        assert!(c.auth[1].pwd.is_empty());
        assert!(c.notes.iter().any(|n| n.contains("已忽略")));
        assert!(c.notes.iter().any(|n| n.contains("密码为空")));
        assert!(c.notes.iter().any(|n| n.contains("已启用 SOCKS5")));
    }

    #[test]
    fn 无认证用户时_auth_为空() {
        let c = load_str(minimal()).unwrap();
        assert!(c.auth.is_empty());
    }

    #[test]
    fn 首包与握手超时可配置且校验() {
        let text = r#"{
            "listenPort": 5000,
            "firstPacketTimeout": 30000,
            "handshakeTimeout": 5000,
            "upstream": [ { "host": "127.0.0.1", "port": 6100 } ]
        }"#;
        let c = load_str(text).unwrap();
        assert_eq!(c.first_packet_timeout, Duration::from_millis(30_000));
        assert_eq!(c.handshake_timeout, Duration::from_millis(5_000));

        let text = r#"{
            "listenPort": 5000,
            "firstPacketTimeout": 999999999,
            "upstream": [ { "host": "127.0.0.1", "port": 6100 } ]
        }"#;
        assert!(load_str(text).is_err());
    }

    #[test]
    fn 选路规则解析与校验() {
        let text = r#"{
            "listenPort": 5000,
            "upstreamSelectRule": "one_by_one",
            "serverChangeTime": 5000,
            "upstream": [ { "host": "127.0.0.1", "port": 6100 } ]
        }"#;
        let c = load_str(text).unwrap();
        assert_eq!(c.select_rule, SelectRule::OneByOne);
        assert_eq!(c.server_change_time, Duration::from_millis(5_000));

        let text = r#"{
            "listenPort": 5000,
            "upstreamSelectRule": "no_such_rule",
            "upstream": [ { "host": "127.0.0.1", "port": 6100 } ]
        }"#;
        let e = load_str(text).unwrap_err().to_string();
        assert!(
            e.contains("upstreamSelectRule"),
            "错误信息应提示字段名: {e}"
        );
    }

    #[test]
    fn tls_探测配置解析与校验() {
        let text = r#"{
            "listenPort": 5000,
            "upstream": [ { "host": "127.0.0.1", "port": 6100 } ],
            "healthCheck": {
                "connect": { "period": 60000, "timeout": 5000 },
                "target": {
                    "host": "www.example.com",
                    "port": 443,
                    "sni": "www.example.com",
                    "httpPath": "/health",
                    "userAgent": "probe/1.0",
                    "expectStatus": [204]
                }
            }
        }"#;
        let c = load_str(text).unwrap();
        let tls = c.health.tls.expect("应启用 TLS 探测");
        assert_eq!(tls.host, "www.example.com");
        assert_eq!(tls.port, 443);
        assert_eq!(tls.sni, "www.example.com");
        assert_eq!(tls.http_path, "/health");
        assert_eq!(tls.user_agent, "probe/1.0");
        assert_eq!(tls.expect_status, vec![204]);
        assert_eq!(tls.period, Duration::from_millis(60_000));
        assert_eq!(tls.timeout, Duration::from_millis(5_000));

        // 只配 connect 不配 target → 报错
        let text = r#"{
            "listenPort": 5000,
            "upstream": [ { "host": "a", "port": 1 } ],
            "healthCheck": { "connect": { "period": 60000 } }
        }"#;
        let e = load_str(text).unwrap_err().to_string();
        assert!(e.contains("healthCheck.target.host"), "实际: {e}");

        // disableConnectTest=true → 不启用 readiness 探测
        let text = r#"{
            "listenPort": 5000,
            "disableConnectTest": true,
            "upstream": [ { "host": "a", "port": 1 } ],
            "healthCheck": { "target": { "host": "www.example.com" } }
        }"#;
        let c = load_str(text).unwrap();
        assert!(c.health.tls.is_none());
        assert!(
            !c.health.enabled,
            "disableConnectTest=true 应完全禁用健康检查"
        );

        // 未配置 healthCheck → 只做 TCP
        let c = load_str(minimal()).unwrap();
        assert!(c.health.tls.is_none());
        assert_eq!(c.health.period, Duration::from_millis(5_000));
    }

    #[test]
    fn 未知字段会告警() {
        let text = r#"{
            "listenPort": 5000,
            "threadStackSizeKb": 512,
            "upstream": [ { "host": "127.0.0.1", "port": 6100, "enabled": false } ]
        }"#;
        let c = load_str(text).unwrap();
        assert!(
            c.notes.iter().any(|n| n.contains("threadStackSizeKb")),
            "notes={:?}",
            c.notes
        );
        assert!(
            c.notes.iter().any(|n| n.contains("enabled")),
            "notes={:?}",
            c.notes
        );
    }

    #[test]
    fn 旧探测字段映射到_health_check() {
        let text = r#"{
            "listenPort": 5000,
            "tcpCheckPeriod": 7000,
            "tcpCheckStart": 100,
            "connectCheckPeriod": 120000,
            "testRemoteHost": "probe.example.com",
            "testRemotePort": 8443,
            "upstream": [ { "host": "127.0.0.1", "port": 6100 } ]
        }"#;
        let c = load_str(text).unwrap();
        assert_eq!(c.health.period, Duration::from_millis(7_000));
        assert_eq!(c.health.start_delay, Duration::from_millis(100));
        let tls = c.health.tls.expect("旧字段应启用 TLS 探测");
        assert_eq!(tls.host, "probe.example.com");
        assert_eq!(tls.port, 8443);
        assert_eq!(tls.period, Duration::from_millis(120_000));
        assert!(
            c.notes.iter().any(|n| n.contains("映射")),
            "notes={:?}",
            c.notes
        );
    }

    #[test]
    fn dns_ttl_解析与校验() {
        let text = r#"{"listenPort":5000,"dnsTtl":60000,"upstream":[{"host":"a","port":1}]}"#;
        let c = load_str(text).unwrap();
        assert_eq!(c.dns_ttl, Some(Duration::from_millis(60_000)));
        let text = r#"{"listenPort":5000,"dnsTtl":500,"upstream":[{"host":"a","port":1}]}"#;
        assert!(load_str(text).is_err(), "dnsTtl 小于 1000ms 应报错");
        let text = r#"{"listenPort":5000,"dnsTtl":0,"upstream":[{"host":"a","port":1}]}"#;
        assert_eq!(load_str(text).unwrap().dns_ttl, None, "0 表示不刷新");
    }

    #[test]
    fn disable_http_forward_解析与提示() {
        let text = r#"{
            "listenPort": 5000,
            "disableHttpForward": true,
            "upstream": [ { "host": "127.0.0.1", "port": 6100 } ]
        }"#;
        let c = load_str(text).unwrap();
        assert!(c.disable_http_forward);
        assert!(
            c.notes.iter().any(|n| n.contains("501")),
            "notes={:?}",
            c.notes
        );
        let c = load_str(minimal()).unwrap();
        assert!(!c.disable_http_forward, "默认应允许 HTTP 正向代理");
    }

    #[test]
    fn 连接监测与监控面配置解析() {
        // 默认：明细开启，detailLimit = maxConnections，监控面复用控制面
        let c = load_str(minimal()).unwrap();
        assert!(c.conn_tracking.enabled);
        assert!(c.conn_tracking.detail);
        assert_eq!(c.conn_tracking.detail_limit, c.max_connections);
        assert!(c.monitor_server.is_none());

        // 自定义明细与独立监控端口
        let text = r#"{
            "listenPort": 5000,
            "maxConnections": 500,
            "monitorServerHost": "127.0.0.1",
            "monitorServerPort": 5120,
            "connectionTracking": {
                "enabled": true,
                "detail": false,
                "detailLimit": 200,
                "clientTopLimit": 50
            },
            "upstream": [ { "host": "127.0.0.1", "port": 6100 } ]
        }"#;
        let c = load_str(text).unwrap();
        assert!(!c.conn_tracking.detail);
        assert_eq!(c.conn_tracking.detail_limit, 200);
        assert_eq!(c.conn_tracking.client_top_limit, 50);
        assert_eq!(c.monitor_server.as_ref().map(|m| m.port), Some(5120));

        // 监控面与控制面同地址 → 合并为共用监听
        let text = r#"{
            "listenPort": 5000,
            "stateServerHost": "127.0.0.1",
            "stateServerPort": 5110,
            "monitorServerHost": "127.0.0.1",
            "monitorServerPort": 5110,
            "upstream": [ { "host": "127.0.0.1", "port": 6100 } ]
        }"#;
        let c = load_str(text).unwrap();
        assert!(c.monitor_server.is_none());
        assert!(
            c.notes.iter().any(|n| n.contains("复用控制面")),
            "notes={:?}",
            c.notes
        );
    }

    #[test]
    fn sleep_time_与_addition_check_period_生效() {
        let text = r#"{
            "listenPort": 5000,
            "sleepTime": 60000,
            "additionCheckPeriod": 10000,
            "upstream": [ { "host": "127.0.0.1", "port": 6100 } ]
        }"#;
        let c = load_str(text).unwrap();
        assert_eq!(
            c.health.sleep_time,
            Some(Duration::from_millis(60_000)),
            "sleepTime 应生效"
        );
        assert_eq!(
            c.health.addition_check_period,
            Some(Duration::from_millis(10_000)),
            "additionCheckPeriod 应生效"
        );
        assert!(c.notes.iter().any(|n| n.contains("sleepTime")));
        assert!(c.notes.iter().any(|n| n.contains("additionCheckPeriod")));
    }

    #[test]
    fn 默认值对齐_cpp_检查时序() {
        let c = load_str(minimal()).unwrap();
        // sleepTime / additionCheckPeriod / xxxCheckStart 的默认值必须与 C++ README
        // "Full configuration" 一致，否则迁移后探测节奏会静默变化。
        assert_eq!(
            c.health.sleep_time,
            Some(Duration::from_millis(DEFAULT_SLEEP_TIME_MS)),
            "sleepTime 默认应为 30min（C++ 默认 1800000ms）"
        );
        assert_eq!(
            c.health.addition_check_period,
            Some(Duration::from_millis(DEFAULT_ADDITION_CHECK_PERIOD_MS)),
            "additionCheckPeriod 默认应为 10s（C++ 默认 10000ms）"
        );
        assert_eq!(
            c.health.start_delay,
            Duration::from_millis(DEFAULT_CHECK_START_MS),
            "tcpCheckStart 默认应为 1s（C++ 默认 1000ms）"
        );
        assert!(
            c.disable_socks4,
            "disableSocks4 默认应为 true（对齐 C++，拦掉实现不正确的陈旧客户端）"
        );
        assert!(
            c.health.connect_fail_mark_degraded,
            "connectFailMarkDegraded 默认应为 true（ADR-002 #3）"
        );
    }

    #[test]
    fn sleep_time_为_0_关闭空闲休眠() {
        let text = r#"{"listenPort":5000,"sleepTime":0,"upstream":[{"host":"a","port":1}]}"#;
        let c = load_str(text).unwrap();
        assert_eq!(c.health.sleep_time, None, "sleepTime=0 应表示始终周期探测");
    }

    #[test]
    fn disable_connection_tracker_映射为关闭连接监测() {
        let text = r#"{"listenPort":5000,"disableConnectionTracker":true,"upstream":[{"host":"a","port":1}]}"#;
        let c = load_str(text).unwrap();
        assert!(!c.conn_tracking.enabled, "旧字段应映射为关闭连接监测");
        assert!(
            c.notes
                .iter()
                .any(|n| n.contains("disableConnectionTracker"))
        );
        // 新字段优先
        let text = r#"{"listenPort":5000,"disableConnectionTracker":true,
            "connectionTracking":{"enabled":true},
            "upstream":[{"host":"a","port":1}]}"#;
        assert!(
            load_str(text).unwrap().conn_tracking.enabled,
            "connectionTracking.enabled 应优先于旧字段"
        );
    }

    #[test]
    fn connect_fail_mark_degraded_可关闭() {
        let text = r#"{"listenPort":5000,
            "healthCheck":{"connectFailMarkDegraded":false},
            "upstream":[{"host":"a","port":1}]}"#;
        let c = load_str(text).unwrap();
        assert!(!c.health.connect_fail_mark_degraded);
    }

    #[test]
    fn inherit_规则降级为_random() {
        let text = r#"{"listenPort":5000,"upstreamSelectRule":"inherit",
            "upstream":[{"host":"a","port":1}]}"#;
        let c = load_str(text).unwrap();
        assert!(
            matches!(c.select_rule, SelectRule::Random),
            "inherit 应按 random 处理，避免 C++ 配置迁移启动失败"
        );
        assert!(c.notes.iter().any(|n| n.contains("inherit")));
    }

    #[test]
    fn 旧字段_check_start_映射到_start_delay() {
        let text = r#"{"listenPort":5000,
            "tcpCheckStart":2500,
            "connectCheckStart":3000,
            "testRemoteHost":"example.com",
            "testRemotePort":443,
            "upstream":[{"host":"a","port":1}]}"#;
        let c = load_str(text).unwrap();
        assert_eq!(
            c.health.start_delay,
            Duration::from_millis(2_500),
            "tcpCheckStart 应映射到 TCP 首探延迟"
        );
        assert_eq!(
            c.health.tls.expect("应启用 TLS 探测").start_delay,
            Duration::from_millis(3_000),
            "connectCheckStart 应映射到 readiness 首探延迟"
        );
    }

    #[test]
    fn 新字段优先于旧探测字段() {
        let text = r#"{
            "listenPort": 5000,
            "tcpCheckPeriod": 7000,
            "testRemoteHost": "old.example.com",
            "healthCheck": {
                "tcp": { "period": 3000 },
                "target": { "host": "new.example.com", "port": 443 }
            },
            "upstream": [ { "host": "127.0.0.1", "port": 6100 } ]
        }"#;
        let c = load_str(text).unwrap();
        assert_eq!(c.health.period, Duration::from_millis(3_000));
        assert_eq!(
            c.health.tls.expect("应启用 TLS 探测").host,
            "new.example.com"
        );
        assert!(
            c.notes.iter().any(|n| n.contains("新字段优先")),
            "notes={:?}",
            c.notes
        );
    }
}
