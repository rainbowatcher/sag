//! Shared application state for SAG.
//!
//! This crate owns [`AppState`], the cross-cutting type used as Axum
//! [`axum::extract::State`] by all handler families (proxy dispatch,
//! admin CRUD, health/analytics). It lives here — below both
//! `sag-admin` and `sag-dispatch` — so that the management layer
//! does not depend on the execution layer.
//!
//! Dispatch-only runtime state lives in `sag-dispatch` as
//! `DispatchRuntime`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use sag_analytics::CallLogCollector;
use sag_config::ConfigHandle;
use sag_core::RouteCircuitBreaker;
use sag_providers::LlmProvider;
use tokio::sync::RwLock;

/// Sticky-failover active-target memory: `model → (provider, real_model,
/// recorded_at)`. Shared between [`AppState`] and `DispatchRuntime` via the
/// same `Arc` so admin mutations (e.g. clearing sticky memory when a breaker
/// is manually closed) are immediately visible to dispatch ordering.
pub type ActiveTargets = Arc<RwLock<HashMap<String, (String, String, Instant)>>>;

/// Shared application state for all Axum handlers.
///
/// Holds only the cross-cutting resources that every handler family
/// (proxy dispatch, admin CRUD, health/analytics) needs: the hot-reloadable
/// config, the provider registry, the analytics collector, and the shared
/// circuit-breaker handle. The circuit breaker is shared with
/// [`sag_dispatch::DispatchRuntime`] via the same `Arc` so admin handlers
/// can observe live breaker state without depending on the dispatch crate.
#[derive(Clone)]
pub struct AppState {
    /// Hot-reloadable gateway configuration.
    pub config: ConfigHandle,
    /// Map of provider name → initialized provider adapter.
    pub providers: Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>>,
    /// Async call log collector for analytics.
    pub collector: Arc<CallLogCollector>,
    /// Shared route-target circuit breaker. The same `Arc` is injected into
    /// `DispatchRuntime` at startup, so this handle observes live state.
    pub circuit_breaker: Arc<RwLock<RouteCircuitBreaker>>,
    /// Sticky-failover active-target memory, shared with `DispatchRuntime`
    /// via the same `Arc`. Admin handlers clear an entry here to reset
    /// sticky routing after a manual breaker close; dispatch reads it in
    /// `order_targets` to prepend the active target, and writes the active
    /// target on a successful response.
    pub active_targets: ActiveTargets,
}

impl AppState {
    /// Look up a provider by name from the registry.
    pub async fn get_provider(&self, name: &str) -> Option<Arc<dyn LlmProvider>> {
        let guard = self.providers.read().await;
        guard.get(name).cloned()
    }

    /// Create or update a provider runtime adapter from the given config and
    /// insert it into the live registry.
    ///
    /// The caller should pass the up-to-date [`ProviderConfig`] that was just
    /// written to `config.providers`; this avoids an extra config read lock.
    pub async fn upsert_provider(
        &self,
        name: &str,
        config: &sag_config::ProviderConfig,
    ) -> sag_core::Result<()> {
        match sag_providers::create_provider(name.to_string(), config) {
            Ok(provider) => {
                let mut providers = self.providers.write().await;
                providers.insert(name.to_string(), provider);
                Ok(())
            }
            Err(e) => {
                tracing::error!(provider = name, error = %e, "failed to create provider adapter");
                Err(e)
            }
        }
    }

    /// Remove a provider from the runtime adapter registry.
    ///
    /// Call after removing the provider from config; in-flight requests that
    /// have already snapshotted the `Arc<dyn LlmProvider>` will continue to
    /// use it, but new lookups will see the absence.
    pub async fn remove_provider_runtime(&self, name: &str) {
        let mut providers = self.providers.write().await;
        providers.remove(name);
    }
}

// ── Testing helpers (not #[cfg(test)] so integration tests can reach them) ──

/// Shared test helpers for constructing [`AppState`].
pub mod testing {
    use std::collections::HashMap;
    use std::sync::Arc;

    use sag_config::ConfigHandle;
    use tokio::sync::RwLock;

    use super::AppState;

    /// Build an [`AppState`] with the given config and provider registry,
    /// backed by an in-memory analytics collector.
    pub async fn test_app_state(
        config: ConfigHandle,
        providers: Arc<RwLock<HashMap<String, Arc<dyn sag_providers::LlmProvider>>>>,
    ) -> AppState {
        AppState {
            config,
            providers,
            collector: Arc::new(sag_analytics::CallLogCollector::new_memory().await.unwrap()),
            circuit_breaker: Arc::new(RwLock::new(sag_core::RouteCircuitBreaker::default())),
            active_targets: Arc::default(),
        }
    }

    /// Build a provider registry from the given config handle, optionally
    /// filtering to only enabled providers. Returns the registry wrapped in
    /// an `Arc<RwLock<...>>` ready for [`test_app_state`].
    pub async fn build_provider_registry(
        handle: &ConfigHandle,
        only_enabled: bool,
    ) -> Arc<RwLock<HashMap<String, Arc<dyn sag_providers::LlmProvider>>>> {
        let cfg_read = handle.read().await;
        let mut registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
        for (name, pc) in cfg_read.providers.iter() {
            if !only_enabled || pc.enabled {
                let provider: Arc<dyn sag_providers::LlmProvider> =
                    Arc::new(sag_providers::OpenAiProvider::new(name.clone(), pc).unwrap());
                registry.insert(name.clone(), provider);
            }
        }
        drop(cfg_read);
        Arc::new(RwLock::new(registry))
    }
}
