//! Per-route in-flight request counting.
//!
//! A route's "in-flight" count is the number of requests currently being
//! processed for it (queueing + upstream time). The count is keyed by
//! **route name** — the matched route's `model_name`, the same identity
//! `sag-dispatch`'s `DispatchCtx::route_name` carries — so the management
//! layer can show "which route is busy right now" without touching the
//! dispatch engine.
//!
//! Two pieces of state live here:
//!
//! - the authoritative `HashMap<route_name, count>`, guarded by a
//!   **`std::sync::Mutex`** (not `tokio::sync::Mutex`): the increment and
//!   the decrement both run inside [`InFlightGuard`]'s `Drop`, which cannot
//!   await, and every critical section is a single map update with no await
//!   inside — the same convention `sag-dispatch`'s cancellation guard uses;
//! - a [`watch::Sender`] publishing an immutable [`InFlightSnapshot`] on
//!   every change. `watch` coalesces bursts of acquires/releases into "the
//!   latest snapshot", so an observer (the SSE endpoint) never has to
//!   reconstruct state from deltas and a reconnecting client is healed by
//!   the next snapshot it receives.
//!
//! Invariants:
//!
//! 1. **Zero-count keys are deleted** — the map never holds a `0` value, so
//!    "show only routes with in-flight > 0" is free for the consumer and a
//!    deleted/renamed route's residual count disappears with its last
//!    request (no prune wiring needed).
//! 2. **Release only through `Drop`** — never decrement manually: every
//!    early-return / error / panic path then releases exactly once. A
//!    streamed response moves its guard into the response body state, so a
//!    client disconnect (body drop) releases the count too.
//! 3. **No `await` inside `Drop`** — the snapshot is published with
//!    [`watch::Sender::send_replace`], which never blocks and never panics
//!    when there are no subscribers.
//! 4. `total` is always the sum of the per-route counts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::watch;

/// One route's in-flight count inside an [`InFlightSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct InFlightRouteCount {
    /// The route's `model_name` (the request-facing route identity).
    pub model_name: String,
    /// Requests currently being processed for this route (`> 0` always —
    /// zero-count routes are dropped from the map).
    pub count: u64,
}

/// Immutable point-in-time view of the per-route in-flight counts.
///
/// Published on every change through [`InFlightRegistry::subscribe`] and
/// serialised verbatim as the SSE event payload (`seq` lets an observer
/// discard an out-of-order snapshot; `routes` is sorted by `count`
/// descending, ties broken by `model_name`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct InFlightSnapshot {
    /// Monotonic publication counter (starts at 0 for the empty registry).
    pub seq: u64,
    /// Sum of all per-route counts — the number of requests in flight.
    pub total: usize,
    /// Per-route counts, `count` descending. Empty when nothing is in flight.
    pub routes: Vec<InFlightRouteCount>,
}

/// Shared handle for the in-flight registry — the [`AppState`] field type,
/// mirroring the `Arc`-shared-handle pattern of the routing caches.
///
/// [`AppState`]: crate::AppState
pub type InFlight = Arc<InFlightRegistry>;

/// Mutable state behind the registry's `std::sync::Mutex`.
#[derive(Default)]
struct InFlightInner {
    /// `route_name` → in-flight count. Never holds a `0` value.
    counts: HashMap<String, u64>,
    /// Publication counter, bumped under the same lock as `counts` so a
    /// published snapshot's `seq` can never go backwards.
    seq: u64,
}

/// Per-route in-flight request counter with a `watch` snapshot channel.
///
/// Construct one per process (`Arc::default()` / [`Self::new`]) and share the
/// same `Arc` between `AppState` and `DispatchRuntime` — exactly like the
/// other shared routing handles.
#[derive(Default)]
pub struct InFlightRegistry {
    inner: Mutex<InFlightInner>,
    snapshot: watch::Sender<InFlightSnapshot>,
}

impl std::fmt::Debug for InFlightRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InFlightRegistry")
            .field("snapshot", &*self.snapshot.borrow())
            .finish()
    }
}

impl InFlightRegistry {
    /// Create an empty registry (initial snapshot: `seq = 0`, `total = 0`).
    pub fn new() -> Self {
        Self::default()
    }

    /// The current snapshot, cloned out of the watch channel.
    ///
    /// Cheap (one small `Vec` clone); the SSE handler uses
    /// [`Self::subscribe`] instead so it is woken on change.
    pub fn snapshot(&self) -> InFlightSnapshot {
        self.snapshot.borrow().clone()
    }

    /// Subscribe to snapshot publications.
    ///
    /// The receiver starts at the latest published snapshot, so a fresh
    /// subscriber can emit it immediately and then await `changed()` —
    /// connect / reconnect needs no separate "full state" request.
    pub fn subscribe(&self) -> watch::Receiver<InFlightSnapshot> {
        self.snapshot.subscribe()
    }

