use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use sag_config::{RouteTarget, Strategy};
use tokio::sync::RwLock;

/// A cached shuffled target order with its creation timestamp.
/// Uses `Arc` so cache hits avoid a per-request deep clone of the
/// full target vector (including per-target `HashMap` extra fields).
#[derive(Debug, Clone)]
pub struct CachedOrder {
    // `pub(crate)` so sibling module tests can construct test instances.
    pub(crate) targets: Arc<[RouteTarget]>,
    pub(crate) created_at: Instant,
}

/// Order the target list according to the route's strategy.
///
/// `FillFirst` shuffles same-priority targets once on first access, then
/// caches the order so every subsequent request follows the same sequence.
/// The cache expires after `shuffle_cache_ttl_secs` (default 2 hours),
/// after which the targets are re-shuffled. The cache is also invalidated
/// when the targets change (config reload).
///
/// `RoundRobin` rotates the list so each request starts at a different
/// position, persisting the per-model counter across calls.
pub async fn order_targets(
    targets: Vec<RouteTarget>,
    model_name: &str,
    strategy: Strategy,
    counters: &Arc<RwLock<HashMap<String, u64>>>,
    fill_first_orders: &Arc<RwLock<HashMap<String, CachedOrder>>>,
    active_targets: &Arc<RwLock<HashMap<String, (String, String, Instant)>>>,
    shuffle_cache_ttl_secs: u64,
    sticky_failover_reset_secs: u64,
) -> Vec<RouteTarget> {
    match strategy {
        Strategy::StickyFailover => {
            let targets_changed = {
                let guard = fill_first_orders.read().await;
                if let Some(cached) = guard.get(model_name) {
                    !targets_match(&targets, &cached.targets)
                } else {
                    false
                }
            };
            if targets_changed {
                let mut active = active_targets.write().await;
                active.remove(model_name);
            }

            let mut shuffled = order_fill_first(
                targets,
                model_name,
                fill_first_orders,
                shuffle_cache_ttl_secs,
                Instant::now(),
            )
            .await;

            let active = active_targets.read().await;
            if let Some((active_provider, active_real_model, created_at)) = active.get(model_name) {
                let expired = sticky_failover_reset_secs > 0
                    && created_at.elapsed().as_secs() >= sticky_failover_reset_secs;
                if expired {
                    // 过期：不做任何事，自然 fall through 到 fill-first 顺序。
                    // 不在此处删除 entry——避免 read-then-write 竞态；
                    // stale entry 下次成功调度时会被 insert 覆盖。
                    tracing::debug!(
                        model = %model_name,
                        sticky_failover_reset_secs,
                        "sticky-failover active target expired, using fill-first order"
                    );
                } else if let Some(idx) = shuffled.iter().position(|t| {
                    t.provider == *active_provider
                        && t.real_model.as_deref().unwrap_or(model_name) == *active_real_model
                }) {
                    let active_target = shuffled.remove(idx);
                    shuffled.insert(0, active_target);
                }
            }
            shuffled
        }
        Strategy::FillFirst => {
            order_fill_first(
                targets,
                model_name,
                fill_first_orders,
                shuffle_cache_ttl_secs,
                Instant::now(),
            )
            .await
        }
        Strategy::RoundRobin => order_round_robin(targets, model_name, counters).await,
    }
}

/// Returns true when the two target lists are semantically identical
/// (same set of fully-populated `RouteTarget`s). Order-agnostic: a
/// passed-in sorted list matched against a previously cached shuffled
/// permutation returns true.
///
/// All `RouteTarget` fields — including `extra` and `retries` — are
/// compared so that config hot-reloads changing any field invalidate
/// the cached shuffle order.
fn targets_match(a: &[RouteTarget], b: &[RouteTarget]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a_sorted: Vec<&RouteTarget> = a.iter().collect();
    let mut b_sorted: Vec<&RouteTarget> = b.iter().collect();
    a_sorted.sort_by(|x, y| {
        (&x.provider, &x.real_model, x.priority).cmp(&(&y.provider, &y.real_model, y.priority))
    });
    b_sorted.sort_by(|x, y| {
        (&x.provider, &x.real_model, x.priority).cmp(&(&y.provider, &y.real_model, y.priority))
    });
    a_sorted == b_sorted
}

