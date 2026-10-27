//! SystemOne (TypeSafe decision API, `POST /v1/systemone`) dispatch with
//! multi-provider fallback.
//!
//! Mirrors [`super::image::dispatch_image`]: the protocol has no chat IR, so
//! the request and response bodies keep **`serde_json::Value`-level semantic
//! fidelity** (`state` / `questions` / `answers` / `id` and the per-question
//! probability fields are never decoded into a typed struct or re-encoded
//! from one). This is semantic, not byte, fidelity: re-serializing a `Value`
//! may normalize key order and numeric literals (`1e3` → `1000.0`). Only
//! `usage` is read out of the response — for analytics, and never fabricated
//! when absent.

use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use std::time::Instant;

use sag_analytics::AnalyticsEvent;
use sag_config::{GatewayConfig, Strategy};
use sag_core::api::extract_usage;
use sag_core::{
    AppError, ChatCompletionRequest, FailureReason, FinalStatus, RequestResultFact,
    TargetAttemptFact, TargetStatus, Usage,
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

/// SystemOne fallback loop. Always non-streaming: the protocol defines no
/// `stream` field, so there is no streaming variant.
///
/// Routing / logging identity comes from the carrier [`ChatCompletionRequest`]
/// (`ctx.request.model`); the wire body is rebuilt from `request.extra` by
/// [`build_systemone_body`].
pub async fn dispatch_systemone(ctx: DispatchCtx<'_>) -> Response<Body> {
    let DispatchCtx {
        state,
        runtime,
        request,
        targets,
        api_endpoint: _,
        is_stream: _,
        trace_id,
        start,
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
        // SystemOne only moves the sticky pin on success (same scope as the
        // image pipeline); session-sticky routes are out of scope here.
        session_writeback: _,
    } = ctx;

    // Per-route in-flight guard: `+1` for this request, released by `Drop`
    // when this dispatch returns (always non-streaming, so the guard lives on
    // this stack frame — every return path releases exactly once).
    let _in_flight = runtime.in_flight.acquire(route_name);

    // Hashed session identity for every `call_logs` row of this dispatch.
    let session_log_key = session_key.as_ref().map(|k| k.map_id(&request.model));

    // Session-registry fact: written exactly once per request. SystemOne
    // carries no IR, so only the preview is dropped.
    if let Some(key) = session_key.as_ref() {
        let fact = sag_core::build_session_fact(key, &request.model, None, started_at);
        state.collector.log_session(fact);
    }

    // Client-disconnect terminal (non-streaming ⇒ the only cancellation
    // detection point). Created first so even a cancel during the config
    // snapshot is covered. Every terminal below calls `mark_done()` first.
    let cancel_guard = DispatchCancelGuard::new(
        state.event_collector.clone(),
        state.collector.clone(),
        trace_id,
        route_name,
        &request.model,
        false,
        sag_core::IngressProtocol::SystemOne,
        started_at,
        start,
        None,
        user_agent.clone(),
        client_ip.clone(),
        session_log_key.clone(),
    );

    // Snapshot the config once for the whole dispatch (backoff, circuit
    // breaker, provider overrides) so the per-target retry loop doesn't hold
    // the config read lock — see `dispatch_chat` for the rationale.
    let config = state.config.read().await.clone();

    let attempt_ctx = SystemOneAttemptCtx {
        state,
        runtime,
        request,
        request_model: &request.model,
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

    run_systemone_fallback(&attempt_ctx, route_name, started_at, targets, errors).await
}

/// Rebuild the caller's SystemOne wire body from the carrier request.
///
/// Contract with the ingress handler: every field of the original
/// `POST /v1/systemone` body (`state` / `questions` / `assets` / …) is kept
/// as-is in `request.extra`; `request.model` is the routing key. Only
/// `model` is rewritten — to the target's `real_model`, exactly like the
/// chat / image paths — so `answers`-affecting fields are never touched
/// here.
///
/// Exemption: the operator-level `body_overrides` / `strip_fields` /
/// `model_overrides` knobs (`sag-providers` `build_request` /
/// `merge_model_overrides`) may still add, replace or remove wire fields
/// downstream. Those are pre-existing opt-in operational mechanisms, not
/// something this function does.
pub fn build_systemone_body(
    request: &ChatCompletionRequest,
    real_model: &str,
) -> serde_json::Value {
    let mut body = serde_json::Map::new();
    for (k, v) in &request.extra {
        body.insert(k.clone(), v.clone());
    }
    body.insert(
        "model".to_string(),
        serde_json::Value::String(real_model.to_string()),
    );
    serde_json::Value::Object(body)
}

/// Map a SystemOne response body's `usage` onto the analytics [`Usage`].
///
/// Missing or malformed counters yield `None` — the analytics columns stay
/// NULL (unknown), never a fabricated `0` (see
/// [`sag_core::api::extract_usage`]). Counters that do not fit [`u32`] are
/// malformed for this purpose: the whole reading becomes `None` instead of
/// being clamped to `u32::MAX`, which would silently invent a number.
fn usage_from_body(body: &serde_json::Value) -> Option<Usage> {
    let (input, output) = extract_usage(body)?;
    let prompt_tokens = u32::try_from(input).ok()?;
    let completion_tokens = u32::try_from(output).ok()?;
    Some(Usage {
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens.saturating_add(completion_tokens),
        cached_tokens: None,
        cache_creation_tokens: None,
        reasoning_tokens: None,
    })
}

/// Bundled dispatch-level parameters for one target attempt, so
/// [`attempt_one_systemone_target`] keeps a short signature.
struct SystemOneAttemptCtx<'a> {
    state: &'a AppState,
    runtime: &'a DispatchRuntime,
    request: &'a ChatCompletionRequest,
    request_model: &'a str,
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

/// Verdict of one target attempt in the SystemOne fallback loop.
enum TargetOutcome {
    /// The provider produced a SystemOne response — terminal success.
    Success(serde_json::Value),
    /// Client-attributed 4xx — terminal, failover forbidden.
    ClientError(AppError),
    /// Continue with the next target (circuit open / provider disabled /
    /// provider-attributable failure).
    Failover,
}

/// Drive the fallback loop to a terminal response: try each target via
/// [`attempt_one_systemone_target`], returning on the first success / client
/// error, or the all-exhausted 502 when every target fell through.
async fn run_systemone_fallback(
    attempt_ctx: &SystemOneAttemptCtx<'_>,
    route_name: &str,
    started_at: chrono::DateTime<chrono::Utc>,
    targets: &[ResolvedTarget],
    errors: &mut Vec<String>,
) -> Response<Body> {
    let mut target_facts: Vec<TargetAttemptFact> = Vec::new();

    for target in targets {
        match attempt_one_systemone_target(attempt_ctx, target, errors, &mut target_facts).await {
            TargetOutcome::Success(body) => {
                return finish_systemone_success(
                    attempt_ctx,
                    route_name,
                    started_at,
                    target,
                    target_facts,
                    body,
                )
                .await;
            }
            TargetOutcome::ClientError(err) => {
                return finish_systemone_client_error(attempt_ctx, started_at, err).await;
            }
            TargetOutcome::Failover => continue,
        }
    }

    finish_systemone_exhausted(attempt_ctx, route_name, started_at, errors, target_facts).await
}

/// Run one target: circuit gate → provider resolve → retries → single
/// attempt fact → circuit outcome → sticky writeback on success. Skipped
/// targets (circuit open / provider disabled) produce no fact.
async fn attempt_one_systemone_target(
    ctx: &SystemOneAttemptCtx<'_>,
    target: &ResolvedTarget,
    errors: &mut Vec<String>,
    target_facts: &mut Vec<TargetAttemptFact>,
) -> TargetOutcome {
    let provider_name: &str = &target.provider;
    let real_model = target.real_model.clone();

    // The circuit key folds in the upstream base address of the actually
    // used protocol (`apis.systemone` on a native lane).
    let circuit_url = target_base_url(
        &ctx.config.providers,
        provider_name,
        target.upstream_protocol,
    );
    let Some(permit) = circuit_allows_target(
        ctx.runtime,
        ctx.request_model,
        provider_name,
        &real_model,
        &circuit_url,
        &ctx.config.circuit_breaker,
    )
    .await
    else {
        errors.push(format!("{provider_name}/{real_model}: circuit open"));
        return TargetOutcome::Failover;
    };

    let Some(provider) = resolve_provider(ctx.state, provider_name, errors).await else {
        // Provider disabled / unregistered: release a claimed half-open probe
        // slot synchronously so the target cannot be skipped forever.
        if let CircuitPermit::Probe(guard) = permit {
            guard.release_probe_slot().await;
        }
        return TargetOutcome::Failover;
    };

    let target_extra = merge_model_overrides(
        &ctx.config.providers,
        provider_name,
        &real_model,
        &target.extra,
    );
    let body = build_systemone_body(ctx.request, &real_model);
    let retries = target.retries;
    let timeout_secs = target.timeout_secs;
    let upstream_protocol = target.upstream_protocol.as_str();
    let lane = target.lane.as_str();

    // Wall-clock start of this target's first actual provider call; retries
    // of the same target extend the single fact's duration.
    let mut target_started_at: Option<chrono::DateTime<chrono::Utc>> = None;

    let outcome = run_with_retries_outcome::<serde_json::Value, _, _>(
        provider_name,
        retries,
        &ctx.config.backoff,
        errors,
        |attempt| {
            if attempt == 0 {
                target_started_at = Some(chrono::Utc::now());
            }
            let provider: Arc<dyn LlmProvider> = provider.clone();
            let real_model = real_model.clone();
            let target_extra = target_extra.clone();
            let body = body.clone();
            let cancel = ctx.cancel_guard.analytics();
            async move {
                // An actual provider call is in flight from here until the
                // attempt's future resolves; a client disconnect in this
                // window is persisted as a `cancelled` attempt.
                cancel.begin_attempt(
                    provider_name,
                    &real_model,
                    Some(upstream_protocol),
                    Some(lane),
                );
                let result = provider.systemone(body, &target_extra, timeout_secs).await;
                cancel.end_attempt();
                result
            }
        },
    )
    .await;

    // Per-target CallLog fields, extracted before the outcome is consumed.
    let (category, attempt_status) = failure_log_fields(&outcome);
    let is_client_error_path =
        matches!(&outcome, RetryOutcome::NonRetryable(e) if is_client_caused_4xx(e));
    // Per the writer's validation contract a `client_error` event can carry no
    // target attempts, so that attempt is recorded only in call_logs.
    if !is_client_error_path
        && let Some(fact) = build_target_attempt_fact(
            &target.provider,
            &real_model,
            &outcome,
            target_started_at,
            Some(upstream_protocol),
            Some(lane),
        )
    {
        // Mirror the fact into the cancel guard before continuing to the next
        // target, so a later client disconnect still carries it. The normal
        // terminals `mark_done` first, so the guard's copy is dropped unread.
        ctx.cancel_guard.record_target_fact(fact.clone());
        target_facts.push(fact);
    }

    // Client-attributed 4xx → terminal; peel the error out by value and hand
    // the breaker a placeholder `NonRetryable` (its payload is ignored).
    let (client_error, cb_outcome) = match outcome {
        RetryOutcome::NonRetryable(e) if is_client_caused_4xx(&e) => (
            Some(e),
            RetryOutcome::NonRetryable(AppError::Internal("client 4xx".into())),
        ),
        other => (None, other),
    };

    let Some(body) =
        apply_circuit_outcome(ctx.runtime, permit, cb_outcome, &ctx.config.circuit_breaker).await
    else {
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
            ctx.user_agent,
            ctx.client_ip,
            Some(sag_core::IngressProtocol::SystemOne.as_str().to_string()),
            Some(upstream_protocol.to_string()),
            Some(lane.to_string()),
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

    TargetOutcome::Success(body)
}

/// Failure-log fields extracted from the terminal retry outcome: the error
/// category and the real upstream status. `Success` sentinels are never
/// logged.
fn failure_log_fields(outcome: &RetryOutcome<serde_json::Value>) -> (&'static str, u16) {
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

/// Build the single [`TargetAttemptFact`] for an actually-issued target from
/// its final retry outcome. Token / cache fields come from the response
/// `usage` and stay NULL when the upstream omits it (never a fabricated 0).
fn build_target_attempt_fact(
    provider: &str,
    real_model: &str,
    outcome: &RetryOutcome<serde_json::Value>,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    upstream_protocol: Option<&str>,
    lane: Option<&str>,
) -> Option<TargetAttemptFact> {
    let completed_at = chrono::Utc::now();
    // `started_at` is captured at the first actual provider call; fall back to
    // `completed_at` only if the closure never ran (cannot happen in
    // practice — the retry loop always issues attempt 0).
    let started_at = started_at.unwrap_or(completed_at);
    let upstream_protocol = upstream_protocol.map(str::to_string);
    let lane = lane.map(str::to_string);
    match outcome {
        RetryOutcome::Success(body) => {
            let usage = usage_from_body(body);
            Some(TargetAttemptFact {
                provider: provider.to_string(),
                real_model: real_model.to_string(),
                status: TargetStatus::Success,
                failure_reason: None,
                started_at,
                completed_at,
                // `None` when the upstream reported no `usage` (NULL column).
                cached_tokens: usage.as_ref().and_then(|u| u.cached_tokens),
                usage,
                ttft_ms: None,
                upstream_duration_ms: None,
                upstream_protocol,
                lane,
            })
        }
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

/// Sticky-failover writeback for a successful SystemOne target. Same
/// conditional version/generation guard as the chat / image paths (see
/// `dispatch_chat` for the race this closes).
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

/// Success terminal: per-target success log, then claim the cancel-guard
/// terminal and emit the `success` request event. The response body is
/// returned as-is (`serde_json::Value`-level semantic fidelity; key order and
/// numeric literals may be normalized).
async fn finish_systemone_success(
    ctx: &SystemOneAttemptCtx<'_>,
    route_name: &str,
    started_at: chrono::DateTime<chrono::Utc>,
    target: &ResolvedTarget,
    target_facts: Vec<TargetAttemptFact>,
    body: serde_json::Value,
) -> Response<Body> {
    let usage = usage_from_body(&body);
    ctx.state.collector.log(success_log(
        ctx.trace_id,
        &target.provider,
        ctx.request_model,
        &target.real_model,
        false,
        ctx.start,
        usage.as_ref(),
        // reasoning_effort — not applicable for SystemOne
        None,
        // upstream_duration_ms / ttft_ms — single-shot non-streaming call
        None,
        None,
        ctx.user_agent.map(str::to_string),
        ctx.client_ip.map(str::to_string),
        Some(sag_core::IngressProtocol::SystemOne.as_str().to_string()),
        Some(target.upstream_protocol.as_str().to_string()),
        Some(target.lane.as_str().to_string()),
        None,
        ctx.session_key.clone(),
    ));
    ctx.cancel_guard.mark_done();
    emit_analytics_event(
        ctx.state,
        AnalyticsEvent {
            result: RequestResultFact {
                request_id: ctx.trace_id,
                route_name: Some(route_name.to_string()),
                requested_model: ctx.request_model.to_string(),
                final_status: FinalStatus::Success,
                failure_reason: None,
                stream: false,
                started_at,
                completed_at: chrono::Utc::now(),
                // `None` when the upstream omitted `usage` (NULL, never 0).
                usage,
                cached_tokens: None,
                cache_creation_tokens: None,
                ttft_ms: None,
                upstream_duration_ms: None,
                ingress_protocol: Some(sag_core::IngressProtocol::SystemOne.as_str().to_string()),
                lane: Some(target.lane.as_str().to_string()),
            },
            targets: target_facts,
        },
    )
    .await;
    (StatusCode::OK, Json(body)).into_response()
}

/// Client-error terminal: emit the `client_error` request event (no route
/// identity, no target attempts), then hand the error straight back to the
/// client — no failover, no pin movement, no circuit change.
async fn finish_systemone_client_error(
    ctx: &SystemOneAttemptCtx<'_>,
    started_at: chrono::DateTime<chrono::Utc>,
    err: AppError,
) -> Response<Body> {
    ctx.cancel_guard.mark_done();
    emit_analytics_event(
        ctx.state,
        AnalyticsEvent::new(RequestResultFact {
            request_id: ctx.trace_id,
            route_name: None,
            requested_model: ctx.request_model.to_string(),
            final_status: FinalStatus::ClientError,
            failure_reason: Some(client_error_failure_reason(&err)),
            stream: false,
            started_at,
            completed_at: chrono::Utc::now(),
            usage: None,
            cached_tokens: None,
            cache_creation_tokens: None,
            ttft_ms: None,
            upstream_duration_ms: None,
            ingress_protocol: Some(sag_core::IngressProtocol::SystemOne.as_str().to_string()),
            lane: None,
        }),
    )
    .await;
    err.into_response()
}

/// All-exhausted terminal: build the 502 fallback response (which also logs
/// the `__all_exhausted__` call row), then claim the cancel-guard terminal and
/// emit the `model_failure` request event with one failure fact per
/// actually-issued target.
async fn finish_systemone_exhausted(
    ctx: &SystemOneAttemptCtx<'_>,
    route_name: &str,
    started_at: chrono::DateTime<chrono::Utc>,
    errors: &[String],
    target_facts: Vec<TargetAttemptFact>,
) -> Response<Body> {
    let response = fallback_exhausted_response(
        ctx.state,
        ctx.request_model,
        false,
        ctx.trace_id,
        ctx.start,
        errors,
        None,
        ctx.user_agent,
        ctx.client_ip,
        Some(sag_core::IngressProtocol::SystemOne.as_str().to_string()),
        None,
        None,
        None,
        ctx.session_key.clone(),
    );
    ctx.cancel_guard.mark_done();
    emit_analytics_event(
        ctx.state,
        AnalyticsEvent {
            result: RequestResultFact {
                request_id: ctx.trace_id,
                route_name: Some(route_name.to_string()),
                requested_model: ctx.request_model.to_string(),
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
                ingress_protocol: Some(sag_core::IngressProtocol::SystemOne.as_str().to_string()),
                lane: target_facts.first().and_then(|f| f.lane.clone()),
            },
            targets: target_facts,
        },
    )
    .await;
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::ResolvedTarget;
    use crate::dispatch::tests::build_state_with_event_pool;
    use crate::state::{AppState, DispatchRuntime};
    use crate::testing::target;
    use async_trait::async_trait;
    use futures_util::Stream;
    use serde_json::json;
    use std::collections::{HashMap, VecDeque};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    enum SystemOneBehavior {
        Ok(serde_json::Value),
        Retryable(AppError),
    }

    struct MockSystemOneProvider {
        name: String,
        queue: Mutex<VecDeque<SystemOneBehavior>>,
        calls: AtomicU32,
    }

    impl MockSystemOneProvider {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                queue: Mutex::new(VecDeque::new()),
                calls: AtomicU32::new(0),
            }
        }

        fn push(&self, behavior: SystemOneBehavior) {
            self.queue.lock().unwrap().push_back(behavior);
        }

        fn calls(&self) -> u32 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LlmProvider for MockSystemOneProvider {
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
        ) -> sag_core::Result<sag_core::ChatCompletionResponse> {
            unreachable!("systemone dispatch should not call chat")
        }

        async fn chat_stream(
            &self,
            _request: &ChatCompletionRequest,
            _real_model: &str,
            _api_endpoint: &str,
            _target_extra: &HashMap<String, serde_json::Value>,
            _timeout_secs: u64,
        ) -> sag_core::Result<Pin<Box<dyn Stream<Item = sag_core::StreamChunk> + Send>>> {
            unreachable!("systemone dispatch should not call chat_stream")
        }

        async fn systemone(
            &self,
            _body: serde_json::Value,
            _target_extra: &HashMap<String, serde_json::Value>,
            _timeout_secs: u64,
        ) -> sag_core::Result<serde_json::Value> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match self.queue.lock().unwrap().pop_front() {
                Some(SystemOneBehavior::Ok(body)) => Ok(body),
                Some(SystemOneBehavior::Retryable(err)) => Err(err),
                None => Ok(json!({"model": "m", "answers": {}})),
            }
        }
    }

    /// Carrier request: `model` is the routing key, every other field of the
    /// original SystemOne body rides in `extra`.
    fn build_request(model: &str) -> ChatCompletionRequest {
        serde_json::from_value(json!({
            "model": model,
            "messages": [],
            "state": {"user": "u-1"},
            "questions": {"sentiment": {"type": "choice", "criteria": ["pos", "neg"]}},
        }))
        .expect("valid systemone carrier request")
    }

    /// Resolved target on the SystemOne native lane (`apis.systemone`).
    fn systemone_target(provider: &str, real_model: &str, retries: u32) -> ResolvedTarget {
        ResolvedTarget {
            upstream_protocol: sag_core::ApiKind::SystemOne,
            lane: sag_core::Lane::Native,
            ..target(provider, real_model, retries)
        }
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
            api_endpoint: "",
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
            ingress_protocol: sag_core::IngressProtocol::SystemOne,
            ir: None,
            entry_loss: Default::default(),
            session_key: None,
            session_writeback: None,
        }
    }

    #[tokio::test]
    async fn dispatch_systemone_success_passes_body_through_and_keeps_usage_null() {
        // Response carries no `usage`: every token column must stay NULL
        // (unknown), never a fabricated 0.
        let body = json!({
            "id": "r1",
            "model": "systemone-v1",
            "answers": {"sentiment": {"type": "choice", "choice": "pos", "confidence": 0.9}},
        });
        let mock = Arc::new(MockSystemOneProvider::new("openai"));
        mock.push(SystemOneBehavior::Ok(body.clone()));
        let (state, pool) = build_state_with_event_pool(vec![mock.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("systemone-v1");
        let targets = vec![systemone_target("openai", "systemone-v1", 0)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, &mut errors);
        let trace_id = ctx.trace_id;

        let resp = dispatch_systemone(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK);
        // Verbatim passthrough: answers / id / probability fields untouched.
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let out: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out, body);

        state.event_collector.shutdown().await;

        let (final_status, prompt_tokens, completion_tokens): (String, Option<i64>, Option<i64>) =
            sqlx::query_as(
                "SELECT final_status, prompt_tokens, completion_tokens FROM request_results \
                 WHERE request_id = ?",
            )
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("success fact must be persisted");
        assert_eq!(final_status, "success");
        assert!(
            prompt_tokens.is_none() && completion_tokens.is_none(),
            "missing usage must stay NULL, not 0"
        );

        let (status, t_pt): (String, Option<i64>) = sqlx::query_as(
            "SELECT status, prompt_tokens FROM target_attempts WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("one success attempt must be persisted");
        assert_eq!(status, "success");
        assert!(t_pt.is_none(), "attempt tokens must stay NULL");
    }

    #[tokio::test]
    async fn dispatch_systemone_failure_falls_back_to_next_target() {
        let m1 = Arc::new(MockSystemOneProvider::new("first"));
        let m2 = Arc::new(MockSystemOneProvider::new("second"));
        m1.push(SystemOneBehavior::Retryable(AppError::Internal(
            "fail".into(),
        )));
        m2.push(SystemOneBehavior::Ok(json!({"model": "m", "answers": {}})));
        let (state, pool) = build_state_with_event_pool(vec![m1.clone(), m2.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("systemone-v1");
        let targets = vec![
            systemone_target("first", "systemone-v1", 0),
            systemone_target("second", "systemone-v1", 0),
        ];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, &mut errors);
        let trace_id = ctx.trace_id;

        let resp = dispatch_systemone(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(m1.calls(), 1);
        assert_eq!(m2.calls(), 1);

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
        assert_eq!(rows[0], ("first".to_string(), "failure".to_string()));
        assert_eq!(rows[1], ("second".to_string(), "success".to_string()));
    }

    #[tokio::test]
    async fn dispatch_systemone_all_exhausted_returns_502_model_failure() {
        let m1 = Arc::new(MockSystemOneProvider::new("first"));
        let m2 = Arc::new(MockSystemOneProvider::new("second"));
        m1.push(SystemOneBehavior::Retryable(AppError::Internal(
            "fail1".into(),
        )));
        m2.push(SystemOneBehavior::Retryable(AppError::Internal(
            "fail2".into(),
        )));
        let (state, pool) = build_state_with_event_pool(vec![m1.clone(), m2.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("systemone-v1");
        let targets = vec![
            systemone_target("first", "systemone-v1", 0),
            systemone_target("second", "systemone-v1", 0),
        ];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, &mut errors);
        let trace_id = ctx.trace_id;

        let resp = dispatch_systemone(ctx).await;
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
        assert_eq!(rows.len(), 2, "both issued targets get a failure row");
        assert_eq!(rows[0], ("first".to_string(), "failure".to_string()));
        assert_eq!(rows[1], ("second".to_string(), "failure".to_string()));
    }

    #[tokio::test]
    async fn dispatch_systemone_skips_open_circuit_target() {
        let m1 = Arc::new(MockSystemOneProvider::new("first"));
        let m2 = Arc::new(MockSystemOneProvider::new("second"));
        m1.push(SystemOneBehavior::Retryable(AppError::Internal(
            "fail".into(),
        )));
        m2.push(SystemOneBehavior::Ok(json!({"model": "m", "answers": {}})));
        let state = build_state_with_event_pool(vec![m1.clone(), m2.clone()])
            .await
            .0;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("systemone-v1");
        let targets = vec![
            systemone_target("first", "systemone-v1", 0),
            systemone_target("second", "systemone-v1", 0),
        ];

        let mut first_errors = Vec::new();
        let first =
            dispatch_systemone(make_ctx(&state, &rt, &req, &targets, &mut first_errors)).await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(m1.calls(), 1);

        let mut second_errors = Vec::new();
        let second =
            dispatch_systemone(make_ctx(&state, &rt, &req, &targets, &mut second_errors)).await;
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(m1.calls(), 1, "open circuit should skip first target");
        assert!(
            second_errors.iter().any(|e| e.contains("circuit open")),
            "second request should record why the first target was skipped"
        );
    }

    #[tokio::test]
    async fn dispatch_systemone_releases_probe_slot_when_provider_unresolved() {
        // Regression (mirrors `dispatch_chat_releases_probe_slot_when_provider_unresolved`):
        // when `circuit_allows_target` claims the half-open probe slot but
        // `resolve_provider` returns `None` (provider disabled or no longer
        // registered, e.g. after a config hot-reload), the slot must be
        // released. Otherwise `probe_in_flight` stays true and the target's
        // half-open recovery is defeated for the rest of the cooldown window.
        let m2 = Arc::new(MockSystemOneProvider::new("second"));
        m2.push(SystemOneBehavior::Ok(json!({"model": "m", "answers": {}})));
        // Only `second` is registered; `first` is referenced by the route but
        // gone from the registry, so `resolve_provider("first")` returns None.
        let state = build_state_with_event_pool(vec![m2.clone()]).await.0;
        // Zero probe interval so the released slot is immediately probeable
        // again (otherwise the assertion below would have to wait out the
        // default cooldown before the target is due for a new probe).
        state
            .config
            .write()
            .await
            .circuit_breaker
            .probe_interval_secs = 0;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("systemone-v1");
        let targets = vec![
            systemone_target("first", "systemone-v1", 0),
            systemone_target("second", "systemone-v1", 0),
        ];

        // Pre-open `first`'s circuit (zero cooldown ⇒ probe immediately due)
        // so the request claims the probe slot before `resolve_provider`
        // fails. The circuit key folds in the upstream base URL, which is
        // empty for a provider absent from the config.
        let key = sag_core::RouteTargetKey::new("systemone-v1", "first", "systemone-v1", "");
        rt.circuit_breaker.write().await.open(key.clone(), 0, 0);

        let mut errors = Vec::new();
        let resp = dispatch_systemone(make_ctx(&state, &rt, &req, &targets, &mut errors)).await;
        assert_eq!(resp.status(), StatusCode::OK, "falls through to second");
        assert_eq!(m2.calls(), 1);
        assert!(
            errors.iter().any(|e| e.contains("first: not found")),
            "the unresolved provider should be recorded: {errors:?}"
        );

        // The slot must have been released: the target is probeable again and
        // the single probe slot can be claimed once more.
        let mut b = rt.circuit_breaker.write().await;
        assert!(
            b.to_half_open_if_due(&key),
            "after release, target should be probeable again (not stuck half-open)"
        );
        assert!(
            b.try_acquire_probe(&key),
            "released probe slot should be claimable again"
        );
    }

    #[test]
    fn build_systemone_body_keeps_extra_verbatim_and_rewrites_model() {
        let req = build_request("route-model");
        let body = build_systemone_body(&req, "real-model");
        assert_eq!(body["model"], json!("real-model"));
        assert_eq!(body["state"], json!({"user": "u-1"}));
        assert_eq!(
            body["questions"]["sentiment"]["type"],
            json!("choice"),
            "question payload must pass through untouched"
        );
    }

    #[test]
    fn usage_from_body_extracts_counters_and_rejects_missing() {
        assert_eq!(
            usage_from_body(&json!({"usage": {"input_tokens": 7, "output_tokens": 3}})).map(|u| (
                u.prompt_tokens,
                u.completion_tokens,
                u.total_tokens
            )),
            Some((7, 3, 10))
        );
        assert!(
            usage_from_body(&json!({"answers": {}})).is_none(),
            "absent usage must stay NULL"
        );
        // Counters that do not fit u32 are malformed for analytics: the whole
        // reading is dropped (NULL) instead of being clamped to u32::MAX,
        // which would fabricate a number.
        assert!(
            usage_from_body(&json!({
                "usage": {"input_tokens": u32::MAX as u64 + 1, "output_tokens": 3}
            }))
            .is_none(),
            "u32 overflow on input_tokens must yield None, not u32::MAX"
        );
        assert!(
            usage_from_body(&json!({
                "usage": {"input_tokens": 3, "output_tokens": u64::MAX}
            }))
            .is_none(),
            "u32 overflow on output_tokens must yield None, not u32::MAX"
        );
    }
}
