//! Shared test helpers for constructing [`AppState`] + [`DispatchRuntime`].
//!
//! Every test module previously hand-rolled its own `build_state` /
//! `test_state` that repeated the same 4 boilerplate fields
//! (`collector`, `route_counters`, `fill_first_orders`,
//! `route_circuit_breaker`). After the `AppState` / `DispatchRuntime`
//! split, `test_app_state` builds the shared [`AppState`] and
//! `test_dispatch_runtime` builds the dispatch-only runtime. Both are
//! fresh-per-call so tests stay isolated.
//!
//! These functions are not `#[cfg(test)]`-gated so that integration
//! tests (`tests/auth_regression.rs`) can reach them through the
//! `pub use` re-export. In production builds the linker removes
//! the dead code; the module itself adds zero runtime cost.

use std::collections::HashMap;
use std::sync::Arc;

use sag_config::ConfigHandle;
use tokio::sync::RwLock;

use crate::dispatch::ResolvedTarget;
use crate::state::{AppState, DispatchRuntime};

/// Shared test target constructor — avoids copy-paste across chat / image
/// dispatch test modules.
pub fn target(provider: &str, real_model: &str, retries: u32) -> ResolvedTarget {
    (
        provider.into(),
        real_model.into(),
        std::collections::HashMap::new(),
        retries,
    )
}

/// Shared `ProviderConfig` builder for unit tests. Returns a minimal
/// enabled OpenAI provider config.
pub fn provider_config(enabled: bool) -> sag_config::ProviderConfig {
    sag_config::ProviderConfig {
        provider_type: sag_config::ProviderType::OpenAI,
        api_keys: vec!["sk-test".into()],
        key_strategy: sag_config::KeyStrategy::FillFirst,
        base_url: "https://api.openai.com/v1".into(),
        models_endpoint: "models".into(),
        timeout_secs: 120,
        headers: std::collections::HashMap::new(),
        body_overrides: std::collections::HashMap::new(),
        role_mappings: std::collections::HashMap::new(),
        enabled_models: vec![],
        enabled,
        anthropic_cache_control: true,
    }
}

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

/// Build a fresh [`DispatchRuntime`] with empty circuit-breaker / ordering
/// state. Call once per test for isolation.
pub fn test_dispatch_runtime() -> DispatchRuntime {
    DispatchRuntime::default()
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
