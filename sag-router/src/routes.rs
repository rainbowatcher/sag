//! HTTP routes for the SAG gateway.
//!
//! This file is intentionally thin: it only owns the router wiring and the
//! `/v1/chat/completions` orchestrator. All dispatch, resolution, retry,
//! and streaming logic lives under [`sag_dispatch`]; the miscellaneous
//! `/health`, `/v1/models`, and `/v1/analytics/*` (legacy + v2) handlers
//! live under [`crate::handlers`].

use std::time::{Duration, Instant};

use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use sag_analytics::AnalyticsEvent;
use sag_core::{ChatCompletionRequest, FailureReason, FinalStatus, RequestResultFact};
use tower_http::cors::{Any, CorsLayer};
use tracing::{instrument, warn};

use crate::handlers::analytics_v2::{analytics_v2_summary, analytics_v2_trend};
use crate::handlers::{
    analytics_cache_stats, analytics_distribution, analytics_stats, analytics_trend, list_models,
    show_health,
};
use crate::middleware::auth::gateway_auth;
use crate::middleware::trace::trace_middleware;
use sag_admin::build_admin_routes;
use sag_dispatch::{
    AppState, DispatchCtx, DispatchRuntime, ResolvedRoute, dispatch_chat, dispatch_image,
    resolve_route, resolve_route_with_ingress,
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
    // `/health` and `/v1/analytics/*` routes (legacy + v2) are added to
    // the outer router *after* the merge, so they stay public.
    let protected = Router::new()
        .nest(
            "/v1/chat/completions",
            Router::new()
                .route("/", post(chat_completions))
                .layer(axum::Extension(runtime.clone())),
        )
        .nest(
            "/v1/messages",
            Router::new()
                .route("/", post(anthropic_messages))
                .layer(axum::Extension(runtime.clone())),
        )
        .nest(
            "/v1/responses",
            Router::new()
                .route("/", post(responses))
                .layer(axum::Extension(runtime)),
        )
        .nest("/v1", Router::new().route("/models", get(list_models)))
        .nest("/api", build_admin_routes(state.clone()))
        .route_layer(middleware::from_fn_with_state(state.clone(), gateway_auth));

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .max_age(Duration::from_secs(600));

    Router::new()
        .route("/health", get(show_health))
        .route("/v1/analytics/stats", get(analytics_stats))
        .route("/v1/analytics/cache", get(analytics_cache_stats))
        .route("/v1/analytics/trend", get(analytics_trend))
        .route("/v1/analytics/distribution", get(analytics_distribution))
        .route("/v1/analytics/v2/summary", get(analytics_v2_summary))
        .route("/v1/analytics/v2/trend", get(analytics_v2_trend))
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
///
/// JSON extraction failures (which bypass the handler otherwise) are
/// captured as a formal `client_error` analytics fact — see
/// [`emit_client_error_event`] — before the original rejection response
/// is returned.
#[instrument(skip_all, fields(request_id, model, stream))]
async fn chat_completions(
    State(state): State<AppState>,
    Extension(trace_id): Extension<uuid::Uuid>,
    Extension(runtime): Extension<DispatchRuntime>,
    headers: HeaderMap,
    body: Result<Json<ChatCompletionRequest>, JsonRejection>,
) -> Response<Body> {
    let start = Instant::now();
    // Wall-clock timestamp captured at route scheduling time, next to the
    // `Instant` used for legacy duration timing — carried by `ResolvedRoute`
    // into `DispatchCtx` for the analytics route-start event. Also the
    // `started_at` of the `client_error` fact when JSON extraction fails.
    let started_at = chrono::Utc::now();

    let Json(mut request) = match body {
        Ok(json) => json,
        Err(rejection) => {
            emit_client_error_event(&state, trace_id, started_at, &rejection).await;
            return rejection.into_response();
        }
    };
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

    // Resolve route → ordered targets + api_endpoint + strategy + the
    // sticky-failover token captured at decision time, plus the route
    // identity facts (`route_name`, wall-clock `started_at`) for the
    // analytics route-start event.
    let ResolvedRoute {
        targets,
        api_endpoint,
        strategy,
        sticky_token,
        route_name,
        started_at,
        precheck_errors,
    } = match resolve_route(
        &state,
        &runtime,
        &request,
        trace_id,
        start,
        started_at,
        is_stream,
        user_agent.as_deref(),
        client_ip.as_deref(),
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(response) => return response,
    };

    // Seed the dispatch `errors` channel with capability pre-check drops so
    // a later all-exhausted terminal still reports "lacks capability …".
    let mut errors = precheck_errors;

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
            route_name: &route_name,
            started_at,
            errors: &mut errors,
            strategy,
            sticky_token,
            user_agent,
            client_ip,
            ingress_protocol: sag_core::IngressProtocol::ChatCompletions,
            // Image path stays out of IR (plan § D8 / M4 hard constraint).
            ir: None,
        })
        .await;
    }

    // M4: decode the protocol-neutral request once after normalization;
    // `dispatch_chat` orchestrates on IR while providers still receive the
    // original `ChatCompletionRequest`.
    let ir = sag_providers::ChatCompletionsCodec::decode_request(&request);

    dispatch_chat(DispatchCtx {
        state: &state,
        runtime: &runtime,
        request: &request,
        targets: &targets,
        api_endpoint: &api_endpoint,
        is_stream,
        trace_id,
        start,
        route_name: &route_name,
        started_at,
        errors: &mut errors,
        strategy,
        sticky_token,
        user_agent,
        client_ip,
        ingress_protocol: sag_core::IngressProtocol::ChatCompletions,
        ir: Some(&ir),
    })
    .await
}

