//! Chat-completion dispatch with multi-provider fallback.

use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use sag_analytics::AnalyticsEvent;
use sag_core::{AppError, FailureReason, FinalStatus, RequestResultFact, TargetAttemptFact};

use crate::dispatch::resolve::resolve_provider;
use crate::dispatch::retry::RetryOutcome;
use crate::dispatch::retry::is_client_caused_4xx;
use crate::dispatch::retry::run_with_retries_outcome;
use crate::dispatch::stream::build_streaming_response;
use crate::dispatch::{
    CircuitPermit, DispatchCancelGuard, DispatchCtx, apply_circuit_outcome, circuit_allows_target,
    client_error_failure_reason, emit_analytics_event, fallback_exhausted_response,
    log_per_target_failure, merge_model_overrides, success_log,
};

mod attempt;
mod empty;

#[cfg(test)]
mod tests;

pub(crate) use attempt::ChatAttempt;
pub(crate) use attempt::build_target_attempt_fact;
pub(crate) use empty::{is_empty_non_stream_response, validate_stream_first_chunk};

/// Try each target in order. On a retryable error, retry the same target with
/// exponential backoff. On a non-retryable 4xx error, do **not** retry the
/// same target and do **not** open the circuit breaker:
/// - client-caused 4xx (400/413/422/unknown) return straight to the client,
///   no failover and no shared-state mutation;
/// - provider-persistent 4xx (401/404/non-quota 403) failover to the next
///   target, which may move the sticky pin on success.
pub async fn dispatch_chat(ctx: DispatchCtx<'_>) -> Response<Body> {
    let DispatchCtx {
        state,
        runtime,
        request,
        targets,
        api_endpoint,
        is_stream,
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
    } = ctx;

    // Client-disconnect terminal: if the handler future is dropped before
    // any normal terminal (non-streaming request, or a streaming request
    // cancelled before `build_streaming_response` commits), this guard
    // emits the `client_cancelled` fact — with a `cancelled` attempt if a
    // provider call was in flight, and 0 attempts otherwise (spec scenario
    // 11). Created first so even a cancel during the config snapshot below
    // is covered. Every terminal calls `mark_done()` before emitting its
    // own event.
    let cancel_guard = DispatchCancelGuard::new(
        state.event_collector.clone(),
        trace_id,
        route_name,
        &request.model,
        is_stream,
        started_at,
    );

    // Snapshot the backoff config once so the per-target retry loop doesn't
    // hold the config read lock and so all targets within one request share
    // a consistent view (a hot-reload mid-dispatch shouldn't change the
    // schedule for an in-flight request).
    let config = state.config.read().await.clone();
    let backoff = config.backoff.clone();
    let retry_on_empty = backoff.retry_on_empty_response;
    let circuit_breaker_cfg = config.circuit_breaker.clone();
    let api_endpoint = api_endpoint.to_string();

    // One `TargetAttemptFact` per actually-issued target, emitted once in a
    // single atomic request event at the end of the dispatch (non-stream
    // slice only). Skipped targets (circuit open / provider disabled) never
    // reach the provider and produce no fact.
    let mut target_facts: Vec<TargetAttemptFact> = Vec::new();

    for target in targets {
        let provider_name = &target.provider;
        let real_model = &target.real_model;

        let Some(permit) = circuit_allows_target(
            runtime,
            &request.model,
            provider_name,
            real_model,
            &circuit_breaker_cfg,
        )
        .await
        else {
            errors.push(format!("{provider_name}/{real_model}: circuit open"));
            continue;
        };

        let Some(provider) = resolve_provider(state, provider_name, errors).await else {
            // Provider is disabled / unregistered. If this request claimed the
            // half-open probe slot we must release it synchronously, otherwise
            // the target would be skipped forever (probe_in_flight stuck
            // true). `release_probe_slot` consumes the guard (mark_completed
            // first), so the drop after this branch can never release twice.
            if let CircuitPermit::Probe(guard) = permit {
                guard.release_probe_slot().await;
            }
            continue;
        };

        // Clone per-target values once so the retry closure can cheaply clone
        // them for each attempt.
        let real_model = real_model.clone();
        let target_extra =
            merge_model_overrides(&config.providers, provider_name, &real_model, &target.extra);
        let retries = target.retries;
        let timeout_secs = target.timeout_secs;

        // Wall-clock start of this target's first actual provider call,
        // captured inside the retry closure on attempt 0. Retries of the
        // same target extend the single fact's duration; the legacy
        // `upstream_start` `Instant` remains untouched.
        let mut target_started_at: Option<chrono::DateTime<chrono::Utc>> = None;

        let outcome = run_with_retries_outcome::<ChatAttempt, _, _>(
            provider_name,
            retries,
            &backoff,
            errors,
            |attempt| {
                if attempt == 0 {
                    target_started_at = Some(chrono::Utc::now());
                }
                let provider = provider.clone();
                let real_model = real_model.clone();
                let target_extra = target_extra.clone();
                let api_endpoint = api_endpoint.clone();
                let provider_name = provider_name.to_string();
                let cancel = cancel_guard.analytics();
                async move {
                    // An actual provider call is in flight from here until
                    // the attempt's future resolves. A client disconnect
                    // during this window drops the future with the marker
                    // still set, so the dispatch cancel guard persists the
                    // attempt as `cancelled` (spec: 在途调用 → cancelled).
                    // Cancels during backoff sleeps or between targets have
                    // no marker → 0 target attempts (spec: 退避/fallback
                    // 间隙 → 0 条).
                    cancel.begin_attempt(&provider_name, &real_model);
                    let result = async {
                        // Capture the upstream start time immediately before the
                        // provider call so retry/circuit overhead is excluded.
                        let upstream_start = std::time::Instant::now();
                        if is_stream {
                            let s = provider
                                .chat_stream(
                                    request,
                                    &real_model,
                                    &api_endpoint,
                                    &target_extra,
                                    timeout_secs,
                                )
                                .await?;
                            if retry_on_empty {
                                // Peek at the first chunk before committing the HTTP
                                // 200 response.  If the upstream returned an empty
                                // response (ct=0, no content), surface an
                                // `EmptyResponse` error so the retry loop retries the
                                // same target with backoff.
                                let s = validate_stream_first_chunk(Box::pin(s), &provider_name)
                                    .await?;
                                Ok(ChatAttempt::Stream(s, upstream_start))
                            } else {
                                Ok(ChatAttempt::Stream(Box::pin(s), upstream_start))
                            }
                        } else {
                            let r = provider
                                .chat(
                                    request,
                                    &real_model,
                                    &api_endpoint,
                                    &target_extra,
                                    timeout_secs,
                                )
                                .await?;
                            if retry_on_empty && is_empty_non_stream_response(&r) {
                                return Err(AppError::EmptyResponse {
                                    provider: provider_name.clone(),
                                });
                            }
                            Ok(ChatAttempt::NonStream(r, upstream_start))
                        }
                    }
                    .await;
                    cancel.end_attempt();
                    result
                }
            },
        )
        .await;

        // Extract the error_category from the outcome before
        // `apply_circuit_outcome` consumes it, so per-target
        // failure logs carry the real failure type instead of a
        // generic "provider_error". Non-retryable errors use the real
        // `error_category()` too: a client-caused `BadRequest` maps to
        // "bad_request" exactly as before, while provider-persistent 4xx
        // (401/403/404 upstream) correctly classify as "provider_error"
        // instead of the historical hardcoded "bad_request".
        let category = match &outcome {
            RetryOutcome::Exhausted(e) => e.error_category(),
            RetryOutcome::NonRetryable(e) => e.error_category(),
            RetryOutcome::Success(_) => "", // never logged — Success never reaches the else block
        };

        // Extract the real HTTP status from the outcome error so the
        // per-target `call_logs` row carries the actual upstream status
        // (429/500/503/…) instead of a hardcoded 502.
        let attempt_status = match &outcome {
            RetryOutcome::Exhausted(e) | RetryOutcome::NonRetryable(e) => e.status_code().as_u16(),
            RetryOutcome::Success(_) => 200, // never logged — Success never reaches the else block
        };

        // Collapse this target's whole retry sequence into exactly one
        // `TargetAttemptFact` (final outcome only — no retry counts). For
        // streaming requests only pre-commit failures land here: a stream
        // success fact is built at terminal time inside
        // `build_streaming_response`, which also receives the failure
        // facts accumulated so far (retry success collapses into the single
        // success fact either way). The client-caused 4xx path is excluded:
        // per the writer's validation contract a `client_error` event can
        // carry no target attempts, so that attempt is recorded only in
        // call_logs.
        let is_client_error_path =
            matches!(&outcome, RetryOutcome::NonRetryable(e) if is_client_caused_4xx(e));
        if !is_client_error_path
            && let Some(fact) = build_target_attempt_fact(
                provider_name,
                &real_model,
                &outcome,
                target_started_at,
                chrono::Utc::now(),
            )
        {
            target_facts.push(fact);
        }

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

        let Some(attempt) =
            apply_circuit_outcome(runtime, permit, cb_outcome, &circuit_breaker_cfg).await
        else {
            // Per-provider failure: log a failure entry with the actual provider/real_model
            // so the analytics table knows which provider was attempted.
            log_per_target_failure(
                state,
                trace_id,
                provider_name,
                &request.model,
                &real_model,
                is_stream,
                start,
                errors,
                category,
                attempt_status,
                request.reasoning_effort.as_deref(),
                user_agent.as_deref(),
                client_ip.as_deref(),
            );
            if let Some(err) = client_error {
                // Client-attributed 4xx: return to the client directly — no
                // failover to the next target, no pin movement, no circuit
                // change. `into_response` transparently forwards the upstream
                // status code + body.
                // Formal analytics event: the request failed with a
                // client-caused 4xx → `client_error`. Per the writer's
                // validation contract a `client_error` event carries no
                // route identity (`route_id` NULL) and no target
                // attempts — the attempt above is still visible in the
                // legacy call_logs row. The reason maps per the spec
                // table (`invalid_request` / `payload_too_large`).
                // Emitted for **both** streaming and non-streaming
                // requests: a streaming request whose first provider call
                // returns a client-caused 4xx never reaches
                // `build_streaming_response` (no bytes were committed), so
                // this is the only terminal it gets — skipping it would
                // violate the "one request, one fact" invariant.
                cancel_guard.mark_done();
                emit_analytics_event(
                    state,
                    AnalyticsEvent::new(RequestResultFact {
                        request_id: trace_id,
                        route_name: None,
                        requested_model: request.model.clone(),
                        final_status: FinalStatus::ClientError,
                        failure_reason: Some(client_error_failure_reason(&err)),
                        stream: is_stream,
                        started_at,
                        completed_at: chrono::Utc::now(),
                        usage: None,
                        cached_tokens: None,
                        cache_creation_tokens: None,
                        ttft_ms: None,
                        upstream_duration_ms: None,
                    }),
                )
                .await;
                return err.into_response();
            }
            continue; // provider-persistent 4xx: failover as before
        };

        if strategy == sag_config::Strategy::StickyFailover {
            // Write back only while the token captured at routing decision
            // time still matches: a DELETE / breaker close / targets-changed
            // / prune (per-model generation, or the global epoch for
            // no-entry clears) or any config mutation (routes_version) that
            // happened after this request decided its route must not be
            // overwritten by a stale pin.
            //
            // Linearization protocol: the config read guard is held across
            // the version read *and* the active write, so a config mutation
            // (which commits the new config and bumps `routes_version`
            // atomically under `config.write()`, see
            // `ConfigMutationCoordinator`) can never interleave between the
            // version read and the conditional insert — either this
            // writeback linearizes before the mutation (the mutation waits
            // for the guard) or the version mismatch rejects the pin. The
            // read-compare-insert itself runs under the single active write
            // lock, atomic against concurrent clears.
            let _config_guard = state.config.read().await;
            let current_version = state.config.routes_version();
            let mut active = runtime.active_targets.write().await;
            active.insert_if_generation(
                request.model.clone(),
                sag_state::ActiveTargetEntry::new(
                    provider_name.clone(),
                    real_model.clone(),
                    target.route_model.clone(),
                    target.target_real_model.clone(),
                    std::time::Instant::now(),
                ),
                &sticky_token,
                current_version,
            );
            drop(active);
        }

        match attempt {
            ChatAttempt::Stream(stream, upstream_start) => {
                // The stream's single terminal analytics event (success /
                // stream-error-after-commit / client-cancelled) is emitted
                // from inside `build_streaming_response`, which receives the
                // route identity facts, the wall-clock start of this
                // target's first provider call, and the failure facts of
                // targets that fell through before commit. Hand the
                // post-commit cancellation lifecycle to the stream's own
                // drop guard; the dispatch guard must not fire on the
                // normal return.
                cancel_guard.mark_done();
                return build_streaming_response(
                    state,
                    stream,
                    route_name,
                    provider_name,
                    &request.model,
                    &real_model,
                    trace_id,
                    start,
                    upstream_start,
                    started_at,
                    target_started_at.unwrap_or(chrono::Utc::now()),
                    target_facts,
                    request.reasoning_effort.clone(),
                    user_agent.clone(),
                    client_ip.clone(),
                );
            }
            ChatAttempt::NonStream(resp, upstream_start) => {
                // Non-streaming responses are single-shot: TTFT equals the
                // total upstream duration.
                let upstream_duration_ms = upstream_start.elapsed().as_millis() as u64;
                let timing = Some(upstream_duration_ms);
                state.collector.log(success_log(
                    trace_id,
                    provider_name,
                    &request.model,
                    &real_model,
                    false,
                    start,
                    resp.usage.as_ref(),
                    request.reasoning_effort.as_deref(),
                    timing,
                    timing,
                    user_agent.clone(),
                    client_ip.clone(),
                ));
                // Formal analytics event: the request completed successfully.
                // The writer derives `actual_model` from the final
                // successful target attempt; usage / cache come from the
                // final response; route latency spans the whole dispatch
                // (fallback + retry) via `started_at` → `completed_at`.
                cancel_guard.mark_done();
                emit_analytics_event(
                    state,
                    AnalyticsEvent {
                        result: RequestResultFact {
                            request_id: trace_id,
                            route_name: Some(route_name.to_string()),
                            requested_model: request.model.clone(),
                            final_status: FinalStatus::Success,
                            failure_reason: None,
                            stream: false,
                            started_at,
                            completed_at: chrono::Utc::now(),
                            usage: resp.usage.clone(),
                            cached_tokens: resp.usage.as_ref().and_then(|u| u.cached_tokens),
                            cache_creation_tokens: resp
                                .usage
                                .as_ref()
                                .and_then(|u| u.cache_creation_tokens),
                            ttft_ms: timing,
                            upstream_duration_ms: timing,
                        },
                        targets: target_facts,
                    },
                )
                .await;
                return (StatusCode::OK, Json(resp)).into_response();
            }
        }
    }

    let response = fallback_exhausted_response(
        state,
        &request.model,
        is_stream,
        trace_id,
        start,
        errors,
        request.reasoning_effort.as_deref(),
        user_agent.as_deref(),
        client_ip.as_deref(),
    );
    // Formal analytics event: every target exhausted → `model_failure`
    // with `all_exhausted` and one failure fact per actually-issued
    // target (circuit-open / disabled skips never produce facts). Applies
    // to streaming requests too — a pre-commit failure is the only
    // terminal a stream can reach without committing bytes, so its
    // failure facts are exactly the ones accumulated above.
    cancel_guard.mark_done();
    emit_analytics_event(
        state,
        AnalyticsEvent {
            result: RequestResultFact {
                request_id: trace_id,
                route_name: Some(route_name.to_string()),
                requested_model: request.model.clone(),
                final_status: FinalStatus::ModelFailure,
                failure_reason: Some(FailureReason::AllExhausted),
                stream: is_stream,
                started_at,
                completed_at: chrono::Utc::now(),
                usage: None,
                cached_tokens: None,
                cache_creation_tokens: None,
                ttft_ms: None,
                upstream_duration_ms: None,
            },
            targets: target_facts,
        },
    )
    .await;
    response
}
