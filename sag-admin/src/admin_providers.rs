//! Provider CRUD handlers for the admin API.
//!
//! All routes are mounted under `/api/providers*` by
//! [`crate::admin::build_admin_routes`].

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use indexmap::IndexMap;
use sag_config::{
    ApiConfig, ApiKind, AuthMode, KeyStrategy, OAuthConfig, ProviderConfig, ProxyMode,
};
use sag_core::AppError;
use serde_json::json;
use tracing::instrument;

use sag_state::AppState;

/// Mask for OAuth `client_secret` on every admin out-path. Update patches
/// treat this literal as "keep the stored secret" (never write it back as a
/// real credential).
const SECRET_MASK: &str = "***";

/// Apply load-time normalization to an incoming `apis` map: trim each
/// `base_url`'s trailing `/` (mirrors `GatewayConfig::normalize`) and
/// normalize a present `models_endpoint` (empty → `"models"`). The
/// coordinator's `mutate` path validates but does not normalize, so the
/// admin entry point does it here to match what `load_config` produces.
fn normalize_apis(apis: &mut IndexMap<ApiKind, ApiConfig>) {
    for api in apis.values_mut() {
        let trimmed = api.base_url.trim_end_matches('/');
        if trimmed != api.base_url {
            api.base_url = trimmed.to_string();
        }
        if let Some(endpoint) = api.models_endpoint.take() {
            api.models_endpoint = Some(sag_config::normalize_models_endpoint(&endpoint));
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct AddProviderRequest {
    /// Provider name (must be unique).
    pub name: String,
    #[serde(default)]
    pub api_keys: Vec<String>,
    #[serde(default)]
    pub key_strategy: KeyStrategy,
    /// 协议 API 目录：每节携带自己的 `base_url`（地址唯一来源）+ 可选
    /// `models_endpoint` 与协议覆盖字段。空目录在 handler 内 400；空 /
    /// 未知 apis key 反序列化即报错（axum 422，枚举严格），其余未知
    /// 字段照例容忍。
    #[serde(default)]
    pub apis: IndexMap<ApiKind, ApiConfig>,
    /// 降级协议（translated 路径的编码目标）。`apis` 恰好一个协议 key
    /// 时可省略；多个协议 key 时必填（由提交时的全量校验兜底）。
    #[serde(default)]
    pub fallback: Option<ApiKind>,
    /// How this provider resolves the upstream proxy: `inherit` (default),
    /// `enabled`, or `disabled`. Unknown *fields* on the request body are
    /// tolerated as elsewhere in the admin DTOs, but an unknown
    /// `proxy_mode` *value* fails deserialization (axum 422) — the same
    /// strictness as the config loader's typed enums.
    #[serde(default)]
    pub proxy_mode: ProxyMode,
    #[serde(default = "sag_config::default_timeout_secs")]
    pub timeout_secs: u64,
    /// Custom headers to override upstream request headers.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Body field overrides merged into the upstream JSON request.
    #[serde(default)]
    pub body_overrides: HashMap<String, serde_json::Value>,
    /// Top-level fields to strip from the upstream JSON body before sending.
    #[serde(default)]
    pub strip_fields: Vec<String>,
    /// Per-model body field strip list, keyed by real_model name.
    #[serde(default)]
    pub model_strip_fields: HashMap<String, Vec<String>>,
    /// Per-model body field overrides, keyed by real_model name.
    #[serde(default)]
    pub model_overrides: HashMap<String, HashMap<String, serde_json::Value>>,
    /// Role mappings: maps request roles to upstream-supported roles.
    #[serde(default)]
    pub role_mappings: HashMap<String, String>,
    /// List of model IDs enabled for this provider (optional).
    #[serde(default)]
    pub enabled_models: Vec<String>,
    /// Credential mode: `static` (default, uses `api_keys`) or `oauth`
    /// (uses the `[providers.<name>.oauth]` section). Unknown values fail
    /// deserialization (typed enum strictness).
    #[serde(default)]
    pub auth_mode: AuthMode,
    /// OAuth client-credentials settings. Required when `auth_mode` is
    /// `oauth`; ignored when `auth_mode` is `static`.
    #[serde(default)]
    pub oauth: Option<OAuthConfig>,
}

#[derive(Debug, serde::Deserialize)]
pub struct UpdateProviderRequest {
    pub name: String,
    #[serde(default)]
    pub api_keys: Option<Vec<String>>,
    #[serde(default)]
    pub key_strategy: Option<KeyStrategy>,
    /// Optional proxy-mode patch: omitted/`null` keeps the current value;
    /// `Some(ProxyMode::Inherit)` explicitly resets to `inherit`. This
    /// mirrors the other `Option<...>` patch fields.
    #[serde(default)]
    pub proxy_mode: Option<ProxyMode>,
    /// Whole-directory patch for `apis`: **absent** keeps the current
    /// sections; **present** (including `{}`) replaces the entire map
    /// (sections not repeated are dropped). Per-section overrides are not
    /// merged field-by-field — send the full desired directory.
    #[serde(default)]
    pub apis: Option<IndexMap<ApiKind, ApiConfig>>,
    /// Triple-state `fallback` patch: absent → keep, JSON `null` → clear
    /// (back to the single-protocol-key default), value → set.
    #[serde(default, deserialize_with = "double_option_api_kind")]
    pub fallback: Option<Option<ApiKind>>,
    pub timeout_secs: Option<u64>,
    pub headers: Option<HashMap<String, String>>,
    pub body_overrides: Option<HashMap<String, serde_json::Value>>,
    pub strip_fields: Option<Vec<String>>,
    pub model_strip_fields: Option<HashMap<String, Vec<String>>>,
    pub model_overrides: Option<HashMap<String, HashMap<String, serde_json::Value>>>,
    pub role_mappings: Option<HashMap<String, String>>,
    pub enabled_models: Option<Vec<String>>,
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Credential-mode patch: omitted/`null` keeps the current value.
    /// When set to `static`, the stored `oauth` section is cleared.
    #[serde(default)]
    pub auth_mode: Option<AuthMode>,
    /// Whole-section OAuth patch: omitted keeps the current section;
    /// `null` clears it (when switching to static); value replaces the
    /// entire section. A `client_secret` of `"***"` keeps the stored
    /// secret — the admin out-path never returns the plaintext.
    #[serde(default, deserialize_with = "double_option_oauth")]
    pub oauth: Option<Option<OAuthConfig>>,
}

/// Serde helper for the triple-state `fallback` patch: missing →
/// `None` (keep — uses `#[serde(default)]`), JSON `null` →
/// `Some(None)` (clear), value → `Some(Some(v))` (set).
fn double_option_api_kind<'de, D>(deserializer: D) -> Result<Option<Option<ApiKind>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let value: Option<ApiKind> = Option::deserialize(deserializer)?;
    Ok(Some(value))
}

/// Serde helper for the triple-state `oauth` patch: missing → `None`
/// (keep), JSON `null` → `Some(None)` (clear), value → `Some(Some(v))`.
fn double_option_oauth<'de, D>(deserializer: D) -> Result<Option<Option<OAuthConfig>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let value: Option<OAuthConfig> = Option::deserialize(deserializer)?;
    Ok(Some(value))
}

/// Resolve an inbound `client_secret` against the stored secret.
/// `SECRET_MASK` means "keep"; any other string is the new plaintext.
fn resolve_oauth_secret(request_secret: &str, existing: Option<&str>) -> String {
    if request_secret == SECRET_MASK {
        existing.unwrap_or_default().to_string()
    } else {
        request_secret.to_string()
    }
}

/// Admin out-path OAuth shape: plaintext `client_secret` is never returned.
/// `client_secret: "***"` + `client_secret_set` is the stable contract.
#[derive(Debug, serde::Serialize)]
pub struct OAuthInfo {
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub client_secret_set: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

fn oauth_info(oauth: &OAuthConfig) -> OAuthInfo {
    OAuthInfo {
        token_url: oauth.token_url.clone(),
        client_id: oauth.client_id.clone(),
        client_secret: SECRET_MASK.to_string(),
        client_secret_set: !oauth.client_secret.trim().is_empty(),
        scope: oauth.scope.clone(),
        audience: oauth.audience.clone(),
        refresh_token: oauth.refresh_token.clone(),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct DeleteProviderRequest {
    pub name: String,
}

#[derive(serde::Serialize)]
pub struct ProviderInfo {
    pub name: String,
    pub api_keys: Vec<String>,
    pub key_strategy: KeyStrategy,
    /// Protocol API catalog: one section per declared [`ApiKind`], each
    /// carrying its own `base_url` (the only address source) plus the
    /// optional `models_endpoint` / protocol override fields.
    pub apis: IndexMap<ApiKind, ApiConfig>,
    /// Downgrade protocol for translated-lane dispatch (`null` when the
    /// single protocol key makes it implicit).
    pub fallback: Option<ApiKind>,
    pub proxy_mode: ProxyMode,
    pub timeout_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttfb_timeout_secs: Option<u64>,
    pub headers: HashMap<String, String>,
    pub body_overrides: HashMap<String, serde_json::Value>,
    pub strip_fields: Vec<String>,
    pub model_strip_fields: HashMap<String, Vec<String>>,
    pub model_overrides: HashMap<String, HashMap<String, serde_json::Value>>,
    pub role_mappings: HashMap<String, String>,
    pub enabled_models: Vec<String>,
    pub enabled: bool,
    /// Credential mode for this provider (`static` / `oauth`).
    pub auth_mode: AuthMode,
    /// OAuth settings when `auth_mode = "oauth"`. `client_secret` is
    /// always the mask `"***"`; `client_secret_set` reports whether a
    /// secret is stored. Never returns the plaintext secret.
    pub oauth: Option<OAuthInfo>,
    /// Per-API capability bitmap keyed by `ApiKind` — a JSON object of
    /// `{ "<api_kind>": { ...capabilities } }`, read-only UI aid.
    ///
    /// Values come from `sag_dispatch::dispatch::capability::
    /// target_capabilities`, the single source of truth for the protocol
    /// capability matrix (anthropic / responses claim `hosted_tools`,
    /// responses claims `stateful`, chat claims neither).
    pub capabilities: serde_json::Value,
}

#[derive(Debug, serde::Deserialize)]
pub struct ListProvidersQuery {
    #[serde(default)]
    pub model_name: Option<String>,
    /// Only keep providers that declare this API section (e.g.
    /// `apis=anthropic_messages`). Replaces the deleted `provider_type`
    /// filter — unknown query fields are tolerated by serde_urlencoded.
    #[serde(default)]
    pub apis: Option<ApiKind>,
    #[serde(default)]
    pub sort: Option<String>,
}

fn provider_matches_filters(cfg: &ProviderConfig, query: &ListProvidersQuery) -> bool {
    let model_filter = query
        .model_name
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty());

    let matches_model = match model_filter {
        Some(model) => {
            cfg.enabled_models.is_empty() || cfg.enabled_models.iter().any(|m| m == model)
        }
        None => true,
    };

    let matches_api = query
        .apis
        .map(|kind| cfg.apis.contains_key(&kind))
        .unwrap_or(true);

    matches_model && matches_api
}

/// Capability JSON for the provider's declared API sections, keyed by
/// [`ApiKind`]. Values are [`sag_dispatch::dispatch::capability::target_capabilities`]
/// — the dispatch pre-check table — so the admin UI and the router always
/// agree on what each protocol supports.
fn provider_capabilities(cfg: &ProviderConfig) -> serde_json::Value {
    let map: serde_json::Map<String, serde_json::Value> = cfg
        .apis
        .keys()
        .map(|kind| {
            let caps = sag_dispatch::dispatch::capability::target_capabilities(*kind);
            (
                kind.as_str().to_string(),
                serde_json::to_value(caps).unwrap_or(serde_json::Value::Null),
            )
        })
        .collect();
    serde_json::Value::Object(map)
}

fn provider_info(name: &str, cfg: &ProviderConfig) -> ProviderInfo {
    ProviderInfo {
        name: name.to_string(),
        api_keys: cfg.api_keys.clone(),
        key_strategy: cfg.key_strategy,
        apis: cfg.apis.clone(),
        fallback: cfg.fallback,
        proxy_mode: cfg.proxy_mode,
        timeout_secs: cfg.timeout_secs,
        chunk_timeout_secs: cfg.chunk_timeout_secs,
        ttfb_timeout_secs: cfg.ttfb_timeout_secs,
        headers: cfg.headers.clone(),
        body_overrides: cfg.body_overrides.clone(),
        strip_fields: cfg.strip_fields.clone(),
        model_strip_fields: cfg.model_strip_fields.clone(),
        model_overrides: cfg.model_overrides.clone(),
        role_mappings: cfg.role_mappings.clone(),
        enabled_models: cfg.enabled_models.clone(),
        enabled: cfg.enabled,
        auth_mode: cfg.auth_mode,
        oauth: cfg.oauth.as_ref().map(oauth_info),
        capabilities: provider_capabilities(cfg),
    }
}

// ═══════════════════════════════════════════════════════════════════
// Handlers
// ═══════════════════════════════════════════════════════════════════

/// GET /api/providers — list all registered providers.
pub async fn list_providers(
    State(state): State<AppState>,
    Query(query): Query<ListProvidersQuery>,
) -> impl IntoResponse {
    let config = state.config.read().await;
    let mut providers: Vec<ProviderInfo> = config
        .providers
        .iter()
        .filter(|(_, cfg)| provider_matches_filters(cfg, &query))
        .map(|(name, cfg)| provider_info(name, cfg))
        .collect();

    if query.sort.as_deref() == Some("name") || query.sort.is_none() {
        providers.sort_by(|a, b| a.name.cmp(&b.name));
    }

    (StatusCode::OK, Json(json!({ "providers": providers }))).into_response()
}

/// POST /api/providers — add a new provider and register it in the runtime registry.
#[instrument(skip(state), fields(provider_name))]
pub async fn add_provider(
    State(state): State<AppState>,
    Json(req): Json<AddProviderRequest>,
) -> impl IntoResponse {
    tracing::Span::current().record("provider_name", &req.name);

    // Validate: empty api_keys is rejected only for static mode — oauth
    // uses a single client-credentials grant and allows an empty key pool.
    if req.auth_mode == AuthMode::Static && req.api_keys.is_empty() {
        return AppError::BadRequest("at least one api_key required".into()).into_response();
    }
    // Validate: oauth mode must ship an oauth section (early 400; the
    // coordinator's full validate would also catch it).
    if req.auth_mode == AuthMode::OAuth && req.oauth.is_none() {
        return AppError::BadRequest(
            "auth_mode='oauth' requires an [providers.<name>.oauth] section".into(),
        )
        .into_response();
    }
    // Validate: the `apis` catalog must not be empty (the coordinator's
    // full validate additionally requires a protocol key + fallback rules).
    if req.apis.is_empty() {
        return AppError::BadRequest(
            "at least one api section required, e.g. apis.chat_completions with its base_url"
                .into(),
        )
        .into_response();
    }

    let mut apis = req.apis;
    normalize_apis(&mut apis);
    let echo_apis = apis.clone();

    // Static mode never persists an oauth section (matches load-time
    // semantics: the section is ignored under `auth_mode = "static"`).
    let oauth = if req.auth_mode == AuthMode::OAuth {
        req.oauth
    } else {
        None
    };

    let pc = ProviderConfig {
        api_keys: req.api_keys.clone(),
        key_strategy: req.key_strategy,
        auth_mode: req.auth_mode,
        oauth,
        apis,
        fallback: req.fallback,
        proxy_mode: req.proxy_mode,
        timeout_secs: req.timeout_secs,
        headers: req.headers.clone(),
        body_overrides: req.body_overrides.clone(),
        model_overrides: req.model_overrides.clone(),
        strip_fields: req.strip_fields.clone(),
        model_strip_fields: req.model_strip_fields.clone(),
        role_mappings: req.role_mappings.clone(),
        enabled_models: req.enabled_models.clone(),
        enabled: true,
        anthropic_cache_control: true,
        chunk_timeout_secs: None,
        ttfb_timeout_secs: None,
    };

    // Delegate to the coordinator which atomically writes config + provider
    // registry + version, and persists to disk.
    match state.coordinator.add_provider(req.name.clone(), pc).await {
        Ok(_report) => (
            StatusCode::CREATED,
            Json(json!({
                "name": req.name,
                "apis": echo_apis,
                "fallback": req.fallback,
            })),
        )
            .into_response(),
        Err(e) => match e {
            // A config-invalid provider (e.g. `proxy_mode = "enabled"` with
            // no global `proxy_url`) is a request-level error, not a server
            // fault: report 400 rather than 500.
            AppError::Config(msg) => AppError::BadRequest(msg).into_response(),
            other => other.into_response(),
        },
    }
}

/// PUT /api/providers — update an existing provider's configuration.
#[instrument(skip(state), fields(provider_name))]
pub async fn update_provider(
    State(state): State<AppState>,
    Json(req): Json<UpdateProviderRequest>,
) -> impl IntoResponse {
    tracing::Span::current().record("provider_name", &req.name);

    // 1. Read the current config, clone the existing provider, merge fields.
    let provider_config = {
        let config = state.config.read().await;

        let existing = match config.providers.get(&req.name) {
            Some(p) => p,
            None => {
                return AppError::NotFound(format!("provider '{}' not found", req.name))
                    .into_response();
            }
        };

        let mut pc = existing.clone();

        if let Some(v) = req.api_keys {
            pc.api_keys = v;
        }
        if let Some(v) = req.key_strategy {
            pc.key_strategy = v;
        }
        if let Some(v) = req.proxy_mode {
            pc.proxy_mode = v;
        }
        if let Some(apis) = req.apis {
            pc.apis = apis;
            normalize_apis(&mut pc.apis);
        }
        if let Some(patch) = req.fallback {
            pc.fallback = patch;
        }
        if let Some(v) = req.timeout_secs {
            pc.timeout_secs = v;
        }
        if let Some(v) = req.headers {
            pc.headers = v;
        }
        if let Some(v) = req.body_overrides {
            pc.body_overrides = v;
        }
        if let Some(v) = req.strip_fields {
            pc.strip_fields = v;
        }
        if let Some(v) = req.model_strip_fields {
            pc.model_strip_fields = v;
        }
        if let Some(v) = req.model_overrides {
            pc.model_overrides = v;
        }
        if let Some(v) = req.role_mappings {
            pc.role_mappings = v;
        }
        if let Some(v) = req.enabled_models {
            pc.enabled_models = v;
        }
        if let Some(v) = req.enabled {
            pc.enabled = v;
        }

        // ── auth_mode / oauth patch ──
        // - `auth_mode` omitted → keep; `static` → clear oauth; `oauth` →
        //   require a usable oauth section after merge.
        // - `oauth` omitted keeps the stored section; `null` clears it;
        //   value replaces the whole section. A `"***"` `client_secret`
        //   keeps the stored plaintext (admin out-path never returns it).
        if let Some(mode) = req.auth_mode {
            pc.auth_mode = mode;
        }
        if pc.auth_mode == AuthMode::Static {
            pc.oauth = None;
        } else {
            if let Some(patch) = req.oauth {
                match patch {
                    Some(mut oauth) => {
                        let existing_secret = pc.oauth.as_ref().map(|o| o.client_secret.as_str());
                        oauth.client_secret =
                            resolve_oauth_secret(&oauth.client_secret, existing_secret);
                        pc.oauth = Some(oauth);
                    }
                    None => pc.oauth = None,
                }
            }
            if pc.oauth.is_none() {
                return AppError::BadRequest(format!(
                    "auth_mode='oauth' requires an [providers.{}] oauth section",
                    req.name
                ))
                .into_response();
            }
        }

        pc
    };

    // 2. Delegate to the coordinator which atomically writes config + provider
    //    registry + version, and persists to disk.
    match state
        .coordinator
        .update_provider(&req.name, provider_config)
        .await
    {
        Ok(_report) => (StatusCode::OK, Json(json!({ "name": req.name }))).into_response(),
        Err(e) => match e {
            // A config-invalid provider (e.g. `proxy_mode = "enabled"` with
            // no global `proxy_url`) is a request-level error, not a server
            // fault: report 400 rather than 500.
            AppError::Config(msg) => AppError::BadRequest(msg).into_response(),
            other => other.into_response(),
        },
    }
}

/// GET /api/providers/{name}/models — list models enabled for this provider.
/// Returns the provider's configured `enabled_models` directly without
/// making an upstream request.
pub async fn list_provider_models(
    State(state): State<AppState>,
    Path(provider_name): Path<String>,
) -> impl IntoResponse {
    let config = state.config.read().await;
    let pc = match config.providers.get(&provider_name) {
        Some(p) => p,
        None => {
            return AppError::NotFound(format!("provider '{provider_name}' not found"))
                .into_response();
        }
    };

    (StatusCode::OK, Json(json!({ "models": pc.enabled_models }))).into_response()
}

/// DELETE /api/providers/{name} — remove a provider from the gateway.
///
/// Cascading behavior:
/// 1. Removes the provider entry from `config.providers`.
/// 2. Removes any `RouteTarget` in `config.routes[].targets` that references
///    the deleted provider. Routes that become empty are also dropped, since
///    an empty `targets` list is functionally equivalent to "no route" and
///    would silently swallow matching model requests with no fallback.
/// 3. Persists the updated config to disk if the config is file-backed.
/// 4. Removes the runtime provider adapter from the in-memory registry so
///    in-flight lookups immediately fail-soft (the next request matching a
///    removed target will surface "provider not found" instead of dispatching
///    to a deleted adapter).
#[instrument(skip(state), fields(provider_name))]
pub async fn delete_provider(
    State(state): State<AppState>,
    Json(req): Json<DeleteProviderRequest>,
) -> impl IntoResponse {
    let provider_name = req.name;
    tracing::Span::current().record("provider_name", &provider_name);

    // Delegate to the coordinator which atomically removes the provider
    // from config + registry + version, handles cascade, and persists.
    match state.coordinator.delete_provider(&provider_name).await {
        Ok((_report, cascade)) => (
            StatusCode::OK,
            Json(json!({
                "name": provider_name,
                "cascaded_routes": cascade.cascaded_routes,
                "deleted_routes": cascade.deleted_routes,
                "removed_targets": cascade.removed_targets,
            })),
        )
            .into_response(),
        // Post-delete full-config validation (e.g. deleting the last
        // provider leaves an invalid empty set) surfaces as
        // `AppError::Config`; report 400 not 500.
        Err(AppError::Config(msg)) => AppError::BadRequest(msg).into_response(),
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{
        build_state, build_state_with_proxy, one_provider_config, test_app, test_provider,
    };
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use sag_config::{ApiKind, Pipeline, ProxyMode, RouteConfig, RouteTarget, Strategy};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    async fn provider_names_from_response(response: axum::response::Response) -> Vec<String> {
        let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        body["providers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|provider| provider["name"].as_str().unwrap().to_string())
            .collect()
    }

    // 1. Happy path — keep a second provider so the post-delete config
    // still satisfies `GatewayConfig::validate` (at least one provider).
    #[tokio::test]
    async fn delete_provider_success() {
        let (mut providers, routes) = one_provider_config("openai", vec![("gpt-4o", "gpt-4o", 1)]);
        let (extra, _) = one_provider_config("anthropic", vec![]);
        providers.extend(extra);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());
        let _ = &state; // keep state alive

        let req = Request::builder()
            .method("DELETE")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"openai"}"#))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["name"], "openai");
        assert_eq!(body["removed_targets"], 1);
        assert_eq!(body["cascaded_routes"], serde_json::json!(["gpt-4o"]));
        assert_eq!(body["deleted_routes"], serde_json::json!(["gpt-4o"]));

        let cfg = state.config.read().await;
        assert!(!cfg.providers.contains_key("openai"));
        assert!(cfg.providers.contains_key("anthropic"));
        assert!(cfg.routes.is_empty());
        let reg = state.providers.read().await;
        assert!(!reg.contains_key("openai"));
    }

    // 2. 404
    #[tokio::test]
    async fn delete_provider_not_found() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());
        let _ = &state;

        let req = Request::builder()
            .method("DELETE")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"nonexistent"}"#))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["error"]["code"], "NOT_FOUND");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("nonexistent")
        );

        let cfg = state.config.read().await;
        assert!(cfg.providers.contains_key("openai"));
    }

    // 3. Cascade
    #[tokio::test]
    async fn delete_provider_cascades_routes() {
        let mut providers = HashMap::new();
        for (name, kind) in [
            ("openai", ApiKind::ChatCompletions),
            ("anthropic", ApiKind::AnthropicMessages),
        ] {
            providers.insert(
                name.to_string(),
                test_provider(
                    kind,
                    "https://example.com",
                    vec![format!("sk-{name}")],
                    vec![],
                ),
            );
        }

        let routes = vec![
            RouteConfig {
                model_name: "gpt-4o".into(),
                pipeline: Pipeline::Chat,
                api_endpoint: None,
                retries: None,
                timeout_secs: None,
                strategy: Strategy::FillFirst,
                targets: vec![
                    RouteTarget {
                        provider: "openai".into(),
                        real_model: Some("gpt-4o".into()),
                        priority: 1,
                        retries: None,
                        timeout_secs: None,
                        extra: HashMap::new(),
                        capabilities: None,
                    },
                    RouteTarget {
                        provider: "anthropic".into(),
                        real_model: Some("claude-3-5-sonnet".into()),
                        retries: None,
                        timeout_secs: None,
                        extra: HashMap::new(),
                        priority: 2,
                        capabilities: None,
                    },
                ],
            },
            RouteConfig {
                model_name: "claude-sonnet".into(),
                pipeline: Pipeline::Chat,
                api_endpoint: None,
                retries: None,
                timeout_secs: None,
                strategy: Strategy::FillFirst,
                targets: vec![RouteTarget {
                    provider: "anthropic".into(),
                    extra: HashMap::new(),
                    retries: None,
                    timeout_secs: None,
                    real_model: Some("claude-3-5-sonnet".into()),
                    priority: 1,
                    capabilities: None,
                }],
            },
        ];

        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());
        let _ = &state;

        let req = Request::builder()
            .method("DELETE")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"openai"}"#))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["removed_targets"], 1);
        assert_eq!(body["cascaded_routes"], serde_json::json!(["gpt-4o"]));
        assert_eq!(body["deleted_routes"], serde_json::json!([]));

        let cfg = state.config.read().await;
        assert!(!cfg.providers.contains_key("openai"));
        assert!(cfg.providers.contains_key("anthropic"));

        let gpt = cfg
            .routes
            .iter()
            .find(|r| r.model_name == "gpt-4o")
            .unwrap();
        assert_eq!(gpt.targets.len(), 1);
        assert_eq!(gpt.targets[0].provider, "anthropic");

        let claude = cfg
            .routes
            .iter()
            .find(|r| r.model_name == "claude-sonnet")
            .unwrap();
        assert_eq!(claude.targets.len(), 1);
        assert_eq!(claude.targets[0].provider, "anthropic");

        let reg = state.providers.read().await;
        assert!(!reg.contains_key("openai"));
        assert!(reg.contains_key("anthropic"));
    }

    // 4. Persistence
    #[tokio::test]
    async fn delete_provider_persists_to_disk() {
        use std::io::Write;
        let dir = std::env::temp_dir();
        let path = dir.join(format!("sag-delete-test-{}.toml", std::process::id()));

        let initial = r#"
[server]
host = "127.0.0.1"
port = 3000

[gateway_auth]
api_keys = ["test-key"]

[providers.openai]
api_keys = ["sk-test"]
[providers.openai.apis.chat_completions]
base_url = "https://api.openai.com/v1"

[providers.anthropic]
api_keys = ["sk-ant"]
[providers.anthropic.apis.anthropic_messages]
base_url = "https://api.anthropic.com"

[[routes]]
model_name = "gpt-4o"
targets = [
    { provider = "openai", real_model = "gpt-4o" },
    { provider = "anthropic", real_model = "claude-3-5-sonnet" },
]
"#;
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(initial.as_bytes()).unwrap();
        }

        let handle = sag_config::load_config(&path).unwrap();
        assert!(handle.is_file_backed());

        let cfg_read = handle.read().await;
        let mut registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
        for (name, pc) in cfg_read.providers.iter() {
            let provider: Arc<dyn sag_providers::LlmProvider> =
                Arc::new(sag_providers::OpenAiProvider::new(name.clone(), pc).unwrap());
            registry.insert(name.clone(), provider);
        }
        drop(cfg_read);

        let state =
            sag_state::testing::test_app_state(handle, Arc::new(RwLock::new(registry))).await;
        let app = test_app(state);

        let req = Request::builder()
            .method("DELETE")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"openai"}"#))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let content = std::fs::read_to_string(&path).unwrap();

        // Providers are serialized as sub-table format (`[providers.xxx]`).
        assert!(
            !content.contains("[providers.openai]"),
            "openai provider section should be removed: {content}"
        );
        assert!(
            content.contains("[providers.anthropic]"),
            "anthropic provider section should be preserved: {content}"
        );
        assert!(
            content.contains("base_url = \"https://api.anthropic.com\""),
            "anthropic apis section base_url should be preserved: {content}"
        );

        assert!(
            !content.contains("real_model = \"gpt-4o\""),
            "the openai→gpt-4o target should be removed: {content}"
        );
        assert!(
            content.contains("real_model = \"claude-3-5-sonnet\""),
            "the anthropic→claude target should be preserved: {content}"
        );

        let _ = std::fs::remove_file(&path);
    }

    // 5. Sanity: GET /api/providers still works
    #[tokio::test]
    async fn list_providers_still_works_after_adding_delete_route() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state);

        let req = Request::builder()
            .method("GET")
            .uri("/api/providers")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        let arr = body["providers"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "openai");
        assert_eq!(
            arr[0]["apis"]["chat_completions"]["base_url"],
            "https://api.openai.com/v1"
        );
    }

    #[tokio::test]
    async fn list_providers_filters_by_model_and_api_and_sorts_by_name() {
        let mut providers = HashMap::new();
        providers.insert(
            "z-openai".into(),
            test_provider(
                ApiKind::ChatCompletions,
                "https://api.openai.com/v1",
                vec!["sk-z".into()],
                vec!["gpt-4o".into()],
            ),
        );
        providers.insert(
            "a-anthropic".into(),
            test_provider(
                ApiKind::AnthropicMessages,
                "https://api.anthropic.com",
                vec!["sk-a".into()],
                vec!["gpt-4o".into()],
            ),
        );
        providers.insert(
            "m-openai".into(),
            test_provider(
                ApiKind::ChatCompletions,
                "https://example.com/v1",
                vec!["sk-m".into()],
                vec!["gpt-4o".into(), "gpt-4.1".into()],
            ),
        );

        let state = build_state(providers, vec![]).await;
        let app = test_app(state);

        // `apis=chat_completions` replaces the deleted `provider_type`
        // filter: the anthropic-only provider drops out.
        let req = Request::builder()
            .method("GET")
            .uri("/api/providers?model_name=gpt-4o&apis=chat_completions&sort=name")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let names = provider_names_from_response(response).await;
        assert_eq!(names, vec!["m-openai".to_string(), "z-openai".to_string()]);
    }

    #[tokio::test]
    async fn list_providers_model_filter_includes_unrestricted_providers() {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".into(),
            test_provider(
                ApiKind::ChatCompletions,
                "https://api.openai.com/v1",
                vec!["sk-openai".into()],
                vec![],
            ),
        );
        providers.insert(
            "restricted".into(),
            test_provider(
                ApiKind::ChatCompletions,
                "https://example.com/v1",
                vec!["sk-restricted".into()],
                vec!["other-model".into()],
            ),
        );

        let state = build_state(providers, vec![]).await;
        let app = test_app(state);

        let req = Request::builder()
            .method("GET")
            .uri("/api/providers?model_name=gpt-4o")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let names = provider_names_from_response(response).await;
        assert_eq!(names, vec!["openai".to_string()]);
    }

    // 6. Update provider — happy path
    #[tokio::test]
    async fn update_provider_success() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());
        let _ = &state;

        // `apis` replaces the whole directory (single section here, so the
        // provider stays valid); `models_endpoint` is per-section now and
        // normalized (`/v1/models/` → `v1/models`).
        let req = Request::builder()
            .method("PUT")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"name":"openai","timeout_secs":60,"apis":{"chat_completions":{"base_url":"https://custom.com/v1/","models_endpoint":"/v1/models/"}}}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        let p = cfg.providers.get("openai").unwrap();
        assert_eq!(p.apis.len(), 1);
        let chat = &p.apis[&ApiKind::ChatCompletions];
        assert_eq!(chat.base_url, "https://custom.com/v1");
        assert_eq!(chat.models_endpoint.as_deref(), Some("v1/models"));
        assert_eq!(p.timeout_secs, 60);
        // Fields not in the update should keep their original values
        assert_eq!(p.api_keys, vec!["sk-test"]);
        assert_eq!(p.key_strategy, KeyStrategy::FillFirst);
    }

    #[tokio::test]
    async fn update_provider_empty_models_endpoint_defaults() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());
        let _ = &state;

        let req = Request::builder()
            .method("PUT")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"name":"openai","apis":{"chat_completions":{"base_url":"https://api.openai.com/v1","models_endpoint":""}}}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        let p = cfg.providers.get("openai").unwrap();
        assert_eq!(
            p.apis[&ApiKind::ChatCompletions].models_endpoint.as_deref(),
            Some("models")
        );
    }

    // 7. Update provider — 404
    #[tokio::test]
    async fn update_provider_not_found() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());
        let _ = &state;

        let req = Request::builder()
            .method("PUT")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"name":"nonexistent","apis":{"chat_completions":{"base_url":"https://x.com"}}}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // 8. Update provider — update api_keys and swap the apis directory
    #[tokio::test]
    async fn update_provider_key_and_api() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());
        let _ = &state;

        let req = Request::builder()
            .method("PUT")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"name":"openai","api_keys":["sk-updated"],"apis":{"anthropic_messages":{"base_url":"https://api.anthropic.com"}}}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        let p = cfg.providers.get("openai").unwrap();
        assert_eq!(p.api_keys, vec!["sk-updated"]);
        assert_eq!(p.apis.len(), 1, "apis patch replaces the directory");
        assert!(
            !p.apis.contains_key(&ApiKind::ChatCompletions),
            "the previous chat section must be dropped by the directory patch"
        );
        let ant = &p.apis[&ApiKind::AnthropicMessages];
        assert_eq!(ant.base_url, "https://api.anthropic.com");
        assert_eq!(ant.models_endpoint, None, "provider default behavior");

        // Ensure the runtime registry was updated
        let reg = state.providers.read().await;
        assert!(reg.contains_key("openai"));
    }

    #[tokio::test]
    async fn add_and_update_provider_with_model_strip_fields() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        // 1. Add provider with model_strip_fields
        let add_payload = serde_json::json!({
            "name": "new_provider",
            "api_keys": ["sk-new"],
            "apis": {"chat_completions": {"base_url": "https://api.example.com/v1"}},
            "model_strip_fields": {
                "glm-5.2": ["chat_template_kwargs"],
                "qwen-2.5": ["extra_a"]
            }
        });
        let req = Request::builder()
            .method("POST")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(add_payload.to_string()))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        // Verify config state
        let cfg = state.config.read().await;
        let p = cfg.providers.get("new_provider").unwrap();
        assert_eq!(
            p.model_strip_fields.get("glm-5.2"),
            Some(&vec!["chat_template_kwargs".to_string()])
        );
        drop(cfg);

        // Verify GET /api/providers returns model_strip_fields
        let req_get = Request::builder()
            .method("GET")
            .uri("/api/providers")
            .body(Body::empty())
            .unwrap();
        let response_get = app.clone().oneshot(req_get).await.unwrap();
        let body_bytes = axum::body::to_bytes(response_get.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        let provider_info = body["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == "new_provider")
            .unwrap();
        assert_eq!(
            provider_info["model_strip_fields"]["glm-5.2"],
            serde_json::json!(["chat_template_kwargs"])
        );

        // 2. Update provider to clear model_strip_fields
        let update_payload = serde_json::json!({
            "name": "new_provider",
            "model_strip_fields": {}
        });
        let req2 = Request::builder()
            .method("PUT")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(update_payload.to_string()))
            .unwrap();
        let response2 = app.clone().oneshot(req2).await.unwrap();
        assert_eq!(response2.status(), StatusCode::OK);

        let cfg2 = state.config.read().await;
        let p2 = cfg2.providers.get("new_provider").unwrap();
        assert!(p2.model_strip_fields.is_empty());
    }

    // ── proxy_mode support ────────────────────────────────────────────

    #[test]
    fn add_provider_request_defaults_proxy_mode_to_inherit() {
        let req: AddProviderRequest =
            serde_json::from_str(r#"{"name":"p","api_keys":["k"]}"#).unwrap();
        assert_eq!(req.proxy_mode, ProxyMode::Inherit);

        let req: AddProviderRequest =
            serde_json::from_str(r#"{"name":"p","api_keys":["k"],"proxy_mode":"disabled"}"#)
                .unwrap();
        assert_eq!(req.proxy_mode, ProxyMode::Disabled);
    }

    #[test]
    fn update_provider_request_proxy_mode_is_optional_patch() {
        // Omitted -> keep current value (None patch).
        let req: UpdateProviderRequest = serde_json::from_str(r#"{"name":"p"}"#).unwrap();
        assert_eq!(req.proxy_mode, None);

        // Explicit null -> also keep current value.
        let req: UpdateProviderRequest =
            serde_json::from_str(r#"{"name":"p","proxy_mode":null}"#).unwrap();
        assert_eq!(req.proxy_mode, None);

        // Explicit value -> patch (including resetting to inherit).
        let req: UpdateProviderRequest =
            serde_json::from_str(r#"{"name":"p","proxy_mode":"inherit"}"#).unwrap();
        assert_eq!(req.proxy_mode, Some(ProxyMode::Inherit));
    }

    #[tokio::test]
    async fn add_provider_defaults_proxy_mode_to_inherit() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let req = Request::builder()
            .method("POST")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"name":"new_provider","api_keys":["sk-new"],"apis":{"chat_completions":{"base_url":"https://api.example.com/v1"}}}"#,
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let cfg = state.config.read().await;
        assert_eq!(
            cfg.providers.get("new_provider").unwrap().proxy_mode,
            ProxyMode::Inherit
        );
    }

    #[tokio::test]
    async fn add_provider_with_explicit_proxy_mode_and_info_echo() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let req = Request::builder()
            .method("POST")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"name":"new_provider","api_keys":["sk-new"],"apis":{"chat_completions":{"base_url":"https://api.example.com/v1"}},"proxy_mode":"disabled"}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let cfg = state.config.read().await;
        assert_eq!(
            cfg.providers.get("new_provider").unwrap().proxy_mode,
            ProxyMode::Disabled
        );
        drop(cfg);

        // GET /api/providers echoes proxy_mode for every provider.
        let req_get = Request::builder()
            .method("GET")
            .uri("/api/providers")
            .body(Body::empty())
            .unwrap();
        let response_get = app.oneshot(req_get).await.unwrap();
        let body_bytes = axum::body::to_bytes(response_get.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        let providers = body["providers"].as_array().unwrap();
        let new_provider = providers
            .iter()
            .find(|item| item["name"] == "new_provider")
            .unwrap();
        assert_eq!(new_provider["proxy_mode"], "disabled");
        let openai = providers
            .iter()
            .find(|item| item["name"] == "openai")
            .unwrap();
        assert_eq!(openai["proxy_mode"], "inherit");
    }

    #[tokio::test]
    async fn update_provider_proxy_mode_keeps_value_when_omitted() {
        // Start with a Disabled-mode provider; a patch that does not mention
        // proxy_mode must keep Disabled.
        let (mut providers, routes) = one_provider_config("openai", vec![]);
        providers.get_mut("openai").unwrap().proxy_mode = ProxyMode::Disabled;
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let req = Request::builder()
            .method("PUT")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"name":"openai","apis":{"chat_completions":{"base_url":"https://custom.com/v1"}}}"#,
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        let p = cfg.providers.get("openai").unwrap();
        assert_eq!(p.proxy_mode, ProxyMode::Disabled);
        assert_eq!(
            p.apis[&ApiKind::ChatCompletions].base_url,
            "https://custom.com/v1"
        );
    }

    #[tokio::test]
    async fn update_provider_proxy_mode_reset_to_inherit() {
        // Start with a Disabled-mode provider; an explicit
        // `"proxy_mode":"inherit"` patch resets it to Inherit. A global
        // proxy is configured so the later `enabled` switch is satisfiable.
        let (mut providers, routes) = one_provider_config("openai", vec![]);
        providers.get_mut("openai").unwrap().proxy_mode = ProxyMode::Disabled;
        let state =
            build_state_with_proxy(providers, routes, Some("http://proxy.example:8080".into()))
                .await;
        let app = test_app(state.clone());

        let req = Request::builder()
            .method("PUT")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"openai","proxy_mode":"inherit"}"#))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // Drop the read guard before the next PUT (which needs a write lock).
        {
            let cfg = state.config.read().await;
            assert_eq!(cfg.providers["openai"].proxy_mode, ProxyMode::Inherit);
        }

        // And a follow-up patch switching to enabled round-trips too.
        let req = Request::builder()
            .method("PUT")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"openai","proxy_mode":"enabled"}"#))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        assert_eq!(cfg.providers["openai"].proxy_mode, ProxyMode::Enabled);
    }

    // ── coordinator `AppError::Config` surfaces as 400 (not 500) ──────

    #[tokio::test]
    async fn add_provider_proxy_enabled_without_global_proxy_is_400() {
        // `proxy_mode = "enabled"` with no global `proxy_url` is rejected by
        // the coordinator with `AppError::Config`; the admin API must report
        // it as 400 BAD_REQUEST rather than 500.
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let req = Request::builder()
            .method("POST")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"name":"new_provider","api_keys":["sk-new"],"apis":{"chat_completions":{"base_url":"https://api.example.com/v1"}},"proxy_mode":"enabled"}"#,
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["error"]["code"], "BAD_REQUEST");

        // The invalid provider must not be committed.
        let cfg = state.config.read().await;
        assert!(!cfg.providers.contains_key("new_provider"));
        let reg = state.providers.read().await;
        assert!(!reg.contains_key("new_provider"));
    }

    #[tokio::test]
    async fn add_provider_proxy_enabled_with_global_proxy_succeeds() {
        // Sanity: the same request succeeds when a global proxy exists —
        // only genuine `AppError::Config` failures are remapped.
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state =
            build_state_with_proxy(providers, routes, Some("http://proxy.example:8080".into()))
                .await;
        let app = test_app(state.clone());

        let req = Request::builder()
            .method("POST")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"name":"new_provider","api_keys":["sk-new"],"apis":{"chat_completions":{"base_url":"https://api.example.com/v1"}},"proxy_mode":"enabled"}"#,
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let cfg = state.config.read().await;
        assert_eq!(cfg.providers["new_provider"].proxy_mode, ProxyMode::Enabled);
    }

    #[tokio::test]
    async fn update_provider_proxy_enabled_without_global_proxy_is_400_and_config_unchanged() {
        // Switching an existing provider to `proxy_mode = "enabled"` without
        // a global `proxy_url` must fail with 400 and leave config intact.
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let req = Request::builder()
            .method("PUT")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"openai","proxy_mode":"enabled"}"#))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["error"]["code"], "BAD_REQUEST");

        // The failed mutation must not commit.
        let cfg = state.config.read().await;
        assert_eq!(cfg.providers["openai"].proxy_mode, ProxyMode::Inherit);
    }

    // ── M7 rework: apis / fallback admin round-trip + illegal enum ──

    /// Round-trip an `apis` directory with all three protocol sections plus
    /// a `fallback`, and assert the read-side capability matrix is keyed by
    /// [`ApiKind`] with values taken from
    /// `sag_dispatch::dispatch::capability::target_capabilities`.
    #[tokio::test]
    async fn add_and_list_provider_round_trips_apis_fallback_and_capabilities() {
        let (providers, routes) = one_provider_config("openai", vec![("gpt-4o", "gpt-4o", 1)]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let req = Request::builder()
            .method("POST")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "name": "oa-multi",
                    "api_keys": ["sk-test"],
                    "apis": {
                        "chat_completions": {"base_url": "https://api.openai.com/v1"},
                        "responses": {"base_url": "https://api.openai.com/v1"},
                        "anthropic_messages": {"base_url": "https://api.anthropic.com"},
                    },
                    "fallback": "responses",
                })
                .to_string(),
            ))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/providers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let p = body["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "oa-multi")
            .expect("provider present");
        assert_eq!(p["fallback"], serde_json::json!("responses"));
        assert_eq!(
            p["apis"]["chat_completions"]["base_url"],
            "https://api.openai.com/v1"
        );
        assert_eq!(
            p["apis"]["responses"]["base_url"],
            "https://api.openai.com/v1"
        );
        assert_eq!(
            p["apis"]["anthropic_messages"]["base_url"],
            "https://api.anthropic.com"
        );

        // Capability matrix: keyed by ApiKind, values aligned with the
        // dispatch `target_capabilities` table (anthropic claims
        // hosted_tools; responses claims stateful + hosted_tools; chat
        // claims neither).
        let caps = &p["capabilities"];
        assert!(caps.is_object());
        assert_eq!(
            caps["chat_completions"]["hosted_tools"],
            serde_json::json!(false)
        );
        assert_eq!(
            caps["chat_completions"]["stateful"],
            serde_json::json!(false)
        );
        assert_eq!(
            caps["anthropic_messages"]["hosted_tools"],
            serde_json::json!(true)
        );
        assert_eq!(
            caps["anthropic_messages"]["stateful"],
            serde_json::json!(false)
        );
        assert_eq!(caps["responses"]["hosted_tools"], serde_json::json!(true));
        assert_eq!(caps["responses"]["stateful"], serde_json::json!(true));
        assert_eq!(
            caps["responses"]["documents"],
            serde_json::json!(false),
            "responses input schema takes no file parts"
        );
    }

    #[tokio::test]
    async fn update_provider_clears_fallback_with_null() {
        let (providers, routes) = one_provider_config("openai", vec![("gpt-4o", "gpt-4o", 1)]);
        let mut providers = providers;
        // A single-protocol provider may still declare an explicit fallback
        // (it must be one of the configured protocol keys).
        providers.get_mut("openai").unwrap().fallback = Some(ApiKind::ChatCompletions);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/providers")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "openai",
                            "fallback": null,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let config = state.config.read().await;
        assert_eq!(
            config.providers["openai"].fallback, None,
            "null patch must clear fallback back to the implicit default"
        );
    }

    /// The `apis` table key is a typed enum: an unknown key must fail
    /// deserialization (422), same strictness as every other typed config
    /// enum — while unknown *fields* stay tolerated.
    #[tokio::test]
    async fn add_provider_illegal_apis_key_is_422() {
        let (providers, routes) = one_provider_config("openai", vec![("gpt-4o", "gpt-4o", 1)]);
        let state = build_state(providers, routes).await;
        let app = test_app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/providers")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "bad",
                            "api_keys": ["sk"],
                            "apis": {"teleport": {"base_url": "https://x.com"}},
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown apis key must be 422"
        );
    }

    #[tokio::test]
    async fn add_provider_unknown_fields_are_tolerated() {
        let (providers, routes) = one_provider_config("openai", vec![("gpt-4o", "gpt-4o", 1)]);
        let state = build_state(providers, routes).await;
        let app = test_app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/providers")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "tolerant",
                            "api_keys": ["sk"],
                            "apis": {"chat_completions": {"base_url": "https://x.com/v1"}},
                            "some_future_field": {"nested": true},
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    // ── M7 rework: mutate() full validate rejects a bad fallback ──────

    /// B1: a provider declaring `fallback = "responses"` without a
    /// `responses` section in `apis` fails `GatewayConfig::validate` on the
    /// coordinator's post-mutation check. Must surface as 4xx and must NOT
    /// rewrite the on-disk config file.
    #[tokio::test]
    async fn add_provider_illegal_fallback_is_4xx_and_config_file_unchanged() {
        use crate::test_utils::build_file_backed_state;

        let path = std::env::temp_dir().join(format!(
            "sag_admin_illegal_fallback_{}.toml",
            uuid::Uuid::new_v4()
        ));
        let _ = std::fs::remove_file(&path);

        let (providers, routes) = one_provider_config("openai", vec![("gpt-4o", "gpt-4o", 1)]);
        let state = build_file_backed_state(&path, providers, routes).await;
        let app = test_app(state.clone());

        let bytes_before = std::fs::read(&path).expect("config file must exist after setup");

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/providers")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "oa-bad",
                            "api_keys": ["sk"],
                            "apis": {"chat_completions": {"base_url": "https://api.openai.com/v1"}},
                            "fallback": "responses",
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            resp.status().is_client_error(),
            "fallback outside the apis protocol keys must be 4xx, got {}",
            resp.status()
        );

        // Config file must be byte-identical (mutation never persisted).
        let bytes_after = std::fs::read(&path).expect("config file still readable");
        assert_eq!(
            bytes_before, bytes_after,
            "config file must not be rewritten on validate failure"
        );

        // In-memory config must also be unchanged.
        let cfg = state.config.read().await;
        assert!(
            !cfg.providers.contains_key("oa-bad"),
            "invalid provider must not commit in-memory either"
        );

        let _ = std::fs::remove_file(&path);
    }

    // ── end M7 ──

    // ── OAuth auth_mode CRUD (Phase 4) ────────────────────────────────

    fn oauth_section_json() -> serde_json::Value {
        serde_json::json!({
            "token_url": "https://login.example.com/oauth2/token",
            "client_id": "cid",
            "client_secret": "sec-plain",
            "scope": "scope-a",
            "audience": "https://api.example.com",
            "refresh_token": "rt-1",
        })
    }

    async fn list_provider_json(app: &axum::Router, name: &str) -> serde_json::Value {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/providers")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        body["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .expect("provider present in list")
            .clone()
    }

    async fn post_provider(
        app: &axum::Router,
        payload: serde_json::Value,
    ) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/providers")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn put_provider(
        app: &axum::Router,
        payload: serde_json::Value,
    ) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/providers")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    /// Round-trip: create an oauth provider, GET must never echo the
    /// plaintext `client_secret` — only `"***"` + `client_secret_set`.
    #[tokio::test]
    async fn oauth_provider_round_trip_masks_client_secret() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let resp = post_provider(
            &app,
            serde_json::json!({
                "name": "azure-oauth",
                "auth_mode": "oauth",
                "oauth": oauth_section_json(),
                "apis": {"chat_completions": {"base_url": "https://azure.example.com/v1"}},
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        let p = list_provider_json(&app, "azure-oauth").await;
        assert_eq!(p["auth_mode"], "oauth");
        assert_eq!(p["oauth"]["client_secret"], "***");
        assert_eq!(p["oauth"]["client_secret_set"], true);
        assert_eq!(p["oauth"]["client_id"], "cid");
        assert_eq!(
            p["oauth"]["token_url"],
            "https://login.example.com/oauth2/token"
        );
        assert_eq!(p["oauth"]["scope"], "scope-a");
        assert_eq!(p["oauth"]["audience"], "https://api.example.com");
        assert_eq!(p["oauth"]["refresh_token"], "rt-1");
        assert_eq!(p["api_keys"], serde_json::json!([]));

        let cfg = state.config.read().await;
        let oauth = cfg.providers["azure-oauth"]
            .oauth
            .as_ref()
            .expect("oauth section stored");
        assert_eq!(oauth.client_secret, "sec-plain");
        assert_eq!(cfg.providers["azure-oauth"].auth_mode, AuthMode::OAuth);
    }

    /// Switching `auth_mode` to `static` clears the stored oauth section.
    #[tokio::test]
    async fn oauth_update_switch_to_static_clears_oauth() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let resp = post_provider(
            &app,
            serde_json::json!({
                "name": "azure-oauth",
                "auth_mode": "oauth",
                "oauth": oauth_section_json(),
                "api_keys": [],
                "apis": {"chat_completions": {"base_url": "https://azure.example.com/v1"}},
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        // Give the new oauth provider a static key pool so the switch back
        // to static remains config-valid.
        let resp = put_provider(
            &app,
            serde_json::json!({
                "name": "azure-oauth",
                "auth_mode": "static",
                "api_keys": ["sk-static"],
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        let p = &cfg.providers["azure-oauth"];
        assert_eq!(p.auth_mode, AuthMode::Static);
        assert!(p.oauth.is_none(), "oauth section must be cleared on static");
        assert_eq!(p.api_keys, vec!["sk-static"]);
        drop(cfg);

        let p = list_provider_json(&app, "azure-oauth").await;
        assert_eq!(p["auth_mode"], "static");
        assert!(p["oauth"].is_null());
    }

    /// OAuth mode allows empty `api_keys` on create.
    #[tokio::test]
    async fn oauth_provider_allows_empty_api_keys() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let resp = post_provider(
            &app,
            serde_json::json!({
                "name": "oauth-empty-keys",
                "auth_mode": "oauth",
                "oauth": {
                    "token_url": "https://login.example.com/oauth2/token",
                    "client_id": "cid",
                    "client_secret": "sec",
                },
                "api_keys": [],
                "apis": {"chat_completions": {"base_url": "https://x.example.com/v1"}},
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        let cfg = state.config.read().await;
        let p = &cfg.providers["oauth-empty-keys"];
        assert_eq!(p.auth_mode, AuthMode::OAuth);
        assert!(p.api_keys.is_empty());
        assert!(p.oauth.is_some());
    }

    /// Update with `client_secret: "***"` keeps the stored secret.
    #[tokio::test]
    async fn oauth_update_masked_secret_keeps_original() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let resp = post_provider(
            &app,
            serde_json::json!({
                "name": "azure-oauth",
                "auth_mode": "oauth",
                "oauth": oauth_section_json(),
                "apis": {"chat_completions": {"base_url": "https://azure.example.com/v1"}},
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);

        let resp = put_provider(
            &app,
            serde_json::json!({
                "name": "azure-oauth",
                // Whole-section replace: the UI edit form always sends the
                // full oauth object; `client_secret: "***"` keeps the
                // stored secret.
                "oauth": {
                    "token_url": "https://login.example.com/oauth2/token-v2",
                    "client_id": "cid-new",
                    "client_secret": "***",
                    "scope": "scope-b",
                    "audience": "https://api.example.com",
                    "refresh_token": "rt-1",
                },
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        let oauth = cfg.providers["azure-oauth"]
            .oauth
            .as_ref()
            .expect("oauth kept");
        assert_eq!(
            oauth.client_secret, "sec-plain",
            "mask must not overwrite secret"
        );
        assert_eq!(oauth.client_id, "cid-new");
        assert_eq!(oauth.token_url, "https://login.example.com/oauth2/token-v2");
        assert_eq!(oauth.scope.as_deref(), Some("scope-b"));
        assert_eq!(oauth.audience.as_deref(), Some("https://api.example.com"));
        assert_eq!(oauth.refresh_token.as_deref(), Some("rt-1"));

        // And a real new secret still updates.
        drop(cfg);
        let resp = put_provider(
            &app,
            serde_json::json!({
                "name": "azure-oauth",
                "oauth": {
                    "token_url": "https://login.example.com/oauth2/token-v2",
                    "client_id": "cid-new",
                    "client_secret": "sec-rotated",
                    "scope": "scope-b",
                    "audience": "https://api.example.com",
                    "refresh_token": "rt-1",
                },
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let cfg = state.config.read().await;
        assert_eq!(
            cfg.providers["azure-oauth"]
                .oauth
                .as_ref()
                .unwrap()
                .client_secret,
            "sec-rotated"
        );
    }

    /// Create with `auth_mode = "oauth"` and no oauth section → 400.
    #[tokio::test]
    async fn oauth_provider_missing_section_fails_create() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let resp = post_provider(
            &app,
            serde_json::json!({
                "name": "oauth-missing",
                "auth_mode": "oauth",
                "apis": {"chat_completions": {"base_url": "https://x.example.com/v1"}},
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"]["code"], "BAD_REQUEST");
        assert!(
            body["error"]["message"].as_str().unwrap().contains("oauth"),
            "error should mention oauth section requirement"
        );

        let cfg = state.config.read().await;
        assert!(!cfg.providers.contains_key("oauth-missing"));
    }

    /// Static mode still rejects empty api_keys on create.
    #[tokio::test]
    async fn static_provider_still_rejects_empty_api_keys_on_create() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let resp = post_provider(
            &app,
            serde_json::json!({
                "name": "static-empty",
                "auth_mode": "static",
                "apis": {"chat_completions": {"base_url": "https://x.example.com/v1"}},
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let cfg = state.config.read().await;
        assert!(!cfg.providers.contains_key("static-empty"));
    }

    /// Update that sets `auth_mode = "oauth"` without an oauth section →
    /// 400 and config unchanged.
    #[tokio::test]
    async fn oauth_update_switch_to_oauth_without_section_fails() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let resp = put_provider(
            &app,
            serde_json::json!({
                "name": "openai",
                "auth_mode": "oauth",
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let cfg = state.config.read().await;
        assert_eq!(cfg.providers["openai"].auth_mode, AuthMode::Static);
        assert!(cfg.providers["openai"].oauth.is_none());
    }

    /// Unknown `auth_mode` value fails deserialization (422).
    #[tokio::test]
    async fn oauth_unknown_auth_mode_is_422() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state);

        let resp = post_provider(
            &app,
            serde_json::json!({
                "name": "bad-mode",
                "auth_mode": "jwt",
                "oauth": oauth_section_json(),
                "apis": {"chat_completions": {"base_url": "https://x.example.com/v1"}},
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}
