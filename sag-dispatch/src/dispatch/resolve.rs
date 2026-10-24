//! Route resolution: model → ordered fallback targets + `api_endpoint`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::{body::Body, http::StatusCode, response::Response};
use sag_analytics::AnalyticsEvent;
use sag_config::{ProviderConfig, RouteConfig, RouteTarget};
use sag_core::{AppError, ChatCompletionRequest, FailureReason, FinalStatus, RequestResultFact};
use sag_providers::LlmProvider;
use tracing::{info, warn};

use crate::dispatch::capability::{filter_capable_targets, used_protocol};
use crate::dispatch::{ResolvedRoute, ResolvedTarget, failure_response};
use crate::order::order_targets;
use crate::state::{AppState, DispatchRuntime};

/// Snapshot of provider fields needed to fill `ResolvedTarget` lane fields.
#[derive(Clone)]
struct ProviderLaneMeta {
    /// All `apis` keys (protocol keys plus the optional non-protocol
    /// `image` entry); [`sag_core::resolve_lane`] ignores non-protocol keys.
    apis_keys: Vec<sag_core::ApiKind>,
    /// Translated-lane target protocol — config validation guarantees it
    /// is one of the provider's protocol keys when set.
    fallback: Option<sag_core::ApiKind>,
}

/// Resolve a chat request to an ordered list of fallback targets and the
/// configured `api_endpoint`, carrying the matched `route_name` and the
/// wall-clock `started_at` (captured by the caller alongside `start`) for
/// the analytics route-start event. Returns a fully-built error response
/// (with the legacy call log and the formal analytics event already emitted)
/// when the model is unknown or all targets are pointing at disabled
/// providers.
#[allow(clippy::too_many_arguments)]
// Resolution failures return the fully-built HTTP response so logging,
// analytics emission, and client-facing error semantics stay in one path.
#[allow(clippy::result_large_err)]
pub async fn resolve_route(
    state: &AppState,
    runtime: &DispatchRuntime,
    request: &ChatCompletionRequest,
    trace_id: uuid::Uuid,
    start: Instant,
    started_at: chrono::DateTime<chrono::Utc>,
    is_stream: bool,
    user_agent: Option<&str>,
    client_ip: Option<&str>,
    session_key: Option<&sag_core::SessionKey>,
) -> Result<ResolvedRoute, Response<Body>> {
    // M3: only the Chat Completions mount exists; lane decisions use this
    // ingress. Future ingress mounts pass their own protocol here.
    resolve_route_with_ingress(
        state,
        runtime,
        request,
        trace_id,
        start,
        started_at,
        is_stream,
        user_agent,
        client_ip,
        session_key,
        sag_core::IngressProtocol::ChatCompletions,
    )
    .await
}

/// Same as [`resolve_route`] with an explicit ingress protocol for lane
/// decisions (§4.2).
#[allow(clippy::too_many_arguments)]
#[allow(clippy::result_large_err)]
pub async fn resolve_route_with_ingress(
    state: &AppState,
    runtime: &DispatchRuntime,
    request: &ChatCompletionRequest,
    trace_id: uuid::Uuid,
    start: Instant,
    started_at: chrono::DateTime<chrono::Utc>,
    is_stream: bool,
    user_agent: Option<&str>,
    client_ip: Option<&str>,
    session_key: Option<&sag_core::SessionKey>,
    ingress_protocol: sag_core::IngressProtocol,
) -> Result<ResolvedRoute, Response<Body>> {
    let config_guard = state.config.read().await;
    let current_version = prune_stale_if_needed(state, runtime, &config_guard.routes).await;

    let route = sag_config::resolve_model_route(&config_guard.routes, &request.model);
    let Some(route) = route else {
        return Err(emit_no_route_failure(
            state,
            request,
            trace_id,
            is_stream,
            start,
            started_at,
            user_agent,
            client_ip,
            session_key,
            ingress_protocol,
        )
        .await);
    };

    // Drop targets pointing at disabled providers or models not in enabled_models.
    let targets = filter_enabled_targets(&config_guard.providers, &route.targets, &request.model);

    if targets.is_empty() {
        return Err(emit_all_disabled_failure(
            state,
            request,
            &route.model_name,
            trace_id,
            is_stream,
            start,
            started_at,
            user_agent,
            client_ip,
            &[],
            session_key,
            ingress_protocol,
        )
        .await);
    }

    // Hard-requirement capability pre-check (after enabled filter, before
    // ordering). Per-drop `info!` happens inside `filter_capable_targets`.
    // Partial drops ride on `ResolvedRoute::precheck_errors` into the
    // dispatch `errors` channel; a full drop builds the 502 with the
    // capability reason inline (not the generic disabled wording).
    //
    // The image pipeline is a different species (no chat protocol
    // semantics, see D3/D8) — the chat hard-requirement table does not
    // apply to it, so it is never pre-checked here.
    let mut cap_errors = Vec::new();
    let targets = if route.pipeline == sag_config::Pipeline::Image {
        targets
    } else {
        filter_capable_targets(
            request,
            &config_guard.providers,
            targets,
            ingress_protocol,
            &mut cap_errors,
        )
    };
    if targets.is_empty() {
        return Err(emit_all_disabled_failure(
            state,
            request,
            &route.model_name,
            trace_id,
            is_stream,
            start,
            started_at,
            user_agent,
            client_ip,
            &cap_errors,
            session_key,
            ingress_protocol,
        )
        .await);
    }

    // Snapshot route + global settings so the config guard can be dropped
    // before the async `order_targets` call below.
    let strategy = route.strategy;
    let pipeline = route.pipeline;
    // Chat routes carry no `api_endpoint` (the provider derives the path
    // from the actually-used protocol); the image pipeline's real upstream
    // path is appended to `apis.image.base_url` by `chat_image`.
    let api_endpoint = match pipeline {
        sag_config::Pipeline::Image => route
            .api_endpoint
            .clone()
            .unwrap_or_else(|| "/images/generations".to_string()),
        sag_config::Pipeline::Chat => String::new(),
    };
    let route_retries = route.retries;
    let route_timeout = route.timeout_secs;
    let route_model_name = route.model_name.clone();
    // Session-sticky knobs (`session_sticky` routes only; inert elsewhere).
    let session_ttl_secs = route
        .session_ttl_secs
        .unwrap_or(sag_state::DEFAULT_SESSION_TTL_SECS);
    let session_max_entries = route
        .session_max_entries
        .unwrap_or(sag_state::DEFAULT_SESSION_MAX_ENTRIES);
    let global_retries = config_guard.backoff.retries;
    let shuffle_cache_ttl_secs = config_guard.shuffle_cache_ttl_secs;
    let sticky_failover_reset_secs = config_guard.sticky_failover_reset_secs;
    let provider_timeouts: HashMap<String, u64> = config_guard
        .providers
        .iter()
        .map(|(k, v)| (k.clone(), v.timeout_secs))
        .collect();
    // `apis` keys + `fallback` for the lane / actually-used-protocol
    // decision in `map_resolved_targets`.
    let provider_meta: HashMap<String, ProviderLaneMeta> = config_guard
        .providers
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                ProviderLaneMeta {
                    apis_keys: v.apis.keys().copied().collect(),
                    fallback: v.fallback,
                },
            )
        })
        .collect();
    drop(config_guard);

    // Apply target selection strategy. `current_version` (captured at the
    // same linearization point as the config read above) becomes part of the
    // sticky token, so a config mutation after this decision invalidates any
    // in-flight writeback.
    let (ordered, sticky_token, session_writeback) = order_targets(
        targets,
        &request.model,
        strategy,
        crate::order::OrderParams {
            counters: &runtime.route_counters,
            fill_first_orders: &runtime.fill_first_orders,
            active_targets: &runtime.active_targets,
            session_affinity: &runtime.session_affinity,
            session_key,
            shuffle_cache_ttl_secs,
            sticky_failover_reset_secs,
            session_ttl_secs,
            session_max_entries,
            routes_version: current_version,
        },
    )
    .await;

    let resolved = map_resolved_targets(
        ordered,
        &request.model,
        route_retries,
        route_timeout,
        &route_model_name,
        global_retries,
        &provider_timeouts,
        &provider_meta,
        ingress_protocol,
        pipeline,
    );

    Ok(ResolvedRoute {
        targets: resolved,
        pipeline,
        api_endpoint,
        strategy,
        sticky_token,
        session_writeback,
        route_name: route_model_name,
        started_at,
        precheck_errors: cap_errors,
    })
}

