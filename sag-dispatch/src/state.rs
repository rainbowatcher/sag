//! Dispatch-only runtime state.
//!
//! Shared [`AppState`] has moved to `sag-state` so that `sag-admin`
//! can use it without depending on the dispatch engine. This module
//! re-exports `AppState` for backwards compatibility and owns
//! [`DispatchRuntime`] — the runtime state only touched by
//! `dispatch/` and `order.rs`.

pub use sag_state::AppState;

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::circuit_breaker::RouteCircuitBreaker;
use crate::order::CachedOrder;

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
