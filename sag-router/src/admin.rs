use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use reqwest::header::CONTENT_TYPE;
use sag_config::{self, KeyStrategy, ProviderConfig, RouteConfig, RouteTarget, Strategy};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{info, warn};

use crate::state::AppState;

/// Build admin routes under `/api/*` (all auth-protected by the parent).
pub fn build_admin_routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/providers",
            get(list_providers)
                .post(add_provider)
                .put(update_provider)
                .delete(delete_provider),
        )
        .route("/providers/{name}/models", get(list_provider_models))
        .route("/model/test", post(model_test))
        .route(
            "/routes",
            get(list_routes).post(create_route).put(update_route).delete(delete_route),
        )
        .with_state(state)
}

// ═══════════════════════════════════════════════════════════════════
// Request / Response types
// ═══════════════════════════════════════════════════════════════════

#[derive(serde::Deserialize)]
struct AddProviderRequest {
    /// Provider name (must be unique).
    name: String,
    api_key: String,
    #[serde(default)]
    api_keys: Vec<String>,
    #[serde(default)]
    key_strategy: KeyStrategy,
    #[serde(default = "default_provider_type")]
    provider_type: String,
    #[serde(default = "default_base_url")]
    base_url: String,
    #[serde(default = "default_priority")]
    priority: u32,
    #[serde(default = "default_max_retries")]
    max_retries: u32,
    #[serde(default = "default_timeout_secs")]
    timeout_secs: u64,
    /// Custom headers to override upstream request headers.
    #[serde(default)]
    headers: HashMap<String, String>,
    /// Body field overrides merged into the upstream JSON request.
    #[serde(default)]
    body_overrides: HashMap<String, serde_json::Value>,
    /// List of model IDs enabled for this provider (optional).
    #[serde(default)]
    enabled_models: Vec<String>,
}

fn default_provider_type() -> String {
    "openai".into()
}
fn default_base_url() -> String {
    "https://api.openai.com/v1".into()
}
fn default_priority() -> u32 {
    0
}
fn default_max_retries() -> u32 {
    2
}
fn default_timeout_secs() -> u64 {
    120
}

fn default_api_endpoint() -> String {
    "/chat/completions".into()
}

// ── Route request types ───────────────────────────────────────────

