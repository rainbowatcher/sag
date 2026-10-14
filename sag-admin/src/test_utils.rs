//! Shared test helpers for the admin crate.
//!
//! Previously `build_state` / `test_app` / `one_provider_config` were
//! copy-pasted across `admin_providers.rs`, `admin_routes.rs`, and
//! `admin_logs.rs`. This module holds a single canonical copy.
#![cfg(test)]

use std::collections::HashMap;

use axum::Router;
use sag_config::{
    ApiConfig, ApiKind, ConfigHandle, GatewayConfig, KeyStrategy, Pipeline, ProviderConfig,
    ProxyMode, RouteConfig, RouteTarget, Strategy,
};
use sag_state::AppState;

/// Build an in-memory `AppState` with the given providers and routes.
/// The runtime provider registry is pre-populated with `OpenAiProvider`
/// adapters for every provider in the config.
pub(crate) async fn build_state(
    providers: HashMap<String, ProviderConfig>,
    routes: Vec<RouteConfig>,
) -> AppState {
    build_state_with_proxy(providers, routes, None).await
}

/// Build an in-memory `AppState` with the given providers, routes, and an
/// optional gateway-level `proxy_url`. Same as [`build_state`] but lets
/// tests exercise proxy resolution paths.
pub(crate) async fn build_state_with_proxy(
    providers: HashMap<String, ProviderConfig>,
    routes: Vec<RouteConfig>,
    proxy_url: Option<String>,
) -> AppState {
    let config = GatewayConfig {
        providers,
        routes,
        proxy_url,
        ..sag_config::gateway_config_base!()
    };
    let handle = ConfigHandle::from_config(config);
    let provider_registry = sag_state::testing::build_provider_registry(&handle, false).await;
    sag_state::testing::test_app_state(handle, provider_registry).await
}

/// Build an `AppState` whose config handle is **file-backed** at `path`
/// (so `mutate` takes the persist/conflict path). Seeds a minimal but
/// fully-valid TOML document (`[server]` + providers + routes) that
/// `load_config` accepts — `save_config_to_file` alone cannot create
/// `[server]` on a blank path.
pub(crate) async fn build_file_backed_state(
    path: &std::path::Path,
    providers: HashMap<String, ProviderConfig>,
    routes: Vec<RouteConfig>,
) -> AppState {
    let config = GatewayConfig {
        providers,
        routes,
        ..sag_config::gateway_config_base!()
    };
    // `toml` re-serializes the whole struct (fine for a seed file; the
    // in-place `toml_edit` save path is not exercised here).
    let seed = toml::to_string(&config).expect("config must serialize to TOML");
    std::fs::write(path, seed).expect("seed config must write");
    let handle = sag_config::load_config(path).expect("config must load from path");
    let provider_registry = sag_state::testing::build_provider_registry(&handle, false).await;
    sag_state::testing::test_app_state(handle, provider_registry).await
}

/// Build a test Axum app mounting admin routes without auth middleware.
pub(crate) fn test_app(state: AppState) -> Router {
    Router::new()
        .nest("/api", crate::admin::build_admin_routes(state.clone()))
        .with_state(state)
}

/// Build a provider declaring exactly one API section `kind` at `base_url`.
///
/// Everything else is `ProviderConfig::default()` plus the shared test
/// defaults (FillFirst key strategy, default proxy mode, 120s timeout).
/// Tests that need a second section or an override patch the returned
/// `apis` map themselves.
pub(crate) fn test_provider(
    kind: ApiKind,
    base_url: &str,
    api_keys: Vec<String>,
    enabled_models: Vec<String>,
) -> ProviderConfig {
    let mut pc = ProviderConfig {
        api_keys,
        key_strategy: KeyStrategy::FillFirst,
        proxy_mode: ProxyMode::default(),
        timeout_secs: 120,
        enabled_models,
        ..ProviderConfig::default()
    };
    pc.apis.insert(
        kind,
        ApiConfig {
            base_url: base_url.into(),
            ..ApiConfig::default()
        },
    );
    pc
}

