//! Chat-completion dispatch with multi-provider fallback.

use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use sag_analytics::AnalyticsEvent;
use sag_core::{
    ApiKind, IngressProtocol, Lane,
    api::{
        anthropic::{AnthropicContentBlock, AnthropicMessagesResponse},
        responses::{OutputItem, ResponsesResponse},
    },
};
use sag_core::{AppError, FailureReason, FinalStatus, RequestResultFact, TargetAttemptFact};
use sag_privacy::{Outcome as PrivacyOutcome, PlaceholderMap, PrivacyEngine};
use sag_providers::{AnthropicMessagesCodec, ChatCompletionsCodec, ResponsesCodec};
use serde_json::json;

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

/// Which terminal frame the first-`End` synthesis (#2 / #3) must produce on a
/// **translated** upstream.
#[derive(Clone, Copy)]
enum EndSynthesis {
    /// Bridge #2 — Anthropic ingress + translated upstream: the upstream never
    /// emits `message_stop`, so the ingress encoder's `End => message_stop`
    /// branch would never run and the client would never see a terminator.
    /// The synthesized frame is matched by `sag_core::is_client_terminal_frame`,
    /// so a client drop right after it classifies as success (200) instead of
    /// client-cancelled (499).
    AnthropicMessageStop,
    /// Bridge #3 — Responses ingress + translated upstream: the ingress
    /// encoder renders `response.completed` from a structured `Finish`, never
    /// from `End`, so a non-compliant upstream (no `finish_reason`) would leave
    /// the client without its terminal frame.
    ResponsesCompleted,
}

