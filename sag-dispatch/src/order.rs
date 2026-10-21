use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use sag_config::{RouteTarget, Strategy};
use sag_state::CachedOrder;
use tokio::sync::RwLock;

// The cached shuffled-order type itself lives in `sag-state` (as
// [`sag_state::CachedOrder`]) so the admin layer can observe and reset the
// fill-first order through the shared `AppState`; this module re-uses it.

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
///
/// Returns the ordered targets plus the sticky-failover token captured at
/// the decision's linearization point (the second element carries the same
/// two-component token for every strategy; only `StickyFailover` consumes
/// it). The dispatch success path hands that token back to `active_targets`
/// so a clear (DELETE / breaker close / targets-changed / prune) or config
/// mutation that happened after this decision can never be overwritten by a
/// stale pin.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub async fn order_targets(
    targets: Vec<RouteTarget>,
    model_name: &str,
    strategy: Strategy,
    counters: &Arc<RwLock<HashMap<String, u64>>>,
    fill_first_orders: &Arc<RwLock<HashMap<String, CachedOrder>>>,
    active_targets: &sag_state::ActiveTargets,
    shuffle_cache_ttl_secs: u64,
    sticky_failover_reset_secs: u64,
    routes_version: u64,
) -> (Vec<RouteTarget>, sag_state::StickyToken) {
    match strategy {
        Strategy::StickyFailover => {
            order_sticky_failover(
                targets,
                model_name,
                fill_first_orders,
                active_targets,
                shuffle_cache_ttl_secs,
                sticky_failover_reset_secs,
                routes_version,
            )
            .await
        }
        Strategy::FillFirst => {
            let ordered = order_fill_first(
                targets,
                model_name,
                fill_first_orders,
                shuffle_cache_ttl_secs,
                Instant::now(),
                routes_version,
            )
            .await;
            // Epoch 0: fill-first never writes back to `active_targets`, so
            // the token's epoch/generation components are inert.
            (ordered, sag_state::StickyToken::new(0, 0, routes_version))
        }
        Strategy::RoundRobin => {
            let ordered = order_round_robin(targets, model_name, counters).await;
            (ordered, sag_state::StickyToken::new(0, 0, routes_version))
        }
        Strategy::SessionSticky => {
            // TODO(P3): replaced by the real session-sticky branch
            // (session pin hoist + weighted-random permutation) once the
            // `SessionAffinity` state lands. Until then this arm only keeps
            // the workspace compiling; the new enum value is inert.
            order_sticky_failover(
                targets,
                model_name,
                fill_first_orders,
                active_targets,
                shuffle_cache_ttl_secs,
                sticky_failover_reset_secs,
                routes_version,
            )
            .await
        }
    }
}

/// Sticky-failover branch of [`order_targets`]: clear a stale shared
/// order/pin when the route's targets changed, capture the sticky
/// decision state at its linearization point, shuffle (fill-first), then
/// hoist the pinned target to the front when it is still present and
/// unexpired. Returns the ordered targets plus the [`StickyToken`]
/// captured at the decision point (see [`order_targets`] for what each
/// component blocks).
#[allow(clippy::too_many_arguments)]
async fn order_sticky_failover(
    targets: Vec<RouteTarget>,
    model_name: &str,
    fill_first_orders: &Arc<RwLock<HashMap<String, CachedOrder>>>,
    active_targets: &sag_state::ActiveTargets,
    shuffle_cache_ttl_secs: u64,
    sticky_failover_reset_secs: u64,
    routes_version: u64,
) -> (Vec<RouteTarget>, sag_state::StickyToken) {
    clear_stale_order_if_targets_changed(
        &targets,
        model_name,
        fill_first_orders,
        active_targets,
        routes_version,
    )
    .await;

    // Capture the sticky decision state (entry + generation + epoch)
    // at the linearization point — *before* the async shuffle
    // below. A clear (DELETE / breaker close / targets-changed /
    // prune) that lands during `order_fill_first` must fail the
    // eventual writeback, so the token has to be captured here,
    // never after the await. The snapshot doubles as the decision
    // input, so the returned token always represents the state this
    // order used.
    let sticky = {
        let active = active_targets.read().await;
        (
            active.get(model_name).cloned(),
            active.generation(model_name),
            active.epoch(),
        )
    };

    let mut shuffled = order_fill_first(
        targets,
        model_name,
        fill_first_orders,
        shuffle_cache_ttl_secs,
        Instant::now(),
        routes_version,
    )
    .await;

    if let Some(entry) = &sticky.0 {
        prepend_sticky_target(&mut shuffled, entry, model_name, sticky_failover_reset_secs);
    }
    (
        shuffled,
        sag_state::StickyToken::new(sticky.2, sticky.1, routes_version),
    )
}

