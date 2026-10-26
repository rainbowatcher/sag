//! Image generation dispatch with multi-provider fallback.

use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use sag_analytics::AnalyticsEvent;
use sag_config::{GatewayConfig, Strategy};
use sag_core::{
    AppError, ChatCompletionRequest, FailureReason, FinalStatus, ImageGenerationRequest,
    ImageGenerationResponse, RequestResultFact, TargetAttemptFact, TargetStatus,
};
use sag_providers::LlmProvider;

use crate::dispatch::resolve::resolve_provider;
use crate::dispatch::retry::RetryOutcome;
use crate::dispatch::retry::is_client_caused_4xx;
use crate::dispatch::retry::run_with_retries_outcome;
use crate::dispatch::{
    CircuitPermit, DispatchCancelGuard, DispatchCtx, ResolvedTarget, apply_circuit_outcome,
    circuit_allows_target, client_error_failure_reason, emit_analytics_event,
    fallback_exhausted_response, log_per_target_failure, merge_model_overrides, success_log,
    target_base_url,
};
use crate::state::{AppState, DispatchRuntime};

/// Image generation fallback loop. Mirrors the chat dispatch shape but
/// always non-streaming.
///
/// `is_stream` is unused here (image generation is always non-streaming),
/// but it's part of [`DispatchCtx`] so the call site can build the same
/// context for both dispatch paths. `api_endpoint` is the route's real
/// upstream path — the provider appends it to `apis.image.base_url`.
pub async fn dispatch_image(ctx: DispatchCtx<'_>) -> Response<Body> {
    let DispatchCtx {
        state,
        runtime,
        request,
        targets,
        api_endpoint,
        is_stream: _,
        trace_id,
        start,
        // Route-level identity facts for the analytics request event:
        // the matched route's `model_name` and the wall-clock timestamp
        // captured at route scheduling time.
        route_name,
        started_at,
        errors,
        strategy,
        sticky_token,
        user_agent,
        client_ip,
        ingress_protocol: _,
        ir: _,
        entry_loss: _,
        session_key,
        session_writeback: _,
    } = ctx;

    // Per-route in-flight guard: `+1` for this request, released by `Drop`
    // when this dispatch returns (image generation is always non-streaming,
    // so the guard simply lives on this stack frame — every return path,
    // error and panic unwind included, releases exactly once).
    let _in_flight = runtime.in_flight.acquire(route_name);

    // Hashed session identity for every `call_logs` row of this dispatch
    // (recorded regardless of strategy — a request fact, not a behavior).
    let session_log_key = session_key.as_ref().map(|k| k.map_id(&request.model));

    // Session-registry fact for the log page's session info card: same key
    // and route model as `session_log_key`, written exactly **once per
    // request** (never in a terminal path). Image dispatch carries no IR, so
    // only the preview is dropped — `conversation_id` is still recorded.
    if let Some(key) = session_key.as_ref() {
        let fact = sag_core::build_session_fact(key, &request.model, None, started_at);
        state.collector.log_session(fact);
    }

    // Client-disconnect terminal (image dispatch is non-streaming, so this
    // is the only cancellation detection point): if the handler future is
    // dropped before any normal terminal, emit `client_cancelled` — with a
    // `cancelled` attempt if a provider call was in flight, 0 attempts
    // otherwise. Created first so even a cancel during the config snapshot
    // is covered. Every terminal below calls `mark_done()` first.
    let cancel_guard = DispatchCancelGuard::new(
        state.event_collector.clone(),
        state.collector.clone(),
        trace_id,
        route_name,
        &request.model,
        false,
        // image generation is always non-streaming
        sag_core::IngressProtocol::ChatCompletions,
        started_at,
        start,
        None,
        // image requests carry no reasoning_effort
        user_agent.clone(),
        client_ip.clone(),
        session_log_key.clone(),
    );

    let img_request = build_image_request(request);

    // Snapshot the config once for the whole dispatch (backoff, circuit
    // breaker, provider overrides) so the per-target retry loop doesn't
    // hold the config read lock — see `dispatch_chat` for the rationale.
    let config = state.config.read().await.clone();

    let attempt_ctx = ImageAttemptCtx {
        state,
        runtime,
        request_model: &request.model,
        img_request: &img_request,
        api_endpoint,
        config: &config,
        cancel_guard: &cancel_guard,
        strategy,
        sticky_token: &sticky_token,
        trace_id,
        start,
        user_agent: user_agent.as_deref(),
        client_ip: client_ip.as_deref(),
        session_key: session_log_key,
    };

    run_image_fallback(&attempt_ctx, route_name, started_at, targets, errors).await
}

/// Drive the image fallback loop to a terminal response: try each target
/// via [`attempt_one_image_target`], returning on the first success /
/// client error, or the all-exhausted response when every target fell
/// through. Each terminal emits its own analytics request event.
///
/// One `TargetAttemptFact` per actually-issued target, emitted once in a
/// single atomic request event at the terminal. Skipped targets (circuit
/// open / provider disabled) never reach the provider and produce no
/// fact.
async fn run_image_fallback(
    attempt_ctx: &ImageAttemptCtx<'_>,
    route_name: &str,
    started_at: chrono::DateTime<chrono::Utc>,
    targets: &[ResolvedTarget],
    errors: &mut Vec<String>,
) -> Response<Body> {
    let mut target_facts: Vec<TargetAttemptFact> = Vec::new();

    for target in targets {
        match attempt_one_image_target(attempt_ctx, target, errors, &mut target_facts).await {
            TargetOutcome::Success(resp) => {
                return finish_image_success(
                    attempt_ctx,
                    route_name,
                    started_at,
                    target,
                    target_facts,
                    resp,
                )
                .await;
            }
            TargetOutcome::ClientError(err) => {
                return finish_image_client_error(attempt_ctx, started_at, err).await;
            }
            TargetOutcome::Failover => continue,
        }
    }

    finish_image_exhausted(attempt_ctx, route_name, started_at, errors, target_facts).await
}

/// Success terminal: per-target success log, then claim the cancel-guard
/// terminal via `mark_done` and emit the `success` request event (see
/// [`emit_image_success_event`]). Image responses carry no usage — all
/// token / cache fields stay NULL (unknown), never fabricated as 0.
/// Returns the 200 JSON response.
async fn finish_image_success(
    attempt_ctx: &ImageAttemptCtx<'_>,
    route_name: &str,
    started_at: chrono::DateTime<chrono::Utc>,
    target: &ResolvedTarget,
    target_facts: Vec<TargetAttemptFact>,
    resp: ImageGenerationResponse,
) -> Response<Body> {
    attempt_ctx.state.collector.log(success_log(
        attempt_ctx.trace_id,
        &target.provider,
        attempt_ctx.request_model,
        &target.real_model,
        false,
        attempt_ctx.start,
        None,
        None,
        // reasoning_effort — not applicable for image generation
        None,
        // upstream_duration_ms — image path timing is out of scope for stage 2
        None,
        // ttft_ms — image generation is single-shot, conceptually non-streaming
        attempt_ctx.user_agent.map(str::to_string),
        attempt_ctx.client_ip.map(str::to_string),
        Some(
            sag_core::IngressProtocol::ChatCompletions
                .as_str()
                .to_string(),
        ),
        Some(target.upstream_protocol.as_str().to_string()),
        Some(target.lane.as_str().to_string()),
        None,
        // image path has no IR LossInventory,
        attempt_ctx.session_key.clone(),
    ));
    emit_image_success_event(
        attempt_ctx.state,
        attempt_ctx.cancel_guard,
        attempt_ctx.trace_id,
        route_name,
        attempt_ctx.request_model,
        started_at,
        target_facts,
    )
    .await;
    (StatusCode::OK, Json(resp)).into_response()
}