/// POST `/v1/messages` — Anthropic Messages ingress (M5).
///
/// Decodes the Anthropic SDK-shaped body to IR, resolves the route with
/// `IngressProtocol::AnthropicMessages`, reuses `dispatch_chat`, and
/// renders responses/errors in the Anthropic envelope (§8.2).
#[instrument(skip_all, fields(request_id, model, stream))]
async fn anthropic_messages(
    State(state): State<AppState>,
    Extension(trace_id): Extension<uuid::Uuid>,
    Extension(runtime): Extension<DispatchRuntime>,
    headers: HeaderMap,
    body: Result<Json<serde_json::Value>, JsonRejection>,
) -> Response<Body> {
    let start = Instant::now();
    let started_at = chrono::Utc::now();

    let Json(raw_body) = match body {
        Ok(json) => json,
        Err(rejection) => {
            emit_client_error_event(&state, trace_id, started_at, &rejection).await;
            // Anthropic ingress uses the Anthropic error envelope.
            let status = rejection.status();
            let msg = rejection.body_text();
            let error_type = match status.as_u16() {
                401 => "authentication_error",
                413 => "request_too_large",
                _ => "invalid_request_error",
            };
            return (
                status,
                Json(serde_json::json!({
                    "type": "error",
                    "error": { "type": error_type, "message": msg },
                })),
            )
                .into_response();
        }
    };

    // Decode Anthropic body → IR (structural drops recorded on ir.loss).
    let ir = sag_providers::AnthropicMessagesCodec::decode_request(&raw_body);
    // Synthesize a ChatCompletionRequest so resolve_route / cancel_guard /
    // analytics have model / stream / reasoning_effort / tools (plan M5-C).
    let (mut request, encode_loss) =
        sag_providers::ChatCompletionsCodec::encode_request_with_loss(&ir);
    if !encode_loss.is_empty() {
        warn!(loss = ?encode_loss.entries, "chat encode recorded losses");
    }
    request.normalize_reasoning_effort();

    let is_stream = request.stream.unwrap_or(false);
    let span = tracing::Span::current();
    span.record("request_id", trace_id.to_string());
    span.record("model", request.model.as_str());
    span.record("stream", is_stream);

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

    let ResolvedRoute {
        targets,
        api_endpoint,
        strategy,
        sticky_token,
        route_name,
        started_at,
        precheck_errors,
    } = match resolve_route_with_ingress(
        &state,
        &runtime,
        &request,
        trace_id,
        start,
        started_at,
        is_stream,
        user_agent.as_deref(),
        client_ip.as_deref(),
        sag_core::IngressProtocol::AnthropicMessages,
    )
    .await
    {
        Ok(resolved) => resolved,
        Err(response) => return render_anthropic_error(response).await,
    };

    let mut errors = precheck_errors;

    if api_endpoint.ends_with("/images/generations") {
        // Image path is Chat-only (plan D8); reject with Anthropic envelope.
        let err = sag_core::AppError::BadRequest(
            "image generation is not supported on /v1/messages".into(),
        );
        return err.into_anthropic_response();
    }

    let response = dispatch_chat(DispatchCtx {
        state: &state,
        runtime: &runtime,
        request: &request,
        targets: &targets,
        api_endpoint: &api_endpoint,
        is_stream,
        trace_id,
        start,
        route_name: &route_name,
        started_at,
        errors: &mut errors,
        strategy,
        sticky_token,
        user_agent,
        client_ip,
        ingress_protocol: sag_core::IngressProtocol::AnthropicMessages,
        ir: Some(&ir),
    })
    .await;

    render_anthropic_error(response).await
}

