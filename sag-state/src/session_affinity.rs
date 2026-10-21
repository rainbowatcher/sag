//! Session-level sticky pins for `strategy = "session_sticky"` routes.
//!
//! Structurally isomorphic to [`crate::ActiveTargetsInner`] (same
//! linearizability protocol, see [`StickyToken`]) but keyed by **session
//! identity** instead of request model: a request carrying a session key
//! sticks to the target that last served that session successfully.
//!
//! Three design invariants live here (design doc §4.2):
//!
//! - **Hashed keys only.** The map key is `SessionKey::map_id` — a truncated
//!   SHA-256 digest. Raw session values (conversation ids, prompt cache
//!   keys, content hashes) never enter this state.
//! - **TTL sliding + lazy expiry.** A hit refreshes `recorded_at` (idle
//!   timeout semantics — an active session never loses stickiness
//!   mid-conversation). An expired pin reads as a miss but is **not**
//!   deleted (read-then-write race); capacity eviction is the only
//!   proactive delete path.
//! - **Every removal path invalidates in-flight writebacks** by bumping the
//!   key's generation (or the global epoch for never-seen keys), so a
//!   request that decided its route before a clear can never re-pin the
//!   removed entry (mirrors [`StickyToken`]'s three-component token).

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use sag_config::{RouteConfig, Strategy};

use crate::StickyToken;

/// Default session-pin TTL in seconds (route `session_ttl_secs` default).
pub const DEFAULT_SESSION_TTL_SECS: u64 = 3600;
/// Default per-route session-pin capacity (route `session_max_entries`
/// default).
pub const DEFAULT_SESSION_MAX_ENTRIES: usize = 10_000;

/// A session pin: which target last successfully served a session, plus
/// the identity needed to re-check it against the current target list
/// (mirrors [`crate::ActiveTargetEntry`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPin {
    pub provider: String,
    /// Resolved real model sent upstream (the target's explicit
    /// `real_model`, or the request model for a passthrough target).
    pub real_model: String,
    /// Config route `model_name` that recorded this pin (`"*"` for a
    /// wildcard route) — the route-ownership record used by
    /// [`SessionAffinityInner::retain_sticky_routes`].
    pub route_model: String,
    /// The target's `real_model` spec at pin time: `None` = passthrough,
    /// `Some(rm)` = explicit override (see [`crate::ActiveTargetEntry`]).
    pub target_real_model: Option<String>,
    /// Recording/last-hit time — the TTL sliding-expiration clock.
    pub recorded_at: Instant,
    /// Config `routes_version` at pin time. A pin recorded under an older
    /// version reads as a miss (never deleted) after a config mutation.
    pub routes_version: u64,
}

impl SessionPin {
    /// Construct a pin.
    pub fn new(
        provider: impl Into<String>,
        real_model: impl Into<String>,
        route_model: impl Into<String>,
        target_real_model: Option<String>,
        recorded_at: Instant,
        routes_version: u64,
    ) -> Self {
        Self {
            provider: provider.into(),
            real_model: real_model.into(),
            route_model: route_model.into(),
            target_real_model,
            recorded_at,
            routes_version,
        }
    }
}

impl crate::StickyIdentity for SessionPin {
    fn id_provider(&self) -> &str {
        &self.provider
    }
    fn id_target_real_model(&self) -> Option<&str> {
        self.target_real_model.as_deref()
    }
    fn id_effective_real_model(&self) -> &str {
        &self.real_model
    }
}

/// Session-scoped concurrency token — same three components as
/// [`StickyToken`], captured at the routing decision point and verified at
/// the success write-back. A type alias keeps the two protocols visibly
/// identical.
pub type SessionStickyToken = StickyToken;

/// Everything the dispatch success path needs to conditionally (re)bind a
/// session pin: the hashed key, its space, the decision-point token, and
/// the route's capacity at decision time.
#[derive(Debug, Clone)]
pub struct SessionWriteback {
    /// Hashed session identity (`SessionKey::map_id`) — never the raw value.
    pub map_id: String,
    /// The **request model** — the space key (same scoping key as the
    /// model-level sticky map). `map_id` already folds the request model
    /// into its digest, so switching the requested model drops stickiness
    /// (design O1).
    pub model: String,
    /// Token captured at the decision point (see [`SessionStickyToken`]).
    pub token: SessionStickyToken,
    /// The route's `session_max_entries` at decision time (space capacity).
    pub capacity: usize,
}