/// Prune stale cache entries for routes that have been deleted (e.g. via
/// admin API or config hot-reload). Skip when the `routes_version` hasn't
/// changed — in steady state every request avoids three write-lock
/// acquisitions. Returns the current `routes_version`.
async fn prune_stale_if_needed(
    state: &AppState,
    runtime: &DispatchRuntime,
    routes: &[RouteConfig],
) -> u64 {
    let current_version = state.config.routes_version();
    let last = runtime
        .last_prune_version
        .load(std::sync::atomic::Ordering::Acquire);
    if current_version != last {
        runtime.prune_stale_entries(routes).await;
        runtime
            .last_prune_version
            .store(current_version, std::sync::atomic::Ordering::Release);
    }
    current_version
}

/// No-route terminal: fully-built 404 response (legacy CallLog row with the
/// `__no_route__` sentinel) plus the formal analytics fact.
#[allow(clippy::too_many_arguments)]
async fn emit_no_route_failure(
    state: &AppState,
    request: &ChatCompletionRequest,
    trace_id: uuid::Uuid,
    is_stream: bool,
    start: Instant,
    started_at: chrono::DateTime<chrono::Utc>,
    user_agent: Option<&str>,
    client_ip: Option<&str>,
    session_key: Option<&sag_core::SessionKey>,
    ingress_protocol: sag_core::IngressProtocol,
) -> Response<Body> {
    let response = failure_response(
        state,
        trace_id,
        "__no_route__",
        &request.model,
        "__no_route__",
        is_stream,
        start,
        StatusCode::NOT_FOUND,
        AppError::ModelNotFound(request.model.clone()),
        request.reasoning_effort.as_deref(),
        user_agent,
        client_ip,
        Some(ingress_protocol.as_str().to_string()),
        None,
        None,
        None,
        session_key.map(|k| k.map_id(&request.model)),
    );
    // Formal analytics fact: no route matched → no route identity, no
    // target attempts. The sentinel `__no_route__` CallLog row above is
    // left untouched and must never leak into this fact.
    emit_route_failure_event(
        state,
        route_failure_event(
            trace_id,
            &request.model,
            None,
            FinalStatus::NoRoute,
            FailureReason::ModelNotFound,
            is_stream,
            started_at,
            Some(ingress_protocol),
        ),
    )
    .await;
    response
}

/// All-disabled / all-unincapable terminal: fully-built 502 response
/// (legacy CallLog row with the `__all_disabled__` sentinel) plus the
/// formal analytics fact carrying the real matched route name.
///
/// `capability_errors` is empty on the plain all-disabled path; when the
/// capability pre-check emptied the target list it carries the per-drop
/// `"{provider}/{model}: lacks capability X"` strings, which are merged
/// verbatim into the client-facing reason / CallLog message so the 502
/// never falls back to the generic "disabled or unavailable" wording.
#[allow(clippy::too_many_arguments)]
async fn emit_all_disabled_failure(
    state: &AppState,
    request: &ChatCompletionRequest,
    route_model_name: &str,
    trace_id: uuid::Uuid,
    is_stream: bool,
    start: Instant,
    started_at: chrono::DateTime<chrono::Utc>,
    user_agent: Option<&str>,
    client_ip: Option<&str>,
    capability_errors: &[String],
    session_key: Option<&sag_core::SessionKey>,
    ingress_protocol: sag_core::IngressProtocol,
) -> Response<Body> {
    let reason = if capability_errors.is_empty() {
        format!(
            "all targets for route '{route_model_name}' point to disabled or unavailable providers/models"
        )
    } else {
        format!(
            "all targets for route '{route_model_name}' lack required capabilities: {}",
            capability_errors.join("; ")
        )
    };
    let err = AppError::AllProvidersExhausted {
        model: request.model.clone(),
        reason,
    };
    let response = failure_response(
        state,
        trace_id,
        "__all_disabled__",
        &request.model,
        "__all_disabled__",
        is_stream,
        start,
        StatusCode::BAD_GATEWAY,
        err,
        request.reasoning_effort.as_deref(),
        user_agent,
        client_ip,
        Some(ingress_protocol.as_str().to_string()),
        None,
        None,
        None,
        session_key.map(|k| k.map_id(&request.model)),
    );
    // Formal analytics fact: the route matched but every target is
    // disabled → carry the real matched route name (never the
    // `__all_disabled__` sentinel); the writer resolves `route_id` from
    // it. No target attempts, and the sentinel CallLog row above is
    // left untouched.
    emit_route_failure_event(
        state,
        route_failure_event(
            trace_id,
            &request.model,
            Some(route_model_name.to_string()),
            FinalStatus::AllDisabled,
            FailureReason::AllDisabled,
            is_stream,
            started_at,
            Some(ingress_protocol),
        ),
    )
    .await;
    response
}