#[derive(serde::Deserialize)]
struct RouteTargetInput {
    provider: String,
    #[serde(default)]
    real_model: Option<String>,
    #[serde(default)]
    priority: u32,
    #[serde(default)]
    extra: HashMap<String, serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct CreateRouteRequest {
    model_name: String,
    #[serde(default = "default_api_endpoint")]
    api_endpoint: String,
    #[serde(default)]
    strategy: Option<Strategy>,
    targets: Vec<RouteTargetInput>,
}

#[derive(serde::Deserialize)]
struct UpdateRouteRequest {
    model_name: String,
    #[serde(default)]
    api_endpoint: Option<String>,
    #[serde(default)]
    strategy: Option<Strategy>,
    #[serde(default)]
    targets: Option<Vec<RouteTargetInput>>,
}

#[derive(serde::Deserialize)]
struct DeleteRouteRequest {
    model_name: String,
}

#[derive(serde::Deserialize)]
struct UpdateProviderRequest {
    name: String,
    api_key: Option<String>,
    #[serde(default)]
    api_keys: Option<Vec<String>>,
    #[serde(default)]
    key_strategy: Option<KeyStrategy>,
    provider_type: Option<String>,
    base_url: Option<String>,
    priority: Option<u32>,
    max_retries: Option<u32>,
    timeout_secs: Option<u64>,
    headers: Option<HashMap<String, String>>,
    body_overrides: Option<HashMap<String, serde_json::Value>>,
    enabled_models: Option<Vec<String>>,
}

#[derive(serde::Deserialize)]
struct DeleteProviderRequest {
    name: String,
}

#[derive(serde::Serialize)]
struct ProviderInfo {
    name: String,
    api_key: String,
    api_keys: Vec<String>,
    key_strategy: KeyStrategy,
    provider_type: String,
    base_url: String,
    priority: u32,
    max_retries: u32,
    timeout_secs: u64,
    headers: HashMap<String, String>,
    body_overrides: HashMap<String, serde_json::Value>,
    enabled_models: Vec<String>,
}

// ═══════════════════════════════════════════════════════════════════
// Handlers
// ═══════════════════════════════════════════════════════════════════

/// GET /api/providers — list all registered providers.
async fn list_providers(State(state): State<AppState>) -> impl IntoResponse {
    let config = state.config.read().await;
    let providers: Vec<ProviderInfo> = config
        .providers
        .iter()
        .map(|(name, cfg)| ProviderInfo {
            name: name.clone(),
            api_key: cfg.api_key.clone(),
            api_keys: cfg.api_keys.clone(),
            key_strategy: cfg.key_strategy,
            provider_type: cfg.provider_type.clone(),
            base_url: cfg.base_url.clone(),
            priority: cfg.priority,
            max_retries: cfg.max_retries,
            timeout_secs: cfg.timeout_secs,
            headers: cfg.headers.clone(),
            body_overrides: cfg.body_overrides.clone(),
            enabled_models: cfg.enabled_models.clone(),
        })
        .collect();

    (StatusCode::OK, Json(json!({ "providers": providers }))).into_response()
}

/// GET /api/routes — list all routing rules.
async fn list_routes(State(state): State<AppState>) -> impl IntoResponse {
    let config = state.config.read().await;
    (StatusCode::OK, Json(json!({ "routes": config.routes }))).into_response()
}

/// POST /api/routes — create a new route.
async fn create_route(
    State(state): State<AppState>,
    Json(req): Json<CreateRouteRequest>,
) -> impl IntoResponse {
    {
        let mut config = state.config.write().await;

        if config.routes.iter().any(|r| r.model_name == req.model_name) {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": { "code": "CONFLICT", "message": format!("route '{}' already exists", req.model_name) }
                })),
            )
                .into_response();
        }

        for t in &req.targets {
            if !config.providers.contains_key(&t.provider) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": { "code": "BAD_REQUEST", "message": format!("provider '{}' not found", t.provider) }
                    })),
                )
                    .into_response();
            }
            // Validate target model against provider's enabled_models
            let pc = &config.providers[&t.provider];
            if !pc.enabled_models.is_empty() {
                let model_to_check = t.real_model.as_deref().unwrap_or(&req.model_name);
                if !pc.enabled_models.iter().any(|m| m == model_to_check) {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "error": {
                                "code": "BAD_REQUEST",
                                "message": format!(
                                    "model '{}' is not in provider '{}' enabled_models: {:?}",
                                    model_to_check, t.provider, pc.enabled_models
                                )
                            }
                        })),
                    )
                        .into_response();
                }
            }
        }

        let route = RouteConfig {
            model_name: req.model_name.clone(),
            api_endpoint: req.api_endpoint,
            strategy: req.strategy.unwrap_or_default(),
            targets: req
                .targets
                .into_iter()
                .map(|t| RouteTarget {
                    provider: t.provider,
                    real_model: t.real_model,
                    priority: t.priority,
                    extra: t.extra,
                })
                .collect(),
        };

        config.routes.push(route);

        if state.config.is_file_backed()
            && let Err(e) = sag_config::save_config_to_file(&config, state.config.path())
        {
            warn!(?e, "failed to persist config after creating route");
        }
    }

    info!(model = %req.model_name, "route created via admin API");

    (
        StatusCode::CREATED,
        Json(json!({
            "model_name": req.model_name,
        })),
    )
        .into_response()
}

/// PUT /api/routes — update an existing route (partial update).
async fn update_route(
    State(state): State<AppState>,
    Json(req): Json<UpdateRouteRequest>,
) -> impl IntoResponse {
    {
        let mut config = state.config.write().await;

        // Validate targets upfront before any mutable borrow
        if let Some(ref targets) = req.targets {
            for t in targets {
                if !config.providers.contains_key(&t.provider) {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({
                            "error": { "code": "BAD_REQUEST", "message": format!("provider '{}' not found", t.provider) }
                        })),
                    )
                        .into_response();
                }
                // Validate target model against provider's enabled_models
                let pc = &config.providers[&t.provider];
                if !pc.enabled_models.is_empty() {
                    let model_to_check = t.real_model.as_deref().unwrap_or(&req.model_name);
                    if !pc.enabled_models.iter().any(|m| m == model_to_check) {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({
                                "error": {
                                    "code": "BAD_REQUEST",
                                    "message": format!(
                                        "model '{}' is not in provider '{}' enabled_models: {:?}",
                                        model_to_check, t.provider, pc.enabled_models
                                    )
                                }
                            })),
                        )
                            .into_response();
                    }
                }
            }
        }

        let idx = match config.routes.iter().position(|r| r.model_name == req.model_name) {
            Some(i) => i,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({
                        "error": { "code": "NOT_FOUND", "message": format!("route '{}' not found", req.model_name) }
                    })),
                )
                    .into_response();
            }
        };

        let existing = &mut config.routes[idx];

        if let Some(endpoint) = req.api_endpoint {
            existing.api_endpoint = endpoint;
        }
        if let Some(strategy) = req.strategy {
            existing.strategy = strategy;
        }
        if let Some(targets) = req.targets {
            existing.targets = targets
                .into_iter()
                .map(|t| RouteTarget {
                    provider: t.provider,
                    real_model: t.real_model,
                    priority: t.priority,
                    extra: t.extra,
                })
                .collect();
        }

        if state.config.is_file_backed()
            && let Err(e) = sag_config::save_config_to_file(&config, state.config.path())
        {
            warn!(?e, "failed to persist config after updating route");
        }
    }

    info!(model = %req.model_name, "route updated via admin API");

    (
        StatusCode::OK,
        Json(json!({
            "model_name": req.model_name,
        })),
    )
        .into_response()
}