/// One captured upstream request from a mock server (see
/// [`spawn_upstream_capture_many`] / [`spawn_upstream_capture_scripted`]).
#[derive(Debug, Clone)]
pub(crate) struct CapturedRequest {
    pub method: String,
    pub path: String,
    /// Header names lowercased for case-insensitive assertions.
    pub headers: HashMap<String, String>,
    /// Parsed JSON body (`Value::Null` when the body is not JSON).
    pub body: serde_json::Value,
}

/// Find the first occurrence of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Read one full HTTP request (headers + `Content-Length` body) off `stream`.
async fn read_http_request(
    stream: &mut tokio::net::TcpStream,
) -> Option<(String, String, HashMap<String, String>, serde_json::Value)> {
    use tokio::io::AsyncReadExt;

    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body_bytes = buf[header_end + 4..].to_vec();
    while body_bytes.len() < content_length {
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            break;
        }
        body_bytes.extend_from_slice(&tmp[..n]);
    }
    let body = serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);

    Some((method, path, headers, body))
}

/// Serialize a minimal HTTP/1.1 response with `Connection: close`.
fn http_response(status: u16, body: &str) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// Handle one accepted connection: capture the request, then answer with
/// `(status, body)`.
async fn serve_captured(
    mut stream: tokio::net::TcpStream,
    tx: tokio::sync::mpsc::UnboundedSender<CapturedRequest>,
    status: u16,
    body: &str,
) {
    use tokio::io::AsyncWriteExt;

    if let Some((method, path, headers, parsed)) = read_http_request(&mut stream).await {
        let _ = tx.send(CapturedRequest {
            method,
            path,
            headers,
            body: parsed,
        });
        let _ = stream.write_all(&http_response(status, body)).await;
        let _ = stream.flush().await;
    }
}

/// Spawn a mock upstream that answers **every** connection with the same
/// `(status, body)` and reports each captured request on the returned
/// channel. Returns `(base_url, rx)`.
pub(crate) async fn spawn_upstream_capture_many(
    status: u16,
    body: &'static str,
) -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<CapturedRequest>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let tx = tx.clone();
            tokio::spawn(serve_captured(stream, tx, status, body));
        }
    });
    (format!("http://{addr}"), rx)
}

/// Spawn a mock upstream whose *i*-th connection answers
/// `responses[i]` (repeating the last entry once the script is exhausted).
/// Captured requests are reported in connection order.
pub(crate) async fn spawn_upstream_capture_scripted(
    responses: Vec<(u16, &'static str)>,
) -> (
    String,
    tokio::sync::mpsc::UnboundedReceiver<CapturedRequest>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut idx = 0usize;
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let i = idx.min(responses.len().saturating_sub(1));
            let (status, body) = responses[i];
            idx += 1;
            let tx = tx.clone();
            tokio::spawn(serve_captured(stream, tx, status, body));
        }
    });
    (format!("http://{addr}"), rx)
}

/// Convenience: create a single-provider config with optional routes.
/// Returns `(providers, routes)` ready for [`build_state`].
///
/// The provider declares a `chat_completions` section; the routes are
/// chat-pipeline routes (no `api_endpoint` — that field is image-only in
/// the new config model).
pub(crate) fn one_provider_config(
    name: &str,
    routes: Vec<(&str, &str, u32)>,
) -> (HashMap<String, ProviderConfig>, Vec<RouteConfig>) {
    let mut providers = HashMap::new();
    providers.insert(
        name.to_string(),
        test_provider(
            ApiKind::ChatCompletions,
            "https://api.openai.com/v1",
            vec!["sk-test".into()],
            vec![],
        ),
    );
    let routes = routes
        .into_iter()
        .map(|(model, real_model, prio)| RouteConfig {
            model_name: model.into(),
            pipeline: Pipeline::Chat,
            api_endpoint: None,
            retries: None,
            timeout_secs: None,
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                provider: name.into(),
                real_model: Some(real_model.into()),
                retries: None,
                timeout_secs: None,
                priority: prio,
                extra: HashMap::new(),
                capabilities: None,
            }],
        })
        .collect();
    (providers, routes)
}
