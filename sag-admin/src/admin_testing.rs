//! Provider testing utilities: discover available models on a provider
//! (`fetch_models`) and verify a specific model is reachable with a given
//! key (`model_test`). These are unauthenticated-by-default convenience
//! endpoints that take a temporary provider config and short-lived
//! reqwest client, so callers don't need a registered provider entry
//! to test connectivity.

use std::collections::HashMap;

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use reqwest::header::CONTENT_TYPE;
use sag_config::{ApiKind, ProxyMode};
use sag_core::AppError;
use sag_providers::{provider_auth_headers, provider_chat_endpoint};
use serde_json::json;
use tracing::{instrument, warn};

use sag_state::AppState;

/// Which API section this temporary probe talks to — selects the auth
/// header shape (`anthropic_messages` → `x-api-key`, everything else →
/// `Authorization: Bearer`) and the chat endpoint path. Defaults to
/// `chat_completions`, matching the old `provider_type` default of OpenAI.
fn api_kind_of(kind: Option<ApiKind>) -> ApiKind {
    kind.unwrap_or(ApiKind::ChatCompletions)
}

#[derive(serde::Deserialize)]
pub struct FetchModelsRequest {
    /// Temporary probe address: the `base_url` of whichever `apis.<kind>`
    /// section the caller is testing (providers no longer carry a
    /// provider-level `base_url`).
    pub base_url: String,
    #[serde(default)]
    pub api_keys: Vec<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// API section whose auth style applies (default `chat_completions`).
    #[serde(default)]
    pub api_kind: Option<ApiKind>,
    /// Custom endpoint path for listing models (default: `models`).
    /// When empty, falls back to `models` appended to `base_url`.
    /// Equivalent to `apis.<kind>.models_endpoint` on a saved provider.
    #[serde(default)]
    pub models_endpoint: String,
    /// Proxy policy for this temporary request only (default `inherit`):
    /// resolved against the **current in-memory** gateway proxy and never
    /// persisted. `enabled` without a global proxy is a 400; `disabled`
    /// forces a direct connection ignoring the global/system proxy.
    #[serde(default)]
    pub proxy_mode: ProxyMode,
}

fn build_models_url(base_url: &str, models_endpoint: &str) -> String {
    let endpoint = sag_config::normalize_models_endpoint(models_endpoint);
    format!("{}/{}", base_url.trim_end_matches('/'), endpoint)
}

/// Resolve the temporary test request's proxy policy against the **current
/// in-memory** gateway config (never the file, never a saved provider's
/// mode). Errors — a malformed in-memory global URL or `enabled` mode with
/// no global proxy — are surfaced as 400-style [`AppError::BadRequest`]
/// per the temporary-test API contract.
async fn resolve_test_proxy(state: &AppState, mode: ProxyMode) -> Result<Option<String>, AppError> {
    let global = state
        .config
        .read()
        .await
        .normalized_global_proxy()
        .map_err(AppError::BadRequest)?;
    let effective =
        sag_config::resolve_effective_proxy(mode, global.as_ref()).map_err(AppError::BadRequest)?;
    Ok(effective.map(|p| p.to_connect_url()))
}

/// Build the short-lived reqwest client for a temporary test request,
/// honoring the resolved proxy policy: `None` → explicit direct connection
/// (system proxy disabled), `Some(connect_url)` → that single explicit
/// proxy. Never falls back to reqwest's default system-proxy discovery.
fn build_test_client(connect_url: Option<&str>) -> Result<reqwest::Client, AppError> {
    sag_core::build_http_client(connect_url, Some(std::time::Duration::from_secs(30)))
        .map_err(|e| AppError::Internal(format!("failed to build http client: {e}")))
}

