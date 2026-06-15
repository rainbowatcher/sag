//! Route CRUD handlers for the admin API.
//!
//! All routes are mounted under `/api/routes*` by
//! [`crate::admin::build_admin_routes`].

use std::collections::HashMap;

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use sag_config::{RouteConfig, RouteTarget, Strategy};
use serde::Serialize;
use serde_json::json;
use tracing::{instrument, warn};

use crate::state::AppState;

#[derive(Debug, serde::Deserialize)]
pub struct RouteTargetInput {
    pub provider: String,
    #[serde(default)]
    pub real_model: Option<String>,
    #[serde(default)]
    pub priority: u32,
    /// Per-target retry override (defaults to route-level, then global).
    #[serde(default)]
    pub retries: Option<u32>,
    #[serde(default)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, serde::Deserialize)]
pub struct CreateRouteRequest {
    pub model_name: String,
    #[serde(default = "default_api_endpoint")]
    pub api_endpoint: String,
    #[serde(default)]
    pub strategy: Option<Strategy>,
    /// Per-target retry count override for this entire route.
    #[serde(default)]
    pub retries: Option<u32>,
    pub targets: Vec<RouteTargetInput>,
}

#[derive(Debug, serde::Deserialize)]
pub struct UpdateRouteRequest {
    pub model_name: String,
    #[serde(default)]
    pub api_endpoint: Option<String>,
    #[serde(default)]
    pub strategy: Option<Strategy>,
    /// Per-target retry count override for this route.
    #[serde(default)]
    pub retries: Option<u32>,
    #[serde(default)]
    pub targets: Option<Vec<RouteTargetInput>>,
}

#[derive(Debug, serde::Deserialize)]
pub struct DeleteRouteRequest {
    pub model_name: String,
}

fn default_api_endpoint() -> String {
    "/chat/completions".into()
}

/// Borrowing response item for `GET /api/routes` that ensures `api_endpoint`
/// always appears in the JSON response, even when it equals the default value.
#[derive(Serialize)]
pub struct RouteResponseItem<'a> {
    model_name: &'a str,
    api_endpoint: &'a str,
    strategy: Strategy,
    #[serde(skip_serializing_if = "Option::is_none")]
    retries: Option<u32>,
    targets: &'a [RouteTarget],
}

// ═══════════════════════════════════════════════════════════════════
// Handlers
// ═══════════════════════════════════════════════════════════════════

/// GET /api/routes — list all routing rules.
pub async fn list_routes(State(state): State<AppState>) -> impl IntoResponse {
    let config = state.config.read().await;
    let routes: Vec<RouteResponseItem<'_>> = config
        .routes
        .iter()
        .map(|r| RouteResponseItem {
            model_name: &r.model_name,
            api_endpoint: &r.api_endpoint,
            strategy: r.strategy,
            retries: r.retries,
            targets: &r.targets,
        })
        .collect();
    (StatusCode::OK, Json(json!({ "routes": routes }))).into_response()
}

/// POST /api/routes — create a new route.
#[instrument(skip(state), fields(model_name))]
pub async fn create_route(
    State(state): State<AppState>,
    Json(req): Json<CreateRouteRequest>,
) -> impl IntoResponse {
    tracing::Span::current().record("model_name", &req.model_name);
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
            retries: req.retries,
            strategy: req.strategy.unwrap_or_default(),
            targets: req
                .targets
                .into_iter()
                .map(|t| RouteTarget {
                    provider: t.provider,
                    real_model: t.real_model,
                    retries: t.retries,
                    priority: t.priority,
                    extra: t.extra,
                })
                .collect(),
        };

        config.routes.push(route);

        if let Some(path) = state.config.path()
            && let Err(e) = sag_config::save_config_to_file(&config, path)
        {
            warn!(?e, "failed to persist config after creating route");
        }
    }

    (
        StatusCode::CREATED,
        Json(json!({
            "model_name": req.model_name,
        })),
    )
        .into_response()
}

/// PUT /api/routes — update an existing route (partial update).
#[instrument(skip(state), fields(model_name))]
pub async fn update_route(
    State(state): State<AppState>,
    Json(req): Json<UpdateRouteRequest>,
) -> impl IntoResponse {
    tracing::Span::current().record("model_name", &req.model_name);
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

        let idx = match config
            .routes
            .iter()
            .position(|r| r.model_name == req.model_name)
        {
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
        if let Some(retries) = req.retries {
            existing.retries = Some(retries);
        }
        if let Some(targets) = req.targets {
            existing.targets = targets
                .into_iter()
                .map(|t| RouteTarget {
                    provider: t.provider,
                    real_model: t.real_model,
                    retries: t.retries,
                    priority: t.priority,
                    extra: t.extra,
                })
                .collect();
        }

        if let Some(path) = state.config.path()
            && let Err(e) = sag_config::save_config_to_file(&config, path)
        {
            warn!(?e, "failed to persist config after updating route");
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "model_name": req.model_name,
        })),
    )
        .into_response()
}