/// Resolve effective retries and timeout per target: target → route →
/// provider / global. Lane and the actually-used protocol come straight
/// from the new catalog formula:
///
/// ```text
/// lane             = (ingress ∈ provider 协议 key 集) ? Native : Translated
/// 实际使用协议       = Native ? ingress : fallback（或唯一协议 key）
/// ```
/// with image-pipeline routes pinned to [`sag_core::ApiKind::Image`].
#[allow(clippy::too_many_arguments)]
fn map_resolved_targets(
    ordered: Vec<RouteTarget>,
    requested_model: &str,
    route_retries: Option<u32>,
    route_timeout: Option<u64>,
    route_model_name: &str,
    global_retries: u32,
    provider_timeouts: &HashMap<String, u64>,
    provider_meta: &HashMap<String, ProviderLaneMeta>,
    ingress_protocol: sag_core::IngressProtocol,
    pipeline: sag_config::Pipeline,
) -> Vec<ResolvedTarget> {
    ordered
        .into_iter()
        .map(|t| {
            let target_real_model = t.real_model.clone();
            let rm = t.real_model.unwrap_or_else(|| requested_model.to_string());
            let retries = t.retries.or(route_retries).unwrap_or(global_retries);
            let timeout_secs = t
                .timeout_secs
                .or(route_timeout)
                .or_else(|| provider_timeouts.get(&t.provider).copied())
                .unwrap_or(sag_config::default_timeout_secs());

            let meta = provider_meta.get(&t.provider);
            let lane = sag_core::resolve_lane(
                meta.map(|m| m.apis_keys.as_slice()).unwrap_or_default(),
                ingress_protocol,
            );
            let upstream_protocol = if pipeline == sag_config::Pipeline::Image {
                // Image has no protocol-matching semantics — it is a
                // non-protocol catalog entry, not a lane participant.
                sag_core::ApiKind::Image
            } else {
                used_protocol(
                    lane,
                    ingress_protocol,
                    meta.map(|m| m.apis_keys.as_slice()).unwrap_or_default(),
                    meta.and_then(|m| m.fallback),
                )
            };

            ResolvedTarget {
                provider: t.provider,
                real_model: rm,
                route_model: route_model_name.to_string(),
                target_real_model,
                extra: t.extra,
                retries,
                timeout_secs,
                upstream_protocol,
                lane,
            }
        })
        .collect()
}

/// Build the formal analytics event for a request that failed during route
/// resolution, before any target attempt:
///
/// - `no_route`: no route matched → `route_name = None` (the writer keeps
///   `route_id` NULL); the sentinel `__no_route__` identity used in the
///   legacy CallLog must never leak into the formal fact.
/// - `all_disabled`: a route matched but every target is disabled →
///   `route_name = Some(matched route)`; the writer resolves `route_id`
///   from that name.
///
/// Both cases carry zero target attempts. `completed_at` is captured at
/// completion time (UTC); `request_id` is the caller's `trace_id`.
#[allow(clippy::too_many_arguments)]
fn route_failure_event(
    trace_id: uuid::Uuid,
    requested_model: &str,
    route_name: Option<String>,
    final_status: FinalStatus,
    failure_reason: FailureReason,
    stream: bool,
    started_at: chrono::DateTime<chrono::Utc>,
    ingress_protocol: Option<sag_core::IngressProtocol>,
) -> AnalyticsEvent {
    AnalyticsEvent::new(RequestResultFact {
        request_id: trace_id,
        route_name,
        requested_model: requested_model.to_string(),
        final_status,
        failure_reason: Some(failure_reason),
        stream,
        started_at,
        completed_at: chrono::Utc::now(),
        usage: None,
        cached_tokens: None,
        cache_creation_tokens: None,
        ttft_ms: None,
        upstream_duration_ms: None,
        // no_route / all_disabled: ingress known from the mount path, lane
        // stays None (no target attempted).
        ingress_protocol: ingress_protocol.map(|p| p.as_str().to_string()),
        lane: None,
    })
}

/// Enqueue a route-level failure event with formal backpressure
/// ([`sag_analytics::EventCollector::emit`], never `try_emit` — the fact must not be
/// dropped). A collector failure is only possible once the collector is
/// shut down; it is logged and swallowed so the client-facing response
/// already built by the caller keeps its exact semantics.
async fn emit_route_failure_event(state: &AppState, event: AnalyticsEvent) {
    if let Err(e) = state.event_collector.emit(event.clone()).await {
        warn!(
            request_id = %event.result.request_id,
            status = ?event.result.final_status,
            error = %e,
            "analytics event not enqueued (event collector shut down)"
        );
    }
}

