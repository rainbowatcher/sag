//! Route CRUD handlers for the admin API.
//!
//! All routes are mounted under `/api/routes*` by
//! [`crate::admin::build_admin_routes`].

use std::collections::HashMap;

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use sag_config::{GatewayConfig, Pipeline, RouteConfig, RouteTarget, Strategy};
use sag_core::AppError;
use serde::Serialize;
use serde_json::json;
use tracing::instrument;

use sag_state::AppState;

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
    /// Optional fixed-key capability override. Unknown keys are rejected
    /// by `GatewayConfig::validate` on commit (400 via config error path).
    #[serde(default)]
    pub capabilities: Option<sag_config::CapabilitiesOverride>,
}

impl From<RouteTargetInput> for RouteTarget {
    fn from(t: RouteTargetInput) -> Self {
        RouteTarget {
            provider: t.provider,
            real_model: t.real_model,
            retries: t.retries,
            timeout_secs: None,
            priority: t.priority,
            extra: t.extra,
            capabilities: t.capabilities,
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct CreateRouteRequest {
    pub model_name: String,
    /// Dispatch pipeline (default `chat`). `image` routes additionally
    /// require `api_endpoint`; chat routes must not set it.
    #[serde(default)]
    pub pipeline: Pipeline,
    /// Real upstream path appended to `apis.image.base_url` — only legal
    /// when `pipeline = "image"` (omitted/`null` for chat routes).
    #[serde(default)]
    pub api_endpoint: Option<String>,
    #[serde(default)]
    pub strategy: Option<Strategy>,
    /// Per-target retry count override for this entire route.
    #[serde(default)]
    pub retries: Option<u32>,
    /// Session pin TTL in seconds (`0` = pins never expire). Omitted means
    /// the built-in default (`sag_state::DEFAULT_SESSION_TTL_SECS`).
    #[serde(default)]
    pub session_ttl_secs: Option<u64>,
    /// Per-space session pin capacity. Omitted means the built-in default
    /// (`sag_state::DEFAULT_SESSION_MAX_ENTRIES`); `0` is rejected by
    /// `GatewayConfig::validate` on commit (400 via config error path).
    #[serde(default)]
    pub session_max_entries: Option<usize>,
    pub targets: Vec<RouteTargetInput>,
}

#[derive(Debug, serde::Deserialize)]
pub struct UpdateRouteRequest {
    pub model_name: String,
    /// Omitted/`null` keeps the current pipeline.
    #[serde(default)]
    pub pipeline: Option<Pipeline>,
    /// Triple-state `api_endpoint` patch: absent → keep, JSON `null` →
    /// clear (required when switching an image route back to chat), value
    /// → set.
    #[serde(default, deserialize_with = "double_option_api_endpoint")]
    pub api_endpoint: Option<Option<String>>,
    #[serde(default)]
    pub strategy: Option<Strategy>,
    /// Per-target retry count override for this route.
    #[serde(default)]
    pub retries: Option<u32>,
    /// Omitted/`null` keeps the current session TTL.
    #[serde(default)]
    pub session_ttl_secs: Option<u64>,
    /// Omitted/`null` keeps the current session capacity.
    #[serde(default)]
    pub session_max_entries: Option<usize>,
    #[serde(default)]
    pub targets: Option<Vec<RouteTargetInput>>,
}

/// Serde helper for the triple-state `api_endpoint` patch: missing →
/// `None` (keep), JSON `null` → `Some(None)` (clear), value →
/// `Some(Some(v))` (set).
fn double_option_api_endpoint<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize;
    let value: Option<String> = Option::deserialize(deserializer)?;
    Ok(Some(value))
}

#[derive(Debug, serde::Deserialize)]
pub struct DeleteRouteRequest {
    pub model_name: String,
}

/// Borrowing response item for `GET /api/routes` that ensures `pipeline`
/// and `api_endpoint` always appear in the JSON response, even when they
/// equal the defaults (`chat` / `null`). The optional overrides (`retries`,
/// `session_ttl_secs`, `session_max_entries`) are omitted when unset.
#[derive(Serialize)]
pub struct RouteResponseItem<'a> {
    model_name: &'a str,
    pipeline: Pipeline,
    api_endpoint: Option<&'a str>,
    strategy: Strategy,
    #[serde(skip_serializing_if = "Option::is_none")]
    retries: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_ttl_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_max_entries: Option<usize>,
    targets: &'a [RouteTarget],
}

/// Validate that every target references an existing provider and that the
/// model (either `real_model` or the route's `model_name` as passthrough)
/// is present in the provider's `enabled_models` when that list is non-empty.
fn validate_targets(
    config: &GatewayConfig,
    targets: &[RouteTargetInput],
    route_model_name: &str,
) -> Result<(), AppError> {
    for t in targets {
        let pc = match config.providers.get(&t.provider) {
            Some(pc) => pc,
            None => {
                return Err(AppError::BadRequest(format!(
                    "provider '{}' not found",
                    t.provider
                )));
            }
        };
        if !pc.enabled_models.is_empty() {
            let model_to_check = t.real_model.as_deref().unwrap_or(route_model_name);
            if !pc.enabled_models.iter().any(|m| m == model_to_check) {
                return Err(AppError::BadRequest(format!(
                    "model '{}' is not in provider '{}' enabled_models: {:?}",
                    model_to_check, t.provider, pc.enabled_models
                )));
            }
        }
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════
// Handlers
// ═══════════════════════════════════════════════════════════════════

/// After a committed route mutation, prune the shared routing memory so it
/// matches the new config without waiting for the next dispatch:
/// - drop `active_targets` entries whose request model no longer resolves to
///   a [`Strategy::StickyFailover`] route — a strategy switch
///   (StickyFailover → FillFirst / RoundRobin) or a route removal /
///   shadowing leaves a stale sticky pin that must not linger;
/// - drop `fill_first_orders` entries whose key no longer resolves to **any**
///   route — a deleted route's cached shuffle must not linger either. Keys
///   are judged by re-resolving against the current route table, never by
///   membership in the config's `model_name` set: a wildcard route's caches
///   live under concrete request models, which still resolve to the
///   wildcard route and are therefore kept. Still-resolving caches are left
///   untouched — `order_fill_first` self-heals any targets mismatch by
///   re-shuffling on next use, so a live fill-first order is never broken.
/// - drop `route_counters` entries under the same rule (same
///   [`sag_state::retain_resolving_route_keys`] helper the dispatch prune
///   uses), so a deleted route's round-robin counter does not survive until
///   the next request observes a `routes_version` change.
/// - drop `session_affinity` pins whose request-model key no longer resolves
///   to the [`Strategy::SessionSticky`] route that recorded them — the
///   session analogue of the `active_targets` rule (a strategy switch, a
///   route removal, or an exact route shadowing a wildcard leaves pins the
///   current config would never consult).
///
/// Mirrors `sag_dispatch::DispatchRuntime::prune_stale_entries`'s four
/// routing memories (fill-first orders, round-robin counters, active
/// targets, session pins). Runs with no config read guard held across
/// the writes (routes are cloned first), so it cannot deadlock against
/// dispatch or the coordinator.
async fn prune_stale_routing_state(state: &AppState) {
    let routes = state.config.read().await.routes.clone();
    {
        let mut orders = state.fill_first_orders.write().await;
        sag_state::retain_resolving_route_keys(&mut orders, &routes);
    }
    {
        let mut counters = state.route_counters.write().await;
        sag_state::retain_resolving_route_keys(&mut counters, &routes);
    }
    state
        .active_targets
        .write()
        .await
        .retain_sticky_routes(&routes);
    state
        .session_affinity
        .write()
        .await
        .retain_sticky_routes(&routes);
}

/// GET /api/routes — list all routing rules.
pub async fn list_routes(State(state): State<AppState>) -> impl IntoResponse {
    let config = state.config.read().await;
    let routes: Vec<RouteResponseItem<'_>> = config
        .routes
        .iter()
        .map(|r| RouteResponseItem {
            model_name: &r.model_name,
            pipeline: r.pipeline,
            api_endpoint: r.api_endpoint.as_deref(),
            strategy: r.strategy,
            retries: r.retries,
            session_ttl_secs: r.session_ttl_secs,
            session_max_entries: r.session_max_entries,
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

    // Read-only validation: check targets reference existing providers and
    // compatible enabled_models. The coordinator handles duplicate name check
    // and atomic commit.
    {
        let config = state.config.read().await;
        if let Err(e) = validate_targets(&config, &req.targets, &req.model_name) {
            return e.into_response();
        }
    }

    let route = RouteConfig {
        session_ttl_secs: req.session_ttl_secs,
        session_max_entries: req.session_max_entries,
        model_name: req.model_name.clone(),
        pipeline: req.pipeline,
        api_endpoint: req.api_endpoint,
        retries: req.retries,
        timeout_secs: None,
        strategy: req.strategy.unwrap_or_default(),
        targets: req.targets.into_iter().map(Into::into).collect(),
    };

    match state.coordinator.create_route(route).await {
        Ok(_report) => {
            prune_stale_routing_state(&state).await;
            (
                StatusCode::CREATED,
                Json(json!({
                    "model_name": req.model_name,
                })),
            )
                .into_response()
        }
        // Full-config validation failures (unknown capability keys,
        // chat route carrying `api_endpoint`, image route without one, …)
        // surface as `AppError::Config` from the coordinator; report 400
        // not 500.
        Err(AppError::Config(msg)) => AppError::BadRequest(msg).into_response(),
        Err(e) => e.into_response(),
    }
}

/// PUT /api/routes — update an existing route (partial update).
#[instrument(skip(state), fields(model_name))]
pub async fn update_route(
    State(state): State<AppState>,
    Json(req): Json<UpdateRouteRequest>,
) -> impl IntoResponse {
    tracing::Span::current().record("model_name", &req.model_name);

    // 1. Read current config, validate targets (if changed), find existing
    //    route, merge fields.
    let route = {
        let config = state.config.read().await;

        // Validate targets upfront before any clone.
        if let Some(ref targets) = req.targets
            && let Err(e) = validate_targets(&config, targets, &req.model_name)
        {
            return e.into_response();
        }

        let existing = match config
            .routes
            .iter()
            .find(|r| r.model_name == req.model_name)
        {
            Some(r) => r,
            None => {
                return AppError::NotFound(format!("route '{}' not found", req.model_name))
                    .into_response();
            }
        };

        let mut route = existing.clone();
        if let Some(pipeline) = req.pipeline {
            route.pipeline = pipeline;
        }
        if let Some(endpoint) = req.api_endpoint {
            route.api_endpoint = endpoint;
        }
        if let Some(strategy) = req.strategy {
            route.strategy = strategy;
        }
        if let Some(retries) = req.retries {
            route.retries = Some(retries);
        }
        // Same "omitted = keep" merge as `retries`: an admin patch can set
        // these session fields but never clear them (a JSON `null` is
        // indistinguishable from an absent field here).
        if let Some(ttl_secs) = req.session_ttl_secs {
            route.session_ttl_secs = Some(ttl_secs);
        }
        if let Some(max_entries) = req.session_max_entries {
            route.session_max_entries = Some(max_entries);
        }
        if let Some(targets) = req.targets {
            route.targets = targets.into_iter().map(Into::into).collect();
        }

        route
    };

    // 2. Delegate to the coordinator.
    match state.coordinator.update_route(&req.model_name, route).await {
        Ok(_report) => {
            // The mutation may have switched the route's strategy (or
            // removed targets / the route itself): proactively drop stale
            // sticky pins so the routing memory matches the new config.
            prune_stale_routing_state(&state).await;
            (
                StatusCode::OK,
                Json(json!({
                    "model_name": req.model_name,
                })),
            )
                .into_response()
        }
        // Same Config→400 remap as create_route (see above).
        Err(AppError::Config(msg)) => AppError::BadRequest(msg).into_response(),
        Err(e) => e.into_response(),
    }
}

/// DELETE /api/routes — delete a route by model_name.
#[instrument(skip(state), fields(model_name))]
pub async fn delete_route(
    State(state): State<AppState>,
    Json(req): Json<DeleteRouteRequest>,
) -> impl IntoResponse {
    tracing::Span::current().record("model_name", &req.model_name);

    match state.coordinator.delete_route(&req.model_name).await {
        Ok(_report) => {
            // The route is gone: drop any pins recorded under it.
            prune_stale_routing_state(&state).await;
            (
                StatusCode::OK,
                Json(json!({
                    "model_name": req.model_name,
                })),
            )
                .into_response()
        }
        Err(e) => e.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::test_provider;
    use crate::test_utils::{build_state, one_provider_config, test_app};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use sag_config::{ApiKind, Pipeline, RouteConfig, RouteTarget, Strategy};
    use std::collections::HashMap;
    use tower::ServiceExt;

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
            test_provider(
                ApiKind::ChatCompletions,
                "https://api.openai.com/v1",
                vec!["sk-test".into()],
                vec!["gpt-4o".into(), "gpt-4o-mini".into()],
            ),
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
            test_provider(
                ApiKind::ChatCompletions,
                "https://api.openai.com/v1",
                vec!["sk-test".into()],
                vec!["gpt-4o".into(), "gpt-4o-mini".into()],
            ),
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
            test_provider(
                ApiKind::ChatCompletions,
                "https://api.openai.com/v1",
                vec!["sk-test".into()],
                vec!["gpt-4o".into()],
            ),
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
            test_provider(
                ApiKind::ChatCompletions,
                "https://api.openai.com/v1",
                vec!["sk-test".into()],
                vec!["gpt-4o".into()],
            ),
        );
        let routes = vec![RouteConfig {
            session_ttl_secs: None,
            session_max_entries: None,
            model_name: "gpt-4o".into(),
            pipeline: Pipeline::Chat,
            api_endpoint: None,
            retries: None,
            timeout_secs: None,
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                provider: "openai".into(),
                real_model: Some("gpt-4o".into()),
                priority: 10,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
                capabilities: None,
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
                r#"{"model_name":"gpt-4o","targets":[{"provider":"openai","real_model":"gpt-4o-updated","priority":5}]}"#,
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
        // `api_endpoint` is image-only in the new config model: an update
        // that does not mention it leaves the chat route's `None` alone.
        assert_eq!(r.pipeline, Pipeline::Chat);
        assert_eq!(r.api_endpoint, None);
        assert_eq!(r.targets[0].real_model.as_deref(), Some("gpt-4o-updated"));
        assert_eq!(r.targets[0].priority, 5);
    }

    /// Create an image-pipeline route: `pipeline = "image"` +
    /// `api_endpoint` round-trip through GET /api/routes.
    #[tokio::test]
    async fn create_image_route_round_trips_pipeline_and_api_endpoint() {
        let (providers, routes) = one_provider_config("openai", vec![("gpt-4o", "gpt-4o", 1)]);
        let state = build_state(providers, routes).await;
        let app = test_app(state);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/routes")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"model_name":"dall-e-3","pipeline":"image","api_endpoint":"/images/generations","targets":[{"provider":"openai","real_model":"dall-e-3"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/routes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let route = body["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["model_name"] == "dall-e-3")
            .expect("image route present");
        assert_eq!(route["pipeline"], serde_json::json!("image"));
        assert_eq!(
            route["api_endpoint"],
            serde_json::json!("/images/generations")
        );
        // Chat routes still report the default pipeline + null endpoint.
        let chat = body["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["model_name"] == "gpt-4o")
            .expect("chat route present");
        assert_eq!(chat["pipeline"], serde_json::json!("chat"));
        assert_eq!(chat["api_endpoint"], serde_json::json!(null));
    }

    /// An image route switched back to chat must drop its `api_endpoint`
    /// (validation rejects a chat route carrying one) — the triple-state
    /// `api_endpoint: null` patch is the way to clear it.
    #[tokio::test]
    async fn update_route_clears_api_endpoint_with_null() {
        let (providers, routes) = one_provider_config("openai", vec![("gpt-4o", "gpt-4o", 1)]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/routes")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"model_name":"dall-e-3","pipeline":"image","api_endpoint":"/images/generations","targets":[{"provider":"openai","real_model":"dall-e-3"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/routes")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"model_name":"dall-e-3","pipeline":"chat","api_endpoint":null}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        let r = cfg
            .routes
            .iter()
            .find(|r| r.model_name == "dall-e-3")
            .unwrap();
        assert_eq!(r.pipeline, Pipeline::Chat);
        assert_eq!(r.api_endpoint, None, "null patch must clear the endpoint");
    }

    #[tokio::test]
    async fn update_route_strategy_switch_prunes_stale_active_target() {
        // A route pinned under StickyFailover; switching it to FillFirst via
        // the admin API must proactively drop the stale pin (without waiting
        // for the next dispatch prune).
        let (providers, _) = one_provider_config("openai", vec![]);
        let route = RouteConfig {
            session_ttl_secs: None,
            session_max_entries: None,
            model_name: "gpt-4o".into(),
            pipeline: Pipeline::Chat,
            api_endpoint: None,
            retries: None,
            timeout_secs: None,
            strategy: Strategy::StickyFailover,
            targets: vec![RouteTarget {
                provider: "openai".into(),
                real_model: Some("gpt-4o".into()),
                retries: None,
                timeout_secs: None,
                priority: 1,
                extra: HashMap::new(),
                capabilities: None,
            }],
        };
        let state = build_state(providers, vec![route]).await;
        state.active_targets.write().await.insert(
            "gpt-4o".into(),
            sag_state::ActiveTargetEntry::new(
                "openai",
                "gpt-4o",
                "gpt-4o",
                Some("gpt-4o".into()),
                std::time::Instant::now(),
            ),
        );
        let app = test_app(state.clone());

        let req = Request::builder()
            .method("PUT")
            .uri("/api/routes")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model_name":"gpt-4o","strategy":"fill_first"}"#,
            ))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        assert!(
            state.active_targets.read().await.get("gpt-4o").is_none(),
            "strategy switch to fill_first must prune the stale sticky pin"
        );
        let cfg = state.config.read().await;
        assert_eq!(cfg.routes[0].strategy, Strategy::FillFirst);
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

    /// Build a FillFirst route config (mirror of `one_provider_config`).
    fn fill_first_route(model: &str) -> RouteConfig {
        RouteConfig {
            session_ttl_secs: None,
            session_max_entries: None,
            model_name: model.into(),
            pipeline: Pipeline::Chat,
            api_endpoint: None,
            retries: None,
            timeout_secs: None,
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                provider: "openai".into(),
                real_model: Some(model.into()),
                retries: None,
                timeout_secs: None,
                priority: 1,
                extra: HashMap::new(),
                capabilities: None,
            }],
        }
    }

    fn cached_order(model: &str) -> sag_state::CachedOrder {
        sag_state::CachedOrder {
            targets: std::sync::Arc::from(vec![RouteTarget {
                provider: "openai".into(),
                real_model: Some(model.into()),
                retries: None,
                timeout_secs: None,
                priority: 1,
                extra: HashMap::new(),
                capabilities: None,
            }]),
            created_at: std::time::Instant::now(),
            routes_version: Some(1),
        }
    }

    #[tokio::test]
    async fn delete_route_prunes_fill_first_orders_cache() {
        // A deleted route must not leave its cached shuffled order behind:
        // the proactive prune drops fill_first_orders keys that no longer
        // resolve to any route, while keeping caches of still-existing routes.
        let (providers, _) = one_provider_config("openai", vec![]);
        let state = build_state(
            providers,
            vec![fill_first_route("gpt-4o"), fill_first_route("dall-e")],
        )
        .await;
        {
            let mut orders = state.fill_first_orders.write().await;
            orders.insert("gpt-4o".into(), cached_order("gpt-4o"));
            orders.insert("dall-e".into(), cached_order("dall-e"));
            orders.insert(
                "old-deleted-model".into(),
                cached_order("old-deleted-model"),
            );
        }
        let app = test_app(state.clone());

        let req = Request::builder()
            .method("DELETE")
            .uri("/api/routes")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model_name":"dall-e"}"#))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let orders = state.fill_first_orders.read().await;
        assert!(
            orders.contains_key("gpt-4o"),
            "surviving route's cached order is kept"
        );
        assert!(
            !orders.contains_key("dall-e"),
            "deleted route's cached order is pruned"
        );
        assert!(
            !orders.contains_key("old-deleted-model"),
            "stale key from an already-deleted route is pruned too"
        );
    }

    #[tokio::test]
    async fn delete_route_keeps_wildcard_concrete_fill_first_caches() {
        // fill_first_orders is keyed by the request model; for a wildcard
        // route that is any concrete model it served. The proactive prune
        // must not mis-clear those keys: after deleting an exact route, a
        // concrete key that now resolves to the wildcard route keeps its
        // cache (and self-heals its targets on next use).
        let (providers, _) = one_provider_config("openai", vec![]);
        let state = build_state(
            providers,
            vec![fill_first_route("gpt-4o"), fill_first_route("*")],
        )
        .await;
        {
            let mut orders = state.fill_first_orders.write().await;
            orders.insert("gpt-4o".into(), cached_order("gpt-4o"));
            orders.insert("gpt-4.1".into(), cached_order("gpt-4.1"));
        }
        let app = test_app(state.clone());

        let req = Request::builder()
            .method("DELETE")
            .uri("/api/routes")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model_name":"gpt-4o"}"#))
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let orders = state.fill_first_orders.read().await;
        assert!(
            orders.contains_key("gpt-4o"),
            "key still resolving (now via the wildcard) keeps its cache"
        );
        assert!(
            orders.contains_key("gpt-4.1"),
            "wildcard concrete request key keeps its cache"
        );
    }

    // ── Session-affinity field surface (session_ttl_secs / max_entries) ──

    /// `POST /api/routes` must persist both session fields and expose them
    /// in the `GET /api/routes` snapshot (they used to be hardcoded `None`).
    #[tokio::test]
    async fn create_route_round_trips_session_fields() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/routes")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "model_name": "sticky-route",
                            "strategy": "session_sticky",
                            "session_ttl_secs": 600,
                            "session_max_entries": 500,
                            "targets": [{"provider": "openai", "real_model": "gpt-4o"}],
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        {
            let cfg = state.config.read().await;
            let r = cfg
                .routes
                .iter()
                .find(|r| r.model_name == "sticky-route")
                .expect("route persisted");
            assert_eq!(r.session_ttl_secs, Some(600));
            assert_eq!(r.session_max_entries, Some(500));
        }

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/routes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let route = body["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["model_name"] == "sticky-route")
            .expect("route present in listing");
        assert_eq!(route["session_ttl_secs"], serde_json::json!(600));
        assert_eq!(route["session_max_entries"], serde_json::json!(500));
    }

    /// A route that never set the session fields must not grow them in the
    /// listing (`skip_serializing_if = "Option::is_none"`).
    #[tokio::test]
    async fn list_routes_omits_unset_session_fields() {
        let (providers, routes) = one_provider_config("openai", vec![("gpt-4o", "gpt-4o", 1)]);
        let state = build_state(providers, routes).await;
        let app = test_app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/routes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let route = &body["routes"][0];
        assert!(route.get("session_ttl_secs").is_none());
        assert!(route.get("session_max_entries").is_none());
    }

    /// `PUT` with both session fields set must overwrite the stored values.
    #[tokio::test]
    async fn update_route_sets_session_fields() {
        let (providers, routes) = one_provider_config("openai", vec![("gpt-4o", "gpt-4o", 1)]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        for (ttl, cap) in [(600u64, 500usize), (120, 42)] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("PUT")
                        .uri("/api/routes")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "model_name": "gpt-4o",
                                "session_ttl_secs": ttl,
                                "session_max_entries": cap,
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);

            let cfg = state.config.read().await;
            let r = cfg
                .routes
                .iter()
                .find(|r| r.model_name == "gpt-4o")
                .unwrap();
            assert_eq!(r.session_ttl_secs, Some(ttl));
            assert_eq!(r.session_max_entries, Some(cap));
        }
    }

    /// An update that omits the session fields keeps the stored values
    /// (same "omitted = keep" merge as `retries`).
    #[tokio::test]
    async fn update_route_keeps_session_fields_when_omitted() {
        let (providers, _) = one_provider_config("openai", vec![]);
        let route = RouteConfig {
            session_ttl_secs: Some(600),
            session_max_entries: Some(500),
            model_name: "gpt-4o".into(),
            pipeline: Pipeline::Chat,
            api_endpoint: None,
            retries: None,
            timeout_secs: None,
            strategy: Strategy::SessionSticky,
            targets: vec![RouteTarget {
                provider: "openai".into(),
                real_model: Some("gpt-4o".into()),
                retries: None,
                timeout_secs: None,
                priority: 1,
                extra: HashMap::new(),
                capabilities: None,
            }],
        };
        let state = build_state(providers, vec![route]).await;
        let app = test_app(state.clone());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/routes")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"model_name":"gpt-4o","targets":[{"provider":"openai","real_model":"gpt-4o","priority":5}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        let r = cfg
            .routes
            .iter()
            .find(|r| r.model_name == "gpt-4o")
            .unwrap();
        assert_eq!(r.session_ttl_secs, Some(600), "omitted ttl must be kept");
        assert_eq!(
            r.session_max_entries,
            Some(500),
            "omitted capacity must be kept"
        );
        assert_eq!(r.targets[0].priority, 5);
    }

    /// `session_max_entries = 0` is rejected by the coordinator's full
    /// config validation on commit → 400 (no admin-side duplicate check).
    #[tokio::test]
    async fn create_route_zero_session_max_entries_is_400() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/routes")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "model_name": "zero-cap",
                            "strategy": "session_sticky",
                            "session_max_entries": 0,
                            "targets": [{"provider": "openai", "real_model": "gpt-4o"}],
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Nothing committed.
        let cfg = state.config.read().await;
        assert!(!cfg.routes.iter().any(|r| r.model_name == "zero-cap"));
    }

