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
pub async fn resolve_route(
    state: &AppState,
    runtime: &DispatchRuntime,
    request: &ChatCompletionRequest,
    trace_id: uuid::Uuid,
    start: Instant,
    is_stream: bool,
) -> Result<ResolvedRoute, Response<Body>> {
    let config_guard = state.config.read().await;

    let route = config_guard
        .routes
        .iter()
        .find(|r| r.model_name == request.model)
        .or_else(|| config_guard.routes.iter().find(|r| r.model_name == "*"));

    let Some(route) = route else {
        return Err(failure_response(
            state,
            trace_id,
            "",
            &request.model,
            "",
            is_stream,
            start,
            StatusCode::NOT_FOUND,
            AppError::ModelNotFound(request.model.clone()),
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
            "",
            &request.model,
            "",
            is_stream,
            start,
            StatusCode::BAD_GATEWAY,
            err,
        ));
    }

    let strategy = route.strategy;
    let api_endpoint = route.api_endpoint.clone();
    let route_retries = route.retries;
    let route_timeout = route.timeout_secs;
    let global_retries = config_guard.backoff.retries;
    let shuffle_cache_ttl_secs = config_guard.shuffle_cache_ttl_secs;
    let provider_timeouts: HashMap<String, u64> = config_guard
        .providers
        .iter()
        .map(|(k, v)| (k.clone(), v.timeout_secs))
        .collect();
    drop(config_guard);

    // Apply target selection strategy
    let ordered = order_targets(
        targets,
        &request.model,
        strategy,
        &runtime.route_counters,
        &runtime.fill_first_orders,
        &runtime.active_targets,
        shuffle_cache_ttl_secs,
    )
    .await;

    // Resolve effective retries and timeout per target:
    // target → route → provider / global
    let resolved: Vec<ResolvedTarget> = ordered
        .into_iter()
        .map(|t| {
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
                extra: t.extra,
                retries,
                timeout_secs,
            }
        })
        .collect();

    Ok((resolved, api_endpoint, strategy))
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
}
