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
use sag_config::{KeyStrategy, ProviderConfig, ProviderType, ProxyMode};
use sag_core::AppError;
use serde_json::json;
use tracing::instrument;

use sag_state::AppState;

#[derive(Debug, serde::Deserialize)]
pub struct AddProviderRequest {
    /// Provider name (must be unique).
    pub name: String,
    #[serde(default)]
    pub api_keys: Vec<String>,
    #[serde(default)]
    pub key_strategy: KeyStrategy,
    #[serde(default)]
    pub provider_type: ProviderType,
    #[serde(default = "sag_config::default_base_url")]
    pub base_url: String,
    #[serde(default = "sag_config::default_models_endpoint")]
    pub models_endpoint: String,
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
    pub provider_type: Option<ProviderType>,
    pub base_url: Option<String>,
    pub models_endpoint: Option<String>,
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
    pub provider_type: String,
    pub base_url: String,
    pub models_endpoint: String,
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
}

#[derive(Debug, serde::Deserialize)]
pub struct ListProvidersQuery {
    #[serde(default)]
    pub model_name: Option<String>,
    #[serde(default)]
    pub provider_type: Option<ProviderType>,
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

    let matches_type = query
        .provider_type
        .map(|provider_type| cfg.provider_type == provider_type)
        .unwrap_or(true);

    matches_model && matches_type
}

