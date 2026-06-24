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

use crate::state::{AppState, DispatchRuntime};

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
