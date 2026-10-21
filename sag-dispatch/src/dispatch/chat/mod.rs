//! Chat-completion dispatch with multi-provider fallback.

use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use sag_analytics::AnalyticsEvent;
use sag_core::{ApiKind, IngressProtocol, Lane};
use sag_core::{AppError, FailureReason, FinalStatus, RequestResultFact, TargetAttemptFact};
use sag_providers::{AnthropicMessagesCodec, ChatCompletionsCodec, ResponsesCodec};

use crate::dispatch::resolve::resolve_provider;
use crate::dispatch::retry::RetryOutcome;
use crate::dispatch::retry::is_client_caused_4xx;
use crate::dispatch::retry::run_with_retries_outcome;
use crate::dispatch::stream::build_streaming_response;
use crate::dispatch::{
    CircuitPermit, DispatchCancelGuard, DispatchCtx, apply_circuit_outcome, circuit_allows_target,
    client_error_failure_reason, emit_analytics_event, fallback_exhausted_response,
    log_per_target_failure, merge_model_overrides, success_log, target_base_url,
};

mod attempt;
mod empty;

#[cfg(test)]
mod tests;

pub(crate) use attempt::{ChatAttempt, NonStreamAttempt, build_target_attempt_fact};
pub(crate) use empty::{is_empty_ir_response, validate_stream_first_chunk};

/// Chat ingress stream encoder: Raw bytes pass through unchanged; structured
/// events / control frames go through the Chat codec. (`End` / `Usage` /
/// `Error` are handled inline by the unfold loop and never reach `encode`
/// for client yield — this fn is only for client-visible events.)
fn encode_chat_stream_event(ev: &sag_core::IrStreamEvent) -> Vec<u8> {
    ChatCompletionsCodec::encode_stream_event(ev)
}

/// Build a per-response Anthropic stream encoder with content-block
/// lifecycle state (start/stop) + Raw passthrough. Satisfies
/// `Fn(&IrStreamEvent) -> Vec<u8> + Send + 'static`.
fn anthropic_stream_encoder() -> impl Fn(&sag_core::IrStreamEvent) -> Vec<u8> + Send + 'static {
    AnthropicMessagesCodec::stream_encoder()
}

