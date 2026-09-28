//! Shared test helpers for constructing [`DispatchRuntime`].
//!
//! [`crate::state::AppState`] construction helpers (`test_app_state`,
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
/// dispatch test modules. Records an exact-route entry (`route_model` =
/// `real_model`, explicit `target_real_model`) which matches the semantics
/// of the exact-route scenarios these tests exercise.
pub fn target(provider: &str, real_model: &str, retries: u32) -> ResolvedTarget {
    ResolvedTarget {
        provider: provider.into(),
        real_model: real_model.into(),
        route_model: real_model.into(),
        target_real_model: Some(real_model.into()),
        extra: std::collections::HashMap::new(),
        retries,
        timeout_secs: 120,
        upstream_protocol: sag_core::ApiKind::ChatCompletions,
        lane: sag_core::Lane::Translated,
    }
}

/// Shared `ProviderConfig` builder for unit tests. Returns a minimal
/// enabled OpenAI-compatible provider declaring a single
/// `chat_completions` API node.
pub fn provider_config(enabled: bool) -> sag_config::ProviderConfig {
    let mut pc = sag_config::ProviderConfig {
        api_keys: vec!["sk-test".into()],
        key_strategy: sag_config::KeyStrategy::FillFirst,
        proxy_mode: sag_config::ProxyMode::default(),
        timeout_secs: 120,
        enabled,
        ..sag_config::ProviderConfig::default()
    };
    pc.apis.insert(
        sag_core::ApiKind::ChatCompletions,
        sag_config::ApiConfig {
            base_url: "https://api.openai.com/v1".into(),
            ..sag_config::ApiConfig::default()
        },
    );
    pc
}

/// Build a fresh [`DispatchRuntime`] with empty circuit-breaker / ordering
/// state. Call once per test for isolation.
pub fn test_dispatch_runtime() -> DispatchRuntime {
    DispatchRuntime::default()
}