/// Synthesize the ingress protocol's terminal frame on the first `End` of a
/// **translated** upstream (`kind == None` disables the bridge: the native
/// lanes keep the upstream's own `message_stop` / `response.completed` as Raw
/// passthrough).
///
/// `finish_seen` tracks whether the upstream already produced a structured
/// `Finish` (i.e. a compliant upstream that carries `finish_reason`). A
/// non-compliant upstream (no `finish_reason`) leaves the ingress encoder with
/// an unclosed content block / no terminal frame, so the first `End` is
/// preceded by a synthesized `Finish { finish_reason: None }`:
/// - Anthropic encoder → `content_block_stop` (closes the open block) +
///   `message_delta` with the protocol-required `stop_reason`;
/// - Responses encoder → `response.completed` (`status: completed`).
///
/// A compliant upstream must not gain a second `message_delta` /
/// `response.completed`, so the synthesis is skipped there — but the Anthropic
/// lane still emits the literal `message_stop` frame, because the ingress
/// encoder never renders `End` itself. Later Ends pass through unchanged and
/// terminate the loop.
///
/// **Call-site invariant:** this runs *after* the empty-response peek, so the
/// peek observes the upstream's own semantics (a vacuum upstream whose first
/// event is `End` still yields `EmptyResponse` and retries). Applying it
/// before the peek would make `classify_raw_frame` read the synthesized frame
/// as `Content` and silently skip the retry.
fn apply_end_synthesis_bridge(
    s: futures_util::stream::BoxStream<'static, sag_core::IrStreamEvent>,
    kind: Option<EndSynthesis>,
) -> futures_util::stream::BoxStream<'static, sag_core::IrStreamEvent> {
    let Some(kind) = kind else {
        return s;
    };
    let mut finish_seen = false;
    let mut end_sent = false;
    s.flat_map(move |ev| match ev {
        sag_core::IrStreamEvent::Finish { .. } => {
            finish_seen = true;
            futures_util::stream::iter(vec![ev])
        }
        sag_core::IrStreamEvent::End if !end_sent => {
            end_sent = true;
            let mut out = Vec::with_capacity(2);
            if !finish_seen {
                out.push(sag_core::IrStreamEvent::Finish {
                    index: 0,
                    finish_reason: None,
                });
            }
            match kind {
                EndSynthesis::AnthropicMessageStop => out.push(sag_core::IrStreamEvent::Raw(
                    b"data: {\"type\":\"message_stop\"}\n\n".to_vec(),
                )),
                // A compliant upstream already produced `response.completed`
                // from its own `Finish`; the first End only terminates.
                EndSynthesis::ResponsesCompleted if finish_seen => {
                    out.push(sag_core::IrStreamEvent::End)
                }
                EndSynthesis::ResponsesCompleted => {}
            }
            futures_util::stream::iter(out)
        }
        other => futures_util::stream::iter(vec![other]),
    })
    .boxed()
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

    // ── Privacy filter ─────────────────────────────────────────────────
    // Snapshot the config once per request (the privacy section here, the
    // retry schedule and the circuit-breaker lookups below all read from the
    // same consistent view: a hot-reload mid-dispatch must not change the
    // schedule for an in-flight request).
    let config = state.config.read().await.clone();
    let privacy_cfg = config.privacy.clone();
    // Disabled (the default) changes nothing else: no engine compilation, no
    // scan, no response rewriting.
    let mut privacy_request: Option<sag_core::ChatCompletionRequest> = None;
    let mut privacy_ir: Option<sag_core::IrRequest> = None;
    let mut privacy_map: Option<PlaceholderMap> = None;
    if privacy_cfg.enabled {
        // Fail closed: an engine that cannot be compiled must never let an
        // unmasked request through. The client message stays generic (no
        // regex detail); the diagnostic goes to the log.
        let engine = match PrivacyEngine::compile(&privacy_cfg) {
            Ok(engine) => engine,
            Err(err) => {
                tracing::error!(error = %err, "privacy engine init failed");
                return privacy_unavailable_response();
            }
        };
        // The ingress request is borrowed immutably (route identity, session
        // key, analytics all read it), so the filter works on a copy.
        let mut candidate = request.clone();
        match engine.mask_request(&mut candidate) {
            PrivacyOutcome::Blocked(report) => {
                tracing::warn!(hits = %render_hits(&report.hits), "privacy: request blocked");
                return privacy_violation_response(&report.hits);
            }
            PrivacyOutcome::Masked(report) => {
                tracing::info!(hits = %render_hits(&report.hits), "privacy: request masked");
                // The IR is the encode source for translated lanes, so the
                // placeholders must reach it too. Mask the IR **in place**:
                // re-decoding it from the masked Chat request would rebuild
                // the whole IR through the Chat codec and lose the ingress
                // protocol's structure (`routes.rs` decodes Anthropic /
                // Responses bodies directly) — `Reasoning` would be
                // downgraded to `Text`, `ToolResult` split into `role: "tool"`
                // messages, and the loss silent (the clone keeps `ir.loss`).
                // `mask_ir` keeps every block variant and only rewrites text.
                let mut masked_ir = match ir {
                    Some(original) => original.clone(),
                    // Direct test constructions pass `ir: None`; the fallback
                    // decode below needs the same masked source.
                    None => ChatCompletionsCodec::decode_request(&candidate),
                };
                // The IR is a **second** masking source, not a mirror of the
                // Chat request: on the Anthropic / Responses ingresses the IR
                // is decoded from the raw body, so it carries protocol-only
                // fields (message `raw`, unknown top-level keys, `Reasoning`
                // text) that the synthesized Chat request never held. Those
                // placeholders exist only in `mask_ir`'s report, so the maps
                // must be merged — otherwise a model echoing such a
                // placeholder reaches the client unmasked. Only the maps are
                // merged: `hits` stay as `mask_request` counted them, since
                // both scans cover the shared text and merging would
                // double-count it.
                let mut restore_map = report.map;
                if let PrivacyOutcome::Masked(ir_report) = engine.mask_ir(&mut masked_ir) {
                    restore_map.extend(&ir_report.map);
                }
                privacy_ir = Some(masked_ir);
                // The reverse map is only useful when the response is
                // restored; `restore = false` forwards placeholders as-is.
                if privacy_cfg.restore && !restore_map.is_empty() {
                    privacy_map = Some(restore_map);
                }
                privacy_request = Some(candidate);
            }
            PrivacyOutcome::Detected(report) => {
                tracing::warn!(
                    hits = %render_hits(&report.hits),
                    "privacy: sensitive values detected (warn mode, forwarded unchanged)"
                );
            }
            PrivacyOutcome::Clean => {}
        }
    }
    // Registry preview source: the **raw** (pre-masking) request and IR,
    // captured before the masked copies replace them below. `session_registry`
    // is a local management surface — privacy masking exists to keep sensitive
    // values out of the content sent upstream, so the registry keeps the
    // original text (see `sag_core::session_preview`'s module docs).
    let registry_request = request;
    let registry_ir = ir;
    // From here on the masked copy (when there is one) is the request every
    // provider call and every egress path sees.
    let request = privacy_request.as_ref().unwrap_or(request);

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
    let ir = if let Some(masked) = privacy_ir.as_ref() {
        // A masked request must be encoded from its masked IR (see above).
        masked
    } else {
        match ir {
            Some(ir) => ir,
            None => {
                ir_fallback = ChatCompletionsCodec::decode_request(request);
                &ir_fallback
            }
        }
    };
    // Session-registry fact for the log page's session info card: the
    // display companion of `session_log_key` (same key, same route model,
    // same `started_at`). Written exactly **once per request**, here, before
    // the target loop — never in a terminal path (`success_log` /
    // `failure_log` / cancel), which would repeat the write per attempt.
    //
    // The preview comes from the **raw** IR: the registry is a local
    // management surface, so privacy masking (an upstream-bound concern) must
    // not reach it. When the caller passed `ir: None`, decode the raw request
    // for the same reason — the masked `ir` above is the encode source, not a
    // display source.
    if let Some(key) = session_key.as_ref() {
        let registry_ir_fallback;
        let preview_ir = match registry_ir {
            Some(raw) => Some(raw),
            None => {
                registry_ir_fallback = ChatCompletionsCodec::decode_request(registry_request);
                Some(&registry_ir_fallback)
            }
        };
        let fact = sag_core::build_session_fact(key, &request.model, preview_ir, started_at);
        state.collector.log_session(fact);
    }

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

    // Backoff / circuit-breaker knobs come from the config snapshot taken at
    // the top of the dispatch (see the privacy block).
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
                                    .anthropic_messages_stream(body_to_value(&body)?, &target_extra)
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
                                    .responses_stream_native(
                                        body_to_value(&body)?,
                                        &target_extra,
                                        &api_endpoint,
                                    )
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
                            // Terminal-frame bridge #1 (Chat ingress +
                            // Anthropic **or** Responses upstream): neither
                            // upstream protocol emits `data: [DONE]`, so
                            // synthesize it on the first End; later Ends (e.g.
                            // the provider's trailing Done) still terminate the
                            // loop. Applied before the empty-response peek —
                            // `classify_raw_frame` treats `data: [DONE]` as
                            // `RawVerdict::Empty`, so an empty upstream still
                            // surfaces `EmptyResponse` and retries.
                            //
                            // On the Responses arm the response frames stay a
                            // byte-identical Raw passthrough (the Chat-ingress
                            // M4 byte-fidelity invariant): `[DONE]` only adds
                            // the Chat protocol's terminator. Converting those
                            // frames into Chat-shaped chunks is a separate
                            // concern and out of scope here.
                            //
                            // Bridges #2 / #3 (Anthropic ingress
                            // `message_stop` / Responses ingress
                            // `response.completed`) are applied **after** the
                            // peek — see `apply_end_synthesis_bridge`.
                            let s = if is_chat_ingress && (use_anthropic || use_responses) {
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
                            // Translated-ingress bridges: synthesize the
                            // ingress protocol's terminal frame (plus the
                            // missing `Finish` when the upstream sent no
                            // `finish_reason`) on the first End. Applied only to
                            // the stream handed to `ChatAttempt::Stream` (i.e.
                            // **after** the empty-response peek), so the peek
                            // still sees the raw upstream semantics: a vacuum
                            // upstream whose first event is `End` stays empty →
                            // `EmptyResponse` → retry, instead of being masked
                            // by the synthesized frame. The native lanes are
                            // excluded (`!use_anthropic` / `!use_responses`):
                            // they already forward the upstream's own terminal
                            // frame as Raw.
                            let end_synthesis = if is_anthropic_ingress && !use_anthropic {
                                Some(EndSynthesis::AnthropicMessageStop)
                            } else if is_responses_ingress && !use_responses {
                                Some(EndSynthesis::ResponsesCompleted)
                            } else {
                                None
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
                                Ok(ChatAttempt::Stream(
                                    apply_end_synthesis_bridge(s, end_synthesis),
                                    upstream_start,
                                ))
                            } else {
                                Ok(ChatAttempt::Stream(
                                    apply_end_synthesis_bridge(s, end_synthesis),
                                    upstream_start,
                                ))
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
                                .anthropic_messages(
                                    body_to_value(&body)?,
                                    &target_extra,
                                    timeout_secs,
                                )
                                .await?;
                            let mut ir_resp = decode_anthropic_response(&raw);
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
                                .responses_native(
                                    body_to_value(&body)?,
                                    &target_extra,
                                    &api_endpoint,
                                    timeout_secs,
                                )
                                .await?;
                            // Decode only for empty/usage/CallLog — the raw
                            // JSON is the sole egress source (B1 plan A:
                            // never decode→encode round-trip on Responses
                            // ingress, which drops instructions/tools/
                            // temperature/text/item ids/annotations).
                            let ir_resp = decode_responses_response(&raw);
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
                // Privacy: the placeholders a masked request left in the
                // upstream payload come back inside the SSE frames; the
                // writer rewrites them frame by frame (both the translated
                // and the native lane go through `encode`). `None` (privacy
                // off, no hits, or `restore = false`) passes bytes through.
                let mut privacy_restore = privacy_map.clone().map(sag_privacy::RestoreWriter::new);
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
                        privacy_restore.take(),
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
                        privacy_restore.take(),
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
                    privacy_restore.take(),
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
                        return privacy_restored_response(raw.clone(), privacy_map.as_ref());
                    }
                    let body = AnthropicMessagesCodec::encode_response(&att.ir);
                    return privacy_restored_response(body, privacy_map.as_ref());
                }
                if is_responses_ingress {
                    if let Some(raw) = att.raw_responses.as_ref() {
                        return privacy_restored_response(raw.clone(), privacy_map.as_ref());
                    }
                    let body = ResponsesCodec::encode_response(&att.ir);
                    return privacy_restored_response(body, privacy_map.as_ref());
                }
                return privacy_restored_response(att.resp, privacy_map.as_ref());
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