/// Client-error terminal: emit the `client_error` request event (see
/// [`emit_image_client_error_event`]) — no route identity, no target
/// attempts (writer validation contract) — then hand the error straight
/// back to the client: no failover to the next target, no pin movement,
/// no circuit change. `into_response` transparently forwards the
/// upstream status code + body.
async fn finish_image_client_error(
    attempt_ctx: &ImageAttemptCtx<'_>,
    started_at: chrono::DateTime<chrono::Utc>,
    err: AppError,
) -> Response<Body> {
    emit_image_client_error_event(
        attempt_ctx.state,
        attempt_ctx.cancel_guard,
        attempt_ctx.trace_id,
        attempt_ctx.request_model,
        started_at,
        &err,
    )
    .await;
    err.into_response()
}

/// All-exhausted terminal: build the 502 fallback response (which also
/// logs the `__all_exhausted__` call row), then claim the cancel-guard
/// terminal via `mark_done` and emit the `model_failure` request event
/// with `all_exhausted` and one failure fact per actually-issued target
/// (see [`emit_image_model_failure_event`]).
async fn finish_image_exhausted(
    attempt_ctx: &ImageAttemptCtx<'_>,
    route_name: &str,
    started_at: chrono::DateTime<chrono::Utc>,
    errors: &[String],
    target_facts: Vec<TargetAttemptFact>,
) -> Response<Body> {
    let response = fallback_exhausted_response(
        attempt_ctx.state,
        attempt_ctx.request_model,
        false,
        attempt_ctx.trace_id,
        attempt_ctx.start,
        errors,
        None,
        // reasoning_effort — not applicable for image generation
        attempt_ctx.user_agent,
        attempt_ctx.client_ip,
        Some(
            sag_core::IngressProtocol::ChatCompletions
                .as_str()
                .to_string(),
        ),
        None,
        None,
        None,
        attempt_ctx.session_key.clone(),
    );
    emit_image_model_failure_event(
        attempt_ctx.state,
        attempt_ctx.cancel_guard,
        attempt_ctx.trace_id,
        route_name,
        attempt_ctx.request_model,
        started_at,
        target_facts,
    )
    .await;
    response
}

/// Verdict of one target attempt in the image fallback loop.
enum TargetOutcome {
    /// The provider produced an image response — terminal success.
    Success(ImageGenerationResponse),
    /// Client-attributed 4xx — terminal, failover forbidden.
    ClientError(AppError),
    /// Continue with the next target: either the target was skipped
    /// before any provider call (circuit open / provider disabled — no
    /// fact) or its attempt failed provider-attributably (one fact was
    /// recorded).
    Failover,
}

/// Bundled dispatch-level parameters for one target attempt, so
/// [`attempt_one_image_target`] keeps a short signature as the loop body
/// grows. Mirrors the [`DispatchCtx`] pattern.
struct ImageAttemptCtx<'a> {
    state: &'a AppState,
    runtime: &'a DispatchRuntime,
    request_model: &'a str,
    img_request: &'a ImageGenerationRequest,
    /// Route-declared upstream path appended to `apis.image.base_url`.
    api_endpoint: &'a str,
    config: &'a GatewayConfig,
    cancel_guard: &'a DispatchCancelGuard,
    strategy: Strategy,
    sticky_token: &'a sag_state::StickyToken,
    trace_id: uuid::Uuid,
    start: Instant,
    user_agent: Option<&'a str>,
    client_ip: Option<&'a str>,
    /// Hashed session identity for the terminal `call_logs` rows.
    session_key: Option<String>,
}

/// Run one target of the image fallback loop: circuit gate → provider
/// resolve → retries → single attempt fact → circuit outcome → sticky
/// writeback on success. Skipped targets (circuit open / provider
/// disabled) produce no fact; client-caused 4xx produce no fact either
/// (writer validation contract — see [`record_image_attempt`]).
async fn attempt_one_image_target(
    ctx: &ImageAttemptCtx<'_>,
    target: &ResolvedTarget,
    errors: &mut Vec<String>,
    target_facts: &mut Vec<TargetAttemptFact>,
) -> TargetOutcome {
    let Some(prepared) = prepare_image_attempt(ctx, target, errors).await else {
        return TargetOutcome::Failover;
    };

    let (outcome, started_at) = send_image_attempt(ctx, &target.provider, &prepared, errors).await;

    let (category, attempt_status) = image_failure_log_fields(&outcome);
    record_image_attempt(
        ctx.cancel_guard,
        &target.provider,
        &prepared.real_model,
        &outcome,
        started_at,
        target_facts,
        Some(prepared.upstream_protocol),
        Some(prepared.lane),
    );

    apply_image_outcome(
        ctx,
        target,
        prepared,
        outcome,
        category,
        attempt_status,
        errors,
    )
    .await
}

/// Inputs for one circuit-gated, provider-resolved image attempt:
/// everything [`send_image_attempt`] and [`apply_image_outcome`] need
/// after [`prepare_image_attempt`] succeeds.
struct PreparedAttempt<'a> {
    permit: CircuitPermit<'a>,
    provider: Arc<dyn LlmProvider>,
    real_model: String,
    target_extra: HashMap<String, serde_json::Value>,
    img_request: ImageGenerationRequest,
    /// Route-declared upstream path appended to `apis.image.base_url`.
    api_endpoint: &'a str,
    retries: u32,
    timeout_secs: u64,
    upstream_protocol: &'static str,
    lane: &'static str,
}

