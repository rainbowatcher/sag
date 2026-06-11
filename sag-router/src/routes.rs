use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    Json, Router,
    body::Body,
    extract::{Query, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::StreamExt;
use sag_config::Strategy;
use sag_core::{CallLog, ImageGenerationRequest, StreamChunk, Usage};
use serde_json::json;
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, instrument, warn};

use crate::auth::gateway_auth;
use crate::state::AppState;

/// Build the main Axum router with all routes and middleware.
pub fn build_router(state: AppState) -> Router {
    let chat_route = Router::new()
        .route("/", post(chat_completions_handler))
        .route_layer(middleware::from_fn_with_state(state.clone(), gateway_auth));

    let models_route = Router::new()
        .route("/models", get(models_handler))
        .route_layer(middleware::from_fn_with_state(state.clone(), gateway_auth));

    let admin_routes = crate::admin::build_admin_routes(state.clone())
        .route_layer(middleware::from_fn_with_state(state.clone(), gateway_auth));

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/health", get(health_handler))
        .route("/v1/analytics/stats", get(analytics_stats_handler))
        .nest("/v1/chat/completions", chat_route)
        .nest("/v1", models_route)
        .nest("/api", admin_routes)
        // .layer(
        //     TraceLayer::new_for_http()
        //         .make_span_with(|request: &Request<Body>| {
        //             tracing::span!(
        //                 Level::INFO,
        //                 "request",
        //                 method = %request.method(),
        //                 uri = %request.uri(),
        //                 status = tracing::field::Empty,
        //                 latency_ms = tracing::field::Empty,
        //             )
        //         })
        //         .on_response(
        //             |response: &Response<Body>, latency: std::time::Duration, span: &tracing::Span| {
        //                 span.record("status", response.status().as_u16());
        //                 span.record("latency_ms", latency.as_millis() as u64);
        //             },
        //         ),
        // )
        .layer(cors)
        .with_state(state)
}