/// POST /api/model/test — test a specific model by sending a real chat completion.
/// POST /api/providers/fetch-models — fetch upstream models using a temporary provider config.
#[instrument(skip(state, req), fields(url))]
pub async fn fetch_models(
    State(state): State<AppState>,
    Json(req): Json<FetchModelsRequest>,
) -> impl IntoResponse {
    let Some(api_key) = req.api_keys.first() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": { "code": "MISSING_API_KEYS", "message": "at least one api_key required" }
            })),
        )
            .into_response();
    };

    let models_url = build_models_url(&req.base_url, &req.models_endpoint);
    tracing::Span::current().record("url", &models_url);

    let connect_url = match resolve_test_proxy(&state, req.proxy_mode).await {
        Ok(url) => url,
        Err(e) => return e.into_response(),
    };
    let client = match build_test_client(connect_url.as_deref()) {
        Ok(client) => client,
        Err(e) => {
            warn!(?e, "failed to build temporary test http client");
            return e.into_response();
        }
    };
    let http_req = client.get(&models_url);

    let api_kind = api_kind_of(req.api_kind);
    let auth_headers = provider_auth_headers(api_kind, api_key);
    let extra_refs: Vec<(&str, &str)> =
        auth_headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let http_req = sag_providers::apply_headers(http_req, &extra_refs, &req.headers);

    match http_req.send().await {
        Ok(resp) => {
            if !resp.status().is_success() {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "error": {
                            "code": "UPSTREAM_ERROR",
                            "message": format!("upstream returned status {}", resp.status()),
                        }
                    })),
                )
                    .into_response();
            }

            match resp.json::<serde_json::Value>().await {
                Ok(mut body) => {
                    let models = body
                        .get_mut("data")
                        .map(std::mem::take)
                        .unwrap_or(serde_json::Value::Array(vec![]));

                    Json(json!({ "models": models })).into_response()
                }
                Err(e) => {
                    warn!(?e, "failed to parse upstream models response");
                    (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({
                            "error": {
                                "code": "PARSE_ERROR",
                                "message": format!("failed to parse upstream response: {e}"),
                            }
                        })),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            warn!(?e, "failed to reach upstream models endpoint");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": {
                        "code": "CONNECTION_ERROR",
                        "message": format!("failed to connect to upstream: {e}"),
                    }
                })),
            )
                .into_response()
        }
    }
}

#[derive(serde::Deserialize)]
pub struct ModelTestRequest {
    pub model: String,
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// Temporary probe address (the `base_url` of the `apis.<kind>`
    /// section under test).
    pub base_url: String,
    /// API section under test: picks the auth header shape and the chat
    /// endpoint path (default `chat_completions`).
    #[serde(default)]
    pub api_kind: Option<ApiKind>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Proxy policy for this temporary request only (default `inherit`):
    /// resolved against the **current in-memory** gateway proxy and never
    /// persisted. `enabled` without a global proxy is a 400; `disabled`
    /// forces a direct connection ignoring the global/system proxy.
    #[serde(default)]
    pub proxy_mode: ProxyMode,
}

pub async fn model_test(
    State(state): State<AppState>,
    Json(req): Json<ModelTestRequest>,
) -> impl IntoResponse {
    let connect_url = match resolve_test_proxy(&state, req.proxy_mode).await {
        Ok(url) => url,
        Err(e) => return e.into_response(),
    };
    let client = match build_test_client(connect_url.as_deref()) {
        Ok(client) => client,
        Err(e) => {
            warn!(?e, "failed to build temporary test http client");
            return e.into_response();
        }
    };

    if req.api_keys.is_empty() {
        return Json(json!({ "results": [] })).into_response();
    }

    let mut results = Vec::new();

    for key in &req.api_keys {
        let result = test_single_key(&client, &req, key).await;
        let prefix = if key.len() > 12 {
            format!("{}…", &key[..12])
        } else {
            key.clone()
        };
        results.push(json!({
            "key_prefix": prefix,
            "success": result.0,
            "message": result.1,
        }));
    }

    Json(json!({ "results": results })).into_response()
}

// ── Protocol support probing (Phase B) ────────────────────────────