/// Stage 1 — circuit gate → provider resolve → cloned per-attempt
/// inputs. `None` means the target is skipped before any provider call
/// (circuit open / provider disabled): no attempt fact is produced; a
/// claimed half-open probe slot is released on the provider-unresolved
/// path so `probe_in_flight` cannot stick true forever.
async fn prepare_image_attempt<'a>(
    ctx: &ImageAttemptCtx<'a>,
    target: &ResolvedTarget,
    errors: &mut Vec<String>,
) -> Option<PreparedAttempt<'a>> {
    let provider_name = &target.provider;
    let real_model = &target.real_model;

    // The circuit key folds in the upstream base address; an image target
    // dispatches to `apis.image` (its actually-used catalog entry).
    let circuit_url = target_base_url(
        &ctx.config.providers,
        provider_name,
        target.upstream_protocol,
    );
    let Some(permit) = circuit_allows_target(
        ctx.runtime,
        ctx.request_model,
        provider_name,
        real_model,
        &circuit_url,
        &ctx.config.circuit_breaker,
    )
    .await
    else {
        errors.push(format!("{provider_name}/{real_model}: circuit open"));
        return None;
    };

    let Some(provider) = resolve_provider(ctx.state, provider_name, errors).await else {
        // Provider is disabled / unregistered. If this request claimed the
        // half-open probe slot we must release it synchronously, otherwise
        // the target would be skipped forever (probe_in_flight stuck
        // true). `release_probe_slot` consumes the guard (mark_completed
        // first), so the drop after this branch can never release twice.
        if let CircuitPermit::Probe(guard) = permit {
            guard.release_probe_slot().await;
        }
        return None;
    };

    let real_model = real_model.clone();
    let target_extra = merge_model_overrides(
        &ctx.config.providers,
        provider_name,
        &real_model,
        &target.extra,
    );

    Some(PreparedAttempt {
        permit,
        provider,
        real_model,
        target_extra,
        img_request: ctx.img_request.clone(),
        api_endpoint: ctx.api_endpoint,
        retries: target.retries,
        timeout_secs: target.timeout_secs,
        upstream_protocol: target.upstream_protocol.as_str(),
        lane: target.lane.as_str(),
    })
}

/// Stage 2 — run the retry sequence for a prepared target. Returns the
/// terminal outcome plus the wall-clock start of attempt 0 (the first
/// actual provider call), which the collapsed attempt fact uses so
/// retries of the same target extend its single duration.
async fn send_image_attempt(
    ctx: &ImageAttemptCtx<'_>,
    provider_name: &str,
    prepared: &PreparedAttempt<'_>,
    errors: &mut Vec<String>,
) -> (
    RetryOutcome<ImageGenerationResponse>,
    Option<chrono::DateTime<chrono::Utc>>,
) {
    // Wall-clock start of this target's first actual provider call,
    // captured inside the retry closure on attempt 0. Retries of the
    // same target extend the single fact's duration.
    let mut target_started_at: Option<chrono::DateTime<chrono::Utc>> = None;

    let outcome = run_with_retries_outcome::<ImageGenerationResponse, _, _>(
        provider_name,
        prepared.retries,
        &ctx.config.backoff,
        errors,
        |attempt| {
            if attempt == 0 {
                target_started_at = Some(chrono::Utc::now());
            }
            let provider = prepared.provider.clone();
            let real_model = prepared.real_model.clone();
            let target_extra = prepared.target_extra.clone();
            let img_request = prepared.img_request.clone();
            let api_endpoint = prepared.api_endpoint;
            let timeout_secs = prepared.timeout_secs;
            let cancel = ctx.cancel_guard.analytics();
            async move {
                // An actual provider call is in flight from here until
                // the attempt's future resolves; a client disconnect
                // during this window is persisted as a `cancelled`
                // attempt by the dispatch cancel guard (spec scenario
                // 11).
                cancel.begin_attempt(
                    provider_name,
                    &real_model,
                    Some(prepared.upstream_protocol),
                    Some(prepared.lane),
                );
                let result = provider
                    .chat_image(
                        &img_request,
                        &real_model,
                        api_endpoint,
                        &target_extra,
                        timeout_secs,
                    )
                    .await;
                cancel.end_attempt();
                result
            }
        },
    )
    .await;

    (outcome, target_started_at)
}

/// Per-target failure-log fields extracted from the terminal retry
/// outcome *before* [`apply_image_outcome`] consumes it: the error
/// category and the real HTTP status, so the per-target `call_logs` row
/// carries the actual upstream status (429/500/503/…) instead of a
/// hardcoded 502. `Success` sentinels are never logged.
fn image_failure_log_fields(
    outcome: &RetryOutcome<ImageGenerationResponse>,
) -> (&'static str, u16) {
    let category = match outcome {
        RetryOutcome::Exhausted(e) => e.error_category(),
        RetryOutcome::NonRetryable(_) => "bad_request",
        RetryOutcome::Success(_) => "", // never logged
    };
    let attempt_status = match outcome {
        RetryOutcome::Exhausted(e) | RetryOutcome::NonRetryable(e) => e.status_code().as_u16(),
        RetryOutcome::Success(_) => 200, // never logged
    };
    (category, attempt_status)
}

/// Stage 3 — collapse this target's whole retry sequence into exactly
/// one [`TargetAttemptFact`] (final outcome only — no retry counts). The
/// client-caused 4xx path is excluded: per the writer's validation
/// contract a `client_error` event can carry no target attempts, so
/// that attempt is recorded only in call_logs.
#[allow(clippy::too_many_arguments)]
fn record_image_attempt(
    cancel_guard: &DispatchCancelGuard,
    provider_name: &str,
    real_model: &str,
    outcome: &RetryOutcome<ImageGenerationResponse>,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    target_facts: &mut Vec<TargetAttemptFact>,
    upstream_protocol: Option<&str>,
    lane: Option<&str>,
) {
    let is_client_error_path =
        matches!(outcome, RetryOutcome::NonRetryable(e) if is_client_caused_4xx(e));
    if !is_client_error_path
        && let Some(fact) = build_target_attempt_fact(
            provider_name,
            real_model,
            outcome,
            started_at,
            chrono::Utc::now(),
            upstream_protocol,
            lane,
        )
    {
        // Mirror the fact into the cancel guard *before* continuing to
        // the next target: if the client disconnects while a later
        // target is in flight (or during the fallback gap), the final
        // `client_cancelled` event must still carry this completed
        // failure. The normal terminals consume the loop-local vec
        // only and `mark_done` first, so the guard's copy is dropped
        // unread — never a duplicate row.
        cancel_guard.record_target_fact(fact.clone());
        target_facts.push(fact);
    }
}