/// Build an HTTP 422 response with the OpenAI error envelope (§8.2)
/// used by the Responses (and Chat) ingress paths.
fn unprocessable(message: impl Into<String>) -> Response<Body> {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(serde_json::json!({
            "error": {
                "code": "UNPROCESSABLE_ENTITY",
                "message": message.into(),
            },
        })),
    )
        .into_response()
}

/// POST `/v1/responses` — OpenAI Responses ingress, stateless subset (M6).
///
/// Decodes the Responses-shaped body to IR (rejecting D1-unsupported
/// stateful/background features with 422 before dispatch), resolves the
/// route with `IngressProtocol::Responses`, reuses `dispatch_chat`, and
/// renders responses/errors in the OpenAI error envelope (§8.2).
///
/// Auth: Bearer only (x-api-key stays Anthropic-`/v1/messages`-only).
#[instrument(skip_all, fields(request_id, model, stream))]
async fn responses(
    State(state): State<AppState>,
    Extension(trace_id): Extension<uuid::Uuid>,
    Extension(runtime): Extension<DispatchRuntime>,
    headers: HeaderMap,
    body: Result<Json<serde_json::Value>, JsonRejection>,
) -> Response<Body> {
    let start = Instant::now();
    let started_at = chrono::Utc::now();

    let Json(raw_body) = match body {
        Ok(json) => json,
        Err(rejection) => {
            emit_client_error_event(&state, trace_id, started_at, &rejection).await;
            // Responses uses the OpenAI error envelope (§8.2).
            return rejection.into_response();
        }
    };

    // Decode Responses body → IR. D1-unsupported features (and other
    // capability rejects) return BadRequest — surface as 422 **before**
    // entering dispatch (never capability-precheck drops).
    let ir = match sag_providers::ResponsesCodec::decode_request(&raw_body) {
        Ok(ir) => ir,
        Err(err) => {
            let message = match &err {
                sag_core::AppError::BadRequest(m) => m.clone(),
                other => other.to_string(),
            };
            return unprocessable(message);
        }
    };
    // Synthesize a ChatCompletionRequest so resolve_route / cancel_guard /
    // analytics have model / stream / reasoning_effort / tools (plan M5-C).
    let (mut request, encode_loss) =
        sag_providers::ChatCompletionsCodec::encode_request_with_loss(&ir);
    if !encode_loss.is_empty() {
        warn!(loss = ?encode_loss.entries, "chat encode recorded losses");
    }
    request.normalize_reasoning_effort();

    let is_stream = request.stream.unwrap_or(false);
    let span = tracing::Span::current();
    span.record("request_id", trace_id.to_string());
    span.record("model", request.model.as_str());
    span.record("stream", is_stream);

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

    let ResolvedRoute {
        targets,
        api_endpoint,
        strategy,
        sticky_token,
        route_name,
        started_at,
        precheck_errors,
    } = match resolve_route_with_ingress(
        &state,
        &runtime,
        &request,
        trace_id,
        start,
        started_at,
        is_stream,
        user_agent.as_deref(),
        client_ip.as_deref(),
        sag_core::IngressProtocol::Responses,
    )
    .await
    {
        Ok(resolved) => resolved,
        // Resolution failures already use the OpenAI error envelope — pass through.
        Err(response) => return response,
    };

    let mut errors = precheck_errors;

    if api_endpoint.ends_with("/images/generations") {
        // Image path is Chat-only (D8); reject with the OpenAI envelope.
        return unprocessable("image generation is not supported on /v1/responses");
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
        route_name: &route_name,
        started_at,
        errors: &mut errors,
        strategy,
        sticky_token,
        user_agent,
        client_ip,
        ingress_protocol: sag_core::IngressProtocol::Responses,
        ir: Some(&ir),
    })
    .await
}

