//! 精简控制面（M3，ADR-001）+ 连接监测面。
//!
//! 端点（无鉴权、无 CORS、无静态文件、不返回任何密码）：
//! - `GET /healthz`：存活探针，返回 `200 ok`（不读业务状态）；
//! - `GET /api/v1/status`：进程/统计/健康/连接摘要（JSON，`schema: "v1"`）；
//! - `GET /api/v1/upstreams`：上游逐条状态（JSON 数组，不含认证信息）；
//! - `GET /api/v1/connections`：连接明细（过滤 + 游标分页，需 `connectionTracking.detail=true`）；
//! - `GET /api/v1/connections/summary`：按上游/客户端/监听三维度连接数；
//! - `POST /api/v1/control`：唯一写端点（控制面监听提供；监控面监听不提供）。
//!
//! 安全默认（ADR-001）：默认仅监听回环地址；绑定非回环只告警不拒绝；
//! 响应上限由配置 `stateMaxResponseBytes` 控制；每个控制命令记一条审计日志。
//!
//! `close_conn`/`close_all` 依赖连接明细（`connectionTracking.detail=true`，默认开启）；
//! `reload_config` 不提供（U5 不做），未知命令返回 `unsupported`。

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::app::AppCtx;
use crate::config::ListenTarget;
use crate::conn::{ConnFilter, ConnId};
use crate::upstream::SelectRule;

/// 请求体上限（ADR-001：≤64 KB）。
const MAX_BODY: usize = 64 * 1024;
/// 单个控制面连接的处理超时（防慢速连接占资源）。
const CONN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 启动控制面 HTTP 服务（随 `cancel` 退出；绑定失败只告警，不影响主服务）。
pub async fn serve(
    ctx: Arc<AppCtx>,
    bind: ListenTarget,
    max_response: usize,
    cancel: CancellationToken,
) {
    serve_with(ctx, bind, max_response, cancel, true, "控制面").await;
}

/// 启动监控面（只读：healthz/status/upstreams/connections；**不提供** `/api/v1/control` 写命令）。
pub async fn serve_monitor(
    ctx: Arc<AppCtx>,
    bind: ListenTarget,
    max_response: usize,
    cancel: CancellationToken,
) {
    serve_with(ctx, bind, max_response, cancel, false, "监控面").await;
}

async fn serve_with(
    ctx: Arc<AppCtx>,
    bind: ListenTarget,
    max_response: usize,
    cancel: CancellationToken,
    allow_control: bool,
    label: &'static str,
) {
    let addr = format!("{}:{}", bind.host, bind.port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            warn!(%addr, error = %e, "{}绑定失败（不影响主服务）", label);
            return;
        }
    };
    let local = listener.local_addr().ok();
    info!(
        local = ?local,
        allow_control,
        "{}已就绪（无鉴权，建议仅回环监听；ADR-001）",
        label
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => {
                let Ok((stream, peer)) = accepted else { continue };
                let ctx = Arc::clone(&ctx);
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| {
                        let ctx = Arc::clone(&ctx);
                        async move { handle(req, ctx, peer, max_response, allow_control).await }
                    });
                    let conn = http1::Builder::new().serve_connection(io, service);
                    if tokio::time::timeout(CONN_TIMEOUT, conn).await.is_err() {
                        warn!(%peer, "{}连接超时", label);
                    }
                });
            }
        }
    }
    info!("{}已停止", label);
}

/// 路由与请求处理。
async fn handle(
    req: Request<hyper::body::Incoming>,
    ctx: Arc<AppCtx>,
    peer: SocketAddr,
    max_response: usize,
    allow_control: bool,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();

    let result = match (&method, path.as_str()) {
        (&Method::GET, "/healthz") => Ok(text(StatusCode::OK, "ok")),
        (&Method::GET, "/api/v1/status") => json_response(status_json(&ctx), max_response),
        (&Method::GET, "/api/v1/upstreams") => json_response(upstreams_json(&ctx), max_response),
        (&Method::GET, "/api/v1/connections") => {
            json_response(connections_json(&ctx, &query), max_response)
        }
        (&Method::GET, "/api/v1/connections/summary") => {
            json_response(connections_summary_json(&ctx), max_response)
        }
        (&Method::POST, "/api/v1/control") if allow_control => {
            control(&ctx, req, peer, max_response).await
        }
        _ => Ok(text(StatusCode::NOT_FOUND, "not found")),
    };

    match result {
        Ok(resp) => Ok(resp),
        Err(status) => Ok(text(status, status.canonical_reason().unwrap_or("error"))),
    }
}

