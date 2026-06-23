//! HTTP routes for the SAG gateway.
//!
//! This file is intentionally thin: it only owns the router wiring and the
//! `/v1/chat/completions` orchestrator. All dispatch, resolution, retry,
//! and streaming logic lives under [`crate::dispatch`]; the miscellaneous
//! `/health`, `/v1/models`, and `/v1/analytics/stats` handlers live under
//! [`crate::handlers`].

use std::time::Instant;

use axum::{
    Extension, Json, Router,
    body::Body,
    extract::State,
    middleware,
    response::Response,
    routing::{get, post},
};
use sag_core::ChatCompletionRequest;
use tower_http::cors::{Any, CorsLayer};
use tracing::instrument;

use crate::dispatch::{DispatchCtx, dispatch_chat, dispatch_image, resolve_route};
use crate::handlers::{
    analytics_cache_stats, analytics_distribution, analytics_stats, analytics_trend, list_models,
    show_health,
};
use crate::middleware::auth::gateway_auth;
use crate::middleware::trace::trace_middleware;
use crate::state::AppState;

/// Build the main Axum router with all routes and middleware.
pub fn build_router(state: AppState) -> Router {
    // The three auth-protected sub-routers are built first, then wrapped
    // in a single `route_layer` so `gateway_auth` is registered once
    // (one `state.clone()`) instead of three times. The unprotected
    // `/health` and `/v1/analytics/stats` routes are added to the outer
    // router *after* the merge, so they stay public.
    let protected = Router::new()
        .nest(
            "/v1/chat/completions",
            Router::new().route("/", post(chat_completions)),
        )
        .nest("/v1", Router::new().route("/models", get(list_models)))
        .nest("/api", crate::admin::build_admin_routes(state.clone()))
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
    Json(request): Json<ChatCompletionRequest>,
) -> Response<Body> {
    let start = Instant::now();
    let is_stream = request.stream.unwrap_or(false);
    let span = tracing::Span::current();
    span.record("request_id", trace_id.to_string());
    span.record("model", request.model.as_str());
    span.record("stream", is_stream);

    // Resolve route → ordered targets + api_endpoint
    let (targets, api_endpoint) =
        match resolve_route(&state, &request, trace_id, start, is_stream).await {
            Ok(resolved) => resolved,
            Err(response) => return response,
        };

    let mut errors = Vec::new();

    if api_endpoint.ends_with("/images/generations") {
        return dispatch_image(DispatchCtx {
            state: &state,
            request: &request,
            targets: &targets,
            api_endpoint: &api_endpoint,
            is_stream,
            trace_id,
            start,
            errors: &mut errors,
        })
        .await;
    }

    dispatch_chat(DispatchCtx {
        state: &state,
        request: &request,
        targets: &targets,
        api_endpoint: &api_endpoint,
        is_stream,
        trace_id,
        start,
        errors: &mut errors,
    })
    .await
}