/// Convert an error response (OpenAI `{"error":{...}}` envelope) into the
/// Anthropic ingress envelope. Successful responses pass through unchanged.
async fn render_anthropic_error(response: Response<Body>) -> Response<Body> {
    if response.status().is_success() {
        return response;
    }
    let status = response.status();
    // Streaming / SSE bodies (post-commit) are left alone — only JSON
    // error envelopes are re-rendered.
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type.contains("application/json") {
        return response;
    }
    let bytes = match axum::body::to_bytes(response.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(_) => {
            return Response::builder()
                .status(status)
                .body(Body::empty())
                .unwrap();
        }
    };
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_default();
    // Already Anthropic-shaped?
    if parsed.get("type").and_then(|t| t.as_str()) == Some("error") {
        return (status, bytes).into_response();
    }
    let message = parsed
        .pointer("/error/message")
        .and_then(|m| m.as_str())
        .unwrap_or("request failed")
        .to_string();
    let error_type = match status.as_u16() {
        401 => "authentication_error",
        400 | 422 => "invalid_request_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        403 => "permission_error",
        s if (500..600).contains(&s) => "api_error",
        _ => "api_error",
    };
    (
        status,
        Json(serde_json::json!({
            "type": "error",
            "error": { "type": error_type, "message": message },
        })),
    )
        .into_response()
}

/// `requested_model` placeholder for requests whose JSON body never parsed
/// (no model is knowable without guessing the body).
const UNKNOWN_MODEL: &str = "unknown";