/// One request-model's session space. Spaces are keyed by request model —
/// the same scoping key as the model-level sticky map — so capacity and
/// LRU eviction never mix sessions of different requested models; each
/// space is still route-owned through the recorded
/// [`SessionPin::route_model`] (see [`SessionAffinityInner::retain_sticky_routes`]).
#[derive(Debug, Clone, Default)]
struct RouteSessionSpace {
    /// map_id → pin.
    map: HashMap<String, SessionPin>,
    /// map_id → generation tombstone counter (bumped on every removal).
    generations: HashMap<String, u64>,
    /// LRU order: front = least recently used. Kept in sync with `map`
    /// (every insert/touch moves the key to the back; every removal drops
    /// the key), so `lru.len() == map.len()`.
    lru: VecDeque<String>,
    /// Capacity enforced at insert time (route `session_max_entries`).
    capacity: usize,
}

impl RouteSessionSpace {
    fn generation(&self, map_id: &str) -> u64 {
        self.generations.get(map_id).copied().unwrap_or(0)
    }

    /// Refresh the LRU position (move to back = most recently used).
    fn touch(&mut self, map_id: &str) {
        if let Some(pos) = self.lru.iter().position(|k| k == map_id) {
            self.lru.remove(pos);
        }
        self.lru.push_back(map_id.to_string());
    }

    /// Insert/overwrite a pin and enforce the capacity: evict the
    /// least-recently-used entries (bumping each evicted key's generation,
    /// see the module docs) until the map fits. `epoch` is bumped only in
    /// the unreachable case where the evicted key has no state at all.
    fn insert(&mut self, map_id: String, pin: SessionPin, capacity: usize, epoch: &mut u64) {
        self.capacity = capacity;
        self.map.insert(map_id.clone(), pin);
        self.touch(&map_id);
        while self.map.len() > self.capacity {
            let Some(old) = self.lru.pop_front() else {
                break;
            };
            if self.map.remove(&old).is_some() || self.generations.contains_key(&old) {
                *self.generations.entry(old).or_insert(0) += 1;
            } else {
                *epoch += 1;
            }
        }
    }

    /// Drop every entry in this space (generations are bumped and kept as
    /// tombstones — never removed, see [`Self::insert`] and the ActiveTargets
    /// `retain` rationale).
    fn clear_entries(&mut self) {
        let keys: HashSet<String> = self
            .map
            .keys()
            .chain(self.generations.keys())
            .cloned()
            .collect();
        self.map.clear();
        self.lru.clear();
        for k in keys {
            *self.generations.entry(k).or_insert(0) += 1;
        }
    }
}

/// Session-affinity memory: request model → [`RouteSessionSpace`].
#[derive(Debug, Clone, Default)]
pub struct SessionAffinityInner {
    spaces: HashMap<String, RouteSessionSpace>,
    /// Global epoch — bumped when a clear names a key/space with no state
    /// (never pinned), mirroring [`crate::ActiveTargetsInner`]'s bounded
    /// tombstone protocol.
    epoch: u64,
}

impl SessionAffinityInner {
    /// Create an empty memory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Decision-point read (design §4.2 protocol step 1): return the live
    /// pin for `(model, map_id)` — applying the **TTL lazy-expiry** and
    /// **stale-routes_version** checks — plus the [`SessionStickyToken`]
    /// the success path must hand back to
    /// [`insert_if_generation`](Self::insert_if_generation).
    ///
    /// A hit **touches** the entry (sliding TTL refresh + LRU move); a miss
    /// due to expiry or a stale `routes_version` is lazy — the entry stays
    /// and simply reads as absent (capacity eviction is the only delete
    /// path).
    ///
    /// `ttl_secs == 0` means pins never expire.
    pub fn get_with_token(
        &mut self,
        model: &str,
        map_id: &str,
        ttl_secs: u64,
        current_routes_version: u64,
        now: Instant,
    ) -> (Option<SessionPin>, SessionStickyToken) {
        let generation = self
            .spaces
            .get(model)
            .map(|s| s.generation(map_id))
            .unwrap_or(0);
        let token = StickyToken::new(self.epoch, generation, current_routes_version);

        let Some(space) = self.spaces.get_mut(model) else {
            return (None, token);
        };
        let Some(pin) = space.map.get_mut(map_id) else {
            return (None, token);
        };
        // Config mutation after the pin → miss (not deleted).
        if pin.routes_version != current_routes_version {
            return (None, token);
        }
        // TTL lazy expiry → miss (not deleted). `0` = never expire.
        if ttl_secs > 0 && now.duration_since(pin.recorded_at) > Duration::from_secs(ttl_secs) {
            return (None, token);
        }
        // Hit: touch (sliding TTL + LRU move-to-back).
        pin.recorded_at = now;
        let cloned = pin.clone();
        space.touch(map_id);
        (Some(cloned), token)
    }

