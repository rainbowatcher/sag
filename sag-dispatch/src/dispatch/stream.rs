//! SSE response builder for streaming chat completions.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use axum::{body::Body, http::StatusCode, response::Response};
use futures_util::{Stream, StreamExt, stream::unfold};
use sag_analytics::{CallLogCollector, EventCollector, EventQueueError};
use sag_core::{
    CallLog, FailureReason, FinalStatus, IrStreamEvent, RequestResultFact, TargetAttemptFact,
    TargetStatus, Usage,
};
use std::sync::Mutex;
use tracing::{info, warn};

use crate::state::AppState;

/// Terminal analytics facts shared between the stream loop and its drop
/// guard ([`TerminalGuard`]).
struct StreamAnalytics {
    event_collector: Arc<EventCollector>,
    request_id: uuid::Uuid,
    route_name: String,
    /// The model the client requested (route-level identity).
    requested_model: String,
    provider: String,
    real_model: String,
    /// Ingress protocol of the request (mount path → protocol).
    ingress_protocol: Option<String>,
    /// Effective upstream protocol of the streaming target.
    upstream_protocol: Option<String>,
    /// Lane of the streaming target (`"native"` / `"translated"`).
    lane: Option<String>,
    /// Wall-clock route start captured at dispatch time (fact timestamp).
    started_at: chrono::DateTime<chrono::Utc>,
    /// Wall-clock start of this target's first actual provider call.
    target_started_at: chrono::DateTime<chrono::Utc>,
    /// Failure facts of targets that fell through before this stream
    /// committed (retries / fallbacks). Included once in the single
    /// terminal event of this request.
    prior_target_facts: Vec<TargetAttemptFact>,
    /// Set when the normal `Done`/`None` terminal ran (its event is
    /// authoritative); the drop guard must not emit a second event.
    done: AtomicBool,
    /// Set when a mid-stream `Error` chunk was observed — distinguishes
    /// `stream_error_after_commit` from `client_cancelled` on drop.
    error: AtomicBool,
    /// The upstream error message of a mid-stream error, if one was
    /// observed. Mirrors `StreamState.error` (whose owned copy is dropped
    /// with the body before the drop guard fires) so the drop guard can
    /// populate the `call_logs` `error` column. Guarded by a `Mutex` because
    /// `Drop` cannot await and the writer runs on the body future.
    error_message: Mutex<Option<String>>,
    /// Legacy call-log collector: the drop guard writes its `call_logs` row
    /// here (client-disconnect terminal). The normal terminal writes via
    /// `StreamState.collector` instead; the guard holds no handle to that
    /// body-owned state.
    call_log_collector: Arc<CallLogCollector>,
    /// Wall-clock route start (`Instant`) for the `duration_ms` column of the
    /// drop-guard `call_logs` row.
    start: Instant,
    /// Client-request metadata carried onto the drop-guard `call_logs` row.
    reasoning_effort: Option<String>,
    user_agent: Option<String>,
    client_ip: Option<String>,
    /// M8 LossInventory summary (request + encode) for the drop-guard
    /// `call_logs` row — mirrors `StreamState.loss_summary`.
    loss_summary: Option<String>,
    /// Hashed session identity for the drop-guard `call_logs` row (see
    /// `CallLog::session_key`).
    session_key: Option<String>,
    /// Set when a client-visible terminal frame (`data: [DONE]` /
    /// `"type":"message_stop"` / `response.completed`+`response.incomplete`)
    /// was yielded to hyper. Stored *before* the yield, so a disconnect that
    /// hyper detects afterwards is the normal-completion tail (upstream FIN
    /// latency), not a cancel — see `.plan/stream-terminal-frame-race-fix.md`.
    terminal_frame_forwarded: AtomicBool,
    /// Running usage accumulator, shared with the drop guard: the guard needs
    /// the final usage for a `Success` terminal reached after the terminal
    /// frame was forwarded. Locked only at the read / merge point (never
    /// across an await).
    usage: Mutex<Usage>,
    /// Wall-clock first client-visible byte, shared with the drop guard for
    /// the `ttft_ms` column of the drop-as-success terminal.
    first_byte_at: Mutex<Option<Instant>>,
    /// Wall-clock start of this target's first provider call, shared with the
    /// drop guard for `upstream_duration_ms` of the drop-as-success terminal.
    upstream_start: Instant,
}

/// Build the single terminal [`sag_analytics::AnalyticsEvent`] for a
/// streamed request: one request-level fact plus the current target's
/// terminal fact, preceded by the failure facts of targets that fell
/// through before commit.
///
/// `usage` / `ttft_ms` / `upstream_duration_ms` are the success-only
/// payload (mid-stream partial usage never enters a fact); failure and
/// cancel terminals pass `None` so every token / timing column stays NULL.
#[allow(clippy::too_many_arguments)]
fn terminal_event(
    analytics: &StreamAnalytics,
    result_status: FinalStatus,
    result_reason: Option<FailureReason>,
    completed_at: chrono::DateTime<chrono::Utc>,
    usage: Option<Usage>,
    ttft_ms: Option<u64>,
    upstream_duration_ms: Option<u64>,
    target_status: TargetStatus,
    target_reason: Option<FailureReason>,
) -> sag_analytics::AnalyticsEvent {
    sag_analytics::AnalyticsEvent {
        result: RequestResultFact {
            request_id: analytics.request_id,
            route_name: Some(analytics.route_name.clone()),
            requested_model: analytics.requested_model.clone(),
            final_status: result_status,
            failure_reason: result_reason,
            stream: true,
            started_at: analytics.started_at,
            completed_at,
            usage: usage.clone(),
            cached_tokens: usage.as_ref().and_then(|u| u.cached_tokens),
            cache_creation_tokens: usage.as_ref().and_then(|u| u.cache_creation_tokens),
            ttft_ms,
            upstream_duration_ms,
            ingress_protocol: analytics.ingress_protocol.clone(),
            // Stream success: lane of this target; failure: same lane (the
            // attempt was issued on this target).
            lane: analytics.lane.clone(),
        },
        targets: {
            let mut targets = analytics.prior_target_facts.clone();
            targets.push(TargetAttemptFact {
                provider: analytics.provider.clone(),
                real_model: analytics.real_model.clone(),
                status: target_status,
                failure_reason: target_reason,
                started_at: analytics.target_started_at,
                completed_at,
                usage: usage.clone(),
                cached_tokens: usage.as_ref().and_then(|u| u.cached_tokens),
                ttft_ms,
                upstream_duration_ms,
                upstream_protocol: analytics.upstream_protocol.clone(),
                lane: analytics.lane.clone(),
            });
            targets
        },
    }
}

/// Deliver a claimed terminal event so the enqueue survives the response
/// body's cancellation lifecycle *and* normal shutdown:
///
/// 1. Fast path — [`EventCollector::try_emit`]: the event is enqueued
///    synchronously before this function returns. There is no task to
///    cancel and nothing to drain later — the collector's writer, awaited
///    by [`EventCollector::shutdown`] on graceful shutdown, persists the
///    event from the channel. This is the normal-operation path and it is
///    immune to both body-future cancellation and runtime scheduling.
/// 2. Queue full — spawn a detached task that awaits backpressure via
///    [`EventCollector::emit`]. The task is independent of the body
///    future: dropping the body (client disconnect) can never cancel it.
///    Graceful shutdown cannot lose it either: the in-flight `emit` holds
///    its own sender clone, the writer drains the queue (the gateway
///    awaits the drain before the runtime drops), and the `send` resolves
///    once capacity frees — the event is written before the process
///    exits. The only remaining loss window is the runtime-shutdown race
///    on a task that never gets its first poll, and that now additionally
///    requires the queue to be genuinely full at terminal time (the
///    accepted abnormal case, not normal operation).
/// 3. Queue full with no Tokio runtime context (a `Drop` guard dropped
///    off-runtime) — warn and retry `try_emit` once; the writer may have
///    freed a slot in the meantime.
///
/// Returns the spawned backpressure task's join handle when one was
/// spawned, so awaitable call sites can bind normal completion to the
/// enqueue. [`deliver_terminal_event_detached`] covers `Drop` (cannot
/// await) and logs task panics.
///
/// `pub(crate)`: also used by the dispatch-level `DispatchCancelGuard`
/// (the client-disconnect terminal of the dispatch loop) to deliver the
/// `client_cancelled` event of a request cancelled before any terminal was
/// reached.
pub(crate) fn deliver_terminal_event(
    collector: Arc<EventCollector>,
    event: sag_analytics::AnalyticsEvent,
) -> Option<tokio::task::JoinHandle<()>> {
    let request_id = event.result.request_id;
    let status = event.result.final_status;
    match collector.try_emit(event.clone()) {
        Ok(()) => None,
        Err(EventQueueError::Closed(_)) => {
            warn!(
                request_id = %request_id,
                status = ?status,
                "analytics terminal event not enqueued (event collector shut down)"
            );
            None
        }
        Err(EventQueueError::Full) => match tokio::runtime::Handle::try_current() {
            Ok(rt) => Some(rt.spawn(async move {
                if let Err(e) = collector.emit(event).await {
                    warn!(
                        request_id = %request_id,
                        status = ?status,
                        error = %e,
                        "analytics terminal event not enqueued from detached task (collector shut down)"
                    );
                }
            })),
            Err(_) => {
                warn!(
                    request_id = %request_id,
                    status = ?status,
                    "no tokio runtime context and analytics queue full; retrying try_emit once"
                );
                if let Err(e) = collector.try_emit(event) {
                    warn!(
                        request_id = %request_id,
                        status = ?status,
                        error = %e,
                        "analytics terminal event not enqueued via try_emit retry"
                    );
                }
                None
            }
        },
    }
}

/// Fire-and-forget delivery for contexts that cannot await (`Drop`).
/// Enqueue failures are logged by [`deliver_terminal_event`]; panics in
/// an already-spawned backpressure task are logged here via a supervisor.
pub(crate) fn deliver_terminal_event_detached(
    collector: Arc<EventCollector>,
    event: sag_analytics::AnalyticsEvent,
) {
    let Some(handle) = deliver_terminal_event(collector, event) else {
        return;
    };
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        rt.spawn(async move {
            if let Err(e) = handle.await {
                warn!(error = ?e, "analytics terminal delivery task panicked");
            }
        });
    }
}

/// Drop guard on the stream state: emits the terminal analytics event when
/// the response body is abandoned before a normal `Done`/`None` terminal —
/// the axum/hyper client-disconnect detection point (dropping the response
/// body drops this stream, which drops the guard).
///
/// Classifies by the shared terminal state (three-way):
/// - a mid-stream `Error` chunk was observed → `model_failure` /
///   `stream_error_after_commit` (the upstream failed after the first
///   committed byte; no fallback). The error takes precedence over the
///   terminal-frame flag — `response.failed` must never be upgraded to a
///   success;
/// - no error, but a client-visible terminal frame was already forwarded →
///   `Success` with the shared usage / TTFT / upstream timing (the client is
///   entitled to close the connection after `[DONE]` / `message_stop` /
///   `response.completed`; the drop is upstream FIN latency, not a cancel);
/// - no error and no terminal frame → `client_cancelled` (a real mid-stream
///   client disconnect).
///
/// The guard fires at most once: the normal terminal sets the shared
/// `done` flag *before* delivering its event, so a state dropped during or
/// after the terminal delivery is a no-op and can never produce a second
/// request result. Delivery runs through [`deliver_terminal_event`], which
/// decouples the enqueue from the body future's cancellation lifecycle.
struct TerminalGuard {
    analytics: Arc<StreamAnalytics>,
}