/// Build a per-response Responses stream encoder with output-item /
/// content-part lifecycle state + Raw passthrough.
fn responses_stream_encoder() -> impl Fn(&sag_core::IrStreamEvent) -> Vec<u8> + Send + 'static {
    ResponsesCodec::stream_encoder()
}

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
        ingress_protocol,
        ir,
        entry_loss,
        session_key,
        mut session_writeback,
    } = ctx;

    // Hashed session identity for the `call_logs` row: recorded for
    // **every** strategy — the session grouping is a request fact, the
    // stickiness is a routing behavior, and a fact column must not depend
    // on behavior. NULL means "no session identity was extracted" (and
    // nothing else).
    let session_log_key = session_key.as_ref().map(|k| k.map_id(&request.model));

    // IR orchestration view: routes.rs decodes once after
    // `normalize_reasoning_effort`; fall back to a local decode so direct
    // test constructions that pass `ir: None` still work. Model / reasoning
    // identity continues to read from `request` for exact parity with the
    // pre-M4 path (`request.model` is the route identity).
    let ir_fallback;
    let ir = match ir {
        Some(ir) => ir,
        None => {
            ir_fallback = ChatCompletionsCodec::decode_request(request);
            &ir_fallback
        }
    };
    let is_anthropic_ingress = matches!(ingress_protocol, IngressProtocol::AnthropicMessages);
    let is_responses_ingress = matches!(ingress_protocol, IngressProtocol::Responses);
    // M8: request-side LossInventory summary for CallLog terminals.
    // Encode-side losses are collected per-attempt into this cell and
    // merged (append unique entries — never overwrite) with response
    // losses at every terminal: non-stream success, stream commit,
    // per-target failure, and fallback. Seeded with the entry-route
    // encode loss (`routes.rs` synthesis) so it is never dropped when
    // attempts produce no further encode loss.
    let encode_losses = std::sync::Arc::new(std::sync::Mutex::new(entry_loss));
    let loss_summary = sag_core::summarize_losses(&[&ir.loss], 5);
    // Snapshot encode-side losses + request losses into one summary.
    // When the encode cell is non-empty it already carries `ir.loss` as
    // its base (encode codecs seed from `ir.loss`), so prefer it alone.
    let encode_aware_loss_summary = {
        let encode_losses = encode_losses.clone();
        let base_summary = loss_summary.clone();
        move || {
            let encode_snap = encode_losses.lock().map(|g| g.clone()).unwrap_or_default();
            if encode_snap.is_empty() {
                base_summary.clone()
            } else {
                sag_core::summarize_losses(&[&encode_snap], 5)
            }
        }
    };

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
        state.collector.clone(),
        trace_id,
        route_name,
        &request.model,
        is_stream,
        ingress_protocol,
        started_at,
        start,
        request.reasoning_effort.clone(),
        user_agent.clone(),
        client_ip.clone(),
        session_log_key.clone(),
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

        // The circuit key folds in the upstream base address, so targets
        // on different addresses (同址多协议 excluded) have isolated health.
        let circuit_url =
            target_base_url(&config.providers, provider_name, target.upstream_protocol);
        let Some(permit) = circuit_allows_target(
            runtime,
            &request.model,
            provider_name,
            real_model,
            &circuit_url,
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
        let encode_losses = encode_losses.clone();

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
                let encode_losses = encode_losses.clone();
                async move {
                    // An actual provider call is in flight from here until
                    // the attempt's future resolves. A client disconnect
                    // during this window drops the future with the marker
                    // still set, so the dispatch cancel guard persists the
                    // attempt as `cancelled` (spec: 在途调用 → cancelled).
                    // Cancels during backoff sleeps or between targets have
                    // no marker → 0 target attempts (spec: 退避/fallback
                    // 间隙 → 0 条).
                    cancel.begin_attempt(
                        &provider_name,
                        &real_model,
                        Some(target.upstream_protocol.as_str()),
                        Some(target.lane.as_str()),
                    );
                    // Branch on the actually-used upstream protocol, not
                    // just the native-lane flags: a Translated lane whose
                    // fallback is `anthropic_messages` must encode the IR
                    // down to an Anthropic body and hit
                    // `apis.anthropic_messages.base_url` — never the Chat
                    // node's address (CatalogProvider::chat used to
                    // blind-prefer chat_completions on multi-protocol
                    // providers).
                    let use_anthropic = target.upstream_protocol == ApiKind::AnthropicMessages;
                    let use_responses = target.upstream_protocol == ApiKind::Responses;
                    // Native flags only gate *raw upstream JSON* egress
                    // (structure fidelity on the ingress-native lane); they
                    // no longer select the call path.
                    let use_native_anthropic = is_anthropic_ingress && target.lane == Lane::Native;
                    let use_native_responses = is_responses_ingress && target.lane == Lane::Native;
                    let is_chat_ingress =
                        matches!(ingress_protocol, IngressProtocol::ChatCompletions);
                    let result = async {
                        // Capture the upstream start time immediately before the
                        // provider call so retry/circuit overhead is excluded.
                        let upstream_start = std::time::Instant::now();
                        if is_stream {
                            let s = if use_anthropic {
                                let (body, loss) = AnthropicMessagesCodec::encode_request_with_loss(
                                    ir,
                                    &real_model,
                                );
                                if !loss.is_empty() {
                                    tracing::warn!(
                                        loss = ?loss.entries,
                                        "anthropic encode recorded losses"
                                    );

                                    // Accumulate (append unique) — never overwrite:
                                    // multi-attempt / multi-target encodes must not
                                    // drop earlier encode-side losses.
                                    if let Ok(mut acc) = encode_losses.lock() {
                                        for e in loss.entries {
                                            if !acc.entries.contains(&e) {
                                                acc.entries.push(e);
                                            }
                                        }
                                    }
                                }
                                provider
                                    .anthropic_messages_stream(body, &target_extra)
                                    .await?
                            } else if use_responses {
                                let (body, loss) =
                                    ResponsesCodec::encode_request_with_loss(ir, &real_model);
                                if !loss.is_empty() {
                                    tracing::warn!(
                                        loss = ?loss.entries,
                                        "native responses encode recorded losses"
                                    );

                                    if let Ok(mut acc) = encode_losses.lock() {
                                        for e in loss.entries {
                                            if !acc.entries.contains(&e) {
                                                acc.entries.push(e);
                                            }
                                        }
                                    }
                                }
                                provider
                                    .responses_stream_native(body, &target_extra, &api_endpoint)
                                    .await?
                            } else {
                                provider
                                    .chat_stream(
                                        request,
                                        &real_model,
                                        &api_endpoint,
                                        &target_extra,
                                        timeout_secs,
                                    )
                                    .await?
                            };
                            // Dispatch-boundary mapping depends on ingress vs
                            // the upstream frame protocol:
                            // - Anthropic frames (native or translated) on an
                            //   Anthropic ingress: Raw passthrough (native).
                            // - Anthropic frames on any other ingress:
                            //   decode to IR so the ingress encoder can
                            //   re-render them.
                            // - Chat ingress + Chat bridge frames: Raw
                            //   passthrough (M4 byte-fidelity invariant).
                            // - Anthropic/Responses ingress + Chat-shaped
                            //   frames: decode to IR, re-render per ingress.
                            // - Responses-ingress native: Raw passthrough.
                            // Usage/Error/End → IR 1:1 in all cases.
                            let needs_ir_decode = if use_anthropic {
                                !is_anthropic_ingress
                            } else {
                                (is_anthropic_ingress || is_responses_ingress)
                                    && !use_native_responses
                            };
                            let s = if needs_ir_decode {
                                // Frame protocol for the decode: the anthropic
                                // arm always emits Anthropic SSE (native and
                                // translated alike); other arms bridge through
                                // OpenAI-shaped Data.
                                let decode: fn(&[u8]) -> Vec<sag_core::IrStreamEvent> =
                                    if use_anthropic {
                                        AnthropicMessagesCodec::decode_stream_frame
                                    } else {
                                        ChatCompletionsCodec::decode_stream_frame
                                    };
                                s.flat_map(move |chunk| match chunk {
                                    sag_core::StreamChunk::Data(bytes) => {
                                        futures_util::stream::iter(decode(&bytes))
                                    }
                                    other => futures_util::stream::iter(vec![
                                        sag_core::IrStreamEvent::from(other),
                                    ]),
                                })
                                .boxed()
                            } else {
                                s.map(sag_core::IrStreamEvent::from).boxed()
                            };
                            // Chat ingress expects the OpenAI SSE terminator.
                            // An Anthropic upstream never emits `data: [DONE]`;
                            // synthesize exactly one on the first End (later
                            // Ends — e.g. the provider's trailing Done — still
                            // terminate the loop).
                            let s = if is_chat_ingress && use_anthropic {
                                let mut done_sent = false;
                                s.map(move |ev| match ev {
                                    sag_core::IrStreamEvent::End if !done_sent => {
                                        done_sent = true;
                                        sag_core::IrStreamEvent::Raw(b"data: [DONE]\n\n".to_vec())
                                    }
                                    other => other,
                                })
                                .boxed()
                            } else {
                                s
                            };
                            // Frame protocol for empty-peek: the anthropic /
                            // responses arms pass the target protocol's SSE
                            // (native and translated alike); the Chat bridge
                            // (`chat_stream`) always re-encodes to Chat-shaped
                            // Data frames, so the chat arm peeks as
                            // ChatCompletions.
                            let frame_upstream = if use_anthropic {
                                ApiKind::AnthropicMessages
                            } else if use_responses {
                                ApiKind::Responses
                            } else {
                                ApiKind::ChatCompletions
                            };
                            if retry_on_empty {
                                // Peek at the first chunk before committing the HTTP
                                // 200 response.  If the upstream returned an empty
                                // response (ct=0, no content), surface an
                                // `EmptyResponse` error so the retry loop retries the
                                // same target with backoff.
                                let s =
                                    validate_stream_first_chunk(s, &provider_name, frame_upstream)
                                        .await?;
                                Ok(ChatAttempt::Stream(s, upstream_start))
                            } else {
                                Ok(ChatAttempt::Stream(s, upstream_start))
                            }
                        } else if use_anthropic {
                            let (body, loss) =
                                AnthropicMessagesCodec::encode_request_with_loss(ir, &real_model);
                            if !loss.is_empty() {
                                tracing::warn!(
                                    loss = ?loss.entries,
                                    "anthropic encode recorded losses"
                                );

                                if let Ok(mut acc) = encode_losses.lock() {
                                    for e in loss.entries {
                                        if !acc.entries.contains(&e) {
                                            acc.entries.push(e);
                                        }
                                    }
                                }
                            }
                            let raw = provider
                                .anthropic_messages(body, &target_extra, timeout_secs)
                                .await?;
                            let mut ir_resp = AnthropicMessagesCodec::decode_response(&raw);
                            // Chat egress reports the real_model we requested
                            // (parity with the legacy AnthropicProvider::chat
                            // path); Anthropic leaves `model` unset when the
                            // upstream omits it.
                            ir_resp.model = real_model.clone();
                            if retry_on_empty && is_empty_ir_response(&ir_resp) {
                                return Err(AppError::EmptyResponse {
                                    provider: provider_name.clone(),
                                });
                            }
                            // Dual-hold: IR for empty/usage + the original
                            // Anthropic JSON. Raw passthrough only on the
                            // native lane (Anthropic ingress egress prefers
                            // it); a translated anthropic fallback re-encodes
                            // from IR per ingress (Chat placeholder in `resp`
                            // covers accidental Chat egress).
                            let resp = ChatCompletionsCodec::encode_response(&ir_resp);
                            Ok(ChatAttempt::NonStream(
                                Box::new(NonStreamAttempt {
                                    ir: ir_resp,
                                    resp,
                                    raw_responses: use_native_anthropic.then_some(raw),
                                }),
                                upstream_start,
                            ))
                        } else if use_responses {
                            let (body, loss) =
                                ResponsesCodec::encode_request_with_loss(ir, &real_model);
                            if !loss.is_empty() {
                                tracing::warn!(
                                    loss = ?loss.entries,
                                    "native responses encode recorded losses"
                                );

                                if let Ok(mut acc) = encode_losses.lock() {
                                    for e in loss.entries {
                                        if !acc.entries.contains(&e) {
                                            acc.entries.push(e);
                                        }
                                    }
                                }
                            }
                            let raw = provider
                                .responses_native(body, &target_extra, &api_endpoint, timeout_secs)
                                .await?;
                            // Decode only for empty/usage/CallLog — the raw
                            // JSON is the sole egress source (B1 plan A:
                            // never decode→encode round-trip on Responses
                            // ingress, which drops instructions/tools/
                            // temperature/text/item ids/annotations).
                            let ir_resp = ResponsesCodec::decode_response(&raw);
                            if retry_on_empty && is_empty_ir_response(&ir_resp) {
                                return Err(AppError::EmptyResponse {
                                    provider: provider_name.clone(),
                                });
                            }
                            // Chat-shaped placeholder (Chat egress never
                            // reaches this arm — native requires Responses
                            // ingress).
                            let resp = ChatCompletionsCodec::encode_response(&ir_resp);
                            Ok(ChatAttempt::NonStream(
                                Box::new(NonStreamAttempt {
                                    ir: ir_resp,
                                    resp,
                                    // Plan B1: only the native arm egresses the
                                    // original upstream JSON; a translated
                                    // Responses fallback re-encodes from IR.
                                    raw_responses: use_native_responses.then_some(raw),
                                }),
                                upstream_start,
                            ))
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
                            // Decode once for IR-aware empty detection + usage
                            // facts; the original `r` is retained for egress.
                            let ir_resp = ChatCompletionsCodec::decode_response(&r);
                            if retry_on_empty && is_empty_ir_response(&ir_resp) {
                                return Err(AppError::EmptyResponse {
                                    provider: provider_name.clone(),
                                });
                            }
                            Ok(ChatAttempt::NonStream(
                                Box::new(NonStreamAttempt {
                                    ir: ir_resp,
                                    resp: r,
                                    raw_responses: None,
                                }),
                                upstream_start,
                            ))
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
                Some(target.upstream_protocol.as_str()),
                Some(target.lane.as_str()),
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
                Some(ingress_protocol.as_str().to_string()),
                Some(target.upstream_protocol.as_str().to_string()),
                Some(target.lane.as_str().to_string()),
                encode_aware_loss_summary(),
                session_log_key.clone(),
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
                        ingress_protocol: Some(ingress_protocol.as_str().to_string()),
                        lane: None,
                    }),
                )
                .await;
                return err.into_response();
            }
            continue; // provider-persistent 4xx: failover as before
        };

        match strategy {
            sag_config::Strategy::StickyFailover => {
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
            sag_config::Strategy::SessionSticky => {
                // Rebind the session pin to the target that actually succeeded
                // (failover convergence — design §4.3): without it, every
                // later request of the session would keep re-trying the dead
                // target first. Same conditional-write protocol as above:
                // the epoch / per-key generation / routes_version captured at
                // the ordering decision must all still match, and the compare
                // + insert run under one write lock (atomic against admin
                // clears / LRU eviction). A failed request writes nothing —
                // the old pin stays and simply falls through until a success
                // rebinds it.
                if let Some(wb) = session_writeback.take() {
                    let _config_guard = state.config.read().await;
                    let current_version = state.config.routes_version();
                    let mut session = runtime.session_affinity.write().await;
                    session.insert_if_generation(
                        &wb,
                        sag_state::SessionPin::new(
                            provider_name.clone(),
                            real_model.clone(),
                            target.route_model.clone(),
                            target.target_real_model.clone(),
                            std::time::Instant::now(),
                            current_version,
                        )
                        .with_source(session_key.as_ref().map(|k| k.source).unwrap_or_default()),
                        current_version,
                    );
                }
            }
            _ => {}
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
                // Ingress stream encoder: Chat → Chat codec (Raw passthrough);
                // Anthropic → per-response stateful Anthropic SSE encoder
                // (Raw passthrough, block lifecycle, [DONE] suppressed);
                // Responses → per-response stateful Responses SSE encoder
                // (Raw passthrough, output-item lifecycle, no [DONE]).
                if is_anthropic_ingress {
                    return build_streaming_response(
                        state,
                        stream,
                        anthropic_stream_encoder(),
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
                        Some(ingress_protocol.as_str().to_string()),
                        Some(target.upstream_protocol.as_str().to_string()),
                        Some(target.lane.as_str().to_string()),
                        encode_aware_loss_summary(),
                        session_log_key.clone(),
                    );
                }
                if is_responses_ingress {
                    return build_streaming_response(
                        state,
                        stream,
                        responses_stream_encoder(),
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
                        Some(ingress_protocol.as_str().to_string()),
                        Some(target.upstream_protocol.as_str().to_string()),
                        Some(target.lane.as_str().to_string()),
                        encode_aware_loss_summary(),
                        session_log_key.clone(),
                    );
                }
                return build_streaming_response(
                    state,
                    stream,
                    encode_chat_stream_event,
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
                    Some(ingress_protocol.as_str().to_string()),
                    Some(target.upstream_protocol.as_str().to_string()),
                    Some(target.lane.as_str().to_string()),
                    encode_aware_loss_summary(),
                    session_log_key.clone(),
                );
            }
            ChatAttempt::NonStream(att, upstream_start) => {
                // Non-streaming responses are single-shot: TTFT equals the
                // total upstream duration. Usage comes from the IR view
                // (same `Usage` type / values as the original response).
                let upstream_duration_ms = upstream_start.elapsed().as_millis() as u64;
                let timing = Some(upstream_duration_ms);
                // Merge request + encode + response losses into the CallLog.
                // Encode inventory already starts from ir.loss, so prefer it
                // over the bare request inventory when present.
                let encode_snap = encode_losses.lock().map(|g| g.clone()).unwrap_or_default();
                let request_snap: sag_core::LossInventory = if encode_snap.is_empty() {
                    ir.loss.clone()
                } else {
                    encode_snap
                };
                let success_loss = sag_core::summarize_losses(&[&request_snap, &att.ir.loss], 5);
                state.collector.log(success_log(
                    trace_id,
                    provider_name,
                    &request.model,
                    &real_model,
                    false,
                    start,
                    att.ir.usage.as_ref(),
                    request.reasoning_effort.as_deref(),
                    timing,
                    timing,
                    user_agent.clone(),
                    client_ip.clone(),
                    Some(ingress_protocol.as_str().to_string()),
                    Some(target.upstream_protocol.as_str().to_string()),
                    Some(target.lane.as_str().to_string()),
                    success_loss,
                    session_log_key.clone(),
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
                            usage: att.ir.usage.clone(),
                            cached_tokens: att.ir.usage.as_ref().and_then(|u| u.cached_tokens),
                            cache_creation_tokens: att
                                .ir
                                .usage
                                .as_ref()
                                .and_then(|u| u.cache_creation_tokens),
                            ttft_ms: timing,
                            upstream_duration_ms: timing,
                            ingress_protocol: Some(ingress_protocol.as_str().to_string()),
                            // Request-level lane: lane of the successful target
                            // (first successful attempt; the loop only reaches
                            // here with `target` still in scope).
                            lane: Some(target.lane.as_str().to_string()),
                        },
                        targets: target_facts,
                    },
                )
                .await;
                // Egress:
                // - Chat ingress uses the **original** ChatCompletionResponse —
                //   dual-hold guarantees byte-identical JSON vs pre-M4
                //   (`encode_response` is not byte-safe for null content etc.).
                // - Anthropic ingress: native arm returns the **original**
                //   upstream JSON (structure fidelity — unknown fields / ids
                //   survive); translated arms encode from IR (semantic
                //   round-trip; tool_use / thinking / usage preserved by the
                //   codec).
                // - Responses ingress: native arm returns the **original**
                //   upstream JSON (B1 plan A — IR round-trip drops
                //   instructions/tools/temperature/text/item id/annotations);
                //   translated arms still encode from IR (stateless subset).
                if is_anthropic_ingress {
                    if let Some(raw) = att.raw_responses.as_ref() {
                        return (StatusCode::OK, Json(raw.clone())).into_response();
                    }
                    let body = AnthropicMessagesCodec::encode_response(&att.ir);
                    return (StatusCode::OK, Json(body)).into_response();
                }
                if is_responses_ingress {
                    if let Some(raw) = att.raw_responses.as_ref() {
                        return (StatusCode::OK, Json(raw.clone())).into_response();
                    }
                    let body = ResponsesCodec::encode_response(&att.ir);
                    return (StatusCode::OK, Json(body)).into_response();
                }
                return (StatusCode::OK, Json(att.resp)).into_response();
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
        Some(ingress_protocol.as_str().to_string()),
        None,
        None,
        {
            let encode_snap = encode_losses.lock().map(|g| g.clone()).unwrap_or_default();
            if encode_snap.is_empty() {
                loss_summary.clone()
            } else {
                sag_core::summarize_losses(&[&encode_snap], 5)
            }
        },
        session_log_key,
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
                ingress_protocol: Some(ingress_protocol.as_str().to_string()),
                // All-exhausted: lane of the first actually-issued attempt
                // (target_facts non-empty ⇒ at least one attempt carried a lane).
                lane: target_facts.first().and_then(|f| f.lane.clone()),
            },
            targets: target_facts,
        },
    )
    .await;
    response
}