/// DELETE /api/routes — delete a route by model_name.
async fn delete_route(
    State(state): State<AppState>,
    Json(req): Json<DeleteRouteRequest>,
) -> impl IntoResponse {
    {
        let mut config = state.config.write().await;

        let before = config.routes.len();
        config.routes.retain(|r| r.model_name != req.model_name);

        if config.routes.len() == before {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": { "code": "NOT_FOUND", "message": format!("route '{}' not found", req.model_name) }
                })),
            )
                .into_response();
        }

        if state.config.is_file_backed()
            && let Err(e) = sag_config::save_config_to_file(&config, state.config.path())
        {
            warn!(?e, "failed to persist config after deleting route");
        }
    }

    info!(model = %req.model_name, "route deleted via admin API");

    (
        StatusCode::OK,
        Json(json!({
            "model_name": req.model_name,
        })),
    )
        .into_response()
}

/// POST /api/providers — add a new provider and register it in the runtime registry.
async fn add_provider(
    State(state): State<AppState>,
    Json(req): Json<AddProviderRequest>,
) -> impl IntoResponse {
    // 1. Write-lock config, insert provider, and persist to disk
    let provider_config = {
        let mut config = state.config.write().await;

        if config.providers.contains_key(&req.name) {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": { "code": "CONFLICT", "message": format!("provider '{}' already exists", req.name) }
                })),
            )
                .into_response();
        }

        let pc = ProviderConfig {
            provider_type: req.provider_type.clone(),
            api_key: req.api_key.clone(),
            api_keys: req.api_keys.clone(),
            key_strategy: req.key_strategy,
            base_url: req.base_url.clone(),
            priority: req.priority,
            max_retries: req.max_retries,
            timeout_secs: req.timeout_secs,
            headers: req.headers.clone(),
            body_overrides: req.body_overrides.clone(),
            enabled_models: req.enabled_models.clone(),
        };

        config.providers.insert(req.name.clone(), pc.clone());

        // Persist to disk while holding the write lock
        if state.config.is_file_backed()
            && let Err(e) = sag_config::save_config_to_file(&config, state.config.path()) {
                warn!(?e, "failed to persist config after adding provider");
            }

        pc
    };

    // 2. Build the provider adapter and register it
    let provider: Arc<dyn sag_providers::LlmProvider> = match provider_config.provider_type.as_str()
    {
        "openai" => Arc::new(sag_providers::OpenAiProvider::new(
            req.name.clone(),
            &provider_config,
        )),
        "anthropic" => Arc::new(sag_providers::AnthropicProvider::new(
            req.name.clone(),
            &provider_config,
        )),
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": { "code": "BAD_REQUEST", "message": format!("unknown provider_type: '{other}'. supported: openai, anthropic") }
                })),
            )
                .into_response();
        }
    };

    {
        let mut providers = state.providers.write().await;
        providers.insert(req.name.clone(), provider);
    }

    info!(name = %req.name, provider_type = %provider_config.provider_type, "provider added via admin API");

    (
        StatusCode::CREATED,
        Json(json!({
            "name": req.name,
            "provider_type": provider_config.provider_type,
            "base_url": provider_config.base_url,
        })),
    )
        .into_response()
}

