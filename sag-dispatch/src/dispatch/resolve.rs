//! Route resolution: model → ordered fallback targets + `api_endpoint`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::{body::Body, http::StatusCode, response::Response};
use sag_config::{ProviderConfig, RouteTarget};
use sag_core::{AppError, ChatCompletionRequest};
use sag_providers::LlmProvider;
use tracing::{info, warn};

use crate::dispatch::{ResolvedRoute, ResolvedTarget, failure_response};
use crate::order::order_targets;
use crate::state::{AppState, DispatchRuntime};

/// Resolve a chat request to an ordered list of fallback targets and the
/// configured `api_endpoint`. Returns a fully-built error response (with
/// analytics already logged) when the model is unknown or all targets are
/// pointing at disabled providers.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_route(
    state: &AppState,
    runtime: &DispatchRuntime,
    request: &ChatCompletionRequest,
    trace_id: uuid::Uuid,
    start: Instant,
    is_stream: bool,
    user_agent: Option<&str>,
    client_ip: Option<&str>,
) -> Result<ResolvedRoute, Response<Body>> {
    let config_guard = state.config.read().await;

    // Prune stale cache entries for routes that have been deleted
    // (e.g. via admin API or config hot-reload). Skip when the
    // routes_version hasn't changed — in steady state every request
    // avoids three write-lock acquisitions.
    let current_version = state.config.routes_version();
    let last = runtime
        .last_prune_version
        .load(std::sync::atomic::Ordering::Acquire);
    if current_version != last {
        runtime.prune_stale_entries(&config_guard.routes).await;
        runtime
            .last_prune_version
            .store(current_version, std::sync::atomic::Ordering::Release);
    }

    let route = config_guard
        .routes
        .iter()
        .find(|r| r.model_name == request.model)
        .or_else(|| config_guard.routes.iter().find(|r| r.model_name == "*"));

    let Some(route) = route else {
        return Err(failure_response(
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
        ));
    };

    // Drop targets pointing at disabled providers.
    let targets =
        filter_enabled_targets(&config_guard.providers, &route.targets, &route.model_name);

    if targets.is_empty() {
        let err = AppError::AllProvidersExhausted {
            model: request.model.clone(),
            reason: format!(
                "all targets for route '{}' point to disabled providers",
                route.model_name
            ),
        };
        return Err(failure_response(
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
        ));
    }

    let strategy = route.strategy;
    let api_endpoint = route.api_endpoint.clone();
    let route_retries = route.retries;
    let route_timeout = route.timeout_secs;
    let route_model_name = route.model_name.clone();
    let global_retries = config_guard.backoff.retries;
    let shuffle_cache_ttl_secs = config_guard.shuffle_cache_ttl_secs;
    let sticky_failover_reset_secs = config_guard.sticky_failover_reset_secs;
    let provider_timeouts: HashMap<String, u64> = config_guard
        .providers
        .iter()
        .map(|(k, v)| (k.clone(), v.timeout_secs))
        .collect();
    drop(config_guard);

    // Apply target selection strategy. `current_version` (captured at the
    // same linearization point as the config read above) becomes part of the
    // sticky token, so a config mutation after this decision invalidates any
    // in-flight writeback.
    let (ordered, sticky_token) = order_targets(
        targets,
        &request.model,
        strategy,
        &runtime.route_counters,
        &runtime.fill_first_orders,
        &runtime.active_targets,
        shuffle_cache_ttl_secs,
        sticky_failover_reset_secs,
        current_version,
    )
    .await;

    // Resolve effective retries and timeout per target:
    // target → route → provider / global
    let resolved: Vec<ResolvedTarget> = ordered
        .into_iter()
        .map(|t| {
            let target_real_model = t.real_model.clone();
            let rm = t.real_model.unwrap_or_else(|| request.model.clone());
            let retries = t.retries.or(route_retries).unwrap_or(global_retries);
            let timeout_secs = t
                .timeout_secs
                .or(route_timeout)
                .or_else(|| provider_timeouts.get(&t.provider).copied())
                .unwrap_or(sag_config::default_timeout_secs());
            ResolvedTarget {
                provider: t.provider,
                real_model: rm,
                route_model: route_model_name.clone(),
                target_real_model,
                extra: t.extra,
                retries,
                timeout_secs,
            }
        })
        .collect();

    Ok((resolved, api_endpoint, strategy, sticky_token))
}