/// Shuffle same-priority targets once and cache the result.
///
/// - First access: shuffle and cache with current timestamp.
/// - Subsequent access with same targets: return cached order.
/// - If TTL (`shuffle_cache_ttl_secs`) has elapsed since last shuffle:
///   re-shuffle and update timestamp.
/// - If targets changed (config reload): re-shuffle regardless of TTL.
///
/// This gives a fair initial distribution while keeping the "fill first"
/// stickiness: the first target in the shuffled order absorbs all traffic
/// until it becomes unavailable (disabled, circuit-breaker open, or retries
/// exhausted), at which point the next target takes over.
async fn order_fill_first(
    mut targets: Vec<RouteTarget>,
    model_name: &str,
    orders: &Arc<RwLock<HashMap<String, CachedOrder>>>,
    ttl_secs: u64,
    now: Instant,
) -> Vec<RouteTarget> {
    // Stable sort by priority descending so same-priority items are
    // adjacent and in TOML insertion order before shuffling.
    targets.sort_by_key(|t| std::cmp::Reverse(t.priority));
    let mut guard = orders.write().await;

    // Reuse cached order when targets haven't changed AND TTL hasn't expired.
    if let Some(cached) = guard.get(model_name)
        && targets_match(&targets, &cached.targets)
    {
        let age_secs = now.duration_since(cached.created_at).as_secs();
        // ttl_secs == 0 → never expire (cache lasts until config change).
        // ttl_secs > 0 → expire after that many seconds.
        if ttl_secs == 0 || age_secs < ttl_secs {
            return cached.targets.to_vec();
        }
        // TTL expired — fall through to re-shuffle.
        tracing::info!(
            model = %model_name,
            age_secs,
            ttl_secs,
            "fill-first shuffle cache expired, re-shuffling"
        );
    }

    // First access, targets changed, or TTL expired: shuffle within each
    // priority group.
    let mut i = 0;
    while i < targets.len() {
        let prio = targets[i].priority;
        let mut j = i + 1;
        while j < targets.len() && targets[j].priority == prio {
            j += 1;
        }
        fastrand::shuffle(&mut targets[i..j]);
        i = j;
    }

    guard.insert(
        model_name.to_string(),
        CachedOrder {
            targets: Arc::from(targets.clone()),
            created_at: now,
        },
    );
    targets
}

/// Rotate the list so each request starts at a different position,
/// using a shared counter keyed by model name.
async fn order_round_robin(
    targets: Vec<RouteTarget>,
    model_name: &str,
    counters: &Arc<RwLock<HashMap<String, u64>>>,
) -> Vec<RouteTarget> {
    let mut guard = counters.write().await;
    let counter = guard.entry(model_name.to_string()).or_insert(0);
    let len = targets.len();
    let start = (*counter % len as u64) as usize;
    *counter += 1;
    let mut rotated = targets;
    rotated.rotate_left(start);
    rotated
}

#[cfg(test)]
mod tests {
    use super::*;
    use sag_config::RouteTarget;
    use std::collections::HashMap;

    fn target(provider: &str, priority: u32) -> RouteTarget {
        RouteTarget {
            provider: provider.into(),
            real_model: None,
            priority,
            retries: None,
            timeout_secs: None,
            extra: HashMap::new(),
        }
    }

    fn orders_map() -> Arc<RwLock<HashMap<String, CachedOrder>>> {
        Arc::new(RwLock::new(HashMap::new()))
    }

    #[tokio::test]
    async fn fill_first_shuffles_once_then_caches() {
        let orders = orders_map();
        let ttl = 7200; // long TTL for test
        // 10 same-priority targets — shuffle must produce a real
        // permutation (not TOML order) on first call, and the same
        // order on subsequent calls.
        let targets: Vec<RouteTarget> = (0..10)
            .map(|i| RouteTarget {
                provider: format!("p{i}"),
                real_model: None,
                priority: 0,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
            })
            .collect();

        let first = order_fill_first(targets.clone(), "m", &orders, ttl, Instant::now()).await;
        let second = order_fill_first(targets.clone(), "m", &orders, ttl, Instant::now()).await;
        let third = order_fill_first(targets, "m", &orders, ttl, Instant::now()).await;

        assert_eq!(first, second, "cached order must be stable");
        assert_eq!(second, third, "cached order must be stable");

        // With 10 items the chance of TOML order (1/10! ≈ 2.7e-7) is
        // negligible — assert the order is actually shuffled.
        let is_toml_order = first
            .iter()
            .enumerate()
            .all(|(i, t)| t.provider == format!("p{i}"));
        assert!(!is_toml_order, "shuffle must reorder same-priority targets");
    }