/// When the route's targets changed since the cached shuffle was built
/// (config reload), clear the shared order — and with it the active pin
/// — but only when this request is at least as new as the cache entry.
///
/// The version check must hold *together with* the clear decision: only a
/// request at least as new as the cached entry may invalidate the shared
/// order — and with it the active pin a newer config may have recorded.
/// The cache is re-checked under the active write lock so a cache entry
/// written (and pin recorded) between the read above and this clear can
/// never be clobbered by a stale request. A stale request
/// (routes_version < cache version) falls through without clearing: the
/// shared cache/active state belongs to the newer config.
async fn clear_stale_order_if_targets_changed(
    targets: &[RouteTarget],
    model_name: &str,
    fill_first_orders: &Arc<RwLock<HashMap<String, CachedOrder>>>,
    active_targets: &sag_state::ActiveTargets,
    routes_version: u64,
) {
    let targets_changed = {
        let guard = fill_first_orders.read().await;
        if let Some(cached) = guard.get(model_name) {
            !targets_match(targets, &cached.targets)
        } else {
            false
        }
    };
    if !targets_changed {
        return;
    }
    let mut active = active_targets.write().await;
    let clear_gate = fill_first_orders
        .read()
        .await
        .get(model_name)
        .is_some_and(|cached| routes_version >= cached.routes_version.unwrap_or(0));
    if clear_gate {
        active.clear(model_name);
    }
}