/// Stage 4 — apply the terminal retry outcome to the circuit breaker and
/// map it to the loop verdict: client-caused 4xx →
/// [`TargetOutcome::ClientError`] (terminal, failover forbidden),
/// otherwise failure → [`TargetOutcome::Failover`], success →
/// [`TargetOutcome::Success`] plus sticky writeback.
async fn apply_image_outcome<'a>(
    ctx: &ImageAttemptCtx<'a>,
    target: &ResolvedTarget,
    prepared: PreparedAttempt<'a>,
    outcome: RetryOutcome<ImageGenerationResponse>,
    category: &str,
    attempt_status: u16,
    errors: &[String],
) -> TargetOutcome {
    // Client-attributed 4xx (400/413/422/unknown 4xx): return straight
    // to the client — no failover, no pin movement, no circuit change.
    // `AppError` is not `Clone`, so peel the error out of the outcome by
    // value and hand the breaker a placeholder `NonRetryable` (its
    // payload is ignored — the breaker only needs the verdict to release
    // a half-open probe slot). The peel happens *before*
    // `apply_circuit_outcome` consumes the outcome.
    let (client_error, cb_outcome) = match outcome {
        RetryOutcome::NonRetryable(e) if is_client_caused_4xx(&e) => (
            Some(e),
            RetryOutcome::NonRetryable(AppError::Internal("client 4xx".into())),
        ),
        other => (None, other),
    };

    let PreparedAttempt {
        permit, real_model, ..
    } = prepared;

    let Some(resp) =
        apply_circuit_outcome(ctx.runtime, permit, cb_outcome, &ctx.config.circuit_breaker).await
    else {
        // Per-provider failure: log a failure entry with the actual provider/real_model
        // so the analytics table knows which provider was attempted.
        log_per_target_failure(
            ctx.state,
            ctx.trace_id,
            &target.provider,
            ctx.request_model,
            &real_model,
            false,
            ctx.start,
            errors,
            category,
            attempt_status,
            None,
            // reasoning_effort — not applicable for image generation
            ctx.user_agent,
            ctx.client_ip,
            Some(
                sag_core::IngressProtocol::ChatCompletions
                    .as_str()
                    .to_string(),
            ),
            Some(target.upstream_protocol.as_str().to_string()),
            Some(target.lane.as_str().to_string()),
            None,
            ctx.session_key.clone(),
        );
        if let Some(err) = client_error {
            return TargetOutcome::ClientError(err);
        }
        return TargetOutcome::Failover; // provider-persistent 4xx: failover as before
    };

    if ctx.strategy == Strategy::StickyFailover {
        writeback_sticky_target(
            ctx.state,
            ctx.runtime,
            ctx.request_model,
            target,
            ctx.sticky_token,
        )
        .await;
    }

    TargetOutcome::Success(resp)
}

/// Sticky-failover writeback for a successful image target. Conditional
/// version/generation guard — see `dispatch_chat` for the race this
/// closes: the config read guard is held across the version read *and*
/// the active write so a config mutation (commit + version bump are
/// atomic under `config.write()`) can never interleave between them:
/// either the writeback linearizes before the mutation or the version
/// mismatch rejects the pin. The read-compare-insert runs under the
/// single active write lock.
async fn writeback_sticky_target(
    state: &AppState,
    runtime: &DispatchRuntime,
    request_model: &str,
    target: &ResolvedTarget,
    sticky_token: &sag_state::StickyToken,
) {
    let _config_guard = state.config.read().await;
    let current_version = state.config.routes_version();
    let mut active = runtime.active_targets.write().await;
    active.insert_if_generation(
        request_model.to_string(),
        sag_state::ActiveTargetEntry::new(
            target.provider.clone(),
            target.real_model.clone(),
            target.route_model.clone(),
            target.target_real_model.clone(),
            Instant::now(),
        ),
        sticky_token,
        current_version,
    );
    drop(active);
}

/// Formal success terminal: claim the cancel-guard terminal via
/// `mark_done`, then emit the `success` request event. Image responses
/// carry no usage — all token / cache fields stay NULL (unknown), never
/// fabricated as 0.
async fn emit_image_success_event(
    state: &AppState,
    cancel_guard: &DispatchCancelGuard,
    trace_id: uuid::Uuid,
    route_name: &str,
    requested_model: &str,
    started_at: chrono::DateTime<chrono::Utc>,
    target_facts: Vec<TargetAttemptFact>,
) {
    cancel_guard.mark_done();
    emit_analytics_event(
        state,
        AnalyticsEvent {
            result: RequestResultFact {
                request_id: trace_id,
                route_name: Some(route_name.to_string()),
                requested_model: requested_model.to_string(),
                final_status: FinalStatus::Success,
                failure_reason: None,
                stream: false,
                started_at,
                completed_at: chrono::Utc::now(),
                usage: None,
                cached_tokens: None,
                cache_creation_tokens: None,
                ttft_ms: None,
                upstream_duration_ms: None,
                ingress_protocol: Some(
                    sag_core::IngressProtocol::ChatCompletions
                        .as_str()
                        .to_string(),
                ),
                lane: None,
            },
            targets: target_facts,
        },
    )
    .await;
}

/// Formal client-error terminal: claim the cancel-guard terminal via
/// `mark_done`, then emit the `client_error` request event — no route
/// identity and no target attempts (writer validation contract); the
/// attempt is still visible in the legacy call_logs row.
async fn emit_image_client_error_event(
    state: &AppState,
    cancel_guard: &DispatchCancelGuard,
    trace_id: uuid::Uuid,
    requested_model: &str,
    started_at: chrono::DateTime<chrono::Utc>,
    err: &AppError,
) {
    cancel_guard.mark_done();
    emit_analytics_event(
        state,
        AnalyticsEvent::new(RequestResultFact {
            request_id: trace_id,
            route_name: None,
            requested_model: requested_model.to_string(),
            final_status: FinalStatus::ClientError,
            failure_reason: Some(client_error_failure_reason(err)),
            stream: false,
            started_at,
            completed_at: chrono::Utc::now(),
            usage: None,
            cached_tokens: None,
            cache_creation_tokens: None,
            ttft_ms: None,
            upstream_duration_ms: None,
            ingress_protocol: Some(
                sag_core::IngressProtocol::ChatCompletions
                    .as_str()
                    .to_string(),
            ),
            lane: None,
        }),
    )
    .await;
}

/// Formal all-exhausted terminal: claim the cancel-guard terminal via
/// `mark_done`, then emit the `model_failure` request event with
/// `all_exhausted` and one failure fact per actually-issued target
/// (circuit-open / disabled skips never produce facts).
async fn emit_image_model_failure_event(
    state: &AppState,
    cancel_guard: &DispatchCancelGuard,
    trace_id: uuid::Uuid,
    route_name: &str,
    requested_model: &str,
    started_at: chrono::DateTime<chrono::Utc>,
    target_facts: Vec<TargetAttemptFact>,
) {
    cancel_guard.mark_done();
    emit_analytics_event(
        state,
        AnalyticsEvent {
            result: RequestResultFact {
                request_id: trace_id,
                route_name: Some(route_name.to_string()),
                requested_model: requested_model.to_string(),
                final_status: FinalStatus::ModelFailure,
                failure_reason: Some(FailureReason::AllExhausted),
                stream: false,
                started_at,
                completed_at: chrono::Utc::now(),
                usage: None,
                cached_tokens: None,
                cache_creation_tokens: None,
                ttft_ms: None,
                upstream_duration_ms: None,
                ingress_protocol: Some(
                    sag_core::IngressProtocol::ChatCompletions
                        .as_str()
                        .to_string(),
                ),
                lane: None,
            },
            targets: target_facts,
        },
    )
    .await;
}