/// Render a `(kind, occurrences)` hit list as `kind=count,kind=count`.
///
/// Only detector kinds and counts ever leave the gateway — a matched value
/// must never reach a log line or an error body.
fn render_hits(hits: &[(String, usize)]) -> String {
    hits.iter()
        .map(|(kind, count)| format!("{kind}={count}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// `400` for a request the privacy filter refused (`mode = "block"`).
///
/// The message carries detector kinds and counts only: the caller learns
/// *what kind* of sensitive value tripped the filter, never the value.
fn privacy_violation_response(hits: &[(String, usize)]) -> Response<Body> {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": {
                "code": "privacy_violation",
                "message": format!(
                    "request rejected by the privacy filter: sensitive values detected ({})",
                    render_hits(hits)
                ),
            }
        })),
    )
        .into_response()
}

/// `503` for a privacy engine that failed to compile (fail-closed).
///
/// The message is deliberately generic: a compile failure names the offending
/// user regex, which must not be echoed to the client (the detail is logged).
fn privacy_unavailable_response() -> Response<Body> {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": {
                "code": "SERVICE_UNAVAILABLE",
                "message": "privacy engine initialization failed; the request was not forwarded",
            }
        })),
    )
        .into_response()
}

/// Serialize a codec-encoded typed body for the provider trait boundary.
///
/// The codecs hand back typed DTOs (`AnthropicMessagesRequest` /
/// `ResponsesRequest`); `LlmProvider`'s native methods still take raw JSON, so
/// the conversion happens here and nowhere else. The DTOs are plain data, so a
/// failure cannot happen in practice — surface it as an internal error rather
/// than panicking mid-dispatch.
fn body_to_value<T: serde::Serialize>(body: &T) -> sag_core::Result<serde_json::Value> {
    serde_json::to_value(body)
        .map_err(|err| AppError::Internal(format!("encode request body to JSON: {err}")))
}

