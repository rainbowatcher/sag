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
use sag_config::{KeyStrategy, ProviderConfig, ProviderType};
use sag_core::AppError;
use serde_json::json;
use tracing::{instrument, warn};

use sag_dispatch::AppState;

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
    #[serde(default = "sag_config::default_timeout_secs")]
    pub timeout_secs: u64,
    /// Custom headers to override upstream request headers.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Body field overrides merged into the upstream JSON request.
    #[serde(default)]
    pub body_overrides: HashMap<String, serde_json::Value>,
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
    pub provider_type: Option<ProviderType>,
    pub base_url: Option<String>,
    pub models_endpoint: Option<String>,
    pub timeout_secs: Option<u64>,
    pub headers: Option<HashMap<String, String>>,
    pub body_overrides: Option<HashMap<String, serde_json::Value>>,
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
    pub timeout_secs: u64,
    pub headers: HashMap<String, String>,
    pub body_overrides: HashMap<String, serde_json::Value>,
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
        timeout_secs: cfg.timeout_secs,
        headers: cfg.headers.clone(),
        body_overrides: cfg.body_overrides.clone(),
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
    // 1. Write-lock config, insert provider, and persist to disk
    let provider_config = {
        let mut config = state.config.write().await;

        if config.providers.contains_key(&req.name) {
            return AppError::Conflict(format!("provider '{}' already exists", req.name))
                .into_response();
        }

        if req.api_keys.is_empty() {
            return AppError::BadRequest("at least one api_key required".into()).into_response();
        }

        let pc = ProviderConfig {
            provider_type: req.provider_type,
            api_keys: req.api_keys.clone(),
            key_strategy: req.key_strategy,
            base_url: req.base_url.clone(),
            models_endpoint: sag_config::normalize_models_endpoint(&req.models_endpoint),
            timeout_secs: req.timeout_secs,
            headers: req.headers.clone(),
            body_overrides: req.body_overrides.clone(),
            role_mappings: req.role_mappings.clone(),
            enabled_models: req.enabled_models.clone(),
            enabled: true,
            anthropic_cache_control: true,
        };

        config.providers.insert(req.name.clone(), pc.clone());

        // Persist to disk while holding the write lock
        if let Some(path) = state.config.path()
            && let Err(e) = sag_config::save_config_to_file(&config, path)
        {
            warn!(?e, "failed to persist config after adding provider");
        }

        pc
    };

    // 2. Build the provider adapter and register it
    let provider = sag_providers::create_provider(req.name.clone(), &provider_config);

    {
        let mut providers = state.providers.write().await;
        providers.insert(req.name.clone(), provider);
    }

    (
        StatusCode::CREATED,
        Json(json!({
            "name": req.name,
            "provider_type":            provider_config.provider_type.as_str(),
            "base_url": provider_config.base_url,
            "models_endpoint": provider_config.models_endpoint,
        })),
    )
        .into_response()
}