    #[tokio::test]
    async fn fill_first_re_shuffles_when_ttl_expires() {
        let orders = orders_map();
        let ttl = 1;

        // 2 targets: shuffle probability of same order = 1/2.
        let targets: Vec<RouteTarget> = (0..2)
            .map(|i| RouteTarget {
                provider: format!("p{i}"),
                real_model: None,
                priority: 0,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
            })
            .collect();

        let t0 = Instant::now();
        let first = order_fill_first(targets.clone(), "m", &orders, ttl, t0).await;
        // Cache is fresh — next call returns same order.
        let second = order_fill_first(targets.clone(), "m", &orders, ttl, t0).await;
        assert_eq!(first, second, "same order while cache is fresh");

        // Advance time past TTL — the first call must expire and update
        // the cache timestamp. This directly proves the TTL expiry path
        // executed, without relying on shuffle probability.
        let t1 = t0 + std::time::Duration::from_secs(2);
        let _next = order_fill_first(targets.clone(), "m", &orders, ttl, t1).await;
        let guard = orders.read().await;
        let cached = guard.get("m").expect("cache entry must exist");
        assert!(
            cached.created_at > t0,
            "cache must be refreshed after TTL expiry"
        );
    }

    #[tokio::test]
    async fn fill_first_preserves_priority_groups() {
        // Priorities 10 → shuffle among the two, then 1.
        let orders = orders_map();
        let ttl = 7200;
        let targets = vec![target("low", 1), target("high-b", 10), target("high-a", 10)];
        let ordered = order_fill_first(targets, "m", &orders, ttl, Instant::now()).await;

        let priorities: Vec<u32> = ordered.iter().map(|t| t.priority).collect();
        assert_eq!(&priorities[..2], &[10, 10], "first two must be priority 10");
        assert_eq!(priorities[2], 1, "last must be priority 1");
    }

    #[tokio::test]
    async fn fill_first_re_shuffles_when_targets_change() {
        let orders = orders_map();
        let ttl = 7200;
        let targets_a: Vec<RouteTarget> = vec![target("a", 0), target("b", 0)];
        let targets_b: Vec<RouteTarget> = vec![target("a", 0), target("b", 0), target("c", 0)];

        let a1 = order_fill_first(targets_a.clone(), "m", &orders, ttl, Instant::now()).await;
        let a2 = order_fill_first(targets_a.clone(), "m", &orders, ttl, Instant::now()).await;
        assert_eq!(a1, a2, "same targets → cached");

        // Targets changed (added "c") → must re-shuffle
        let b1 = order_fill_first(targets_b.clone(), "m", &orders, ttl, Instant::now()).await;
        assert_eq!(b1.len(), 3, "new target list includes c");
        let b2 = order_fill_first(targets_b, "m", &orders, ttl, Instant::now()).await;
        assert_eq!(b1, b2, "cached after re-shuffle");
    }

    #[tokio::test]
    async fn fill_first_re_shuffles_when_extra_changes() {
        let orders = orders_map();
        let ttl = 7200;
        let targets_a: Vec<RouteTarget> = vec![RouteTarget {
            provider: "a".into(),
            real_model: None,
            priority: 0,
            retries: None,
            timeout_secs: None,
            extra: HashMap::from([("temperature".into(), serde_json::json!(0.7))]),
        }];
        let targets_b: Vec<RouteTarget> = vec![RouteTarget {
            provider: "a".into(),
            real_model: None,
            priority: 0,
            retries: None,
            timeout_secs: None,
            extra: HashMap::from([("temperature".into(), serde_json::json!(0.9))]),
        }];

        let a1 = order_fill_first(targets_a.clone(), "m", &orders, ttl, Instant::now()).await;
        let a2 = order_fill_first(targets_a.clone(), "m", &orders, ttl, Instant::now()).await;
        assert_eq!(a1, a2, "same targets → cached");

        // Extra changed → must re-shuffle even though provider/real_model/priority are identical
        let b1 = order_fill_first(targets_b.clone(), "m", &orders, ttl, Instant::now()).await;
        assert_eq!(
            b1[0].extra.get("temperature").unwrap(),
            &serde_json::json!(0.9)
        );
        let b2 = order_fill_first(targets_b, "m", &orders, ttl, Instant::now()).await;
        assert_eq!(b1, b2, "cached after re-shuffle");
    }