/// Parse an upstream Anthropic body into the typed DTO, then decode it to IR.
///
/// The pre-typed `decode_response(&Value)` never failed: a body that did not
/// match the schema still yielded an IR response with whatever fields were
/// readable. Keep that contract — a body the typed schema rejects is decoded
/// leniently instead of surfacing a hard error: strings are read directly,
/// every content block that does not fit the DTO rides through as `Unknown`
/// (the codec turns it into a loss + `Raw`), and the untouched body is kept in
/// `extra` so the native passthrough lane still egresses the original JSON.
fn decode_anthropic_response(raw: &serde_json::Value) -> sag_core::IrResponse {
    if let Ok(typed) = serde_json::from_value::<AnthropicMessagesResponse>(raw.clone()) {
        return AnthropicMessagesCodec::decode_response(&typed);
    }
    tracing::warn!("upstream anthropic body does not match the typed schema; decoding leniently");

    // NOTE: mirrors `parse_anthropic_response` in `sag-providers/src/anthropic.rs`
    // — keep the two in sync (the duplication is known; a shared helper is a
    // separate change). Both keep the whole body in `extra`; the overlap with
    // typed fields is harmless because `decode_response` re-inserts only the
    // fields it typed and `encode_response` skips the known keys. This twin
    // needs the full body regardless: `ir.raw` is what the native passthrough
    // lane egresses, so nothing may be dropped here.

    let text = |key: &str| {
        raw.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let content = raw
        .get("content")
        .and_then(serde_json::Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .cloned()
                .map(|block| {
                    serde_json::from_value(block.clone())
                        .unwrap_or(AnthropicContentBlock::Unknown(block))
                })
                .collect()
        })
        .unwrap_or_default();
    let typed = AnthropicMessagesResponse {
        id: text("id").unwrap_or_default(),
        kind: text("type").unwrap_or_else(|| "message".to_string()),
        role: text("role").unwrap_or_else(|| "assistant".to_string()),
        content,
        model: text("model").unwrap_or_default(),
        stop_reason: text("stop_reason"),
        stop_sequence: text("stop_sequence"),
        stop_details: raw
            .get("stop_details")
            .and_then(|value| serde_json::from_value(value.clone()).ok()),
        container: raw
            .get("container")
            .and_then(|value| serde_json::from_value(value.clone()).ok()),
        usage: raw
            .get("usage")
            .and_then(|usage| serde_json::from_value(usage.clone()).ok()),
        extra: raw.as_object().cloned().unwrap_or_default(),
    };
    AnthropicMessagesCodec::decode_response(&typed)
}

