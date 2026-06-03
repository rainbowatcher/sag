use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json,
    Router,
};
use sag_config::{self, ProviderConfig, RouteConfig, RouteTarget};
use serde_json::json;
use std::sync::Arc;
use tracing::{info, warn};

use crate::state::AppState;

/// Build admin routes under `/api/*` (all auth-protected by the parent).
pub fn build_admin_routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/providers", get(list_providers).post(add_provider))
        .route(
            "/providers/{name}/routes",
            post(add_route_for_provider),
        )
        .route("/routes", get(list_routes))
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
    #[serde(default)]
    supported_params: Vec<String>,
}

fn default_provider_type() -> String { "openai".into() }
fn default_base_url() -> String { "https://api.openai.com/v1".into() }
fn default_priority() -> u32 { 10 }
fn default_max_retries() -> u32 { 2 }
fn default_timeout_secs() -> u64 { 120 }

#[derive(serde::Deserialize)]
struct AddRouteRequest {
    /// The model name clients request (e.g. "gpt-4o").
    model_name: String,
    /// The real model name sent to the upstream provider.
    real_model: String,
    /// Upstream API endpoint path (default: /v1/chat/completions).
    #[serde(default = "default_api_endpoint")]
    api_endpoint: String,
    #[serde(default = "default_priority")]
    priority: u32,
}

fn default_api_endpoint() -> String { "/v1/chat/completions".into() }

#[derive(serde::Serialize)]
struct ProviderInfo {
    name: String,
    provider_type: String,
    base_url: String,
    priority: u32,
    max_retries: u32,
    timeout_secs: u64,
    supported_params: Vec<String>,
}

// ═══════════════════════════════════════════════════════════════════
// Handlers
// ═══════════════════════════════════════════════════════════════════

/// GET /api/providers — list all registered providers.
async fn list_providers(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let config = state.config.read().await;
    let providers: Vec<ProviderInfo> = config
        .providers
        .iter()
        .map(|(name, cfg)| ProviderInfo {
            name: name.clone(),
            provider_type: cfg.provider_type.clone(),
            base_url: cfg.base_url.clone(),
            priority: cfg.priority,
            max_retries: cfg.max_retries,
            timeout_secs: cfg.timeout_secs,
            supported_params: cfg.supported_params.clone(),
        })
        .collect();

    (StatusCode::OK, Json(json!({ "providers": providers }))).into_response()
}

/// GET /api/routes — list all routing rules.
async fn list_routes(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let config = state.config.read().await;
    (StatusCode::OK, Json(json!({ "routes": config.routes }))).into_response()
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
            base_url: req.base_url.clone(),
            priority: req.priority,
            max_retries: req.max_retries,
            timeout_secs: req.timeout_secs,
            supported_params: req.supported_params.clone(),
        };

        config.providers.insert(req.name.clone(), pc.clone());

        // Persist to disk while holding the write lock
        if state.config.is_file_backed() {
            if let Err(e) = sag_config::save_config_to_file(&config, state.config.path()) {
                warn!(?e, "failed to persist config after adding provider");
            }
        }

        pc
    };

    // 2. Build the provider adapter and register it
    let provider: Arc<dyn sag_providers::LlmProvider> = match provider_config.provider_type.as_str() {
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

/// POST /api/providers/{name}/routes — add a routing rule for an existing provider.
async fn add_route_for_provider(
    State(state): State<AppState>,
    Path(provider_name): Path<String>,
    Json(req): Json<AddRouteRequest>,
) -> impl IntoResponse {
    let mut config = state.config.write().await;

    // Verify the provider exists
    if !config.providers.contains_key(&provider_name) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": { "code": "NOT_FOUND", "message": format!("provider '{}' not found", provider_name) }
            })),
        )
            .into_response();
    }

    let target = RouteTarget {
        provider: provider_name.clone(),
        real_model: req.real_model.clone(),
        priority: req.priority,
    };

    // Check if route for this model already exists → append target
    if let Some(route) = config
        .routes
        .iter_mut()
        .find(|r| r.model_name == req.model_name)
    {
        // Avoid duplicate target for same provider+real_model combo
        if route
            .targets
            .iter()
            .any(|t| t.provider == provider_name && t.real_model == req.real_model)
        {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": { "code": "CONFLICT", "message": format!("route target '{}' already exists for model '{}'", req.real_model, req.model_name) }
                })),
            )
                .into_response();
        }
        route.targets.push(target);
    } else {
        config.routes.push(RouteConfig {
            model_name: req.model_name.clone(),
            api_endpoint: req.api_endpoint.clone(),
            targets: vec![target],
        });
    }

    // Persist to disk while holding the write lock
    if state.config.is_file_backed() {
        if let Err(e) = sag_config::save_config_to_file(&config, state.config.path()) {
            warn!(?e, "failed to persist config after adding route");
        }
    }

    info!(
        model = %req.model_name,
        provider = %provider_name,
        real_model = %req.real_model,
        "route added via admin API"
    );

    (
        StatusCode::CREATED,
        Json(json!({
            "model_name": req.model_name,
            "provider": provider_name,
            "real_model": req.real_model,
            "priority": req.priority,
        })),
    )
        .into_response()
}