/// The three-way classification of a terminal reached through the drop guard
/// ([`TerminalGuard`]).
struct DropTerminal {
    result_status: FinalStatus,
    result_reason: Option<FailureReason>,
    target_status: TargetStatus,
    target_reason: Option<FailureReason>,
    status_code: u16,
    error_category: Option<String>,
    error: Option<String>,
    /// Success-only payload (tokens / cache / timing): `None` for the error
    /// and cancel terminals so every such column stays NULL, mirroring the
    /// normal-terminal contract of `build_stream_call_log`.
    usage: Option<Usage>,
    ttft_ms: Option<u64>,
    upstream_duration_ms: Option<u64>,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Claim the terminal. The normal Done/None path already set `done`;
        // only an abandoned stream reaches this swap with `false`.
        if self.analytics.done.swap(true, Ordering::SeqCst) {
            return;
        }
        let had_error = self.analytics.error.load(Ordering::SeqCst);
        // A client-visible terminal frame was already yielded to hyper: the
        // client may legitimately close the connection at that point, so the
        // drop is the normal-completion tail (upstream FIN latency), not a
        // cancel. The flag is stored before the yield, so it cannot be raced.
        let terminal_forwarded = self
            .analytics
            .terminal_frame_forwarded
            .load(Ordering::SeqCst);
        let terminal = if had_error {
            // An in-stream upstream error was observed before the drop: the
            // failure takes precedence over both the cancel and the
            // terminal-frame flag (model_failure, never upgraded to success
            // or downgraded to client_cancelled). The call_logs row records
            // the upstream error message at 502.
            DropTerminal {
                result_status: FinalStatus::ModelFailure,
                result_reason: Some(FailureReason::StreamErrorAfterCommit),
                target_status: TargetStatus::Failure,
                target_reason: Some(FailureReason::StreamErrorAfterCommit),
                status_code: 502,
                error_category: Some("stream_error".to_string()),
                error: Some(
                    self.analytics
                        .error_message
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                        .unwrap_or_else(|| "upstream stream error".to_string()),
                ),
                usage: None,
                ttft_ms: None,
                upstream_duration_ms: None,
            }
        } else if terminal_forwarded {
            // Race tail: the terminal frame reached the client, the drop is
            // hyper noticing the client's close before the upstream `End`
            // was enqueued. Same payload shape as the non-race success
            // terminal (final shared usage / TTFT / upstream duration).
            let usage = self
                .analytics
                .usage
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let first_byte_at = *self
                .analytics
                .first_byte_at
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let upstream_duration_ms = self.analytics.upstream_start.elapsed().as_millis() as u64;
            DropTerminal {
                result_status: FinalStatus::Success,
                result_reason: None,
                target_status: TargetStatus::Success,
                target_reason: None,
                status_code: 200,
                error_category: None,
                error: None,
                usage: Some(usage),
                ttft_ms: first_byte_at
                    .map(|t| t.duration_since(self.analytics.upstream_start).as_millis() as u64),
                upstream_duration_ms: Some(upstream_duration_ms),
            }
        } else {
            DropTerminal {
                result_status: FinalStatus::ClientCancelled,
                result_reason: Some(FailureReason::ClientCancelled),
                target_status: TargetStatus::Cancelled,
                target_reason: None,
                // 499 = client closed request (nginx convention).
                status_code: 499,
                error_category: Some("client_cancelled".to_string()),
                error: Some("client disconnected before stream completion".to_string()),
                usage: None,
                ttft_ms: None,
                upstream_duration_ms: None,
            }
        };
        let completed_at = chrono::Utc::now();
        let duration_ms = self.analytics.start.elapsed().as_millis() as u64;
        // The error terminal is already logged by `record_stream_error`; the
        // two no-error terminals are logged here.
        if !had_error && terminal_forwarded {
            info!(
                request_id = %self.analytics.request_id,
                provider = %self.analytics.provider,
                model = %self.analytics.requested_model,
                real_model = %self.analytics.real_model,
                duration_ms,
                "client closed after terminal frame; classified as success"
            );
        } else if !had_error {
            warn!(
                request_id = %self.analytics.request_id,
                provider = %self.analytics.provider,
                model = %self.analytics.requested_model,
                real_model = %self.analytics.real_model,
                duration_ms,
                "client disconnected before stream completion; client_cancelled fact + call_logs row enqueued"
            );
        }
        let event = terminal_event(
            &self.analytics,
            terminal.result_status,
            terminal.result_reason,
            completed_at,
            // Success-only payload: the drop-as-success terminal carries the
            // final shared usage / TTFT / upstream timing; failure / cancel
            // keep every token / timing field NULL.
            terminal.usage.clone(),
            terminal.ttft_ms,
            terminal.upstream_duration_ms,
            terminal.target_status,
            terminal.target_reason,
        );
        // Same decoupled delivery as the normal terminal. `Drop` cannot
        // await, so use the detached path (logs enqueue failure + panics).
        deliver_terminal_event_detached(self.analytics.event_collector.clone(), event);

        // The abandoned terminal ALSO writes a `call_logs` row so the
        // request is visible in the admin logs list (`GET /api/logs` reads
        // `call_logs` only). `CallLogCollector::log` is synchronous (a channel
        // `try_send`), safe inside `Drop`. Exactly one row per request: the
        // normal Done/None terminal writes its own row and sets `done`
        // *before* returning, so a state dropped during/after it short-circuits
        // here.
        self.analytics.call_log_collector.log(CallLog {
            request_id: self.analytics.request_id,
            provider: self.analytics.provider.clone(),
            model: self.analytics.requested_model.clone(),
            real_model: self.analytics.real_model.clone(),
            stream: true,
            status_code: terminal.status_code,
            duration_ms,
            error: terminal.error,
            timestamp: completed_at,
            reasoning_effort: self.analytics.reasoning_effort.clone(),
            upstream_duration_ms: terminal.upstream_duration_ms,
            ttft_ms: terminal.ttft_ms,
            // Mirror the normal success row: the flat cache / reasoning
            // columns carry the same values as `usage` (the persistence layer
            // binds these columns from the flat fields). They stay NULL on the
            // error / cancel terminals because `usage` is `None` there.
            cached_tokens: terminal.usage.as_ref().and_then(|u| u.cached_tokens),
            cache_creation_tokens: terminal
                .usage
                .as_ref()
                .and_then(|u| u.cache_creation_tokens),
            reasoning_tokens: terminal.usage.as_ref().and_then(|u| u.reasoning_tokens),
            usage: terminal.usage,
            user_agent: self.analytics.user_agent.clone(),
            client_ip: self.analytics.client_ip.clone(),
            error_category: terminal.error_category,
            // Legacy call_logs keeps its diagnostic role: formal identity
            // columns stay NULL here; M7 protocol/lane dimensions come
            // from the shared stream analytics when dispatch wired them.
            route_id: None,
            target_id: None,
            ingress_protocol: self.analytics.ingress_protocol.clone(),
            upstream_protocol: self.analytics.upstream_protocol.clone(),
            lane: self.analytics.lane.clone(),
            loss_summary: self.analytics.loss_summary.clone(),
            session_key: self.analytics.session_key.clone(),
        });
    }
}

/// Per-response mutable state carried through the `unfold` loop of
/// [`build_streaming_response`].
struct StreamState<S, E> {
    stream: S,
    /// Ingress-protocol encoder: event → client-visible SSE bytes.
    /// `Raw` must pass through original bytes; structured events are
    /// protocol-encoded (M5+).
    encode: E,
    error: Option<String>,
    done: bool,
    collector: Arc<CallLogCollector>,
    start: Instant,
    reasoning_effort: Option<String>,
    user_agent: Option<String>,
    client_ip: Option<String>,
    /// M8 LossInventory summary for the terminal `call_logs` row.
    loss_summary: Option<String>,
    /// Hashed session identity for the terminal `call_logs` row.
    session_key: Option<String>,
    // Declared before the guard so it drops first; the guard's own
    // `Arc` clone keeps the shared data alive regardless. The guard
    // field is never read — it exists solely for its `Drop` (client
    // disconnect terminal), hence the underscore name.
    analytics: Arc<StreamAnalytics>,
    _guard: TerminalGuard,
}

/// Construct the shared [`StreamAnalytics`] for one streamed response from
/// the dispatch args / app state.
#[allow(clippy::too_many_arguments)]
fn make_stream_analytics(
    state: &AppState,
    trace_id: uuid::Uuid,
    route_name: &str,
    provider_name: &str,
    model: &str,
    real_model: &str,
    started_at: chrono::DateTime<chrono::Utc>,
    target_started_at: chrono::DateTime<chrono::Utc>,
    target_facts: Vec<TargetAttemptFact>,
    start: Instant,
    upstream_start: Instant,
    reasoning_effort: Option<String>,
    user_agent: Option<String>,
    client_ip: Option<String>,
    ingress_protocol: Option<String>,
    upstream_protocol: Option<String>,
    lane: Option<String>,
    loss_summary: Option<String>,
    session_key: Option<String>,
) -> Arc<StreamAnalytics> {
    Arc::new(StreamAnalytics {
        event_collector: state.event_collector.clone(),
        request_id: trace_id,
        route_name: route_name.to_string(),
        requested_model: model.to_string(),
        provider: provider_name.to_string(),
        real_model: real_model.to_string(),
        ingress_protocol,
        upstream_protocol,
        lane,
        started_at,
        target_started_at,
        prior_target_facts: target_facts,
        done: AtomicBool::new(false),
        error: AtomicBool::new(false),
        error_message: Mutex::new(None),
        call_log_collector: state.collector.clone(),
        start,
        reasoning_effort,
        user_agent,
        client_ip,
        loss_summary,
        session_key,
        terminal_frame_forwarded: AtomicBool::new(false),
        usage: Mutex::new(Usage::default()),
        first_byte_at: Mutex::new(None),
        upstream_start,
    })
}

/// Construct the [`StreamState`] that seeds the `unfold` loop, wiring the
/// client-disconnect [`TerminalGuard`].
#[allow(clippy::too_many_arguments)]
fn make_stream_state<S, E>(
    stream: S,
    encode: E,
    collector: Arc<CallLogCollector>,
    analytics: Arc<StreamAnalytics>,
    start: Instant,
    reasoning_effort: Option<String>,
    user_agent: Option<String>,
    client_ip: Option<String>,
    loss_summary: Option<String>,
    session_key: Option<String>,
) -> StreamState<S, E> {
    StreamState {
        stream,
        encode,
        error: None,
        done: false,
        collector,
        start,
        reasoning_effort,
        user_agent,
        client_ip,
        loss_summary,
        session_key,
        analytics: analytics.clone(),
        _guard: TerminalGuard { analytics },
    }
}

/// Accumulate one `Usage` frame into the running total.
///
/// Token-count heuristics & aggregation strategy:
/// 1. prompt_tokens: Take maximum across frames because providers report cumulative
///    prompt_tokens (handling GLM echoes as well as Anthropic tool-use/web-search
///    prompt token expansion).
/// 2. completion_tokens: Take maximum across frames because providers (including Anthropic
///    message_start + message_delta) report cumulative completion_tokens in usage events.
/// 3. total_tokens: Respect upstream's reported total_tokens when specified, while
///    ensuring total_tokens is at least prompt_tokens + completion_tokens.
fn merge_stream_usage(acc: &mut Usage, frame: &Usage) {
    if frame.prompt_tokens > 0 {
        acc.prompt_tokens = acc.prompt_tokens.max(frame.prompt_tokens);
    }
    acc.completion_tokens = acc.completion_tokens.max(frame.completion_tokens);

    let derived_total = acc.prompt_tokens.saturating_add(acc.completion_tokens);

    acc.total_tokens = if frame.total_tokens > 0 {
        acc.total_tokens.max(frame.total_tokens).max(derived_total)
    } else {
        derived_total
    };
    // Cache/reasoning fields take the last non-None value.
    if frame.cached_tokens.is_some() {
        acc.cached_tokens = frame.cached_tokens;
    }
    if frame.cache_creation_tokens.is_some() {
        acc.cache_creation_tokens = frame.cache_creation_tokens;
    }
    if frame.reasoning_tokens.is_some() {
        acc.reasoning_tokens = frame.reasoning_tokens;
    }
}

/// Record a mid-stream `Error` chunk: stash the message for the terminal
/// `call_logs` row and flag the shared analytics state —
///
/// Shared with the drop guard: an abandoned stream after
/// an upstream error classifies as `stream_error_after_commit`,
/// not `client_cancelled`, and the call_logs `error` column
/// carries the upstream message.
fn record_stream_error<S, E>(state: &mut StreamState<S, E>, msg: String) {
    // This runs while the response body is polled by hyper — outside the
    // request span — so the correlation identity is recorded explicitly.
    warn!(
        request_id = %state.analytics.request_id,
        provider = %state.analytics.provider,
        model = %state.analytics.requested_model,
        real_model = %state.analytics.real_model,
        error = %msg,
        "mid-stream error"
    );
    state.error = Some(msg.clone());
    state.analytics.error.store(true, Ordering::SeqCst);
    *state
        .analytics
        .error_message
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(msg);
}

