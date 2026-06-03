use std::time::Instant;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Json,
    Router,
};
use sag_core::{CallLog, Usage};
use serde_json::json;
use tracing::{error, info, instrument, warn};

use crate::auth::gateway_auth;
use crate::state::AppState;

/// Build the main Axum router with all routes and middleware.
pub fn build_router(state: AppState) -> Router {
    let chat_route = Router::new()
        .route("/", post(chat_completions_handler))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            gateway_auth,
        ));

    Router::new()
        .route("/health", get(health_handler))
        .route("/v1/analytics/stats", get(analytics_stats_handler))
        .nest("/v1/chat/completions", chat_route)
        .with_state(state)
}

/// POST /v1/chat/completions
///
/// Full request lifecycle with multi-provider fallback:
/// 1. Parse request body
/// 2. Match model → route (exact → wildcard `*`)
/// 3. Sort route targets by priority (ascending)
/// 4. Try each target: parameter validation → dispatch
/// 5. On 4xx error: return immediately (don't retry)
/// 6. On 5xx / network / timeout: try next target
/// 7. If all exhausted: return AllProvidersExhausted
/// 8. Log analytics via CallLogCollector
#[instrument(skip(state), fields())]
async fn chat_completions_handler(
    State(state): State<AppState>,
    Json(request): Json<sag_core::ChatCompletionRequest>,
) -> impl IntoResponse {
    let start = Instant::now();
    let request_id = uuid::Uuid::new_v4();
    let is_stream = request.stream.unwrap_or(false);

    // Resolve model → sorted targets from routing table
    let targets = {
        let config_guard = state.config.read().await;

        let route = config_guard
            .routes
            .iter()
            .find(|r| r.model_name == request.model)
            .or_else(|| config_guard.routes.iter().find(|r| r.model_name == "*"));

        let targets = match route {
            Some(r) => r.targets.clone(),
            None => {
                let err = sag_core::AppError::ModelNotFound(request.model.clone());
                let log = CallLog {
                    request_id,
                    provider: String::new(),
                    model: request.model.clone(),
                    real_model: String::new(),
                    stream: is_stream,
                    status_code: 404,
                    duration_ms: start.elapsed().as_millis() as u64,
                    usage: None,
                    error: Some(err.to_string()),
                    timestamp: chrono::Utc::now(),
                };
                state.collector.log(log);
                return err.into_response();
            }
        };

        // Sort by priority ascending (lower = higher priority)
        let mut sorted = targets;
        sorted.sort_by_key(|t| t.priority);

        // Resolve passthrough
        sorted
            .into_iter()
            .map(|t| {
                let rm = if t.real_model == "<passthrough>" {
                    request.model.clone()
                } else {
                    t.real_model
                };
                (t.provider, rm, t.priority)
            })
            .collect::<Vec<_>>()
        // config_guard dropped here
    };

    info!(
        model = %request.model,
        num_targets = targets.len(),
        stream = is_stream,
        "dispatching request with fallback"
    );

    let mut errors = Vec::new();

    for (provider_name, real_model, priority) in &targets {
        let provider = match state.get_provider(provider_name).await {
            Some(p) => p,
            None => {
                warn!(provider = %provider_name, "provider not found in registry");
                errors.push(format!("{provider_name}: not found"));
                continue;
            }
        };

        info!(
            provider = %provider_name,
            real_model = %real_model,
            priority = priority,
            "trying provider"
        );

        // Dispatch to this provider
        let result = if is_stream {
            provider
                .chat_stream(&request, real_model)
                .await
                .map(|resp| (resp.into_response(), None::<Usage>))
        } else {
            provider
                .chat(&request, real_model)
                .await
                .map(|resp| {
                    let usage = resp.usage.clone();
                    (
                        (StatusCode::OK, Json(resp)).into_response(),
                        usage,
                    )
                })
        };

        match result {
            Ok((response, usage)) => {
                // Success — log and return
                let log = CallLog {
                    request_id,
                    provider: provider_name.clone(),
                    model: request.model.clone(),
                    real_model: real_model.clone(),
                    stream: is_stream,
                    status_code: 200,
                    duration_ms: start.elapsed().as_millis() as u64,
                    usage,
                    error: None,
                    timestamp: chrono::Utc::now(),
                };
                state.collector.log(log);
                return response;
            }
            Err(e) => {
                // Determine if this error is retryable
                let is_retryable = is_retryable_error(&e);
                warn!(
                    provider = %provider_name,
                    error = %e,
                    retryable = is_retryable,
                    "provider returned error"
                );

                errors.push(format!("{provider_name}: {e}"));

                if !is_retryable {
                    // Non-retryable (4xx/BadRequest) — stop fallback immediately
                    break;
                }
                // Otherwise, try next target
            }
        }
    }

    // All targets exhausted
    let reason = errors.join("; ");
    let err = sag_core::AppError::AllProvidersExhausted {
        model: request.model.clone(),
        reason,
    };

    error!(model = %request.model, "all providers exhausted");

    let log = CallLog {
        request_id,
        provider: String::new(),
        model: request.model.clone(),
        real_model: String::new(),
        stream: is_stream,
        status_code: 502,
        duration_ms: start.elapsed().as_millis() as u64,
        usage: None,
        error: Some(err.to_string()),
        timestamp: chrono::Utc::now(),
    };
    state.collector.log(log);
    err.into_response()
}

/// Determine if an error should trigger fallback to the next provider.
fn is_retryable_error(e: &sag_core::AppError) -> bool {
    match e {
        sag_core::AppError::BadRequest(_) => false,
        sag_core::AppError::ModelNotFound(_) => false,
        sag_core::AppError::Unauthorized(_) => false,
        _ => true,
    }
}

/// GET /v1/analytics/stats?provider=X&model=Y
///
/// Returns aggregated call statistics grouped by provider and model.
#[derive(serde::Deserialize)]
struct StatsQuery {
    provider: Option<String>,
    model: Option<String>,
}

async fn analytics_stats_handler(
    State(state): State<AppState>,
    Query(params): Query<StatsQuery>,
) -> impl IntoResponse {
    match sag_analytics::query_stats(
        state.collector.pool(),
        params.provider.as_deref(),
        params.model.as_deref(),
    )
    .await
    {
        Ok(rows) => (StatusCode::OK, Json(json!({ "data": rows }))).into_response(),
        Err(e) => {
            error!(?e, "analytics query failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": { "code": "INTERNAL_ERROR", "message": e.to_string() }
                })),
            )
                .into_response()
        }
    }
}

/// GET /health
async fn health_handler() -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}
