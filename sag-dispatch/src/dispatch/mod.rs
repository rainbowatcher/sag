//! Multi-provider dispatch logic for the SAG gateway.
//!
//! ## Layout
//!
//! - [`resolve_route`] — model → ordered targets + `api_endpoint` resolution
//!   with disabled-provider filtering and retry-count inheritance.
//! - [`dispatch_chat`] / [`dispatch_image`] — multi-provider fallback loops
//!   for chat and image generation respectively.
//! - [`run_with_retries`] — generic per-target retry loop shared by both
//!   dispatch paths.
//! - [`build_streaming_response`] — SSE response builder that forwards
//!   `StreamChunk::Data` to the client and accumulates `Usage` / `Error`
//!   chunks into a final `CallLog`.
//!
//! Target-ordering helpers (FillFirst / RoundRobin) live in [`crate::order`].

use std::collections::HashMap;
use std::time::Instant;

use axum::{
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use sag_config::{ProviderConfig, Strategy};
use sag_core::{
    AppError, CallLog, ChatCompletionRequest, FailureReason, FinalStatus, RequestResultFact,
    TargetAttemptFact, TargetStatus, Usage,
};
use tracing::{error, info, warn};

use crate::dispatch::retry::RetryOutcome;
use crate::state::{AppState, DispatchRuntime};
use sag_analytics::EventCollector;
use sag_core::RouteTargetKey;
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub mod chat;
pub mod image;
pub mod resolve;
pub mod retry;
pub mod stream;

pub use chat::dispatch_chat;
pub use image::dispatch_image;
pub use resolve::resolve_route;

/// A single resolved fallback target with effective values after
/// three-tier fallback resolution.
pub struct ResolvedTarget {
    pub provider: String,
    pub real_model: String,
    /// The config route `model_name` that resolved this target (`"*"` for a
    /// wildcard route). Recorded on the sticky writeback so prune and the
    /// admin snapshot can attribute the pin to the correct route.
    pub route_model: String,
    /// The target's `real_model` spec (`None` = passthrough, the effective
    /// model is the request model). Recorded on the sticky writeback so the
    /// admin snapshot can tell a passthrough pin from an explicit pin.
    pub target_real_model: Option<String>,
    pub extra: HashMap<String, serde_json::Value>,
    pub retries: u32,
    pub timeout_secs: u64,
}

/// Resolved route data: ordered targets, configured `api_endpoint`,
/// strategy, sticky-failover token captured at decision time, plus the
/// route-identity facts (`route_name`, wall-clock `started_at`) that the
/// upcoming analytics route-start event consumes.
pub struct ResolvedRoute {
    pub targets: Vec<ResolvedTarget>,
    pub api_endpoint: String,
    pub strategy: Strategy,
    pub sticky_token: sag_state::StickyToken,
    /// The config route `model_name` matched for this request (`"*"` for a
    /// wildcard match). Route-level identity for analytics facts; distinct
    /// from [`ResolvedTarget::route_model`], which attributes individual
    /// targets (sticky writeback / admin snapshot).
    pub route_name: String,
    /// Wall-clock timestamp captured at route scheduling time, next to the
    /// `Instant` used for legacy duration timing.
    pub started_at: chrono::DateTime<chrono::Utc>,
}

/// Bundled parameters for a single dispatch call. Replaces the 7–8
/// argument lists on [`dispatch_chat`] / [`dispatch_image`] so the
/// function signatures stay readable as the dispatch surface grows.
pub struct DispatchCtx<'a> {
    pub state: &'a AppState,
    pub runtime: &'a DispatchRuntime,
    pub request: &'a ChatCompletionRequest,
    pub targets: &'a [ResolvedTarget],
    pub api_endpoint: &'a str,
    pub is_stream: bool,
    pub trace_id: uuid::Uuid,
    pub start: Instant,
    /// The matched route's `model_name` (`"*"` for a wildcard route) —
    /// route-level identity for the analytics route-start event.
    pub route_name: &'a str,
    /// Wall-clock timestamp of route scheduling, captured in `routes.rs`
    /// alongside `start`. Fact timestamp for the analytics route-start
    /// event.
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub errors: &'a mut Vec<String>,
    pub strategy: Strategy,
    /// Sticky-failover token captured when the route was ordered (see
    /// [`crate::order::order_targets`]). The success path writes back to
    /// `active_targets` only while all components still match — the global
    /// epoch, the per-model generation, and the config `routes_version` —
    /// so a clear (DELETE / breaker close / targets-changed / prune) or
    /// config mutation that happened after the routing decision can never
    /// be overwritten by an in-flight request's stale pin.
    pub sticky_token: sag_state::StickyToken,
    pub user_agent: Option<String>,
    pub client_ip: Option<String>,
}

// ═══════════════════════════════════════════════════════════════════
// Circuit breaker helpers — shared across the dispatch submodules.
// ═══════════════════════════════════════════════════════════════════

/// Circuit-gate permit (RAII guard). `'a` only borrows the
/// [`DispatchRuntime`] snapshot fields — it never carries a
/// `RwLockReadGuard` (the release timing is spelled out under
/// [`ProbeGuard`]).
pub(in crate::dispatch) enum CircuitPermit<'a> {
    /// Target healthy (`Closed`): holds no probe slot, nothing to release;
    /// only carries the identity for outcome application.
    Allowed { key: RouteTargetKey },
    /// A half-open probe slot was claimed: the slot is exclusively held by
    /// this guard until it is consumed. Release is done synchronously by the
    /// consuming [`apply_circuit_outcome`] on the normal path; abnormal
    /// paths are caught by `ProbeGuard`'s `Drop`.
    Probe(ProbeGuard<'a>),
}

/// RAII guard over a claimed half-open probe slot.
///
/// Holds a read-only borrow of the [`DispatchRuntime`] — used only to reach
/// the shared breaker `Arc` and call the release method. The
/// `Arc<RwLock<RouteCircuitBreaker>>` itself survives across `await` (the
/// guard borrows the `Arc`, not the lock guard); detaching clones it once so
/// the release task is decoupled from the caller's lifetime.
pub(in crate::dispatch) struct ProbeGuard<'a> {
    runtime: &'a DispatchRuntime,
    pub key: RouteTargetKey,
    /// Snapshot of `probe_interval_secs` taken at guard creation, shared by
    /// [`apply_circuit_outcome`] and the abnormal-path release so the
    /// judgement and the release can never drift apart.
    pub interval_secs: u64,
    /// Single-completion marker: set by the two consuming release paths
    /// ([`apply_circuit_outcome`] and [`Self::release_probe_slot`]) before
    /// their first release; once set, `Drop` short-circuits and never
    /// releases twice.
    completed: bool,
}

impl<'a> ProbeGuard<'a> {
    pub fn new(runtime: &'a DispatchRuntime, key: RouteTargetKey, interval_secs: u64) -> Self {
        Self {
            runtime,
            key,
            interval_secs,
            completed: false,
        }
    }

    /// Mark the slot as consumed. Must be called by any consuming release
    /// path *before* its first await — dropping the guard after a cancelled
    /// future then short-circuits instead of releasing a second time.
    fn mark_completed(&mut self) {
        self.completed = true;
    }

