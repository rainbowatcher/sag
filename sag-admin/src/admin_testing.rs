//! Provider testing utilities: discover available models on a provider
//! (`fetch_models`) and verify a specific model is reachable with a given
//! key (`model_test`). These are unauthenticated-by-default convenience
//! endpoints that take a temporary provider config and short-lived
//! reqwest client, so callers don't need a registered provider entry
//! to test connectivity.

use std::collections::HashMap;

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use sag_config::{ApiKind, ProxyMode};
use sag_core::AppError;
use sag_providers::{create_provider_with_proxy, provider_auth_headers};
use serde_json::json;
use tracing::{instrument, warn};

use crate::probe_provider::{
    self, MAX_MODEL_TEST_KEYS, MODEL_TEST_TIMEOUT_SECS, ProviderDefaults, TEMP_PROVIDER_NAME,
};
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
async fn resolve_test_proxy_config(
    state: &AppState,
    mode: ProxyMode,
) -> Result<Option<sag_config::ProxyConfig>, AppError> {
    let global = state
        .config
        .read()
        .await
        .normalized_global_proxy()
        .map_err(AppError::BadRequest)?;
    sag_config::resolve_effective_proxy(mode, global.as_ref()).map_err(AppError::BadRequest)
}

/// Same resolution as [`resolve_test_proxy_config`], flattened to the
/// connect URL used by the raw reqwest paths (`fetch_models` /
/// `probe-protocols`).
async fn resolve_test_proxy(state: &AppState, mode: ProxyMode) -> Result<Option<String>, AppError> {
    Ok(resolve_test_proxy_config(state, mode)
        .await?
        .map(|p| p.to_connect_url()))
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
    /// 旧版扁平探测地址；`api` 缺省时作为 `ApiConfig.base_url` 的回退。
    #[serde(default)]
    pub base_url: String,
    /// API section under test（默认 chat_completions）。
    #[serde(default)]
    pub api_kind: Option<ApiKind>,
    /// 旧版扁平 headers；作为 provider 级 headers 的回退。
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// 待测的整个 `apis.<kind>` 节；缺省时回退 `base_url` + `headers`（向后兼容）。
    #[serde(default)]
    pub api: Option<sag_config::ApiConfig>,
    /// provider 级 headers（表单当前值）。
    #[serde(default)]
    pub provider_headers: HashMap<String, String>,
    #[serde(default)]
    pub body_overrides: HashMap<String, serde_json::Value>,
    #[serde(default)]
    pub strip_fields: Vec<String>,
    #[serde(default)]
    pub model_overrides: HashMap<String, HashMap<String, serde_json::Value>>,
    #[serde(default)]
    pub model_strip_fields: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub role_mappings: HashMap<String, String>,
    #[serde(default)]
    pub anthropic_cache_control: Option<bool>,
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
    let kind = api_kind_of(req.api_kind);

    // image 非协议：400 早退（行为变更：原先返回上游 400 文案）。
    if !kind.is_protocol() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "code": "UNSUPPORTED_API_KIND",
                    "message": "model test supports chat_completions / responses / anthropic_messages only"
                }
            })),
        )
            .into_response();
    }

    if req.api_keys.len() > MAX_MODEL_TEST_KEYS {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": {
                    "code": "TOO_MANY_KEYS",
                    "message": format!("at most {} api_keys per model test", MAX_MODEL_TEST_KEYS)
                }
            })),
        )
            .into_response();
    }

    if req.api_keys.is_empty() {
        return Json(json!({ "results": [] })).into_response();
    }

    let effective_proxy = match resolve_test_proxy_config(&state, req.proxy_mode).await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };

    // `api` 优先；缺省回退旧扁平字段。
    let api = req.api.clone().unwrap_or_else(|| sag_config::ApiConfig {
        base_url: req.base_url.clone(),
        headers: Some(req.headers.clone()),
        ..Default::default()
    });

    // provider 级 headers：provider_headers 非空则用它，否则回退扁平 headers。
    let defaults = ProviderDefaults {
        headers: if req.provider_headers.is_empty() {
            req.headers.clone()
        } else {
            req.provider_headers.clone()
        },
        body_overrides: req.body_overrides.clone(),
        strip_fields: req.strip_fields.clone(),
        model_overrides: req.model_overrides.clone(),
        model_strip_fields: req.model_strip_fields.clone(),
        role_mappings: req.role_mappings.clone(),
        anthropic_cache_control: req.anthropic_cache_control,
    };

    // 模型级 override 只有两层：api 级优先，整段取一（不 merge）。
    let target_extra = api
        .model_overrides
        .as_ref()
        .and_then(|m| m.get(&req.model))
        .or_else(|| defaults.model_overrides.get(&req.model))
        .cloned()
        .unwrap_or_default();

    let mut results = Vec::new();
    for key in &req.api_keys {
        let cfg =
            probe_provider::synthesize_config(kind, &api, key, &defaults, MODEL_TEST_TIMEOUT_SECS);
        // 生效超时（per-API 已被钳制）——文案必须用它，避免谎报 30s。
        let effective_timeout = cfg
            .apis
            .get(&kind)
            .and_then(|a| a.timeout_secs)
            .unwrap_or(cfg.timeout_secs);

        let outcome = match create_provider_with_proxy(
            TEMP_PROVIDER_NAME.to_string(),
            &cfg,
            effective_proxy.as_ref(),
        ) {
            Ok(provider) => {
                probe_provider::test_protocol(
                    provider.as_ref(),
                    kind,
                    &req.model,
                    &target_extra,
                    effective_timeout,
                )
                .await
            }
            Err(e) => Err(e),
        };

        let (success, message) = match outcome {
            Ok(()) => (true, "OK".to_string()),
            Err(e) => (
                false,
                probe_provider::message_from_error(&e, &req.model, effective_timeout),
            ),
        };
        results.push(json!({
            "key_prefix": key_prefix(key),
            "success": success,
            "message": message,
        }));
    }

    Json(json!({ "results": results })).into_response()
}

