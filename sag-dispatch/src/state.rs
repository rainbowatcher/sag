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

use std::time::Duration;

use sag_config::{GatewayConfig, default_timeout_secs, default_ttfb_timeout_secs};
use sag_core::compute_stale_probe_threshold;

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
    /// Stores (provider, real_model, stick_start_time) of the last successful target.
    /// The third element is the `Instant` when the sticky target was recorded,
    /// used for the configurable expiry mechanism.
    ///
    /// Shared with [`AppState`] via the same `Arc` (injected through
    /// [`DispatchRuntime::with_shared_state`]) so admin mutations — e.g.
    /// clearing sticky memory when a breaker is manually closed — are
    /// immediately visible to dispatch ordering.
    pub active_targets: sag_state::ActiveTargets,
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
    /// Construct a `DispatchRuntime` that shares the given circuit-breaker
    /// and sticky-failover active-target `Arc`s (instead of creating its
    /// own defaults). Used by `main.rs` so that `AppState` and
    /// `DispatchRuntime` observe the *same* live breaker and the *same*
    /// sticky memory — admin mutations to `active_targets` (e.g. clearing
    /// sticky routing when a breaker is manually closed) are immediately
    /// visible to dispatch ordering.
    ///
    /// All other fields (counters, ordering cache, prune version) are
    /// initialized to their defaults.
    pub fn with_shared_state(
        route_circuit_breaker: Arc<RwLock<RouteCircuitBreaker>>,
        active_targets: sag_state::ActiveTargets,
    ) -> Self {
        Self {
            route_circuit_breaker,
            active_targets,
            ..Default::default()
        }
    }

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