    #[tokio::test]
    async fn fill_first_re_shuffles_when_retries_changes() {
        let orders = orders_map();
        let ttl = 7200;
        let targets_a: Vec<RouteTarget> = vec![RouteTarget {
            provider: "a".into(),
            real_model: None,
            priority: 0,
            retries: Some(1),
            timeout_secs: None,
            extra: HashMap::new(),
        }];
        let targets_b: Vec<RouteTarget> = vec![RouteTarget {
            provider: "a".into(),
            real_model: None,
            priority: 0,
            retries: Some(3),
            timeout_secs: None,
            extra: HashMap::new(),
        }];

        let a1 = order_fill_first(targets_a.clone(), "m", &orders, ttl, Instant::now()).await;
        let a2 = order_fill_first(targets_a.clone(), "m", &orders, ttl, Instant::now()).await;
        assert_eq!(a1, a2, "same targets → cached");

        // Retries changed → must re-shuffle even though provider/real_model/priority are identical
        let b1 = order_fill_first(targets_b.clone(), "m", &orders, ttl, Instant::now()).await;
        assert_eq!(b1[0].retries, Some(3));
        let b2 = order_fill_first(targets_b, "m", &orders, ttl, Instant::now()).await;
        assert_eq!(b1, b2, "cached after re-shuffle");
    }

    fn active_map() -> Arc<RwLock<HashMap<String, (String, String, Instant)>>> {
        Arc::new(RwLock::new(HashMap::new()))
    }

    #[tokio::test]
    async fn round_robin_rotates_by_counter() {
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let orders = orders_map();
        let active = active_map();
        let ttl = 7200;
        let targets = vec![target("a", 0), target("b", 0), target("c", 0)];

        // First call: starts at 0
        let r1 = order_targets(
            targets.clone(),
            "model",
            Strategy::RoundRobin,
            &counters,
            &orders,
            &active,
            ttl,
            0,
        )
        .await;
        assert_eq!(r1[0].provider, "a");

        // Second call: starts at 1
        let r2 = order_targets(
            targets.clone(),
            "model",
            Strategy::RoundRobin,
            &counters,
            &orders,
            &active,
            ttl,
            0,
        )
        .await;
        assert_eq!(r2[0].provider, "b");

        // Third call: starts at 2
        let r3 = order_targets(
            targets.clone(),
            "model",
            Strategy::RoundRobin,
            &counters,
            &orders,
            &active,
            ttl,
            0,
        )
        .await;
        assert_eq!(r3[0].provider, "c");

        // Fourth call: wraps back to 0
        let r4 = order_targets(
            targets,
            "model",
            Strategy::RoundRobin,
            &counters,
            &orders,
            &active,
            ttl,
            0,
        )
        .await;
        assert_eq!(r4[0].provider, "a");
    }

    #[tokio::test]
    async fn round_robin_keys_are_per_model() {
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let orders = orders_map();
        let active = active_map();
        let ttl = 7200;
        let targets = vec![target("a", 0), target("b", 0)];

        let r1 = order_targets(
            targets.clone(),
            "model-a",
            Strategy::RoundRobin,
            &counters,
            &orders,
            &active,
            ttl,
            0,
        )
        .await;
        let r2 = order_targets(
            targets.clone(),
            "model-b",
            Strategy::RoundRobin,
            &counters,
            &orders,
            &active,
            ttl,
            0,
        )
        .await;

        // Each model has its own counter, so both should start at 0
        assert_eq!(r1[0].provider, "a");
        assert_eq!(r2[0].provider, "a");
    }