/// Build the single [`TargetAttemptFact`] for an actually-issued target
/// from its final retry outcome.
///
/// - retry success → one `Success` row (all attempts of the retry
///   sequence collapse into it, no retry count);
/// - retry exhaustion / non-retryable → one `Failure` row; the failure
///   tokens stay NULL (unknown — never fabricated as 0).
fn build_target_attempt_fact(
    provider: &str,
    real_model: &str,
    outcome: &RetryOutcome<ImageGenerationResponse>,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    completed_at: chrono::DateTime<chrono::Utc>,
    upstream_protocol: Option<&str>,
    lane: Option<&str>,
) -> Option<TargetAttemptFact> {
    // `started_at` is captured at the first actual provider call; fall back
    // to `completed_at` only if the closure never ran (cannot happen in
    // practice — the retry loop always issues attempt 0).
    let started_at = started_at.unwrap_or(completed_at);
    let upstream_protocol = upstream_protocol.map(str::to_string);
    let lane = lane.map(str::to_string);
    match outcome {
        RetryOutcome::Success(_) => Some(TargetAttemptFact {
            provider: provider.to_string(),
            real_model: real_model.to_string(),
            status: TargetStatus::Success,
            failure_reason: None,
            started_at,
            completed_at,
            // Image responses carry no usage — keep NULL (unknown).
            usage: None,
            cached_tokens: None,
            ttft_ms: None,
            upstream_duration_ms: None,
            upstream_protocol,
            lane,
        }),
        RetryOutcome::Exhausted(err) | RetryOutcome::NonRetryable(err) => Some(TargetAttemptFact {
            provider: provider.to_string(),
            real_model: real_model.to_string(),
            status: TargetStatus::Failure,
            failure_reason: err.failure_reason(),
            started_at,
            completed_at,
            usage: None,
            cached_tokens: None,
            ttft_ms: None,
            upstream_duration_ms: None,
            upstream_protocol,
            lane,
        }),
    }
}