    /// Synchronous release entry for normal early-skip paths (provider
    /// unresolved / request skipped before the retry loop), keeping the
    /// "dispatch returns ⇒ slot released" semantics.
    ///
    /// Consuming `mut self`: `mark_completed()` runs **before** any await,
    /// so if the future is cancelled while waiting for the write lock the
    /// guard's `Drop` sees `completed = true` and does not release again.
    pub async fn release_probe_slot(mut self) {
        self.mark_completed(); // must precede any await
        // `self` implements `Drop`, so `self.key` cannot be moved out; clone
        // it (same cost as the plan's original by-value transfer).
        self.runtime
            .circuit_breaker
            .write()
            .await
            .record_probe_result(self.key.clone(), false, self.interval_secs);
    }
}

impl Drop for ProbeGuard<'_> {
    fn drop(&mut self) {
        // Only abnormal paths reach here: the two consuming release paths
        // ([`apply_circuit_outcome`] / [`Self::release_probe_slot`]) both
        // call `mark_completed` first, so a dropped guard here means the
        // future was dropped before any release point (panic unwind / client
        // disconnect). Best-effort release keeps `probe_in_flight` from
        // leaking; a reclamation window is the last-resort safety net (see
        // `sag_core::circuit_breaker::try_acquire_probe`).
        if self.completed {
            return;
        }
        let breaker = self.runtime.circuit_breaker.clone();
        let key = self.key.clone();
        let interval = self.interval_secs;

        // Best-effort, never unconditionally `tokio::spawn` — spawning
        // without a current runtime (or while one is shutting down) would
        // panic. Same pattern as
        // `sag-dispatch/src/dispatch/stream.rs::deliver_terminal_event`:
        // without a runtime we warn and leave the slot to the stale-probe
        // reclamation.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    breaker
                        .write()
                        .await
                        .record_probe_result(key, false, interval);
                });
            }
            Err(_) => {
                tracing::warn!(
                    ?key,
                    "dropping un-consumed probe guard with no tokio runtime; slot left for stale reclamation"
                );
            }
        }
    }
}

/// Check whether a target's circuit allows a try.
///
/// Returns `None` if the target is `Open` (cooling down) or is `HalfOpen`
/// with a probe already in flight — in both cases the caller should skip to
/// the next target. Returns `Some(CircuitPermit)` when the target may be
/// tried: `Allowed` for a healthy (`Closed`) target, `Probe(guard)` when
/// this request was handed the half-open probe slot (the guard's drop
/// releases the slot).
pub(in crate::dispatch) async fn circuit_allows_target<'a>(
    runtime: &'a DispatchRuntime,
    model: &str,
    provider_name: &str,
    real_model: &str,
    circuit_breaker_cfg: &sag_config::CircuitBreakerConfig,
) -> Option<CircuitPermit<'a>> {
    let circuit_key = RouteTargetKey::new(model, provider_name, real_model);

    // Single read pass. `can_probe_now` returns true for a healthy (Closed)
    // target as well as a due `Open`/`HalfOpen` one.
    {
        let breaker = runtime.circuit_breaker.read().await;
        if !breaker.can_probe_now(&circuit_key) {
            let next_ms = breaker
                .next_probe_in(&circuit_key)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            info!(
                provider = %provider_name,
                real_model = %real_model,
                next_probe_in_ms = next_ms,
                "skipping open route target circuit (next probe not due)"
            );
            return None;
        }
        // Healthy (Closed) target: allow as a normal attempt without taking a
        // write lock or claiming a probe slot.
        if breaker.is_closed(&circuit_key) {
            return Some(CircuitPermit::Allowed { key: circuit_key });
        }
    }

    // Upgrade to a write pass to transition a due `Open` to `HalfOpen`
    // and claim the single probe slot.
    let mut breaker = runtime.circuit_breaker.write().await;
    if !breaker.to_half_open_if_due(&circuit_key) {
        // Not due after the transition check (race) — treat as skipped.
        return None;
    }

    if breaker.try_acquire_probe(&circuit_key) {
        info!(
            provider = %provider_name,
            real_model = %real_model,
            "half-open circuit probe allowed"
        );
        Some(CircuitPermit::Probe(ProbeGuard::new(
            runtime,
            circuit_key,
            circuit_breaker_cfg.probe_interval_secs,
        )))
    } else {
        // Half-open but a probe is already in flight: skip this request.
        info!(
            provider = %provider_name,
            real_model = %real_model,
            "skipping route target: half-open probe already in flight"
        );
        None
    }
}