/// `POST /api/v1/control`：解析命令并执行，返回 `{"ok":bool,...}`。
async fn control(
    ctx: &Arc<AppCtx>,
    req: Request<hyper::body::Incoming>,
    peer: SocketAddr,
    max_response: usize,
) -> Result<Response<Full<Bytes>>, StatusCode> {
    // 先按 Content-Length 拒绝超大请求体，避免把超大 body 全部 collect 进内存（ADR-001）。
    if let Some(len) = req
        .headers()
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
        && len > MAX_BODY
    {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let body = req
        .into_body()
        .collect()
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?
        .to_bytes();
    if body.len() > MAX_BODY {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let parsed: Value = serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let cmd = parsed.get("cmd").and_then(Value::as_str).unwrap_or("");
    let args = parsed.get("args").cloned().unwrap_or(Value::Null);
    if cmd.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let (ok, detail) = execute(ctx, cmd, &args);
    // 审计日志：来源 IP + cmd + 参数 + 结果（ADR-001）
    if ok {
        info!(%peer, cmd, args = %args, detail = %detail, "控制命令");
    } else {
        warn!(%peer, cmd, args = %args, detail = %detail, "控制命令失败");
    }
    json_response(
        json!({"ok": ok, "cmd": cmd, "detail": detail}),
        max_response,
    )
}

/// 执行控制命令；返回 `(是否成功, 说明)`。
///
/// 支持的命令（精简集，ADR-001 + 维度扩展）：
/// - `check_upstream`：立即探测一次（`args.index` 可选，缺省=全部）；
/// - `enable_upstream` / `disable_upstream`：人工启停（`args.index` 或 `args.name`）；
/// - `set_rule`：切换选路规则（`args.rule`），或设置维度强制上游
///   （`args.scope` = global/client/listen、`args.target`、`args.index`；`clear=true` 清除）；
/// - `close_conn`：按 `id`/`client`/`listen`/`upstream` 中断连接；
/// - `close_all`：中断全部连接（需 `connectionTracking.detail=true`）。
fn execute(ctx: &Arc<AppCtx>, cmd: &str, args: &Value) -> (bool, String) {
    match cmd {
        "check_upstream" => {
            if !ctx.health.enabled() {
                (
                    false,
                    "健康检查已禁用（disableConnectTest=true）".to_string(),
                )
            } else {
                match resolve_index(ctx, args) {
                    Ok(None) => {
                        let total = ctx.health.len();
                        for i in 0..total {
                            ctx.health.probe_now(i);
                        }
                        (true, format!("已安排 {total} 个上游立即探测"))
                    }
                    Ok(Some(i)) => {
                        if ctx.health.probe_now(i) {
                            (true, format!("已安排上游 {i} 立即探测"))
                        } else {
                            (false, format!("index 越界: {i}"))
                        }
                    }
                    Err(e) => (false, e),
                }
            }
        }
        "enable_upstream" | "disable_upstream" => {
            let enable = cmd == "enable_upstream";
            match resolve_index(ctx, args) {
                Ok(Some(i)) => {
                    if ctx.selector.set_manual_enabled(i, enable) {
                        (
                            true,
                            format!(
                                "上游 {i} 已{}",
                                if enable {
                                    "人工启用"
                                } else {
                                    "人工禁用"
                                }
                            ),
                        )
                    } else {
                        (false, format!("index 越界: {i}"))
                    }
                }
                Ok(None) => (false, "需要 args.index 或 args.name".to_string()),
                Err(e) => (false, e),
            }
        }
        "set_rule" => {
            let mut messages: Vec<String> = Vec::new();
            // 1) 全局规则（可选）
            if let Some(rule_raw) = args.get("rule").and_then(Value::as_str) {
                match SelectRule::parse(rule_raw) {
                    Some(rule) => {
                        ctx.selector.set_rule(rule);
                        messages.push(format!("选路规则已切换为 {}", rule.as_str()));
                    }
                    None => return (false, format!("未知规则: {rule_raw}")),
                }
            }
            // 2) 维度强制覆盖（可选）：scope + target + index；clear=true 表示清除
            if let Some(scope) = args.get("scope").and_then(Value::as_str) {
                let clear = args.get("clear").and_then(Value::as_bool).unwrap_or(false);
                let target = args.get("target").and_then(Value::as_str);
                let index = if clear {
                    None
                } else {
                    match args.get("index").and_then(Value::as_u64) {
                        Some(v) => Some(v as usize),
                        None => {
                            return (
                                false,
                                "设置维度覆盖需要 args.index（清除请用 clear=true）".to_string(),
                            );
                        }
                    }
                };
                match ctx.selector.set_scope_override(scope, target, index) {
                    Ok(msg) => messages.push(msg),
                    Err(e) => return (false, e),
                }
            } else if args.get("index").is_some() {
                return (
                    false,
                    "设置维度覆盖需要 args.scope（global/client/listen）".to_string(),
                );
            }
            if messages.is_empty() {
                (
                    false,
                    "set_rule 需要 args.rule，或 scope+target+index（clear=true 清除）".to_string(),
                )
            } else {
                (true, messages.join("；"))
            }
        }
        "close_conn" | "close_all" => {
            if !ctx.conns.enabled() {
                return (
                    false,
                    "连接监测未启用（connectionTracking.enabled=false）".to_string(),
                );
            }
            if !ctx.conns.detail_enabled() {
                return (
                    false,
                    "连接明细未启用（connectionTracking.detail=false），无法按连接中断".to_string(),
                );
            }
            let filter = match parse_close_filter(cmd, args) {
                Ok(f) => f,
                Err(e) => return (false, e),
            };
            let closed = ctx.conns.close(&filter);
            (true, format!("已中断 {closed} 条连接"))
        }
        _ => (
            false,
            format!(
                "不支持的命令: {cmd}（支持 check_upstream / enable_upstream / disable_upstream / set_rule / close_conn / close_all）"
            ),
        ),
    }
}

/// 解析 `close_conn` 的过滤条件（`close_all` 等价于匹配全部）。
fn parse_close_filter(cmd: &str, args: &Value) -> Result<ConnFilter, String> {
    if cmd == "close_all" || args.get("all").and_then(Value::as_bool) == Some(true) {
        return Ok(ConnFilter::default());
    }
    let mut filter = ConnFilter::default();
    let mut any = false;
    if let Some(v) = args.get("id").and_then(Value::as_u64) {
        filter.id = Some(v);
        any = true;
    }
    if let Some(v) = args.get("client").and_then(Value::as_str) {
        filter.client = Some(
            v.parse()
                .map_err(|_| format!("client 不是合法 IP: {v:?}"))?,
        );
        any = true;
    }
    if let Some(v) = args.get("listen").and_then(Value::as_str) {
        filter.listen = Some(
            v.parse()
                .map_err(|_| format!("listen 不是合法 ip:port: {v:?}"))?,
        );
        any = true;
    }
    if let Some(v) = args.get("upstream").and_then(Value::as_u64) {
        filter.upstream = Some(v as usize);
        any = true;
    }
    if !any {
        return Err(
            "close_conn 需要 id/client/listen/upstream 之一（关闭全部用 close_all）".to_string(),
        );
    }
    Ok(filter)
}

/// 解析 `index` 或 `name`；都不提供返回 `Ok(None)`。
fn resolve_index(ctx: &Arc<AppCtx>, args: &Value) -> Result<Option<usize>, String> {
    if let Some(raw) = args.get("index").and_then(Value::as_u64) {
        let idx = raw as usize;
        if idx >= ctx.selector.len() {
            return Err(format!(
                "index 越界: {idx}（共 {} 个上游）",
                ctx.selector.len()
            ));
        }
        return Ok(Some(idx));
    }
    if let Some(name) = args.get("name").and_then(Value::as_str) {
        return ctx
            .selector
            .upstreams()
            .iter()
            .position(|u| u.name == name)
            .map(Some)
            .ok_or_else(|| format!("未找到上游: {name}"));
    }
    Ok(None)
}

/// `/api/v1/status`：进程 / 统计 / 健康摘要。
fn status_json(ctx: &Arc<AppCtx>) -> Value {
    let stats = ctx.stats.snapshot();
    let health = ctx.health.snapshot();
    let available = health.iter().filter(|h| h.available).count();
    let cfg = &ctx.cfg;
    json!({
        "schema": "v1",
        "version": env!("CARGO_PKG_VERSION"),
        "uptimeSeconds": ctx.started.elapsed().as_secs(),
        "workers": ctx_workers(ctx),
        "bufferSize": cfg.buffer_size,
        "maxConnections": cfg.max_connections,
        "traditionTcpRelay": cfg.tradition_tcp_relay,
        "listen": cfg.listen.iter().map(|l| l.to_string()).collect::<Vec<_>>(),
        "selectRule": ctx.selector.rule().as_str(),
        "stats": {
            "accepted": stats.accepted,
            "rejected": stats.rejected,
            "active": stats.active,
            "bytesDownToUp": stats.bytes_down_to_up,
            "bytesUpToDown": stats.bytes_up_to_down,
            "connectFailed": stats.connect_failed,
            "relayErrors": stats.relay_errors,
            "protocolErrors": stats.protocol_errors,
            "handshakeFailed": stats.handshake_failed,
            "upstreamHandshakeFailed": stats.upstream_handshake_failed,
            "firstPacketTimeout": stats.first_packet_timeout,
        },
        "health": {
            "available": available,
            "total": health.len(),
        },
        "connections": {
            "enabled": ctx.conns.enabled(),
            "detail": ctx.conns.detail_enabled(),
            "active": ctx.conns.active(),
            "detailLimit": ctx.conns.cfg().detail_limit,
        },
    })
}

/// 实际 worker 线程数（配置 0 表示"自动=核数"，这里按同样规则展示）。
fn ctx_workers(ctx: &Arc<AppCtx>) -> usize {
    let t = &ctx.cfg.thread;
    if t.worker_threads > 0 {
        t.worker_threads
    } else if t.thread_num > 0 {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        t.thread_num.min(cores)
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    }
}

/// `/api/v1/upstreams`：JSON 数组，索引=**解析成功上游的顺序**（通常等于配置顺序；
/// 解析失败的条目不进入池），**不含认证信息**。
fn upstreams_json(ctx: &Arc<AppCtx>) -> Value {
    let health = ctx.health.snapshot();
    let list: Vec<Value> = ctx
        .selector
        .upstreams()
        .iter()
        .enumerate()
        .map(|(i, u)| {
            let h = health.get(i);
            json!({
                "index": i,
                "name": u.name,
                "host": u.host,
                "port": u.port,
                "addr": u.addr().to_string(),
                "enabled": ctx.selector.manual_enabled(i),
                "available": h.map(|h| h.available).unwrap_or(false),
                "liveness": h.map(|h| h.liveness).unwrap_or(false),
                "readiness": h.map(|h| h.readiness).unwrap_or(false),
                "failures": h.map(|h| h.failures).unwrap_or(0),
                "tlsFailures": h.map(|h| h.tls_failures).unwrap_or(0),
                "backoffMs": h.map(|h| h.backoff_ms).unwrap_or(0),
                "rttMs": h.map(|h| h.rtt_ms).unwrap_or(0),
            })
        })
        .collect();
    Value::Array(list)
}

/// 解析查询串（参数只接受 IP/端口/数字，无需 URL 解码）。
fn parse_query(query: &str) -> std::collections::HashMap<&str, &str> {
    query
        .split('&')
        .filter(|s| !s.is_empty())
        .filter_map(|kv| kv.split_once('='))
        .collect()
}

/// 解析 `/api/v1/connections` 的过滤/分页参数。
fn parse_conn_filter(query: &str) -> Result<(ConnFilter, Option<ConnId>, usize), String> {
    let q = parse_query(query);
    let client = match q.get("client") {
        Some(s) => Some(
            s.parse()
                .map_err(|_| format!("client 不是合法 IP: {s:?}"))?,
        ),
        None => None,
    };
    let listen = match q.get("listen") {
        Some(s) => Some(
            s.parse()
                .map_err(|_| format!("listen 不是合法 ip:port: {s:?}"))?,
        ),
        None => None,
    };
    let upstream = match q.get("upstream") {
        Some(s) => Some(
            s.parse::<usize>()
                .map_err(|_| format!("upstream 不是合法下标: {s:?}"))?,
        ),
        None => None,
    };
    let cursor = match q.get("cursor") {
        Some(s) => Some(
            s.parse::<ConnId>()
                .map_err(|_| format!("cursor 不是合法数字: {s:?}"))?,
        ),
        None => None,
    };
    let limit = match q.get("limit") {
        Some(s) => s
            .parse::<usize>()
            .map_err(|_| format!("limit 不是合法数字: {s:?}"))?,
        None => 100,
    };
    Ok((
        ConnFilter {
            client,
            listen,
            upstream,
            ..Default::default()
        },
        cursor,
        limit,
    ))
}

/// `/api/v1/connections`：连接明细（过滤 + 游标分页）。
fn connections_json(ctx: &Arc<AppCtx>, query: &str) -> Value {
    if !ctx.conns.enabled() {
        return json!({
            "enabled": false,
            "detail": false,
            "total": 0,
            "connections": [],
            "note": "connectionTracking.enabled=false：连接监测未启用",
        });
    }
    if !ctx.conns.detail_enabled() {
        return json!({
            "enabled": true,
            "detail": false,
            "total": ctx.conns.active(),
            "connections": [],
            "note": "connectionTracking.detail=false：只保留三维度计数，不登记明细",
        });
    }
    match parse_conn_filter(query) {
        Ok((filter, cursor, limit)) => {
            let (items, next) = ctx.conns.page(&filter, cursor, limit);
            let list: Vec<Value> = items
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id,
                        "peer": c.peer.to_string(),
                        "listen": c.local.to_string(),
                        "protocol": c.protocol,
                        "upstream": c.upstream,
                        "state": c.state,
                        "uptimeMs": c.uptime_ms,
                        "bytesUp": c.bytes_up,
                        "bytesDown": c.bytes_down,
                    })
                })
                .collect();
            json!({
                "enabled": true,
                "detail": true,
                "total": ctx.conns.active(),
                "connections": list,
                "nextCursor": next,
            })
        }
        Err(e) => json!({"enabled": true, "detail": true, "error": e}),
    }
}