/// Build an `ImageGenerationRequest` from a chat completion request,
/// forwarding the well-known extra fields (`size`, `quality`, etc.).
pub fn build_image_request(request: &ChatCompletionRequest) -> ImageGenerationRequest {
    let prompt = request
        .messages
        .last()
        .and_then(|m| m.text_content().map(String::from))
        .unwrap_or_default();

    let mut extra = serde_json::Map::new();
    for key in ["size", "quality", "response_format", "style"] {
        if let Some(v) = request.extra.get(key) {
            extra.insert(key.to_string(), v.clone());
        }
    }

    ImageGenerationRequest {
        model: request.model.clone(),
        prompt,
        n: request.n,
        size: None,
        quality: None,
        response_format: None,
        extra,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::ResolvedTarget;
    use crate::state::{AppState, DispatchRuntime};
    use crate::testing::target;
    use async_trait::async_trait;
    use futures_util::Stream;

    use sag_config::{CircuitBreakerConfig, ConfigHandle, GatewayAuthConfig, GatewayConfig};
    use sag_core::{AppError, ChatCompletionResponse, ChatMessage, ImageData, StreamChunk};
    use sag_providers::LlmProvider;
    use serde_json::json;
    use std::collections::{HashMap, VecDeque};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;
    use tokio::sync::RwLock;
    use uuid::Uuid;

    enum ImageBehavior {
        Ok(ImageGenerationResponse),
        Retryable(AppError),
    }

    struct MockImageProvider {
        name: String,
        image_queue: Mutex<VecDeque<ImageBehavior>>,
        image_calls: AtomicU32,
        /// Route-declared path of the most recent `chat_image` call —
        /// regression surface for the `api_endpoint` passthrough.
        last_api_endpoint: Mutex<Option<String>>,
    }

    impl MockImageProvider {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                image_queue: Mutex::new(VecDeque::new()),
                image_calls: AtomicU32::new(0),
                last_api_endpoint: Mutex::new(None),
            }
        }

        fn push_image(&self, behavior: ImageBehavior) {
            self.image_queue.lock().unwrap().push_back(behavior);
        }

        fn image_calls(&self) -> u32 {
            self.image_calls.load(Ordering::SeqCst)
        }

        fn last_api_endpoint(&self) -> Option<String> {
            self.last_api_endpoint.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LlmProvider for MockImageProvider {
        fn name(&self) -> &str {
            &self.name
        }

        async fn chat(
            &self,
            _request: &ChatCompletionRequest,
            _real_model: &str,
            _api_endpoint: &str,
            _target_extra: &HashMap<String, serde_json::Value>,
            _timeout_secs: u64,
        ) -> sag_core::Result<ChatCompletionResponse> {
            unreachable!("image dispatch should not call chat")
        }

        async fn chat_stream(
            &self,
            _request: &ChatCompletionRequest,
            _real_model: &str,
            _api_endpoint: &str,
            _target_extra: &HashMap<String, serde_json::Value>,
            _timeout_secs: u64,
        ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
            unreachable!("image dispatch should not call chat_stream")
        }

        async fn chat_image(
            &self,
            _request: &ImageGenerationRequest,
            _real_model: &str,
            api_endpoint: &str,
            _target_extra: &HashMap<String, serde_json::Value>,
            _timeout_secs: u64,
        ) -> sag_core::Result<ImageGenerationResponse> {
            self.image_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_api_endpoint.lock().unwrap() = Some(api_endpoint.to_string());
            match self.image_queue.lock().unwrap().pop_front() {
                Some(ImageBehavior::Ok(resp)) => Ok(resp),
                Some(ImageBehavior::Retryable(err)) => Err(err),
                None => Ok(build_image_response()),
            }
        }
    }

    /// Provider whose `chat_image` never resolves: holds a dispatch in an
    /// in-flight image call so the test can cancel the handler future
    /// mid-call (client-disconnect simulation).
    struct BlockingImageProvider {
        name: String,
    }

    impl BlockingImageProvider {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for BlockingImageProvider {
        fn name(&self) -> &str {
            &self.name
        }

        async fn chat(
            &self,
            _request: &ChatCompletionRequest,
            _real_model: &str,
            _api_endpoint: &str,
            _target_extra: &HashMap<String, serde_json::Value>,
            _timeout_secs: u64,
        ) -> sag_core::Result<ChatCompletionResponse> {
            unreachable!("image dispatch should not call chat")
        }

        async fn chat_stream(
            &self,
            _request: &ChatCompletionRequest,
            _real_model: &str,
            _api_endpoint: &str,
            _target_extra: &HashMap<String, serde_json::Value>,
            _timeout_secs: u64,
        ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
            unreachable!("image dispatch should not call chat_stream")
        }

        async fn chat_image(
            &self,
            _request: &ImageGenerationRequest,
            _real_model: &str,
            _api_endpoint: &str,
            _target_extra: &HashMap<String, serde_json::Value>,
            _timeout_secs: u64,
        ) -> sag_core::Result<ImageGenerationResponse> {
            std::future::pending().await
        }
    }

    fn build_image_response() -> ImageGenerationResponse {
        ImageGenerationResponse {
            created: 0,
            data: vec![ImageData {
                url: Some("https://example.test/image.png".into()),
                b64_json: None,
                revised_prompt: None,
            }],
        }
    }

    fn build_request(model: &str) -> ChatCompletionRequest {
        serde_json::from_value(json!({
            "model": model,
            "messages": [{"role": "user", "content": "draw a cat"}],
        }))
        .expect("valid image test request")
    }

    fn build_request_with_reasoning(model: &str) -> ChatCompletionRequest {
        serde_json::from_value(json!({
            "model": model,
            "messages": [{"role": "user", "content": "draw a cat"}],
            "reasoning_effort": "high",
        }))
        .expect("valid image test request with reasoning_effort")
    }

    async fn build_state(providers: Vec<Arc<dyn LlmProvider>>) -> AppState {
        let mut registry: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
        for provider in providers {
            registry.insert(provider.name().to_string(), provider);
        }
        let config = GatewayConfig {
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["k".into()],
            }),
            ..sag_config::gateway_config_base!()
        };
        crate::testing::test_app_state(
            ConfigHandle::from_config(config),
            Arc::new(RwLock::new(registry)),
        )
        .await
    }

    /// Like [`build_state`] but with a custom circuit-breaker config, used to
    /// drive cooldowns/intervals to ~0 so half-open behavior is exercisable.
    async fn build_state_with_cb(
        providers: Vec<Arc<dyn LlmProvider>>,
        circuit_breaker: CircuitBreakerConfig,
    ) -> AppState {
        let mut registry: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
        for provider in providers {
            registry.insert(provider.name().to_string(), provider);
        }
        let config = GatewayConfig {
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["k".into()],
            }),
            circuit_breaker,
            ..sag_config::gateway_config_base!()
        };
        crate::testing::test_app_state(
            ConfigHandle::from_config(config),
            Arc::new(RwLock::new(registry)),
        )
        .await
    }

    fn make_ctx<'a>(
        state: &'a AppState,
        runtime: &'a DispatchRuntime,
        request: &'a ChatCompletionRequest,
        targets: &'a [ResolvedTarget],
        errors: &'a mut Vec<String>,
    ) -> DispatchCtx<'a> {
        DispatchCtx {
            state,
            runtime,
            request,
            targets,
            api_endpoint: "/images/generations",
            is_stream: false,
            trace_id: Uuid::new_v4(),
            start: Instant::now(),
            route_name: "test-route",
            started_at: chrono::Utc::now(),
            errors,
            strategy: sag_config::Strategy::StickyFailover,
            sticky_token: sag_state::StickyToken::default(),
            user_agent: None,
            client_ip: None,
            ingress_protocol: sag_core::IngressProtocol::ChatCompletions,
            ir: None,
            entry_loss: Default::default(),
            session_key: None,
            session_writeback: None,
        }
    }

    /// Build an [`AppState`] whose formal event collector writes into a
    /// shared in-memory pool the test keeps a handle on — same wiring as
    /// `sag_state::testing::test_app_state`, minus the hidden pool. Draining
    /// the queue via `state.event_collector.shutdown().await` commits all
    /// pending events before the pool is read.
    async fn build_state_with_event_pool(
        providers: Vec<Arc<dyn LlmProvider>>,
    ) -> (AppState, sqlx::SqlitePool) {
        let mut registry: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
        for p in providers {
            registry.insert(p.name().to_string(), p);
        }
        let registry = Arc::new(RwLock::new(registry));
        let config = GatewayConfig {
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["k".into()],
            }),
            ..sag_config::gateway_config_base!()
        };
        let handle = ConfigHandle::from_config(config);
        let pool = sag_analytics::open_analytics_pool(":memory:")
            .await
            .unwrap();
        let state = AppState {
            config: handle.clone(),
            providers: registry.clone(),
            coordinator: Arc::new(sag_state::ConfigMutationCoordinator::new(handle, registry)),
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
            route_counters: Arc::default(),
            session_affinity: Arc::default(),
            in_flight: Arc::default(),
        };
        (state, pool)
    }

    #[tokio::test]
    async fn dispatch_image_success_emits_success_analytics_event_with_null_usage() {
        // Image responses carry no usage: the request fact and the attempt
        // fact must keep token / cache fields NULL (unknown) — never
        // fabricated as 0.
        let mock = Arc::new(MockImageProvider::new("openai"));
        mock.push_image(ImageBehavior::Ok(build_image_response()));
        let (state, pool) = build_state_with_event_pool(vec![mock.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("dall-e-3");
        let targets = vec![target("openai", "dall-e-3", 0)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, &mut errors);
        let trace_id = ctx.trace_id;
        let started_at = ctx.started_at;
        let resp = dispatch_image(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK);
        // Route `api_endpoint` must pass through to the provider call
        // (regression surface for the api_endpoint passthrough).
        assert_eq!(
            mock.last_api_endpoint(),
            Some("/images/generations".to_string()),
        );

        state.event_collector.shutdown().await;

        let (final_status, actual_model, prompt_tokens, cached_tokens, started_ms, completed_ms): (
            String,
            Option<String>,
            Option<i64>,
            Option<i64>,
            i64,
            i64,
        ) = sqlx::query_as(
            "SELECT final_status, actual_model, prompt_tokens, cached_tokens, started_at, \
             completed_at FROM request_results WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("success fact must be persisted");

        assert_eq!(final_status, "success");
        assert_eq!(actual_model.as_deref(), Some("dall-e-3"));
        assert!(
            prompt_tokens.is_none(),
            "image has no usage — tokens must stay NULL, not 0"
        );
        assert!(
            cached_tokens.is_none(),
            "image has no cache info — must stay NULL, not 0"
        );
        assert_eq!(
            started_ms,
            started_at.timestamp_millis(),
            "started_at must come from the caller-supplied route start"
        );
        assert!(completed_ms >= started_ms);

        let (status, t_pt, t_cached): (String, Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT status, prompt_tokens, cached_tokens FROM target_attempts \
             WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("one success attempt must be persisted");
        assert_eq!(status, "success");
        assert!(t_pt.is_none(), "attempt tokens must stay NULL");
        assert!(t_cached.is_none(), "attempt cache must stay NULL");
    }

    #[tokio::test]
    async fn dispatch_image_fallback_emits_failure_and_success_attempts() {
        let m1 = Arc::new(MockImageProvider::new("first"));
        let m2 = Arc::new(MockImageProvider::new("second"));
        m1.push_image(ImageBehavior::Retryable(AppError::Internal("fail".into())));
        m2.push_image(ImageBehavior::Ok(build_image_response()));
        let (state, pool) = build_state_with_event_pool(vec![m1.clone(), m2.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("dall-e-3");
        let targets = vec![
            target("first", "dall-e-3", 0),
            target("second", "dall-e-3", 0),
        ];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, &mut errors);
        let trace_id = ctx.trace_id;
        let resp = dispatch_image(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK);

        state.event_collector.shutdown().await;

        let final_status: String =
            sqlx::query_scalar("SELECT final_status FROM request_results WHERE request_id = ?")
                .bind(trace_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(final_status, "success", "fallback success is route success");

        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT rt.provider, t.status \
             FROM target_attempts t \
             JOIN analytics_route_targets rt ON rt.target_id = t.target_id \
             WHERE t.request_id = ? ORDER BY t.started_at",
        )
        .bind(trace_id.to_string())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "first");
        assert_eq!(rows[0].1, "failure");
        assert_eq!(rows[1].0, "second");
        assert_eq!(rows[1].1, "success");
    }

    #[tokio::test]
    async fn dispatch_image_all_exhausted_emits_model_failure_with_all_attempts() {
        let m1 = Arc::new(MockImageProvider::new("first"));
        let m2 = Arc::new(MockImageProvider::new("second"));
        m1.push_image(ImageBehavior::Retryable(AppError::Internal("fail1".into())));
        m2.push_image(ImageBehavior::Retryable(AppError::Internal("fail2".into())));
        let (state, pool) = build_state_with_event_pool(vec![m1.clone(), m2.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("dall-e-3");
        let targets = vec![
            target("first", "dall-e-3", 0),
            target("second", "dall-e-3", 0),
        ];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, &mut errors);
        let trace_id = ctx.trace_id;
        let resp = dispatch_image(ctx).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

        state.event_collector.shutdown().await;

        let (final_status, failure_reason): (String, Option<String>) = sqlx::query_as(
            "SELECT final_status, failure_reason FROM request_results WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("model_failure fact must be persisted");
        assert_eq!(final_status, "model_failure");
        assert_eq!(failure_reason.as_deref(), Some("all_exhausted"));

        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT rt.provider, t.status \
             FROM target_attempts t \
             JOIN analytics_route_targets rt ON rt.target_id = t.target_id \
             WHERE t.request_id = ? ORDER BY t.started_at",
        )
        .bind(trace_id.to_string())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows.len(),
            2,
            "both actually-issued targets get a failure row"
        );
        assert_eq!(rows[0].0, "first");
        assert_eq!(rows[0].1, "failure");
        assert_eq!(rows[1].0, "second");
        assert_eq!(rows[1].1, "failure");
    }

    /// Major-3 e2e (image path): a client disconnect on a *later* fallback
    /// target must keep the failure fact of an earlier completed target.
    /// `first` fails fast, `second` is cancelled mid-call, so the single
    /// `client_cancelled` fact must carry both attempts in order:
    /// `first` → `failure`, `second` → `cancelled`.
    #[tokio::test]
    async fn dispatch_image_cancel_on_later_target_keeps_prior_failure_facts() {
        let m1 = Arc::new(MockImageProvider::new("first"));
        m1.push_image(ImageBehavior::Retryable(AppError::Internal("boom".into())));
        let m2 = Arc::new(BlockingImageProvider::new("second"));
        let (state, pool) = build_state_with_event_pool(vec![m1.clone(), m2.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("dall-e-3");
        let targets = vec![
            target("first", "dall-e-3", 0),
            target("second", "dall-e-3", 0),
        ];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, &mut errors);
        let trace_id = ctx.trace_id;

        // The second image call never resolves; the timeout drops the
        // handler future mid-call — the client-disconnect signal.
        let result =
            tokio::time::timeout(std::time::Duration::from_millis(100), dispatch_image(ctx)).await;
        assert!(result.is_err(), "dispatch must be cancelled by the timeout");

        state.event_collector.shutdown().await;

        let (final_status, stream): (String, i32) =
            sqlx::query_as("SELECT final_status, stream FROM request_results WHERE request_id = ?")
                .bind(trace_id.to_string())
                .fetch_one(&pool)
                .await
                .expect("client_cancelled fact must be persisted");
        assert_eq!(final_status, "client_cancelled");
        assert_eq!(stream, 0, "image dispatch is non-streaming");

        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT rt.provider, t.status \
             FROM target_attempts t \
             JOIN analytics_route_targets rt ON rt.target_id = t.target_id \
             WHERE t.request_id = ? ORDER BY t.started_at",
        )
        .bind(trace_id.to_string())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows.len(),
            2,
            "image cancel on a later target must keep the earlier completed failure; got {rows:?}"
        );
        assert_eq!(rows[0], ("first".to_string(), "failure".to_string()));
        assert_eq!(rows[1], ("second".to_string(), "cancelled".to_string()));
    }

    #[test]
    fn build_image_request_extracts_prompt_from_last_message() {
        let request = ChatCompletionRequest {
            model: "dall-e-3".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: serde_json::json!("a cat in space"),
                name: None,
                extra: serde_json::Map::new(),
            }],
            temperature: None,
            top_p: None,
            n: Some(2),
            stream: None,
            stop: None,
            presence_penalty: None,
            frequency_penalty: None,
            logit_bias: None,
            max_completion_tokens: None,
            modalities: None,
            audio: None,
            stream_options: None,
            reasoning_effort: None,
            metadata: None,
            parallel_tool_calls: None,
            prediction: None,
            tools: None,
            tool_choice: None,
            response_format: None,
            seed: None,
            logprobs: None,
            top_logprobs: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            safety_identifier: None,
            store: None,
            service_tier: None,
            verbosity: None,
            moderation: None,
            web_search_options: None,
            extra: serde_json::Map::new(),
        };

        let img = build_image_request(&request);
        assert_eq!(img.model, "dall-e-3");
        assert_eq!(img.prompt, "a cat in space");
        assert_eq!(img.n, Some(2));
    }

    #[tokio::test]
    async fn dispatch_image_remembers_exhausted_target_on_next_request() {
        let m1 = Arc::new(MockImageProvider::new("first"));
        let m2 = Arc::new(MockImageProvider::new("second"));
        m1.push_image(ImageBehavior::Retryable(AppError::Internal("fail".into())));
        m2.push_image(ImageBehavior::Ok(build_image_response()));
        m2.push_image(ImageBehavior::Ok(build_image_response()));
        let state = build_state(vec![m1.clone(), m2.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("dall-e-3");
        let targets = vec![
            target("first", "dall-e-3", 0),
            target("second", "dall-e-3", 0),
        ];

        let mut first_errors = Vec::new();
        let first = dispatch_image(make_ctx(&state, &rt, &req, &targets, &mut first_errors)).await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(m1.image_calls(), 1);
        assert_eq!(m2.image_calls(), 1);

        let mut second_errors = Vec::new();
        let second =
            dispatch_image(make_ctx(&state, &rt, &req, &targets, &mut second_errors)).await;
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(m1.image_calls(), 1, "open circuit should skip first target");
        assert_eq!(m2.image_calls(), 2);
        assert!(
            second_errors.iter().any(|e| e.contains("circuit open")),
            "second request should record why the first target was skipped"
        );
    }

    #[tokio::test]
    async fn dispatch_image_half_open_probe_recovers_target() {
        // Mirrors the chat half-open recovery path for image dispatch: `first`
        // fails once (circuit opens), then a probe during cooldown hits the
        // recovered `first` and restores it.
        let m1 = Arc::new(MockImageProvider::new("first"));
        let m2 = Arc::new(MockImageProvider::new("second"));
        m1.push_image(ImageBehavior::Retryable(AppError::Internal("fail".into())));
        m1.push_image(ImageBehavior::Ok(build_image_response()));
        m2.push_image(ImageBehavior::Ok(build_image_response()));
        let cb = CircuitBreakerConfig {
            quota_exhausted_secs: 0,
            rate_limited_secs: 0,
            generic_secs: 0,
            probe_interval_secs: 0,
        };
        let state = build_state_with_cb(vec![m1.clone(), m2.clone()], cb).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("dall-e-3");
        let targets = vec![
            target("first", "dall-e-3", 0),
            target("second", "dall-e-3", 0),
        ];

        let mut first_errors = Vec::new();
        let first = dispatch_image(make_ctx(&state, &rt, &req, &targets, &mut first_errors)).await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(m1.image_calls(), 1);
        assert_eq!(m2.image_calls(), 1);

        let mut second_errors = Vec::new();
        let second =
            dispatch_image(make_ctx(&state, &rt, &req, &targets, &mut second_errors)).await;
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            m1.image_calls(),
            2,
            "half-open probe should hit first target and recover it"
        );
        assert_eq!(m2.image_calls(), 1, "recovered target absorbs traffic");
    }

    #[tokio::test]
    async fn dispatch_image_releases_probe_slot_when_provider_unresolved() {
        // Same regression guard as the chat path: a claimed probe slot must be
        // released when `resolve_provider` returns `None` (provider gone).
        let m2 = Arc::new(MockImageProvider::new("second"));
        m2.push_image(ImageBehavior::Ok(build_image_response()));
        let cb = CircuitBreakerConfig {
            quota_exhausted_secs: 0,
            rate_limited_secs: 0,
            generic_secs: 0,
            probe_interval_secs: 0,
        };
        let state = build_state_with_cb(vec![m2.clone()], cb).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("dall-e-3");
        let targets = vec![
            target("first", "dall-e-3", 0),
            target("second", "dall-e-3", 0),
        ];

        rt.circuit_breaker.write().await.open(
            sag_core::RouteTargetKey::new("dall-e-3", "first", "dall-e-3", ""),
            0,
            0,
        );

        let mut errors = Vec::new();
        let resp = dispatch_image(make_ctx(&state, &rt, &req, &targets, &mut errors)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(m2.image_calls(), 1);

        let mut b = rt.circuit_breaker.write().await;
        assert!(
            b.to_half_open_if_due(&sag_core::RouteTargetKey::new(
                "dall-e-3", "first", "dall-e-3", ""
            )),
            "after release, target should be probeable again"
        );
        assert!(
            b.try_acquire_probe(&sag_core::RouteTargetKey::new(
                "dall-e-3", "first", "dall-e-3", ""
            )),
            "released probe slot should be claimable again"
        );
    }

    #[test]
    fn build_image_request_forwards_known_extra_fields() {
        let mut extra = serde_json::Map::new();
        extra.insert("size".into(), serde_json::json!("1024x1024"));
        extra.insert("quality".into(), serde_json::json!("hd"));
        extra.insert("response_format".into(), serde_json::json!("b64_json"));
        extra.insert("style".into(), serde_json::json!("vivid"));
        extra.insert("unsupported_field".into(), serde_json::json!("ignored"));

        let request = ChatCompletionRequest {
            model: "dall-e-3".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: serde_json::json!("a dog"),
                name: None,
                extra: serde_json::Map::new(),
            }],
            temperature: None,
            top_p: None,
            n: None,
            stream: None,
            stop: None,
            presence_penalty: None,
            frequency_penalty: None,
            logit_bias: None,
            max_completion_tokens: None,
            modalities: None,
            audio: None,
            stream_options: None,
            reasoning_effort: None,
            metadata: None,
            parallel_tool_calls: None,
            prediction: None,
            tools: None,
            tool_choice: None,
            response_format: None,
            seed: None,
            logprobs: None,
            top_logprobs: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            safety_identifier: None,
            store: None,
            service_tier: None,
            verbosity: None,
            moderation: None,
            web_search_options: None,
            extra,
        };

        let img = build_image_request(&request);
        assert_eq!(img.extra.get("size"), Some(&serde_json::json!("1024x1024")));
        assert_eq!(img.extra.get("quality"), Some(&serde_json::json!("hd")));
        assert_eq!(
            img.extra.get("response_format"),
            Some(&serde_json::json!("b64_json"))
        );
        assert_eq!(img.extra.get("style"), Some(&serde_json::json!("vivid")));
        assert!(!img.extra.contains_key("unsupported_field"));
    }

    // ── reasoning_effort round-trip test ─────────────────

    #[tokio::test]
    async fn image_failure_always_writes_reasoning_effort_none() {
        // Even when the client request includes reasoning_effort, the image
        // dispatch path must always write None because image generation has
        // no reasoning concept.
        let m1 = Arc::new(MockImageProvider::new("first"));
        m1.push_image(ImageBehavior::Retryable(AppError::Internal("fail".into())));
        let state = build_state(vec![m1.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request_with_reasoning("dall-e-3");
        let targets = vec![target("first", "dall-e-3", 0)];
        let mut errors = Vec::new();

        let _ = dispatch_image(make_ctx(&state, &rt, &req, &targets, &mut errors)).await;

        state.collector.flush().await;
        let logs = state
            .collector
            .store()
            .query_logs(sag_analytics::LogFilter {
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        // Two logs: per-target failure + fallback_exhausted_response
        assert_eq!(logs.len(), 2);
        for log in &logs {
            assert_eq!(
                log.reasoning_effort, None,
                "image dispatch path must always write reasoning_effort=None, got {:?} for provider={}",
                log.reasoning_effort, log.provider,
            );
        }
    }

    // ── session registry fact ───────────────────────────

    #[tokio::test]
    async fn image_dispatch_writes_session_registry_fact_without_preview() {
        // The image path has no IR, so the registry fact keeps the
        // conversation id but drops the preview. One write per request.
        let m1 = Arc::new(MockImageProvider::new("first"));
        let state = build_state(vec![m1.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request_with_reasoning("dall-e-3");
        let targets = vec![target("first", "dall-e-3", 0)];
        let key = sag_core::SessionKey::new("meta", "conv-image");
        let mut errors = Vec::new();
        let mut ctx = make_ctx(&state, &rt, &req, &targets, &mut errors);
        ctx.session_key = Some(key.clone());

        let _ = dispatch_image(ctx).await;
        state.collector.flush().await;

        let rec = state
            .collector
            .store()
            .get_session(&key.map_id("dall-e-3"))
            .await
            .unwrap()
            .expect("session registry row for the image dispatch");
        assert_eq!(rec.session_source, "meta");
        assert_eq!(rec.conversation_id.as_deref(), Some("conv-image"));
        assert_eq!(rec.first_message_preview, None, "image path has no IR");
        assert_eq!(rec.first_seen, rec.last_seen, "single write per request");
    }
}