/// Compute the stale-probe threshold for the circuit breaker from the current
/// gateway configuration.
///
/// Walks every (route, target), applying the **same fallback resolution** used
/// by `resolve.rs` to derive the effective `timeout_secs` and `retries` for each
/// target:
/// - `timeout_secs`: **three-tier** fallback —
///   `target → route → provider → global default`.
/// - `retries`: **two-tier** fallback —
///   `target → route → global` (there is no provider tier, since `retries` is
///   not a provider-level field).
///
/// The per-attempt upper bound also considers the max effective
/// `ttfb_timeout_secs` across providers (streaming requests are bounded by ttfb,
/// not by `timeout_secs`).
///
/// # Conservative over-approximation
///
/// `max_per_attempt_secs` and `max_retries` are taken **independently** across
/// all targets — i.e. one target may supply the max timeout while another
/// supplies the max retries. The resulting threshold is therefore an upper
/// bound on the true per-target worst case, never an underestimate. For a
/// stale-probe reclamation threshold this is the safe direction: it may
/// delay reclamation slightly but never reclaims a slot too eagerly.
///
/// The resulting `max_per_attempt_secs`, `max_retries`, and
/// `config.backoff.max_ms` are fed to [`compute_stale_probe_threshold`].
///
/// # Edge cases
/// - Routes with no targets still contribute their route-level `retries` /
///   `timeout_secs` overrides (they are valid upper bounds for a future target).
/// - Empty `routes` / no contributing values fall back to the provider defaults
///   for `timeout_secs` / `ttfb_timeout_secs` (or the global defaults), and to
///   the global default for `retries`, so the threshold is never zero — the
///   `compute_stale_probe_threshold` floor (600s) is the final safety net.
///
/// This is a pure, synchronous function over a config snapshot; callers should
/// hold a read lock on the [`ConfigHandle`] when invoking it during hot-reload.
pub fn stale_probe_threshold_from_config(config: &GatewayConfig) -> Duration {
    let global_retries = config.backoff.retries;
    let max_backoff_ms = config.backoff.max_ms;
    let provider_timeouts: HashMap<String, u64> = config
        .providers
        .iter()
        .map(|(k, v)| (k.clone(), v.timeout_secs))
        .collect();
    let provider_ttfbs: HashMap<String, u64> = config
        .providers
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                v.ttfb_timeout_secs.unwrap_or(default_ttfb_timeout_secs()),
            )
        })
        .collect();

    let mut max_per_attempt: u64 = 0;
    let mut max_retries: u32 = 0;

    for route in &config.routes {
        let rt_timeout = route.timeout_secs;
        let rt_retries = route.retries;
        for target in &route.targets {
            // Mirror resolve.rs fallback: `timeout_secs` uses three-tier resolution
            // (target → route → provider → default), while `retries` uses two-tier
            // (target → route → global; there is no provider tier).
            let t_retries = target.retries.or(rt_retries).unwrap_or(global_retries);
            let t_timeout = target
                .timeout_secs
                .or(rt_timeout)
                .or_else(|| provider_timeouts.get(&target.provider).copied())
                .unwrap_or(default_timeout_secs());
            let t_ttfb = provider_ttfbs
                .get(&target.provider)
                .copied()
                .unwrap_or(default_ttfb_timeout_secs());

            max_per_attempt = max_per_attempt.max(t_timeout).max(t_ttfb);
            max_retries = max_retries.max(t_retries);
        }

        // Route with no targets: route-level overrides are still valid upper
        // bounds for any future target added to this route.
        if route.targets.is_empty() {
            if let Some(t) = rt_timeout {
                max_per_attempt = max_per_attempt.max(t);
            }
            if let Some(r) = rt_retries {
                max_retries = max_retries.max(r);
            }
        }
    }

    // No routes contributed (e.g. empty `routes`): fall back to provider
    // defaults so the threshold is never zero — the floor is the final net.
    // Consider both `timeout_secs` and `ttfb_timeout_secs` so a provider with
    // a high ttfb but default timeout still seeds the per-attempt bound.
    if max_per_attempt == 0 {
        max_per_attempt = provider_timeouts
            .values()
            .copied()
            .chain(provider_ttfbs.values().copied())
            .max()
            .unwrap_or(default_timeout_secs());
    }
    if max_retries == 0 {
        max_retries = global_retries;
    }

    compute_stale_probe_threshold(max_per_attempt, max_retries, max_backoff_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

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
        rt.active_targets.write().await.insert(
            "deleted-model".into(),
            ("p".into(), "m".into(), Instant::now()),
        );

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
        rt.active_targets.write().await.insert(
            "valid-model".into(),
            ("p2".into(), "m2".into(), Instant::now()),
        );

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
            .insert("m1".into(), ("p".into(), "rm".into(), Instant::now()));

        let valid: HashSet<&str> = ["m1"].into_iter().collect();
        rt.prune_stale_entries(&valid).await;

        assert!(rt.fill_first_orders.read().await.contains_key("m1"));
        assert!(rt.route_counters.read().await.contains_key("m1"));
        assert!(rt.active_targets.read().await.contains_key("m1"));
    }

    // ── stale_probe_threshold_from_config tests ────────────────────

    use sag_config::{
        BackoffConfig, GatewayConfig, ProviderConfig, ProviderType, RouteConfig, RouteTarget,
        Strategy,
    };

    /// Minimal provider config with a custom `timeout_secs` and optional ttfb.
    fn prov(timeout_secs: u64, ttfb: Option<u64>) -> ProviderConfig {
        ProviderConfig {
            provider_type: ProviderType::OpenAI,
            api_keys: vec!["sk-test".into()],
            key_strategy: sag_config::KeyStrategy::FillFirst,
            base_url: "https://api.openai.com/v1".into(),
            models_endpoint: "models".into(),
            timeout_secs,
            headers: HashMap::new(),
            body_overrides: HashMap::new(),
            strip_fields: vec![],
            model_overrides: HashMap::new(),
            role_mappings: HashMap::new(),
            enabled_models: vec![],
            enabled: true,
            anthropic_cache_control: true,
            chunk_timeout_secs: None,
            ttfb_timeout_secs: ttfb,
        }
    }

    fn route(model: &str, targets: Vec<RouteTarget>) -> RouteConfig {
        RouteConfig {
            model_name: model.into(),
            api_endpoint: "/chat/completions".into(),
            retries: None,
            timeout_secs: None,
            strategy: Strategy::FillFirst,
            targets,
        }
    }

    fn target(provider: &str) -> RouteTarget {
        RouteTarget {
            provider: provider.into(),
            real_model: None,
            priority: 0,
            retries: None,
            timeout_secs: None,
            extra: HashMap::new(),
        }
    }

    fn config_with(
        providers: HashMap<String, ProviderConfig>,
        routes: Vec<RouteConfig>,
        backoff: BackoffConfig,
    ) -> GatewayConfig {
        GatewayConfig {
            providers,
            routes,
            backoff,
            ..sag_config::gateway_config_base!()
        }
    }

    #[test]
    fn threshold_takes_per_target_timeout_override() {
        // provider timeout=120, but one target overrides to 1200.
        // per-attempt should be 1200, retries=2 (global) → 1200×3 + 10 = 3610
        let mut providers = HashMap::new();
        providers.insert("p1".into(), prov(120, None));
        let mut t = target("p1");
        t.timeout_secs = Some(1200);
        let routes = vec![route("m1", vec![t])];
        let config = config_with(providers, routes, BackoffConfig::default());
        let threshold = stale_probe_threshold_from_config(&config);
        assert_eq!(threshold, Duration::from_secs(3610));
    }

    #[test]
    fn threshold_takes_per_target_retries_override() {
        // global retries=2, but target overrides to 5.
        // per-attempt=120 (provider), attempts=6 → 120×6 + backoff(5000×5/1000=25)=745
        // 745 > 600 (floor) so no floor intervention.
        let mut providers = HashMap::new();
        providers.insert("p1".into(), prov(120, None));
        let mut t = target("p1");
        t.retries = Some(5);
        let routes = vec![route("m1", vec![t])];
        let config = config_with(providers, routes, BackoffConfig::default());
        let threshold = stale_probe_threshold_from_config(&config);
        assert_eq!(threshold, Duration::from_secs(745));
    }

    #[test]
    fn threshold_takes_route_level_retries_fallback() {
        // target.retries=None, route.retries=4 → effective retries=4.
        // per-attempt=120, attempts=5 → 120×5 + 5000×4/1000=20 = 620
        let mut providers = HashMap::new();
        providers.insert("p1".into(), prov(120, None));
        let mut r = route("m1", vec![target("p1")]);
        r.retries = Some(4);
        let config = config_with(providers, vec![r], BackoffConfig::default());
        let threshold = stale_probe_threshold_from_config(&config);
        assert_eq!(threshold, Duration::from_secs(620));
    }

    #[test]
    fn threshold_applies_ttfb_max() {
        // provider timeout=120 but ttfb=900 → per-attempt = max(120, 900) = 900.
        // retries=2 → 900×3 + 10 = 2710
        let mut providers = HashMap::new();
        providers.insert("p1".into(), prov(120, Some(900)));
        let routes = vec![route("m1", vec![target("p1")])];
        let config = config_with(providers, routes, BackoffConfig::default());
        let threshold = stale_probe_threshold_from_config(&config);
        assert_eq!(threshold, Duration::from_secs(2710));
    }

    #[test]
    fn threshold_floor_kicks_in_for_extreme_small() {
        // timeout=10, retries=0 → 10×1 + 0 = 10 < 600 → floor
        let mut providers = HashMap::new();
        providers.insert("p1".into(), prov(10, None));
        let mut t = target("p1");
        t.retries = Some(0);
        let routes = vec![route("m1", vec![t])];
        let config = config_with(providers, routes, BackoffConfig::default());
        let threshold = stale_probe_threshold_from_config(&config);
        assert_eq!(threshold, Duration::from_secs(600));
    }

    #[test]
    fn threshold_empty_routes_uses_provider_default() {
        // routes=[], one provider timeout=300, retries=2 (global default).
        // per-attempt=300 (from provider fallback), attempts=3 → 300×3 + 10 = 910
        let mut providers = HashMap::new();
        providers.insert("p1".into(), prov(300, None));
        let config = config_with(providers, vec![], BackoffConfig::default());
        let threshold = stale_probe_threshold_from_config(&config);
        assert_eq!(threshold, Duration::from_secs(910));
    }

    #[test]
    fn threshold_route_timeout_beats_provider_timeout() {
        // Core fallback chain: target → route → provider → default.
        // target leaves timeout unset, route sets 200, provider sets 100.
        // Route must win over provider: per-attempt=200, retries=2 (global)
        // → 200×3 + 10 = 610.
        let mut providers = HashMap::new();
        providers.insert("p1".into(), prov(100, None));
        let mut r = route("m1", vec![target("p1")]);
        r.timeout_secs = Some(200);
        let config = config_with(providers, vec![r], BackoffConfig::default());
        let threshold = stale_probe_threshold_from_config(&config);
        assert_eq!(threshold, Duration::from_secs(610));
    }

    #[test]
    fn threshold_takes_independent_max_across_targets() {
        // `max_per_attempt` and `max_retries` are taken independently across
        // all targets (conservative over-approximation): target A supplies the
        // max retries (5), target B supplies the max per-attempt timeout (500).
        // per-attempt = 500, attempts = 6 → 500×6 + 5000×5/1000 = 3000 + 25 = 3025.
        let mut providers = HashMap::new();
        providers.insert("p1".into(), prov(120, None));
        providers.insert("p2".into(), prov(500, None));
        let mut a = target("p1");
        a.retries = Some(5);
        let b = target("p2");
        let routes = vec![route("m1", vec![a, b])];
        let config = config_with(providers, routes, BackoffConfig::default());
        let threshold = stale_probe_threshold_from_config(&config);
        assert_eq!(threshold, Duration::from_secs(3025));
    }
}