/// PUT /api/providers — update an existing provider's configuration.
async fn update_provider(
    State(state): State<AppState>,
    Json(req): Json<UpdateProviderRequest>,
) -> impl IntoResponse {
    let provider_config = {
        let mut config = state.config.write().await;

        let existing = match config.providers.get_mut(&req.name) {
            Some(p) => p,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({
                        "error": { "code": "NOT_FOUND", "message": format!("provider '{}' not found", req.name) }
                    })),
                )
                    .into_response();
            }
        };

        if let Some(v) = req.api_key {
            existing.api_key = v;
        }
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
        if let Some(v) = req.priority {
            existing.priority = v;
        }
        if let Some(v) = req.max_retries {
            existing.max_retries = v;
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
        if let Some(v) = req.enabled_models {
            existing.enabled_models = v;
        }

        let pc = existing.clone();

        // Persist to disk while holding the write lock
        if state.config.is_file_backed()
            && let Err(e) = sag_config::save_config_to_file(&config, state.config.path()) {
                warn!(?e, "failed to persist config after updating provider");
            }

        pc
    };

    // Rebuild the provider adapter and register it
    let provider: Arc<dyn sag_providers::LlmProvider> = match provider_config.provider_type.as_str()
    {
        "openai" => Arc::new(sag_providers::OpenAiProvider::new(
            req.name.clone(),
            &provider_config,
        )),
        "anthropic" => Arc::new(sag_providers::AnthropicProvider::new(
            req.name.clone(),
            &provider_config,
        )),
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": { "code": "BAD_REQUEST", "message": format!("unknown provider_type: '{other}'. supported: openai, anthropic") }
                })),
            )
                .into_response();
        }
    };

    {
        let mut providers = state.providers.write().await;
        providers.insert(req.name.clone(), provider);
    }

    info!(name = %req.name, provider_type = %provider_config.provider_type, "provider updated via admin API");

    (
        StatusCode::OK,
        Json(json!({
            "name": req.name,
            "provider_type": provider_config.provider_type,
            "base_url": provider_config.base_url,
            "priority": provider_config.priority,
            "max_retries": provider_config.max_retries,
            "timeout_secs": provider_config.timeout_secs,
            "headers": provider_config.headers,
            "body_overrides": provider_config.body_overrides,
            "enabled_models": provider_config.enabled_models,
        })),
    )
        .into_response()
}