/// Drop targets whose provider is marked `enabled = false` in the config
/// or whose model is not in the provider's non-empty `enabled_models` list.
pub fn filter_enabled_targets(
    providers: &HashMap<String, ProviderConfig>,
    targets: &[RouteTarget],
    requested_model: &str,
) -> Vec<RouteTarget> {
    targets
        .iter()
        .filter(|t| {
            let Some(pc) = providers.get(&t.provider) else {
                return true;
            };
            if !pc.enabled {
                info!(
                    provider = %t.provider,
                    requested_model = %requested_model,
                    "skipping disabled provider in route target"
                );
                return false;
            }
            if !pc.enabled_models.is_empty() {
                let model_to_check = t.real_model.as_deref().unwrap_or(requested_model);
                if model_to_check != "*" && !pc.enabled_models.iter().any(|m| m == model_to_check) {
                    info!(
                        provider = %t.provider,
                        model = %model_to_check,
                        requested_model = %requested_model,
                        "skipping target because model is not in provider enabled_models"
                    );
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect()
}

/// Whether a provider is enabled in the current config. Used as a
/// re-check between resolve and dispatch to avoid race conditions when
/// an admin disables a provider mid-request.
pub async fn is_provider_enabled(state: &AppState, name: &str) -> bool {
    let cfg = state.config.read().await;
    cfg.providers.get(name).map(|pc| pc.enabled).unwrap_or(true)
}

/// Look up a provider in the registry, skipping disabled ones. Records
/// the reason in `errors` and returns `None` on any failure. Shared by
/// both dispatch loops to keep the disabled-check + lookup sequence DRY.
pub async fn resolve_provider(
    state: &AppState,
    provider_name: &str,
    errors: &mut Vec<String>,
) -> Option<Arc<dyn LlmProvider>> {
    if !is_provider_enabled(state, provider_name).await {
        info!(provider = %provider_name, "skipping disabled provider");
        errors.push(format!("{provider_name}: disabled"));
        return None;
    }
    match state.get_provider(provider_name).await {
        Some(p) => Some(p),
        None => {
            warn!(provider = %provider_name, "provider not found in registry");
            errors.push(format!("{provider_name}: not found"));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::provider_config;
    use sag_config::RouteTarget;
    use std::collections::HashMap;
    use tokio::sync::RwLock;

    #[test]
    fn filter_enabled_targets_drops_disabled_providers() {
        let mut providers = HashMap::new();
        providers.insert("on".into(), provider_config(true));
        providers.insert("off".into(), provider_config(false));

        let targets = vec![
            RouteTarget {
                provider: "on".into(),
                real_model: None,
                priority: 1,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
                capabilities: None,
            },
            RouteTarget {
                provider: "off".into(),
                real_model: None,
                priority: 1,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
                capabilities: None,
            },
        ];

        let filtered = filter_enabled_targets(&providers, &targets, "test-route");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].provider, "on");
    }

    #[test]
    fn filter_enabled_targets_drops_targets_not_in_enabled_models() {
        let mut providers = HashMap::new();
        let mut pc_restricted = provider_config(true);
        pc_restricted.enabled_models = vec!["gpt-4o".into(), "gpt-4o-mini".into()];
        providers.insert("openai".into(), pc_restricted);

        // target 1: real_model "gpt-4o" is allowed
        // target 2: real_model "claude-3-opus" is not in enabled_models
        // target 3: real_model None with requested_model "gpt-4o" is allowed
        // target 4: real_model None with requested_model "unknown-model" is not allowed
        let targets = vec![
            RouteTarget {
                provider: "openai".into(),
                real_model: Some("gpt-4o".into()),
                priority: 1,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
                capabilities: None,
            },
            RouteTarget {
                provider: "openai".into(),
                real_model: Some("claude-3-opus".into()),
                priority: 2,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
                capabilities: None,
            },
        ];

        let filtered = filter_enabled_targets(&providers, &targets, "any-route");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].real_model.as_deref(), Some("gpt-4o"));

        // When requested_model is "gpt-4o-mini" and target has real_model = None
        let passthrough_targets = vec![RouteTarget {
            provider: "openai".into(),
            real_model: None,
            priority: 1,
            retries: None,
            timeout_secs: None,
            extra: HashMap::new(),
            capabilities: None,
        }];
        let filtered_pass = filter_enabled_targets(&providers, &passthrough_targets, "gpt-4o-mini");
        assert_eq!(filtered_pass.len(), 1);

        let filtered_blocked =
            filter_enabled_targets(&providers, &passthrough_targets, "non-existent-model");
        assert_eq!(filtered_blocked.len(), 0);
    }

    #[test]
    fn filter_enabled_targets_keeps_targets_when_provider_unknown() {
        // Unknown provider → no entry in providers map → default to enabled
        let providers = HashMap::new();
        let targets = vec![RouteTarget {
            provider: "ghost".into(),
            real_model: None,
            priority: 1,
            retries: None,
            timeout_secs: None,
            extra: HashMap::new(),
            capabilities: None,
        }];
        let filtered = filter_enabled_targets(&providers, &targets, "test-route");
        assert_eq!(filtered.len(), 1);
    }

    // ── resolve_route prune-stale-entries tests ───────────────────

    /// Build a minimal [`AppState`] with a single route for testing
    /// resolve_route + prune behaviour. The route is StickyFailover so a
    /// pre-inserted `active_targets` pin is a *valid* entry that survives
    /// the first prune — the deletion test below then verifies prune clears
    /// all three maps (a FillFirst route would never legitimately hold a
    /// pin, so it would be pruned on the very first resolve).
    async fn build_state_for_prune_test() -> (AppState, DispatchRuntime) {
        use std::collections::HashMap;

        let mut providers: HashMap<String, sag_config::ProviderConfig> = HashMap::new();
        providers.insert("openai".into(), crate::testing::provider_config(true));

        let routes = vec![sag_config::RouteConfig {
            session_ttl_secs: None,
            session_max_entries: None,
            model_name: "gpt-4o".into(),
            api_endpoint: None,
            pipeline: sag_config::Pipeline::Chat,
            retries: None,
            timeout_secs: None,
            strategy: sag_config::Strategy::StickyFailover,
            targets: vec![sag_config::RouteTarget {
                provider: "openai".into(),
                real_model: Some("gpt-4o".into()),
                priority: 10,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
                capabilities: None,
            }],
        }];

        let config = sag_config::GatewayConfig {
            providers,
            routes,
            ..sag_config::gateway_config_base!()
        };
        let handle = sag_config::ConfigHandle::from_config(config);
        let provider_registry = sag_state::testing::build_provider_registry(&handle, false).await;
        let state = sag_state::testing::test_app_state(handle, provider_registry).await;
        let runtime = DispatchRuntime::default();
        (state, runtime)
    }

    fn test_request(model: &str) -> ChatCompletionRequest {
        serde_json::from_value(serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .expect("valid test request")
    }

    #[tokio::test]
    async fn resolve_route_prunes_stale_entries_after_route_deletion() {
        let (state, runtime) = build_state_for_prune_test().await;
        let req = test_request("gpt-4o");
        let trace_id = uuid::Uuid::new_v4();
        let start = std::time::Instant::now();
        let started_at = chrono::Utc::now();

        // Pre-populate all three caches so we can verify prune clears
        // every map, not just fill_first_orders.
        runtime
            .route_counters
            .write()
            .await
            .insert("gpt-4o".into(), 42);
        runtime.active_targets.write().await.insert(
            "gpt-4o".into(),
            sag_state::ActiveTargetEntry::new(
                "openai",
                "gpt-4o",
                "gpt-4o",
                Some("gpt-4o".into()),
                std::time::Instant::now(),
            ),
        );

        // 1. First call: resolve route → populates fill_first_orders.
        let result = resolve_route(
            &state, &runtime, &req, trace_id, start, started_at, false, None, None, None,
        )
        .await;
        assert!(result.is_ok(), "route should resolve successfully");

        // All three caches are populated.
        assert!(
            runtime
                .fill_first_orders
                .read()
                .await
                .contains_key("gpt-4o")
        );
        assert!(runtime.route_counters.read().await.contains_key("gpt-4o"));
        assert!(runtime.active_targets.read().await.contains_key("gpt-4o"));

        // 2. Simulate route deletion: remove it from config and bump
        //    the routes version so the prune guard knows to run.
        {
            let mut cfg = state.config.write().await;
            cfg.routes.retain(|r| r.model_name != "gpt-4o");
            state.config.bump_routes_version();
        }

        // 3. Second call: route gone → ModelNotFound, but prune ran first.
        let result2 = resolve_route(
            &state, &runtime, &req, trace_id, start, started_at, false, None, None, None,
        )
        .await;
        assert!(
            result2.is_err(),
            "second resolve should fail with ModelNotFound"
        );

        // 4. Verify all stale cache entries are cleaned up.
        assert!(
            !runtime
                .fill_first_orders
                .read()
                .await
                .contains_key("gpt-4o"),
            "stale fill_first_orders entry must be pruned"
        );
        assert!(
            !runtime.route_counters.read().await.contains_key("gpt-4o"),
            "stale route_counters entry must be pruned"
        );
        assert!(
            !runtime.active_targets.read().await.contains_key("gpt-4o"),
            "stale active_targets entry must be pruned"
        );
    }

    #[tokio::test]
    async fn resolve_route_captures_sticky_token_while_ordering() {
        // A StickyFailover route so `order_targets` captures a token.
        let (state, runtime) = build_sticky_state().await;
        let req = test_request("gpt-4o");
        let trace_id = uuid::Uuid::new_v4();
        let start = std::time::Instant::now();
        let started_at = chrono::Utc::now();

        // Fresh memory, nothing cleared: the decision captured 0 and
        // the current routes_version.
        let ResolvedRoute {
            sticky_token: token1,
            ..
        } = resolve_route(
            &state, &runtime, &req, trace_id, start, started_at, false, None, None, None,
        )
        .await
        .unwrap();
        assert_eq!(token1.generation, 0, "fresh map captures 0");
        assert_eq!(
            token1.routes_version,
            state.config.routes_version(),
            "token must carry the routes_version captured at decision time"
        );

        // A clear (DELETE / close / targets-changed / prune) invalidates
        // pre-clear decisions. For a model with no per-model state (fresh
        // map) the clear bumps the global epoch instead of creating a
        // per-model tombstone; the *next* decision captures the new epoch so
        // its writeback still succeeds, while pre-clear decisions (old
        // epoch) fail.
        runtime.active_targets.write().await.clear("gpt-4o");
        let ResolvedRoute {
            sticky_token: token2,
            ..
        } = resolve_route(
            &state, &runtime, &req, trace_id, start, started_at, false, None, None, None,
        )
        .await
        .unwrap();
        assert_eq!(
            token2.epoch, 1,
            "post-clear decision captures the bumped epoch"
        );
        assert_eq!(
            token2.generation, 0,
            "no per-model tombstone is created for an untouched model"
        );

        runtime.active_targets.write().await.clear("gpt-4o");
        let ResolvedRoute {
            sticky_token: token3,
            ..
        } = resolve_route(
            &state, &runtime, &req, trace_id, start, started_at, false, None, None, None,
        )
        .await
        .unwrap();
        assert_eq!(token3.epoch, 2, "every no-entry clear bumps again");
    }

    #[tokio::test]
    async fn resolve_route_captures_post_mutation_routes_version() {
        // A config mutation moves `routes_version`; the next decision must
        // capture the new version (so its writeback still succeeds) while
        // decisions captured before the mutation fail.
        let (state, runtime) = build_sticky_state().await;
        let req = test_request("gpt-4o");
        let trace_id = uuid::Uuid::new_v4();
        let start = std::time::Instant::now();
        let started_at = chrono::Utc::now();

        let ResolvedRoute {
            sticky_token: before,
            ..
        } = resolve_route(
            &state, &runtime, &req, trace_id, start, started_at, false, None, None, None,
        )
        .await
        .unwrap();
        let version_before = before.routes_version;

        // Simulate a config mutation (route CRUD / hot-reload).
        {
            let mut cfg = state.config.write().await;
            cfg.routes.retain(|r| r.model_name != "gpt-4o");
            cfg.routes.push(sag_config::RouteConfig {
                session_ttl_secs: None,
                session_max_entries: None,
                model_name: "gpt-4o".into(),
                api_endpoint: None,
                pipeline: sag_config::Pipeline::Chat,
                retries: None,
                timeout_secs: None,
                strategy: sag_config::Strategy::StickyFailover,
                targets: vec![sag_config::RouteTarget {
                    provider: "openai".into(),
                    real_model: Some("gpt-4o".into()),
                    priority: 10,
                    retries: None,
                    timeout_secs: None,
                    extra: HashMap::new(),
                    capabilities: None,
                }],
            });
            state.config.bump_routes_version();
        }
        assert!(state.config.routes_version() > version_before);

        let ResolvedRoute {
            sticky_token: after,
            ..
        } = resolve_route(
            &state, &runtime, &req, trace_id, start, started_at, false, None, None, None,
        )
        .await
        .unwrap();
        assert_eq!(
            after.routes_version,
            state.config.routes_version(),
            "post-mutation decision captures the new routes_version"
        );
        assert!(after.routes_version > version_before);

        // The pre-mutation token must be rejected by a writeback.
        let mut guard = runtime.active_targets.write().await;
        assert!(
            !guard.insert_if_generation(
                "gpt-4o".into(),
                sag_state::ActiveTargetEntry::new(
                    "openai",
                    "gpt-4o",
                    "gpt-4o",
                    Some("gpt-4o".into()),
                    std::time::Instant::now(),
                ),
                &before,
                state.config.routes_version(),
            ),
            "pre-mutation token must be rejected after the config mutation"
        );
    }

    #[tokio::test]
    async fn resolve_route_carries_route_name_and_started_at() {
        // Exact-match route: the resolved carrier must expose the matched
        // route's `model_name` and pass through the caller-supplied
        // wall-clock `started_at` unchanged (captured in `routes.rs` next
        // to the `Instant` used for legacy duration timing).
        let (state, runtime) = build_sticky_state().await;
        let req = test_request("gpt-4o");
        let trace_id = uuid::Uuid::new_v4();
        let start = std::time::Instant::now();
        let started_at = chrono::Utc::now();

        let ResolvedRoute {
            route_name,
            started_at: carried_started_at,
            ..
        } = resolve_route(
            &state, &runtime, &req, trace_id, start, started_at, false, None, None, None,
        )
        .await
        .unwrap();
        assert_eq!(
            route_name, "gpt-4o",
            "exact match must carry the route model_name"
        );
        assert_eq!(
            carried_started_at, started_at,
            "started_at must pass through unchanged"
        );

        // Wildcard route: a model with no exact route falls back to `*`;
        // the carrier must record the wildcard `model_name`.
        {
            let mut cfg = state.config.write().await;
            cfg.routes.push(sag_config::RouteConfig {
                session_ttl_secs: None,
                session_max_entries: None,
                model_name: "*".into(),
                api_endpoint: None,
                pipeline: sag_config::Pipeline::Chat,
                retries: None,
                timeout_secs: None,
                strategy: sag_config::Strategy::FillFirst,
                targets: vec![sag_config::RouteTarget {
                    provider: "openai".into(),
                    real_model: None,
                    priority: 1,
                    retries: None,
                    timeout_secs: None,
                    extra: HashMap::new(),
                    capabilities: None,
                }],
            });
            state.config.bump_routes_version();
        }

        let req = test_request("some-unknown-model");
        let start = std::time::Instant::now();
        let started_at = chrono::Utc::now();
        let ResolvedRoute {
            route_name: wildcard_name,
            ..
        } = resolve_route(
            &state, &runtime, &req, trace_id, start, started_at, false, None, None, None,
        )
        .await
        .unwrap();
        assert_eq!(
            wildcard_name, "*",
            "wildcard match must carry the `*` route name"
        );
    }

    /// Build an AppState + DispatchRuntime for a single StickyFailover route.
    async fn build_sticky_state() -> (AppState, DispatchRuntime) {
        let mut providers: HashMap<String, sag_config::ProviderConfig> = HashMap::new();
        providers.insert("openai".into(), crate::testing::provider_config(true));
        let routes = vec![sag_config::RouteConfig {
            session_ttl_secs: None,
            session_max_entries: None,
            model_name: "gpt-4o".into(),
            api_endpoint: None,
            pipeline: sag_config::Pipeline::Chat,
            retries: None,
            timeout_secs: None,
            strategy: sag_config::Strategy::StickyFailover,
            targets: vec![sag_config::RouteTarget {
                provider: "openai".into(),
                real_model: Some("gpt-4o".into()),
                priority: 10,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
                capabilities: None,
            }],
        }];
        let config = sag_config::GatewayConfig {
            providers,
            routes,
            ..sag_config::gateway_config_base!()
        };
        let handle = sag_config::ConfigHandle::from_config(config);
        let provider_registry = sag_state::testing::build_provider_registry(&handle, false).await;
        let state = sag_state::testing::test_app_state(handle, provider_registry).await;
        let runtime = DispatchRuntime::default();
        (state, runtime)
    }

    // ── Sentinel value tests ─────────────────────────────────────

    /// Build state with an empty route table for __no_route__ tests.
    async fn build_state_no_routes() -> (AppState, DispatchRuntime) {
        let config = sag_config::GatewayConfig {
            providers: HashMap::new(),
            routes: vec![],
            ..sag_config::gateway_config_base!()
        };
        let handle = sag_config::ConfigHandle::from_config(config);
        let provider_registry = Arc::new(RwLock::new(HashMap::new()));
        let state = sag_state::testing::test_app_state(handle, provider_registry).await;
        let runtime = DispatchRuntime::default();
        (state, runtime)
    }

    /// Build state where the only provider is disabled, so all route targets
    /// are filtered out → __all_disabled__.
    async fn build_state_all_disabled() -> (AppState, DispatchRuntime) {
        use std::collections::HashMap;

        let mut providers: HashMap<String, sag_config::ProviderConfig> = HashMap::new();
        providers.insert("off".into(), provider_config(false));

        let routes = vec![sag_config::RouteConfig {
            session_ttl_secs: None,
            session_max_entries: None,
            model_name: "gpt-4o".into(),
            api_endpoint: None,
            pipeline: sag_config::Pipeline::Chat,
            retries: None,
            timeout_secs: None,
            strategy: sag_config::Strategy::FillFirst,
            targets: vec![sag_config::RouteTarget {
                provider: "off".into(),
                real_model: Some("gpt-4o".into()),
                priority: 10,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
                capabilities: None,
            }],
        }];

        let config = sag_config::GatewayConfig {
            providers,
            routes,
            ..sag_config::gateway_config_base!()
        };
        let handle = sag_config::ConfigHandle::from_config(config);
        let provider_registry = sag_state::testing::build_provider_registry(&handle, false).await;
        let state = sag_state::testing::test_app_state(handle, provider_registry).await;
        let runtime = DispatchRuntime::default();
        (state, runtime)
    }

    #[tokio::test]
    async fn resolve_route_no_routes_logs_no_route_sentinel() {
        let (state, runtime) = build_state_no_routes().await;
        let req = test_request("gpt-4o");
        let trace_id = uuid::Uuid::new_v4();
        let start = std::time::Instant::now();
        let started_at = chrono::Utc::now();

        let result = resolve_route(
            &state, &runtime, &req, trace_id, start, started_at, false, None, None, None,
        )
        .await;
        assert!(result.is_err(), "expected ModelNotFound error");

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
        assert_eq!(log.provider, "__no_route__");
        assert_eq!(log.real_model, "__no_route__");
        assert_eq!(log.model, "gpt-4o");
        assert_eq!(log.status_code, 404);
        assert_eq!(log.error_category.as_deref(), Some("model_not_found"));
    }

    #[tokio::test]
    async fn resolve_route_all_disabled_logs_sentinel() {
        let (state, runtime) = build_state_all_disabled().await;
        let req = test_request("gpt-4o");
        let trace_id = uuid::Uuid::new_v4();
        let start = std::time::Instant::now();
        let started_at = chrono::Utc::now();

        let result = resolve_route(
            &state, &runtime, &req, trace_id, start, started_at, false, None, None, None,
        )
        .await;
        assert!(result.is_err(), "expected AllProvidersExhausted error");

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
        assert_eq!(log.provider, "__all_disabled__");
        assert_eq!(log.real_model, "__all_disabled__");
        assert_eq!(log.model, "gpt-4o");
        assert_eq!(log.status_code, 502);
        assert_eq!(log.error_category.as_deref(), Some("all_exhausted"));
    }

    // ── capability pre-check observability (B1) ─────────────────

    /// Build state with two targets for `gpt-4o`:
    /// - `weird`: target-level capability override takes `reasoning` away;
    /// - `good`: plain target on a chat-capable provider.
    ///
    /// A `reasoning_effort` request therefore partially drops only `weird`.
    async fn build_state_partial_capability() -> (AppState, DispatchRuntime) {
        let mut providers: HashMap<String, sag_config::ProviderConfig> = HashMap::new();
        providers.insert("good".into(), provider_config(true));
        providers.insert("weird".into(), provider_config(true));

        let routes = vec![sag_config::RouteConfig {
            session_ttl_secs: None,
            session_max_entries: None,
            model_name: "gpt-4o".into(),
            api_endpoint: None,
            pipeline: sag_config::Pipeline::Chat,
            retries: None,
            timeout_secs: None,
            strategy: sag_config::Strategy::FillFirst,
            targets: vec![
                sag_config::RouteTarget {
                    provider: "weird".into(),
                    real_model: Some("gpt-4o".into()),
                    priority: 10,
                    retries: None,
                    timeout_secs: None,
                    extra: HashMap::new(),
                    capabilities: Some(sag_config::CapabilitiesOverride {
                        reasoning: Some(false),
                        ..Default::default()
                    }),
                },
                sag_config::RouteTarget {
                    provider: "good".into(),
                    real_model: Some("gpt-4o".into()),
                    priority: 5,
                    retries: None,
                    timeout_secs: None,
                    extra: HashMap::new(),
                    capabilities: None,
                },
            ],
        }];

        let config = sag_config::GatewayConfig {
            providers,
            routes,
            ..sag_config::gateway_config_base!()
        };
        let handle = sag_config::ConfigHandle::from_config(config);
        let provider_registry = sag_state::testing::build_provider_registry(&handle, false).await;
        let state = sag_state::testing::test_app_state(handle, provider_registry).await;
        let runtime = DispatchRuntime::default();
        (state, runtime)
    }

    #[tokio::test]
    async fn resolve_route_partial_capability_drop_carries_precheck_errors() {
        let (state, runtime) = build_state_partial_capability().await;
        let req = test_request_with_reasoning("gpt-4o", "high");
        let trace_id = uuid::Uuid::new_v4();
        let start = std::time::Instant::now();
        let started_at = chrono::Utc::now();

        let resolved = resolve_route(
            &state, &runtime, &req, trace_id, start, started_at, false, None, None, None,
        )
        .await
        .expect("one capable target remains");

        assert!(
            resolved.targets.iter().any(|t| t.provider == "good"),
            "capable target must survive"
        );
        assert!(
            !resolved.targets.iter().any(|t| t.provider == "weird"),
            "incapable target must be dropped"
        );
        assert_eq!(
            resolved.precheck_errors.len(),
            1,
            "partial drop must surface one precheck error"
        );
        assert!(
            resolved.precheck_errors[0].contains("lacks capability reasoning"),
            "precheck_errors must name the missing capability; got: {:?}",
            resolved.precheck_errors
        );
        assert!(
            resolved.precheck_errors[0].contains("weird/"),
            "precheck_errors must identify the dropped provider; got: {:?}",
            resolved.precheck_errors
        );
    }

    #[tokio::test]
    async fn resolve_route_all_lack_capability_reports_reason_in_body_and_calllog() {
        // Only the incapable target — full capability drop → 502 with the
        // capability detail in the response body and the CallLog row.
        let mut providers: HashMap<String, sag_config::ProviderConfig> = HashMap::new();
        providers.insert("weird".into(), provider_config(true));
        let routes = vec![sag_config::RouteConfig {
            session_ttl_secs: None,
            session_max_entries: None,
            model_name: "gpt-4o".into(),
            api_endpoint: None,
            pipeline: sag_config::Pipeline::Chat,
            retries: None,
            timeout_secs: None,
            strategy: sag_config::Strategy::FillFirst,
            targets: vec![sag_config::RouteTarget {
                provider: "weird".into(),
                real_model: Some("gpt-4o".into()),
                priority: 10,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
                capabilities: Some(sag_config::CapabilitiesOverride {
                    reasoning: Some(false),
                    ..Default::default()
                }),
            }],
        }];
        let config = sag_config::GatewayConfig {
            providers,
            routes,
            ..sag_config::gateway_config_base!()
        };
        let handle = sag_config::ConfigHandle::from_config(config);
        let provider_registry = sag_state::testing::build_provider_registry(&handle, false).await;
        let state = sag_state::testing::test_app_state(handle, provider_registry).await;
        let runtime = DispatchRuntime::default();

        let req = test_request_with_reasoning("gpt-4o", "high");
        let trace_id = uuid::Uuid::new_v4();
        let start = std::time::Instant::now();
        let started_at = chrono::Utc::now();

        let result = resolve_route(
            &state, &runtime, &req, trace_id, start, started_at, false, None, None, None,
        )
        .await;
        let response = result.err().expect("all targets lack capability → 502");
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_str = String::from_utf8_lossy(&body);
        assert!(
            body_str.contains("lacks capability"),
            "response body must expose the capability reason; got: {body_str}"
        );
        assert!(
            !body_str.contains("disabled or unavailable"),
            "capability full-drop must not fall back to generic disabled wording; got: {body_str}"
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
        assert_eq!(log.status_code, 502);
        let error = log.error.as_deref().unwrap_or_default();
        assert!(
            error.contains("lacks capability"),
            "CallLog error must expose the capability reason; got: {error}"
        );
    }

    // ── reasoning_effort round-trip test ─────────────────

    fn test_request_with_reasoning(model: &str, reasoning_effort: &str) -> ChatCompletionRequest {
        serde_json::from_value(serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": reasoning_effort,
        }))
        .expect("valid test request")
    }

    #[tokio::test]
    async fn resolve_route_failure_preserves_reasoning_effort() {
        let (state, runtime) = build_state_no_routes().await;
        let req = test_request_with_reasoning("gpt-4o", "low");
        let trace_id = uuid::Uuid::new_v4();
        let start = std::time::Instant::now();
        let started_at = chrono::Utc::now();

        let result = resolve_route(
            &state, &runtime, &req, trace_id, start, started_at, false, None, None, None,
        )
        .await;
        assert!(result.is_err(), "expected ModelNotFound error");

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
            Some("low"),
            "reasoning_effort must round-trip through resolve failure path"
        );
    }

    // ── formal analytics event tests ────────────────────────────

    /// Build an [`AppState`] whose formal event collector writes into a
    /// shared in-memory pool the test keeps a handle on — same wiring as
    /// `sag_state::testing::test_app_state`, minus the hidden pool.
    async fn build_state_with_event_pool(
        config: sag_config::GatewayConfig,
    ) -> (AppState, DispatchRuntime, sqlx::SqlitePool) {
        let handle = sag_config::ConfigHandle::from_config(config);
        let provider_registry = sag_state::testing::build_provider_registry(&handle, false).await;
        let pool = sag_analytics::open_analytics_pool(":memory:")
            .await
            .unwrap();
        let state = AppState {
            config: handle.clone(),
            providers: provider_registry.clone(),
            coordinator: Arc::new(sag_state::ConfigMutationCoordinator::new(
                handle,
                provider_registry,
            )),
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
        };
        let runtime = DispatchRuntime::default();
        (state, runtime, pool)
    }

    #[tokio::test]
    async fn resolve_route_no_route_persists_no_route_analytics_event() {
        let (state, runtime, pool) = build_state_with_event_pool(sag_config::GatewayConfig {
            providers: HashMap::new(),
            routes: vec![],
            ..sag_config::gateway_config_base!()
        })
        .await;
        let req = test_request("gpt-4o");
        let trace_id = uuid::Uuid::new_v4();
        let start = std::time::Instant::now();
        let started_at = chrono::Utc::now();

        let result = resolve_route(
            &state, &runtime, &req, trace_id, start, started_at, false, None, None, None,
        )
        .await;
        assert!(result.is_err(), "expected ModelNotFound error");

        // Drain the event queue so the writer commits before we read.
        state.event_collector.shutdown().await;

        let (
            final_status,
            failure_reason,
            route_id,
            requested_model,
            stream,
            started_ms,
            completed_ms,
        ): (
            String,
            Option<String>,
            Option<String>,
            String,
            i32,
            i64,
            i64,
        ) = sqlx::query_as(
            "SELECT final_status, failure_reason, route_id, requested_model, stream, \
             started_at, completed_at FROM request_results WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("no_route fact must be persisted");

        assert_eq!(final_status, "no_route");
        assert_eq!(failure_reason.as_deref(), Some("model_not_found"));
        assert!(route_id.is_none(), "no_route must keep route_id NULL");
        assert_eq!(requested_model, "gpt-4o");
        assert_eq!(stream, 0, "non-stream request must record stream=0");
        assert_eq!(
            started_ms,
            started_at.timestamp_millis(),
            "started_at must come from the caller-supplied capture"
        );
        assert!(
            completed_ms >= started_ms,
            "completed_at (completion time, UTC) must not precede started_at"
        );

        let attempts: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM target_attempts WHERE request_id = ?")
                .bind(trace_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(attempts, 0, "no_route must not produce target attempts");
    }

    #[tokio::test]
    async fn resolve_route_all_disabled_persists_all_disabled_analytics_event() {
        let mut providers: HashMap<String, sag_config::ProviderConfig> = HashMap::new();
        providers.insert("off".into(), provider_config(false));

        let routes = vec![sag_config::RouteConfig {
            session_ttl_secs: None,
            session_max_entries: None,
            model_name: "gpt-4o".into(),
            api_endpoint: None,
            pipeline: sag_config::Pipeline::Chat,
            retries: None,
            timeout_secs: None,
            strategy: sag_config::Strategy::FillFirst,
            targets: vec![sag_config::RouteTarget {
                provider: "off".into(),
                real_model: Some("gpt-4o".into()),
                priority: 10,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
                capabilities: None,
            }],
        }];

        let (state, runtime, pool) = build_state_with_event_pool(sag_config::GatewayConfig {
            providers,
            routes,
            ..sag_config::gateway_config_base!()
        })
        .await;
        let req = test_request("gpt-4o");
        let trace_id = uuid::Uuid::new_v4();
        let start = std::time::Instant::now();
        let started_at = chrono::Utc::now();

        let result = resolve_route(
            &state, &runtime, &req, trace_id, start, started_at, false, None, None, None,
        )
        .await;
        assert!(result.is_err(), "expected AllProvidersExhausted error");

        // Drain the event queue so the writer commits before we read.
        state.event_collector.shutdown().await;

        let (final_status, failure_reason, route_id, requested_model, started_ms, completed_ms): (
            String,
            Option<String>,
            Option<String>,
            String,
            i64,
            i64,
        ) = sqlx::query_as(
            "SELECT final_status, failure_reason, route_id, requested_model, started_at, \
             completed_at FROM request_results WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("all_disabled fact must be persisted");

        assert_eq!(final_status, "all_disabled");
        assert_eq!(failure_reason.as_deref(), Some("all_disabled"));
        let route_id = route_id.expect("all_disabled must resolve a route_id");
        assert_eq!(requested_model, "gpt-4o");
        assert_eq!(
            started_ms,
            started_at.timestamp_millis(),
            "started_at must come from the caller-supplied capture"
        );
        assert!(
            completed_ms >= started_ms,
            "completed_at (completion time, UTC) must not precede started_at"
        );

        // The resolved identity must be the route the request actually
        // matched — the real route name, never the __all_disabled__ sentinel.
        let resolved_name: String =
            sqlx::query_scalar("SELECT route_name FROM analytics_routes WHERE route_id = ?")
                .bind(&route_id)
                .fetch_one(&pool)
                .await
                .expect("resolved route identity must exist");
        assert_eq!(resolved_name, "gpt-4o");

        let attempts: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM target_attempts WHERE request_id = ?")
                .bind(trace_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(attempts, 0, "all_disabled must not produce target attempts");
    }
}
