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

/// C++ 的 `internalBehavior`（整节在 C++ 中未被解析）。
///
/// 仅用于兼容解析与 `connectTimeout` 兜底；其余字段保留定义以免旧配置"静默丢字段"。
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
#[allow(dead_code)] // max_probe_concurrency / connect_fail_mark_degraded 在 M1.5（TLS 探测）启用
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
    /// M1.5（TLS 探测）使用；M1.4 仅解析。
    #[serde(default)]
    pub max_probe_concurrency: Option<usize>,
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

/// 探测目标（M1.5 使用：TLS + HTTP readiness 探测）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // 字段在 M1.5（TLS/HTTP 探测）启用
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
    /// 期望状态码；为空表示接受 2xx/3xx。
    pub expect_status: Vec<u16>,
    /// 自定义信任根（PEM 文件）；None 表示使用操作系统根证书。
    pub ca_file: Option<std::path::PathBuf>,
    /// 探测并发上限（上游很多时避免同时打满）。
    pub max_concurrency: usize,
}

/// 健康检查有效配置（M1.4：TCP 探活；M1.5：+ TLS/HTTP readiness）。
#[derive(Debug, Clone)]
pub struct HealthConfig {
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
}

/// 原始配置：字段名与 C++ `config.json` 对齐。
///
/// 标注 `allow(dead_code)` 的原因：其中一部分字段（探测周期、控制面等）在 M0 只做兼容解析，
/// M1–M3 才会真正使用；保留定义可保证旧配置字段有明确归属，也避免 serde 静默丢弃。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
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
    #[serde(default)]
    pub disable_socks4: bool,

    // ===== 控制面（M3 生效，M0 仅兼容解析）=====
    #[serde(default)]
    pub state_server_host: Option<String>,
    #[serde(default)]
    pub state_server_port: Option<u16>,
    #[serde(default)]
    pub state_max_response_bytes: Option<usize>,

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
    /// 健康检查配置（M1.4：TCP 探活 + fail-fast + DOWN 退避）。
    pub health: HealthConfig,
    pub tradition_tcp_relay: bool,
    pub disable_socks4: bool,
    /// 解析过程中发现的"被忽略/降级"的配置项说明，启动时以 warning 打印。
    pub notes: Vec<String>,
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
    if value.get("healthCheck").is_some() {
        notes.push("healthCheck 已启用：TCP 探活 + fail-fast + DOWN 指数退避（M1.4）".into());
    }
    if value.get("upstreamSelectRule").is_some() || value.get("serverChangeTime").is_some() {
        notes.push("upstreamSelectRule/serverChangeTime 已启用（M1.3）".into());
    }
    if value.get("testRemoteHost").is_some() || value.get("disableConnectTest").is_some() {
        notes.push(
            "旧版探测字段（testRemoteHost/tcpCheck*/connectCheck*）已忽略，请改用 healthCheck 节"
                .into(),
        );
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
        return Err(err("所有上游都被禁用（disable=true），没有可用上游"));
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
        Some(s) => SelectRule::parse(s).ok_or_else(|| {
            err(format!(
                "upstreamSelectRule={s:?} 非法，允许值: loop/random/one_by_one/change_by_time/force_only_one"
            ))
        })?,
    };
    let server_change_time =
        Duration::from_millis(raw.server_change_time.filter(|v| *v > 0).unwrap_or(60_000));

    // ===== 健康检查（ADR-002：fail-fast，不重试/不防抖）=====
    let health_raw = raw.health_check.clone().unwrap_or_default();
    if health_raw.connect.is_some() || health_raw.target.is_some() {
        notes.push("healthCheck.connect/target 已启用：TLS + HTTP readiness 探测（M1.5）".into());
    }
    let health = build_health_config(&health_raw, raw.disable_connect_test)?;

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

    Ok(Config {
        listen,
        upstreams,
        retry_times: raw.retry_times.max(1),
        connect_timeout: Duration::from_millis(connect_timeout_ms),
        first_packet_timeout: Duration::from_millis(first_packet_timeout_ms),
        handshake_timeout: Duration::from_millis(handshake_timeout_ms),
        select_rule,
        server_change_time,
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
            start_delay: Duration::from_millis(connect.start_delay.unwrap_or(1_000)),
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
        period: Duration::from_millis(period_ms),
        timeout: Duration::from_millis(timeout_ms),
        start_delay: Duration::from_millis(start_delay_ms),
        backoff_initial: Duration::from_millis(initial_ms),
        backoff_multiplier: multiplier,
        backoff_max: Duration::from_millis(max_ms),
        probe_jitter: jitter,
        backoff_jitter,
        tls,
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
    fn 全部上游禁用_报错() {
        let text = r#"{"listenPort":5000,"upstream":[{"host":"a","port":1,"disable":1}]}"#;
        assert!(load_str(text).is_err());
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

        // 未配置 healthCheck → 只做 TCP
        let c = load_str(minimal()).unwrap();
        assert!(c.health.tls.is_none());
        assert_eq!(c.health.period, Duration::from_millis(5_000));
    }
}