/// Apply the outcome of a single target attempt to the circuit breaker.
///
/// Consuming: `permit` enters by value; `key` / `is_probe` are extracted in
/// the `match`, and a `Probe` guard is marked completed so its `Drop`
/// short-circuits — the release is owned by exactly this call, never by a
/// later `Drop`.
///
/// # Asymmetry note (intentional)
///
/// A non-retryable (4xx) error on a *healthy* target does **not** open the
/// circuit — the target stays usable, because a 4xx is usually a client-side
/// request problem, not a target fault. The same 4xx arriving during a
/// *probe* instead re-schedules the next probe one interval later
/// (effectively a short cooldown). This asymmetry is deliberate: a probe
/// must always report an outcome, and a 4xx during a probe means "not usable
/// right now", so it is treated as a failed probe rather than left half-open
/// forever. The practical effect — a target that returns 4xx to probe
/// requests is briefly skipped — is acceptable and self-heals on the next
/// interval.
pub(in crate::dispatch) async fn apply_circuit_outcome<'a, T>(
    runtime: &'a DispatchRuntime,
    permit: CircuitPermit<'a>,
    outcome: RetryOutcome<T>,
    circuit_breaker_cfg: &sag_config::CircuitBreakerConfig,
) -> Option<T> {
    let interval = circuit_breaker_cfg.probe_interval_secs;
    // Extract `key` and `is_probe`. The `Probe` guard is moved out and
    // dropped at the end of the match arm — `mark_completed()` runs first,
    // so the drop is a strict no-op.
    let (breaker, key, is_probe) = match permit {
        CircuitPermit::Allowed { key } => (runtime.circuit_breaker.clone(), key, false),
        CircuitPermit::Probe(mut guard) => {
            let breaker = guard.runtime.circuit_breaker.clone();
            let key = guard.key.clone();
            guard.mark_completed();
            (breaker, key, true)
        }
    };
    match outcome {
        RetryOutcome::Success(value) => {
            breaker.write().await.close(&key);
            if is_probe {
                info!(
                    provider = %key.provider,
                    real_model = %key.real_model,
                    "half-open circuit probe succeeded, circuit closed"
                );
            }
            Some(value)
        }
        RetryOutcome::Exhausted(last_err) => {
            let hint = last_err.circuit_hint();
            let duration_secs = hint.duration_secs(
                circuit_breaker_cfg.quota_exhausted_secs,
                circuit_breaker_cfg.rate_limited_secs,
                circuit_breaker_cfg.generic_secs,
            );
            if is_probe {
                // A probe during cooldown failed: reschedule the next probe
                // one interval later (steady, low-traffic cadence) rather than
                // re-opening the full cooldown — otherwise early recovery
                // would again be invisible until the whole window elapsed.
                info!(
                    provider = %key.provider,
                    real_model = %key.real_model,
                    hint = ?hint,
                    interval_secs = interval,
                    "half-open circuit probe failed, rescheduling next probe"
                );
                breaker
                    .write()
                    .await
                    .record_probe_result(key, false, interval);
            } else {
                // First failure (target was Closed/healthy): open the circuit
                // with the error-derived cooldown as the hard lower bound and
                // `interval` as the periodic probe cadence.
                info!(
                    hint = ?hint,
                    duration_secs,
                    interval_secs = interval,
                    "opening circuit with error-based duration"
                );
                breaker.write().await.open(key, duration_secs, interval);
            }
            None
        }
        RetryOutcome::NonRetryable(_) => {
            if is_probe {
                // A non-retryable (4xx) probe means the target is not usable
                // right now — reschedule the next probe one interval later
                // rather than leaving it half-open forever.
                breaker
                    .write()
                    .await
                    .record_probe_result(key, false, interval);
            }
            None
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// Model-override helpers — shared across the dispatch submodules.
// ═══════════════════════════════════════════════════════════════════

/// Merge model-level overrides into target extra.
///
/// Provider-level `body_overrides` are already applied inside the
/// provider adapter (`ProviderBase::build_request`). Model-level
/// overrides sit between provider-level and target-level — they form
/// the base, and target-level `extra` is inserted on top so the
/// route-level config wins.
pub(in crate::dispatch) fn merge_model_overrides(
    providers: &HashMap<String, ProviderConfig>,
    provider_name: &str,
    real_model: &str,
    target_extra: &HashMap<String, serde_json::Value>,
) -> HashMap<String, serde_json::Value> {
    let model_ov = providers
        .get(provider_name)
        .and_then(|pc| pc.model_overrides.get(real_model))
        .cloned()
        .unwrap_or_default();
    let mut merged = model_ov;
    for (k, v) in target_extra {
        merged.insert(k.clone(), v.clone());
    }
    merged
}

// ═══════════════════════════════════════════════════════════════════
// Analytics event helpers — shared across the dispatch submodules.
// ═══════════════════════════════════════════════════════════════════

/// Map a client-caused 4xx (see [`retry::is_client_caused_4xx`]) to the
/// registered [`FailureReason`] for the `client_error` final status.
///
/// The spec table registers exactly two reasons for `client_error`:
/// `invalid_request` / `payload_too_large`. A 413 (payload too large)
/// maps to `payload_too_large`; every other client-caused 4xx (400 / 422 /
/// unknown) maps to `invalid_request`.
pub(in crate::dispatch) fn client_error_failure_reason(err: &AppError) -> FailureReason {
    match err {
        AppError::Upstream { status: 413, .. } => FailureReason::PayloadTooLarge,
        _ => FailureReason::InvalidRequest,
    }
}

/// Enqueue a request-level analytics event with formal backpressure
/// ([`EventCollector::emit`](sag_analytics::EventCollector::emit), never
/// `try_emit` — the fact must not be dropped). A collector failure is only
/// possible once the collector is shut down; it is logged and swallowed so
/// the client-facing response already built by the caller keeps its exact
/// semantics.
pub(in crate::dispatch) async fn emit_analytics_event(
    state: &AppState,
    event: sag_analytics::AnalyticsEvent,
) {
    if let Err(e) = state.event_collector.emit(event.clone()).await {
        warn!(
            request_id = %event.result.request_id,
            status = ?event.result.final_status,
            error = %e,
            "analytics event not enqueued (event collector shut down)"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════
// Dispatch cancellation guard — client-disconnect terminal.
// ═══════════════════════════════════════════════════════════════════

/// An in-flight (actually-issued) target attempt, recorded so a client
/// disconnect can persist it as a `cancelled` target fact.
pub(crate) struct InFlightAttempt {
    provider: String,
    real_model: String,
    started_at: chrono::DateTime<chrono::Utc>,
}

/// Shared state of one dispatch: the request-identity facts needed to
/// build the `client_cancelled` terminal plus the in-flight attempt
/// marker.
pub(crate) struct DispatchAnalytics {
    event_collector: Arc<EventCollector>,
    request_id: uuid::Uuid,
    route_name: String,
    /// The model the client requested (route-level identity).
    requested_model: String,
    is_stream: bool,
    /// Wall-clock route start captured at dispatch time (fact timestamp).
    started_at: chrono::DateTime<chrono::Utc>,
    /// Set by every normal terminal **before** it emits; the drop guard
    /// claims this flag so it can never emit a second request result.
    done: std::sync::atomic::AtomicBool,
    /// The currently in-flight provider call, if any. Updated around the
    /// retry closure's actual provider call; `None` during backoff sleeps
    /// and between targets. On drop (client disconnect), a `Some` value
    /// becomes a `cancelled` target attempt; `None` means the request had
    /// no in-flight attempt (retry/fallback gap) and the event carries 0
    /// target attempts.
    ///
    /// `std::sync::Mutex`: the guard's `Drop` cannot await, and the
    /// critical sections are a single set/clear with no await inside.
    in_flight: std::sync::Mutex<Option<InFlightAttempt>>,
}

/// Drop guard on the dispatch loop: emits the `client_cancelled` terminal
/// when the handler future is dropped before any normal terminal ran — the
/// axum/hyper client-disconnect signal for **non-streaming** requests and
/// for **streaming** requests cancelled before `build_streaming_response`
/// commits (the stream body's own `TerminalGuard` only covers post-commit
/// drops).
///
/// Fires at most once: every normal terminal (success / client_error /
/// all-exhausted / stream handoff) calls [`mark_done`](Self::mark_done)
/// **before** emitting, so a guard dropped on a normal return is a no-op.
/// Delivery runs through the stream module's `deliver_terminal_event`,
/// which decouples the enqueue from the future's cancellation lifecycle.
///
/// ## Trade-off (documented per spec)
///
/// `Drop` cannot await, so the queue-full path spawns a detached task that
/// awaits backpressure. The detached task can be lost in the runtime
/// shutdown race (a task that never gets its first poll while the runtime
/// is tearing down) — the same accepted loss window as the stream
/// `TerminalGuard`. The spec allows "异常崩溃允许少量未提交的分析事件丢失";
/// the common path (`try_emit` fast path) enqueues synchronously inside
/// `Drop` and is fully drained by graceful shutdown.
pub(crate) struct DispatchCancelGuard {
    analytics: Arc<DispatchAnalytics>,
}

impl DispatchAnalytics {
    /// Mark an actual provider call as in-flight. Called immediately
    /// before the upstream call inside the retry closure; cleared by
    /// [`end_attempt`](Self::end_attempt) when the call completes.
    ///
    /// Poisoned-lock recovery: a panic while holding the lock would only
    /// happen on a fatal bug; `into_inner` keeps the guard functional
    /// instead of dropping the event.
    fn begin_attempt(&self, provider: &str, real_model: &str) {
        let mut slot = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(InFlightAttempt {
            provider: provider.to_string(),
            real_model: real_model.to_string(),
            started_at: chrono::Utc::now(),
        });
    }

    /// Clear the in-flight marker after a provider call completed (or the
    /// retry sequence gave up). A client disconnect during the call never
    /// reaches this line — the future is dropped with the marker still
    /// set, so the guard persists a `cancelled` attempt.
    fn end_attempt(&self) {
        let mut slot = self.in_flight.lock().unwrap_or_else(|e| e.into_inner());
        *slot = None;
    }
}

impl DispatchCancelGuard {
    /// Create the guard for one dispatch. Must be kept alive until the
    /// dispatch returns; every terminal path calls [`mark_done`](Self::mark_done)
    /// before emitting its own event.
    pub(crate) fn new(
        event_collector: Arc<EventCollector>,
        request_id: uuid::Uuid,
        route_name: &str,
        requested_model: &str,
        is_stream: bool,
        started_at: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        Self {
            analytics: Arc::new(DispatchAnalytics {
                event_collector,
                request_id,
                route_name: route_name.to_string(),
                requested_model: requested_model.to_string(),
                is_stream,
                started_at,
                done: std::sync::atomic::AtomicBool::new(false),
                in_flight: std::sync::Mutex::new(None),
            }),
        }
    }

    /// Claim the terminal: the normal path is about to emit its own
    /// event, so the drop guard must not fire.
    pub(crate) fn mark_done(&self) {
        self.analytics.done.store(true, Ordering::SeqCst);
    }

    /// Shared state handle for the retry closure (cloned per attempt).
    pub(crate) fn analytics(&self) -> Arc<DispatchAnalytics> {
        self.analytics.clone()
    }
}

impl Drop for DispatchCancelGuard {
    fn drop(&mut self) {
        // Claim the terminal. A normal path already set `done`; only a
        // cancelled dispatch reaches this swap with `false`.
        if self.analytics.done.swap(true, Ordering::SeqCst) {
            return;
        }
        let analytics = &*self.analytics;
        let completed_at = chrono::Utc::now();
        // Take the in-flight attempt, if any. A `Some` becomes one
        // `cancelled` target fact; `None` (cancel during a retry/backoff
        // gap or between targets) leaves the event without target
        // attempts — spec scenario 11.
        let in_flight = self
            .analytics
            .in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let targets = match in_flight {
            Some(att) => vec![TargetAttemptFact {
                provider: att.provider,
                real_model: att.real_model,
                status: TargetStatus::Cancelled,
                failure_reason: None,
                started_at: att.started_at,
                completed_at,
                usage: None,
                cached_tokens: None,
                ttft_ms: None,
                upstream_duration_ms: None,
            }],
            None => Vec::new(),
        };
        let event = sag_analytics::AnalyticsEvent {
            result: RequestResultFact {
                request_id: analytics.request_id,
                route_name: Some(analytics.route_name.clone()),
                requested_model: analytics.requested_model.clone(),
                final_status: FinalStatus::ClientCancelled,
                failure_reason: Some(FailureReason::ClientCancelled),
                stream: analytics.is_stream,
                started_at: analytics.started_at,
                completed_at,
                usage: None,
                cached_tokens: None,
                cache_creation_tokens: None,
                ttft_ms: None,
                upstream_duration_ms: None,
            },
            targets,
        };
        let request_id = event.result.request_id;
        let _ = crate::dispatch::stream::deliver_terminal_event(
            analytics.event_collector.clone(),
            event,
        );
        warn!(
            request_id = %request_id,
            "dispatch cancelled by client disconnect; client_cancelled fact enqueued"
        );
    }
}

// ═══════════════════════════════════════════════════════════════════
// Logging / response helpers — shared across the dispatch submodules.

/// Build a `CallLog` for a successful response.
#[allow(clippy::too_many_arguments)]
pub(in crate::dispatch) fn success_log(
    request_id: uuid::Uuid,
    provider: &str,
    model: &str,
    real_model: &str,
    stream: bool,
    start: Instant,
    usage: Option<&Usage>,
    reasoning_effort: Option<&str>,
    upstream_duration_ms: Option<u64>,
    ttft_ms: Option<u64>,
    user_agent: Option<String>,
    client_ip: Option<String>,
) -> CallLog {
    let usage = usage.cloned();
    CallLog {
        request_id,
        provider: provider.to_string(),
        model: model.to_string(),
        real_model: real_model.to_string(),
        stream,
        status_code: 200,
        duration_ms: start.elapsed().as_millis() as u64,
        error: None,
        timestamp: chrono::Utc::now(),
        reasoning_effort: reasoning_effort.map(|s| s.to_string()),
        upstream_duration_ms,
        ttft_ms,
        cached_tokens: usage.as_ref().and_then(|u| u.cached_tokens),
        cache_creation_tokens: usage.as_ref().and_then(|u| u.cache_creation_tokens),
        reasoning_tokens: usage.as_ref().and_then(|u| u.reasoning_tokens),
        usage,
        user_agent,
        client_ip,
        error_category: None,
        // Internal analytics identity fields: not populated yet (dispatch
        // wiring is a later phase); kept None for now.
        route_id: None,
        target_id: None,
    }
}

/// Build a `CallLog` for a non-success response.
#[allow(clippy::too_many_arguments)]
pub(in crate::dispatch) fn failure_log(
    request_id: uuid::Uuid,
    provider: &str,
    model: &str,
    real_model: &str,
    stream: bool,
    start: Instant,
    status_code: u16,
    err: &AppError,
    reasoning_effort: Option<&str>,
    user_agent: Option<&str>,
    client_ip: Option<&str>,
) -> CallLog {
    CallLog {
        request_id,
        provider: provider.to_string(),
        model: model.to_string(),
        real_model: real_model.to_string(),
        stream,
        status_code,
        duration_ms: start.elapsed().as_millis() as u64,
        usage: None,
        error: Some(err.to_string()),
        timestamp: chrono::Utc::now(),
        reasoning_effort: reasoning_effort.map(|s| s.to_string()),
        upstream_duration_ms: None,
        ttft_ms: None,
        cached_tokens: None,
        cache_creation_tokens: None,
        reasoning_tokens: None,
        user_agent: user_agent.map(|s| s.to_string()),
        client_ip: client_ip.map(|s| s.to_string()),
        error_category: Some(err.error_category().to_string()),
        // Internal analytics identity fields: not populated yet (dispatch
        // wiring is a later phase); kept None for now.
        route_id: None,
        target_id: None,
    }
}

/// Log a per-target failure so analytics can attribute it to the actual
/// provider / real_model that was attempted.
///
/// Stage 0.6 fix: previously the `failure_log` used empty provider /
/// real_model strings, which biased per-provider error rates toward zero.
/// Use this helper on every `apply_circuit_outcome` → `None` arm in dispatch
/// loops (chat, image, …) to keep attribution symmetric.
///
/// The most-recent entry in `errors` is recorded as the failure reason.
/// `error_category` is supplied by the caller based on the dispatch context
/// (e.g. `"provider_error"` for upstream failures, `"network_error"` for
/// transport errors, etc.) because the wrapped `AppError::Internal(...)` would
/// otherwise lose the original error classification.
#[allow(clippy::too_many_arguments)]
pub(in crate::dispatch) fn log_per_target_failure(
    state: &AppState,
    trace_id: uuid::Uuid,
    provider_name: &str,
    model: &str,
    real_model: &str,
    is_stream: bool,
    start: Instant,
    errors: &[String],
    error_category: &str,
    status_code: u16,
    reasoning_effort: Option<&str>,
    user_agent: Option<&str>,
    client_ip: Option<&str>,
) {
    let last_err = errors
        .last()
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());
    let mut log = failure_log(
        trace_id,
        provider_name,
        model,
        real_model,
        is_stream,
        start,
        status_code,
        &AppError::Internal(last_err),
        reasoning_effort,
        user_agent,
        client_ip,
    );
    // Override the error_category that `failure_log` derived from
    // `AppError::Internal` with the caller-supplied category so the
    // per-target row carries the real failure type.
    log.error_category = Some(error_category.to_string());
    state.collector.log(log);
}

/// Log a non-success entry and return the matching JSON error response.
#[allow(clippy::too_many_arguments)]
pub(in crate::dispatch) fn failure_response(
    state: &AppState,
    trace_id: uuid::Uuid,
    provider: &str,
    model: &str,
    real_model: &str,
    is_stream: bool,
    start: Instant,
    status_code: StatusCode,
    err: AppError,
    reasoning_effort: Option<&str>,
    user_agent: Option<&str>,
    client_ip: Option<&str>,
) -> Response<Body> {
    state.collector.log(failure_log(
        trace_id,
        provider,
        model,
        real_model,
        is_stream,
        start,
        status_code.as_u16(),
        &err,
        reasoning_effort,
        user_agent,
        client_ip,
    ));
    err.into_response()
}

/// Log a final "all providers exhausted" entry and return its response.
#[allow(clippy::too_many_arguments)]
pub(in crate::dispatch) fn fallback_exhausted_response(
    state: &AppState,
    model: &str,
    is_stream: bool,
    trace_id: uuid::Uuid,
    start: Instant,
    errors: &[String],
    reasoning_effort: Option<&str>,
    user_agent: Option<&str>,
    client_ip: Option<&str>,
) -> Response<Body> {
    let reason = errors.join("; ");
    let err = AppError::AllProvidersExhausted {
        model: model.to_string(),
        reason,
    };
    error!(
        model = %model,
        duration_ms = %start.elapsed().as_millis() as u64,
        attempts = %errors.join("; "),
        "all providers exhausted"
    );
    state.collector.log(failure_log(
        trace_id,
        "__all_exhausted__",
        model,
        "__all_exhausted__",
        is_stream,
        start,
        502,
        &err,
        reasoning_effort,
        user_agent,
        client_ip,
    ));
    err.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sag_config::{ConfigHandle, GatewayAuthConfig, GatewayConfig, ProviderConfig};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// Build an [`AppState`] whose formal event collector writes into a
    /// shared in-memory pool the test keeps a handle on — same wiring as
    /// `sag_state::testing::test_app_state`, minus the hidden pool. Draining
    /// the queue via `state.event_collector.shutdown().await` commits all
    /// pending events before the pool is read.
    ///
    /// Shared by the `chat` / `stream` dispatch test modules.
    pub(crate) async fn build_state_with_event_pool(
        providers: Vec<Arc<dyn sag_providers::LlmProvider>>,
    ) -> (AppState, sqlx::SqlitePool) {
        build_state_with_event_queue_capacity(
            providers,
            sag_analytics::DEFAULT_EVENT_QUEUE_CAPACITY,
        )
        .await
    }

    /// Same wiring as [`build_state_with_event_pool`] but with a
    /// configurable collector queue capacity: backpressure tests fill a
    /// tiny queue deterministically while the writer is blocked on the
    /// pool's single connection (`open_analytics_pool(":memory:")` uses
    /// `max_connections = 1`).
    pub(crate) async fn build_state_with_event_queue_capacity(
        providers: Vec<Arc<dyn sag_providers::LlmProvider>>,
        capacity: usize,
    ) -> (AppState, sqlx::SqlitePool) {
        let mut registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
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
            event_collector: Arc::new(sag_analytics::EventCollector::with_capacity(
                sag_analytics::EventWriter::new(pool.clone()),
                capacity,
            )),
            analytics_query: Some(Arc::new(sag_analytics::AnalyticsQueryEngine::new(
                pool.clone(),
            ))),
            circuit_breaker: Arc::new(RwLock::new(sag_core::RouteCircuitBreaker::default())),
            active_targets: Arc::default(),
            fill_first_orders: Arc::default(),
        };
        (state, pool)
    }

    /// Same wiring as [`build_state_with_event_queue_capacity`] but with a
    /// custom backoff config: the cancellation tests need a long,
    /// jitter-free backoff so a timeout lands deterministically inside the
    /// backoff sleep (no in-flight attempt) rather than inside a provider
    /// call.
    pub(crate) async fn build_state_with_event_pool_and_backoff(
        providers: Vec<Arc<dyn sag_providers::LlmProvider>>,
        backoff: sag_config::BackoffConfig,
    ) -> (AppState, sqlx::SqlitePool) {
        let mut registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
        for p in providers {
            registry.insert(p.name().to_string(), p);
        }
        let registry = Arc::new(RwLock::new(registry));
        let config = GatewayConfig {
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["k".into()],
            }),
            backoff,
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
            event_collector: Arc::new(sag_analytics::EventCollector::with_capacity(
                sag_analytics::EventWriter::new(pool.clone()),
                sag_analytics::DEFAULT_EVENT_QUEUE_CAPACITY,
            )),
            analytics_query: Some(Arc::new(sag_analytics::AnalyticsQueryEngine::new(
                pool.clone(),
            ))),
            circuit_breaker: Arc::new(RwLock::new(sag_core::RouteCircuitBreaker::default())),
            active_targets: Arc::default(),
            fill_first_orders: Arc::default(),
        };
        (state, pool)
    }

    fn make_provider(
        model_overrides: HashMap<String, HashMap<String, serde_json::Value>>,
    ) -> ProviderConfig {
        ProviderConfig {
            provider_type: sag_config::ProviderType::OpenAI,
            api_keys: vec!["sk-test".into()],
            key_strategy: sag_config::KeyStrategy::FillFirst,
            base_url: "https://api.openai.com/v1".into(),
            models_endpoint: "models".into(),
            timeout_secs: 120,
            chunk_timeout_secs: None,
            ttfb_timeout_secs: None,
            headers: HashMap::new(),
            body_overrides: HashMap::new(),
            strip_fields: vec![],
            model_strip_fields: HashMap::new(),
            model_overrides,
            role_mappings: HashMap::new(),
            enabled_models: vec![],
            enabled: true,
            anthropic_cache_control: false,
        }
    }

    #[test]
    fn merge_target_extra_overrides_model_override() {
        let mut mo = HashMap::new();
        mo.insert("max_tokens".into(), serde_json::json!(4096));
        let providers = HashMap::from([(
            "openai".into(),
            make_provider(HashMap::from([("gpt-4o".into(), mo)])),
        )]);

        let target_extra = HashMap::from([
            ("max_tokens".into(), serde_json::json!(2048)),
            ("temperature".into(), serde_json::json!(0.5)),
        ]);

        let merged = merge_model_overrides(&providers, "openai", "gpt-4o", &target_extra);
        assert_eq!(merged.get("max_tokens").unwrap(), 2048);
        assert_eq!(merged.get("temperature").unwrap(), 0.5);
    }

    #[test]
    fn merge_distinct_keys_accumulate() {
        let mut mo = HashMap::new();
        mo.insert("top_p".into(), serde_json::json!(0.9));
        let providers = HashMap::from([(
            "openai".into(),
            make_provider(HashMap::from([("gpt-4o".into(), mo)])),
        )]);

        let target_extra = HashMap::from([("temperature".into(), serde_json::json!(0.5))]);

        let merged = merge_model_overrides(&providers, "openai", "gpt-4o", &target_extra);
        assert_eq!(merged.get("top_p").unwrap(), 0.9);
        assert_eq!(merged.get("temperature").unwrap(), 0.5);
    }

    #[test]
    fn merge_no_model_override_returns_target_extra() {
        let providers = HashMap::from([("openai".into(), make_provider(HashMap::new()))]);

        let target_extra = HashMap::from([("temperature".into(), serde_json::json!(0.5))]);

        let merged = merge_model_overrides(&providers, "openai", "gpt-4o", &target_extra);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged.get("temperature").unwrap(), 0.5);
    }

    #[test]
    fn merge_empty_target_extra_returns_model_overrides() {
        let mut mo = HashMap::new();
        mo.insert("max_tokens".into(), serde_json::json!(4096));
        let providers = HashMap::from([(
            "openai".into(),
            make_provider(HashMap::from([("gpt-4o".into(), mo)])),
        )]);

        let merged = merge_model_overrides(&providers, "openai", "gpt-4o", &HashMap::new());
        assert_eq!(merged.len(), 1);
        assert_eq!(merged.get("max_tokens").unwrap(), 4096);
    }

    #[test]
    fn merge_both_empty_returns_empty() {
        let providers = HashMap::from([("openai".into(), make_provider(HashMap::new()))]);

        let merged = merge_model_overrides(&providers, "openai", "gpt-4o", &HashMap::new());
        assert!(merged.is_empty());
    }

    #[test]
    fn merge_nested_object_replaced_wholly() {
        let mut mo = HashMap::new();
        mo.insert(
            "response_format".into(),
            serde_json::json!({ "type": "text" }),
        );
        let providers = HashMap::from([(
            "openai".into(),
            make_provider(HashMap::from([("gpt-4o".into(), mo)])),
        )]);

        let target_extra = HashMap::from([(
            "response_format".into(),
            serde_json::json!({ "type": "json_object" }),
        )]);

        let merged = merge_model_overrides(&providers, "openai", "gpt-4o", &target_extra);
        assert_eq!(
            merged.get("response_format").unwrap(),
            &serde_json::json!({ "type": "json_object" })
        );
    }

    async fn build_test_state() -> AppState {
        let config = sag_config::GatewayConfig {
            routes: vec![],
            ..sag_config::gateway_config_base!()
        };
        let handle = sag_config::ConfigHandle::from_config(config);
        let registry = Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        sag_state::testing::test_app_state(handle, registry).await
    }

    #[tokio::test]
    async fn failure_response_preserves_error_category() {
        let state = build_test_state().await;

        let resp = failure_response(
            &state,
            uuid::Uuid::new_v4(),
            "openai",
            "gpt-4o",
            "gpt-4o",
            false,
            Instant::now(),
            StatusCode::BAD_GATEWAY,
            AppError::AllProvidersExhausted {
                model: "gpt-4o".into(),
                reason: "no targets".into(),
            },
            Some("high"),
            None,
            None,
        );

        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn fallback_exhausted_response_logs_sentinel() {
        let state = build_test_state().await;

        let trace_id = uuid::Uuid::new_v4();
        let resp = fallback_exhausted_response(
            &state,
            "gpt-4o",
            false,
            trace_id,
            Instant::now(),
            &["openai: 500".into()],
            Some("high"),
            None,
            None,
        );

        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

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

        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].provider, "__all_exhausted__");
        assert_eq!(logs[0].real_model, "__all_exhausted__");
        assert_eq!(logs[0].reasoning_effort.as_deref(), Some("high"));
    }

    #[tokio::test]
    async fn log_per_target_failure_preserves_reasoning_effort() {
        let state = build_test_state().await;

        let trace_id = uuid::Uuid::new_v4();
        let start = std::time::Instant::now();

        log_per_target_failure(
            &state,
            trace_id,
            "openai",
            "gpt-4o",
            "gpt-4o",
            false,
            start,
            &["upstream 500".into()],
            "provider_error",
            502,
            Some("medium"),
            None,
            None,
        );

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
        assert_eq!(logs.len(), 1);
        assert_eq!(
            logs[0].reasoning_effort.as_deref(),
            Some("medium"),
            "reasoning_effort must round-trip through log_per_target_failure"
        );
    }

    #[tokio::test]
    async fn log_per_target_failure_preserves_status_code() {
        let state = build_test_state().await;

        let trace_id = uuid::Uuid::new_v4();
        let start = std::time::Instant::now();

        log_per_target_failure(
            &state,
            trace_id,
            "openai",
            "gpt-4o",
            "gpt-4o",
            false,
            start,
            &["upstream 429".into()],
            "provider_error",
            429,
            None,
            None,
            None,
        );

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
        assert_eq!(logs.len(), 1);
        // Regression guard: if log_per_target_failure is changed to hardcode
        // 502 again, this assertion fails (429 != 502).
        assert_eq!(
            logs[0].status_code, 429,
            "status_code must round-trip through log_per_target_failure (not hardcoded 502)"
        );
    }

    // ── DispatchCancelGuard unit contract ────────────────────────

    /// The guard dropped with no in-flight attempt (retry/fallback gap)
    /// emits exactly one `client_cancelled` fact with **zero** target
    /// attempts (spec scenario 11: 退避/fallback 间隙 → 0 条).
    #[tokio::test]
    async fn dispatch_cancel_guard_no_in_flight_emits_client_cancelled_without_attempts() {
        let (state, pool) = build_state_with_event_pool(vec![]).await;
        let trace_id = uuid::Uuid::new_v4();
        let started_at = chrono::Utc::now();
        let guard = DispatchCancelGuard::new(
            state.event_collector.clone(),
            trace_id,
            "test-route",
            "gpt-4o",
            false,
            started_at,
        );
        drop(guard);

        state.event_collector.shutdown().await;

        let (final_status, failure_reason, route_name, stream, started_ms): (
            String,
            Option<String>,
            String,
            i32,
            i64,
        ) = sqlx::query_as(
            "SELECT rr.final_status, rr.failure_reason, ar.route_name, rr.stream, rr.started_at \
             FROM request_results rr \
             LEFT JOIN analytics_routes ar ON ar.route_id = rr.route_id \
             WHERE rr.request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("cancel guard must persist a client_cancelled fact");
        assert_eq!(final_status, "client_cancelled");
        assert_eq!(failure_reason.as_deref(), Some("client_cancelled"));
        assert_eq!(route_name, "test-route");
        assert_eq!(stream, 0);
        assert_eq!(started_ms, started_at.timestamp_millis());

        let attempts: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM target_attempts WHERE request_id = ?")
                .bind(trace_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(attempts, 0, "no in-flight attempt → 0 target attempts");
    }

    /// The guard dropped with an in-flight attempt (client disconnect
    /// during an actual provider call) emits one `client_cancelled` fact
    /// plus one `cancelled` target attempt (spec: 在途调用 → cancelled).
    #[tokio::test]
    async fn dispatch_cancel_guard_in_flight_emits_client_cancelled_with_cancelled_attempt() {
        let (state, pool) = build_state_with_event_pool(vec![]).await;
        let trace_id = uuid::Uuid::new_v4();
        let guard = DispatchCancelGuard::new(
            state.event_collector.clone(),
            trace_id,
            "test-route",
            "gpt-4o",
            true, // streaming
            chrono::Utc::now(),
        );
        guard.analytics().begin_attempt("openai", "gpt-4o");
        drop(guard);

        state.event_collector.shutdown().await;

        let (final_status, stream): (String, i32) =
            sqlx::query_as("SELECT final_status, stream FROM request_results WHERE request_id = ?")
                .bind(trace_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(final_status, "client_cancelled");
        assert_eq!(
            stream, 1,
            "streaming flag must round-trip through the guard"
        );

        let (status, actual_model, pt): (String, String, Option<i64>) = sqlx::query_as(
            "SELECT t.status, t.actual_model, t.prompt_tokens \
             FROM target_attempts t WHERE t.request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("in-flight attempt must be recorded as cancelled");
        assert_eq!(status, "cancelled");
        assert_eq!(actual_model, "gpt-4o");
        assert!(pt.is_none(), "cancelled attempt tokens stay NULL");
    }

    /// A guard marked done (normal terminal already emitted) must be a
    /// strict no-op on drop — no second request result, ever.
    #[tokio::test]
    async fn dispatch_cancel_guard_marked_done_emits_nothing() {
        let (state, pool) = build_state_with_event_pool(vec![]).await;
        let trace_id = uuid::Uuid::new_v4();
        let guard = DispatchCancelGuard::new(
            state.event_collector.clone(),
            trace_id,
            "test-route",
            "gpt-4o",
            false,
            chrono::Utc::now(),
        );
        guard.mark_done();
        guard.analytics().begin_attempt("openai", "gpt-4o");
        drop(guard);

        state.event_collector.shutdown().await;

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_results WHERE request_id = ?")
                .bind(trace_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 0, "mark_done must suppress the drop terminal");
    }

    // ── CircuitPermit / ProbeGuard RAII contract ───────────────────

    fn cb_config(probe_interval_secs: u64) -> sag_config::CircuitBreakerConfig {
        sag_config::CircuitBreakerConfig {
            quota_exhausted_secs: 3600,
            rate_limited_secs: 10,
            generic_secs: 60,
            probe_interval_secs,
        }
    }

    /// An un-consumed `ProbeGuard` dropped outside any tokio runtime must
    /// take the best-effort warn path — never panic (a naive unconditional
    /// `tokio::spawn` in `Drop` would panic with "there is no reactor
    /// running"). The slot is left for the stale-probe reclamation.
    #[test]
    fn probe_guard_drop_without_runtime_does_not_panic() {
        let rt = crate::testing::test_dispatch_runtime();
        let guard = ProbeGuard::new(&rt, RouteTargetKey::new("m", "p", "rm"), 60);
        drop(guard); // must not panic
    }

    /// A `ProbeGuard` whose single-completion marker is set drops as a strict
    /// no-op — it must not touch the runtime at all (`Drop` short-circuits
    /// before `Handle::try_current`, so this also holds without a runtime).
    #[test]
    fn probe_guard_marked_completed_drop_is_noop() {
        let rt = crate::testing::test_dispatch_runtime();
        let mut guard = ProbeGuard::new(&rt, RouteTargetKey::new("m", "p", "rm"), 60);
        guard.mark_completed();
        drop(guard); // no-op — must not panic even without a runtime
    }

    /// An un-consumed `ProbeGuard` dropped inside a tokio runtime releases
    /// the claimed slot through a detached task: after the drop the slot
    /// must become claimable again (`probe_in_flight` cleared), so the
    /// best-effort `Drop` path really does release.
    #[tokio::test]
    async fn probe_guard_drop_with_runtime_releases_slot_detached() {
        let rt = crate::testing::test_dispatch_runtime();
        let cfg = cb_config(0); // interval 0 → the detached release lands immediately
        rt.circuit_breaker
            .write()
            .await
            .open(RouteTargetKey::new("m", "p", "rm"), 3600, 0);

        let permit = circuit_allows_target(&rt, "m", "p", "rm", &cfg)
            .await
            .expect("half-open probe should be granted");
        let CircuitPermit::Probe(guard) = permit else {
            panic!("expected a probe permit");
        };
        // Drop un-consumed: the guard's Drop must detach a release.
        drop(guard);

        // Poll until the detached release lands (interval 0 → the reschedule
        // is due immediately, so the slot is claimable again after the write).
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let mut breaker = rt.circuit_breaker.write().await;
            if breaker.to_half_open_if_due(&RouteTargetKey::new("m", "p", "rm"))
                && breaker.try_acquire_probe(&RouteTargetKey::new("m", "p", "rm"))
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "detached drop release never landed"
            );
            drop(breaker);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// A consumed probe failure records exactly one `record_probe_result`:
    /// the guard's `Drop` after `apply_circuit_outcome` is a strict no-op
    /// (already `mark_completed`), so the entry ends up `Open` with
    /// `probe_in_flight` cleared and a single rescheduled `next_probe_at`.
    #[tokio::test]
    async fn apply_circuit_outcome_consumed_probe_failure_reschedules_once() {
        let rt = crate::testing::test_dispatch_runtime();
        let cfg = cb_config(5);
        let key = RouteTargetKey::new("m", "p", "rm");
        rt.circuit_breaker.write().await.open(key.clone(), 3600, 0);

        let permit = circuit_allows_target(&rt, "m", "p", "rm", &cfg)
            .await
            .expect("probe should be granted");
        assert!(
            matches!(&permit, CircuitPermit::Probe(_)),
            "expected a probe permit"
        );

        let result = apply_circuit_outcome(
            &rt,
            permit,
            RetryOutcome::<u32>::Exhausted(AppError::Upstream {
                provider: "p".into(),
                status: 503,
                body: "boom".into(),
            }),
            &cfg,
        )
        .await;
        assert!(result.is_none());

        let breaker = rt.circuit_breaker.read().await;
        assert!(
            !breaker.can_probe_now(&key),
            "failed probe must reschedule, not leave the slot probeable"
        );
        let next_in = breaker
            .next_probe_in(&key)
            .expect("Open entry must have a next probe");
        assert!(
            next_in.as_millis() >= 4900 && next_in.as_millis() <= 8000,
            "failed probe reschedules ~interval (5s), got {next_in:?}"
        );
        let entry = breaker
            .snapshot()
            .into_iter()
            .find(|e| e.model == "m")
            .expect("entry must still exist after probe failure");
        assert_eq!(entry.state, sag_core::CircuitStateKind::Open);
        assert!(
            !entry.probe_in_flight,
            "consumed guard must release the slot exactly once"
        );
    }

    /// `Success` on both an `Allowed` (healthy) and a `Probe` target closes
    /// the circuit (a no-op for the healthy one, a recovery for the probe).
    #[tokio::test]
    async fn apply_circuit_outcome_success_closes_healthy_and_probe() {
        let rt = crate::testing::test_dispatch_runtime();
        let cfg = cb_config(5);
        let key = RouteTargetKey::new("m", "p", "rm");

        // Healthy (Allowed) + Success: no entry is created, stays closed.
        let out = apply_circuit_outcome(
            &rt,
            CircuitPermit::Allowed { key: key.clone() },
            RetryOutcome::Success(42u32),
            &cfg,
        )
        .await;
        assert_eq!(out, Some(42));
        assert!(
            rt.circuit_breaker.read().await.is_closed(&key),
            "healthy success must stay closed"
        );

        // Probe + Success: the half-open entry is closed (recovery).
        rt.circuit_breaker.write().await.open(key.clone(), 3600, 0);
        let permit = circuit_allows_target(&rt, "m", "p", "rm", &cfg)
            .await
            .expect("probe should be granted");
        let out = apply_circuit_outcome(&rt, permit, RetryOutcome::Success(7u32), &cfg).await;
        assert_eq!(out, Some(7));
        assert!(
            rt.circuit_breaker.read().await.is_closed(&key),
            "successful probe must close the circuit"
        );
    }

    /// `Exhausted` on a healthy (`Allowed`) target opens the circuit with the
    /// error-derived cooldown as the hard lower bound.
    #[tokio::test]
    async fn apply_circuit_outcome_exhausted_opens_healthy_with_duration() {
        let rt = crate::testing::test_dispatch_runtime();
        let cfg = cb_config(5);
        let key = RouteTargetKey::new("m", "p", "rm");

        let out = apply_circuit_outcome(
            &rt,
            CircuitPermit::Allowed { key: key.clone() },
            RetryOutcome::<u32>::Exhausted(AppError::Upstream {
                provider: "p".into(),
                status: 503,
                body: "boom".into(),
            }),
            &cfg,
        )
        .await;
        assert!(out.is_none());

        let breaker = rt.circuit_breaker.read().await;
        let entry = breaker
            .snapshot()
            .into_iter()
            .find(|e| e.model == "m")
            .expect("exhausted healthy target must open");
        assert_eq!(entry.state, sag_core::CircuitStateKind::Open);
        assert!(
            !entry.probe_in_flight,
            "healthy open does not hold a probe slot"
        );
    }

    /// A non-retryable (client-caused) 4xx on a healthy target must **not**
    /// open the circuit — the asymmetry invariant that keeps client errors
    /// from poisoning a healthy target.
    #[tokio::test]
    async fn apply_circuit_outcome_non_retryable_healthy_does_not_open() {
        let rt = crate::testing::test_dispatch_runtime();
        let cfg = cb_config(5);
        let key = RouteTargetKey::new("m", "p", "rm");

        let out = apply_circuit_outcome(
            &rt,
            CircuitPermit::Allowed { key: key.clone() },
            RetryOutcome::<u32>::NonRetryable(AppError::BadRequest("client 4xx".into())),
            &cfg,
        )
        .await;
        assert!(out.is_none());
        assert!(
            rt.circuit_breaker.read().await.is_closed(&key),
            "client 4xx on a healthy target must not open the circuit"
        );
    }

    /// A non-retryable (4xx) during a probe is treated as a failed probe:
    /// the next probe is pushed one interval later and the slot is released
    /// — never left half-open forever, and never a full cooldown.
    #[tokio::test]
    async fn apply_circuit_outcome_non_retryable_probe_reschedules_not_cooldown() {
        let rt = crate::testing::test_dispatch_runtime();
        let cfg = cb_config(5);
        let key = RouteTargetKey::new("m", "p", "rm");
        rt.circuit_breaker.write().await.open(key.clone(), 3600, 0);

        let permit = circuit_allows_target(&rt, "m", "p", "rm", &cfg)
            .await
            .expect("probe should be granted");
        let out = apply_circuit_outcome(
            &rt,
            permit,
            RetryOutcome::<u32>::NonRetryable(AppError::BadRequest("client 4xx".into())),
            &cfg,
        )
        .await;
        assert!(out.is_none());

        let breaker = rt.circuit_breaker.read().await;
        let next_in = breaker
            .next_probe_in(&key)
            .expect("4xx probe must reschedule the next probe");
        assert!(
            next_in.as_millis() >= 4900 && next_in.as_millis() <= 8000,
            "4xx probe must reschedule ~interval (5s), not open a full cooldown, got {next_in:?}"
        );
        let entry = breaker
            .snapshot()
            .into_iter()
            .find(|e| e.model == "m")
            .expect("entry must still exist after the failed 4xx probe");
        assert!(!entry.probe_in_flight, "4xx probe must release the slot");
    }

    /// `release_probe_slot` (the provider-unresolved early-skip path) is a
    /// consuming release: it marks the guard completed before awaiting, so
    /// the guard's drop after the call can never record a second time. The
    /// slot is synchronously claimable once the future resolves.
    #[tokio::test]
    async fn probe_guard_release_probe_slot_releases_once() {
        let rt = crate::testing::test_dispatch_runtime();
        let cfg = cb_config(5);
        let key = RouteTargetKey::new("m", "p", "rm");
        rt.circuit_breaker.write().await.open(key.clone(), 3600, 0);

        let permit = circuit_allows_target(&rt, "m", "p", "rm", &cfg)
            .await
            .expect("probe should be granted");
        let CircuitPermit::Probe(guard) = permit else {
            panic!("expected a probe permit");
        };
        guard.release_probe_slot().await;

        let breaker = rt.circuit_breaker.read().await;
        let next_in = breaker
            .next_probe_in(&key)
            .expect("released probe must reschedule the next probe");
        assert!(
            next_in.as_millis() >= 4900 && next_in.as_millis() <= 8000,
            "released probe reschedules ~interval (5s), got {next_in:?}"
        );
        let entry = breaker
            .snapshot()
            .into_iter()
            .find(|e| e.model == "m")
            .expect("entry must still exist after the release");
        assert!(
            !entry.probe_in_flight,
            "release_probe_slot must release the slot exactly once"
        );
    }
}