/// Build the terminal `call_logs` row of the normal `Done`/`None` path.
/// `error` is the taken mid-stream error (if any).
///
/// `upstream_duration_ms` / `ttft_ms` are the success-only payload: on the
/// mid-stream error terminal every flat timing / token / cache column stays
/// NULL — together with `usage` — so a failed row can never feed the
/// latency coverage denominators or be mistaken for a success sample.
/// `duration_ms` (route wall-clock) still carries the total request time
/// for diagnostics.
fn build_stream_call_log<S, E>(
    state: &StreamState<S, E>,
    error: Option<String>,
    upstream_duration_ms: u64,
    ttft_ms: Option<u64>,
    usage: &Usage,
) -> CallLog {
    let had_error = error.is_some();
    let status_code = if had_error { 502 } else { 200 };
    // Mirror success_log (mod.rs:151-153): the flat
    // cache/reasoning columns must carry the same values as
    // `usage`, because the persistence layer binds these
    // columns from the flat fields, not from `log.usage`.
    // Without this, query_stats undercounts streaming
    // responses. Capture before the move into `Some(state.usage)`.
    // On the error terminal they revert to NULL (the same
    // failure contract as the flat token columns).
    let cached_tokens = if had_error { None } else { usage.cached_tokens };
    let cache_creation_tokens = if had_error {
        None
    } else {
        usage.cache_creation_tokens
    };
    let reasoning_tokens = if had_error {
        None
    } else {
        usage.reasoning_tokens
    };
    CallLog {
        request_id: state.analytics.request_id,
        provider: state.analytics.provider.clone(),
        model: state.analytics.requested_model.clone(),
        real_model: state.analytics.real_model.clone(),
        stream: true,
        status_code,
        duration_ms: state.start.elapsed().as_millis() as u64,
        error,
        timestamp: chrono::Utc::now(),
        reasoning_effort: state.reasoning_effort.clone(),
        upstream_duration_ms: if had_error {
            None
        } else {
            Some(upstream_duration_ms)
        },
        ttft_ms: if had_error { None } else { ttft_ms },
        cached_tokens,
        cache_creation_tokens,
        reasoning_tokens,
        usage: if had_error { None } else { Some(usage.clone()) },
        user_agent: state.user_agent.clone(),
        client_ip: state.client_ip.clone(),
        error_category: if status_code == 200 {
            None
        } else {
            Some("stream_error".to_string())
        },
        // Legacy call_logs keep its diagnostic role: the
        // formal identity columns stay NULL here; the
        // request_results / target_attempts facts resolve
        // identity via route_name / provider / real_model.
        // M7 protocol/lane dimensions come from the shared
        // stream analytics (present whenever dispatch wired them).
        route_id: None,
        target_id: None,
        ingress_protocol: state.analytics.ingress_protocol.clone(),
        upstream_protocol: state.analytics.upstream_protocol.clone(),
        lane: state.analytics.lane.clone(),
        loss_summary: state.loss_summary.clone(),
        session_key: state.session_key.clone(),
    }
}

/// Claim the stream terminal and write both the `call_logs` row and the
/// formal analytics terminal event for the normal `Done`/`None` path.
/// Call order is load-bearing: mark `done` → `CallLog` → `terminal_event`
/// → deliver → return.
async fn finish_stream_terminal<S, E>(state: &mut StreamState<S, E>) {
    state.done = true;
    // Claim the terminal *before* writing the call_logs row
    // or delivering the event: if log construction panics or
    // this body future is dropped mid-terminal, the drop
    // guard sees `done` and must not emit a second,
    // conflicting request result or a duplicate call_logs
    // row.
    state.analytics.done.store(true, Ordering::SeqCst);
    let error = state.error.take();
    let had_error = error.is_some();
    // `upstream_duration_ms` / `ttft_ms` mark *successful*
    // stream timing: they are the success-only payload, the
    // same contract the formal analytics fact follows (see
    // `terminal_event`). On the mid-stream error terminal the
    // call_logs flat columns stay NULL — together with the
    // token / cache columns — so a failed row can never feed
    // the latency coverage denominators or be mistaken for a
    // success sample. `duration_ms` (route wall-clock) still
    // carries the total request time for diagnostics.
    let upstream_duration_ms = state.analytics.upstream_start.elapsed().as_millis() as u64;
    let first_byte_at = *state
        .analytics
        .first_byte_at
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let ttft_ms =
        first_byte_at.map(|t| t.duration_since(state.analytics.upstream_start).as_millis() as u64);
    // Also shared by the analytics success fact below — the
    // fact's usage must be the *final* accumulated usage,
    // not a mid-stream partial frame.
    let usage = state
        .analytics
        .usage
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    state.collector.log(build_stream_call_log(
        state,
        error,
        upstream_duration_ms,
        ttft_ms,
        &usage,
    ));

    // ── Formal analytics terminal event ──
    let (result_status, result_reason) = if had_error {
        (
            FinalStatus::ModelFailure,
            Some(FailureReason::StreamErrorAfterCommit),
        )
    } else {
        (FinalStatus::Success, None)
    };
    let (target_status, target_reason) = if had_error {
        (
            TargetStatus::Failure,
            Some(FailureReason::StreamErrorAfterCommit),
        )
    } else {
        (TargetStatus::Success, None)
    };
    let completed_at = chrono::Utc::now();
    let event = terminal_event(
        &state.analytics,
        result_status,
        result_reason,
        completed_at,
        // Success-only payload: usage / cache / TTFT /
        // upstream timing come from StreamState; failure
        // keeps every token / timing field NULL.
        if had_error { None } else { Some(usage) },
        if had_error { None } else { ttft_ms },
        if had_error {
            None
        } else {
            Some(upstream_duration_ms)
        },
        target_status,
        target_reason,
    );
    // Deliver the terminal outside this body future's cancellation
    // scope: dropping the body while the queue is full must not cancel
    // the enqueue — the drop guard skips (the claim above), so this is
    // the only delivery for the normal terminal. The fast path enqueues
    // synchronously; under backpressure awaiting the task binds a normal
    // completion to the enqueue (event is in the channel before the body
    // ends). Cancelling that await (body drop) cannot cancel the task.
    if let Some(task) = deliver_terminal_event(state.analytics.event_collector.clone(), event)
        && let Err(e) = task.await
    {
        warn!(error = ?e, "analytics terminal delivery task panicked");
    }
}