    #[tokio::test]
    async fn sticky_failover_prepends_active_target() {
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let orders = orders_map();
        let active = active_map();
        let ttl = 7200;
        let targets = vec![target("a", 0), target("b", 0), target("c", 0)];

        // Record "b" as the active target
        {
            let mut guard = active.write().await;
            guard.insert(
                "model".to_string(),
                ("b".to_string(), "model".to_string(), Instant::now()),
            );
        }

        let ordered = order_targets(
            targets,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0,
        )
        .await;

        // "b" must be prepended to the front (index 0)
        assert_eq!(ordered[0].provider, "b");
        // Remaining targets are preserved
        let remaining: Vec<String> = ordered[1..].iter().map(|t| t.provider.clone()).collect();
        assert!(remaining.contains(&"a".to_string()));
        assert!(remaining.contains(&"c".to_string()));
        assert_eq!(remaining.len(), 2);
    }

    #[tokio::test]
    async fn sticky_failover_clears_active_on_target_change() {
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let orders = orders_map();
        let active = active_map();
        let ttl = 7200;
        let targets_a = vec![target("a", 0), target("b", 0)];
        let targets_b = vec![target("a", 0), target("b", 0), target("c", 0)];

        // First run with targets_a to initialize cached order
        let _ = order_targets(
            targets_a.clone(),
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0,
        )
        .await;

        // Record "b" as the active target
        {
            let mut guard = active.write().await;
            guard.insert(
                "model".to_string(),
                ("b".to_string(), "model".to_string(), Instant::now()),
            );
        }

        // Run again with targets_b (which is different)
        let ordered = order_targets(
            targets_b,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0,
        )
        .await;

        // Since targets changed, active target must have been cleared from active_targets
        {
            let guard = active.read().await;
            assert!(
                guard.get("model").is_none(),
                "active target must be cleared on config change"
            );
        }

        // And the first element should be whatever fill_first decided, not necessarily "b"
        // (no prepending happens).
        assert_eq!(ordered.len(), 3);
    }

    #[tokio::test]
    async fn sticky_failover_clears_active_when_extra_changes() {
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let orders = orders_map();
        let active = active_map();
        let ttl = 7200;
        let targets_a = vec![
            RouteTarget {
                provider: "a".into(),
                real_model: None,
                priority: 0,
                retries: None,
                timeout_secs: None,
                extra: HashMap::from([("temperature".into(), serde_json::json!(0.7))]),
            },
            RouteTarget {
                provider: "b".into(),
                real_model: None,
                priority: 0,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
            },
        ];
        let targets_b = vec![
            RouteTarget {
                provider: "a".into(),
                real_model: None,
                priority: 0,
                retries: None,
                timeout_secs: None,
                extra: HashMap::from([("temperature".into(), serde_json::json!(0.9))]),
            },
            RouteTarget {
                provider: "b".into(),
                real_model: None,
                priority: 0,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
            },
        ];

        // First run with targets_a to initialize cached order
        let _ = order_targets(
            targets_a.clone(),
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0,
        )
        .await;

        // Record "b" as the active target
        {
            let mut guard = active.write().await;
            guard.insert(
                "model".to_string(),
                ("b".to_string(), "model".to_string(), Instant::now()),
            );
        }

        // Run again with targets_b (extra changed but provider/real_model/priority identical)
        let ordered = order_targets(
            targets_b,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0,
        )
        .await;

        // Active target must have been cleared because targets_match now detects extra changes
        {
            let guard = active.read().await;
            assert!(
                guard.get("model").is_none(),
                "active target must be cleared when extra changes"
            );
        }

        // The returned targets must carry the new extra value
        let a_target = ordered.iter().find(|t| t.provider == "a").unwrap();
        assert_eq!(
            a_target.extra.get("temperature").unwrap(),
            &serde_json::json!(0.9)
        );
    }

