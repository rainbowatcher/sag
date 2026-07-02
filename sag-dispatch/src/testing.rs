//! Shared test helpers for constructing [`DispatchRuntime`].
//!
//! [`AppState`] construction helpers (`test_app_state`,
//! `build_provider_registry`) have moved to `sag-state` and are
//! re-exported here for backwards compatibility.
//!
//! These functions are not `#[cfg(test)]`-gated so that integration
//! tests can reach them through the `pub use` re-export. In production
//! builds the linker removes the dead code.

pub use sag_state::testing::{build_provider_registry, test_app_state};

use crate::dispatch::ResolvedTarget;
use crate::state::DispatchRuntime;

/// Shared test target constructor — avoids copy-paste across chat / image
/// dispatch test modules.
pub fn target(provider: &str, real_model: &str, retries: u32) -> ResolvedTarget {
    ResolvedTarget {
        provider: provider.into(),
        real_model: real_model.into(),
        extra: std::collections::HashMap::new(),
        retries,
        timeout_secs: 120,
    }
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
        model_overrides: std::collections::HashMap::new(),
        role_mappings: std::collections::HashMap::new(),
        enabled_models: vec![],
        enabled,
        anthropic_cache_control: true,
        chunk_timeout_secs: None,
    }
}

/// Build a fresh [`DispatchRuntime`] with empty circuit-breaker / ordering
/// state. Call once per test for isolation.
pub fn test_dispatch_runtime() -> DispatchRuntime {
    DispatchRuntime::default()
}