/// POST /v1/chat/completions
///
/// Full request lifecycle with multi-provider fallback:
/// 1. Parse request body
/// 2. Match model -> route (exact -> wildcard `*`)
/// 3. If api_endpoint is image-based, dispatch as image generation
/// 4. Sort route targets by priority (descending — higher value wins)
/// 5. Try each target with tiered retries (target → route → global retries count)
/// 6. On 4xx error: return immediately (don't retry, don't fallback)
/// 7. On retryable error (5xx / network / timeout): retry same provider
///    up to the effective retry count; if exhausted, try next target
/// 8. If all targets exhausted: return AllProvidersExhausted
/// 9. Log analytics via CallLogCollector
#[instrument(skip(state), fields(request_id, model, stream))]
async fn chat_completions_handler(
    State(state): State<AppState>,
    Json(request): Json<sag_core::ChatCompletionRequest>,
) -> Response<Body> {
    let start = Instant::now();
    let request_id = uuid::Uuid::new_v4();
    let is_stream = request.stream.unwrap_or(false);

    tracing::Span::current().record("request_id", request_id.to_string());
    tracing::Span::current().record("model", &request.model);
    tracing::Span::current().record("stream", is_stream);

    // Resolve model -> targets + api_endpoint from routing table
    let (targets, api_endpoint) = {
        let config_guard = state.config.read().await;

        let route = config_guard
            .routes
            .iter()
            .find(|r| r.model_name == request.model)
            .or_else(|| config_guard.routes.iter().find(|r| r.model_name == "*"));

        match route {
            Some(r) => {
                let targets = r.targets.clone();
                let strategy = r.strategy;
                let api_endpoint = r.api_endpoint.clone();
                let model_name = r.model_name.clone();
                let route_retries = r.retries;
                let global_retries = config_guard.retries;
                // Drop the read lock before accessing route_counters (write lock)
                drop(config_guard);

                // Apply target selection strategy
                let ordered = match strategy {
                    Strategy::FillFirst => order_fill_first(targets),
                    Strategy::RoundRobin => {
                        order_round_robin(targets, &model_name, &state.route_counters).await
                    }
                };

                // Resolve effective retries per target: target.retries → route.retries → global.retries
                let resolved: Vec<(String, String, HashMap<String, serde_json::Value>, u32)> =
                    ordered
                        .into_iter()
                        .map(|t| {
                            let rm = t.real_model.unwrap_or_else(|| request.model.clone());
                            let retries = t.retries.or(route_retries).unwrap_or(global_retries);
                            (t.provider, rm, t.extra, retries)
                        })
                        .collect();

                (resolved, api_endpoint)
            }
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
        }
    };

    info!(
        num_targets = targets.len(),
        api_endpoint = %api_endpoint,
        "dispatching request with fallback"
    );

    let mut errors = Vec::new();

    // -- Image generation path --
    if api_endpoint.ends_with("/images/generations") {
        let prompt = request
            .messages
            .last()
            .and_then(|m| m.text_content().map(String::from))
            .unwrap_or_default();

        // Forward relevant extra keys from the chat request
        let mut extra = serde_json::Map::new();
        for key in &["size", "quality", "response_format", "style"] {
            if let Some(v) = request.extra.get(*key) {
                extra.insert(key.to_string(), v.clone());
            }
        }

        let img_request = ImageGenerationRequest {
            model: request.model.clone(),
            prompt,
            n: request.n,
            size: None,
            quality: None,
            response_format: None,
            extra,
        };

        return dispatch_image_request(
            &state,
            img_request,
            &targets,
            &request_id,
            &request.model,
            start,
            &mut errors,
        )
        .await;
    }

    'fallback: for (provider_name, real_model, target_extra, retries) in &targets {
        let provider = match state.get_provider(provider_name).await {
            Some(p) => p,
            None => {
                warn!(provider = %provider_name, "provider not found in registry");
                errors.push(format!("{provider_name}: not found"));
                continue;
            }
        };

        for attempt in 0..=*retries {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }

            info!(
                provider = %provider_name,
                attempt = attempt,
                max_retries = retries,
                "trying provider"
            );

            let pname = provider_name.clone();
            let rmodel = real_model.clone();
            let tex = target_extra.clone();

            // ── Streaming path ──────────────────────────────────────
            if is_stream {
                match provider
                    .chat_stream(&request, &rmodel, &api_endpoint, &tex)
                    .await
                {
                    Ok(chunk_stream) => {
                        let collector = state.collector.clone();
                        let log_request_id = request_id;
                        let log_provider = pname;
                        let log_model = request.model.clone();
                        let log_real_model = rmodel;

                        let body = Body::from_stream(futures_util::stream::unfold(
                            (
                                chunk_stream,
                                Usage::default(),
                                None::<String>,
                                collector,
                                log_request_id,
                                log_provider,
                                log_model,
                                log_real_model,
                                start,
                            ),
                            |(
                                mut stream,
                                mut usage,
                                mut stream_error,
                                collector,
                                request_id,
                                provider,
                                model,
                                real_model,
                                start,
                            )| async move {
                                loop {
                                    match stream.next().await {
                                        Some(StreamChunk::Data(raw)) => {
                                            return Some((
                                                Ok::<_, std::io::Error>(axum::body::Bytes::from(
                                                    raw,
                                                )),
                                                (
                                                    stream,
                                                    usage,
                                                    stream_error,
                                                    collector,
                                                    request_id,
                                                    provider,
                                                    model,
                                                    real_model,
                                                    start,
                                                ),
                                            ));
                                        }
                                        Some(StreamChunk::Usage(u)) => {
                                            usage.prompt_tokens += u.prompt_tokens;
                                            usage.completion_tokens += u.completion_tokens;
                                            usage.total_tokens += u.total_tokens;
                                        }
                                        Some(StreamChunk::Error(msg)) => {
                                            warn!(provider = %provider, error = %msg, "mid-stream error");
                                            stream_error = Some(msg);
                                        }
                                        Some(StreamChunk::Done) | None => {
                                            let error = stream_error.take();
                                            let status_code =
                                                if error.is_some() { 502 } else { 200 };
                                            collector.log(CallLog {
                                                request_id,
                                                provider: provider.clone(),
                                                model: model.clone(),
                                                real_model: real_model.clone(),
                                                stream: true,
                                                status_code,
                                                duration_ms: start.elapsed().as_millis() as u64,
                                                usage: Some(usage),
                                                error,
                                                timestamp: chrono::Utc::now(),
                                            });
                                            return None;
                                        }
                                    }
                                }
                            },
                        ));

                        let response = Response::builder()
                            .status(StatusCode::OK)
                            .header("Content-Type", "text/event-stream")
                            .header("Cache-Control", "no-cache")
                            .header("Connection", "keep-alive")
                            .body(body)
                            .unwrap();
                        return response;
                    }
                    Err(e) => {
                        let is_retryable = is_retryable_error(&e);
                        warn!(
                            provider = %provider_name,
                            error = %e,
                            retryable = is_retryable,
                            attempt = attempt,
                            "provider returned error"
                        );

                        if !is_retryable {
                            errors.push(format!("{provider_name}: {e}"));
                            break 'fallback;
                        }

                        if attempt == *retries {
                            errors.push(format!("{provider_name}: {e} (retries exhausted)"));
                            break;
                        }
                    }
                }

            // ── Non-streaming path ──────────────────────────────────
            } else {
                match provider.chat(&request, &rmodel, &api_endpoint, &tex).await {
                    Ok(resp) => {
                        let log = CallLog {
                            request_id,
                            provider: pname,
                            model: request.model.clone(),
                            real_model: rmodel,
                            stream: false,
                            status_code: 200,
                            duration_ms: start.elapsed().as_millis() as u64,
                            usage: resp.usage.clone(),
                            error: None,
                            timestamp: chrono::Utc::now(),
                        };
                        state.collector.log(log);
                        return (StatusCode::OK, Json(resp)).into_response();
                    }
                    Err(e) => {
                        let is_retryable = is_retryable_error(&e);
                        warn!(
                            provider = %provider_name,
                            error = %e,
                            retryable = is_retryable,
                            attempt = attempt,
                            "provider returned error"
                        );

                        if !is_retryable {
                            errors.push(format!("{provider_name}: {e}"));
                            break 'fallback;
                        }

                        if attempt == *retries {
                            errors.push(format!("{provider_name}: {e} (retries exhausted)"));
                            break;
                        }
                    }
                }
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

/// Image generation fallback loop - mirrors the chat fallback pattern.
async fn dispatch_image_request(
    state: &AppState,
    img_request: ImageGenerationRequest,
    targets: &[(String, String, HashMap<String, serde_json::Value>, u32)],
    request_id: &uuid::Uuid,
    model: &str,
    start: Instant,
    errors: &mut Vec<String>,
) -> Response<Body> {
    'fallback: for (provider_name, real_model, target_extra, retries) in targets {
        let provider = match state.get_provider(provider_name).await {
            Some(p) => p,
            None => {
                warn!(provider = %provider_name, "provider not found in registry");
                errors.push(format!("{provider_name}: not found"));
                continue;
            }
        };

        // Per-provider retry loop: 1 initial attempt + max_retries retries.
        for attempt in 0..=*retries {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }

            info!(
                provider = %provider_name,
                attempt = attempt,
                "trying provider for image generation"
            );

            match provider
                .chat_image(&img_request, real_model, target_extra)
                .await
            {
                Ok(resp) => {
                    let log = CallLog {
                        request_id: *request_id,
                        provider: provider_name.clone(),
                        model: model.to_string(),
                        real_model: real_model.clone(),
                        stream: false,
                        status_code: 200,
                        duration_ms: start.elapsed().as_millis() as u64,
                        usage: None,
                        error: None,
                        timestamp: chrono::Utc::now(),
                    };
                    state.collector.log(log);
                    return (StatusCode::OK, Json(resp)).into_response();
                }
                Err(e) => {
                    let is_retryable = is_retryable_error(&e);
                    warn!(
                        provider = %provider_name,
                        error = %e,
                        retryable = is_retryable,
                        attempt = attempt,
                        "provider image generation error"
                    );

                    if !is_retryable {
                        errors.push(format!("{provider_name}: {e}"));
                        break 'fallback;
                    }

                    if attempt == *retries {
                        errors.push(format!("{provider_name}: {e} (retries exhausted)"));
                        break;
                    }
                    // Otherwise: retry the same provider.
                }
            }
        }
    }

    let reason = errors.join("; ");
    let err = sag_core::AppError::AllProvidersExhausted {
        model: model.to_string(),
        reason,
    };

    error!(model = %model, "all providers exhausted for image generation");
    let log = CallLog {
        request_id: *request_id,
        provider: String::new(),
        model: model.to_string(),
        real_model: String::new(),
        stream: false,
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
    !matches!(
        e,
        sag_core::AppError::BadRequest(_)
            | sag_core::AppError::ModelNotFound(_)
            | sag_core::AppError::Unauthorized(_)
    )
}

/// Order targets for `FillFirst` strategy:
/// group by priority (descending), shuffle within each group.
fn order_fill_first(mut targets: Vec<sag_config::RouteTarget>) -> Vec<sag_config::RouteTarget> {
    // Sort by priority descending so same-priority items are adjacent
    targets.sort_by_key(|t| std::cmp::Reverse(t.priority));

    // Shuffle within each priority group
    let mut i = 0;
    while i < targets.len() {
        let prio = targets[i].priority;
        let mut j = i + 1;
        while j < targets.len() && targets[j].priority == prio {
            j += 1;
        }
        fastrand::shuffle(&mut targets[i..j]);
        i = j;
    }
    targets
}

/// Order targets for `RoundRobin` strategy:
/// rotate the list so that each request starts at a different position.
async fn order_round_robin(
    targets: Vec<sag_config::RouteTarget>,
    model_name: &str,
    counters: &Arc<RwLock<HashMap<String, u64>>>,
) -> Vec<sag_config::RouteTarget> {
    let mut guard = counters.write().await;
    let counter = guard.entry(model_name.to_string()).or_insert(0);
    let len = targets.len();
    let start = (*counter % len as u64) as usize;
    *counter += 1;
    let mut rotated = targets;
    rotated.rotate_left(start);
    rotated
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
) -> Response<Body> {
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

/// GET /v1/models
///
/// Returns the list of available models in OpenAI-compatible format.
/// The model list is derived from the routing table — each route's
/// `model_name` is surfaced as a model `id`.
#[instrument(skip(state))]
async fn models_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    let config = state.config.read().await;

    let data: Vec<serde_json::Value> = config
        .routes
        .iter()
        .map(|route| {
            let owned_by = route
                .targets
                .first()
                .map(|t| t.provider.as_str())
                .unwrap_or("sag");

            json!({
                "id": route.model_name,
                "object": "model",
                "created": 0,
                "owned_by": owned_by,
            })
        })
        .collect();

    Json(json!({
        "object": "list",
        "data": data,
    }))
}