/// Verdict of one (protocol × base_url × key) probe attempt.
///
/// Maps the plan's verdict table: 2xx → supported, 401/403 → auth_failed
/// (path exists — not "unsupported"), 404 → uncertain (body read to tell
/// `model_not_found` from an unknown route), 405 → unsupported, other 4xx
/// → partial (path exists, schema/params rejected). `Error` is
/// transport-level (connect/timeout) — no HTTP verdict was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeVerdict {
    Supported,
    AuthFailed,
    Uncertain,
    Unsupported,
    Partial,
    Error,
}

impl ProbeVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::AuthFailed => "auth_failed",
            Self::Uncertain => "uncertain",
            Self::Unsupported => "unsupported",
            Self::Partial => "partial",
            Self::Error => "error",
        }
    }
}

/// Whether a 404 body indicates a *model* problem (endpoint exists) rather
/// than an unknown route: OpenAI's `model_not_found` error code, or a
/// "model … not found / does not exist" message (Anthropic's
/// `model: x: not found` shape).
fn body_looks_like_model_not_found(body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    if b.contains("model_not_found") {
        return true;
    }
    b.contains("model")
        && (b.contains("does not exist") || b.contains("not found") || b.contains("not_found"))
}

/// Pure verdict table: HTTP status + response body → (verdict, message).
/// Split out for unit testing without a network mock.
fn verdict_from_status_body(status: u16, body: &str) -> (ProbeVerdict, String) {
    match status {
        200..=299 => (ProbeVerdict::Supported, "OK".into()),
        401 | 403 => (
            ProbeVerdict::AuthFailed,
            format!("path exists but authentication failed: {body}"),
        ),
        404 => {
            if body_looks_like_model_not_found(body) {
                (
                    ProbeVerdict::Uncertain,
                    format!("endpoint responded — model_not_found (path likely exists): {body}"),
                )
            } else {
                (
                    ProbeVerdict::Uncertain,
                    format!("not found — unknown route or missing model: {body}"),
                )
            }
        }
        405 => (
            ProbeVerdict::Unsupported,
            format!("method not allowed — path does not serve this protocol: {body}"),
        ),
        400..=499 => (
            ProbeVerdict::Partial,
            format!("path exists but request rejected (schema/params): {body}"),
        ),
        _ => (
            ProbeVerdict::Uncertain,
            format!("unexpected status {status}: {body}"),
        ),
    }
}

#[derive(serde::Deserialize)]
pub struct ProbeTarget {
    pub protocol: ApiKind,
    pub base_url: String,
    /// Per-target extra headers (merged over the request-level `headers`,
    /// simulating `apis.<protocol>.headers`).
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

#[derive(serde::Deserialize)]
pub struct ProbeProtocolsRequest {
    #[serde(default)]
    pub api_keys: Vec<String>,
    pub model: String,
    /// Fixed set of three probeable protocols (`image` is rejected with a 400).
    #[serde(default)]
    pub targets: Vec<ProbeTarget>,
    /// Extra headers applied to every target (simulate `apis.*.headers`).
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Proxy policy for these temporary requests only (default `inherit`);
    /// resolved against the current in-memory gateway proxy, never persisted.
    #[serde(default)]
    pub proxy_mode: ProxyMode,
}

/// Minimal probe body per protocol — output tokens are the expensive part,
/// so both counts are pinned to the smallest accepted values
/// (`max_tokens: 1` for chat/anthropic; `max_output_tokens: 16` for
/// responses, some vendors enforce that floor).
fn probe_body(protocol: ApiKind, model: &str) -> serde_json::Value {
    match protocol {
        ApiKind::Responses => json!({
            "model": model,
            "input": "hi",
            "max_output_tokens": 16,
        }),
        _ => json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1,
        }),
    }
}

/// Probe URL = `base_url` + the fixed protocol path
/// (`sag_core::protocol_path`). `None` only for non-protocol kinds.
fn probe_url(base_url: &str, protocol: ApiKind) -> Option<String> {
    sag_core::protocol_path(protocol)
        .map(|path| format!("{}/{}", base_url.trim_end_matches('/'), path))
}

