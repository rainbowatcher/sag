use std::collections::HashMap;
use std::sync::Arc;

use sag_analytics::CallLogCollector;
use sag_config::ConfigHandle;
use sag_providers::LlmProvider;
use tokio::sync::RwLock;

use crate::circuit_breaker::RouteCircuitBreaker;
use crate::order::CachedOrder;

/// Shared application state for all Axum handlers.
///
/// Holds only the cross-cutting resources that every handler family
/// (proxy dispatch, admin CRUD, health/analytics) needs: the hot-reloadable
/// config, the provider registry, and the analytics collector. Dispatch-only
/// runtime state (circuit breaker, ordering caches) lives in
/// [`DispatchRuntime`] so non-dispatch handlers don't carry it.
#[derive(Clone)]
pub struct AppState {
    /// Hot-reloadable gateway configuration.
    pub config: ConfigHandle,
    /// Map of provider name → initialized provider adapter.
    pub providers: Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>>,
    /// Async call log collector for analytics.
    pub collector: Arc<CallLogCollector>,
}

impl AppState {
    /// Look up a provider by name from the registry.
    pub async fn get_provider(&self, name: &str) -> Option<Arc<dyn LlmProvider>> {
        let guard = self.providers.read().await;
        guard.get(name).cloned()
    }
}

/// Per-request runtime state for the dispatch engine.
///
/// These three fields are only touched by `dispatch/` and `order.rs`:
/// the circuit-breaker map, the round-robin counters, and the FillFirst
/// shuffle cache. Keeping them out of [`AppState`] means the admin and
/// analytics handlers don't depend on dispatch internals, and the
/// dispatch crate can later be extracted without dragging unrelated
/// state along.
#[derive(Clone, Default)]
pub struct DispatchRuntime {
    /// Round-robin counters keyed by route model_name.
    pub route_counters: Arc<RwLock<HashMap<String, u64>>>,
    /// Fill-first shuffled orders keyed by route model_name.
    /// Shuffled once on first access; reused across subsequent requests
    /// until the targets for that route change (config reload) or the
    /// TTL expires (default 2 hours).
    pub fill_first_orders: Arc<RwLock<HashMap<String, CachedOrder>>>,
    /// Sticky-failover active targets keyed by request model name.
    /// Stores (provider, real_model) of the last successful target.
    pub active_targets: Arc<RwLock<HashMap<String, (String, String)>>>,
    /// Runtime availability memory for route targets.
    pub route_circuit_breaker: Arc<RwLock<RouteCircuitBreaker>>,
}
