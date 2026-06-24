use std::collections::HashMap;
use std::sync::Arc;

use sag_analytics::CallLogCollector;
use sag_config::ConfigHandle;
use sag_providers::LlmProvider;
use tokio::sync::RwLock;

use crate::circuit_breaker::RouteCircuitBreaker;
use crate::order::CachedOrder;

/// Shared application state for all Axum handlers.
#[derive(Clone)]
pub struct AppState {
    /// Hot-reloadable gateway configuration.
    pub config: ConfigHandle,
    /// Map of provider name → initialized provider adapter.
    pub providers: Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>>,
    /// Async call log collector for analytics.
    pub collector: Arc<CallLogCollector>,
    /// Round-robin counters keyed by route model_name.
    pub route_counters: Arc<RwLock<HashMap<String, u64>>>,
    /// Fill-first shuffled orders keyed by route model_name.
    /// Shuffled once on first access; reused across subsequent requests
    /// until the targets for that route change (config reload) or the
    /// TTL expires (default 2 hours).
    pub fill_first_orders: Arc<RwLock<HashMap<String, CachedOrder>>>,
    /// Runtime availability memory for route targets.
    pub route_circuit_breaker: Arc<RwLock<RouteCircuitBreaker>>,
}

impl AppState {
    /// Look up a provider by name from the registry.
    pub async fn get_provider(&self, name: &str) -> Option<Arc<dyn LlmProvider>> {
        let guard = self.providers.read().await;
        guard.get(name).cloned()
    }
}