/// Drop targets whose provider is marked `enabled = false` in the config.
pub fn filter_enabled_targets(
    providers: &HashMap<String, ProviderConfig>,
    targets: &[RouteTarget],
    route_name: &str,
) -> Vec<RouteTarget> {
    targets
        .iter()
        .filter(|t| {
            let enabled = providers
                .get(&t.provider)
                .map(|pc| pc.enabled)
                .unwrap_or(true);
            if !enabled {
                info!(
                    provider = %t.provider,
                    route = %route_name,
                    "skipping disabled provider in route target"
                );
            }
            enabled
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
            },
            RouteTarget {
                provider: "off".into(),
                real_model: None,
                priority: 1,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
            },
        ];

        let filtered = filter_enabled_targets(&providers, &targets, "test-route");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].provider, "on");
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
        }];
        let filtered = filter_enabled_targets(&providers, &targets, "test-route");
        assert_eq!(filtered.len(), 1);
    }

    // ── resolve_route prune-stale-entries tests ───────────────────

    /// Build a minimal [`AppState`] with a single route for testing
    /// resolve_route + prune behaviour.
    async fn build_state_for_prune_test() -> (AppState, DispatchRuntime) {
        use std::collections::HashMap;

        let mut providers: HashMap<String, sag_config::ProviderConfig> = HashMap::new();
        providers.insert("openai".into(), crate::testing::provider_config(true));

        let routes = vec![sag_config::RouteConfig {
            model_name: "gpt-4o".into(),
            api_endpoint: "/chat/completions".into(),
            retries: None,
            timeout_secs: None,
            strategy: sag_config::Strategy::FillFirst,
            targets: vec![sag_config::RouteTarget {
                provider: "openai".into(),
                real_model: Some("gpt-4o".into()),
                priority: 10,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
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
        let result =
            resolve_route(&state, &runtime, &req, trace_id, start, false, None, None).await;
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
        let result2 =
            resolve_route(&state, &runtime, &req, trace_id, start, false, None, None).await;
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

        // Fresh memory, nothing cleared: the decision captured 0 and
        // the current routes_version.
        let (_, _, _, token1) =
            resolve_route(&state, &runtime, &req, trace_id, start, false, None, None)
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
        let (_, _, _, token2) =
            resolve_route(&state, &runtime, &req, trace_id, start, false, None, None)
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
        let (_, _, _, token3) =
            resolve_route(&state, &runtime, &req, trace_id, start, false, None, None)
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

        let (_, _, _, before) =
            resolve_route(&state, &runtime, &req, trace_id, start, false, None, None)
                .await
                .unwrap();
        let version_before = before.routes_version;

        // Simulate a config mutation (route CRUD / hot-reload).
        {
            let mut cfg = state.config.write().await;
            cfg.routes.retain(|r| r.model_name != "gpt-4o");
            cfg.routes.push(sag_config::RouteConfig {
                model_name: "gpt-4o".into(),
                api_endpoint: "/chat/completions".into(),
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
                }],
            });
            state.config.bump_routes_version();
        }
        assert!(state.config.routes_version() > version_before);

        let (_, _, _, after) =
            resolve_route(&state, &runtime, &req, trace_id, start, false, None, None)
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

    /// Build an AppState + DispatchRuntime for a single StickyFailover route.
    async fn build_sticky_state() -> (AppState, DispatchRuntime) {
        let mut providers: HashMap<String, sag_config::ProviderConfig> = HashMap::new();
        providers.insert("openai".into(), crate::testing::provider_config(true));
        let routes = vec![sag_config::RouteConfig {
            model_name: "gpt-4o".into(),
            api_endpoint: "/chat/completions".into(),
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
            model_name: "gpt-4o".into(),
            api_endpoint: "/chat/completions".into(),
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

        let result =
            resolve_route(&state, &runtime, &req, trace_id, start, false, None, None).await;
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

        let result =
            resolve_route(&state, &runtime, &req, trace_id, start, false, None, None).await;
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

        let result =
            resolve_route(&state, &runtime, &req, trace_id, start, false, None, None).await;
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
}