/// Wrap an [`IrStreamEvent`] stream in an SSE `Response`, encoding each
/// client-visible event via `encode` and accumulating `Usage` / `Error`
/// events into a final `CallLog` when the stream ends.
///
/// `encode` maps one event to client-facing bytes. For the Chat ingress it
/// must forward [`IrStreamEvent::Raw`] **byte-identically** (the original
/// upstream SSE frame) and render structured events via the ingress codec.
/// `Usage` / `Error` / `End` are handled inline by the loop (merge /
/// record / finish) and are never re-encoded into the body — matching the
/// legacy `StreamChunk` contract (only Data bytes reach the client;
/// `[DONE]` itself arrives as a Raw Data frame).
///
/// `upstream_start` is captured immediately before the provider call. It
/// drives two timing fields on the final `CallLog`:
/// - `upstream_duration_ms` — wall-clock from start to stream end.
/// - `ttft_ms` — wall-clock to the first client-visible byte, or `None`
///   if the stream emitted only metadata.
///
/// ## Formal analytics terminal event
///
/// Every streamed request emits exactly one [`sag_analytics::AnalyticsEvent`]
/// (the EventWriter's `request_id` idempotency is the backstop against
/// duplicates):
///
/// - normal `End`/`None` without a mid-stream error → `Success` with the
///   final accumulated usage / cache / TTFT / upstream timing, plus a
///   `Success` target fact for this streamed target;
/// - normal `End`/`None` after a mid-stream `Error` → `ModelFailure` /
///   `stream_error_after_commit` (no fallback after commit; the HTTP 200 /
///   truncated-SSE legacy protocol behavior is unchanged), with a
///   `Failure` target fact; tokens stay NULL;
/// - the response body dropped before a normal terminal — the axum/hyper
///   client-disconnect signal — is classified by the shared terminal state:
///   a mid-stream error already observed → `ModelFailure` /
///   `stream_error_after_commit`; no error but a client-visible terminal
///   frame already forwarded → `Success` with the shared usage / TTFT /
///   upstream timing (the client may close after `[DONE]`; the drop is
///   upstream FIN latency, not a cancel); otherwise `ClientCancelled` /
///   `client_cancelled` with a `Cancelled` target fact. Failure / cancel
///   keep tokens NULL; no fallback.
///
/// The terminal event is delivered by `deliver_terminal_event`, which
/// decouples the enqueue from the body future's cancellation lifecycle:
/// the common path enqueues synchronously (`try_emit` — nothing to cancel,
/// nothing to drain later), and only when the queue is genuinely full does
/// a detached task own the backpressured `emit`, surviving a body drop
/// (client disconnect) mid-delivery and completing before graceful
/// shutdown drains the collector.
///
/// The terminal event also carries the failure facts of targets that fell
/// through before this stream committed (see `target_facts`). Route latency
/// spans `started_at` → the terminal wall-clock instant; the target fact's
/// `started_at` is the wall-clock start of this target's first provider
/// call, and both facts share the same terminal `completed_at`.
#[allow(clippy::too_many_arguments)]
pub fn build_streaming_response<S, E>(
    state: &AppState,
    chunk_stream: S,
    encode: E,
    route_name: &str,
    provider_name: &str,
    model: &str,
    real_model: &str,
    trace_id: uuid::Uuid,
    start: Instant,
    upstream_start: Instant,
    started_at: chrono::DateTime<chrono::Utc>,
    target_started_at: chrono::DateTime<chrono::Utc>,
    target_facts: Vec<TargetAttemptFact>,
    reasoning_effort: Option<String>,
    user_agent: Option<String>,
    client_ip: Option<String>,
    ingress_protocol: Option<String>,
    upstream_protocol: Option<String>,
    lane: Option<String>,
    loss_summary: Option<String>,
    session_key: Option<String>,
) -> Response<Body>
where
    // `+ Unpin` is required by `StreamExt::next().await` inside the
    // unfold body. All upstream provider adapters already return
    // `Pin<Box<dyn Stream + Send>>` (boxed at the provider boundary),
    // which satisfies this bound — non-`Unpin` streams would need to
    // be boxed by the caller first.
    S: Stream<Item = IrStreamEvent> + Send + Unpin + 'static,
    E: Fn(&IrStreamEvent) -> Vec<u8> + Send + 'static,
{
    let analytics = make_stream_analytics(
        state,
        trace_id,
        route_name,
        provider_name,
        model,
        real_model,
        started_at,
        target_started_at,
        target_facts,
        start,
        upstream_start,
        reasoning_effort.clone(),
        user_agent.clone(),
        client_ip.clone(),
        ingress_protocol,
        upstream_protocol,
        lane,
        loss_summary.clone(),
        session_key.clone(),
    );
    let state = make_stream_state(
        chunk_stream,
        encode,
        state.collector.clone(),
        analytics,
        start,
        reasoning_effort,
        user_agent,
        client_ip,
        loss_summary,
        session_key,
    );

    let body = Body::from_stream(unfold(state, |mut state| async move {
        if state.done {
            return None;
        }
        loop {
            match state.stream.next().await {
                // Usage / Error / End are protocol-control events: merge /
                // record / finish inline — never encoded into the client body
                // (legacy StreamChunk contract: only Data bytes reach the
                // client; [DONE] itself arrives as a Raw Data frame).
                Some(IrStreamEvent::Usage(u)) => {
                    let mut acc = state
                        .analytics
                        .usage
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    merge_stream_usage(&mut acc, &u);
                }
                Some(IrStreamEvent::Error(msg)) => {
                    record_stream_error(&mut state, msg);
                }
                Some(IrStreamEvent::End) | None => {
                    finish_stream_terminal(&mut state).await;
                    return None;
                }
                Some(ev) => {
                    // Client-visible event (Raw / structured): encode and
                    // stamp TTFT on the first non-empty yielded byte —
                    // a 0-byte encode is not a first byte.
                    let bytes = (state.encode)(&ev);
                    if bytes.is_empty() {
                        continue;
                    }
                    // Single lock: check-and-stamp TTFT on the first
                    // non-empty yielded byte (a 0-byte encode is not a
                    // first byte).
                    {
                        let mut first_byte_at = state
                            .analytics
                            .first_byte_at
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if first_byte_at.is_none() {
                            *first_byte_at = Some(Instant::now());
                        }
                    }
                    // Judged on the *encoded* bytes: the Responses
                    // translated lane synthesizes its terminal frame from
                    // a structured `Finish` event, so a Raw-only check
                    // would miss that whole lane. The flag is set before
                    // the yield, so a disconnect hyper detects afterwards
                    // cannot race it.
                    if sag_core::is_client_terminal_frame(&bytes) {
                        state
                            .analytics
                            .terminal_frame_forwarded
                            .store(true, Ordering::SeqCst);
                    }
                    return Some((
                        Ok::<_, std::io::Error>(axum::body::Bytes::from(bytes)),
                        state,
                    ));
                }
            }
        }
    }));

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        // `no-transform` + `X-Accel-Buffering: no` keep intermediate
        // proxies (nginx / Cloudflare) from buffering or re-compressing
        // the event stream, which would stall token delivery.
        .header("Cache-Control", "no-cache, no-transform")
        .header("X-Accel-Buffering", "no")
        .header("Connection", "keep-alive")
        .body(body)
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use futures_util::stream;
    use sag_analytics::{LogFilter, StatsFilter};
    use sag_config::{ConfigHandle, GatewayAuthConfig, GatewayConfig};
    use sag_core::{StreamChunk, Usage};
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::RwLock;
    use uuid::Uuid;

    use super::super::tests::{build_state_with_event_pool, build_state_with_event_queue_capacity};
    use http_body_util::BodyExt;

    async fn build_state() -> AppState {
        let config = GatewayConfig {
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["k".into()],
            }),
            ..sag_config::gateway_config_base!()
        };
        let handle = ConfigHandle::from_config(config);
        let registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
        crate::testing::test_app_state(handle, Arc::new(RwLock::new(registry))).await
    }

    #[allow(dead_code)]
    fn chunk_stream(chunks: Vec<StreamChunk>) -> Pin<Box<dyn Stream<Item = StreamChunk> + Send>> {
        Box::pin(stream::iter(chunks))
    }

    /// Map legacy [`StreamChunk`] fixtures onto the IR event stream the
    /// builder now consumes (`Data` → `Raw`, `Done` → `End`, …).
    fn ir_stream(chunks: Vec<StreamChunk>) -> Pin<Box<dyn Stream<Item = IrStreamEvent> + Send>> {
        Box::pin(stream::iter(
            chunks
                .into_iter()
                .map(IrStreamEvent::from)
                .collect::<Vec<_>>(),
        ))
    }

    /// Chat ingress encoder under test: Raw passes through; structured
    /// events / control frames use the Chat codec. (`End` is never encoded
    /// by the loop — it finishes the terminal — but the fn stays total.)
    fn encode_chat(ev: &IrStreamEvent) -> Vec<u8> {
        match ev {
            IrStreamEvent::Raw(b) => b.clone(),
            other => sag_providers::ChatCompletionsCodec::encode_stream_event(other),
        }
    }

    /// Build a streaming response with default identity / timing args
    /// (route `"test-route"`, `real_model` = model, fresh `trace_id`,
    /// current instants, no prior target facts). Tests that need specific
    /// identity / timing / reasoning args call `build_streaming_response`
    /// directly.
    fn stream_response(
        state: &AppState,
        chunks: Vec<StreamChunk>,
        provider: &str,
        model: &str,
    ) -> axum::response::Response<axum::body::Body> {
        build_streaming_response(
            state,
            ir_stream(chunks),
            encode_chat,
            "test-route",
            provider,
            model,
            model,
            Uuid::new_v4(),
            Instant::now(),
            Instant::now(),
            chrono::Utc::now(),
            chrono::Utc::now(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }

    /// Drain the collector and return the most recently inserted CallLog.
    async fn latest_call_log(state: &AppState) -> sag_core::CallLog {
        state.collector.flush().await;
        let mut logs = state
            .collector
            .store()
            .query_logs(LogFilter {
                limit: 1,
                ..Default::default()
            })
            .await
            .unwrap();
        logs.pop().expect("at least one log row")
    }

    /// A valid filler event (success + a successful attempt) used to
    /// occupy the collector queue in backpressure tests. It passes the
    /// writer's validation so the drain stays fast; its `request_id` is
    /// unique and never queried.
    fn filler_event() -> sag_analytics::AnalyticsEvent {
        let now = chrono::Utc::now();
        sag_analytics::AnalyticsEvent {
            result: RequestResultFact {
                request_id: Uuid::new_v4(),
                route_name: Some("fill-route".into()),
                requested_model: "fill-model".into(),
                final_status: FinalStatus::Success,
                failure_reason: None,
                stream: false,
                started_at: now,
                completed_at: now,
                usage: None,
                cached_tokens: None,
                cache_creation_tokens: None,
                ttft_ms: None,
                upstream_duration_ms: None,
                ingress_protocol: None,
                lane: None,
            },
            targets: vec![TargetAttemptFact {
                provider: "openai".into(),
                real_model: "fill-model".into(),
                status: TargetStatus::Success,
                failure_reason: None,
                started_at: now,
                completed_at: now,
                usage: None,
                cached_tokens: None,
                ttft_ms: None,
                upstream_duration_ms: None,
                upstream_protocol: None,
                lane: None,
            }],
        }
    }

    /// Fill the collector's queue until `try_emit` refuses. The caller
    /// holds the pool's only connection (blocking the writer), so the
    /// queue cannot drain between iterations: a refused `try_emit` means
    /// the queue is genuinely full, and every later terminal delivery
    /// must take the backpressure (spawned-task) path.
    fn fill_tiny_queue(state: &AppState) {
        let mut fills = 0;
        while state.event_collector.try_emit(filler_event()).is_ok() {
            fills += 1;
            assert!(fills < 64, "queue must fill while the writer is blocked");
        }
        assert!(
            fills >= 1,
            "at least one filler event must occupy the queue"
        );
    }

    #[tokio::test]
    async fn data_chunks_are_forwarded_to_body() {
        let state = build_state().await;
        let chunks = vec![
            StreamChunk::Data(b"data: chunk-1\n\n".to_vec()),
            StreamChunk::Data(b"data: chunk-2\n\n".to_vec()),
            StreamChunk::Done,
        ];
        let resp = stream_response(&state, chunks, "openai", "gpt-4o");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("Content-Type").unwrap(),
            "text/event-stream"
        );

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"data: chunk-1\n\ndata: chunk-2\n\n");
    }

    /// Regression: SSE responses must carry the anti-buffering header set
    /// so an outer proxy does not buffer or transcode the stream.
    #[tokio::test]
    async fn sse_response_sets_anti_buffering_headers() {
        let state = build_state().await;
        let resp = stream_response(
            &state,
            vec![
                StreamChunk::Data(b"data: hi\n\n".to_vec()),
                StreamChunk::Done,
            ],
            "openai",
            "gpt-4o",
        );
        let headers = resp.headers();
        assert_eq!(headers.get("Content-Type").unwrap(), "text/event-stream");
        assert_eq!(
            headers.get("Cache-Control").unwrap(),
            "no-cache, no-transform"
        );
        assert_eq!(headers.get("X-Accel-Buffering").unwrap(), "no");
        assert_eq!(headers.get("Connection").unwrap(), "keep-alive");
    }

    #[tokio::test]
    async fn usage_chunks_are_accumulated_into_call_log() {
        let state = build_state().await;
        let chunks = vec![
            StreamChunk::Data(b"data: hi\n\n".to_vec()),
            StreamChunk::Usage(Usage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            StreamChunk::Usage(Usage {
                prompt_tokens: 5,
                completion_tokens: 0,
                total_tokens: 5,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            StreamChunk::Done,
        ];
        let resp = stream_response(&state, chunks, "openai", "gpt-4o");
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        // CallLog should record the max cumulative usage across chunks: max(10,5)=10 prompt, 20 completion, 30 total
        state.collector.flush().await;
        let rows = state
            .collector
            .store()
            .query_stats(StatsFilter::default())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, "openai");
        assert_eq!(rows[0].call_count, 1);
        assert_eq!(rows[0].error_count, 0);
        assert_eq!(rows[0].total_prompt_tokens, 10);
        assert_eq!(rows[0].total_completion_tokens, 20);
        assert_eq!(rows[0].total_tokens, 30);
    }

    /// Regression for the streaming cache-passthrough bug: the flat
    /// `cached_tokens` / `cache_creation_tokens` / `reasoning_tokens`
    /// columns on the stream `CallLog` must carry the same values as
    /// `usage`, because `SqliteLogStore::insert_batch` binds these
    /// columns from the flat fields (not from `log.usage`). Without
    /// this, `query_stats` undercounts every streamed response.
    #[tokio::test]
    async fn cache_fields_persisted_from_stream_usage() {
        let state = build_state().await;
        let chunks = vec![
            StreamChunk::Data(b"data: hi\n\n".to_vec()),
            StreamChunk::Usage(Usage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
                cached_tokens: Some(5),
                cache_creation_tokens: Some(3),
                reasoning_tokens: Some(7),
            }),
            StreamChunk::Done,
        ];
        let resp = stream_response(&state, chunks, "anthropic", "claude");
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        let log = latest_call_log(&state).await;
        assert_eq!(log.cached_tokens, Some(5));
        assert_eq!(log.cache_creation_tokens, Some(3));
        assert_eq!(log.reasoning_tokens, Some(7));
    }

    #[tokio::test]
    async fn mid_stream_error_sets_call_log_status_502() {
        let state = build_state().await;
        let chunks = vec![
            StreamChunk::Data(b"data: partial\n\n".to_vec()),
            StreamChunk::Error("upstream died".into()),
            StreamChunk::Done,
        ];
        let resp = stream_response(&state, chunks, "openai", "gpt-4o");
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        state.collector.flush().await;
        let rows = state
            .collector
            .store()
            .query_stats(StatsFilter::default())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].error_count, 1,
            "mid-stream error should bump error_count"
        );
        assert_eq!(rows[0].call_count, 1);
    }

    #[tokio::test]
    async fn clean_done_terminates_stream_with_status_200() {
        let state = build_state().await;
        let chunks = vec![
            StreamChunk::Data(b"data: ok\n\n".to_vec()),
            StreamChunk::Done,
        ];
        let resp = stream_response(&state, chunks, "openai", "gpt-4o");
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        state.collector.flush().await;
        let rows = state
            .collector
            .store()
            .query_stats(StatsFilter::default())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].call_count, 1);
        assert_eq!(rows[0].error_count, 0);
    }

    #[tokio::test]
    async fn empty_stream_terminates_immediately() {
        let state = build_state().await;
        let resp = stream_response(&state, vec![], "openai", "gpt-4o");
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert!(body.is_empty(), "empty stream should yield empty body");

        // CallLog should still be recorded with default (zero) usage
        state.collector.flush().await;
        let rows = state
            .collector
            .store()
            .query_stats(StatsFilter::default())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].call_count, 1);
        assert_eq!(rows[0].error_count, 0);
    }

    #[tokio::test]
    async fn none_terminates_stream_just_like_done() {
        // The `unfold` body matches `StreamChunk::Done | None` identically.
        // This test covers the `None` arm: a stream that yields some data
        // then just ends (no explicit `Done`).
        let state = build_state().await;
        let chunks = vec![StreamChunk::Data(b"data: partial\n\n".to_vec())];
        let resp = stream_response(&state, chunks, "openai", "gpt-4o");
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"data: partial\n\n");

        state.collector.flush().await;
        let rows = state
            .collector
            .store()
            .query_stats(StatsFilter::default())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].call_count, 1);
        assert_eq!(rows[0].error_count, 0);
    }

    // ─── Plan §2.3 — TTFT and upstream_duration_ms population ──────

    /// Stream with at least one `Data` chunk before `Done`/`None`:
    /// `ttft_ms` must be `Some` (and ≤ upstream_duration_ms),
    /// `upstream_duration_ms` must be `Some`.
    #[tokio::test]
    async fn ttft_and_upstream_duration_populated_when_data_chunk_seen() {
        let state = build_state().await;
        let chunks = vec![
            StreamChunk::Data(b"data: hello\n\n".to_vec()),
            StreamChunk::Done,
        ];
        let resp = build_streaming_response(
            &state,
            ir_stream(chunks),
            encode_chat,
            "test-route",
            "openai",
            "gpt-4o",
            "gpt-4o",
            Uuid::new_v4(),
            Instant::now(),
            // upstream_start recorded significantly before the stream
            // completes — guarantees a positive upstream_duration_ms.
            Instant::now() - std::time::Duration::from_millis(50),
            chrono::Utc::now(),
            chrono::Utc::now(),
            Vec::new(),
            Some("medium".into()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        let log = latest_call_log(&state).await;
        let upstream = log
            .upstream_duration_ms
            .expect("upstream_duration_ms must be set");
        let ttft = log
            .ttft_ms
            .expect("ttft_ms must be set when a Data chunk is seen");
        assert!(
            ttft <= upstream,
            "TTFT ({ttft}) must be ≤ upstream_duration_ms ({upstream})"
        );
        assert!(
            upstream >= 50,
            "upstream_duration_ms ({upstream}) should reflect the 50ms upstream_start offset"
        );
        assert_eq!(
            log.reasoning_effort.as_deref(),
            Some("medium"),
            "reasoning_effort must round-trip through the stream CallLog"
        );
    }

    /// Stream with ONLY Usage/Error/Done metadata chunks (no
    /// `StreamChunk::Data`): `ttft_ms` stays `None` (no observable
    /// user-visible first byte), but `upstream_duration_ms` is still
    /// populated (wall-clock end-to-end elapsed).
    #[tokio::test]
    async fn ttft_is_none_when_stream_has_no_data_chunk() {
        let state = build_state().await;
        let chunks = vec![
            StreamChunk::Usage(Usage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            StreamChunk::Done,
        ];
        let resp = stream_response(&state, chunks, "openai", "gpt-4o");
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        let log = latest_call_log(&state).await;
        assert!(
            log.upstream_duration_ms.is_some(),
            "upstream_duration_ms must still be set for metadata-only streams"
        );
        assert!(
            log.ttft_ms.is_none(),
            "ttft_ms must remain None when no Data chunk was emitted"
        );
    }

    #[tokio::test]
    async fn multiple_usage_chunks_max_aggregated_without_double_counting() {
        let state = build_state().await;
        let chunks = vec![
            // Chunk 1: Intermediate usage with prompt_tokens = 100
            StreamChunk::Usage(Usage {
                prompt_tokens: 100,
                completion_tokens: 0,
                total_tokens: 100,
                cached_tokens: Some(30),
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            // Chunk 2: Final usage repeating prompt_tokens = 100 (cumulative) and adding completion_tokens = 50
            StreamChunk::Usage(Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: Some(20),
            }),
            StreamChunk::Done,
        ];
        let resp = stream_response(&state, chunks, "openai", "gpt-4o");
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        let log = latest_call_log(&state).await;
        let usage = log.usage.expect("usage must be set in call log");
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.total_tokens, 150);
        assert_eq!(usage.cached_tokens, Some(30));
        assert_eq!(usage.reasoning_tokens, Some(20));
    }

    #[tokio::test]
    async fn anthropic_cumulative_usage_chunks_max_aggregated() {
        let state = build_state().await;
        let chunks = vec![
            // Chunk 1: message_start with prompt_tokens=25, output_tokens=1
            StreamChunk::Usage(Usage {
                prompt_tokens: 25,
                completion_tokens: 1,
                total_tokens: 26,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            // Chunk 2: message_delta with cumulative output_tokens=15
            StreamChunk::Usage(Usage {
                prompt_tokens: 0,
                completion_tokens: 15,
                total_tokens: 15,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            StreamChunk::Done,
        ];
        let resp = stream_response(&state, chunks, "anthropic", "claude-3-5-sonnet");

        let _body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        let log = latest_call_log(&state).await;
        let usage = log.usage.expect("usage must be set in call log");
        assert_eq!(usage.prompt_tokens, 25);
        assert_eq!(usage.completion_tokens, 15);
        assert_eq!(usage.total_tokens, 40);
    }

    #[tokio::test]
    async fn anthropic_prompt_tokens_expansion_max_aggregated() {
        let state = build_state().await;
        let chunks = vec![
            // Chunk 1: message_start with initial prompt_tokens=25
            StreamChunk::Usage(Usage {
                prompt_tokens: 25,
                completion_tokens: 1,
                total_tokens: 26,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            // Chunk 2: message_delta with web-search / tool-use expanded prompt_tokens=10682
            StreamChunk::Usage(Usage {
                prompt_tokens: 10682,
                completion_tokens: 510,
                total_tokens: 11192,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            StreamChunk::Done,
        ];
        let resp = stream_response(&state, chunks, "anthropic", "claude-3-5-sonnet");

        let _body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        let log = latest_call_log(&state).await;
        let usage = log.usage.expect("usage must be set in call log");
        assert_eq!(usage.prompt_tokens, 10682);
        assert_eq!(usage.completion_tokens, 510);
        assert_eq!(usage.total_tokens, 11192);
    }

    // ─── Formal analytics terminal event ──────────────────────────

    /// Streaming success must emit exactly one `request_results` row
    /// (`success`, `stream=1`, final accumulated usage / cache, route
    /// identity resolved from `route_name`) plus one success target fact,
    /// and the terminal event must also carry the failure fact of a target
    /// that fell through before commit.
    #[tokio::test]
    async fn stream_success_emits_single_success_event_with_target_fact() {
        let (state, pool) = build_state_with_event_pool(vec![]).await;
        let trace_id = Uuid::new_v4();
        let prior_failure = TargetAttemptFact {
            provider: "first".into(),
            real_model: "gpt-4o".into(),
            status: TargetStatus::Failure,
            failure_reason: Some(FailureReason::ProviderError),
            started_at: chrono::Utc::now() - chrono::Duration::seconds(1),
            completed_at: chrono::Utc::now(),
            usage: None,
            cached_tokens: None,
            ttft_ms: None,
            upstream_duration_ms: None,
            upstream_protocol: None,
            lane: None,
        };
        let chunks = vec![
            StreamChunk::Data(b"data: hello\n\n".to_vec()),
            StreamChunk::Usage(Usage {
                prompt_tokens: 5,
                completion_tokens: 7,
                total_tokens: 12,
                cached_tokens: Some(3),
                cache_creation_tokens: None,
                reasoning_tokens: Some(1),
            }),
            StreamChunk::Done,
        ];
        let resp = build_streaming_response(
            &state,
            ir_stream(chunks),
            encode_chat,
            "test-route",
            "openai",
            "gpt-4o",
            "gpt-4o-real",
            trace_id,
            Instant::now(),
            Instant::now(),
            chrono::Utc::now(),
            chrono::Utc::now(),
            vec![prior_failure],
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        state.event_collector.shutdown().await;

        // Exactly one request result — no duplicate from the drop guard.
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_results WHERE request_id = ?")
                .bind(trace_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 1, "exactly one request result per streamed request");

        let (final_status, failure_reason, stream, pt, ct, cached): (
            String,
            Option<String>,
            i32,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        ) = sqlx::query_as(
            "SELECT final_status, failure_reason, stream, prompt_tokens, completion_tokens, \
             cached_tokens \
             FROM request_results WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("success fact must be persisted");
        assert_eq!(final_status, "success");
        assert!(failure_reason.is_none());
        assert_eq!(stream, 1, "streamed request must record stream=1");
        assert_eq!(pt, Some(5), "final accumulated usage enters the fact");
        assert_eq!(ct, Some(7));
        assert_eq!(cached, Some(3));
        let actual_model: Option<String> =
            sqlx::query_scalar("SELECT actual_model FROM request_results WHERE request_id = ?")
                .bind(trace_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            actual_model.as_deref(),
            Some("gpt-4o-real"),
            "actual_model comes from the successful target's real_model"
        );

        // Route identity resolved by the writer from route_name.
        let route_name: String = sqlx::query_scalar(
            "SELECT r.route_name FROM analytics_routes r \
             JOIN request_results rr ON rr.route_id = r.route_id \
             WHERE rr.request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(route_name, "test-route");

        // Two attempts: the prior failure (fallback) + the stream success.
        // `attempt_id` order = the event's targets-vec order (insertion
        // order is deterministic; `started_at` has millisecond ties).
        let rows: Vec<(String, String, Option<String>, Option<i64>)> = sqlx::query_as(
            "SELECT rt.provider, t.status, t.failure_reason, t.prompt_tokens \
             FROM target_attempts t \
             JOIN analytics_route_targets rt ON rt.target_id = t.target_id \
             WHERE t.request_id = ? ORDER BY t.attempt_id",
        )
        .bind(trace_id.to_string())
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2, "fallback failure + stream success");
        assert_eq!(rows[0].0, "first");
        assert_eq!(rows[0].1, "failure");
        assert_eq!(rows[0].2.as_deref(), Some("provider_error"));
        assert!(rows[0].3.is_none(), "failed attempt tokens stay NULL");
        assert_eq!(rows[1].0, "openai");
        assert_eq!(rows[1].1, "success");
        assert!(rows[1].2.is_none());
        assert_eq!(rows[1].3, Some(5), "success attempt carries the usage");
    }

    /// Mid-stream `Error` after a committed `Data` chunk: no fallback, the
    /// terminal event is `model_failure` / `stream_error_after_commit`, the
    /// target attempt is `failure`, and tokens stay NULL — mid-stream
    /// partial usage must never enter a non-success fact.
    #[tokio::test]
    async fn stream_error_after_commit_emits_model_failure_with_null_tokens() {
        let (state, pool) = build_state_with_event_pool(vec![]).await;
        let trace_id = Uuid::new_v4();
        let chunks = vec![
            StreamChunk::Data(b"data: partial\n\n".to_vec()),
            StreamChunk::Usage(Usage {
                prompt_tokens: 5,
                completion_tokens: 7,
                total_tokens: 12,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            StreamChunk::Error("upstream died".into()),
            StreamChunk::Done,
        ];
        let resp = build_streaming_response(
            &state,
            ir_stream(chunks),
            encode_chat,
            "test-route",
            "openai",
            "gpt-4o",
            "gpt-4o",
            trace_id,
            Instant::now(),
            Instant::now(),
            chrono::Utc::now(),
            chrono::Utc::now(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        state.event_collector.shutdown().await;

        let (final_status, failure_reason, pt, ct, ttft): (
            String,
            Option<String>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        ) = sqlx::query_as(
            "SELECT final_status, failure_reason, prompt_tokens, completion_tokens, ttft_ms \
             FROM request_results WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("model_failure fact must be persisted");
        assert_eq!(final_status, "model_failure");
        assert_eq!(failure_reason.as_deref(), Some("stream_error_after_commit"));
        assert!(
            pt.is_none() && ct.is_none(),
            "partial usage must not enter a failed request fact"
        );
        assert!(
            ttft.is_none(),
            "failed request fact keeps timing NULL (success-only payload)"
        );

        let (status, t_fr, t_pt): (String, Option<String>, Option<i64>) = sqlx::query_as(
            "SELECT status, failure_reason, prompt_tokens \
             FROM target_attempts WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "failure");
        assert_eq!(t_fr.as_deref(), Some("stream_error_after_commit"));
        assert!(t_pt.is_none(), "failed attempt tokens stay NULL");
    }

    /// Client-disconnect simulation at the axum/hyper body boundary:
    /// dropping the response (and thus the body stream) without reaching a
    /// normal `Done`/`None` terminal must emit exactly one
    /// `client_cancelled` request result plus a `cancelled` target attempt
    /// — even though partial usage was already observed, the cancel fact
    /// keeps tokens NULL.
    #[tokio::test]
    async fn response_drop_emits_client_cancelled_with_cancelled_attempt() {
        let (state, pool) = build_state_with_event_pool(vec![]).await;
        let trace_id = Uuid::new_v4();
        // One committed byte, partial usage, then the stream would keep
        // streaming forever — the client disconnects before any terminal.
        let stream = stream::iter(vec![
            StreamChunk::Data(b"data: partial\n\n".to_vec()),
            StreamChunk::Usage(Usage {
                prompt_tokens: 5,
                completion_tokens: 7,
                total_tokens: 12,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
        ])
        .map(IrStreamEvent::from)
        .chain(stream::pending());
        let resp = build_streaming_response(
            &state,
            stream,
            encode_chat,
            "test-route",
            "openai",
            "gpt-4o",
            "gpt-4o",
            trace_id,
            Instant::now(),
            Instant::now(),
            chrono::Utc::now(),
            chrono::Utc::now(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        // The disconnect: drop the response without polling the body. The
        // stream state (and its drop guard) is dropped with it.
        drop(resp);
        // The guard spawns the emit task on the current runtime; yield once
        // so it enqueues before the collector drains (Drop cannot await).
        tokio::task::yield_now().await;
        state.event_collector.shutdown().await;

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_results WHERE request_id = ?")
                .bind(trace_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 1, "drop guard must emit exactly one request result");

        let (final_status, failure_reason, stream_flag, pt): (
            String,
            Option<String>,
            i32,
            Option<i64>,
        ) = sqlx::query_as(
            "SELECT final_status, failure_reason, stream, prompt_tokens \
             FROM request_results WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("client_cancelled fact must be persisted");
        assert_eq!(final_status, "client_cancelled");
        assert_eq!(failure_reason.as_deref(), Some("client_cancelled"));
        assert_eq!(stream_flag, 1);
        assert!(
            pt.is_none(),
            "cancelled request must not carry partial usage tokens"
        );

        let (status, t_pt): (String, Option<i64>) = sqlx::query_as(
            "SELECT status, prompt_tokens FROM target_attempts WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "cancelled", "in-flight attempt is cancelled");
        assert!(t_pt.is_none(), "cancelled attempt tokens stay NULL");
    }

    /// A client disconnect after a mid-stream upstream error classifies as
    /// `model_failure` / `stream_error_after_commit` (the error takes
    /// precedence over the cancel), with a `failure` attempt.
    #[tokio::test]
    async fn response_drop_after_upstream_error_emits_stream_error_after_commit() {
        let (state, pool) = build_state_with_event_pool(vec![]).await;
        let trace_id = Uuid::new_v4();
        let stream = stream::iter(vec![
            StreamChunk::Data(b"data: partial\n\n".to_vec()),
            StreamChunk::Error("upstream died".into()),
        ])
        .map(IrStreamEvent::from)
        .chain(stream::pending());
        let resp = build_streaming_response(
            &state,
            stream,
            encode_chat,
            "test-route",
            "openai",
            "gpt-4o",
            "gpt-4o",
            trace_id,
            Instant::now(),
            Instant::now(),
            chrono::Utc::now(),
            chrono::Utc::now(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        // Drive the body one frame at a time with a noop waker: the first
        // poll forwards the committed `Data` byte, the second consumes the
        // `Error` chunk and suspends on `pending` — the client disconnects
        // in that suspended state, so the drop guard must classify by the
        // error flag it saw, not as a plain cancel.
        let mut body = resp.into_body();
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let frame = body.frame();
        futures_util::pin_mut!(frame);
        assert!(
            frame.as_mut().poll(&mut cx).is_ready(),
            "first body frame (the committed Data byte) must be ready"
        );
        assert!(
            frame.poll(&mut cx).is_pending(),
            "Error chunk consumed; the stream must be suspended on pending"
        );
        drop(body);
        tokio::task::yield_now().await;
        state.event_collector.shutdown().await;

        let (final_status, failure_reason): (String, Option<String>) = sqlx::query_as(
            "SELECT final_status, failure_reason FROM request_results WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("model_failure fact must be persisted");
        assert_eq!(final_status, "model_failure");
        assert_eq!(
            failure_reason.as_deref(),
            Some("stream_error_after_commit"),
            "an upstream error seen before the drop wins over client_cancelled"
        );

        let status: String =
            sqlx::query_scalar("SELECT status FROM target_attempts WHERE request_id = ?")
                .bind(trace_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "failure");
    }

    // ─── Fix A: cancelled / failed-after-error streams must be visible
    // ─── in the admin logs list (which reads `call_logs` only) ────────

    /// A client disconnect (body dropped) must ALSO persist a `call_logs`
    /// row — alongside the `client_cancelled` request fact — carrying the
    /// cancellation status (499) and `client_cancelled` category. Exactly one
    /// call_logs row per request.
    #[tokio::test]
    async fn drop_guard_writes_call_logs_row_for_client_cancelled() {
        let (state, pool) = build_state_with_event_pool(vec![]).await;
        let trace_id = Uuid::new_v4();
        let stream = stream::iter(vec![
            StreamChunk::Data(b"data: partial\n\n".to_vec()),
            StreamChunk::Usage(Usage {
                prompt_tokens: 5,
                completion_tokens: 7,
                total_tokens: 12,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
        ])
        .map(IrStreamEvent::from)
        .chain(stream::pending());
        let resp = build_streaming_response(
            &state,
            stream,
            encode_chat,
            "test-route",
            "openai",
            "gpt-4o",
            "gpt-4o",
            trace_id,
            Instant::now(),
            Instant::now(),
            chrono::Utc::now(),
            chrono::Utc::now(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        drop(resp);
        tokio::task::yield_now().await;
        state.event_collector.shutdown().await;

        // Request fact: client_cancelled.
        let (final_status, failure_reason): (String, Option<String>) = sqlx::query_as(
            "SELECT final_status, failure_reason FROM request_results WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("client_cancelled fact must be persisted");
        assert_eq!(final_status, "client_cancelled");
        assert_eq!(failure_reason.as_deref(), Some("client_cancelled"));

        // call_logs row: exactly one, cancellation status + category.
        state.collector.flush().await;
        let mut logs = state
            .collector
            .store()
            .query_logs(LogFilter {
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap();
        logs.retain(|l| l.request_id == trace_id);
        assert_eq!(
            logs.len(),
            1,
            "exactly one call_logs row for the cancelled stream"
        );
        assert_eq!(logs[0].status_code, 499, "cancelled row uses 499");
        assert_eq!(logs[0].error_category.as_deref(), Some("client_cancelled"));
        assert_eq!(logs[0].provider, "openai");
        assert_eq!(logs[0].model, "gpt-4o");
        assert_eq!(logs[0].real_model, "gpt-4o");
        assert!(logs[0].stream, "cancelled stream must record stream=1");
        assert!(logs[0].usage.is_none(), "cancelled row keeps usage NULL");
        let err = logs[0].error.as_deref().unwrap_or("");
        assert!(
            err.contains("client disconnected"),
            "cancellation message expected, got: {err}"
        );
    }

    /// A client disconnect after an in-stream upstream error keeps the
    /// failure classification (`model_failure` / `stream_error_after_commit`,
    /// NOT `client_cancelled`) AND writes a `call_logs` row at 502 carrying
    /// the upstream error message.
    #[tokio::test]
    async fn drop_guard_after_in_stream_error_writes_call_logs_row_with_message() {
        let (state, pool) = build_state_with_event_pool(vec![]).await;
        let trace_id = Uuid::new_v4();
        let stream = stream::iter(vec![
            StreamChunk::Data(b"data: partial\n\n".to_vec()),
            StreamChunk::Error("upstream burst error".into()),
        ])
        .map(IrStreamEvent::from)
        .chain(stream::pending());
        let resp = build_streaming_response(
            &state,
            stream,
            encode_chat,
            "test-route",
            "openai",
            "gpt-4o",
            "gpt-4o",
            trace_id,
            Instant::now(),
            Instant::now(),
            chrono::Utc::now(),
            chrono::Utc::now(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        // Drive the body one frame at a time with a noop waker: the first
        // poll forwards the committed Data byte, the second consumes the
        // Error chunk and suspends on `pending` — the client disconnects in
        // that suspended state.
        let mut body = resp.into_body();
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let frame = body.frame();
        futures_util::pin_mut!(frame);
        assert!(
            frame.as_mut().poll(&mut cx).is_ready(),
            "first body frame (the committed Data byte) must be ready"
        );
        assert!(
            frame.poll(&mut cx).is_pending(),
            "Error chunk consumed; the stream must be suspended on pending"
        );
        drop(body);
        tokio::task::yield_now().await;
        state.event_collector.shutdown().await;

        // Request fact: model_failure (the error takes precedence over the
        // cancel — never downgraded to client_cancelled).
        let (final_status, failure_reason): (String, Option<String>) = sqlx::query_as(
            "SELECT final_status, failure_reason FROM request_results WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("model_failure fact must be persisted");
        assert_eq!(final_status, "model_failure");
        assert_eq!(failure_reason.as_deref(), Some("stream_error_after_commit"));

        // call_logs row: exactly one, 502 + the upstream error message.
        state.collector.flush().await;
        let mut logs = state
            .collector
            .store()
            .query_logs(LogFilter {
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap();
        logs.retain(|l| l.request_id == trace_id);
        assert_eq!(
            logs.len(),
            1,
            "exactly one call_logs row for the failed stream"
        );
        assert_eq!(logs[0].status_code, 502);
        assert_eq!(logs[0].error_category.as_deref(), Some("stream_error"));
        assert_eq!(logs[0].error.as_deref(), Some("upstream burst error"));
        assert!(logs[0].usage.is_none(), "failed row keeps usage NULL");
    }

    /// Failure-contract parity between the two terminals of a stream that
    /// already observed an upstream error: the *normal* `Done`/`None`
    /// terminal (client keeps consuming to EOF) must write the same
    /// failure-shaped `call_logs` row as the drop guard — 502 /
    /// `stream_error`, `usage` NULL, every flat token / cache / timing
    /// column NULL — and the same `model_failure` /
    /// `stream_error_after_commit` request fact with NULL usage / timing.
    /// Mid-stream partial usage must never leak into either terminal.
    #[tokio::test]
    #[allow(clippy::type_complexity)]
    async fn stream_error_then_normal_done_keeps_failure_contract() {
        let (state, pool) = build_state_with_event_pool(vec![]).await;
        let trace_id = Uuid::new_v4();
        let chunks = vec![
            StreamChunk::Data(b"data: partial\n\n".to_vec()),
            StreamChunk::Usage(Usage {
                prompt_tokens: 5,
                completion_tokens: 7,
                total_tokens: 12,
                cached_tokens: Some(3),
                cache_creation_tokens: Some(2),
                reasoning_tokens: Some(1),
            }),
            StreamChunk::Error("upstream burst error".into()),
            StreamChunk::Done,
        ];
        let resp = build_streaming_response(
            &state,
            ir_stream(chunks),
            encode_chat,
            "test-route",
            "openai",
            "gpt-4o",
            "gpt-4o",
            trace_id,
            Instant::now(),
            Instant::now() - std::time::Duration::from_millis(50),
            chrono::Utc::now(),
            chrono::Utc::now(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        state.event_collector.shutdown().await;

        // call_logs: exactly one failure-shaped row. The 50ms
        // `upstream_start` offset and the committed Data chunk must NOT
        // populate `upstream_duration_ms` / `ttft_ms` on a failed row.
        state.collector.flush().await;
        let mut logs = state
            .collector
            .store()
            .query_logs(LogFilter {
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap();
        logs.retain(|l| l.request_id == trace_id);
        assert_eq!(logs.len(), 1, "exactly one call_logs row");
        let log = &logs[0];
        assert_eq!(log.status_code, 502);
        assert_eq!(log.error_category.as_deref(), Some("stream_error"));
        assert_eq!(log.error.as_deref(), Some("upstream burst error"));
        assert!(log.usage.is_none(), "failed row keeps usage NULL");
        assert!(log.cached_tokens.is_none(), "failed row keeps cache NULL");
        assert!(log.cache_creation_tokens.is_none());
        assert!(log.reasoning_tokens.is_none());
        assert!(
            log.ttft_ms.is_none(),
            "failed row must not carry TTFT (success-only timing)"
        );
        assert!(
            log.upstream_duration_ms.is_none(),
            "failed row must not carry upstream timing (success-only timing)"
        );

        // Formal fact: model_failure / stream_error_after_commit, tokens and
        // timing NULL.
        let (final_status, failure_reason, pt, cached, ttft, upstream): (
            String,
            Option<String>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        ) = sqlx::query_as(
            "SELECT final_status, failure_reason, prompt_tokens, cached_tokens, ttft_ms, \
             upstream_duration_ms FROM request_results WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("model_failure fact must be persisted");
        assert_eq!(final_status, "model_failure");
        assert_eq!(failure_reason.as_deref(), Some("stream_error_after_commit"));
        assert!(pt.is_none(), "partial usage must not enter the fact");
        assert!(cached.is_none());
        assert!(ttft.is_none(), "failed request fact keeps timing NULL");
        assert!(upstream.is_none());
    }

    /// Complementary parity check: the cancelled terminal's `call_logs`
    /// row also keeps `ttft_ms` / `upstream_duration_ms` NULL even though
    /// committed `Data` chunks were already forwarded — a cancel is not a
    /// success sample and must not feed the latency coverage denominators.
    #[tokio::test]
    async fn cancelled_terminal_call_log_keeps_timing_null() {
        let (state, pool) = build_state_with_event_pool(vec![]).await;
        let trace_id = Uuid::new_v4();
        let stream = stream::iter(vec![StreamChunk::Data(b"data: partial\n\n".to_vec())])
            .map(IrStreamEvent::from)
            .chain(stream::pending());
        let resp = build_streaming_response(
            &state,
            stream,
            encode_chat,
            "test-route",
            "openai",
            "gpt-4o",
            "gpt-4o",
            trace_id,
            Instant::now(),
            Instant::now() - std::time::Duration::from_millis(50),
            chrono::Utc::now(),
            chrono::Utc::now(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        let mut body = resp.into_body();
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let frame = body.frame();
        futures_util::pin_mut!(frame);
        assert!(
            frame.as_mut().poll(&mut cx).is_ready(),
            "first body frame (the committed Data byte) must be ready"
        );
        drop(body);
        tokio::task::yield_now().await;
        state.event_collector.shutdown().await;

        state.collector.flush().await;
        let mut logs = state
            .collector
            .store()
            .query_logs(LogFilter {
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap();
        logs.retain(|l| l.request_id == trace_id);
        assert_eq!(logs.len(), 1, "exactly one call_logs row");
        assert_eq!(logs[0].status_code, 499);
        assert!(logs[0].ttft_ms.is_none());
        assert!(logs[0].upstream_duration_ms.is_none());
        let _ = pool;
    }

    /// Regression (production incident): an in-stream error frame is
    /// processed as Error-then-Data (the fixed ordering in
    /// `process_sse_frame`). The dispatch unfold loop consumes the `Error`
    /// chunk inline — recording the analytics error state — then yields the
    /// forwarded `Data` frame once. If the client disconnects immediately
    /// after receiving that `Data` frame (stream dropped WITHOUT further
    /// polls), the request must still classify as `model_failure` / 502 with
    /// the upstream message, never `client_cancelled` / 499.
    #[tokio::test]
    async fn drop_after_error_then_data_orders_error_state_before_yield() {
        let (state, pool) = build_state_with_event_pool(vec![]).await;
        let trace_id = Uuid::new_v4();
        // Ordering as produced by the fixed `process_sse_frame`: the `Error`
        // chunk first, then the forwarded `Data` frame. The unfold loop
        // consumes the `Error` inline (no yield), then yields the `Data`
        // once.
        let stream = stream::iter(vec![
            StreamChunk::Error("upstream burst error".into()),
            StreamChunk::Data(
                b"data: {\"error\":{\"message\":\"upstream burst error\"}}\n\n".to_vec(),
            ),
        ])
        .map(IrStreamEvent::from)
        .chain(stream::pending());
        let resp = build_streaming_response(
            &state,
            stream,
            encode_chat,
            "test-route",
            "openai",
            "gpt-4o",
            "gpt-4o",
            trace_id,
            Instant::now(),
            Instant::now(),
            chrono::Utc::now(),
            chrono::Utc::now(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        // Drive the body exactly ONE frame with a noop waker: the first poll
        // consumes the `Error` chunk inline and yields the forwarded `Data`
        // frame — the client receives it and immediately disconnects,
        // dropping the body WITHOUT any further poll of the now-pending
        // stream.
        let mut body = resp.into_body();
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let frame = body.frame();
        futures_util::pin_mut!(frame);
        assert!(
            frame.as_mut().poll(&mut cx).is_ready(),
            "first poll must yield the forwarded Data frame (error state already recorded)"
        );
        drop(body);
        tokio::task::yield_now().await;
        state.event_collector.shutdown().await;

        // Request fact: model_failure — the error recorded BEFORE the Data
        // yield must win over the client disconnect, never client_cancelled.
        let (final_status, failure_reason): (String, Option<String>) = sqlx::query_as(
            "SELECT final_status, failure_reason FROM request_results WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("model_failure fact must be persisted");
        assert_eq!(final_status, "model_failure");
        assert_eq!(
            failure_reason.as_deref(),
            Some("stream_error_after_commit"),
            "the error observed before the Data yield must win over client_cancelled"
        );

        // call_logs row: 502 + the upstream message.
        state.collector.flush().await;
        let mut logs = state
            .collector
            .store()
            .query_logs(LogFilter {
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap();
        logs.retain(|l| l.request_id == trace_id);
        assert_eq!(logs.len(), 1, "exactly one call_logs row");
        assert_eq!(logs[0].status_code, 502);
        assert_eq!(logs[0].error_category.as_deref(), Some("stream_error"));
        assert_eq!(logs[0].error.as_deref(), Some("upstream burst error"));
    }

    // ─── Round-1 rework: terminal emit survives body drop under
    // ─── backpressure ─────────────────────────────────────────────

    /// The round-1 bug: the Done/None terminal claimed `done` and then ran
    /// `emit().await` inside the body future — with the queue full
    /// (backpressure), dropping the body cancelled the pending emit, and
    /// the drop guard skipped (`done` already set), so the success fact
    /// was lost. Regression: with the queue full and the body dropped
    /// *while the terminal delivery is pending*, exactly one `success`
    /// request result must still be persisted, with the final accumulated
    /// usage.
    #[tokio::test]
    async fn backpressure_pending_body_drop_success_terminal_still_lands_once() {
        let (state, pool) = build_state_with_event_queue_capacity(vec![], 1).await;
        // Hold the pool's only connection: `open_analytics_pool(":memory:")`
        // uses `max_connections = 1`, so the writer blocks on its first
        // write and the queue cannot drain.
        let conn = pool.acquire().await.unwrap();
        fill_tiny_queue(&state);

        let trace_id = Uuid::new_v4();
        let resp = build_streaming_response(
            &state,
            ir_stream(vec![
                StreamChunk::Data(b"data: partial\n\n".to_vec()),
                StreamChunk::Usage(Usage {
                    prompt_tokens: 5,
                    completion_tokens: 7,
                    total_tokens: 12,
                    cached_tokens: None,
                    cache_creation_tokens: None,
                    reasoning_tokens: None,
                }),
                StreamChunk::Done,
            ]),
            encode_chat,
            "test-route",
            "openai",
            "gpt-4o",
            "gpt-4o",
            trace_id,
            Instant::now(),
            Instant::now(),
            chrono::Utc::now(),
            chrono::Utc::now(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        // Drive the body one frame at a time with a noop waker: the first
        // poll forwards the committed byte; the second reaches the Done
        // terminal, whose delivery must block on the full queue
        // (backpressure Pending).
        let mut body = resp.into_body();
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let frame = body.frame();
        futures_util::pin_mut!(frame);
        assert!(
            frame.as_mut().poll(&mut cx).is_ready(),
            "first body frame (the committed Data byte) must be ready"
        );
        assert!(
            frame.poll(&mut cx).is_pending(),
            "terminal delivery must pend on the full queue"
        );

        // The disconnect: drop the body while the terminal enqueue is
        // pending. The detached emit task must survive and still enqueue.
        drop(body);
        // Let the detached task start and clone its sender before shutdown
        // closes the slot, so its backpressured send can complete once the
        // queue drains.
        tokio::task::yield_now().await;

        // Unblock the writer: the queue drains, the task's send completes,
        // and shutdown awaits the writer until every event is persisted.
        drop(conn);
        state.event_collector.shutdown().await;

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_results WHERE request_id = ?")
                .bind(trace_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            count, 1,
            "exactly one terminal fact despite body drop under backpressure"
        );

        let (final_status, stream_flag, pt, ct): (String, i32, Option<i64>, Option<i64>) =
            sqlx::query_as(
                "SELECT final_status, stream, prompt_tokens, completion_tokens \
                 FROM request_results WHERE request_id = ?",
            )
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(final_status, "success");
        assert_eq!(stream_flag, 1);
        assert_eq!(pt, Some(5), "final accumulated usage must reach the fact");
        assert_eq!(ct, Some(7));
    }

    /// The error-after-commit variant of the backpressure body-drop
    /// regression: with the queue full and the body dropped while the
    /// `model_failure` / `stream_error_after_commit` delivery is pending,
    /// exactly one request result must still be persisted, and partial
    /// usage must stay NULL.
    #[tokio::test]
    async fn backpressure_pending_body_drop_error_terminal_still_lands_once() {
        let (state, pool) = build_state_with_event_queue_capacity(vec![], 1).await;
        let conn = pool.acquire().await.unwrap();
        fill_tiny_queue(&state);

        let trace_id = Uuid::new_v4();
        let resp = build_streaming_response(
            &state,
            ir_stream(vec![
                StreamChunk::Data(b"data: partial\n\n".to_vec()),
                StreamChunk::Usage(Usage {
                    prompt_tokens: 5,
                    completion_tokens: 7,
                    total_tokens: 12,
                    cached_tokens: None,
                    cache_creation_tokens: None,
                    reasoning_tokens: None,
                }),
                StreamChunk::Error("upstream died".into()),
                StreamChunk::Done,
            ]),
            encode_chat,
            "test-route",
            "openai",
            "gpt-4o",
            "gpt-4o",
            trace_id,
            Instant::now(),
            Instant::now(),
            chrono::Utc::now(),
            chrono::Utc::now(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        let mut body = resp.into_body();
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let frame = body.frame();
        futures_util::pin_mut!(frame);
        assert!(
            frame.as_mut().poll(&mut cx).is_ready(),
            "first body frame (the committed Data byte) must be ready"
        );
        assert!(
            frame.poll(&mut cx).is_pending(),
            "Error consumed; the terminal delivery must pend on the full queue"
        );

        drop(body);
        tokio::task::yield_now().await;
        drop(conn);
        state.event_collector.shutdown().await;

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_results WHERE request_id = ?")
                .bind(trace_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            count, 1,
            "exactly one terminal fact despite body drop under backpressure"
        );

        let (final_status, failure_reason, pt): (String, Option<String>, Option<i64>) =
            sqlx::query_as(
                "SELECT final_status, failure_reason, prompt_tokens \
                 FROM request_results WHERE request_id = ?",
            )
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(final_status, "model_failure");
        assert_eq!(failure_reason.as_deref(), Some("stream_error_after_commit"));
        assert!(
            pt.is_none(),
            "partial usage must stay NULL on the failed terminal"
        );
    }

    /// The client-disconnect variant of the backpressure regression: the
    /// stream never ends, the queue is full, and the response is dropped —
    /// the drop guard's terminal delivery must take the backpressure path
    /// and still persist exactly one `client_cancelled` request result
    /// (tokens NULL).
    #[tokio::test]
    async fn backpressure_response_drop_guard_delivery_still_lands_once() {
        let (state, pool) = build_state_with_event_queue_capacity(vec![], 1).await;
        let conn = pool.acquire().await.unwrap();
        fill_tiny_queue(&state);

        let trace_id = Uuid::new_v4();
        let stream = stream::iter(vec![StreamChunk::Data(b"data: partial\n\n".to_vec())])
            .map(IrStreamEvent::from)
            .chain(stream::pending());
        let resp = build_streaming_response(
            &state,
            stream,
            encode_chat,
            "test-route",
            "openai",
            "gpt-4o",
            "gpt-4o",
            trace_id,
            Instant::now(),
            Instant::now(),
            chrono::Utc::now(),
            chrono::Utc::now(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        let mut body = resp.into_body();
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let frame = body.frame();
        futures_util::pin_mut!(frame);
        assert!(
            frame.as_mut().poll(&mut cx).is_ready(),
            "first body frame (the committed Data byte) must be ready"
        );
        assert!(
            frame.poll(&mut cx).is_pending(),
            "stream must be suspended waiting on the upstream"
        );

        // The disconnect: dropping the body runs the guard, whose delivery
        // must take the backpressure path (the queue is full) and spawn
        // the detached emit.
        drop(body);
        // Let the detached task start and clone its sender before shutdown
        // closes the slot.
        tokio::task::yield_now().await;

        drop(conn);
        state.event_collector.shutdown().await;

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_results WHERE request_id = ?")
                .bind(trace_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            count, 1,
            "drop guard under backpressure must still persist exactly one fact"
        );

        let (final_status, failure_reason, pt): (String, Option<String>, Option<i64>) =
            sqlx::query_as(
                "SELECT final_status, failure_reason, prompt_tokens \
                 FROM request_results WHERE request_id = ?",
            )
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(final_status, "client_cancelled");
        assert_eq!(failure_reason.as_deref(), Some("client_cancelled"));
        assert!(
            pt.is_none(),
            "cancelled terminal must keep tokens NULL even under backpressure"
        );
    }

    /// Controlled wrapper test for the drop guard outside any Tokio
    /// runtime context (Drop cannot await): the synchronous `try_emit`
    /// fast path must enqueue the event without a runtime.
    #[test]
    fn drop_guard_falls_back_to_try_emit_outside_runtime() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let pool = rt.block_on(async {
            sag_analytics::open_analytics_pool(":memory:")
                .await
                .unwrap()
        });
        // The collector must be created inside the runtime — its writer
        // task is spawned via `tokio::spawn`.
        let collector = rt.block_on(async {
            Arc::new(sag_analytics::EventCollector::new(
                sag_analytics::EventWriter::new(pool.clone()),
            ))
        });
        let call_log_collector = rt.block_on(async {
            Arc::new(sag_analytics::CallLogCollector::new_memory().await.unwrap())
        });
        let trace_id = Uuid::new_v4();
        let guard = rt.block_on(async {
            TerminalGuard {
                analytics: Arc::new(StreamAnalytics {
                    event_collector: collector.clone(),
                    request_id: trace_id,
                    route_name: "test-route".to_string(),
                    requested_model: "gpt-4o".to_string(),
                    provider: "openai".to_string(),
                    real_model: "gpt-4o".to_string(),
                    ingress_protocol: None,
                    upstream_protocol: None,
                    lane: None,
                    started_at: chrono::Utc::now(),
                    target_started_at: chrono::Utc::now(),
                    prior_target_facts: Vec::new(),
                    done: AtomicBool::new(false),
                    error: AtomicBool::new(false),
                    error_message: Mutex::new(None),
                    call_log_collector: call_log_collector.clone(),
                    start: Instant::now(),
                    reasoning_effort: None,
                    user_agent: None,
                    client_ip: None,
                    loss_summary: None,
                    session_key: None,
                    terminal_frame_forwarded: AtomicBool::new(false),
                    usage: Mutex::new(Usage::default()),
                    first_byte_at: Mutex::new(None),
                    upstream_start: Instant::now(),
                }),
            }
        });
        // Dropped on the test thread, outside any runtime context: the
        // synchronous `try_emit` fast path still enqueues the event — no
        // task and no runtime are needed while the queue has room.
        drop(guard);

        rt.block_on(async {
            collector.shutdown().await;
            let (final_status, stream): (String, i32) = sqlx::query_as(
                "SELECT final_status, stream FROM request_results WHERE request_id = ?",
            )
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("try_emit fallback must enqueue the event");
            assert_eq!(final_status, "client_cancelled");
            assert_eq!(stream, 1);

            // The drop-guard call_logs row must also land (the synchronous
            // `try_send` path works without a runtime context).
            call_log_collector.flush().await;
            let logs = call_log_collector
                .store()
                .query_logs(sag_analytics::LogFilter {
                    limit: 10,
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(logs.len(), 1, "drop guard writes exactly one call_logs row");
            assert_eq!(logs[0].request_id, trace_id);
            assert_eq!(logs[0].status_code, 499);
            assert_eq!(logs[0].error_category.as_deref(), Some("client_cancelled"));
        });
    }

    /// Drive `body` with a noop waker until it suspends, returning the number
    /// of yielded frames. This is the controlled-timing fixture for the
    /// terminal-frame race: a `stream::iter` that is fully `Ready` would let
    /// the loop consume `End` before the caller can drop the body, so the race
    /// window (`End` not yet enqueued) is reproduced by chaining
    /// `stream::pending()` and dropping the body while suspended.
    fn drain_until_pending(body: &mut axum::body::Body) -> usize {
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut frames = 0;
        loop {
            let fut = body.frame();
            futures_util::pin_mut!(fut);
            match fut.poll(&mut cx) {
                std::task::Poll::Ready(Some(Ok(_))) => frames += 1,
                std::task::Poll::Ready(Some(Err(e))) => panic!("body error: {e}"),
                std::task::Poll::Ready(None) => panic!("body ended before the drop"),
                std::task::Poll::Pending => return frames,
            }
            assert!(frames < 64, "body did not suspend");
        }
    }

    /// Race regression (`.plan/stream-terminal-frame-race-fix.md` §2.2): the
    /// client receives `data: [DONE]`, the upstream `End` is still in flight
    /// (upstream FIN tail latency), the client closes, hyper drops the body.
    /// The drop guard must classify the terminal as `Success` — not
    /// `client_cancelled` / 499 — and carry the full success payload (final
    /// usage, TTFT, upstream duration) into both the fact and the `call_logs`
    /// row.
    #[tokio::test]
    async fn race_client_drop_after_done_classifies_success() {
        let (state, pool) = build_state_with_event_pool(vec![]).await;
        let trace_id = Uuid::new_v4();
        let usage = Usage {
            prompt_tokens: 11,
            completion_tokens: 7,
            total_tokens: 18,
            cached_tokens: Some(2),
            cache_creation_tokens: Some(1),
            reasoning_tokens: Some(3),
        };
        // Usage + `[DONE]` arrive, then the stream suspends: `End` has not
        // been enqueued yet — the race window.
        let stream = stream::iter(vec![
            StreamChunk::Data(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n".to_vec(),
            ),
            StreamChunk::Usage(usage.clone()),
            StreamChunk::Data(b"data: [DONE]\n\n".to_vec()),
        ])
        .map(IrStreamEvent::from)
        .chain(stream::pending());
        let resp = build_streaming_response(
            &state,
            stream,
            encode_chat,
            "test-route",
            "openai",
            "gpt-4o",
            "gpt-4o",
            trace_id,
            Instant::now(),
            Instant::now() - std::time::Duration::from_millis(50),
            chrono::Utc::now(),
            chrono::Utc::now(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        let mut body = resp.into_body();
        // delta + `[DONE]` are forwarded; the next poll suspends (no `End`).
        assert_eq!(drain_until_pending(&mut body), 2);
        // The client closes after `[DONE]`; hyper drops the body.
        drop(body);
        tokio::task::yield_now().await;
        state.event_collector.shutdown().await;

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_results WHERE request_id = ?")
                .bind(trace_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 1, "exactly one terminal fact");

        #[allow(clippy::type_complexity)]
        let (final_status, failure_reason, pt, ct, cached, ttft, upstream): (
            String,
            Option<String>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        ) = sqlx::query_as(
            "SELECT final_status, failure_reason, prompt_tokens, completion_tokens, \
             cached_tokens, ttft_ms, upstream_duration_ms \
             FROM request_results WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("success fact must be persisted");
        assert_eq!(
            final_status, "success",
            "a drop after the terminal frame is a normal completion, not a cancel"
        );
        assert!(failure_reason.is_none());
        assert_eq!(pt, Some(11), "final accumulated usage enters the fact");
        assert_eq!(ct, Some(7));
        assert_eq!(cached, Some(2));
        assert!(ttft.is_some(), "drop-as-success carries TTFT");
        assert!(
            upstream.is_some_and(|v| v >= 50),
            "drop-as-success carries upstream_duration_ms from the 50ms offset"
        );

        let target_status: String =
            sqlx::query_scalar("SELECT status FROM target_attempts WHERE request_id = ?")
                .bind(trace_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(target_status, "success");

        // call_logs row: 200, no error, full success payload.
        state.collector.flush().await;
        let mut logs = state
            .collector
            .store()
            .query_logs(LogFilter {
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap();
        logs.retain(|l| l.request_id == trace_id);
        assert_eq!(logs.len(), 1, "exactly one call_logs row");
        let log = &logs[0];
        assert_eq!(log.status_code, 200);
        assert!(log.error.is_none(), "success row carries no error message");
        assert!(log.error_category.is_none());
        assert_eq!(log.usage.as_ref().map(|u| u.total_tokens), Some(18));
        assert_eq!(log.cached_tokens, Some(2));
        assert_eq!(log.cache_creation_tokens, Some(1));
        assert_eq!(log.reasoning_tokens, Some(3));
        assert!(log.ttft_ms.is_some(), "success row carries TTFT");
        assert!(
            log.upstream_duration_ms.is_some(),
            "success row carries upstream timing"
        );
    }

    /// Invariant regression: a disconnect before any terminal frame stays
    /// `client_cancelled` / 499 with usage / TTFT NULL.
    #[tokio::test]
    async fn midstream_disconnect_still_499() {
        let (state, pool) = build_state_with_event_pool(vec![]).await;
        let trace_id = Uuid::new_v4();
        let stream = stream::iter(vec![
            StreamChunk::Data(b"data: one\n\n".to_vec()),
            StreamChunk::Data(b"data: two\n\n".to_vec()),
        ])
        .map(IrStreamEvent::from)
        .chain(stream::pending());
        let resp = build_streaming_response(
            &state,
            stream,
            encode_chat,
            "test-route",
            "openai",
            "gpt-4o",
            "gpt-4o",
            trace_id,
            Instant::now(),
            Instant::now() - std::time::Duration::from_millis(50),
            chrono::Utc::now(),
            chrono::Utc::now(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        let mut body = resp.into_body();
        assert_eq!(drain_until_pending(&mut body), 2);
        drop(body);
        tokio::task::yield_now().await;
        state.event_collector.shutdown().await;

        let (final_status, failure_reason, pt): (String, Option<String>, Option<i64>) =
            sqlx::query_as(
                "SELECT final_status, failure_reason, prompt_tokens \
                 FROM request_results WHERE request_id = ?",
            )
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("client_cancelled fact must be persisted");
        assert_eq!(final_status, "client_cancelled");
        assert_eq!(failure_reason.as_deref(), Some("client_cancelled"));
        assert!(pt.is_none(), "cancelled fact keeps tokens NULL");

        state.collector.flush().await;
        let mut logs = state
            .collector
            .store()
            .query_logs(LogFilter {
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap();
        logs.retain(|l| l.request_id == trace_id);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].status_code, 499);
        assert_eq!(logs[0].error_category.as_deref(), Some("client_cancelled"));
        assert!(logs[0].usage.is_none());
        assert!(logs[0].ttft_ms.is_none());
        assert!(logs[0].upstream_duration_ms.is_none());
    }

    /// 防伪造回归（评审 Minor-3）：内容帧的文本里原样出现终点字样
    /// （`event: response.completed` 等）不得把 `terminal_frame_forwarded`
    /// 置位 —— 否则真实中途断连会被 Drop 三路分类误记为 200 Success。
    #[tokio::test]
    async fn fake_terminal_text_in_content_still_499() {
        let (state, pool) = build_state_with_event_pool(vec![]).await;
        let trace_id = Uuid::new_v4();
        let stream = stream::iter(vec![
            StreamChunk::Data(
                b"data: {\"choices\":[{\"delta\":{\"text\":\"event: response.completed\"}}]}\n\n"
                    .to_vec(),
            ),
            StreamChunk::Data(
                b"data: {\"choices\":[{\"delta\":{\"text\":\"data: [DONE]\"}}]}\n\n".to_vec(),
            ),
        ])
        .map(IrStreamEvent::from)
        .chain(stream::pending());
        let resp = build_streaming_response(
            &state,
            stream,
            encode_chat,
            "test-route",
            "openai",
            "gpt-4o",
            "gpt-4o",
            trace_id,
            Instant::now(),
            Instant::now() - std::time::Duration::from_millis(50),
            chrono::Utc::now(),
            chrono::Utc::now(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        let mut body = resp.into_body();
        assert_eq!(drain_until_pending(&mut body), 2);
        drop(body);
        tokio::task::yield_now().await;
        state.event_collector.shutdown().await;

        let (final_status, failure_reason, pt): (String, Option<String>, Option<i64>) =
            sqlx::query_as(
                "SELECT final_status, failure_reason, prompt_tokens \
                 FROM request_results WHERE request_id = ?",
            )
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("client_cancelled fact must be persisted");
        assert_eq!(final_status, "client_cancelled");
        assert_eq!(failure_reason.as_deref(), Some("client_cancelled"));
        assert!(pt.is_none(), "cancelled fact keeps tokens NULL");

        state.collector.flush().await;
        let mut logs = state
            .collector
            .store()
            .query_logs(LogFilter {
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap();
        logs.retain(|l| l.request_id == trace_id);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].status_code, 499);
        assert_eq!(logs[0].error_category.as_deref(), Some("client_cancelled"));
        assert!(logs[0].usage.is_none());
        assert!(logs[0].ttft_ms.is_none());
        assert!(logs[0].upstream_duration_ms.is_none());
    }

    /// Invariant regression: an observed upstream error outranks both the
    /// cancel and the terminal-frame flag — the drop stays `model_failure` /
    /// 502.
    #[tokio::test]
    async fn upstream_error_then_drop_still_502() {
        let (state, pool) = build_state_with_event_pool(vec![]).await;
        let trace_id = Uuid::new_v4();
        let stream = stream::iter(vec![
            StreamChunk::Data(b"data: partial\n\n".to_vec()),
            StreamChunk::Error("upstream burst error".into()),
        ])
        .map(IrStreamEvent::from)
        .chain(stream::pending());
        let resp = build_streaming_response(
            &state,
            stream,
            encode_chat,
            "test-route",
            "openai",
            "gpt-4o",
            "gpt-4o",
            trace_id,
            Instant::now(),
            Instant::now(),
            chrono::Utc::now(),
            chrono::Utc::now(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        let mut body = resp.into_body();
        assert_eq!(drain_until_pending(&mut body), 1);
        drop(body);
        tokio::task::yield_now().await;
        state.event_collector.shutdown().await;

        let (final_status, failure_reason): (String, Option<String>) = sqlx::query_as(
            "SELECT final_status, failure_reason FROM request_results WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("model_failure fact must be persisted");
        assert_eq!(final_status, "model_failure");
        assert_eq!(failure_reason.as_deref(), Some("stream_error_after_commit"));

        state.collector.flush().await;
        let mut logs = state
            .collector
            .store()
            .query_logs(LogFilter {
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap();
        logs.retain(|l| l.request_id == trace_id);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].status_code, 502);
        assert_eq!(logs[0].error_category.as_deref(), Some("stream_error"));
        assert_eq!(logs[0].error.as_deref(), Some("upstream burst error"));
        assert!(logs[0].usage.is_none());
    }

    /// Wiring regression: the terminal-frame judgement runs on the bytes
    /// **after** `encode`. A structured `Finish` event encoded by the
    /// Responses codec synthesizes `response.completed` (translated lane) and
    /// must set the flag; a plain delta frame and Anthropic's
    /// `message_delta` with `stop_reason: null` must not.
    #[tokio::test]
    async fn terminal_frame_flag_set_on_encoded_terminal() {
        // ── Positive: `Finish` → `response.completed` via the Responses
        // stream encoder (no Raw terminal frame involved).
        let (state, pool) = build_state_with_event_pool(vec![]).await;
        let trace_id = Uuid::new_v4();
        let stream = stream::iter(vec![IrStreamEvent::Finish {
            index: 0,
            finish_reason: None,
        }])
        .chain(stream::pending());
        let resp = build_streaming_response(
            &state,
            stream,
            sag_providers::ResponsesCodec::stream_encoder(),
            "test-route",
            "openai",
            "gpt-5",
            "gpt-5",
            trace_id,
            Instant::now(),
            Instant::now(),
            chrono::Utc::now(),
            chrono::Utc::now(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let mut body = resp.into_body();
        assert_eq!(drain_until_pending(&mut body), 1);
        drop(body);
        tokio::task::yield_now().await;
        state.event_collector.shutdown().await;

        let final_status: String =
            sqlx::query_scalar("SELECT final_status FROM request_results WHERE request_id = ?")
                .bind(trace_id.to_string())
                .fetch_one(&pool)
                .await
                .expect("success fact must be persisted");
        assert_eq!(
            final_status, "success",
            "the encoded `response.completed` frame must be recognised as terminal"
        );

        // ── Negative: ordinary delta frames do not set the flag.
        for (encode_name, ev) in [
            (
                "chat text delta",
                IrStreamEvent::TextDelta {
                    index: 0,
                    text: "hello".into(),
                },
            ),
            (
                "anthropic message_delta",
                IrStreamEvent::Raw(
                    b"data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":null}}\n\n"
                        .to_vec(),
                ),
            ),
        ] {
            let (state, pool) = build_state_with_event_pool(vec![]).await;
            let trace_id = Uuid::new_v4();
            let stream = stream::iter(vec![ev]).chain(stream::pending());
            let resp = build_streaming_response(
                &state,
                stream,
                encode_chat,
                "test-route",
                "openai",
                "gpt-4o",
                "gpt-4o",
                trace_id,
                Instant::now(),
                Instant::now(),
                chrono::Utc::now(),
                chrono::Utc::now(),
                Vec::new(),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            );
            let mut body = resp.into_body();
            assert_eq!(drain_until_pending(&mut body), 1, "{encode_name}");
            drop(body);
            tokio::task::yield_now().await;
            state.event_collector.shutdown().await;

            let final_status: String =
                sqlx::query_scalar("SELECT final_status FROM request_results WHERE request_id = ?")
                    .bind(trace_id.to_string())
                    .fetch_one(&pool)
                    .await
                    .expect("cancel fact must be persisted");
            assert_eq!(
                final_status, "client_cancelled",
                "{encode_name} is not a terminal frame"
            );
        }
    }
}
