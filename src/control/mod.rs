//! 精简控制面（M3，ADR-001）。
//!
//! 只有 4 个端点，**无鉴权、无 CORS、无静态文件、不返回任何密码**：
//! - `GET /healthz`：存活探针，返回 `200 ok`（不读业务状态）；
//! - `GET /api/v1/status`：进程/统计/健康摘要（JSON，`schema: "v1"`）；
//! - `GET /api/v1/upstreams`：上游逐条状态（JSON 数组，索引=配置顺序，不含认证信息）；
//! - `POST /api/v1/control`：唯一写端点，命令见 [`execute`]。
//!
//! 安全默认（ADR-001）：默认仅监听回环地址；绑定非回环只告警不拒绝；
//! 响应上限由配置 `stateMaxResponseBytes` 控制；每个控制命令记一条审计日志。
//!
//! 本模块不提供 `close_conn` / `close_all` / `reload_config`（需要连接表或热重载，
//! 见 docs/10 §5 与 07 U5），对未知命令返回 `unsupported`。

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
    let addr = format!("{}:{}", bind.host, bind.port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            warn!(%addr, error = %e, "控制面绑定失败（不影响主服务）");
            return;
        }
    };
    let local = listener.local_addr().ok();
    info!(
        local = ?local,
        "控制面已就绪（无鉴权，建议仅回环监听；ADR-001）"
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
                        async move { handle(req, ctx, peer, max_response).await }
                    });
                    let conn = http1::Builder::new().serve_connection(io, service);
                    if tokio::time::timeout(CONN_TIMEOUT, conn).await.is_err() {
                        warn!(%peer, "控制面连接超时");
                    }
                });
            }
        }
    }
    info!("控制面已停止");
}

/// 路由与请求处理。
async fn handle(
    req: Request<hyper::body::Incoming>,
    ctx: Arc<AppCtx>,
    peer: SocketAddr,
    max_response: usize,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();

    let result = match (&method, path.as_str()) {
        (&Method::GET, "/healthz") => Ok(text(StatusCode::OK, "ok")),
        (&Method::GET, "/api/v1/status") => json_response(status_json(&ctx), max_response),
        (&Method::GET, "/api/v1/upstreams") => json_response(upstreams_json(&ctx), max_response),
        (&Method::POST, "/api/v1/control") => control(&ctx, req, peer, max_response).await,
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
/// 支持的命令（精简集，ADR-001）：
/// - `check_upstream`：立即探测一次（`args.index` 可选，缺省=全部）；
/// - `enable_upstream` / `disable_upstream`：人工启停（`args.index` 或 `args.name`）；
/// - `set_rule`：切换选路规则（`args.rule`）。
fn execute(ctx: &Arc<AppCtx>, cmd: &str, args: &Value) -> (bool, String) {
    match cmd {
        "check_upstream" => match resolve_index(ctx, args) {
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
        },
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
            let rule = args.get("rule").and_then(Value::as_str).unwrap_or("");
            match SelectRule::parse(rule) {
                Some(rule) => {
                    ctx.selector.set_rule(rule);
                    (true, format!("选路规则已切换为 {}", rule.as_str()))
                }
                None => (false, format!("未知规则: {rule}")),
            }
        }
        _ => (
            false,
            format!(
                "不支持的命令: {cmd}（支持 check_upstream / enable_upstream / disable_upstream / set_rule）"
            ),
        ),
    }
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

/// `/api/v1/upstreams`：JSON 数组，索引=配置顺序，**不含认证信息**。
fn upstreams_json(ctx: &Arc<AppCtx>) -> Value {
    let health = ctx.health.snapshot();
    let list: Vec<Value> = ctx
        .selector
        .upstreams()
        .iter()
        .enumerate()
        .map(|(i, u)| {
            let h = health.get(i);
            let addr = u.addr.to_string();
            json!({
                "index": i,
                "name": u.name,
                "host": u.addr.ip().to_string(),
                "port": u.addr.port(),
                "addr": addr,
                "enabled": ctx.selector.manual_enabled(i),
                "available": h.map(|h| h.available).unwrap_or(false),
                "liveness": h.map(|h| h.liveness).unwrap_or(false),
                "readiness": h.map(|h| h.readiness).unwrap_or(false),
                "failures": h.map(|h| h.failures).unwrap_or(0),
                "tlsFailures": h.map(|h| h.tls_failures).unwrap_or(0),
                "backoffMs": h.map(|h| h.backoff_ms).unwrap_or(0),
            })
        })
        .collect();
    Value::Array(list)
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
                { "name": "b", "host": "127.0.0.1", "port": 6101 }
            ]
        }"#;
        let cfg = Arc::new(config::load_str(text).expect("配置合法"));
        let upstreams: Vec<Upstream> = cfg
            .upstreams
            .iter()
            .enumerate()
            .map(|(i, u)| Upstream {
                name: u.name.clone(),
                addr: format!("{}:{}", u.host, u.port).parse().unwrap(),
                auth_user: u.auth_user.clone(),
                auth_pwd: u.auth_pwd.clone(),
                health_index: i,
            })
            .collect();
        let selector = Arc::new(UpstreamSelector::new(
            upstreams.clone(),
            cfg.select_rule,
            cfg.server_change_time,
        ));
        let health = HealthRegistry::new(&upstreams, cfg.health.clone());
        let stats = Stats::new();
        AppCtx::new(cfg, selector, health, stats, CancellationToken::new())
    }

    #[test]
    fn status_含_schema_与统计字段() {
        let ctx = test_ctx();
        let v = status_json(&ctx);
        assert_eq!(v["schema"], "v1");
        assert!(v["stats"]["accepted"].is_u64());
        assert_eq!(v["health"]["total"], 2);
        assert_eq!(v["listen"][0], "127.0.0.1:5500");
    }

    #[test]
    fn upstreams_不含密码与认证字段() {
        let ctx = test_ctx();
        let v = upstreams_json(&ctx);
        let text = v.to_string();
        assert!(v.is_array());
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert!(!text.contains("secret"), "响应不得包含密码: {text}");
        assert!(!text.contains("authUser"), "响应不得包含认证字段: {text}");
        assert!(!text.contains("alice"), "响应不得包含用户名: {text}");
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
    fn 人工启停命令生效() {
        let ctx = test_ctx();
        let (ok, _) = execute(&ctx, "disable_upstream", &json!({"index": 0}));
        assert!(ok);
        assert!(!ctx.selector.manual_enabled(0));
        let (ok, _) = execute(&ctx, "enable_upstream", &json!({"name": "a"}));
        assert!(ok);
        assert!(ctx.selector.manual_enabled(0));
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
        let (ok, detail) = execute(&ctx, "close_all", &Value::Null);
        assert!(!ok);
        assert!(detail.contains("不支持"));
    }
}
