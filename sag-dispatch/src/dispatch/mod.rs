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

/// Check whether a target's circuit is open. Returns `Some(key)` if the
/// target is healthy, `None` if the circuit is still open (in cooldown).
pub(in crate::dispatch) async fn circuit_allows_target(
    runtime: &DispatchRuntime,
    model: &str,
    provider_name: &str,
    real_model: &str,
) -> Option<RouteTargetKey> {
    let circuit_key = RouteTargetKey::new(model, provider_name, real_model);
    let breaker = runtime.route_circuit_breaker.read().await;

    if let Some(remaining) = breaker.open_remaining(&circuit_key) {
        info!(
            provider = %provider_name,
            real_model = %real_model,
            remaining_ms = remaining.as_millis() as u64,
            "skipping open route target circuit"
        );
        return None;
    }

    Some(circuit_key)
}

pub(in crate::dispatch) async fn apply_circuit_outcome<T>(
    runtime: &DispatchRuntime,
    circuit_key: RouteTargetKey,
    outcome: RetryOutcome<T>,
    circuit_breaker_cfg: &sag_config::CircuitBreakerConfig,
) -> Option<T> {
    match outcome {
        RetryOutcome::Success(value) => {
            runtime
                .route_circuit_breaker
                .write()
                .await
                .close(&circuit_key);
            Some(value)
        }
        RetryOutcome::Exhausted(last_err) => {
            let hint = last_err.circuit_hint();
            let duration_secs = hint.duration_secs(
                circuit_breaker_cfg.quota_exhausted_secs,
                circuit_breaker_cfg.rate_limited_secs,
                circuit_breaker_cfg.generic_secs,
            );
            info!(
                hint = ?hint,
                duration_secs,
                "opening circuit with error-based duration"
            );
            runtime
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