/// `/api/v1/connections/summary`：按上游/客户端/监听三维度聚合。
fn connections_summary_json(ctx: &Arc<AppCtx>) -> Value {
    if !ctx.conns.enabled() {
        return json!({
            "enabled": false,
            "total": 0,
            "byUpstream": [],
            "byListen": [],
            "byClient": [],
        });
    }
    let sum = ctx.conns.summary();
    let upstreams = ctx.selector.upstreams();
    let by_upstream: Vec<Value> = sum
        .by_upstream
        .iter()
        .map(|(i, count)| {
            let u = upstreams.get(*i);
            json!({
                "index": i,
                "name": u.map(|u| u.name.as_str()).unwrap_or("-"),
                "available": u.is_some_and(|_| ctx.health.is_available(*i)),
                "count": count,
            })
        })
        .collect();
    let by_listen: Vec<Value> = sum
        .by_listen
        .iter()
        .map(|(addr, count)| json!({"listen": addr.to_string(), "count": count}))
        .collect();
    let by_client: Vec<Value> = sum
        .by_client
        .iter()
        .map(|(ip, count)| json!({"client": ip.to_string(), "count": count}))
        .collect();
    json!({
        "enabled": true,
        "detail": ctx.conns.detail_enabled(),
        "total": sum.total,
        "byUpstream": by_upstream,
        "byListen": by_listen,
        "byClient": by_client,
        "clientTopLimit": ctx.conns.cfg().client_top_limit,
    })
}