fn key_prefix(key: &str) -> String {
    if key.len() > 12 {
        format!("{}…", &key[..12])
    } else {
        key.to_string()
    }
}

/// Send one minimal probe request for (protocol × key) and classify the
/// outcome. Returns `(status, verdict, message)` — `status` is `None` on
/// transport failure.
async fn probe_one(
    client: &reqwest::Client,
    target: &ProbeTarget,
    url: &str,
    req: &ProbeProtocolsRequest,
    api_key: &str,
) -> (Option<u16>, ProbeVerdict, String) {
    let body = probe_body(target.protocol, &req.model);

    let auth = provider_auth_headers(target.protocol, api_key);
    let auth_refs: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();

    // Request-level headers first, per-target headers win on conflicts —
    // mirrors the config layering (provider defaults vs `apis.<key>`).
    let mut headers = req.headers.clone();
    for (k, v) in &target.headers {
        headers.insert(k.clone(), v.clone());
    }

    let http_req = sag_providers::apply_headers(client.post(url), &auth_refs, &headers).json(&body);

    match http_req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            let (verdict, message) = verdict_from_status_body(status, &text);
            (Some(status), verdict, message)
        }
        Err(e) => {
            let message = if e.is_timeout() {
                "request timed out after 30s".to_string()
            } else {
                format!("connection failed: {e}")
            };
            (None, ProbeVerdict::Error, message)
        }
    }
}

/// POST /api/providers/probe-protocols — probe which of the three chat-family
/// protocols each (base_url × key) actually serves. Read-only: the probe
/// **never writes gateway.toml**; the UI backfills the form and the user
/// saves through the existing `PUT /api/providers` path.
#[instrument(skip(state, req), fields(model))]
pub async fn probe_protocols(
    State(state): State<AppState>,
    Json(req): Json<ProbeProtocolsRequest>,
) -> impl IntoResponse {
    // Typed enum semantics are strict: `image` is not probeable (its path
    // lives on the route, not the protocol catalog).
    for target in &req.targets {
        if !target.protocol.is_protocol() {
            return AppError::BadRequest(format!(
                "protocol \"{}\" is not probeable — fixed set: chat_completions / responses / anthropic_messages",
                target.protocol.as_str()
            ))
            .into_response();
        }
    }

    let connect_url = match resolve_test_proxy(&state, req.proxy_mode).await {
        Ok(url) => url,
        Err(e) => return e.into_response(),
    };
    let client = match build_test_client(connect_url.as_deref()) {
        Ok(client) => client,
        Err(e) => {
            warn!(?e, "failed to build temporary probe http client");
            return e.into_response();
        }
    };

    let mut results = Vec::new();
    for target in &req.targets {
        // `is_protocol()` above guarantees a path exists.
        let Some(url) = probe_url(&target.base_url, target.protocol) else {
            continue;
        };
        for key in &req.api_keys {
            let (status, verdict, message) = probe_one(&client, target, &url, &req, key).await;
            results.push(json!({
                "protocol": target.protocol.as_str(),
                "url": url,
                "key_prefix": key_prefix(key),
                "status": status,
                "message": message,
                "verdict": verdict.as_str(),
            }));
        }
    }

    Json(json!({ "results": results })).into_response()
}