// ── Protocol support probing (Phase B) ────────────────────────────
//
// probe 走原始 reqwest：2xx 恒为 `supported`，**不**复用 provider adapter——
// adapter 内建的「200 → Upstream{429|502}」改写（`openai.rs` chat / responses、
// `anthropic.rs` messages_native）会破坏本端点的判定语义，且 adapter 会解析
// body 要求 schema 合法，对探测器是新增假阴性。协议知识（path / body / 鉴权头）
// 经 `probe_request::minimal_request` + `sag_core::protocol_path` +
// `provider_auth_headers` 共享；不读取任何已保存 provider 的配置（裸语义）。

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

/// 2xx 的 message：折叠空白 + 截断 160 字符（按 char 计）；空 body → "OK"。
fn ok_message(body: &str) -> String {
    let flat: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        "OK".into()
    } else {
        format!("OK — {}", flat.chars().take(160).collect::<String>())
    }
}

/// Pure verdict table: HTTP status + response body → (verdict, message).
/// Split out for unit testing without a network mock.
fn verdict_from_status_body(status: u16, body: &str) -> (ProbeVerdict, String) {
    match status {
        200..=299 => (ProbeVerdict::Supported, ok_message(body)),
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

/// Minimal probe body per protocol — the body is the sole source of the
/// protocol knowledge shared with `model/test`; see
/// [`crate::probe_request::minimal_request`].
///
/// Probe URL = `base_url` + the fixed protocol path
/// (`sag_core::protocol_path`), see [`crate::probe_request::request_url`].
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
    // The request body is produced solely by `minimal_request` (shared
    // protocol knowledge) — probe never reads a saved provider's overrides.
    let Some(minimal) = crate::probe_request::minimal_request(target.protocol, &req.model) else {
        return (None, ProbeVerdict::Error, "unsupported protocol".into());
    };
    let body = minimal.to_body();

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
        let Some(url) = crate::probe_request::request_url(&target.base_url, target.protocol) else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{
        build_state_with_proxy, spawn_upstream_capture_many, spawn_upstream_capture_scripted,
        test_app,
    };
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

    // ── model/test via the provider adapter (capturing mock upstream) ──

    /// A valid chat completion body (all `ChatCompletionResponse` required
    /// fields present, empty `choices`).
    const CHAT_OK: &str =
        r#"{"id":"x","object":"chat.completion","created":0,"model":"m","choices":[]}"#;

    /// POST `payload` to `uri` on `app`, returning `(status, json body)`.
    async fn post_json(
        app: axum::Router,
        uri: &str,
        payload: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, body)
    }

    #[tokio::test]
    async fn model_test_responses_kind_posts_input_to_responses_path() {
        let (base, mut rx) = spawn_upstream_capture_many(200, "{}").await;
        let app = test_app(state_with_global_proxy(None).await);

        let (status, body) = post_json(
            app,
            "/api/model/test",
            json!({
                "model": "m",
                "api_keys": ["sk-1"],
                "api_kind": "responses",
                "base_url": base,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["results"][0]["success"], true);

        let captured = rx.recv().await.expect("one upstream request");
        assert_eq!(captured.path, "/responses");
        assert_eq!(captured.method, "POST");
        assert!(captured.body.get("input").is_some());
        assert!(captured.body.get("max_output_tokens").is_some());
        assert!(
            captured.body.get("messages").is_none(),
            "responses body must not carry chat messages: {}",
            captured.body
        );
    }

    #[tokio::test]
    async fn model_test_anthropic_kind_sends_x_api_key_and_anthropic_version() {
        let (base, mut rx) = spawn_upstream_capture_many(200, "{}").await;
        let app = test_app(state_with_global_proxy(None).await);

        let (status, body) = post_json(
            app,
            "/api/model/test",
            json!({
                "model": "m",
                "api_keys": ["sk-1"],
                "api_kind": "anthropic_messages",
                "base_url": base,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["results"][0]["success"], true);

        let captured = rx.recv().await.expect("one upstream request");
        assert_eq!(captured.path, "/messages");
        assert_eq!(captured.headers.get("x-api-key"), Some(&"sk-1".to_string()));
        assert_eq!(
            captured.headers.get("anthropic-version"),
            Some(&"2023-06-01".to_string())
        );
        assert_eq!(captured.body["max_tokens"], 1);
    }

    #[tokio::test]
    async fn model_test_chat_kind_posts_max_completion_tokens() {
        let (base, mut rx) = spawn_upstream_capture_many(200, CHAT_OK).await;
        let app = test_app(state_with_global_proxy(None).await);

        let (status, _) = post_json(
            app,
            "/api/model/test",
            json!({ "model": "m", "api_keys": ["sk-1"], "base_url": base }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let captured = rx.recv().await.expect("one upstream request");
        assert_eq!(captured.path, "/chat/completions");
        assert_eq!(captured.body["max_completion_tokens"], 1);
        assert!(
            captured.body.get("max_tokens").is_none(),
            "chat body must use max_completion_tokens: {}",
            captured.body
        );
    }

    #[tokio::test]
    async fn model_test_applies_body_overrides_and_strip_fields() {
        let (base, mut rx) = spawn_upstream_capture_many(200, CHAT_OK).await;
        let app = test_app(state_with_global_proxy(None).await);

        let (status, _) = post_json(
            app,
            "/api/model/test",
            json!({
                "model": "m",
                "api_keys": ["sk-1"],
                "base_url": base,
                "body_overrides": { "temperature": 0.5 },
                "strip_fields": ["max_completion_tokens"],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let captured = rx.recv().await.expect("one upstream request");
        assert_eq!(captured.body["temperature"], 0.5);
        assert!(
            captured.body.get("max_completion_tokens").is_none(),
            "strip_fields must remove the field via the build_request chain: {}",
            captured.body
        );
    }

    #[tokio::test]
    async fn model_test_applies_per_api_headers_over_provider_headers() {
        let (base, mut rx) = spawn_upstream_capture_many(200, CHAT_OK).await;
        let app = test_app(state_with_global_proxy(None).await);

        let (status, _) = post_json(
            app,
            "/api/model/test",
            json!({
                "model": "m",
                "api_keys": ["sk-1"],
                "api_kind": "chat_completions",
                "api": { "base_url": base, "headers": { "x-a": "api" } },
                "provider_headers": { "x-a": "provider", "x-b": "p" },
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let captured = rx.recv().await.expect("one upstream request");
        assert_eq!(captured.headers.get("x-a"), Some(&"api".to_string()));
        // `apis.<kind>.headers` replaces the provider-level map wholesale
        // (config contract: 整段替换，不合并), so the provider-only key is
        // not sent.
        assert!(
            !captured.headers.contains_key("x-b"),
            "per-API headers replace provider headers wholesale"
        );
    }

    #[tokio::test]
    async fn model_test_model_override_reaches_upstream_body() {
        let (base, mut rx) = spawn_upstream_capture_many(200, CHAT_OK).await;
        let app = test_app(state_with_global_proxy(None).await);

        let (status, _) = post_json(
            app,
            "/api/model/test",
            json!({
                "model": "m",
                "api_keys": ["sk-1"],
                "base_url": base,
                "model_overrides": { "m": { "temperature": 0.9 } },
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let captured = rx.recv().await.expect("one upstream request");
        assert_eq!(captured.body["temperature"], 0.9);
    }

    #[tokio::test]
    async fn model_test_provider_headers_apply_when_api_declares_none() {
        let (base, mut rx) = spawn_upstream_capture_many(200, CHAT_OK).await;
        let app = test_app(state_with_global_proxy(None).await);

        let (status, _) = post_json(
            app,
            "/api/model/test",
            json!({
                "model": "m",
                "api_keys": ["sk-1"],
                "api_kind": "chat_completions",
                "api": { "base_url": base },
                "provider_headers": { "x-p": "1" },
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let captured = rx.recv().await.expect("one upstream request");
        assert_eq!(captured.headers.get("x-p"), Some(&"1".to_string()));
    }

    #[tokio::test]
    async fn model_test_flat_headers_fallback_when_api_present() {
        let (base, mut rx) = spawn_upstream_capture_many(200, CHAT_OK).await;
        let app = test_app(state_with_global_proxy(None).await);

        let (status, _) = post_json(
            app,
            "/api/model/test",
            json!({
                "model": "m",
                "api_keys": ["sk-1"],
                "api_kind": "chat_completions",
                "api": { "base_url": base },
                "headers": { "x-flat": "1" },
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let captured = rx.recv().await.expect("one upstream request");
        assert_eq!(captured.headers.get("x-flat"), Some(&"1".to_string()));
    }

    #[tokio::test]
    async fn model_test_api_model_override_reaches_upstream_body() {
        let (base, mut rx) = spawn_upstream_capture_many(200, CHAT_OK).await;
        let app = test_app(state_with_global_proxy(None).await);

        let (status, _) = post_json(
            app,
            "/api/model/test",
            json!({
                "model": "m",
                "api_keys": ["sk-1"],
                "api_kind": "chat_completions",
                "api": {
                    "base_url": base,
                    "model_overrides": { "m": { "temperature": 0.3 } },
                },
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let captured = rx.recv().await.expect("one upstream request");
        assert_eq!(captured.body["temperature"], 0.3);
    }

    #[tokio::test]
    async fn model_test_api_model_override_wins_over_provider_level() {
        let (base, mut rx) = spawn_upstream_capture_many(200, CHAT_OK).await;
        let app = test_app(state_with_global_proxy(None).await);

        let (status, _) = post_json(
            app,
            "/api/model/test",
            json!({
                "model": "m",
                "api_keys": ["sk-1"],
                "api_kind": "chat_completions",
                "api": {
                    "base_url": base,
                    "model_overrides": { "m": { "temperature": 0.3 } },
                },
                "model_overrides": { "m": { "temperature": 0.9 } },
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let captured = rx.recv().await.expect("one upstream request");
        assert_eq!(
            captured.body["temperature"], 0.3,
            "api-level model override must win wholesale"
        );
    }

    #[tokio::test]
    async fn model_test_rejects_too_many_api_keys() {
        let (base, mut rx) = spawn_upstream_capture_many(200, CHAT_OK).await;
        let app = test_app(state_with_global_proxy(None).await);
        let keys: Vec<String> = (0..17).map(|i| format!("sk-{i}")).collect();

        let (status, body) = post_json(
            app,
            "/api/model/test",
            json!({ "model": "m", "api_keys": keys, "base_url": base }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "TOO_MANY_KEYS");
        assert!(
            matches!(
                rx.try_recv(),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)
            ),
            "the 16-key cap must reject before any network request"
        );
    }

    #[tokio::test]
    async fn model_test_treats_200_with_error_body_as_failure() {
        let (base, _rx) = spawn_upstream_capture_many(200, r#"{"error":{"message":"boom"}}"#).await;
        let app = test_app(state_with_global_proxy(None).await);

        let (status, body) = post_json(
            app,
            "/api/model/test",
            json!({ "model": "m", "api_keys": ["sk-1"], "base_url": base }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["results"][0]["success"], false);
        assert!(
            body["results"][0]["message"]
                .as_str()
                .unwrap()
                .contains("boom"),
            "200 + error body must surface the upstream message: {}",
            body["results"][0]["message"]
        );
    }

    #[tokio::test]
    async fn model_test_per_key_results_preserved() {
        let (base, _rx) =
            spawn_upstream_capture_scripted(vec![(200, CHAT_OK), (401, "unauthorized")]).await;
        let app = test_app(state_with_global_proxy(None).await);

        let (status, body) = post_json(
            app,
            "/api/model/test",
            json!({ "model": "m", "api_keys": ["sk-0", "sk-1"], "base_url": base }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["success"], true);
        assert_eq!(results[1]["success"], false);
        assert!(
            results[1]["message"]
                .as_str()
                .unwrap()
                .contains("Authentication failed"),
            "401 must keep the legacy auth wording: {}",
            results[1]["message"]
        );
    }

    #[tokio::test]
    async fn model_test_legacy_flat_payload_still_works() {
        let (base, mut rx) = spawn_upstream_capture_many(200, CHAT_OK).await;
        let app = test_app(state_with_global_proxy(None).await);

        // Exactly the payload today's UI sends: no `api`, no provider-level
        // fields — only the flat legacy keys.
        let (status, body) = post_json(
            app,
            "/api/model/test",
            json!({
                "model": "m",
                "api_keys": ["sk-1"],
                "base_url": base,
                "api_kind": "chat_completions",
                "headers": { "x-legacy": "1" },
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["results"][0]["success"], true);

        let captured = rx.recv().await.expect("legacy payload must reach upstream");
        assert_eq!(captured.path, "/chat/completions");
        assert_eq!(captured.headers.get("x-legacy"), Some(&"1".to_string()));
    }

    #[tokio::test]
    async fn model_test_image_kind_is_400_before_network() {
        let app = test_app(state_with_global_proxy(None).await);

        let (status, body) = post_json(
            app,
            "/api/model/test",
            json!({
                "model": "dall-e-3",
                "api_keys": ["sk-1"],
                "api_kind": "image",
                "base_url": "http://127.0.0.1:9",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "UNSUPPORTED_API_KIND");
    }

    // ── probe 2xx / body preview ──────────────────────────────────

    #[test]
    fn probe_verdict_2xx_is_supported_even_with_error_body() {
        let (verdict, message) =
            verdict_from_status_body(200, r#"{"error":{"code":"AuthenticationError"}}"#);
        assert_eq!(
            verdict,
            ProbeVerdict::Supported,
            "probe does not sniff error bodies out of a 2xx"
        );
        assert!(message.contains("AuthenticationError"), "{message}");
    }

    #[test]
    fn probe_ok_message_truncates_and_flattens_body() {
        let long = "a".repeat(500);
        let msg = ok_message(&long);
        assert!(msg.starts_with("OK — "), "{msg}");
        assert_eq!(
            msg.chars().count(),
            5 + 160,
            "160-char preview after prefix"
        );

        assert_eq!(ok_message("line1\nline2\t line3"), "OK — line1 line2 line3");
        assert_eq!(ok_message("   \n\t "), "OK");
    }

    #[tokio::test]
    async fn probe_ignores_unknown_payload_fields_and_saved_config() {
        let (base, mut rx) = spawn_upstream_capture_many(200, r#"{"ok":true}"#).await;
        let app = test_app(state_with_global_proxy(None).await);

        let (status, body) = post_json(
            app,
            "/api/providers/probe-protocols",
            json!({
                "model": "m",
                "api_keys": ["sk-1"],
                "targets": [{ "protocol": "chat_completions", "base_url": base }],
                "body_overrides": { "temperature": 0.5 },
                "strip_fields": ["max_completion_tokens"],
                "model_overrides": { "m": { "temperature": 0.9 } },
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["results"][0]["verdict"], "supported");
        assert!(
            body["results"][0]["message"]
                .as_str()
                .unwrap()
                .starts_with("OK — "),
            "2xx preview must be the (flattened) response body: {}",
            body["results"][0]["message"]
        );

        let captured = rx.recv().await.expect("one probe request");
        assert_eq!(captured.path, "/chat/completions");
        let expected = crate::probe_request::minimal_request(ApiKind::ChatCompletions, "m")
            .unwrap()
            .to_body();
        assert_eq!(
            captured.body, expected,
            "probe body comes solely from minimal_request — no overrides applied"
        );
    }
}
