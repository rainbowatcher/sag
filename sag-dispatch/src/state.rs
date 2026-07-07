//! Dispatch-only runtime state.
//!
//! Shared [`AppState`] has moved to `sag-state` so that `sag-admin`
//! can use it without depending on the dispatch engine. This module
//! re-exports `AppState` for backwards compatibility and owns
//! [`DispatchRuntime`] — the runtime state only touched by
//! `dispatch/` and `order.rs`.

pub use sag_state::AppState;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
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
#[derive(Clone)]
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
    /// Last routes version seen by the prune guard. When this matches
    /// [`ConfigHandle::routes_version`] the prune is skipped entirely,
    /// avoiding three write-lock acquisitions on every request.
    pub(crate) last_prune_version: Arc<AtomicU64>,
}

impl Default for DispatchRuntime {
    fn default() -> Self {
        Self {
            route_counters: Arc::default(),
            fill_first_orders: Arc::default(),
            active_targets: Arc::default(),
            route_circuit_breaker: Arc::default(),
            last_prune_version: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl DispatchRuntime {
    /// Remove cached entries for model names that no longer exist
    /// in the current config routes. Called at the start of every
    /// `resolve_route` to prevent unbounded growth from
    /// create/delete route cycles.
    pub async fn prune_stale_entries(&self, valid_model_names: &HashSet<&str>) {
        {
            let mut guard = self.fill_first_orders.write().await;
            guard.retain(|k, _| valid_model_names.contains(k.as_str()));
        }
        {
            let mut guard = self.route_counters.write().await;
            guard.retain(|k, _| valid_model_names.contains(k.as_str()));
        }
        {
            let mut guard = self.active_targets.write().await;
            guard.retain(|k, _| valid_model_names.contains(k.as_str()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prune_stale_entries_removes_deleted_routes() {
        let rt = DispatchRuntime::default();

        // Pre-populate caches with a model that simulates a deleted route.
        rt.fill_first_orders.write().await.insert(
            "deleted-model".into(),
            CachedOrder {
                targets: Arc::from([]),
                created_at: std::time::Instant::now(),
            },
        );
        rt.route_counters
            .write()
            .await
            .insert("deleted-model".into(), 42);
        rt.active_targets
            .write()
            .await
            .insert("deleted-model".into(), ("p".into(), "m".into()));

        // Also add a valid entry that should survive.
        rt.fill_first_orders.write().await.insert(
            "valid-model".into(),
            CachedOrder {
                targets: Arc::from([]),
                created_at: std::time::Instant::now(),
            },
        );
        rt.route_counters
            .write()
            .await
            .insert("valid-model".into(), 1);
        rt.active_targets
            .write()
            .await
            .insert("valid-model".into(), ("p2".into(), "m2".into()));

        let valid: HashSet<&str> = ["valid-model"].into_iter().collect();
        rt.prune_stale_entries(&valid).await;

        // Stale entries removed.
        assert!(
            !rt.fill_first_orders
                .read()
                .await
                .contains_key("deleted-model")
        );
        assert!(!rt.route_counters.read().await.contains_key("deleted-model"));
        assert!(!rt.active_targets.read().await.contains_key("deleted-model"));

        // Valid entries survive.
        assert!(
            rt.fill_first_orders
                .read()
                .await
                .contains_key("valid-model")
        );
        assert!(rt.route_counters.read().await.contains_key("valid-model"));
        assert!(rt.active_targets.read().await.contains_key("valid-model"));
    }

    #[tokio::test]
    async fn prune_stale_entries_noop_when_all_valid() {
        let rt = DispatchRuntime::default();
        rt.fill_first_orders.write().await.insert(
            "m1".into(),
            CachedOrder {
                targets: Arc::from([]),
                created_at: std::time::Instant::now(),
            },
        );
        rt.route_counters.write().await.insert("m1".into(), 1);
        rt.active_targets
            .write()
            .await
            .insert("m1".into(), ("p".into(), "rm".into()));

        let valid: HashSet<&str> = ["m1"].into_iter().collect();
        rt.prune_stale_entries(&valid).await;

        assert!(rt.fill_first_orders.read().await.contains_key("m1"));
        assert!(rt.route_counters.read().await.contains_key("m1"));
        assert!(rt.active_targets.read().await.contains_key("m1"));
    }
}