/// GET /api/providers/{name}/models — list models available from a provider.
async fn list_provider_models(
    State(state): State<AppState>,
    Path(provider_name): Path<String>,
) -> impl IntoResponse {
    // Look up the provider config
    let (api_key, base_url) = {
        let config = state.config.read().await;
        let pc = match config.providers.get(&provider_name) {
            Some(p) => p,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({
                        "error": { "code": "NOT_FOUND", "message": format!("provider '{provider_name}' not found") }
                    })),
                )
                    .into_response();
            }
        };
        (pc.api_key.clone(), pc.base_url.clone())
    };

    // Build the models URL: strip trailing slash, append /models
    let models_url = format!("{}/models", base_url.trim_end_matches('/'));

    // Request models from the upstream provider
    match reqwest::Client::new()
        .get(&models_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .send()
        .await
    {
        Ok(resp) => {
            if !resp.status().is_success() {
                warn!(
                    provider = %provider_name,
                    status = %resp.status(),
                    "upstream models endpoint returned error"
                );
                return (StatusCode::OK, Json(json!({ "models": [] }))).into_response();
            }

            // Parse OpenAI-style response: { "data": [{ "id": "gpt-4o", ... }, ...] }
            match resp.json::<serde_json::Value>().await {
                Ok(body) => {
                    let models: Vec<String> = body["data"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|item| item["id"].as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();

                    info!(
                        provider = %provider_name,
                        count = models.len(),
                        "listed provider models"
                    );
                    (StatusCode::OK, Json(json!({ "models": models }))).into_response()
                }
                Err(e) => {
                    warn!(?e, provider = %provider_name, "failed to parse models response");
                    (StatusCode::OK, Json(json!({ "models": [] }))).into_response()
                }
            }
        }
        Err(e) => {
            warn!(?e, provider = %provider_name, "failed to reach models endpoint");
            (StatusCode::OK, Json(json!({ "models": [] }))).into_response()
        }
    }
}

/// POST /api/model/test — test a specific model by sending a real chat completion.
#[derive(serde::Deserialize)]
struct ModelTestRequest {
    model: String,
    #[serde(default)]
    api_keys: Vec<String>,
    api_key: String,
    base_url: String,
    provider_type: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
}

async fn model_test(
    Json(req): Json<ModelTestRequest>,
) -> impl IntoResponse {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap();

    let keys = if req.api_keys.is_empty() {
        vec![req.api_key.clone()]
    } else {
        req.api_keys.clone()
    };

    let mut results = Vec::new();

    for key in &keys {
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

    Json(json!({ "results": results }))
}

async fn test_single_key(
    client: &reqwest::Client,
    req: &ModelTestRequest,
    api_key: &str,
) -> (bool, String) {
    let (url, body, auth_header, auth_value): (String, serde_json::Value, &str, String) =
        if req.provider_type.as_deref() == Some("anthropic") {
            (
                format!("{}/messages", req.base_url.trim_end_matches('/')),
                serde_json::json!({
                    "model": req.model,
                    "messages": [{"role": "user", "content": "say 'hi'"}],
                    "max_tokens": 1
                }),
                "x-api-key",
                api_key.to_string(),
            )
        } else {
            (
                format!("{}/chat/completions", req.base_url.trim_end_matches('/')),
                serde_json::json!({
                    "model": req.model,
                    "messages": [{"role": "user", "content": "say 'hi'"}],
                    "max_tokens": 1
                }),
                "Authorization",
                format!("Bearer {api_key}"),
            )
        };

    let mut http_req = client
        .post(&url)
        .header(auth_header, &auth_value)
        .header(CONTENT_TYPE, "application/json");

    // Anthropic also needs the version header
    if req.provider_type.as_deref() == Some("anthropic") {
        http_req = http_req.header("anthropic-version", "2023-06-01");
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
async fn delete_provider(
    State(state): State<AppState>,
    Json(req): Json<DeleteProviderRequest>,
) -> impl IntoResponse {
    let provider_name = req.name;
    // ── Stage 1: update config + persist ─────────────────────────────
    // Collect (model_name, removed_target_count) for every route that lost
    // at least one target, so we can report it back to the caller.
    let (cascaded_routes, removed_targets) = {
        let mut config = state.config.write().await;

        if !config.providers.contains_key(&provider_name) {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": {
                        "code": "NOT_FOUND",
                        "message": format!("provider '{provider_name}' not found"),
                    }
                })),
            )
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
        if state.config.is_file_backed()
            && let Err(e) = sag_config::save_config_to_file(&config, state.config.path()) {
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

    info!(
        name = %provider_name,
        cascaded_routes = cascaded_routes.len(),
        removed_targets = removed_targets,
        "provider deleted via admin API"
    );

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

// ═══════════════════════════════════════════════════════════════════
// Tests
// ═══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
    };
    use sag_analytics::CallLogCollector;
    use sag_config::{
        ConfigHandle, GatewayAuthConfig, GatewayConfig, ProviderConfig, RouteConfig, RouteTarget,
        ServerConfig,
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    /// Helper: build an in-memory `AppState` populated with the given
    /// providers and routes. The runtime provider registry is also
    /// pre-populated with `OpenAiProvider` adapters for every provider.
    async fn build_state(
        providers: HashMap<String, ProviderConfig>,
        routes: Vec<RouteConfig>,
    ) -> AppState {
        let config = GatewayConfig {
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 3000,
            },
            analytics: None,
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["test-key".into()],
            }),
            providers,
            routes,
        };
        let handle = ConfigHandle::from_config(config);

        let cfg_read = handle.read().await;
        let mut registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
        for (name, pc) in cfg_read.providers.iter() {
            let provider: Arc<dyn sag_providers::LlmProvider> =
                Arc::new(sag_providers::OpenAiProvider::new(name.clone(), pc));
            registry.insert(name.clone(), provider);
        }
        drop(cfg_read);

        AppState {
            config: handle,
            providers: Arc::new(RwLock::new(registry)),
            collector: Arc::new(CallLogCollector::new_memory().await.unwrap()),
            route_counters: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Build a small test app that mounts only the admin routes.
    /// We don't add the gateway_auth middleware here so tests can focus on
    /// the route logic itself.
    fn test_app(state: AppState) -> Router {
        Router::new()
            .nest("/api", build_admin_routes(state.clone()))
            .with_state(state)
    }

    fn one_provider_config(
        name: &str,
        routes: Vec<(&str, &str, u32)>,
    ) -> (HashMap<String, ProviderConfig>, Vec<RouteConfig>) {
        let mut providers = HashMap::new();
        providers.insert(
            name.to_string(),
            ProviderConfig {
                provider_type: "openai".into(),
                api_key: "sk-test".into(),
                api_keys: vec![],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://api.openai.com/v1".into(),
                priority: 10,
                max_retries: 2,
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                enabled_models: vec![],
            },
        );
        let routes = routes
            .into_iter()
            .map(|(model, real_model, prio)| RouteConfig {
                model_name: model.into(),
                api_endpoint: "/chat/completions".into(),
                strategy: Strategy::FillFirst,
                targets: vec![RouteTarget {
                    provider: name.into(),
                    real_model: Some(real_model.into()),
                    priority: prio,
                        extra: HashMap::new(),
                }],
            })
            .collect();
        (providers, routes)
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
                        "openai"
                    } else {
                        "anthropic"
                    }
                    .into(),
                    api_key: format!("sk-{name}"),
                    api_keys: vec![],
                    key_strategy: KeyStrategy::FillFirst,
                    base_url: "https://example.com".into(),
                    priority: 10,
                    max_retries: 2,
                    timeout_secs: 120,
                    headers: HashMap::new(),
                    body_overrides: HashMap::new(),
                    enabled_models: vec![],
                },
            );
        }

        let routes = vec![
            RouteConfig {
                model_name: "gpt-4o".into(),
                api_endpoint: "/chat/completions".into(),
                strategy: Strategy::FillFirst,
                targets: vec![
                    RouteTarget {
                        provider: "openai".into(),
                        real_model: Some("gpt-4o".into()),
                        priority: 1,
                        extra: HashMap::new(),
                    },
                    RouteTarget {
                        provider: "anthropic".into(),
                        real_model: Some("claude-3-5-sonnet".into()),
                        extra: HashMap::new(),
                        priority: 2,
                    },
                ],
            },
            RouteConfig {
                model_name: "claude-sonnet".into(),
                api_endpoint: "/chat/completions".into(),
                strategy: Strategy::FillFirst,
                targets: vec![RouteTarget {
                    provider: "anthropic".into(),
                        extra: HashMap::new(),
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
api_key = "sk-test"
base_url = "https://api.openai.com/v1"

[providers.anthropic]
provider_type = "anthropic"
api_key = "sk-ant"
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

        let state = AppState {
            config: handle,
            providers: Arc::new(RwLock::new(registry)),
            collector: Arc::new(CallLogCollector::new_memory().await.unwrap()),
            route_counters: Arc::new(RwLock::new(HashMap::new())),
        };
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

        // The [providers] section is serialized as an inline table
        // (`anthropic = { ... }`).
        let providers_block = content
            .split("\n\n")
            .find(|s| s.starts_with("[providers]") || s.trim_start().starts_with("providers."))
            .unwrap_or("");
        assert!(
            !providers_block.contains("openai"),
            "openai should not appear in [providers] block: {providers_block}"
        );
        assert!(
            providers_block.contains("anthropic"),
            "anthropic should be preserved in [providers] block: {providers_block}"
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
    }

    // ── Route CRUD tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn create_route_success() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());
        let _ = &state;

        let req = Request::builder()
            .method("POST")
            .uri("/api/routes")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model_name":"gpt-4o","targets":[{"provider":"openai","real_model":"gpt-4o","priority":10}]}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let cfg = state.config.read().await;
        assert_eq!(cfg.routes.len(), 1);
        assert_eq!(cfg.routes[0].model_name, "gpt-4o");
        assert_eq!(cfg.routes[0].targets.len(), 1);
        assert_eq!(cfg.routes[0].targets[0].provider, "openai");
        assert_eq!(cfg.routes[0].targets[0].real_model.as_deref(), Some("gpt-4o"));
        assert_eq!(cfg.routes[0].targets[0].priority, 10);
    }

    #[tokio::test]
    async fn create_route_conflict() {
        let (providers, routes) = one_provider_config("openai", vec![("gpt-4o", "gpt-4o", 1)]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());
        let _ = &state;

        let req = Request::builder()
            .method("POST")
            .uri("/api/routes")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model_name":"gpt-4o","targets":[{"provider":"openai","real_model":"gpt-4o"}]}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);

        let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["error"]["code"], "CONFLICT");
    }

    #[tokio::test]
    async fn create_route_rejected_by_enabled_models() {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfig {
                provider_type: "openai".into(),
                api_key: "sk-test".into(),
                api_keys: vec![],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://api.openai.com/v1".into(),
                priority: 10,
                max_retries: 2,
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                enabled_models: vec!["gpt-4o".into(), "gpt-4o-mini".into()],
            },
        );
        let state = build_state(providers, vec![]).await;
        let app = test_app(state.clone());
        let _ = &state;

        let req = Request::builder()
            .method("POST")
            .uri("/api/routes")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model_name":"gpt-4o","targets":[{"provider":"openai","real_model":"gpt-3.5-turbo","priority":10}]}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["error"]["code"], "BAD_REQUEST");
        assert!(body["error"]["message"].as_str().unwrap().contains("enabled_models"));
    }

    #[tokio::test]
    async fn create_route_allowed_by_enabled_models() {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfig {
                provider_type: "openai".into(),
                api_key: "sk-test".into(),
                api_keys: vec![],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://api.openai.com/v1".into(),
                priority: 10,
                max_retries: 2,
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                enabled_models: vec!["gpt-4o".into(), "gpt-4o-mini".into()],
            },
        );
        let state = build_state(providers, vec![]).await;
        let app = test_app(state.clone());
        let _ = &state;

        let req = Request::builder()
            .method("POST")
            .uri("/api/routes")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model_name":"gpt-4o","targets":[{"provider":"openai","real_model":"gpt-4o","priority":10}]}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);

        let cfg = state.config.read().await;
        assert_eq!(cfg.routes.len(), 1);
    }

    #[tokio::test]
    async fn create_route_passthrough_checked_by_enabled_models() {
        // When real_model is omitted, the route's model_name is checked
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfig {
                provider_type: "openai".into(),
                api_key: "sk-test".into(),
                api_keys: vec![],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://api.openai.com/v1".into(),
                priority: 10,
                max_retries: 2,
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                enabled_models: vec!["gpt-4o".into()],
            },
        );
        let state = build_state(providers, vec![]).await;
        let app = test_app(state.clone());
        let _ = &state;

        // model_name "gpt-4o-mini" is not in enabled_models
        let req = Request::builder()
            .method("POST")
            .uri("/api/routes")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model_name":"gpt-4o-mini","targets":[{"provider":"openai","priority":10}]}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn update_route_rejected_by_enabled_models() {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfig {
                provider_type: "openai".into(),
                api_key: "sk-test".into(),
                api_keys: vec![],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://api.openai.com/v1".into(),
                priority: 10,
                max_retries: 2,
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                enabled_models: vec!["gpt-4o".into()],
            },
        );
        let routes = vec![RouteConfig {
            model_name: "gpt-4o".into(),
            api_endpoint: "/chat/completions".into(),
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                provider: "openai".into(),
                real_model: Some("gpt-4o".into()),
                priority: 10,
                extra: HashMap::new(),
            }],
        }];
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());
        let _ = &state;

        // Try to update with a model not in enabled_models
        let req = Request::builder()
            .method("PUT")
            .uri("/api/routes")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model_name":"gpt-4o","targets":[{"provider":"openai","real_model":"gpt-3.5-turbo","priority":5}]}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_route_invalid_provider() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());
        let _ = &state;

        let req = Request::builder()
            .method("POST")
            .uri("/api/routes")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model_name":"gpt-4o","targets":[{"provider":"nonexistent","real_model":"gpt-4o"}]}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body["error"]["code"], "BAD_REQUEST");
    }

    #[tokio::test]
    async fn update_route_success() {
        let (providers, routes) = one_provider_config("openai", vec![("gpt-4o", "gpt-4o", 1)]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());
        let _ = &state;

        let req = Request::builder()
            .method("PUT")
            .uri("/api/routes")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model_name":"gpt-4o","api_endpoint":"/responses","targets":[{"provider":"openai","real_model":"gpt-4o-updated","priority":5}]}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        let r = cfg.routes.iter().find(|r| r.model_name == "gpt-4o").unwrap();
        assert_eq!(r.api_endpoint, "/responses");
        assert_eq!(r.targets[0].real_model.as_deref(), Some("gpt-4o-updated"));
        assert_eq!(r.targets[0].priority, 5);
    }

    #[tokio::test]
    async fn update_route_not_found() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());
        let _ = &state;

        let req = Request::builder()
            .method("PUT")
            .uri("/api/routes")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model_name":"nonexistent","api_endpoint":"/responses"}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_route_success() {
        let (providers, routes) = one_provider_config("openai", vec![("gpt-4o", "gpt-4o", 1)]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());
        let _ = &state;

        let req = Request::builder()
            .method("DELETE")
            .uri("/api/routes")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model_name":"gpt-4o"}"#))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        assert!(cfg.routes.is_empty());
    }

    #[tokio::test]
    async fn delete_route_not_found() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());
        let _ = &state;

        let req = Request::builder()
            .method("DELETE")
            .uri("/api/routes")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model_name":"nonexistent"}"#))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
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
                r#"{"name":"openai","base_url":"https://custom.com/v1","priority":5,"max_retries":3}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        let p = cfg.providers.get("openai").unwrap();
        assert_eq!(p.base_url, "https://custom.com/v1");
        assert_eq!(p.priority, 5);
        assert_eq!(p.max_retries, 3);
        // Fields not in the update should keep their original values
        assert_eq!(p.api_key, "sk-test");
        assert_eq!(p.provider_type, "openai");
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
                r#"{"name":"openai","api_key":"sk-updated","provider_type":"anthropic","base_url":"https://api.anthropic.com"}"#,
            ))
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        let p = cfg.providers.get("openai").unwrap();
        assert_eq!(p.api_key, "sk-updated");
        assert_eq!(p.provider_type, "anthropic");
        assert_eq!(p.base_url, "https://api.anthropic.com");

        // Ensure the runtime registry was updated
        let reg = state.providers.read().await;
        assert!(reg.contains_key("openai"));
    }

}

