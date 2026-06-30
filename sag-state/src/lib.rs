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

use sag_analytics::CallLogCollector;
use sag_config::ConfigHandle;
use sag_providers::LlmProvider;
use tokio::sync::RwLock;

/// Shared application state for all Axum handlers.
///
/// Holds only the cross-cutting resources that every handler family
/// (proxy dispatch, admin CRUD, health/analytics) needs: the hot-reloadable
/// config, the provider registry, and the analytics collector.
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
                    Arc::new(sag_providers::OpenAiProvider::new(name.clone(), pc));
                registry.insert(name.clone(), provider);
            }
        }
        drop(cfg_read);
        Arc::new(RwLock::new(registry))
    }
}
