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
    targets: Arc<[RouteTarget]>,
    created_at: Instant,
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
    shuffle_cache_ttl_secs: u64,
) -> Vec<RouteTarget> {
    match strategy {
        Strategy::FillFirst => {
            order_fill_first(
                targets,
                model_name,
                fill_first_orders,
                shuffle_cache_ttl_secs,
            )
            .await
        }
        Strategy::RoundRobin => order_round_robin(targets, model_name, counters).await,
    }
}

/// Returns true when the two target lists have the same set of
/// `(provider, real_model, priority)` tuples — i.e. the
/// ordering-relevant fields are identical so we can reuse a cached
/// shuffled order. Order-agnostic: a passed-in sorted list matched
/// against a previously cached shuffled permutation returns true.
fn targets_match(a: &[RouteTarget], b: &[RouteTarget]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a_keys: Vec<(&str, Option<&str>, u32)> = a
        .iter()
        .map(|t| (t.provider.as_str(), t.real_model.as_deref(), t.priority))
        .collect();
    let mut b_keys: Vec<(&str, Option<&str>, u32)> = b
        .iter()
        .map(|t| (t.provider.as_str(), t.real_model.as_deref(), t.priority))
        .collect();
    a_keys.sort();
    b_keys.sort();
    a_keys == b_keys
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
) -> Vec<RouteTarget> {
    // Stable sort by priority descending so same-priority items are
    // adjacent and in TOML insertion order before shuffling.
    targets.sort_by_key(|t| std::cmp::Reverse(t.priority));

    let now = Instant::now();
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
                extra: HashMap::new(),
            })
            .collect();

        let first = order_fill_first(targets.clone(), "m", &orders, ttl).await;
        let second = order_fill_first(targets.clone(), "m", &orders, ttl).await;
        let third = order_fill_first(targets, "m", &orders, ttl).await;

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
        // Use ttl=1 (1-second expiry) so the cached order is valid
        // only for the immediate next call.
        let ttl = 1;

        // 2 targets: shuffle probability of same order = 1/2.
        let targets: Vec<RouteTarget> = (0..2)
            .map(|i| RouteTarget {
                provider: format!("p{i}"),
                real_model: None,
                priority: 0,
                retries: None,
                extra: HashMap::new(),
            })
            .collect();

        let first = order_fill_first(targets.clone(), "m", &orders, ttl).await;
        // Cache is fresh — next call returns same order.
        let second = order_fill_first(targets.clone(), "m", &orders, ttl).await;
        assert_eq!(first, second, "same order while cache is fresh");

        // Wait for TTL to expire. Sleep a bit longer than 1 s to
        // account for timer resolution — `as_secs()` truncates,
        // so we need age_secs ≥ 1.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        // After expiry, we may or may not get the same order.
        // With 2 items, P[different] = 1/2 per call.
        // 10 calls ⇒ (1/2)^10 ≈ 1e-3 chance of all-same.
        let mut any_different = false;
        for _ in 0..10 {
            let next = order_fill_first(targets.clone(), "m", &orders, ttl).await;
            if next != first {
                any_different = true;
                break;
            }
        }
        assert!(any_different, "ttl=1 must cause re-shuffle after expiry");
    }

    #[tokio::test]
    async fn fill_first_preserves_priority_groups() {
        // Priorities 10 → shuffle among the two, then 1.
        let orders = orders_map();
        let ttl = 7200;
        let targets = vec![target("low", 1), target("high-b", 10), target("high-a", 10)];
        let ordered = order_fill_first(targets, "m", &orders, ttl).await;

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

        let a1 = order_fill_first(targets_a.clone(), "m", &orders, ttl).await;
        let a2 = order_fill_first(targets_a.clone(), "m", &orders, ttl).await;
        assert_eq!(a1, a2, "same targets → cached");

        // Targets changed (added "c") → must re-shuffle
        let b1 = order_fill_first(targets_b.clone(), "m", &orders, ttl).await;
        assert_eq!(b1.len(), 3, "new target list includes c");
        let b2 = order_fill_first(targets_b, "m", &orders, ttl).await;
        assert_eq!(b1, b2, "cached after re-shuffle");
    }

    #[tokio::test]
    async fn round_robin_rotates_by_counter() {
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let orders = orders_map();
        let ttl = 7200;
        let targets = vec![target("a", 0), target("b", 0), target("c", 0)];

        // First call: starts at 0
        let r1 = order_targets(
            targets.clone(),
            "model",
            Strategy::RoundRobin,
            &counters,
            &orders,
            ttl,
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
            ttl,
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
            ttl,
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
            ttl,
        )
        .await;
        assert_eq!(r4[0].provider, "a");
    }

    #[tokio::test]
    async fn round_robin_keys_are_per_model() {
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let orders = orders_map();
        let ttl = 7200;
        let targets = vec![target("a", 0), target("b", 0)];

        let r1 = order_targets(
            targets.clone(),
            "model-a",
            Strategy::RoundRobin,
            &counters,
            &orders,
            ttl,
        )
        .await;
        let r2 = order_targets(
            targets.clone(),
            "model-b",
            Strategy::RoundRobin,
            &counters,
            &orders,
            ttl,
        )
        .await;

        // Each model has its own counter, so both should start at 0
        assert_eq!(r1[0].provider, "a");
        assert_eq!(r2[0].provider, "a");
    }
}
