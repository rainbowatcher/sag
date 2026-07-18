//! HTTP routes for the SAG gateway.
//!
//! This file is intentionally thin: it only owns the router wiring and the
//! `/v1/chat/completions` orchestrator. All dispatch, resolution, retry,
//! and streaming logic lives under [`sag_dispatch`]; the miscellaneous
//! `/health`, `/v1/models`, and `/v1/analytics/stats` handlers live under
//! [`crate::handlers`].

use std::time::Instant;

use axum::{
    Extension, Json, Router,
    body::Body,
    extract::State,
    http::HeaderMap,
    middleware,
    response::Response,
    routing::{get, post},
};
use sag_core::ChatCompletionRequest;
use tower_http::cors::{Any, CorsLayer};
use tracing::instrument;

use crate::handlers::{
    analytics_cache_stats, analytics_distribution, analytics_stats, analytics_trend, list_models,
    show_health,
};
use crate::middleware::auth::gateway_auth;
use crate::middleware::trace::trace_middleware;
use sag_admin::build_admin_routes;
use sag_dispatch::{
    AppState, DispatchCtx, DispatchRuntime, dispatch_chat, dispatch_image, resolve_route,
};

/// Build the main Axum router with all routes and middleware.
///
/// `runtime` carries the dispatch-only state (circuit breaker, ordering
/// caches) that `chat_completions` needs but the admin/analytics handlers
/// don't. It is threaded into the orchestrator via a request extension so
/// non-dispatch handlers never see it.
pub fn build_router(state: AppState, runtime: DispatchRuntime) -> Router {
    // The three auth-protected sub-routers are built first, then wrapped
    // in a single `route_layer` so `gateway_auth` is registered once
    // (one `state.clone()`) instead of three times. The unprotected
    // `/health` and `/v1/analytics/stats` routes are added to the outer
    // router *after* the merge, so they stay public.
    let protected = Router::new()
        .nest(
            "/v1/chat/completions",
            Router::new()
                .route("/", post(chat_completions))
                .layer(axum::Extension(runtime)),
        )
        .nest("/v1", Router::new().route("/models", get(list_models)))
        .nest("/api", build_admin_routes(state.clone()))
        .route_layer(middleware::from_fn_with_state(state.clone(), gateway_auth));

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/health", get(show_health))
        .route("/v1/analytics/stats", get(analytics_stats))
        .route("/v1/analytics/cache", get(analytics_cache_stats))
        .route("/v1/analytics/trend", get(analytics_trend))
        .route("/v1/analytics/distribution", get(analytics_distribution))
        .merge(protected)
        .layer(cors)
        .layer(middleware::from_fn(crate::middleware::http_log::log_http))
        .layer(middleware::from_fn(trace_middleware))
        .with_state(state)
}

/// POST `/v1/chat/completions`
///
/// Orchestrates the full request lifecycle:
/// 1. Resolve model → route + targets (exact match, then wildcard `*`).
/// 2. If the route's `api_endpoint` is image-based, dispatch as image gen.
/// 3. Otherwise, dispatch to chat with multi-provider fallback.
#[instrument(skip_all, fields(request_id, model, stream))]
async fn chat_completions(
    State(state): State<AppState>,
    Extension(trace_id): Extension<uuid::Uuid>,
    Extension(runtime): Extension<DispatchRuntime>,
    headers: HeaderMap,
    Json(mut request): Json<ChatCompletionRequest>,
) -> Response<Body> {
    let start = Instant::now();
    request.normalize_reasoning_effort();
    let is_stream = request.stream.unwrap_or(false);
    let span = tracing::Span::current();
    span.record("request_id", trace_id.to_string());
    span.record("model", request.model.as_str());
    span.record("stream", is_stream);

    // Client identification (header-based only; reverse proxies set these).
    let user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let client_ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        });

    // Resolve route → ordered targets + api_endpoint + strategy
    let (targets, api_endpoint, strategy) = match resolve_route(
        &state,
        &runtime,
        &request,
        trace_id,
        start,
        is_stream,
        user_agent.as_deref(),
        client_ip.as_deref(),
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };

    let mut errors = Vec::new();

    if api_endpoint.ends_with("/images/generations") {
        return dispatch_image(DispatchCtx {
            state: &state,
            runtime: &runtime,
            request: &request,
            targets: &targets,
            api_endpoint: &api_endpoint,
            is_stream,
            trace_id,
            start,
            errors: &mut errors,
            strategy,
            user_agent,
            client_ip,
        })
        .await;
    }

    dispatch_chat(DispatchCtx {
        state: &state,
        runtime: &runtime,
        request: &request,
        targets: &targets,
        api_endpoint: &api_endpoint,
        is_stream,
        trace_id,
        start,
        errors: &mut errors,
        strategy,
        user_agent,
        client_ip,
    })
    .await
}
