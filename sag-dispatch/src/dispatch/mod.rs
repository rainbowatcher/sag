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
use sag_core::{AppError, CallLog, ChatCompletionRequest, Usage};
use tracing::{error, info};

use crate::circuit_breaker::RouteTargetKey;
use crate::dispatch::retry::RetryOutcome;
use crate::state::{AppState, DispatchRuntime};

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
    pub extra: HashMap<String, serde_json::Value>,
    pub retries: u32,
    pub timeout_secs: u64,
}

/// Resolved route data: (ordered targets, configured `api_endpoint`, strategy).
pub type ResolvedRoute = (Vec<ResolvedTarget>, String, Strategy);

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
    pub errors: &'a mut Vec<String>,
    pub strategy: Strategy,
    pub user_agent: Option<String>,
    pub client_ip: Option<String>,
}

// ═══════════════════════════════════════════════════════════════════
// Circuit breaker helpers — shared across the dispatch submodules.
// ═══════════════════════════════════════════════════════════════════

/// Verdict returned by [`circuit_allows_target`].
///
/// `key` is the resolved target identity; `is_probe` is `true` when the
/// target's circuit is `HalfOpen` and this request was granted the single
/// probe slot — meaning the attempt that follows should be treated as a
/// recovery probe (its outcome re-opens the circuit on failure rather than
/// just opening it for the first time).
pub(in crate::dispatch) struct CircuitVerdict {
    pub key: RouteTargetKey,
    pub is_probe: bool,
}

/// Check whether a target's circuit allows a try.
///
/// Returns `None` if the target is `Open` (cooling down) or is `HalfOpen`
/// with a probe already in flight — in both cases the caller should skip to
/// the next target. Returns `Some(CircuitVerdict)` when the target may be
/// tried: `is_probe` is `true` iff this request was handed the half-open
/// probe slot.
pub(in crate::dispatch) async fn circuit_allows_target(
    runtime: &DispatchRuntime,
    model: &str,
    provider_name: &str,
    real_model: &str,
) -> Option<CircuitVerdict> {
    let circuit_key = RouteTargetKey::new(model, provider_name, real_model);

    // Single read pass. `can_probe_now` returns true for a healthy (Closed)
    // target as well as a due `Open`/`HalfOpen` one.
    {
        let breaker = runtime.route_circuit_breaker.read().await;
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
            return Some(CircuitVerdict {
                key: circuit_key,
                is_probe: false,
            });
        }
    }

    // Upgrade to a write pass to transition a due `Open` to `HalfOpen`
    // and claim the single probe slot.
    let mut breaker = runtime.route_circuit_breaker.write().await;
    if !breaker.to_half_open_if_due(&circuit_key) {
        // Not due after the transition check (race) — treat as skipped.
        return None;
    }

    let is_probe = breaker.try_acquire_probe(&circuit_key);
    if is_probe {
        info!(
            provider = %provider_name,
            real_model = %real_model,
            "half-open circuit probe allowed"
        );
    } else {
        // Half-open but a probe is already in flight: skip this request.
        info!(
            provider = %provider_name,
            real_model = %real_model,
            "skipping route target: half-open probe already in flight"
        );
        return None;
    }

    Some(CircuitVerdict {
        key: circuit_key,
        is_probe,
    })
}

/// Apply the outcome of a single target attempt to the circuit breaker.
///
/// # Asymmetry note (intentional)
///
/// A non-retryable (4xx) error on a *healthy* target does **not** open the
/// circuit — the target stays usable, because a 4xx is usually a client-side
/// request problem, not a target fault. The same 4xx arriving during a
/// *probe* (`is_probe = true`) instead re-schedules the next probe one
/// interval later (effectively a short cooldown). This asymmetry is
/// deliberate: a probe must always report an outcome, and a 4xx during a probe
/// means "not usable right now", so it is treated as a failed probe rather
/// than left half-open forever. The practical effect — a target that returns
/// 4xx to probe requests is briefly skipped — is acceptable and self-heals on
/// the next interval.
pub(in crate::dispatch) async fn apply_circuit_outcome<T>(
    runtime: &DispatchRuntime,
    circuit_key: RouteTargetKey,
    outcome: RetryOutcome<T>,
    circuit_breaker_cfg: &sag_config::CircuitBreakerConfig,
    is_probe: bool,
) -> Option<T> {
    let interval = circuit_breaker_cfg.probe_interval_secs;
    match outcome {
        RetryOutcome::Success(value) => {
            runtime
                .route_circuit_breaker
                .write()
                .await
                .close(&circuit_key);
            if is_probe {
                info!(
                    provider = %circuit_key.provider,
                    real_model = %circuit_key.real_model,
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
                    provider = %circuit_key.provider,
                    real_model = %circuit_key.real_model,
                    hint = ?hint,
                    interval_secs = interval,
                    "half-open circuit probe failed, rescheduling next probe"
                );
                runtime
                    .route_circuit_breaker
                    .write()
                    .await
                    .record_probe_result(circuit_key, false, interval);
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
                runtime.route_circuit_breaker.write().await.open(
                    circuit_key,
                    duration_secs,
                    interval,
                );
            }
            None
        }
        RetryOutcome::NonRetryable => {
            if is_probe {
                // A non-retryable (4xx) probe means the target is not usable
                // right now — reschedule the next probe one interval later
                // rather than leaving it half-open forever.
                runtime
                    .route_circuit_breaker
                    .write()
                    .await
                    .record_probe_result(circuit_key, false, interval);
            }
            None
        }
    }
}