/// Hoist the sticky-pinned target to the front of the shuffled order,
/// when the pin is present in `shuffled` and not expired. An expired or
/// absent pin falls through to the fill-first order.
///
/// Target identity is provider + the target's *original* `real_model`
/// spec, not the effective real model: a passthrough target (`None`) and
/// an explicit target (`Some(rm)`) on the same provider can both resolve
/// to the same effective real model, and must not be treated as the same
/// sticky target. The effective-model comparison stays as a cross-check.
///
/// Expiry: with `sticky_failover_reset_secs > 0` and the entry older
/// than that window, do nothing (fall through to fill-first order). The
/// stale entry is *not* deleted here — that would be a read-then-write
/// race; the next successful schedule overwrites it via `insert`.
fn prepend_sticky_target(
    shuffled: &mut Vec<RouteTarget>,
    entry: &sag_state::ActiveTargetEntry,
    model_name: &str,
    sticky_failover_reset_secs: u64,
) {
    let expired = sticky_failover_reset_secs > 0
        && entry.recorded_at.elapsed().as_secs() >= sticky_failover_reset_secs;
    if expired {
        // 过期：不做任何事，自然 fall through 到 fill-first 顺序。
        // 不在此处删除 entry——避免 read-then-write 竞态；
        // stale entry 下次成功调度时会被 insert 覆盖。
        tracing::debug!(
            model = %model_name,
            sticky_failover_reset_secs,
            "sticky-failover active target expired, using fill-first order"
        );
        return;
    }
    if let Some(idx) = shuffled.iter().position(|t| {
        t.provider == entry.provider
            && t.real_model == entry.target_real_model
            && t.real_model.as_deref().unwrap_or(model_name) == entry.real_model
    }) {
        let active_target = shuffled.remove(idx);
        shuffled.insert(0, active_target);
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
/// - First access: shuffle and cache with current timestamp and the
///   `routes_version` that produced this resolve.
/// - Subsequent access with same targets and an at-least-as-new
///   `routes_version`: return cached order.
/// - If TTL (`shuffle_cache_ttl_secs`) has elapsed since last shuffle:
///   re-shuffle and update timestamp.
/// - If targets changed (config reload): re-shuffle regardless of TTL.
/// - If this request's `routes_version` is **older** than the cached
///   entry's: the request is stale — it sorted against an older config. It
///   shuffles locally and returns without touching the shared cache, so a
///   stale request can never clobber an order established by a newer
///   config (the cache pollution half of the stale-request bug).
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
    routes_version: u64,
) -> Vec<RouteTarget> {
    // Stable sort by priority descending so same-priority items are
    // adjacent and in TOML insertion order before shuffling.
    targets.sort_by_key(|t| std::cmp::Reverse(t.priority));

    // Fast path: reuse a cached order when this request is at least as new
    // as the cached entry (`None` = legacy entry, treated as version 0) and
    // the targets still match and the TTL hasn't expired. Reads under a
    // read lock so cache hits never serialize on the write lock.
    {
        let guard = orders.read().await;
        if let Some(cached) = guard.get(model_name)
            && routes_version >= cached.routes_version.unwrap_or(0)
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
    }

    // First access, targets changed, TTL expired, or stale request: shuffle
    // within each priority group.
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

    // Write-back path. Re-checks the cache under the write lock:
    // - a concurrent request may have just populated a matching,
    //   at-least-as-new entry — reuse it, preserving "shuffled once";
    // - a newer entry (higher routes_version) means this request is stale —
    //   its local shuffle is returned without touching the shared cache.
    // `CachedOrder` is constructed with public fields now that the type
    // lives in `sag-state`.
    let mut guard = orders.write().await;
    match guard.get(model_name) {
        // Stale: the shared cache belongs to a newer config version.
        Some(cached) if routes_version < cached.routes_version.unwrap_or(0) => {}
        Some(cached) => {
            if targets_match(&targets, &cached.targets)
                && (ttl_secs == 0 || now.duration_since(cached.created_at).as_secs() < ttl_secs)
            {
                return cached.targets.to_vec();
            }
            guard.insert(
                model_name.to_string(),
                CachedOrder {
                    targets: Arc::from(targets.clone()),
                    created_at: now,
                    routes_version: Some(routes_version),
                },
            );
        }
        None => {
            guard.insert(
                model_name.to_string(),
                CachedOrder {
                    targets: Arc::from(targets.clone()),
                    created_at: now,
                    routes_version: Some(routes_version),
                },
            );
        }
    }
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

/// Effective weight for the session-sticky weighted base ordering:
/// `priority` doubles as the weight (no separate `weight` field — design
/// decision D3). Priority layers do not apply in the weighted base.
pub fn effective_weight(target: &RouteTarget) -> u32 {
    target.priority
}

/// Weighted-random **full permutation** of `targets` (Efraimidis–Spirakis
/// A-Res): draw `u ∈ (0, 1]` per target, sort by `key = u^(1/w)`
/// descending. The head of the permutation is the weighted-random primary
/// choice and the tail is its fallback order — one draw decides both, so
/// failover never re-rolls (design §4.4).
///
/// Rules:
/// - all weights 0 → uniform shuffle (avoids the all-zero undefined draw);
/// - a single `w == 0` target gets key 0.0 and stays at the tail — it is
///   never drawn first but never leaves the permutation (fallback stays
///   complete). Stable sort keeps zero-weight targets in input order.
///
/// The RNG is caller-supplied so tests can pin a seed. Every request
/// re-draws; the permutation is never cached.
pub fn weighted_permutation(targets: &[RouteTarget], rng: &mut fastrand::Rng) -> Vec<usize> {
    let mut order: Vec<usize> = (0..targets.len()).collect();
    if targets.iter().all(|t| effective_weight(t) == 0) {
        rng.shuffle(&mut order);
        return order;
    }
    let mut keyed: Vec<(f64, usize)> = targets
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let w = effective_weight(t) as f64;
            // `1.0 - rng.f64()` maps [0,1) to (0,1] — u = 0 would pin the
            // key at 0 and collide with the zero-weight tail.
            let key = if w == 0.0 {
                0.0
            } else {
                (1.0 - rng.f64()).powf(1.0 / w)
            };
            (key, i)
        })
        .collect();
    // Descending; stable so equal keys (only the w == 0 tail — positive
    // weights tie with probability zero) keep input order.
    keyed.sort_by(|a, b| b.0.total_cmp(&a.0));
    keyed.into_iter().map(|(_, i)| i).collect()
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
            capabilities: None,
        }
    }

    fn orders_map() -> Arc<RwLock<HashMap<String, CachedOrder>>> {
        Arc::new(RwLock::new(HashMap::new()))
    }

    #[test]
    fn weighted_permutation_is_full_permutation() {
        let targets = vec![
            target("a", 5),
            target("b", 1),
            target("c", 0),
            target("d", 5),
            target("e", 0),
        ];
        let mut rng = fastrand::Rng::with_seed(42);
        for _ in 0..1000 {
            let order = weighted_permutation(&targets, &mut rng);
            assert_eq!(order.len(), targets.len(), "length must be preserved");
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(
                sorted,
                (0..targets.len()).collect::<Vec<_>>(),
                "permutation must contain every index exactly once"
            );
        }
    }

    #[test]
    fn weighted_permutation_head_is_weighted() {
        // A:B = 2:1 → A must land first ≈ 66.7% of the time.
        // 10k draws, ±3% tolerance (≈6σ for this p) on a pinned seed.
        let targets = vec![target("a", 2), target("b", 1)];
        let mut rng = fastrand::Rng::with_seed(7);
        let n = 10_000;
        let a_first = (0..n)
            .filter(|_| weighted_permutation(&targets, &mut rng)[0] == 0)
            .count();
        let share = a_first as f64 / n as f64;
        assert!(
            (share - 2.0 / 3.0).abs() < 0.03,
            "expected ≈0.667 first-slot share for weight 2/3, got {share}"
        );
    }

    #[test]
    fn weighted_permutation_all_zero_is_uniform() {
        // All-zero (or all-defaulted) weights → uniform shuffle.
        let targets = vec![target("a", 0), target("b", 0), target("c", 0)];
        let mut rng = fastrand::Rng::with_seed(11);
        let n = 10_000;
        let mut firsts = [0usize; 3];
        for _ in 0..n {
            firsts[weighted_permutation(&targets, &mut rng)[0]] += 1;
        }
        for (i, count) in firsts.iter().enumerate() {
            let share = *count as f64 / n as f64;
            assert!(
                (share - 1.0 / 3.0).abs() < 0.03,
                "target {i} expected ≈0.333 first-slot share, got {share}"
            );
        }
    }

    #[test]
    fn weighted_permutation_zero_weight_stays_in_tail_never_first() {
        let targets = vec![target("a", 10), target("b", 1), target("zero", 0)];
        let mut rng = fastrand::Rng::with_seed(99);
        for _ in 0..1000 {
            let order = weighted_permutation(&targets, &mut rng);
            assert_ne!(order[0], 2, "zero-weight target must never be first");
            assert_eq!(
                *order.last().unwrap(),
                2,
                "zero-weight target pins the tail"
            );
        }
    }

    #[test]
    fn weighted_permutation_single_target() {
        let targets = vec![target("solo", 0)];
        let mut rng = fastrand::Rng::with_seed(1);
        assert_eq!(weighted_permutation(&targets, &mut rng), vec![0]);
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
                capabilities: None,
            })
            .collect();

        let first = order_fill_first(targets.clone(), "m", &orders, ttl, Instant::now(), 1).await;
        let second = order_fill_first(targets.clone(), "m", &orders, ttl, Instant::now(), 1).await;
        let third = order_fill_first(targets, "m", &orders, ttl, Instant::now(), 1).await;

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
                capabilities: None,
            })
            .collect();

        let t0 = Instant::now();
        let first = order_fill_first(targets.clone(), "m", &orders, ttl, t0, 1).await;
        // Cache is fresh — next call returns same order.
        let second = order_fill_first(targets.clone(), "m", &orders, ttl, t0, 1).await;
        assert_eq!(first, second, "same order while cache is fresh");

        // Advance time past TTL — the first call must expire and update
        // the cache timestamp. This directly proves the TTL expiry path
        // executed, without relying on shuffle probability.
        let t1 = t0 + std::time::Duration::from_secs(2);
        let _next = order_fill_first(targets.clone(), "m", &orders, ttl, t1, 1).await;
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
        let ordered = order_fill_first(targets, "m", &orders, ttl, Instant::now(), 1).await;

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

        let a1 = order_fill_first(targets_a.clone(), "m", &orders, ttl, Instant::now(), 1).await;
        let a2 = order_fill_first(targets_a.clone(), "m", &orders, ttl, Instant::now(), 1).await;
        assert_eq!(a1, a2, "same targets → cached");

        // Targets changed (added "c") → must re-shuffle
        let b1 = order_fill_first(targets_b.clone(), "m", &orders, ttl, Instant::now(), 1).await;
        assert_eq!(b1.len(), 3, "new target list includes c");
        let b2 = order_fill_first(targets_b, "m", &orders, ttl, Instant::now(), 1).await;
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
            capabilities: None,
        }];
        let targets_b: Vec<RouteTarget> = vec![RouteTarget {
            provider: "a".into(),
            real_model: None,
            priority: 0,
            retries: None,
            timeout_secs: None,
            extra: HashMap::from([("temperature".into(), serde_json::json!(0.9))]),
            capabilities: None,
        }];

        let a1 = order_fill_first(targets_a.clone(), "m", &orders, ttl, Instant::now(), 1).await;
        let a2 = order_fill_first(targets_a.clone(), "m", &orders, ttl, Instant::now(), 1).await;
        assert_eq!(a1, a2, "same targets → cached");

        // Extra changed → must re-shuffle even though provider/real_model/priority are identical
        let b1 = order_fill_first(targets_b.clone(), "m", &orders, ttl, Instant::now(), 1).await;
        assert_eq!(
            b1[0].extra.get("temperature").unwrap(),
            &serde_json::json!(0.9)
        );
        let b2 = order_fill_first(targets_b, "m", &orders, ttl, Instant::now(), 1).await;
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
            capabilities: None,
        }];
        let targets_b: Vec<RouteTarget> = vec![RouteTarget {
            provider: "a".into(),
            real_model: None,
            priority: 0,
            retries: Some(3),
            timeout_secs: None,
            extra: HashMap::new(),
            capabilities: None,
        }];

        let a1 = order_fill_first(targets_a.clone(), "m", &orders, ttl, Instant::now(), 1).await;
        let a2 = order_fill_first(targets_a.clone(), "m", &orders, ttl, Instant::now(), 1).await;
        assert_eq!(a1, a2, "same targets → cached");

        // Retries changed → must re-shuffle even though provider/real_model/priority are identical
        let b1 = order_fill_first(targets_b.clone(), "m", &orders, ttl, Instant::now(), 1).await;
        assert_eq!(b1[0].retries, Some(3));
        let b2 = order_fill_first(targets_b, "m", &orders, ttl, Instant::now(), 1).await;
        assert_eq!(b1, b2, "cached after re-shuffle");
    }

    #[allow(clippy::type_complexity)]
    fn active_map() -> sag_state::ActiveTargets {
        Arc::new(RwLock::new(sag_state::ActiveTargetsInner::new()))
    }

    #[test]
    fn prepend_sticky_target_hoists_matching_pin_to_front() {
        let mut shuffled = vec![target("a", 0), target("b", 0), target("c", 0)];
        let entry = sag_state::ActiveTargetEntry::new("b", "model", "model", None, Instant::now());

        prepend_sticky_target(&mut shuffled, &entry, "model", 0);

        assert_eq!(shuffled[0].provider, "b");
        assert_eq!(shuffled.len(), 3);
    }

    #[test]
    fn prepend_sticky_target_identity_uses_original_real_model_spec() {
        // A passthrough pin (`target_real_model: None`) must NOT hoist an
        // explicit-spec target (`Some("model")`) even though both resolve
        // to the same effective real model.
        let mut shuffled = vec![
            target("b", 0),
            RouteTarget {
                provider: "openai".into(),
                real_model: Some("model".into()),
                priority: 0,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
                capabilities: None,
            },
        ];
        let entry =
            sag_state::ActiveTargetEntry::new("openai", "model", "model", None, Instant::now());

        prepend_sticky_target(&mut shuffled, &entry, "model", 0);

        assert_eq!(
            shuffled[0].provider, "b",
            "passthrough pin must not hoist the explicit-spec target"
        );
    }

    #[test]
    fn prepend_sticky_target_expired_pin_falls_through() {
        let mut shuffled = vec![target("a", 0), target("b", 0)];
        let entry = sag_state::ActiveTargetEntry::new(
            "b",
            "model",
            "model",
            None,
            Instant::now() - std::time::Duration::from_secs(7200),
        );

        prepend_sticky_target(&mut shuffled, &entry, "model", 3600);

        assert_eq!(
            shuffled[0].provider, "a",
            "expired pin must not alter the order"
        );
    }

    #[test]
    fn prepend_sticky_target_zero_ttl_never_expires() {
        let mut shuffled = vec![target("a", 0), target("b", 0)];
        let entry = sag_state::ActiveTargetEntry::new(
            "b",
            "model",
            "model",
            None,
            Instant::now() - std::time::Duration::from_secs(7200),
        );

        prepend_sticky_target(&mut shuffled, &entry, "model", 0);

        assert_eq!(shuffled[0].provider, "b", "ttl=0 means never expire");
    }

    #[test]
    fn prepend_sticky_target_absent_pin_is_noop() {
        let mut shuffled = vec![target("a", 0), target("b", 0)];
        let entry = sag_state::ActiveTargetEntry::new("z", "model", "model", None, Instant::now());

        prepend_sticky_target(&mut shuffled, &entry, "model", 3600);

        assert_eq!(shuffled[0].provider, "a");
        assert_eq!(shuffled.len(), 2);
    }

    #[tokio::test]
    async fn round_robin_rotates_by_counter() {
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let orders = orders_map();
        let active = active_map();
        let ttl = 7200;
        let targets = vec![target("a", 0), target("b", 0), target("c", 0)];

        // First call: starts at 0
        let (r1, _) = order_targets(
            targets.clone(),
            "model",
            Strategy::RoundRobin,
            &counters,
            &orders,
            &active,
            ttl,
            0,
            1,
        )
        .await;
        assert_eq!(r1[0].provider, "a");

        // Second call: starts at 1
        let (r2, _) = order_targets(
            targets.clone(),
            "model",
            Strategy::RoundRobin,
            &counters,
            &orders,
            &active,
            ttl,
            0,
            1,
        )
        .await;
        assert_eq!(r2[0].provider, "b");

        // Third call: starts at 2
        let (r3, _) = order_targets(
            targets.clone(),
            "model",
            Strategy::RoundRobin,
            &counters,
            &orders,
            &active,
            ttl,
            0,
            1,
        )
        .await;
        assert_eq!(r3[0].provider, "c");

        // Fourth call: wraps back to 0
        let (r4, _) = order_targets(
            targets,
            "model",
            Strategy::RoundRobin,
            &counters,
            &orders,
            &active,
            ttl,
            0,
            1,
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

        let (r1, _) = order_targets(
            targets.clone(),
            "model-a",
            Strategy::RoundRobin,
            &counters,
            &orders,
            &active,
            ttl,
            0,
            1,
        )
        .await;
        let (r2, _) = order_targets(
            targets.clone(),
            "model-b",
            Strategy::RoundRobin,
            &counters,
            &orders,
            &active,
            ttl,
            0,
            1,
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

        // Record "b" as the active target. The route targets are passthrough
        // (`real_model: None`, see `target()`), so the pin must record
        // `target_real_model: None` — the identity that dispatch actually
        // writes back for a passthrough target.
        {
            let mut guard = active.write().await;
            guard.insert(
                "model".to_string(),
                sag_state::ActiveTargetEntry::new("b", "model", "model", None, Instant::now()),
            );
        }

        let (ordered, token) = order_targets(
            targets,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0,
            5,
        )
        .await;

        // Fresh memory, nothing cleared: the decision captured 0.
        assert_eq!(token.generation, 0);
        // The token carries the routes_version captured at decision time.
        assert_eq!(token.routes_version, 5);
        // "b" must be prepended to the front (index 0)
        assert_eq!(ordered[0].provider, "b");
    }

    #[tokio::test]
    async fn sticky_failover_identity_is_provider_plus_original_real_model_spec() {
        // A passthrough target (`real_model: None`) and an explicit target
        // (`real_model: Some(model_name)`) on the same provider and priority
        // resolve to the SAME effective real model ("model"). Sticky must
        // still match only the exact target that was pinned — matching by
        // provider + effective model alone would hoist whichever target
        // happens to appear first in the shuffled order.
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let orders = orders_map();
        let active = active_map();
        let targets = vec![
            RouteTarget {
                provider: "openai".into(),
                real_model: None, // passthrough — effective model = "model"
                priority: 10,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
                capabilities: None,
            },
            RouteTarget {
                provider: "openai".into(),
                real_model: Some("model".into()), // explicit — same effective model
                priority: 10,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
                capabilities: None,
            },
        ];

        // Pin the passthrough target (original spec `None`).
        {
            let mut guard = active.write().await;
            guard.insert(
                "model".to_string(),
                sag_state::ActiveTargetEntry::new(
                    "openai",
                    "model", // effective real model (passthrough = request model)
                    "model",
                    None, // original spec: passthrough
                    Instant::now(),
                ),
            );
        }

        let (ordered, _) = order_targets(
            targets.clone(),
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            7200,
            0,
            1,
        )
        .await;
        assert_eq!(
            ordered[0].real_model, None,
            "sticky must prepend the pinned passthrough target, not the explicit one"
        );

        // Pin the explicit target (original spec `Some("model")`) instead.
        {
            let mut guard = active.write().await;
            guard.insert(
                "model".to_string(),
                sag_state::ActiveTargetEntry::new(
                    "openai",
                    "model", // effective real model (explicit override)
                    "model",
                    Some("model".to_string()), // original spec: explicit
                    Instant::now(),
                ),
            );
        }

        let (ordered2, _) = order_targets(
            targets,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            7200,
            0,
            1,
        )
        .await;
        assert_eq!(
            ordered2[0].real_model.as_deref(),
            Some("model"),
            "sticky must prepend the pinned explicit target, not the passthrough one"
        );
    }

    #[tokio::test]
    async fn sticky_failover_token_is_captured_before_the_async_shuffle_window() {
        // The token must represent the decision state read *before* the
        // async `order_fill_first` — a clear that lands after the decision
        // (e.g. during the shuffle, or between the decision and the
        // writeback) must invalidate the token, not be masked by it.
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let orders = orders_map();
        let active = active_map();
        let targets = vec![target("a", 0), target("b", 0)];

        // Passthrough targets → the pin records `target_real_model: None`.
        {
            let mut guard = active.write().await;
            guard.insert(
                "model".to_string(),
                sag_state::ActiveTargetEntry::new("b", "model", "model", None, Instant::now()),
            );
        }

        // Decision happens now, with an existing entry and a fresh (never
        // cleared) generation → token captures (0, 5). The targets are
        // passthrough (`real_model: None`), so the pin records
        // `target_real_model: None`.
        let (ordered, token) = order_targets(
            targets,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            7200,
            0,
            5,
        )
        .await;
        assert_eq!(ordered[0].provider, "b", "sticky entry must be prepended");
        assert_eq!(token.generation, 0);
        assert_eq!(token.routes_version, 5);

        // A clear lands *after* the decision (the exact window the reviewer
        // flagged: between `order_fill_first` and the writeback). The token
        // captured at decision time must now be rejected — this is only
        // possible if the token was captured before the clear, i.e. at the
        // linearization point rather than after the async shuffle.
        active.write().await.clear("model");
        let mut guard = active.write().await;
        assert!(
            !guard.insert_if_generation(
                "model".into(),
                sag_state::ActiveTargetEntry::new(
                    "b",
                    "model",
                    "model",
                    Some("model".to_string()),
                    Instant::now(),
                ),
                &token,
                5,
            ),
            "a clear after the decision must reject the captured token"
        );
        assert!(
            guard.get("model").is_none(),
            "the stale writeback must not re-pin the cleared entry"
        );
    }

    #[tokio::test]
    async fn sticky_failover_token_is_rejected_when_routes_version_moves() {
        // A config mutation after the decision (route / provider CRUD,
        // hot-reload) moves `routes_version`; even a decision that captured
        // the *current* per-model generation (here: `None`, never cleared)
        // must not write back — this covers models that never had an entry.
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let orders = orders_map();
        let active = active_map();
        let targets = vec![target("a", 0)];

        let (_, token) = order_targets(
            targets,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            7200,
            0,
            5,
        )
        .await;
        assert_eq!(token.generation, 0, "model never had an entry");

        // Simulate a config mutation after the decision: writeback with the
        // old routes_version must be rejected, even though the per-model
        // generation is unchanged.
        let mut guard = active.write().await;
        assert!(
            !guard.insert_if_generation(
                "model".into(),
                sag_state::ActiveTargetEntry::new(
                    "a",
                    "model",
                    "model",
                    Some("model".to_string()),
                    Instant::now(),
                ),
                &token,
                6,
            ),
            "a config mutation after the decision must reject the writeback"
        );
        assert!(
            guard.get("model").is_none(),
            "the stale writeback must not pin a model whose route state changed"
        );
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
            1,
        )
        .await;

        // Record "b" as the active target
        {
            let mut guard = active.write().await;
            guard.insert(
                "model".to_string(),
                sag_state::ActiveTargetEntry::new(
                    "b",
                    "model",
                    "model",
                    Some("model".to_string()),
                    Instant::now(),
                ),
            );
        }

        // Run again with targets_b (which is different)
        let (ordered, generation) = order_targets(
            targets_b,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0,
            1,
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
        // The clear also bumped the generation: the decision in *this* call
        // captured the post-clear generation.
        assert_eq!(generation.generation, 1);

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
                capabilities: None,
            },
            RouteTarget {
                provider: "b".into(),
                real_model: None,
                priority: 0,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
                capabilities: None,
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
                capabilities: None,
            },
            RouteTarget {
                provider: "b".into(),
                real_model: None,
                priority: 0,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
                capabilities: None,
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
            1,
        )
        .await;

        // Record "b" as the active target
        {
            let mut guard = active.write().await;
            guard.insert(
                "model".to_string(),
                sag_state::ActiveTargetEntry::new(
                    "b",
                    "model",
                    "model",
                    Some("model".to_string()),
                    Instant::now(),
                ),
            );
        }

        // Run again with targets_b (extra changed but provider/real_model/priority identical)
        let (ordered, generation) = order_targets(
            targets_b,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0,
            1,
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
        assert_eq!(
            generation.generation, 1,
            "targets-changed clear must bump the generation"
        );

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
                capabilities: None,
            },
            RouteTarget {
                provider: "b".into(),
                real_model: None,
                priority: 0,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
                capabilities: None,
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
                capabilities: None,
            },
            RouteTarget {
                provider: "b".into(),
                real_model: None,
                priority: 0,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
                capabilities: None,
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
            1,
        )
        .await;

        // Record "b" as the active target
        {
            let mut guard = active.write().await;
            guard.insert(
                "model".to_string(),
                sag_state::ActiveTargetEntry::new(
                    "b",
                    "model",
                    "model",
                    Some("model".to_string()),
                    Instant::now(),
                ),
            );
        }

        // Run again with targets_b (retries changed but provider/real_model/priority identical)
        let (ordered, generation) = order_targets(
            targets_b,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0,
            1,
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
        assert_eq!(
            generation.generation, 1,
            "targets-changed clear must bump the generation"
        );

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
        let (baseline, _) = order_targets(
            targets.clone(),
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            7200,
            3600, // reset window, irrelevant: no active entry exists
            1,
        )
        .await;

        // Insert an EXPIRED active entry pointing at "b". Passthrough
        // targets → the pin records `target_real_model: None`, so the
        // identity matches and the expiry logic decides the outcome.
        {
            let mut guard = active.write().await;
            guard.insert(
                "model".to_string(),
                sag_state::ActiveTargetEntry::new(
                    "b",
                    "model",
                    "model",
                    None,
                    Instant::now() - std::time::Duration::from_secs(7200),
                ),
            );
        }

        // Re-run: cache is already primed (same shuffle), only difference
        // is the expired active entry.
        let (ordered, _) = order_targets(
            targets,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            7200,
            3600, // entry is 7200s old, window is 3600s -> expired
            1,
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

        // Record "b" as the active target, freshly created. Passthrough
        // targets → the pin records `target_real_model: None`.
        {
            let mut guard = active.write().await;
            guard.insert(
                "model".to_string(),
                sag_state::ActiveTargetEntry::new("b", "model", "model", None, Instant::now()),
            );
        }

        let (ordered, _) = order_targets(
            targets,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            3600, // reset window: 3600s - the fresh entry is not expired
            1,
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
                sag_state::ActiveTargetEntry::new(
                    "b",
                    "model",
                    "model",
                    None,
                    Instant::now() - std::time::Duration::from_secs(7200),
                ),
            );
        }

        let (ordered, _) = order_targets(
            targets,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0, // reset window: 0 -> never expire
            1,
        )
        .await;

        // Even though the entry is old, ttl=0 means never reset - "b" must
        // still be prepended.
        assert_eq!(ordered[0].provider, "b");
    }

    // ── stale-request regression: old config must not pollute the shared
    //    ordering cache / sticky state of a newer config ────────────────

    #[tokio::test]
    async fn stale_request_does_not_clear_new_pin_or_overwrite_new_cache() {
        // Reviewer trigger sequence: request A resolves old targets at
        // routes_version V; the config becomes V+1; request B builds a new
        // CachedOrder and successfully pins; A (stale) then enters
        // `order_targets` and must neither clear B's pin nor overwrite B's
        // cache with its old targets.
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let orders = orders_map();
        let active = active_map();
        let ttl = 7200;
        let targets_a = vec![target("a", 0), target("b", 0)];
        let targets_b = vec![target("a", 0), target("b", 0), target("c", 0)];

        // Request B (routes_version 2) builds the new cache and pins "b".
        let (ordered_b, token_b) = order_targets(
            targets_b.clone(),
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0,
            2,
        )
        .await;
        assert_eq!(ordered_b.len(), 3);
        {
            let mut guard = active.write().await;
            assert!(
                guard.insert_if_generation(
                    "model".into(),
                    sag_state::ActiveTargetEntry::new("b", "model", "model", None, Instant::now()),
                    &token_b,
                    2,
                ),
                "B's pin must record"
            );
        }
        assert_eq!(
            orders.read().await.get("model").unwrap().routes_version,
            Some(2),
            "B's cache must carry its routes_version"
        );

        // Request A (routes_version 1, old targets) continues into
        // `order_targets` *after* B.
        let (ordered_a, _) = order_targets(
            targets_a,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0,
            1,
        )
        .await;

        // A must NOT clear B's active pin...
        assert!(
            active.read().await.get("model").is_some(),
            "stale request must not clear the new active pin"
        );
        // ...must NOT overwrite B's CachedOrder...
        let guard = orders.read().await;
        let cached = guard.get("model").unwrap();
        assert_eq!(cached.routes_version, Some(2), "cache version must stay 2");
        assert_eq!(
            cached.targets.len(),
            3,
            "cache must still hold B's targets, not A's stale ones"
        );
        // ...and A still received a local order over its own old targets.
        assert_eq!(ordered_a.len(), 2, "A sorts its own old targets locally");

        // A later request at the NEW version still uses B's cache/active.
        let (ordered_new, token_new) = order_targets(
            targets_b,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0,
            2,
        )
        .await;
        assert_eq!(ordered_new.len(), 3, "new-version request uses B's cache");
        assert_eq!(
            ordered_new[0].provider, "b",
            "new-version request still honors B's pin"
        );
        assert_eq!(token_new.generation, 0, "B's pin was never cleared");
    }

    #[tokio::test]
    async fn stale_request_with_matching_targets_does_not_overwrite_new_cache() {
        // The config version moved but the route's targets are unchanged: a
        // stale request must still not downgrade the newer cache entry even
        // though its targets match the cached ones.
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let orders = orders_map();
        let active = active_map();
        let ttl = 7200;
        let targets = vec![target("a", 0), target("b", 0)];

        // B (version 2) establishes the cache.
        order_targets(
            targets.clone(),
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0,
            2,
        )
        .await;
        assert_eq!(
            orders.read().await.get("model").unwrap().routes_version,
            Some(2)
        );

        // A (version 1) with identical targets: shuffles locally and leaves
        // the version-2 cache intact.
        let (ordered_a, _) = order_targets(
            targets,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0,
            1,
        )
        .await;
        assert_eq!(ordered_a.len(), 2);
        let guard = orders.read().await;
        assert_eq!(
            guard.get("model").unwrap().routes_version,
            Some(2),
            "stale request must not downgrade the cache version"
        );
    }

    #[tokio::test]
    async fn legacy_cache_without_version_keeps_clear_and_update_behavior() {
        // A cache entry without a routes_version (pre-versioning era / test
        // fixture): any request may invalidate it — the current behavior is
        // preserved, i.e. targets change → clear the pin + replace cache.
        let counters: Arc<RwLock<HashMap<String, u64>>> = Arc::new(RwLock::new(HashMap::new()));
        let orders = orders_map();
        let active = active_map();
        let ttl = 7200;
        orders.write().await.insert(
            "model".into(),
            CachedOrder {
                targets: Arc::from([target("a", 0), target("b", 0)]),
                created_at: Instant::now(),
                routes_version: None,
            },
        );
        active.write().await.insert(
            "model".into(),
            sag_state::ActiveTargetEntry::new(
                "b",
                "model",
                "model",
                Some("model".into()),
                Instant::now(),
            ),
        );

        // New targets at version 1: must clear the pin, bump the generation,
        // and replace the legacy cache (now versioned).
        let targets_b = vec![target("a", 0), target("b", 0), target("c", 0)];
        let (ordered, token) = order_targets(
            targets_b,
            "model",
            Strategy::StickyFailover,
            &counters,
            &orders,
            &active,
            ttl,
            0,
            1,
        )
        .await;

        assert_eq!(ordered.len(), 3);
        assert!(
            active.read().await.get("model").is_none(),
            "legacy cache + changed targets must clear the pin"
        );
        assert_eq!(token.generation, 1, "the clear bumped the generation");
        let guard = orders.read().await;
        let cached = guard.get("model").unwrap();
        assert_eq!(cached.routes_version, Some(1), "cache is now versioned");
        assert_eq!(cached.targets.len(), 3);
    }
}
