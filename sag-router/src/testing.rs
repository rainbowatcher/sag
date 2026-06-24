//! Shared test helpers for constructing [`AppState`].
//!
//! Every test module previously hand-rolled its own `build_state` /
//! `test_state` that repeated the same 4 boilerplate fields
//! (`collector`, `route_counters`, `fill_first_orders`,
//! `route_circuit_breaker`). This module provides [`test_app_state`]
//! so a future `AppState` field addition only needs one edit here.
//!
//! This function is not `#[cfg(test)]`-gated so that integration
//! tests (`tests/auth_regression.rs`) can reach it through the
//! `pub use` re-export. In production builds the linker removes
//! the dead code; the module itself adds zero runtime cost.

use std::collections::HashMap;
use std::sync::Arc;

use sag_config::ConfigHandle;
use tokio::sync::RwLock;

use crate::state::AppState;

/// Build an [`AppState`] with the given config and provider registry,
/// filling in the four shared runtime fields with fresh empty state.
pub async fn test_app_state(
    config: ConfigHandle,
    providers: Arc<RwLock<HashMap<String, Arc<dyn sag_providers::LlmProvider>>>>,
) -> AppState {
    AppState {
        config,
        providers,
        collector: Arc::new(sag_analytics::CallLogCollector::new_memory().await.unwrap()),
        route_counters: Arc::new(RwLock::new(HashMap::new())),
        fill_first_orders: Arc::new(RwLock::new(HashMap::new())),
        route_circuit_breaker: Arc::new(RwLock::new(Default::default())),
    }
}