fn provider_info(name: &str, cfg: &ProviderConfig) -> ProviderInfo {
    ProviderInfo {
        name: name.to_string(),
        api_keys: cfg.api_keys.clone(),
        key_strategy: cfg.key_strategy,
        provider_type: cfg.provider_type.as_str().to_string(),
        base_url: cfg.base_url.clone(),
        models_endpoint: cfg.models_endpoint.clone(),
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
#[instrument(skip(state), fields(provider_name, provider_type))]
pub async fn add_provider(
    State(state): State<AppState>,
    Json(req): Json<AddProviderRequest>,
) -> impl IntoResponse {
    tracing::Span::current().record("provider_name", &req.name);
    tracing::Span::current().record("provider_type", req.provider_type.as_str());

    // Validate: at least one API key is required (early check).
    if req.api_keys.is_empty() {
        return AppError::BadRequest("at least one api_key required".into()).into_response();
    }

    let pc = ProviderConfig {
        provider_type: req.provider_type,
        api_keys: req.api_keys.clone(),
        key_strategy: req.key_strategy,
        base_url: req.base_url.clone(),
        proxy_mode: req.proxy_mode,
        models_endpoint: sag_config::normalize_models_endpoint(&req.models_endpoint),
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
                "provider_type": req.provider_type.as_str(),
                "base_url": req.base_url,
                "models_endpoint": sag_config::normalize_models_endpoint(&req.models_endpoint),
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
        if let Some(v) = req.provider_type {
            pc.provider_type = v;
        }
        if let Some(v) = req.base_url {
            pc.base_url = v;
        }
        if let Some(v) = req.models_endpoint {
            pc.models_endpoint = sag_config::normalize_models_endpoint(&v);
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
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{build_state, build_state_with_proxy, one_provider_config, test_app};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use sag_config::{ProviderConfig, ProviderType, ProxyMode, RouteConfig, RouteTarget, Strategy};
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

    // 1. Happy path
    #[tokio::test]
    async fn delete_provider_success() {
        let (providers, routes) = one_provider_config("openai", vec![("gpt-4o", "gpt-4o", 1)]);
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
        for name in ["openai", "anthropic"] {
            providers.insert(
                name.to_string(),
                ProviderConfig {
                    provider_type: if name == "openai" {
                        ProviderType::OpenAI
                    } else {
                        ProviderType::Anthropic
                    },
                    api_keys: vec![format!("sk-{name}")],
                    key_strategy: KeyStrategy::FillFirst,
                    base_url: "https://example.com".into(),
                    proxy_mode: ProxyMode::default(),
                    models_endpoint: "models".into(),
                    timeout_secs: 120,
                    headers: HashMap::new(),
                    body_overrides: HashMap::new(),
                    strip_fields: vec![],
                    model_strip_fields: HashMap::new(),
                    model_overrides: HashMap::new(),
                    role_mappings: HashMap::new(),
                    enabled_models: vec![],
                    enabled: true,
                    anthropic_cache_control: true,
                    chunk_timeout_secs: None,
                    ttfb_timeout_secs: None,
                },
            );
        }

        let routes = vec![
            RouteConfig {
                model_name: "gpt-4o".into(),
                api_endpoint: "/chat/completions".into(),
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
                    },
                    RouteTarget {
                        provider: "anthropic".into(),
                        real_model: Some("claude-3-5-sonnet".into()),
                        retries: None,
                        timeout_secs: None,
                        extra: HashMap::new(),
                        priority: 2,
                    },
                ],
            },
            RouteConfig {
                model_name: "claude-sonnet".into(),
                api_endpoint: "/chat/completions".into(),
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
base_url = "https://api.openai.com/v1"

[providers.anthropic]
provider_type = "anthropic"
api_keys = ["sk-ant"]
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
            content.contains("provider_type = \"anthropic\""),
            "anthropic provider_type should be present: {content}"
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
        assert_eq!(arr[0]["models_endpoint"], "models");
    }

    #[tokio::test]
    async fn list_providers_filters_by_model_and_type_and_sorts_by_name() {
        let mut providers = HashMap::new();
        providers.insert(
            "z-openai".into(),
            ProviderConfig {
                provider_type: ProviderType::OpenAI,
                api_keys: vec!["sk-z".into()],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://api.openai.com/v1".into(),
                proxy_mode: ProxyMode::default(),
                models_endpoint: "models".into(),
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                strip_fields: vec![],
                model_strip_fields: HashMap::new(),
                model_overrides: HashMap::new(),
                role_mappings: HashMap::new(),
                enabled_models: vec!["gpt-4o".into()],
                enabled: true,
                anthropic_cache_control: true,
                chunk_timeout_secs: None,
                ttfb_timeout_secs: None,
            },
        );
        providers.insert(
            "a-anthropic".into(),
            ProviderConfig {
                provider_type: ProviderType::Anthropic,
                api_keys: vec!["sk-a".into()],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://api.anthropic.com".into(),
                proxy_mode: ProxyMode::default(),
                models_endpoint: "models".into(),
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                strip_fields: vec![],
                model_strip_fields: HashMap::new(),
                model_overrides: HashMap::new(),
                role_mappings: HashMap::new(),
                enabled_models: vec!["gpt-4o".into()],
                enabled: true,
                anthropic_cache_control: true,
                chunk_timeout_secs: None,
                ttfb_timeout_secs: None,
            },
        );
        providers.insert(
            "m-openai".into(),
            ProviderConfig {
                provider_type: ProviderType::OpenAI,
                api_keys: vec!["sk-m".into()],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://example.com/v1".into(),
                proxy_mode: ProxyMode::default(),
                models_endpoint: "models".into(),
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                strip_fields: vec![],
                model_strip_fields: HashMap::new(),
                model_overrides: HashMap::new(),
                role_mappings: HashMap::new(),
                enabled_models: vec!["gpt-4o".into(), "gpt-4.1".into()],
                enabled: true,
                anthropic_cache_control: true,
                chunk_timeout_secs: None,
                ttfb_timeout_secs: None,
            },
        );

        let state = build_state(providers, vec![]).await;
        let app = test_app(state);

        let req = Request::builder()
            .method("GET")
            .uri("/api/providers?model_name=gpt-4o&provider_type=openai&sort=name")
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
            ProviderConfig {
                provider_type: ProviderType::OpenAI,
                api_keys: vec!["sk-openai".into()],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://api.openai.com/v1".into(),
                proxy_mode: ProxyMode::default(),
                models_endpoint: "models".into(),
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                strip_fields: vec![],
                model_strip_fields: HashMap::new(),
                model_overrides: HashMap::new(),
                role_mappings: HashMap::new(),
                enabled_models: vec![],
                enabled: true,
                anthropic_cache_control: true,
                chunk_timeout_secs: None,
                ttfb_timeout_secs: None,
            },
        );
        providers.insert(
            "restricted".into(),
            ProviderConfig {
                provider_type: ProviderType::OpenAI,
                api_keys: vec!["sk-restricted".into()],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://example.com/v1".into(),
                proxy_mode: ProxyMode::default(),
                models_endpoint: "models".into(),
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                strip_fields: vec![],
                model_strip_fields: HashMap::new(),
                model_overrides: HashMap::new(),
                role_mappings: HashMap::new(),
                enabled_models: vec!["other-model".into()],
                enabled: true,
                anthropic_cache_control: true,
                chunk_timeout_secs: None,
                ttfb_timeout_secs: None,
            },
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

        let req = Request::builder()
            .method("PUT")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"name":"openai","base_url":"https://custom.com/v1","timeout_secs":60,"models_endpoint":"/v1/models/"}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        let p = cfg.providers.get("openai").unwrap();
        assert_eq!(p.base_url, "https://custom.com/v1");
        assert_eq!(p.models_endpoint, "v1/models");
        assert_eq!(p.timeout_secs, 60);
        // Fields not in the update should keep their original values
        assert_eq!(p.api_keys, vec!["sk-test"]);
        assert_eq!(p.provider_type, ProviderType::OpenAI);
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
            .body(Body::from(r#"{"name":"openai","models_endpoint":""}"#))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        let p = cfg.providers.get("openai").unwrap();
        assert_eq!(p.models_endpoint, "models");
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
                r#"{"name":"nonexistent","base_url":"https://x.com"}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // 8. Update provider — update api_key and provider_type
    #[tokio::test]
    async fn update_provider_key_and_type() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());
        let _ = &state;

        let req = Request::builder()
            .method("PUT")
            .uri("/api/providers")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"name":"openai","api_keys":["sk-updated"],"provider_type":"anthropic","base_url":"https://api.anthropic.com"}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        let p = cfg.providers.get("openai").unwrap();
        assert_eq!(p.api_keys, vec!["sk-updated"]);
        assert_eq!(p.provider_type, ProviderType::Anthropic);
        assert_eq!(p.base_url, "https://api.anthropic.com");
        assert_eq!(p.models_endpoint, "models");

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
            "provider_type": "openai",
            "api_keys": ["sk-new"],
            "base_url": "https://api.example.com/v1",
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
                r#"{"name":"new_provider","provider_type":"openai","api_keys":["sk-new"],"base_url":"https://api.example.com/v1"}"#,
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
                r#"{"name":"new_provider","provider_type":"openai","api_keys":["sk-new"],"base_url":"https://api.example.com/v1","proxy_mode":"disabled"}"#,
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
                r#"{"name":"openai","base_url":"https://custom.com/v1"}"#,
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        let p = cfg.providers.get("openai").unwrap();
        assert_eq!(p.proxy_mode, ProxyMode::Disabled);
        assert_eq!(p.base_url, "https://custom.com/v1");
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
                r#"{"name":"new_provider","provider_type":"openai","api_keys":["sk-new"],"base_url":"https://api.example.com/v1","proxy_mode":"enabled"}"#,
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
                r#"{"name":"new_provider","provider_type":"openai","api_keys":["sk-new"],"base_url":"https://api.example.com/v1","proxy_mode":"enabled"}"#,
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
}