/// Parse an upstream Responses body into the typed DTO, then decode it to IR.
///
/// Same contract as [`decode_anthropic_response`]: `ResponsesResponse` is
/// almost fully optional, so a schema mismatch only happens when a typed field
/// carries an unexpected shape. The lenient fallback keeps the readable
/// envelope fields and lets each `output` item that does not fit the DTO ride
/// through as `Unknown`, rather than reporting an empty response to the client.
fn decode_responses_response(raw: &serde_json::Value) -> sag_core::IrResponse {
    if let Ok(typed) = serde_json::from_value::<ResponsesResponse>(raw.clone()) {
        return ResponsesCodec::decode_response(&typed);
    }
    tracing::warn!("upstream responses body does not match the typed schema; decoding leniently");

    let text = |key: &str| {
        raw.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let output = raw
        .get("output")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .cloned()
                .map(|item| {
                    serde_json::from_value(item.clone()).unwrap_or(OutputItem::Unknown(item))
                })
                .collect()
        })
        .unwrap_or_default();
    let typed = ResponsesResponse {
        id: text("id").unwrap_or_default(),
        model: text("model"),
        created_at: raw.get("created_at").and_then(serde_json::Value::as_u64),
        status: text("status"),
        output,
        usage: raw
            .get("usage")
            .and_then(|usage| serde_json::from_value(usage.clone()).ok()),
        extra: raw.as_object().cloned().unwrap_or_default(),
        ..Default::default()
    };
    ResponsesCodec::decode_response(&typed)
}

/// Restore placeholders in every string of `value`, returning whether any hit
/// occurred.
///
/// Mirrors the SSE restorer's traversal (string values only — a placeholder
/// never appears as an object key) for the non-streaming egress.
fn restore_value(map: &PlaceholderMap, value: &mut serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => {
            let (restored, hits) = map.replace_in(text);
            if hits == 0 {
                return false;
            }
            *text = restored;
            true
        }
        serde_json::Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= restore_value(map, item);
            }
            changed
        }
        serde_json::Value::Object(fields) => {
            let mut changed = false;
            for field in fields.values_mut() {
                changed |= restore_value(map, field);
            }
            changed
        }
        _ => false,
    }
}

/// Non-streaming egress: return `body` as a JSON response, restoring every
/// placeholder when the privacy filter left a reverse map.
///
/// `map == None` (privacy off, no hits, or `restore = false`) keeps the exact
/// pre-privacy behavior — the typed body goes straight to `Json` and its bytes
/// are untouched.
fn privacy_restored_response<T: serde::Serialize>(
    body: T,
    map: Option<&PlaceholderMap>,
) -> Response<Body> {
    let Some(map) = map else {
        return (StatusCode::OK, Json(body)).into_response();
    };
    let mut value = match serde_json::to_value(&body) {
        Ok(value) => value,
        // Response bodies are plain data; a serialization failure cannot
        // happen in practice, and the typed body is the safer fallback.
        Err(_) => return (StatusCode::OK, Json(body)).into_response(),
    };
    restore_value(map, &mut value);
    (StatusCode::OK, Json(value)).into_response()
}