/// PUT /api/providers — update an existing provider's configuration.
#[instrument(skip(state), fields(provider_name))]
pub async fn update_provider(
    State(state): State<AppState>,
    Json(req): Json<UpdateProviderRequest>,
) -> impl IntoResponse {
    tracing::Span::current().record("provider_name", &req.name);
    let provider_config = {
        let mut config = state.config.write().await;

        let existing = match config.providers.get_mut(&req.name) {
            Some(p) => p,
            None => {
                return AppError::NotFound(format!("provider '{}' not found", req.name))
                    .into_response();
            }
        };

        if let Some(v) = req.api_keys {
            existing.api_keys = v;
        }
        if let Some(v) = req.key_strategy {
            existing.key_strategy = v;
        }
        if let Some(v) = req.provider_type {
            existing.provider_type = v;
        }
        if let Some(v) = req.base_url {
            existing.base_url = v;
        }
        if let Some(v) = req.models_endpoint {
            existing.models_endpoint = sag_config::normalize_models_endpoint(&v);
        }
        if let Some(v) = req.timeout_secs {
            existing.timeout_secs = v;
        }
        if let Some(v) = req.headers {
            existing.headers = v;
        }
        if let Some(v) = req.body_overrides {
            existing.body_overrides = v;
        }
        if let Some(v) = req.role_mappings {
            existing.role_mappings = v;
        }
        if let Some(v) = req.enabled_models {
            existing.enabled_models = v;
        }
        if let Some(v) = req.enabled {
            existing.enabled = v;
        }

        let pc = existing.clone();

        // Persist to disk while holding the write lock
        if let Some(path) = state.config.path()
            && let Err(e) = sag_config::save_config_to_file(&config, path)
        {
            warn!(?e, "failed to persist config after updating provider");
        }

        pc
    };

    // Rebuild the provider adapter and register it
    let provider = sag_providers::create_provider(req.name.clone(), &provider_config);

    {
        let mut providers = state.providers.write().await;
        providers.insert(req.name.clone(), provider);
    }

    (
        StatusCode::OK,
        Json(json!({
            "name": req.name,
            "provider_type":            provider_config.provider_type.as_str(),
            "base_url": provider_config.base_url,
            "models_endpoint": provider_config.models_endpoint,
            "timeout_secs": provider_config.timeout_secs,
            "headers": provider_config.headers,
            "body_overrides": provider_config.body_overrides,
            "role_mappings": provider_config.role_mappings,
            "enabled_models": provider_config.enabled_models,
        })),
    )
        .into_response()
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
    // ── Stage 1: update config + persist ─────────────────────────────
    // Collect (model_name, removed_target_count) for every route that lost
    // at least one target, so we can report it back to the caller.
    let (cascaded_routes, removed_targets) = {
        let mut config = state.config.write().await;

        if !config.providers.contains_key(&provider_name) {
            return AppError::NotFound(format!("provider '{provider_name}' not found"))
                .into_response();
        }

        // Cascade: walk every route, drop targets pointing at the deleted
        // provider, and remember which routes were touched.
        let mut cascaded_routes: Vec<String> = Vec::new();
        let mut removed_targets: usize = 0;
        for route in config.routes.iter_mut() {
            let before = route.targets.len();
            route.targets.retain(|t| t.provider != provider_name);
            let dropped = before - route.targets.len();
            if dropped > 0 {
                removed_targets += dropped;
                cascaded_routes.push(route.model_name.clone());
            }
        }

        // Drop routes that ended up with no targets at all — keeping an
        // empty `targets` list would just create a dead route that matches
        // a model but has no fallback, silently failing every request.
        config.routes.retain(|r| !r.targets.is_empty());

        // Remove the provider from the config map.
        config.providers.remove(&provider_name);

        // Persist to disk while we still hold the write lock.
        if let Some(path) = state.config.path()
            && let Err(e) = sag_config::save_config_to_file(&config, path)
        {
            warn!(?e, "failed to persist config after deleting provider");
        }

        (cascaded_routes, removed_targets)
    };

    // ── Stage 2: remove the runtime adapter ──────────────────────────
    // Done after the config write lock is released to minimize lock
    // contention. Any in-flight request that already snapshotted the
    // Arc<dyn LlmProvider> will continue to use it; new lookups will see
    // the absence and surface a clear "provider not found" error.
    {
        let mut providers = state.providers.write().await;
        providers.remove(&provider_name);
    }

    (
        StatusCode::OK,
        Json(json!({
            "name": provider_name,
            "cascaded_routes": cascaded_routes,
            "removed_targets": removed_targets,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{build_state, one_provider_config, test_app};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use sag_config::{ProviderConfig, ProviderType, RouteConfig, RouteTarget, Strategy};
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
                    models_endpoint: "models".into(),
                    timeout_secs: 120,
                    headers: HashMap::new(),
                    body_overrides: HashMap::new(),
                    role_mappings: HashMap::new(),
                    enabled_models: vec![],
                    enabled: true,
                    anthropic_cache_control: true,
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
                Arc::new(sag_providers::OpenAiProvider::new(name.clone(), pc));
            registry.insert(name.clone(), provider);
        }
        drop(cfg_read);

        let state =
            sag_dispatch::testing::test_app_state(handle, Arc::new(RwLock::new(registry))).await;
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
                models_endpoint: "models".into(),
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                role_mappings: HashMap::new(),
                enabled_models: vec!["gpt-4o".into()],
                enabled: true,
                anthropic_cache_control: true,
            },
        );
        providers.insert(
            "a-anthropic".into(),
            ProviderConfig {
                provider_type: ProviderType::Anthropic,
                api_keys: vec!["sk-a".into()],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://api.anthropic.com".into(),
                models_endpoint: "models".into(),
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                role_mappings: HashMap::new(),
                enabled_models: vec!["gpt-4o".into()],
                enabled: true,
                anthropic_cache_control: true,
            },
        );
        providers.insert(
            "m-openai".into(),
            ProviderConfig {
                provider_type: ProviderType::OpenAI,
                api_keys: vec!["sk-m".into()],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://example.com/v1".into(),
                models_endpoint: "models".into(),
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                role_mappings: HashMap::new(),
                enabled_models: vec!["gpt-4o".into(), "gpt-4.1".into()],
                enabled: true,
                anthropic_cache_control: true,
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
                models_endpoint: "models".into(),
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                role_mappings: HashMap::new(),
                enabled_models: vec![],
                enabled: true,
                anthropic_cache_control: true,
            },
        );
        providers.insert(
            "restricted".into(),
            ProviderConfig {
                provider_type: ProviderType::OpenAI,
                api_keys: vec!["sk-restricted".into()],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://example.com/v1".into(),
                models_endpoint: "models".into(),
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                role_mappings: HashMap::new(),
                enabled_models: vec!["other-model".into()],
                enabled: true,
                anthropic_cache_control: true,
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
}