    #[tokio::test]
    async fn sticky_failover_clears_active_when_retries_changes() {
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let orders = orders_map();
        let active = active_map();
        let ttl = 7200;
        let targets_a = vec![
            RouteTarget {
                provider: "a".into(),
                real_model: None,
                priority: 0,
                retries: Some(1),
                timeout_secs: None,
                extra: HashMap::new(),
            },
            RouteTarget {
                provider: "b".into(),
                real_model: None,
                priority: 0,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
            },
        ];
        let targets_b = vec![
            RouteTarget {
                provider: "a".into(),
                real_model: None,
                priority: 0,
                retries: Some(5),
                timeout_secs: None,
                extra: HashMap::new(),
            },
            RouteTarget {
                provider: "b".into(),
                real_model: None,
                priority: 0,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
            },
        ];

        // First run with targets_a to initialize cached order
        let _ = order_targets(
            targets_a.clone(),
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0,
        )
        .await;

        // Record "b" as the active target
        {
            let mut guard = active.write().await;
            guard.insert(
                "model".to_string(),
                ("b".to_string(), "model".to_string(), Instant::now()),
            );
        }

        // Run again with targets_b (retries changed but provider/real_model/priority identical)
        let ordered = order_targets(
            targets_b,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0,
        )
        .await;

        // Active target must have been cleared because targets_match now detects retries changes
        {
            let guard = active.read().await;
            assert!(
                guard.get("model").is_none(),
                "active target must be cleared when retries changes"
            );
        }

        // The returned targets must carry the new retries value
        let a_target = ordered.iter().find(|t| t.provider == "a").unwrap();
        assert_eq!(a_target.retries, Some(5));
    }

    #[tokio::test]
    async fn sticky_failover_resets_active_after_ttl() {
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let orders = orders_map();
        let active = active_map();
        let targets = vec![target("a", 0), target("b", 0), target("c", 0)];

        // Baseline: no active entry -> primes the fill-first cache with a
        // fixed shuffled order.
        let baseline = order_targets(
            targets.clone(),
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            7200,
            3600, // reset window, irrelevant: no active entry exists
        )
        .await;

        // Insert an EXPIRED active entry pointing at "b".
        {
            let mut guard = active.write().await;
            guard.insert(
                "model".to_string(),
                (
                    "b".to_string(),
                    "model".to_string(),
                    Instant::now() - std::time::Duration::from_secs(7200),
                ),
            );
        }

        // Re-run: cache is already primed (same shuffle), only difference
        // is the expired active entry.
        let ordered = order_targets(
            targets,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            7200,
            3600, // entry is 7200s old, window is 3600s -> expired
        )
        .await;

        // Expired entry must NOT alter the order: result equals the
        // no-active-entry baseline (same fill-first cache).
        assert_eq!(
            ordered, baseline,
            "expired active target must not prepend; result must equal fill-first baseline"
        );
    }

    #[tokio::test]
    async fn sticky_failover_preserves_non_expired() {
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let orders = orders_map();
        let active = active_map();
        let ttl = 7200;
        let targets = vec![target("a", 0), target("b", 0), target("c", 0)];

        // Record "b" as the active target, freshly created.
        {
            let mut guard = active.write().await;
            guard.insert(
                "model".to_string(),
                ("b".to_string(), "model".to_string(), Instant::now()),
            );
        }

        let ordered = order_targets(
            targets,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            3600, // reset window: 3600s - the fresh entry is not expired
        )
        .await;

        // "b" must be prepended to the front (index 0).
        assert_eq!(ordered[0].provider, "b");
        let remaining: Vec<String> = ordered[1..].iter().map(|t| t.provider.clone()).collect();
        assert!(remaining.contains(&"a".to_string()));
        assert!(remaining.contains(&"c".to_string()));
        assert_eq!(remaining.len(), 2);
    }

    #[tokio::test]
    async fn sticky_failover_never_resets_with_zero_ttl() {
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let orders = orders_map();
        let active = active_map();
        let ttl = 7200;
        let targets = vec![target("a", 0), target("b", 0), target("c", 0)];

        // Record "b" as the active target, but with an expired timestamp.
        {
            let mut guard = active.write().await;
            guard.insert(
                "model".to_string(),
                (
                    "b".to_string(),
                    "model".to_string(),
                    Instant::now() - std::time::Duration::from_secs(7200),
                ),
            );
        }

        let ordered = order_targets(
            targets,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0, // reset window: 0 -> never expire
        )
        .await;

        // Even though the entry is old, ttl=0 means never reset - "b" must
        // still be prepended.
        assert_eq!(ordered[0].provider, "b");
    }
}