    /// Success write-back (design §4.2 protocol step 2): record `pin` only
    /// while `wb.token` still matches the current state — epoch, per-key
    /// generation, and `routes_version` must **all** agree. The compare and
    /// the insert run under the single caller-held write lock, atomic
    /// against every concurrent clear / eviction.
    pub fn insert_if_generation(
        &mut self,
        wb: &SessionWriteback,
        pin: SessionPin,
        current_routes_version: u64,
    ) -> bool {
        if wb.token.epoch != self.epoch {
            return false;
        }
        if current_routes_version != wb.token.routes_version {
            return false;
        }
        let space = self.spaces.entry(wb.model.clone()).or_default();
        if space.generation(&wb.map_id) != wb.token.generation {
            return false;
        }
        let mut epoch = self.epoch;
        space.insert(wb.map_id.clone(), pin, wb.capacity, &mut epoch);
        self.epoch = epoch;
        true
    }

    /// Clear one session key (hashed `map_id`) across all spaces. A key
    /// with no state anywhere bumps the global epoch instead (bounded
    /// tombstones).
    pub fn clear(&mut self, map_id: &str) {
        let mut found = false;
        for space in self.spaces.values_mut() {
            if space.map.contains_key(map_id) || space.generations.contains_key(map_id) {
                space.map.remove(map_id);
                space.lru.retain(|k| k != map_id);
                *space.generations.entry(map_id.to_string()).or_insert(0) += 1;
                found = true;
            }
        }
        if !found {
            self.epoch += 1;
        }
    }

    /// Clear every session pin of one request-model space. Unknown space →
    /// epoch bump (no state to tombstone).
    pub fn clear_model(&mut self, model: &str) {
        match self.spaces.get_mut(model) {
            Some(space) => space.clear_entries(),
            None => self.epoch += 1,
        }
    }

    /// Clear the whole memory. Per-key generations are bumped (tombstones
    /// kept) **and** the epoch is bumped so never-seen keys' in-flight
    /// writebacks are also rejected.
    pub fn clear_all(&mut self) {
        for space in self.spaces.values_mut() {
            space.clear_entries();
        }
        self.epoch += 1;
    }

    /// Strategy-aware prune (the session analogue of
    /// [`crate::ActiveTargetsInner::retain_sticky_routes`]): drop every pin
    /// whose request-model key no longer resolves — under the current route
    /// table — to the route that recorded it **and** whose route is still
    /// [`Strategy::SessionSticky`]. A route that switched away (or was
    /// removed / shadowed) leaves pins the current config would never write
    /// or consult. Dropped keys get their generation bumped (tombstones
    /// kept).
    pub fn retain_sticky_routes(&mut self, routes: &[RouteConfig]) {
        let resolves_to = |model: &str| sag_config::resolve_model_route(routes, model);
        for (model, space) in self.spaces.iter_mut() {
            let dropped: Vec<String> = space
                .map
                .iter()
                .filter(|(_, pin)| {
                    !matches!(
                        resolves_to(model),
                        Some(r) if r.model_name == pin.route_model && r.strategy == Strategy::SessionSticky
                    )
                })
                .map(|(k, _)| k.clone())
                .collect();
            for k in dropped {
                space.map.remove(&k);
                space.lru.retain(|x| x != &k);
                *space.generations.entry(k).or_insert(0) += 1;
            }
        }
    }

    /// Clone the whole memory for the admin snapshot (single short lock
    /// hold; DTOs computed outside).
    pub fn snapshot(&self) -> Vec<(String, SessionPin)> {
        self.spaces
            .iter()
            .flat_map(|(route_model, space)| {
                space.map.iter().map(move |(map_id, pin)| {
                    debug_assert_eq!(&pin.route_model, route_model);
                    (map_id.clone(), pin.clone())
                })
            })
            .collect()
    }

    /// Current global epoch (for tests / diagnostics).
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
}