    /// Count the request as in-flight for `model_name` and return the RAII
    /// guard that releases it on drop.
    ///
    /// Takes `self: &Arc<Self>` so the guard can own a strong handle: a
    /// streamed response moves the guard into a `'static` body state, which
    /// outlives the dispatch call stack.
    #[must_use = "dropping the guard immediately releases the count"]
    pub fn acquire(self: &Arc<Self>, model_name: impl Into<String>) -> InFlightGuard {
        let model_name = model_name.into();
        let mut inner = self.lock();
        *inner.counts.entry(model_name.clone()).or_insert(0) += 1;
        self.publish(&mut inner);
        InFlightGuard {
            registry: Arc::clone(self),
            model_name,
        }
    }

    /// Poisoned-lock recovery: a panic while holding the lock would only
    /// happen on a fatal bug; `into_inner` keeps counting functional instead
    /// of turning every later request into a panic.
    fn lock(&self) -> MutexGuard<'_, InFlightInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Build the snapshot for the current map and publish it. Called with
    /// the lock held, so `seq` order matches `counts` order.
    fn publish(&self, inner: &mut InFlightInner) {
        inner.seq += 1;
        let mut routes: Vec<InFlightRouteCount> = inner
            .counts
            .iter()
            .map(|(model_name, count)| InFlightRouteCount {
                model_name: model_name.clone(),
                count: *count,
            })
            .collect();
        // Busiest route first; `model_name` breaks ties so the frame is
        // deterministic (stable SSE output and test assertions).
        routes.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.model_name.cmp(&b.model_name))
        });
        let total = routes.iter().map(|r| r.count as usize).sum();
        self.snapshot.send_replace(InFlightSnapshot {
            seq: inner.seq,
            total,
            routes,
        });
    }
}

/// RAII in-flight guard: `+1` on [`InFlightRegistry::acquire`], `-1` on drop.
///
/// Holds an `Arc` to the registry (never a borrow) so it can be moved into a
/// `'static` structure — the streaming response body state — where a client
/// disconnect (body drop) releases the count. `Drop` does not await.
///
/// Non-streaming dispatches simply keep the guard on the stack; the release
/// then happens on every return path, including error and panic unwinds.
pub struct InFlightGuard {
    registry: InFlight,
    model_name: String,
}

impl InFlightGuard {
    /// The route this guard counts against.
    pub fn model_name(&self) -> &str {
        &self.model_name
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut inner = self.registry.lock();
        let released_to_zero = {
            // A missing key would mean a double release — impossible by
            // construction (`Drop` runs once); never panic in a destructor.
            // Do not swallow it silently either: a future bug would then
            // corrupt the snapshot unnoticed, so leave a trace and keep the
            // defensive no-op.
            let Some(count) = inner.counts.get_mut(&self.model_name) else {
                tracing::warn!(
                    model_name = %self.model_name,
                    "in-flight guard released for unknown route"
                );
                return;
            };
            *count = count.saturating_sub(1);
            *count == 0
        };
        if released_to_zero {
            // Invariant 1: the map never keeps a zero-count entry, so the
            // snapshot only ever lists routes with requests in flight.
            inner.counts.remove(&self.model_name);
        }
        self.registry.publish(&mut inner);
    }
}

