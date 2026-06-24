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
use sag_config::Strategy;
use sag_core::{AppError, CallLog, ChatCompletionRequest, Usage};
use tracing::{error, info};

use crate::circuit_breaker::RouteTargetKey;
use crate::dispatch::retry::RetryOutcome;
use crate::state::AppState;

pub mod chat;
pub mod image;
pub mod resolve;
pub mod retry;
pub mod stream;

pub use chat::dispatch_chat;
pub use image::dispatch_image;
pub use resolve::resolve_route;

/// A single resolved fallback target: (provider, real_model, extra, retries).
pub type ResolvedTarget = (String, String, HashMap<String, serde_json::Value>, u32);

/// Resolved route data: (ordered targets, configured `api_endpoint`, strategy).
pub type ResolvedRoute = (Vec<ResolvedTarget>, String, Strategy);

/// Bundled parameters for a single dispatch call. Replaces the 7–8
/// argument lists on [`dispatch_chat`] / [`dispatch_image`] so the
/// function signatures stay readable as the dispatch surface grows.
pub struct DispatchCtx<'a> {
    pub state: &'a AppState,
    pub request: &'a ChatCompletionRequest,
    pub targets: &'a [ResolvedTarget],
    pub api_endpoint: &'a str,
    pub is_stream: bool,
    pub trace_id: uuid::Uuid,
    pub start: Instant,
    pub errors: &'a mut Vec<String>,
    /// The route's target-selection strategy. Used by the dispatch loop
    /// to decide whether to skip penalized targets (FillFirst) or
    /// include them in rotation (RoundRobin).
    pub strategy: Strategy,
}

// ═══════════════════════════════════════════════════════════════════
// Circuit breaker helpers — shared across the dispatch submodules.
// ═══════════════════════════════════════════════════════════════════

/// Result of checking a target against the circuit breaker.
///
/// Replaces the previous `Option<RouteTargetKey>` return — callers now
/// match on the reason a target was skipped instead of string-matching
/// on error messages to detect penalized targets.
#[derive(Debug)]
pub(in crate::dispatch) enum TargetAvailability {
    /// Target is healthy and may be tried.
    Allowed(RouteTargetKey),
    /// Circuit is still open (inside the cooldown window).
    CircuitOpen,
    /// FillFirst penalized — circuit closed but target not yet
    /// promoted back to primary.
    Penalized,
}

/// Check whether a target is available (circuit closed, not penalized).
/// Returns the decision enum instead of a bare Option so callers can
/// distinguish the skip reason without string-matching on error messages.
pub(in crate::dispatch) async fn circuit_check_target(
    state: &AppState,
    model: &str,
    provider_name: &str,
    real_model: &str,
    strategy: Strategy,
) -> TargetAvailability {
    let circuit_key = RouteTargetKey::new(model, provider_name, real_model);
    let breaker = state.route_circuit_breaker.read().await;

    // Still-open circuit → unconditionally skip.
    if let Some(remaining) = breaker.open_remaining(&circuit_key) {
        info!(
            provider = %provider_name,
            real_model = %real_model,
            remaining_ms = remaining.as_millis() as u64,
            "skipping open route target circuit"
        );
        return TargetAvailability::CircuitOpen;
    }

    // FillFirst: skip penalized targets in the first pass.
    if strategy == Strategy::FillFirst && breaker.is_penalized(model, &circuit_key) {
        info!(
            provider = %provider_name,
            real_model = %real_model,
            "skipping penalized route target (fill-first)"
        );
        return TargetAvailability::Penalized;
    }

    TargetAvailability::Allowed(circuit_key)
}

/// Two-pass dispatch state machine shared by [`dispatch_chat`] and
/// [`dispatch_image`]. Handles the FillFirst penalized-target lifecycle
/// so dispatch loops don't duplicate pass-management logic.
pub(in crate::dispatch) struct TargetPass {
    strategy: Strategy,
    pass: u32,
    penalized_skipped: bool,
    max_pass: u32,
}

impl TargetPass {
    pub fn new(strategy: Strategy) -> Self {
        let max_pass = if strategy == Strategy::FillFirst {
            2
        } else {
            1
        };
        Self {
            strategy,
            pass: 0,
            penalized_skipped: false,
            max_pass,
        }
    }

    /// Track that a target was skipped on the current pass.
    pub fn record_skip(&mut self, availability: &TargetAvailability) {
        if self.pass == 0 && matches!(availability, TargetAvailability::Penalized) {
            self.penalized_skipped = true;
        }
    }

    /// Advance to the next pass, clearing penalized targets if
    /// entering pass 2. Returns `true` if there are more targets
    /// to try, `false` if dispatch is done.
    pub async fn advance(
        &mut self,
        state: &AppState,
        model: &str,
        errors: &mut Vec<String>,
    ) -> bool {
        if self.pass + 1 >= self.max_pass {
            return false;
        }
        // FillFirst: only enter pass 2 if pass 1 actually skipped
        // penalized targets (otherwise every target was already tried).
        if self.strategy == Strategy::FillFirst && self.pass == 0 && !self.penalized_skipped {
            return false;
        }
        self.pass += 1;
        if self.pass == 1 {
            state
                .route_circuit_breaker
                .write()
                .await
                .clear_penalized(model);
            errors.push("clearing penalized targets for retry".into());
        }
        true
    }
}

pub(in crate::dispatch) async fn apply_circuit_outcome<T>(
    state: &AppState,
    circuit_key: RouteTargetKey,
    outcome: RetryOutcome<T>,
) -> Option<T> {
    match outcome {
        RetryOutcome::Success(value) => {
            state
                .route_circuit_breaker
                .write()
                .await
                .close(&circuit_key);
            Some(value)
        }
        RetryOutcome::Exhausted(last_err) => {
            let hint = last_err.circuit_hint();
            let duration_secs = hint.default_duration_secs();
            info!(
                hint = ?hint,
                duration_secs,
                "opening circuit with error-based duration"
            );
            state
                .route_circuit_breaker
                .write()
                .await
                .open(circuit_key, duration_secs);
            None
        }
        RetryOutcome::NonRetryable => None,
    }
}

// ═══════════════════════════════════════════════════════════════════
// Logging / response helpers — shared across the dispatch submodules.
// ═══════════════════════════════════════════════════════════════════

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
        reasoning_effort: None,
        upstream_duration_ms: None,
        ttft_ms: None,
        cached_tokens: None,
        cache_creation_tokens: None,
        reasoning_tokens: None,
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
) {
    let last_err = errors
        .last()
        .cloned()
        .unwrap_or_else(|| "unknown".to_string());
    state.collector.log(failure_log(
        trace_id,
        provider_name,
        model,
        real_model,
        is_stream,
        start,
        502,
        &AppError::Internal(last_err),
    ));
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
    ));
    err.into_response()
}

/// Log a final "all providers exhausted" entry and return its response.
pub(in crate::dispatch) fn fallback_exhausted_response(
    state: &AppState,
    model: &str,
    is_stream: bool,
    trace_id: uuid::Uuid,
    start: Instant,
    errors: &[String],
) -> Response<Body> {
    let reason = errors.join("; ");
    let err = AppError::AllProvidersExhausted {
        model: model.to_string(),
        reason,
    };
    error!(
        model = %model,
        trace_id = %trace_id,
        duration_ms = %start.elapsed().as_millis() as u64,
        attempts = %errors.join("; "),
        "all providers exhausted"
    );
    state.collector.log(failure_log(
        trace_id, "", model, "", is_stream, start, 502, &err,
    ));
    err.into_response()
}