/// Enqueue the formal `client_error` analytics fact for a request that
/// passed auth but failed JSON extraction, before the rejection response
/// is returned.
///
/// Mirrors the route-failure contract of `sag-dispatch`: zero target
/// attempts, `route_name = None` (the writer keeps `route_id` NULL),
/// `requested_model` a fixed unknown placeholder, and `failure_reason`
/// derived from the rejection status — 413 → `payload_too_large`, every
/// other input/parse failure → `invalid_request`. Written via
/// [`EventCollector::emit`] (never `try_emit`) so the formal fact is not
/// dropped; a collector failure is only possible once the collector is
/// shut down, and it is logged and swallowed so the already-built
/// rejection response keeps its exact semantics.
async fn emit_client_error_event(
    state: &AppState,
    trace_id: uuid::Uuid,
    started_at: chrono::DateTime<chrono::Utc>,
    rejection: &JsonRejection,
) {
    let failure_reason = if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        FailureReason::PayloadTooLarge
    } else {
        FailureReason::InvalidRequest
    };
    let event = AnalyticsEvent::new(RequestResultFact {
        request_id: trace_id,
        route_name: None,
        requested_model: UNKNOWN_MODEL.to_string(),
        final_status: FinalStatus::ClientError,
        failure_reason: Some(failure_reason),
        stream: false,
        started_at,
        completed_at: chrono::Utc::now(),
        usage: None,
        cached_tokens: None,
        cache_creation_tokens: None,
        ttft_ms: None,
        upstream_duration_ms: None,
    });
    if let Err(e) = state.event_collector.emit(event.clone()).await {
        warn!(
            request_id = %event.result.request_id,
            status = ?event.result.final_status,
            error = %e,
            "analytics event not enqueued (event collector shut down)"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::Request;
    use sag_config::{GatewayAuthConfig, GatewayConfig};
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    use super::*;

    /// Build an [`AppState`] whose formal event collector writes into a
    /// shared in-memory pool the test keeps a handle on — same wiring as
    /// `sag_state::testing::test_app_state`, minus the hidden pool (same
    /// pattern as the `sag-dispatch` formal-event tests).
    async fn build_state_with_event_pool(
        config: GatewayConfig,
    ) -> (AppState, DispatchRuntime, sqlx::SqlitePool) {
        let handle = sag_config::ConfigHandle::from_config(config);
        let provider_registry = sag_state::testing::build_provider_registry(&handle, false).await;
        let pool = sag_analytics::open_analytics_pool(":memory:")
            .await
            .unwrap();
        let state = AppState {
            config: handle.clone(),
            providers: provider_registry.clone(),
            coordinator: Arc::new(sag_state::ConfigMutationCoordinator::new(
                handle,
                provider_registry,
            )),
            collector: Arc::new(sag_analytics::CallLogCollector::new_memory().await.unwrap()),
            event_collector: Arc::new(sag_analytics::EventCollector::new(
                sag_analytics::EventWriter::new(pool.clone()),
            )),
            analytics_query: Some(Arc::new(sag_analytics::AnalyticsQueryEngine::new(
                pool.clone(),
            ))),
            circuit_breaker: Arc::new(RwLock::new(sag_core::RouteCircuitBreaker::default())),
            active_targets: Arc::default(),
            fill_first_orders: Arc::default(),
        };
        let runtime = DispatchRuntime::default();
        (state, runtime, pool)
    }

    fn authed_chat_request(body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("Authorization", "Bearer test-key")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// Drain the event queue so the writer commits before we read.
    async fn drain(state: &AppState) {
        state.event_collector.shutdown().await;
    }

    #[tokio::test]
    async fn malformed_json_writes_client_error_fact() {
        let (state, runtime, pool) = build_state_with_event_pool(GatewayConfig {
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["test-key".into()],
            }),
            ..sag_config::gateway_config_base!()
        })
        .await;
        let app = build_router(state.clone(), runtime);

        // Malformed JSON: auth passes, extraction fails → the original
        // rejection response (400) must be preserved…
        let resp = app
            .oneshot(authed_chat_request("{ not valid json"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // …and the formal `client_error` fact must be persisted.
        drain(&state).await;
        let (
            final_status,
            failure_reason,
            route_id,
            requested_model,
            stream,
            started_ms,
            completed_ms,
        ): (
            String,
            Option<String>,
            Option<String>,
            String,
            i32,
            i64,
            i64,
        ) = sqlx::query_as(
            "SELECT final_status, failure_reason, route_id, requested_model, stream, \
                 started_at, completed_at FROM request_results",
        )
        .fetch_one(&pool)
        .await
        .expect("client_error fact must be persisted");

        assert_eq!(final_status, "client_error");
        assert_eq!(failure_reason.as_deref(), Some("invalid_request"));
        assert!(route_id.is_none(), "client_error must keep route_id NULL");
        assert_eq!(requested_model, UNKNOWN_MODEL);
        assert_eq!(stream, 0, "failed parse must record stream=0");
        assert!(
            completed_ms >= started_ms,
            "completed_at must not precede started_at"
        );

        let attempts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM target_attempts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(attempts, 0, "client_error must carry zero target attempts");
    }

    /// Over the axum default body limit (2 MiB) the `Json` extractor
    /// rejects with 413 → the fact must carry `payload_too_large` (the
    /// other branch of the rejection-status mapping).
    #[tokio::test]
    async fn payload_too_large_writes_payload_too_large_fact() {
        let (state, runtime, pool) = build_state_with_event_pool(GatewayConfig {
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["test-key".into()],
            }),
            ..sag_config::gateway_config_base!()
        })
        .await;
        let app = build_router(state.clone(), runtime);

        let oversized = format!(
            "{{\"model\":\"gpt-4o\",\"messages\":[{{\"role\":\"user\",\"content\":\"{}\"}}]}}",
            "x".repeat(2 * 1024 * 1024)
        );
        let resp = app.oneshot(authed_chat_request(&oversized)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

        drain(&state).await;
        let (final_status, failure_reason, route_id): (String, Option<String>, Option<String>) =
            sqlx::query_as("SELECT final_status, failure_reason, route_id FROM request_results")
                .fetch_one(&pool)
                .await
                .expect("client_error fact must be persisted");

        assert_eq!(final_status, "client_error");
        assert_eq!(
            failure_reason.as_deref(),
            Some("payload_too_large"),
            "413 rejections must map to payload_too_large"
        );
        assert!(route_id.is_none());
    }

    #[tokio::test]
    async fn auth_failure_writes_no_fact() {
        let (state, runtime, pool) = build_state_with_event_pool(GatewayConfig {
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["test-key".into()],
            }),
            ..sag_config::gateway_config_base!()
        })
        .await;
        let app = build_router(state.clone(), runtime);

        // Wrong key: auth middleware rejects before the handler — malformed
        // body is irrelevant, and no analytics fact may be written.
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("Authorization", "Bearer wrong-key")
                    .header("content-type", "application/json")
                    .body(Body::from("{ not valid json"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        drain(&state).await;
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_results")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0, "auth failures must stay excluded from analytics");
    }

    #[tokio::test]
    async fn valid_json_keeps_original_dispatch_path() {
        // A parseable body must keep the original dispatch path: with no
        // routes configured the request resolves to `no_route` — proving
        // the extractor change did not divert valid requests to
        // `client_error`.
        let (state, runtime, pool) = build_state_with_event_pool(GatewayConfig {
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["test-key".into()],
            }),
            providers: HashMap::new(),
            routes: vec![],
            ..sag_config::gateway_config_base!()
        })
        .await;
        let app = build_router(state.clone(), runtime);

        let resp = app
            .oneshot(authed_chat_request(
                r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        drain(&state).await;
        let (final_status, failure_reason, requested_model): (String, Option<String>, String) =
            sqlx::query_as(
                "SELECT final_status, failure_reason, requested_model FROM request_results",
            )
            .fetch_one(&pool)
            .await
            .expect("dispatch path must persist its own fact");

        assert_eq!(final_status, "no_route");
        assert_eq!(failure_reason.as_deref(), Some("model_not_found"));
        assert_eq!(
            requested_model, "gpt-4o",
            "parsed requests must carry their real requested model"
        );
    }

    #[tokio::test]
    async fn v2_analytics_routes_registered_and_public() {
        // The v2 analytics routes live next to the legacy `/v1/analytics/*`
        // routes in the outer router: no auth required, and the handler
        // answers with the JSON error format for missing params.
        let (state, runtime, _pool) = build_state_with_event_pool(GatewayConfig {
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["test-key".into()],
            }),
            ..sag_config::gateway_config_base!()
        })
        .await;
        let app = build_router(state.clone(), runtime);

        for path in ["/v1/analytics/v2/summary", "/v1/analytics/v2/trend"] {
            let resp = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            // No Authorization header: the route must be reachable (public)
            // and answer 400 with the JSON error body (missing from/to).
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "path {path}");
            let bytes = http_body_util::BodyExt::collect(resp.into_body())
                .await
                .unwrap()
                .to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["error"]["code"], "BAD_REQUEST", "path {path}");
        }
    }
}