/// Shared handle for the session-affinity memory. Shared between
/// [`AppState`](crate::AppState) and `DispatchRuntime` via the same `Arc`,
/// exactly like [`crate::ActiveTargets`].
pub type SessionAffinity = std::sync::Arc<tokio::sync::RwLock<SessionAffinityInner>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn pin_at(route: &str, at: Instant) -> SessionPin {
        SessionPin::new("openai", "gpt-4o", route, None, at, 1)
    }

    fn wb(map_id: &str, route: &str, token: StickyToken) -> SessionWriteback {
        SessionWriteback {
            map_id: map_id.into(),
            model: route.into(),
            token,
            capacity: DEFAULT_SESSION_MAX_ENTRIES,
        }
    }

    #[test]
    fn stale_writeback_after_clear_is_rejected() {
        let mut inner = SessionAffinityInner::new();
        let now = Instant::now();
        let (_, token) = inner.get_with_token("m", "k", 3600, 1, now);

        inner.clear("k"); // key never seen → epoch bump
        assert!(
            !inner.insert_if_generation(&wb("k", "m", token), pin_at("m", now), 1),
            "writeback captured before the clear must be rejected"
        );
    }

    #[test]
    fn stale_writeback_after_generation_clear_is_rejected() {
        let mut inner = SessionAffinityInner::new();
        let now = Instant::now();
        // First bind so the key has state (and a generation).
        let (_, t0) = inner.get_with_token("m", "k", 3600, 1, now);
        assert!(inner.insert_if_generation(&wb("k", "m", t0), pin_at("m", now), 1));

        // Decision, then clear (per-key generation bump), then stale write.
        let (pin, token) = inner.get_with_token("m", "k", 3600, 1, now);
        assert!(pin.is_some(), "fresh pin must read back");
        inner.clear("k");
        assert!(!inner.insert_if_generation(&wb("k", "m", token), pin_at("m", now), 1));
    }

    #[test]
    fn stale_writeback_after_routes_version_bump_is_rejected() {
        let mut inner = SessionAffinityInner::new();
        let now = Instant::now();
        let (_, token) = inner.get_with_token("m", "k", 3600, 1, now);
        assert!(!inner.insert_if_generation(&wb("k", "m", token), pin_at("m", now), 2));
    }

    #[test]
    fn ttl_lazy_expiry_reads_miss_but_keeps_entry() {
        let mut inner = SessionAffinityInner::new();
        let t0 = Instant::now();
        let (_, token) = inner.get_with_token("m", "k", 10, 1, t0);
        assert!(inner.insert_if_generation(&wb("k", "m", token), pin_at("m", t0), 1));

        // Past the TTL: miss…
        let later = t0 + Duration::from_secs(11);
        let (pin, _) = inner.get_with_token("m", "k", 10, 1, later);
        assert!(pin.is_none(), "expired pin must read as a miss");
        // …but not deleted (lazy expiry): still visible to the snapshot.
        assert_eq!(
            inner.snapshot().len(),
            1,
            "expiry must not delete the entry"
        );

        // ttl 0 = never expire.
        let (pin, _) = inner.get_with_token("m", "k", 0, 1, later);
        assert!(pin.is_some(), "ttl 0 must never expire");
    }

    #[test]
    fn ttl_slides_on_hit() {
        let mut inner = SessionAffinityInner::new();
        let t0 = Instant::now();
        let (_, token) = inner.get_with_token("m", "k", 10, 1, t0);
        assert!(inner.insert_if_generation(&wb("k", "m", token), pin_at("m", t0), 1));

        // Hit at t0+8 refreshes the clock (sliding expiration)…
        let t1 = t0 + Duration::from_secs(8);
        let (pin, _) = inner.get_with_token("m", "k", 10, 1, t1);
        assert!(pin.is_some());
        // …so t0+15 (7s after the touch, 15s after insert) is still a hit.
        let t2 = t0 + Duration::from_secs(15);
        let (pin, _) = inner.get_with_token("m", "k", 10, 1, t2);
        assert!(pin.is_some(), "hit must refresh the TTL clock");
    }

    #[test]
    fn stale_routes_version_pin_reads_miss_without_delete() {
        let mut inner = SessionAffinityInner::new();
        let now = Instant::now();
        let (_, token) = inner.get_with_token("m", "k", 3600, 1, now);
        assert!(inner.insert_if_generation(&wb("k", "m", token), pin_at("m", now), 1));

        let (pin, _) = inner.get_with_token("m", "k", 3600, 2, now);
        assert!(pin.is_none(), "pin from an older config must miss");
        assert_eq!(inner.snapshot().len(), 1);
    }

    #[test]
    fn lru_eviction_bumps_generation() {
        let mut inner = SessionAffinityInner::new();
        let now = Instant::now();
        let mut tokens = Vec::new();
        for k in ["a", "b"] {
            let (_, t) = inner.get_with_token("m", k, 3600, 1, now);
            assert!(inner.insert_if_generation(
                &SessionWriteback {
                    map_id: k.into(),
                    model: "m".into(),
                    token: t,
                    capacity: 1,
                },
                pin_at("m", now),
                1
            ));
            tokens.push(t);
        }
        // Capacity 1: inserting `b` evicted `a` (LRU) and bumped its generation.
        assert_eq!(inner.snapshot().len(), 1);
        assert!(
            !inner.insert_if_generation(
                &SessionWriteback {
                    map_id: "a".into(),
                    model: "m".into(),
                    token: tokens[0],
                    capacity: 1,
                },
                pin_at("m", now),
                1
            ),
            "a pre-eviction writeback must be rejected"
        );
    }

    #[test]
    fn capacity_one_keeps_only_the_newest() {
        let mut inner = SessionAffinityInner::new();
        let now = Instant::now();
        for k in ["a", "b", "c"] {
            let (_, t) = inner.get_with_token("m", k, 3600, 1, now);
            inner.insert_if_generation(
                &SessionWriteback {
                    map_id: k.into(),
                    model: "m".into(),
                    token: t,
                    capacity: 1,
                },
                pin_at("m", now),
                1,
            );
        }
        let snap = inner.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].0, "c");
    }

    #[test]
    fn lru_order_respects_touch() {
        let mut inner = SessionAffinityInner::new();
        let now = Instant::now();
        for k in ["a", "b"] {
            let (_, t) = inner.get_with_token("m", k, 3600, 1, now);
            inner.insert_if_generation(
                &SessionWriteback {
                    map_id: k.into(),
                    model: "m".into(),
                    token: t,
                    capacity: 2,
                },
                pin_at("m", now),
                1,
            );
        }
        // Touch `a` (hit) so `b` becomes the LRU…
        assert!(inner.get_with_token("m", "a", 3600, 1, now).0.is_some());
        // …then a capacity-2 insert of `c` evicts `b`, not `a`.
        let (_, t) = inner.get_with_token("m", "c", 3600, 1, now);
        inner.insert_if_generation(
            &SessionWriteback {
                map_id: "c".into(),
                model: "m".into(),
                token: t,
                capacity: 2,
            },
            pin_at("m", now),
            1,
        );
        let mut keys: Vec<String> = inner.snapshot().into_iter().map(|(k, _)| k).collect();
        keys.sort();
        assert_eq!(keys, vec!["a".to_string(), "c".to_string()]);
    }

    #[test]
    fn epoch_bump_rejects_never_seen_key_writeback() {
        let mut inner = SessionAffinityInner::new();
        let now = Instant::now();
        let (_, token) = inner.get_with_token("m", "never-pinned", 3600, 1, now);
        inner.clear("other-unknown"); // no state anywhere → epoch bump
        assert!(!inner.insert_if_generation(&wb("never-pinned", "m", token), pin_at("m", now), 1));
    }

    #[test]
    fn clear_model_and_clear_all_do_not_leak_across_spaces() {
        let mut inner = SessionAffinityInner::new();
        let now = Instant::now();
        for (route, key) in [("m1", "k1"), ("m2", "k2")] {
            let (_, t) = inner.get_with_token(route, key, 3600, 1, now);
            assert!(inner.insert_if_generation(&wb(key, route, t), pin_at(route, now), 1));
        }

        inner.clear_model("m1");
        assert!(inner.get_with_token("m2", "k2", 3600, 1, now).0.is_some());
        assert!(inner.get_with_token("m1", "k1", 3600, 1, now).0.is_none());

        inner.clear_all();
        assert!(inner.get_with_token("m2", "k2", 3600, 1, now).0.is_none());
    }

    #[test]
    fn retain_sticky_routes_drops_non_sticky_spaces() {
        use sag_config::RouteTarget;
        let sticky = RouteConfig {
            model_name: "sticky".into(),
            pipeline: Default::default(),
            api_endpoint: None,
            strategy: Strategy::SessionSticky,
            retries: None,
            timeout_secs: None,
            session_ttl_secs: None,
            session_max_entries: None,
            targets: vec![RouteTarget::default()],
        };
        let switched = RouteConfig {
            model_name: "switched".into(),
            strategy: Strategy::FillFirst,
            ..sticky.clone()
        };
        let mut inner = SessionAffinityInner::new();
        let now = Instant::now();
        for route in ["sticky", "switched"] {
            let (_, t) = inner.get_with_token(route, "k", 3600, 1, now);
            assert!(inner.insert_if_generation(&wb("k", route, t), pin_at(route, now), 1));
        }

        inner.retain_sticky_routes(&[sticky, switched]);
        assert!(
            inner
                .get_with_token("sticky", "k", 3600, 1, now)
                .0
                .is_some()
        );
        assert!(
            inner
                .get_with_token("switched", "k", 3600, 1, now)
                .0
                .is_none(),
            "pins of a route that switched away from session_sticky must drop"
        );
    }
}