/// Release a half-open probe slot without having actually attempted the
/// upstream.
///
/// Claiming a probe slot (via [`circuit_allows_target`]) creates an obligation
/// to report the probe outcome through [`apply_circuit_outcome`]. Some early
/// exits between claiming the slot and the retry loop — notably when
/// [`resolve_provider`](crate::dispatch::resolve::resolve_provider) returns
/// `None` because the provider is disabled or no longer registered — skip that
/// call. Without this release the `HalfOpen` + `probe_in_flight = true` entry
/// would never be cleared (`try_acquire_probe` would reject every later
/// request), permanently skipping the target. Treating the un-attempted probe
/// as a failure reschedules the next probe one interval later, matching the
/// normal failed-probe path.
pub(in crate::dispatch) async fn release_probe_slot(
    runtime: &DispatchRuntime,
    circuit_key: RouteTargetKey,
    probe_interval_secs: u64,
) {
    runtime
        .route_circuit_breaker
        .write()
        .await
        .record_probe_result(circuit_key, false, probe_interval_secs);
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
// Logging / response helpers — shared across the dispatch submodules.
// ═══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use sag_config::ProviderConfig;
    use std::sync::Arc;
    use tokio::sync::RwLock;

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
            model_overrides,
            role_mappings: HashMap::new(),
            enabled_models: vec![],
            enabled: true,
            anthropic_cache_control: false,
        }
    }

    #[test]
    fn merge_target_extra_overrides_model_override() {
        // Model override sets max_tokens=4096; target extra sets
        // max_tokens=2048 and adds temperature=0.5. The target value
        // must win for the shared key, and the model-only key must
        // still be present.
        let mut mo = HashMap::new();
        mo.insert("max_tokens".into(), serde_json::json!(4096));
        let providers = HashMap::from([(
            "openai".into(),
            make_provider(HashMap::from([("gpt-4o".into(), mo)])),
        )]);

        let mut target_extra = HashMap::new();
        target_extra.insert("max_tokens".into(), serde_json::json!(2048));
        target_extra.insert("temperature".into(), serde_json::json!(0.5));

        let merged = merge_model_overrides(&providers, "openai", "gpt-4o", &target_extra);

        assert_eq!(
            merged.get("max_tokens").and_then(|v| v.as_i64()),
            Some(2048),
            "target extra should override model override for shared key"
        );
        assert_eq!(
            merged.get("temperature").and_then(|v| v.as_f64()),
            Some(0.5),
            "target-only key should be present"
        );
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merge_no_model_override_returns_target_extra() {
        // When the provider has no model_overrides for this real_model,
        // the result should be a clone of target_extra.
        let providers = HashMap::from([("openai".into(), make_provider(HashMap::new()))]);

        let mut target_extra = HashMap::new();
        target_extra.insert("temperature".into(), serde_json::json!(0.7));

        let merged = merge_model_overrides(&providers, "openai", "gpt-4o", &target_extra);

        assert_eq!(merged, target_extra);
    }

    #[test]
    fn merge_empty_target_extra_returns_model_overrides() {
        // When target_extra is empty, the model overrides should pass
        // through as-is.
        let mut mo = HashMap::new();
        mo.insert("max_tokens".into(), serde_json::json!(4096));
        let providers = HashMap::from([(
            "openai".into(),
            make_provider(HashMap::from([("gpt-4o".into(), mo.clone())])),
        )]);

        let merged = merge_model_overrides(&providers, "openai", "gpt-4o", &HashMap::new());

        assert_eq!(merged, mo);
    }

    #[test]
    fn merge_nested_object_replaced_wholly() {
        // Model override sets thinking = { type: "enabled" };
        // target extra sets thinking = { budget_tokens: 1600 }.
        // The target value must replace the model value entirely
        // (shallow merge, not deep merge).
        let mut mo = HashMap::new();
        mo.insert("thinking".into(), serde_json::json!({ "type": "enabled" }));
        let providers = HashMap::from([(
            "openai".into(),
            make_provider(HashMap::from([("gpt-4o".into(), mo)])),
        )]);

        let mut target_extra = HashMap::new();
        target_extra.insert(
            "thinking".into(),
            serde_json::json!({ "budget_tokens": 1600 }),
        );

        let merged = merge_model_overrides(&providers, "openai", "gpt-4o", &target_extra);

        let thinking = merged.get("thinking").unwrap();
        assert_eq!(
            thinking.get("budget_tokens").and_then(|v| v.as_i64()),
            Some(1600),
            "target-extra nested object should win"
        );
        assert!(
            thinking.get("type").is_none(),
            "nested object should be replaced wholly, not deep-merged"
        );
    }

    #[test]
    fn merge_distinct_keys_accumulate() {
        // Model override has key A, target extra has key B.
        // Result should contain both keys.
        let mut mo = HashMap::new();
        mo.insert("max_tokens".into(), serde_json::json!(4096));
        let providers = HashMap::from([(
            "openai".into(),
            make_provider(HashMap::from([("gpt-4o".into(), mo)])),
        )]);

        let mut target_extra = HashMap::new();
        target_extra.insert("temperature".into(), serde_json::json!(0.7));

        let merged = merge_model_overrides(&providers, "openai", "gpt-4o", &target_extra);

        assert_eq!(
            merged.get("max_tokens").and_then(|v| v.as_i64()),
            Some(4096),
            "model-only key should be present"
        );
        assert_eq!(
            merged.get("temperature").and_then(|v| v.as_f64()),
            Some(0.7),
            "target-only key should be present"
        );
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn merge_both_empty_returns_empty() {
        // When both model_overrides and target_extra are empty,
        // the result should be an empty map.
        let providers = HashMap::from([("openai".into(), make_provider(HashMap::new()))]);
        let merged = merge_model_overrides(&providers, "openai", "gpt-4o", &HashMap::new());
        assert!(merged.is_empty());
    }

    // ── Sentinel value & error_category tests ────────────────────

    async fn build_test_state() -> AppState {
        let config = sag_config::GatewayConfig {
            routes: vec![],
            ..sag_config::gateway_config_base!()
        };
        let handle = sag_config::ConfigHandle::from_config(config);
        let registry = Arc::new(RwLock::new(HashMap::new()));
        sag_state::testing::test_app_state(handle, registry).await
    }

    #[tokio::test]
    async fn fallback_exhausted_response_logs_sentinel() {
        let state = build_test_state().await;
        let trace_id = uuid::Uuid::new_v4();
        let start = std::time::Instant::now();

        fallback_exhausted_response(
            &state,
            "gpt-4o",
            false,
            trace_id,
            start,
            &["openai: timeout".into(), "anthropic: 500".into()],
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
        assert_eq!(logs.len(), 1, "one call log should be written");
        let log = &logs[0];
        assert_eq!(log.provider, "__all_exhausted__");
        assert_eq!(log.real_model, "__all_exhausted__");
        assert_eq!(log.model, "gpt-4o");
        assert_eq!(log.status_code, 502);
        assert_eq!(log.error_category.as_deref(), Some("all_exhausted"));
    }

    #[tokio::test]
    async fn failure_response_preserves_error_category() {
        let state = build_test_state().await;
        let trace_id = uuid::Uuid::new_v4();
        let start = std::time::Instant::now();

        failure_response(
            &state,
            trace_id,
            "openai",
            "gpt-4o",
            "gpt-4o",
            false,
            start,
            StatusCode::BAD_REQUEST,
            AppError::BadRequest("unsupported param".into()),
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
            logs[0].error_category.as_deref(),
            Some("bad_request"),
            "error_category must reflect the original AppError variant"
        );
    }

    #[tokio::test]
    async fn log_per_target_failure_uses_caller_error_category() {
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
            logs[0].error_category.as_deref(),
            Some("provider_error"),
            "error_category must be the caller-supplied value, not 'internal_error'"
        );
    }
}

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
        reasoning_effort: None,
        upstream_duration_ms: None,
        ttft_ms: None,
        cached_tokens: None,
        cache_creation_tokens: None,
        reasoning_tokens: None,
        user_agent: user_agent.map(|s| s.to_string()),
        client_ip: client_ip.map(|s| s.to_string()),
        error_category: Some(err.error_category().to_string()),
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
        502,
        &AppError::Internal(last_err),
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
        user_agent,
        client_ip,
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
        user_agent,
        client_ip,
    ));
    err.into_response()
}
