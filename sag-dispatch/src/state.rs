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
use std::sync::atomic::AtomicU64;
use tokio::sync::RwLock;

use std::time::Duration;

use sag_config::{GatewayConfig, default_timeout_secs, default_ttfb_timeout_secs};
use sag_core::{RouteCircuitBreaker, compute_stale_probe_threshold};

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
    /// TTL expires (default 2 hours). Shared with [`AppState`] via the
    /// same `Arc` so admin handlers can observe / reset the fill-first
    /// order.
    pub fill_first_orders: sag_state::FillFirstOrders,
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
    /// Runtime availability memory for route targets. Shared with
    /// [`AppState`](sag_state::AppState) via the same `Arc` (injected through
    /// [`DispatchRuntime::with_shared_state`]) so admin mutations — e.g. a
    /// manual breaker close — are immediately visible to dispatch ordering.
    pub circuit_breaker: Arc<RwLock<RouteCircuitBreaker>>,
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
            circuit_breaker: Arc::default(),
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
        circuit_breaker: Arc<RwLock<RouteCircuitBreaker>>,
        active_targets: sag_state::ActiveTargets,
        fill_first_orders: sag_state::FillFirstOrders,
    ) -> Self {
        Self {
            circuit_breaker,
            active_targets,
            fill_first_orders,
            ..Default::default()
        }
    }

    /// Remove cached entries for request models that no longer resolve to any
    /// route in the current config. Called at the start of every
    /// `resolve_route` when the `routes_version` has changed, to prevent
    /// unbounded growth from create/delete route cycles.
    ///
    /// All three maps are keyed by the **request** model — for a wildcard
    /// route that is any concrete model the route has served — so validity is
    /// judged by re-resolving the key against the current route table, never
    /// by checking membership in the config's `model_name` set (which may
    /// only contain `"*"`). Active entries additionally record the route that
    /// created them (`ActiveTargetEntry::route_model`); an entry is kept only
    /// while its key still resolves to that same route **and** the resolved
    /// route is still [`sag_config::Strategy::StickyFailover`] (see
    /// [`ActiveTargetsInner::retain_sticky_routes`]), so:
    /// - a wildcard request key survives config changes that do not affect
    ///   the wildcard route (it keeps resolving to `"*"`),
    /// - a key that no longer resolves to its recorded route — an exact route
    ///   now shadowing a previously-wildcard key, or the route being removed
    ///   entirely — is dropped, and
    /// - a pin whose route switched away from StickyFailover (e.g. to
    ///   FillFirst) is dropped, so no stale active target lingers after a
    ///   strategy change.
    pub async fn prune_stale_entries(&self, routes: &[sag_config::RouteConfig]) {
        let resolves_to = |request_model: &str| -> Option<&str> {
            sag_config::resolve_model_route(routes, request_model).map(|r| r.model_name.as_str())
        };
        {
            let mut guard = self.fill_first_orders.write().await;
            guard.retain(|k, _| resolves_to(k).is_some());
        }
        {
            let mut guard = self.route_counters.write().await;
            guard.retain(|k, _| resolves_to(k).is_some());
        }
        {
            let mut guard = self.active_targets.write().await;
            guard.retain_sticky_routes(routes);
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
    use sag_state::CachedOrder;
    use std::time::Instant;

    /// Minimal route table for prune tests: an exact route `valid-model` and
    /// a wildcard route `*`.
    fn test_routes() -> Vec<sag_config::RouteConfig> {
        use sag_config::{RouteConfig, RouteTarget, Strategy};
        fn route(model: &str) -> RouteConfig {
            RouteConfig {
                model_name: model.into(),
                api_endpoint: "/chat/completions".into(),
                strategy: Strategy::StickyFailover,
                retries: None,
                timeout_secs: None,
                targets: vec![RouteTarget {
                    provider: "p".into(),
                    real_model: None,
                    priority: 0,
                    retries: None,
                    timeout_secs: None,
                    extra: HashMap::new(),
                }],
            }
        }
        vec![route("valid-model"), route("*")]
    }

    #[tokio::test]
    async fn prune_stale_entries_removes_deleted_routes() {
        let rt = DispatchRuntime::default();

        // Pre-populate caches with a model that simulates a deleted route.
        rt.fill_first_orders.write().await.insert(
            "deleted-model".into(),
            CachedOrder {
                targets: Arc::from([]),
                created_at: std::time::Instant::now(),
                routes_version: None,
            },
        );
        rt.route_counters
            .write()
            .await
            .insert("deleted-model".into(), 42);
        rt.active_targets.write().await.insert(
            "deleted-model".into(),
            sag_state::ActiveTargetEntry::new(
                "p",
                "m",
                "deleted-model",
                Some("m".into()),
                Instant::now(),
            ),
        );

        // Also add valid entries that should survive: one exact-route entry
        // and one wildcard-route entry (the wildcard request key must NOT be
        // pruned even though "gpt-4.1" is not a route model_name).
        rt.fill_first_orders.write().await.insert(
            "valid-model".into(),
            CachedOrder {
                targets: Arc::from([]),
                created_at: std::time::Instant::now(),
                routes_version: None,
            },
        );
        rt.fill_first_orders.write().await.insert(
            "gpt-4.1".into(),
            CachedOrder {
                targets: Arc::from([]),
                created_at: std::time::Instant::now(),
                routes_version: None,
            },
        );
        rt.route_counters
            .write()
            .await
            .insert("valid-model".into(), 1);
        rt.route_counters.write().await.insert("gpt-4.1".into(), 1);
        rt.active_targets.write().await.insert(
            "valid-model".into(),
            sag_state::ActiveTargetEntry::new(
                "p2",
                "m2",
                "valid-model",
                Some("m2".into()),
                Instant::now(),
            ),
        );
        rt.active_targets.write().await.insert(
            "gpt-4.1".into(),
            sag_state::ActiveTargetEntry::new("p3", "gpt-4.1", "*", None, Instant::now()),
        );

        let routes = test_routes();
        rt.prune_stale_entries(&routes).await;

        // "deleted-model" resolves through the wildcard (*) route, so
        // fill_first_orders and route_counters entries (keyed by request
        // model) survive — they will produce a correct shuffle on next use.
        // The active_targets entry is dropped because its route_model
        // "deleted-model" does not match the resolved route "*".
        assert!(
            rt.fill_first_orders
                .read()
                .await
                .contains_key("deleted-model"),
            "fill_first_orders entry for a wildcard-resolving key survives the prune"
        );
        assert!(
            rt.route_counters.read().await.contains_key("deleted-model"),
            "route_counters entry for a wildcard-resolving key survives the prune"
        );
        assert!(
            !rt.active_targets.read().await.contains_key("deleted-model"),
            "active_targets entry with mismatched route_model must be dropped"
        );

        // Valid entries survive — including the wildcard request key.
        assert!(
            rt.fill_first_orders
                .read()
                .await
                .contains_key("valid-model")
        );
        assert!(rt.route_counters.read().await.contains_key("valid-model"));
        assert!(rt.active_targets.read().await.contains_key("valid-model"));
        assert!(
            rt.fill_first_orders.read().await.contains_key("gpt-4.1"),
            "wildcard request key must survive the prune"
        );
        assert!(rt.route_counters.read().await.contains_key("gpt-4.1"));
        assert!(
            rt.active_targets.read().await.contains_key("gpt-4.1"),
            "wildcard active entry must survive the prune"
        );
    }

    #[tokio::test]
    async fn prune_stale_entries_drops_wildcard_key_shadowed_by_exact_route() {
        let rt = DispatchRuntime::default();
        // A wildcard pin for "gpt-4o" (request key, route_model "*").
        rt.active_targets.write().await.insert(
            "gpt-4o".into(),
            sag_state::ActiveTargetEntry::new("p", "gpt-4o", "*", None, Instant::now()),
        );
        rt.fill_first_orders.write().await.insert(
            "gpt-4o".into(),
            CachedOrder {
                targets: Arc::from([]),
                created_at: std::time::Instant::now(),
                routes_version: None,
            },
        );
        rt.route_counters.write().await.insert("gpt-4o".into(), 3);

        // Config now has an exact "gpt-4o" route plus the wildcard: "gpt-4o"
        // resolves to the exact route, so the wildcard-route pin must be
        // dropped (it no longer resolves to its recorded route).
        let routes = vec![
            sag_config::RouteConfig {
                model_name: "gpt-4o".into(),
                api_endpoint: "/chat/completions".into(),
                strategy: sag_config::Strategy::StickyFailover,
                retries: None,
                timeout_secs: None,
                targets: vec![],
            },
            sag_config::RouteConfig {
                model_name: "*".into(),
                api_endpoint: "/chat/completions".into(),
                strategy: sag_config::Strategy::StickyFailover,
                retries: None,
                timeout_secs: None,
                targets: vec![],
            },
        ];
        rt.prune_stale_entries(&routes).await;

        assert!(
            !rt.active_targets.read().await.contains_key("gpt-4o"),
            "wildcard pin must be dropped once an exact route shadows the key"
        );
        // Cache entries keyed by a still-resolving request model survive
        // (they self-heal by re-shuffling on next use).
        assert!(rt.fill_first_orders.read().await.contains_key("gpt-4o"));
        assert!(rt.route_counters.read().await.contains_key("gpt-4o"));
    }

    #[tokio::test]
    async fn prune_stale_entries_drops_active_pin_after_strategy_switch_to_fill_first() {
        let rt = DispatchRuntime::default();
        // A pin recorded while the route was StickyFailover.
        rt.active_targets.write().await.insert(
            "gpt-4o".into(),
            sag_state::ActiveTargetEntry::new(
                "p",
                "gpt-4o",
                "gpt-4o",
                Some("gpt-4o".into()),
                Instant::now(),
            ),
        );
        // The route is now FillFirst — the fill-first order cache and the
        // round-robin counter (both keyed by request model) still resolve
        // and must survive; the sticky pin must be dropped.
        rt.fill_first_orders.write().await.insert(
            "gpt-4o".into(),
            CachedOrder {
                targets: Arc::from([]),
                created_at: std::time::Instant::now(),
                routes_version: None,
            },
        );
        rt.route_counters.write().await.insert("gpt-4o".into(), 2);

        let routes = vec![sag_config::RouteConfig {
            model_name: "gpt-4o".into(),
            api_endpoint: "/chat/completions".into(),
            strategy: sag_config::Strategy::FillFirst,
            retries: None,
            timeout_secs: None,
            targets: vec![],
        }];
        rt.prune_stale_entries(&routes).await;

        assert!(
            !rt.active_targets.read().await.contains_key("gpt-4o"),
            "sticky pin must be pruned once the route switches to fill_first"
        );
        assert!(
            rt.fill_first_orders.read().await.contains_key("gpt-4o"),
            "fill_first_orders entry for a resolving key survives the prune"
        );
        assert!(
            rt.route_counters.read().await.contains_key("gpt-4o"),
            "route_counters entry for a resolving key survives the prune"
        );
    }

    #[tokio::test]
    async fn prune_stale_entries_noop_when_all_valid() {
        let rt = DispatchRuntime::default();
        // An exact-route entry: key "valid-model" matches the route
        // model_name "valid-model" in test_routes().
        rt.fill_first_orders.write().await.insert(
            "valid-model".into(),
            CachedOrder {
                targets: Arc::from([]),
                created_at: std::time::Instant::now(),
                routes_version: None,
            },
        );
        rt.route_counters
            .write()
            .await
            .insert("valid-model".into(), 1);
        rt.active_targets.write().await.insert(
            "valid-model".into(),
            sag_state::ActiveTargetEntry::new(
                "p",
                "rm",
                "valid-model",
                Some("rm".into()),
                Instant::now(),
            ),
        );
        // A wildcard-resolving entry: key "gpt-4.1" resolves to "*" and
        // its route_model is "*".
        rt.fill_first_orders.write().await.insert(
            "gpt-4.1".into(),
            CachedOrder {
                targets: Arc::from([]),
                created_at: std::time::Instant::now(),
                routes_version: None,
            },
        );
        rt.route_counters.write().await.insert("gpt-4.1".into(), 1);
        rt.active_targets.write().await.insert(
            "gpt-4.1".into(),
            sag_state::ActiveTargetEntry::new("p", "gpt-4.1", "*", None, Instant::now()),
        );

        let routes = test_routes();
        rt.prune_stale_entries(&routes).await;

        assert!(
            rt.fill_first_orders
                .read()
                .await
                .contains_key("valid-model")
        );
        assert!(rt.route_counters.read().await.contains_key("valid-model"));
        assert!(rt.active_targets.read().await.contains_key("valid-model"));
        assert!(rt.fill_first_orders.read().await.contains_key("gpt-4.1"));
        assert!(rt.route_counters.read().await.contains_key("gpt-4.1"));
        assert!(rt.active_targets.read().await.contains_key("gpt-4.1"));
    }

    // ── stale_probe_threshold_from_config tests ────────────────────

    use sag_config::{
        BackoffConfig, GatewayConfig, ProviderConfig, ProviderType, ProxyMode, RouteConfig,
        RouteTarget, Strategy,
    };

    /// Minimal provider config with a custom `timeout_secs` and optional ttfb.
    fn prov(timeout_secs: u64, ttfb: Option<u64>) -> ProviderConfig {
        ProviderConfig {
            provider_type: ProviderType::OpenAI,
            api_keys: vec!["sk-test".into()],
            key_strategy: sag_config::KeyStrategy::FillFirst,
            base_url: "https://api.openai.com/v1".into(),
            proxy_mode: ProxyMode::default(),
            models_endpoint: "models".into(),
            timeout_secs,
            headers: HashMap::new(),
            body_overrides: HashMap::new(),
            strip_fields: vec![],
            model_strip_fields: HashMap::new(),
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
