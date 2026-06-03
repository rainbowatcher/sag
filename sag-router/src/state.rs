use std::collections::HashMap;
use std::sync::Arc;

use sag_analytics::CallLogCollector;
use sag_config::ConfigHandle;
use sag_providers::LlmProvider;
use tokio::sync::RwLock;

/// Shared application state for all Axum handlers.
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