/// DELETE /api/routes — delete a route by model_name.
#[instrument(skip(state), fields(model_name))]
pub async fn delete_route(
    State(state): State<AppState>,
    Json(req): Json<DeleteRouteRequest>,
) -> impl IntoResponse {
    tracing::Span::current().record("model_name", &req.model_name);
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

        if let Some(path) = state.config.path()
            && let Err(e) = sag_config::save_config_to_file(&config, path)
        {
            warn!(?e, "failed to persist config after deleting route");
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "model_name": req.model_name,
        })),
    )
        .into_response()
}

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
        ConfigHandle, GatewayAuthConfig, GatewayConfig, KeyStrategy, ProviderConfig, ProviderType,
        RouteConfig, RouteTarget, ServerConfig,
    };
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    pub(super) async fn build_state(
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
            retries: 2,
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

    pub(super) fn test_app(state: AppState) -> Router {
        Router::new()
            .nest("/api", crate::admin::build_admin_routes(state.clone()))
            .with_state(state)
    }

    pub(super) fn one_provider_config(
        name: &str,
        routes: Vec<(&str, &str, u32)>,
    ) -> (HashMap<String, ProviderConfig>, Vec<RouteConfig>) {
        let mut providers = HashMap::new();
        providers.insert(
            name.to_string(),
            ProviderConfig {
                provider_type: ProviderType::OpenAI,
                api_keys: vec!["sk-test".into()],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://api.openai.com/v1".into(),
                models_endpoint: "models".into(),
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                enabled_models: vec![],
                enabled: true,
            },
        );
        let routes = routes
            .into_iter()
            .map(|(model, real_model, prio)| RouteConfig {
                model_name: model.into(),
                api_endpoint: "/chat/completions".into(),
                retries: None,
                strategy: Strategy::FillFirst,
                targets: vec![RouteTarget {
                    provider: name.into(),
                    real_model: Some(real_model.into()),
                    retries: None,
                    priority: prio,
                    extra: HashMap::new(),
                }],
            })
            .collect();
        (providers, routes)
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
        assert_eq!(
            cfg.routes[0].targets[0].real_model.as_deref(),
            Some("gpt-4o")
        );
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
                provider_type: ProviderType::OpenAI,
                api_keys: vec!["sk-test".into()],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://api.openai.com/v1".into(),
                models_endpoint: "models".into(),
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                enabled_models: vec!["gpt-4o".into(), "gpt-4o-mini".into()],
                enabled: true,
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
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("enabled_models")
        );
    }

    #[tokio::test]
    async fn create_route_allowed_by_enabled_models() {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfig {
                provider_type: ProviderType::OpenAI,
                api_keys: vec!["sk-test".into()],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://api.openai.com/v1".into(),
                models_endpoint: "models".into(),
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                enabled_models: vec!["gpt-4o".into(), "gpt-4o-mini".into()],
                enabled: true,
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
                provider_type: ProviderType::OpenAI,
                api_keys: vec!["sk-test".into()],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://api.openai.com/v1".into(),
                models_endpoint: "models".into(),
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                enabled_models: vec!["gpt-4o".into()],
                enabled: true,
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
                provider_type: ProviderType::OpenAI,
                api_keys: vec!["sk-test".into()],
                key_strategy: KeyStrategy::FillFirst,
                base_url: "https://api.openai.com/v1".into(),
                models_endpoint: "models".into(),
                timeout_secs: 120,
                headers: HashMap::new(),
                body_overrides: HashMap::new(),
                enabled_models: vec!["gpt-4o".into()],
                enabled: true,
            },
        );
        let routes = vec![RouteConfig {
            model_name: "gpt-4o".into(),
            api_endpoint: "/chat/completions".into(),
            retries: None,
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                provider: "openai".into(),
                real_model: Some("gpt-4o".into()),
                priority: 10,
                retries: None,
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
        let r = cfg
            .routes
            .iter()
            .find(|r| r.model_name == "gpt-4o")
            .unwrap();
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
}