// ═══════════════════════════════════════════════════════════════════
// Auth-regression tests — verify the auth middleware doesn't deadlock
// write-locking handlers.
// ═══════════════════════════════════════════════════════════════════
//
// Earlier versions of `sag-router/src/auth.rs::gateway_auth` held a
// read lock on the config for the entire request lifecycle, which
// deadlocked any handler that needed to acquire the write lock. These
// tests wrap the admin routes with the real auth middleware and
// exercise every write-locking handler with a hard timeout, so a
// regression would surface as a clear test failure rather than a
// production hang.

#[cfg(test)]
mod auth_regression_tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        middleware,
    };
    use sag_analytics::CallLogCollector;
    use sag_config::{
        ConfigHandle, GatewayAuthConfig, GatewayConfig, ProviderConfig, RouteConfig, RouteTarget,
        ServerConfig,
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    async fn build_protected_state() -> AppState {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfig {
                provider_type: "openai".into(),
                api_key: "sk-test".into(),
                api_keys: vec![],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://api.openai.com/v1".into(),
                priority: 10,
                max_retries: 2,
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                enabled_models: vec![],
            },
        );
        let config = GatewayConfig {
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 3000,
            },
            analytics: None,
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["regression-key".into()],
            }),
            providers,
            routes: vec![RouteConfig {
                model_name: "gpt-4o".into(),
                api_endpoint: "/chat/completions".into(),
                strategy: Strategy::FillFirst,
                targets: vec![RouteTarget {
                        extra: HashMap::new(),
                    provider: "openai".into(),
                    real_model: Some("gpt-4o".into()),
                    priority: 1,
                }],
            }],
        };
        let handle = ConfigHandle::from_config(config);
        let mut registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
        let cfg_read = handle.read().await;
        for (name, pc) in cfg_read.providers.iter() {
            let provider: Arc<dyn sag_providers::LlmProvider> =
                Arc::new(sag_providers::OpenAiProvider::new(name.clone(), pc));
            registry.insert(name.clone(), provider);
        }
        drop(cfg_read);
        AppState {
            config: handle,
            providers: Arc::new(RwLock::new(registry)),
            collector: Arc::new(CallLogCollector::new_memory().await.unwrap()),
            route_counters: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn protected_app(state: AppState) -> Router {
        let admin = Router::new()
            .route(
                "/providers",
                get(list_providers)
                    .post(add_provider)
                    .put(update_provider)
                    .delete(delete_provider),
            )
            .route("/providers/{name}/models", get(list_provider_models))
            .route(
                "/routes",
                get(list_routes).post(create_route).put(update_route).delete(delete_route),
            )
            .with_state(state.clone())
            .route_layer(middleware::from_fn_with_state(
                state.clone(),
                super::super::auth::gateway_auth,
            ));
        Router::new().nest("/api", admin).with_state(state)
    }

    async fn bounded<F>(label: &str, fut: F) -> axum::response::Response
    where
        F: std::future::Future<Output = axum::response::Response>,
    {
        let start = Instant::now();
        let result = tokio::time::timeout(Duration::from_secs(3), fut).await;
        assert!(
            result.is_ok(),
            "[{label}] request did not complete in 3s — \
             this strongly suggests the auth middleware is still \
             holding a read lock across the handler (deadlock). \
             See sag-router/src/auth.rs."
        );
        let resp = result.unwrap();
        eprintln!("[{label}] completed in {:?}", start.elapsed());
        resp
    }

    #[tokio::test]
    async fn auth_regression_delete_completes_through_auth_middleware() {
        let state = build_protected_state().await;
        let app = protected_app(state);
        let resp = bounded("DELETE", async {
            app.oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/providers")
                    .header("Authorization", "Bearer regression-key")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"name":"openai"}"#))
                    .unwrap(),
            )
            .await
            .unwrap()
        })
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_regression_add_provider_completes_through_auth_middleware() {
        let state = build_protected_state().await;
        let app = protected_app(state);
        let resp = bounded("POST", async {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/providers")
                    .header("Authorization", "Bearer regression-key")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"name":"anthropic","api_key":"sk-ant","provider_type":"anthropic"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
        })
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn auth_regression_add_route_completes_through_auth_middleware() {
        let state = build_protected_state().await;
        let app = protected_app(state);
        let resp = bounded("POST route", async {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/routes")
                    .header("Authorization", "Bearer regression-key")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"model_name":"gpt-4o-mini","targets":[{"provider":"openai","real_model":"gpt-4o-mini"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
        })
        .await;
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn auth_regression_list_providers_completes_through_auth_middleware() {
        let state = build_protected_state().await;
        let app = protected_app(state);
        let resp = bounded("GET", async {
            app.oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/providers")
                    .header("Authorization", "Bearer regression-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        })
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_regression_update_provider_completes_through_auth_middleware() {
        let state = build_protected_state().await;
        let app = protected_app(state);
        let resp = bounded("PUT", async {
            app.oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/providers")
                    .header("Authorization", "Bearer regression-key")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"name":"openai","base_url":"https://updated.com/v1"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap()
        })
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