/// 构造 JSON 响应；超过 `max_response` 时报 500（避免控制面拖垮内存）。
fn json_response(value: Value, max_response: usize) -> Result<Response<Full<Bytes>>, StatusCode> {
    let body = value.to_string();
    if body.len() > max_response {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/json; charset=utf-8")
        .body(Full::new(Bytes::from(body)))
        .expect("响应头合法"))
}

/// 构造纯文本响应。
fn text(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("Content-Type", "text/plain; charset=utf-8")
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("响应头合法")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::health::HealthRegistry;
    use crate::stats::Stats;
    use crate::upstream::{Upstream, UpstreamSelector};

    fn test_ctx() -> Arc<AppCtx> {
        let text = r#"{
            "listenHost": "127.0.0.1",
            "listenPort": 5500,
            "stateServerPort": 0,
            "upstream": [
                { "name": "a", "host": "127.0.0.1", "port": 6100, "authUser": "alice", "authPwd": "secret" },
                { "name": "b", "host": "127.0.0.1", "port": 6101 },
                { "name": "c", "host": "127.0.0.1", "port": 6102, "disable": true }
            ]
        }"#;
        let cfg = Arc::new(config::load_str(text).expect("配置合法"));
        let upstreams: Vec<Upstream> = cfg
            .upstreams
            .iter()
            .enumerate()
            .map(|(i, u)| Upstream {
                name: u.name.clone(),
                host: u.host.clone(),
                port: u.port,
                addr: std::sync::Arc::new(std::sync::Mutex::new(
                    format!("{}:{}", u.host, u.port).parse().unwrap(),
                )),
                auth_user: u.auth_user.clone(),
                auth_pwd: u.auth_pwd.clone(),
                health_index: i,
                enabled: u.enabled,
            })
            .collect();
        let selector = Arc::new(UpstreamSelector::new(
            upstreams.clone(),
            cfg.select_rule,
            cfg.server_change_time,
        ));
        let health = HealthRegistry::new(&upstreams, cfg.health.clone());
        let stats = Stats::new();
        let conns = crate::conn::ConnRegistry::new(cfg.conn_tracking.clone(), upstreams.len());
        AppCtx::new(
            cfg,
            selector,
            health,
            stats,
            conns,
            CancellationToken::new(),
        )
    }

    #[test]
    fn status_含_schema_与统计字段() {
        let ctx = test_ctx();
        let v = status_json(&ctx);
        assert_eq!(v["schema"], "v1");
        assert!(v["stats"]["accepted"].is_u64());
        assert_eq!(v["health"]["total"], 3);
        assert_eq!(v["listen"][0], "127.0.0.1:5500");
    }

    #[test]
    fn upstreams_不含密码与认证字段() {
        let ctx = test_ctx();
        let v = upstreams_json(&ctx);
        let text = v.to_string();
        assert!(v.is_array());
        assert_eq!(v.as_array().unwrap().len(), 3);
        assert!(!text.contains("secret"), "响应不得包含密码: {text}");
        assert!(!text.contains("authUser"), "响应不得包含认证字段: {text}");
        assert!(!text.contains("alice"), "响应不得包含用户名: {text}");
        // 初始禁用的上游也出现在列表里，且可被控制面启用
        assert_eq!(v[2]["name"], "c");
        assert_eq!(v[2]["enabled"], false);
    }

    #[test]
    fn set_rule_命令生效() {
        let ctx = test_ctx();
        let (ok, detail) = execute(&ctx, "set_rule", &json!({"rule": "loop"}));
        assert!(ok, "{detail}");
        assert_eq!(ctx.selector.rule(), SelectRule::Loop);
        let (ok, _) = execute(&ctx, "set_rule", &json!({"rule": "nope"}));
        assert!(!ok);
    }

    #[test]
    fn set_rule_维度覆盖命令() {
        let ctx = test_ctx();
        // 切到粘性规则，覆盖才生效
        let (ok, _) = execute(&ctx, "set_rule", &json!({"rule": "one_by_one"}));
        assert!(ok);
        // client 维度
        let (ok, detail) = execute(
            &ctx,
            "set_rule",
            &json!({"scope": "client", "target": "10.0.0.9", "index": 1}),
        );
        assert!(ok, "{detail}");
        // listen 维度
        let (ok, detail) = execute(
            &ctx,
            "set_rule",
            &json!({"scope": "listen", "target": "127.0.0.1:5500", "index": 0}),
        );
        assert!(ok, "{detail}");
        // 清除 client 覆盖
        let (ok, detail) = execute(
            &ctx,
            "set_rule",
            &json!({"scope": "client", "target": "10.0.0.9", "clear": true}),
        );
        assert!(ok, "{detail}");
        // 参数校验
        let (ok, _) = execute(&ctx, "set_rule", &json!({"scope": "client", "index": 1}));
        assert!(!ok, "client 维度缺 target 应失败");
        let (ok, _) = execute(
            &ctx,
            "set_rule",
            &json!({"scope": "bogus", "target": "1.2.3.4", "index": 0}),
        );
        assert!(!ok, "未知 scope 应失败");
        let (ok, _) = execute(&ctx, "set_rule", &json!({"index": 1}));
        assert!(!ok, "只有 index 缺 scope 应失败");
        let (ok, _) = execute(&ctx, "set_rule", &json!({}));
        assert!(!ok, "空参数应失败");
    }

    #[test]
    fn 人工启停命令生效() {
        let ctx = test_ctx();
        let (ok, _) = execute(&ctx, "disable_upstream", &json!({"index": 0}));
        assert!(ok);
        assert!(!ctx.selector.manual_enabled(0));
        let (ok, _) = execute(&ctx, "enable_upstream", &json!({"name": "a"}));
        assert!(ok);
        assert!(ctx.selector.manual_enabled(0));
        // 初始 disabled 的上游可被启用
        assert!(!ctx.selector.manual_enabled(2));
        let (ok, _) = execute(&ctx, "enable_upstream", &json!({"name": "c"}));
        assert!(ok);
        assert!(ctx.selector.manual_enabled(2));
        let (ok, _) = execute(&ctx, "enable_upstream", &json!({"index": 9}));
        assert!(!ok);
        let (ok, _) = execute(&ctx, "disable_upstream", &json!({"name": "nope"}));
        assert!(!ok);
    }

    #[test]
    fn check_upstream_命令() {
        let ctx = test_ctx();
        let (ok, _) = execute(&ctx, "check_upstream", &json!({"index": 1}));
        assert!(ok);
        let (ok, _) = execute(&ctx, "check_upstream", &Value::Null);
        assert!(ok);
        let (ok, _) = execute(&ctx, "check_upstream", &json!({"index": 5}));
        assert!(!ok);
    }

    #[test]
    fn 未知命令返回失败() {
        let ctx = test_ctx();
        let (ok, detail) = execute(&ctx, "no_such_cmd", &Value::Null);
        assert!(!ok);
        assert!(detail.contains("不支持"));
    }

    #[test]
    fn close_conn_与_close_all_命令() {
        let ctx = test_ctx();
        let cancel = CancellationToken::new();
        let (guard, token) = ctx.conns.register(
            "127.0.0.1:40001".parse().unwrap(),
            "127.0.0.1:5500".parse().unwrap(),
            &cancel,
        );
        // 参数缺失/非法
        let (ok, _) = execute(&ctx, "close_conn", &json!({}));
        assert!(!ok, "close_conn 无过滤条件应失败");
        let (ok, _) = execute(&ctx, "close_conn", &json!({"client": "not-ip"}));
        assert!(!ok, "非法 client 应失败");
        // 已中断前 token 未取消
        assert!(!token.is_cancelled());
        let (ok, detail) = execute(&ctx, "close_all", &Value::Null);
        assert!(ok, "{detail}");
        assert!(token.is_cancelled(), "close_all 应取消连接 token");
        drop(guard);
    }

    #[test]
    fn connections_端点_json_无敏感字段() {
        let ctx = test_ctx();
        let cancel = CancellationToken::new();
        let (guard, token) = ctx.conns.register(
            "127.0.0.1:40001".parse().unwrap(),
            "127.0.0.1:5500".parse().unwrap(),
            &cancel,
        );
        guard.set_protocol(crate::handshake::Protocol::Socks5);
        guard.set_upstream(1);

        let v = connections_json(&ctx, "");
        assert_eq!(v["enabled"], true);
        assert_eq!(v["detail"], true);
        let list = v["connections"].as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["protocol"], "socks5");
        assert_eq!(list[0]["upstream"], 1);
        assert_eq!(list[0]["listen"], "127.0.0.1:5500");
        let text = v.to_string();
        assert!(
            !text.contains("secret") && !text.contains("alice"),
            "{text}"
        );

        // 过滤与分页参数
        let v = connections_json(&ctx, "listen=127.0.0.1:5500&limit=10");
        assert_eq!(v["connections"].as_array().unwrap().len(), 1);
        let v = connections_json(&ctx, "client=not-an-ip");
        assert!(v.get("error").is_some(), "非法过滤参数应返回 error");

        let s = connections_summary_json(&ctx);
        assert_eq!(s["enabled"], true);
        assert_eq!(s["total"], 1);
        assert!(
            s["byUpstream"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["index"] == 1 && x["count"] == 1),
            "{s}"
        );
        assert!(
            s["byListen"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["listen"] == "127.0.0.1:5500" && x["count"] == 1),
            "{s}"
        );
        assert!(
            s["byClient"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["client"] == "127.0.0.1" && x["count"] == 1),
            "{s}"
        );

        assert!(!token.is_cancelled());
        drop(guard);
    }
}