impl std::fmt::Debug for InFlightGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InFlightGuard")
            .field("model_name", &self.model_name)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::thread;

    use super::*;

    fn counts(snapshot: &InFlightSnapshot) -> Vec<(String, u64)> {
        snapshot
            .routes
            .iter()
            .map(|r| (r.model_name.clone(), r.count))
            .collect()
    }

    #[test]
    fn acquire_increments_and_drop_releases() {
        let registry: InFlight = Arc::default();
        assert_eq!(registry.snapshot(), InFlightSnapshot::default());

        let guard = registry.acquire("gpt-4o");
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.total, 1);
        assert_eq!(snapshot.routes.len(), 1);
        assert_eq!(snapshot.routes[0].model_name, "gpt-4o");
        assert_eq!(snapshot.routes[0].count, 1);
        assert_eq!(guard.model_name(), "gpt-4o");

        drop(guard);
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.total, 0);
        assert!(
            snapshot.routes.is_empty(),
            "a zero-count route must be dropped from the map, got {:?}",
            snapshot.routes
        );
    }

    #[test]
    fn nested_acquisitions_accumulate_per_route() {
        let registry: InFlight = Arc::default();
        let a = registry.acquire("gpt-4o");
        let b = registry.acquire("gpt-4o");
        assert_eq!(counts(&registry.snapshot()), vec![("gpt-4o".into(), 2)]);

        drop(a);
        assert_eq!(
            counts(&registry.snapshot()),
            vec![("gpt-4o".into(), 1)],
            "one release must not clear the route"
        );

        drop(b);
        assert!(registry.snapshot().routes.is_empty());
    }

    #[test]
    fn zero_count_removes_key_only_for_that_route() {
        let registry: InFlight = Arc::default();
        let a = registry.acquire("route-a");
        let b = registry.acquire("route-b");

        drop(a);
        let snapshot = registry.snapshot();
        assert_eq!(
            counts(&snapshot),
            vec![("route-b".into(), 1)],
            "route-a must be gone while route-b stays"
        );
        drop(b);
        assert!(registry.snapshot().routes.is_empty());
    }

    #[test]
    fn snapshot_sorts_by_count_descending() {
        let registry: InFlight = Arc::default();
        // Acquire in a non-monotonic order so the sort (not the insertion
        // order) decides the frame.
        let a = registry.acquire("route-a");
        let c1 = registry.acquire("route-c");
        let b1 = registry.acquire("route-b");
        let b2 = registry.acquire("route-b");
        let c2 = registry.acquire("route-c");
        let c3 = registry.acquire("route-c");

        assert_eq!(
            counts(&registry.snapshot()),
            vec![
                ("route-c".into(), 3),
                ("route-b".into(), 2),
                ("route-a".into(), 1),
            ]
        );

        drop((a, c1, b1, b2, c2, c3));
        assert!(registry.snapshot().routes.is_empty());
    }

    #[test]
    fn ties_are_ordered_by_model_name() {
        let registry: InFlight = Arc::default();
        let z = registry.acquire("route-z");
        let a = registry.acquire("route-a");
        assert_eq!(
            counts(&registry.snapshot()),
            vec![("route-a".into(), 1), ("route-z".into(), 1)]
        );
        drop((z, a));
    }

    #[test]
    fn total_is_the_sum_of_route_counts() {
        let registry: InFlight = Arc::default();
        let _a = registry.acquire("route-a");
        let _b1 = registry.acquire("route-b");
        let _b2 = registry.acquire("route-b");

        let snapshot = registry.snapshot();
        let sum: u64 = snapshot.routes.iter().map(|r| r.count).sum();
        assert_eq!(snapshot.total as u64, sum);
        assert_eq!(snapshot.total, 3);
    }

    #[test]
    fn seq_increases_on_every_publication() {
        let registry: InFlight = Arc::default();
        assert_eq!(registry.snapshot().seq, 0, "empty registry starts at 0");

        let guard = registry.acquire("route-a");
        assert_eq!(registry.snapshot().seq, 1);
        drop(guard);
        assert_eq!(registry.snapshot().seq, 2, "release publishes too");
    }

    /// 1000 concurrent acquires from 4 threads (250 each), all released
    /// afterwards: the count must be conserved at every step — never lost,
    /// never leaked.
    #[test]
    fn concurrent_acquire_release_conserves_count() {
        const THREADS: usize = 4;
        const PER_THREAD: usize = 250;

        let registry: InFlight = Arc::default();
        let barrier = Arc::new(Barrier::new(THREADS + 1));

        let handles: Vec<_> = (0..THREADS)
            .map(|i| {
                let registry = Arc::clone(&registry);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let route = format!("route-{i}");
                    let guards: Vec<InFlightGuard> = (0..PER_THREAD)
                        .map(|_| registry.acquire(route.clone()))
                        .collect();
                    barrier.wait();
                    // Hold every guard across the main thread's assertions.
                    barrier.wait();
                    drop(guards);
                })
            })
            .collect();

        // Every thread has acquired and is holding; nobody released yet.
        barrier.wait();
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.total, THREADS * PER_THREAD);
        assert_eq!(snapshot.routes.len(), THREADS);
        for route in &snapshot.routes {
            assert_eq!(route.count, PER_THREAD as u64, "route {}", route.model_name);
        }

        barrier.wait();
        for handle in handles {
            handle.join().expect("counting thread must not panic");
        }
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.total, 0, "every acquire must be released");
        assert!(snapshot.routes.is_empty(), "no residual zero-count keys");
    }

    #[tokio::test]
    async fn subscribers_observe_each_snapshot() {
        let registry: InFlight = Arc::default();
        let mut rx = registry.subscribe();
        assert_eq!(rx.borrow_and_update().total, 0);

        let guard = registry.acquire("route-a");
        rx.changed()
            .await
            .expect("a publication must wake the subscriber");
        let snapshot = rx.borrow_and_update().clone();
        assert_eq!(snapshot.total, 1);
        assert_eq!(snapshot.routes[0].model_name, "route-a");

        drop(guard);
        rx.changed().await.expect("the release must publish too");
        assert_eq!(rx.borrow_and_update().total, 0);
    }
}