    // ── M7 rework: target capabilities round-trip + illegal enum ──

    #[tokio::test]
    async fn create_route_round_trips_target_capabilities() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
        };
        use tower::ServiceExt;

        let (providers, routes) = one_provider_config("openai", vec![("gpt-4o", "gpt-4o", 1)]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/routes")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "model_name": "route-caps",
                            "targets": [{
                                "provider": "openai",
                                "real_model": "gpt-4o",
                                "priority": 1,
                                "capabilities": {"streaming": true, "tools": false},
                            }],
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/routes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let route = body["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["model_name"] == "route-caps")
            .expect("route present");
        let target = &route["targets"][0];
        // `upstream_protocol` is gone from RouteTarget — the protocol comes
        // from the provider's `apis` keys now.
        assert!(target.get("upstream_protocol").is_none());
        assert_eq!(target["capabilities"]["streaming"], serde_json::json!(true));
        assert_eq!(target["capabilities"]["tools"], serde_json::json!(false));
    }

    /// Typed config enums stay strict: an unknown `pipeline` value fails
    /// deserialization (422).
    #[tokio::test]
    async fn create_route_illegal_pipeline_value_is_422() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
        };
        use tower::ServiceExt;

        let (providers, routes) = one_provider_config("openai", vec![("gpt-4o", "gpt-4o", 1)]);
        let state = build_state(providers, routes).await;
        let app = test_app(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/routes")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "model_name": "bad",
                            "pipeline": "video",
                            "targets": [{
                                "provider": "openai",
                            }],
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// B1: target `capabilities` with an unknown key (`teleport`) is
    /// captured into `CapabilitiesOverride::unknown` at deserialize time
    /// and rejected by `GatewayConfig::validate` on commit → 4xx.
    #[tokio::test]
    async fn create_route_unknown_capabilities_key_is_4xx() {
        let (providers, routes) = one_provider_config("openai", vec![("gpt-4o", "gpt-4o", 1)]);
        let state = build_state(providers, routes).await;
        let app = test_app(state.clone());

        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/routes")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "model_name": "teleport-route",
                            "targets": [{
                                "provider": "openai",
                                "real_model": "gpt-4o",
                                "capabilities": {"teleport": true},
                            }],
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            resp.status().is_client_error(),
            "unknown capability key must be 4xx, got {}",
            resp.status()
        );

        // Must not have committed the route either.
        let cfg = state.config.read().await;
        assert!(
            !cfg.routes.iter().any(|r| r.model_name == "teleport-route"),
            "route with unknown capability key must not commit"
        );
    }
}