async fn test_single_key(
    client: &reqwest::Client,
    req: &ModelTestRequest,
    api_key: &str,
) -> (bool, String) {
    let api_kind = api_kind_of(req.api_kind);
    let endpoint = provider_chat_endpoint(api_kind);
    let url = format!(
        "{}/{}",
        req.base_url.trim_end_matches('/'),
        endpoint.trim_start_matches('/')
    );

    let body = serde_json::json!({
        "model": req.model,
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1
    });

    let mut http_req = client.post(&url).header(CONTENT_TYPE, "application/json");

    for (name, value) in provider_auth_headers(api_kind, api_key) {
        http_req = http_req.header(name, value);
    }

    for (key, value) in &req.headers {
        http_req = http_req.header(key.as_str(), value.as_str());
    }

    http_req = http_req.json(&body);

    match http_req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if (200..300).contains(&status) {
                (true, "OK".into())
            } else if status == 401 || status == 403 {
                let body = resp.text().await.unwrap_or_default();
                (false, format!("Authentication failed: {body}"))
            } else if status == 404 {
                let body = resp.text().await.unwrap_or_default();
                (false, format!("Model '{}' not found: {body}", req.model))
            } else {
                let body = resp.text().await.unwrap_or_default();
                (false, format!("upstream returned status {status}: {body}"))
            }
        }
        Err(e) => {
            if e.is_timeout() {
                (false, "request timed out after 30s".into())
            } else {
                (false, format!("connection failed: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{build_state_with_proxy, test_app};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use std::collections::HashMap;
    use tower::ServiceExt;

    #[test]
    fn build_models_url_normalizes_endpoint_path() {
        assert_eq!(
            build_models_url("https://example.com/v1", "models"),
            "https://example.com/v1/models"
        );
        assert_eq!(
            build_models_url("https://example.com/v1/", "/models"),
            "https://example.com/v1/models"
        );
        assert_eq!(
            build_models_url("https://example.com/v1", "v1/models/"),
            "https://example.com/v1/v1/models"
        );
    }

    // ── proxy_mode support ────────────────────────────────────────────

    #[test]
    fn fetch_models_request_defaults_proxy_mode_to_inherit() {
        let req: FetchModelsRequest =
            serde_json::from_str(r#"{"base_url":"https://example.com/v1"}"#).unwrap();
        assert_eq!(req.proxy_mode, ProxyMode::Inherit);

        let req: FetchModelsRequest = serde_json::from_str(
            r#"{"base_url":"https://example.com/v1","proxy_mode":"disabled"}"#,
        )
        .unwrap();
        assert_eq!(req.proxy_mode, ProxyMode::Disabled);
    }

    #[test]
    fn model_test_request_defaults_proxy_mode_to_inherit() {
        let req: ModelTestRequest =
            serde_json::from_str(r#"{"model":"gpt-4o","base_url":"https://x.com"}"#).unwrap();
        assert_eq!(req.proxy_mode, ProxyMode::Inherit);

        let req: ModelTestRequest = serde_json::from_str(
            r#"{"model":"gpt-4o","base_url":"https://x.com","proxy_mode":"enabled"}"#,
        )
        .unwrap();
        assert_eq!(req.proxy_mode, ProxyMode::Enabled);
    }

    async fn state_with_global_proxy(proxy: Option<&str>) -> sag_state::AppState {
        build_state_with_proxy(HashMap::new(), vec![], proxy.map(str::to_string)).await
    }

    // ── probe verdict table ───────────────────────────────────────────

    #[test]
    fn verdict_2xx_is_supported() {
        for status in [200u16, 201, 204] {
            let (verdict, message) = verdict_from_status_body(status, "");
            assert_eq!(verdict, ProbeVerdict::Supported, "status {status}");
            assert_eq!(message, "OK");
        }
    }

    #[test]
    fn verdict_401_is_auth_failed() {
        let (verdict, message) = verdict_from_status_body(401, r#"{"error":"invalid key"}"#);
        assert_eq!(verdict, ProbeVerdict::AuthFailed);
        assert!(
            message.contains("authentication failed") && message.contains("invalid key"),
            "message must carry the body: {message}"
        );
    }

    #[test]
    fn verdict_403_is_auth_failed_not_unsupported() {
        // 403 = path exists but forbidden; it must not read as "unsupported".
        let (verdict, _) = verdict_from_status_body(403, "forbidden");
        assert_eq!(verdict, ProbeVerdict::AuthFailed);
    }

    #[test]
    fn verdict_404_model_not_found_is_uncertain_with_model_hint() {
        // OpenAI shape: the endpoint processed the body and rejected the model.
        let body =
            r#"{"error":{"message":"The model 'gpt-x' does not exist","code":"model_not_found"}}"#;
        let (verdict, message) = verdict_from_status_body(404, body);
        assert_eq!(verdict, ProbeVerdict::Uncertain);
        assert!(
            message.contains("model_not_found"),
            "body classification must surface model_not_found: {message}"
        );

        // Anthropic shape: `model: x: not found`.
        let body = r#"{"type":"error","error":{"type":"not_found_error","message":"model: foo: not found"}}"#;
        let (verdict, message) = verdict_from_status_body(404, body);
        assert_eq!(verdict, ProbeVerdict::Uncertain);
        assert!(message.contains("model_not_found"), "{message}");
    }

    #[test]
    fn verdict_404_unknown_route_is_uncertain_with_route_hint() {
        let (verdict, message) = verdict_from_status_body(404, r#"{"detail":"Not Found"}"#);
        assert_eq!(verdict, ProbeVerdict::Uncertain);
        assert!(
            message.contains("unknown route"),
            "body without a model signal must read as unknown route: {message}"
        );
        // The two 404 bodies must classify differently (that is the point
        // of reading the body at all).
        let (_, model_msg) =
            verdict_from_status_body(404, r#"{"error":{"code":"model_not_found"}}"#);
        assert_ne!(message, model_msg);
    }

    #[test]
    fn verdict_405_is_unsupported() {
        let (verdict, message) = verdict_from_status_body(405, "method not allowed");
        assert_eq!(verdict, ProbeVerdict::Unsupported);
        assert!(message.contains("method not allowed"), "{message}");
    }

    #[test]
    fn verdict_other_4xx_is_partial() {
        for status in [400u16, 422, 429] {
            let (verdict, message) = verdict_from_status_body(status, r#"{"error":"bad param"}"#);
            assert_eq!(verdict, ProbeVerdict::Partial, "status {status}");
            assert!(
                message.contains("path exists"),
                "status {status}: {message}"
            );
        }
    }

    #[test]
    fn verdict_5xx_is_uncertain() {
        let (verdict, _) = verdict_from_status_body(502, "bad gateway");
        assert_eq!(verdict, ProbeVerdict::Uncertain);
    }

    #[test]
    fn probe_request_defaults_proxy_mode_to_inherit() {
        let req: ProbeProtocolsRequest = serde_json::from_str(
            r#"{"model":"gpt-4o-mini","targets":[{"protocol":"chat_completions","base_url":"https://x.com/v1"}]}"#,
        )
        .unwrap();
        assert_eq!(req.proxy_mode, ProxyMode::Inherit);
        assert_eq!(req.targets.len(), 1);
        assert_eq!(req.targets[0].protocol, ApiKind::ChatCompletions);
    }

    #[test]
    fn probe_body_minimizes_output_tokens() {
        let chat = probe_body(ApiKind::ChatCompletions, "m");
        assert_eq!(chat["max_tokens"], 1);
        assert_eq!(chat["messages"][0]["content"], "hi");

        let anthropic = probe_body(ApiKind::AnthropicMessages, "m");
        assert_eq!(anthropic["max_tokens"], 1);

        let responses = probe_body(ApiKind::Responses, "m");
        assert_eq!(responses["max_output_tokens"], 16);
        assert_eq!(responses["input"], "hi");
        assert!(responses.get("max_tokens").is_none());
    }

    #[test]
    fn probe_url_appends_fixed_protocol_path() {
        assert_eq!(
            probe_url("https://api.openai.com/v1/", ApiKind::ChatCompletions).as_deref(),
            Some("https://api.openai.com/v1/chat/completions")
        );
        assert_eq!(
            probe_url("https://api.openai.com/v1", ApiKind::Responses).as_deref(),
            Some("https://api.openai.com/v1/responses")
        );
        assert_eq!(
            probe_url("https://api.anthropic.com", ApiKind::AnthropicMessages).as_deref(),
            Some("https://api.anthropic.com/messages")
        );
        assert_eq!(probe_url("https://x.com", ApiKind::Image), None);
    }

    #[tokio::test]
    async fn probe_image_target_is_400_before_network() {
        let state = state_with_global_proxy(None).await;
        let app = test_app(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/providers/probe-protocols")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"m","api_keys":["sk-test"],"targets":[{"protocol":"image","base_url":"http://127.0.0.1:9"}]}"#,
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("not probeable")
        );
    }

    #[tokio::test]
    async fn probe_enabled_without_global_proxy_is_400_before_network() {
        let state = state_with_global_proxy(None).await;
        let app = test_app(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/providers/probe-protocols")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"m","api_keys":["sk-test"],"proxy_mode":"enabled","targets":[{"protocol":"chat_completions","base_url":"http://127.0.0.1:9"}]}"#,
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["error"]["code"], "BAD_REQUEST");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("proxy_url")
        );
    }

    #[tokio::test]
    async fn resolve_inherit_uses_in_memory_global_proxy() {
        let state = state_with_global_proxy(Some("http://proxy.example:8080")).await;
        assert_eq!(
            resolve_test_proxy(&state, ProxyMode::Inherit)
                .await
                .unwrap()
                .as_deref(),
            Some("http://proxy.example:8080")
        );

        // No global proxy configured -> direct connection.
        let state = state_with_global_proxy(None).await;
        assert_eq!(
            resolve_test_proxy(&state, ProxyMode::Inherit)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn resolve_disabled_is_direct_even_with_global_proxy() {
        // `disabled` must resolve to a direct connection even when the
        // in-memory gateway has a global proxy. Combined with
        // `build_http_client(None)` (→ `.no_proxy()`), this is what ignores
        // both the gateway proxy and the system proxy environment.
        let state = state_with_global_proxy(Some("http://proxy.example:8080")).await;
        assert_eq!(
            resolve_test_proxy(&state, ProxyMode::Disabled)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn resolve_enabled_uses_global_proxy_or_errors() {
        let state = state_with_global_proxy(Some("http://proxy.example:8080")).await;
        assert_eq!(
            resolve_test_proxy(&state, ProxyMode::Enabled)
                .await
                .unwrap()
                .as_deref(),
            Some("http://proxy.example:8080")
        );

        // `enabled` with no global proxy is a 400-style error.
        let state = state_with_global_proxy(None).await;
        let err = resolve_test_proxy(&state, ProxyMode::Enabled)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
        assert!(err.to_string().contains("proxy_url"));
    }

    #[tokio::test]
    async fn fetch_models_enabled_without_global_proxy_is_400_before_network() {
        // `proxy_mode: enabled` with no global proxy must fail with 400
        // *before* any upstream request is attempted: the response is the
        // proxy-resolution error, never a CONNECTION_ERROR from a send.
        let state = state_with_global_proxy(None).await;
        let app = test_app(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/providers/fetch-models")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"base_url":"http://127.0.0.1:9/v1","api_keys":["sk-test"],"proxy_mode":"enabled"}"#,
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["error"]["code"], "BAD_REQUEST");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("proxy_url"),
            "failure must come from proxy resolution, not a network attempt"
        );
    }

    #[tokio::test]
    async fn model_test_enabled_without_global_proxy_is_400_before_network() {
        let state = state_with_global_proxy(None).await;
        let app = test_app(state);

        let req = Request::builder()
            .method("POST")
            .uri("/api/model/test")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"gpt-4o","base_url":"http://127.0.0.1:9/v1","api_keys":["sk-test"],"proxy_mode":"enabled"}"#,
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["error"]["code"], "BAD_REQUEST");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("proxy_url")
        );
    }
}
