//! TLS + HTTP readiness 探测（M1.5，对齐 C++ `ConnectTestHttps` / ADR-006）。
//!
//! 流程：上游 SOCKS5 CONNECT（域名交上游解析）→ TLS 握手（系统根证书校验）→
//! HTTP GET（hyper）→ 校验状态码。任一环节失败即 readiness 不满足。
//!
//! 与 C++ 版对齐点：TLS 证书校验（不开 `verify_none`）、可配 SNI/Host/User-Agent/期望状态码、
//! 探测目标与周期/超时全部来自配置。

use std::sync::Arc;

use http_body_util::BodyExt;
use http_body_util::Empty;
use hyper::Request;
use hyper::body::Bytes;
use hyper::header::{HOST, USER_AGENT};
use hyper_util::rt::TokioIo;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use rustls_platform_verifier::ConfigVerifierExt;
use tokio_rustls::TlsConnector;
use tokio_socks::tcp::Socks5Stream;

use crate::config::TlsProbeConfig;
use crate::upstream::Upstream;

/// 一次 readiness 探测的结果（ADR-006 #1：读完整响应并记录 RTT）。
#[derive(Debug, Clone, Copy)]
pub struct ProbeOutcome {
    /// HTTP 响应状态码。
    pub status: u16,
    /// 从发出请求到读完响应体的往返耗时。
    pub rtt: std::time::Duration,
}

/// 探测失败原因（便于日志分类）。
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("上游 SOCKS5 握手失败: {0}")]
    Socks(#[from] tokio_socks::Error),
    #[error("TLS 握手失败: {0}")]
    Tls(#[from] rustls::Error),
    #[error("I/O 错误: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP 请求失败: {0}")]
    Http(String),
    #[error("探测目标非法: {0}")]
    InvalidTarget(String),
}

/// 通过上游建立到探测目标的隧道，完成 TLS + HTTP GET，返回状态码与 RTT。
pub async fn probe(upstream: &Upstream, cfg: &TlsProbeConfig) -> Result<ProbeOutcome, ProbeError> {
    // 1) 上游 SOCKS5 隧道（域名交给上游解析）
    let target = tokio_socks::TargetAddr::Domain(cfg.host.clone().into(), cfg.port);
    let stream = match upstream_creds(upstream) {
        Some((user, pwd)) => {
            Socks5Stream::connect_with_password(upstream.addr(), target, &user, &pwd).await?
        }
        None => Socks5Stream::connect(upstream.addr(), target).await?,
    };

    // 2) TLS（默认系统根证书；配置 caFile 时用自定义信任根）
    let client_config = build_client_config(cfg)?;
    let connector = TlsConnector::from(Arc::new(client_config));
    let server_name = ServerName::try_from(cfg.sni.clone())
        .map_err(|e| ProbeError::InvalidTarget(format!("SNI {:?}: {e}", cfg.sni)))?;
    let tls_stream = connector.connect(server_name, stream).await?;

    // 3) HTTP/1.1 GET（hyper）
    let io = TokioIo::new(tls_stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| ProbeError::Http(format!("HTTP 握手失败: {e}")))?;
    let conn_task = tokio::spawn(async move {
        let _ = conn.await;
    });

    // HTTP/1.1 请求行使用 origin-form（只写路径），Host 头单独设置
    let path = if cfg.http_path.starts_with('/') {
        cfg.http_path.clone()
    } else {
        format!("/{}", cfg.http_path)
    };
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .header(HOST, host_header(&cfg.host, cfg.port))
        .header(USER_AGENT, &cfg.user_agent)
        .header("Connection", "close")
        .body(Empty::<Bytes>::new())
        .map_err(|e| ProbeError::Http(format!("构造请求失败: {e}")))?;

    let started = std::time::Instant::now();
    let response = sender
        .send_request(request)
        .await
        .map_err(|e| ProbeError::Http(format!("发送请求失败: {e}")))?;
    let status = response.status().as_u16();
    // ADR-006 #1：读完整响应体（只关心"能否完成一次 HTTP 往返"，内容本身不校验）。
    // 整个 probe 由调用方的 `timeout(tls.timeout, ...)` 兜底，异常大/慢响应会超时失败。
    let body = response
        .into_body()
        .collect()
        .await
        .map_err(|e| ProbeError::Http(format!("读取响应体失败: {e}")))?;
    let _ = body.to_bytes();
    let rtt = started.elapsed();
    conn_task.abort();
    Ok(ProbeOutcome { status, rtt })
}

/// 构建 TLS 客户端配置：默认系统根证书；`caFile` 指定时使用自定义 PEM 信任根。
fn build_client_config(cfg: &TlsProbeConfig) -> Result<ClientConfig, ProbeError> {
    let Some(ca_file) = &cfg.ca_file else {
        return Ok(ClientConfig::with_platform_verifier()?);
    };
    let pem = std::fs::read(ca_file).map_err(|e| {
        ProbeError::InvalidTarget(format!("读取 caFile {} 失败: {e}", ca_file.display()))
    })?;
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut &pem[..]) {
        let cert = cert.map_err(|e| {
            ProbeError::InvalidTarget(format!("解析 caFile {} 失败: {e}", ca_file.display()))
        })?;
        roots
            .add(cert)
            .map_err(|e| ProbeError::InvalidTarget(format!("加入信任根失败: {e}")))?;
    }
    if roots.is_empty() {
        return Err(ProbeError::InvalidTarget(format!(
            "caFile {} 中没有任何证书",
            ca_file.display()
        )));
    }
    Ok(ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}

fn upstream_creds(up: &Upstream) -> Option<(String, String)> {
    up.auth_user
        .as_deref()
        .filter(|u| !u.trim().is_empty())
        .map(|u| (u.to_string(), up.auth_pwd.clone().unwrap_or_default()))
}

fn host_header(host: &str, port: u16) -> String {
    if port == 443 {
        host.to_string()
    } else {
        format!("{host}:{port}")
    }
}

/// 状态码判定：`expect_status` 为空时**不校验状态码**（只要完成一次 HTTP 往返即通过，
/// 对齐 C++ `ConnectTestHttps` 与 ADR-006 #5）；否则精确匹配列表。
pub fn status_acceptable(status: u16, expect_status: &[u16]) -> bool {
    expect_status.is_empty() || expect_status.contains(&status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 状态码默认为不校验() {
        // 空 expectStatus = 不校验：4xx/5xx 也说明 HTTP 往返成功
        for status in [200, 204, 301, 399, 404, 500, 503] {
            assert!(
                status_acceptable(status, &[]),
                "status={status} 在空 expectStatus 下应通过"
            );
        }
    }

    #[test]
    fn 状态码精确匹配配置列表() {
        assert!(status_acceptable(204, &[200, 204]));
        assert!(!status_acceptable(200, &[204]));
    }

    #[test]
    fn host_头默认省略_443() {
        assert_eq!(host_header("example.com", 443), "example.com");
        assert_eq!(host_header("example.com", 8443), "example.com:8443");
    }
}
