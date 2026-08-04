//! Shared application state for SAG.
//!
//! This crate owns [`AppState`], the cross-cutting type used as Axum
//! [`axum::extract::State`] by all handler families (proxy dispatch,
//! admin CRUD, health/analytics). It lives here — below both
//! `sag-admin` and `sag-dispatch` — so that the management layer
//! does not depend on the execution layer.
//!
//! Dispatch-only runtime state lives in `sag-dispatch` as
//! `DispatchRuntime`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use sag_analytics::CallLogCollector;
use sag_config::{ConfigHandle, RouteTarget};
use sag_core::RouteCircuitBreaker;
use sag_providers::LlmProvider;
use tokio::sync::RwLock;

pub mod config_mutation;
pub use config_mutation::{ConfigMutationCoordinator, FileRevision, MutationReport};

/// One sticky-failover active-target entry: which provider + real model
/// absorbed the last successful request for a model, plus the route that
/// resolved the request so prune and the admin snapshot are route-aware.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveTargetEntry {
    pub provider: String,
    /// Resolved real model sent upstream (the target's explicit
    /// `real_model`, or the request model for a passthrough target).
    pub real_model: String,
    /// Config route `model_name` that resolved this request (`"*"` for a
    /// wildcard route). The map key is the request model; this records
    /// which route the key belonged to when the pin was recorded, so a
    /// wildcard request key is never misattributed to — or pruned by — an
    /// unrelated route.
    pub route_model: String,
    /// The target's `real_model` spec at pin time: `None` = passthrough
    /// (effective model = request model), `Some(rm)` = explicit override.
    /// Distinct from [`Self::real_model`] so the admin snapshot can tell a
    /// passthrough pin apart from an explicit pin on the same provider.
    pub target_real_model: Option<String>,
    /// When the entry was recorded, used for the configurable expiry
    /// mechanism (`sticky_failover_reset_secs`).
    pub recorded_at: Instant,
}

impl ActiveTargetEntry {
    /// Construct an entry. Convenience constructor for call sites that
    /// already hold the five values.
    pub fn new(
        provider: impl Into<String>,
        real_model: impl Into<String>,
        route_model: impl Into<String>,
        target_real_model: Option<String>,
        recorded_at: Instant,
    ) -> Self {
        Self {
            provider: provider.into(),
            real_model: real_model.into(),
            route_model: route_model.into(),
            target_real_model,
            recorded_at,
        }
    }
}

/// Token captured at the sticky-failover decision point — the linearization
/// point where the active entry, per-model generation, and global epoch are
/// read, before any further await. The success path writes back only while
/// **all** components still match:
///
/// - `epoch` blocks writebacks from requests that decided their route before
///   a no-entry clear (a model with no per-model state is cleared by bumping
///   the global epoch instead of creating a per-model tombstone);
/// - `generation` blocks writebacks from requests that decided their route
///   before a per-model clear (DELETE / breaker close / targets-changed /
///   prune of that model);
/// - `routes_version` blocks writebacks from requests that decided before a
///   config mutation (route / provider CRUD, hot-reload).
///
/// Every model always has a generation (initialized to 0 on first access),
/// and the epoch is monotonically increasing, so an in-flight request that
/// captured a pre-clear state can never write back afterwards.
///
/// The compare and the insert in
/// [`ActiveTargetsInner::insert_if_generation`] run under the single
/// caller-held write lock, so the read-compare-write is atomic with respect
/// to every concurrent clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StickyToken {
    /// Global epoch captured at decision time. Bumped by clears of models
    /// that carry neither an entry nor a per-model generation (never pinned
    /// / never cleared) — the bounded replacement for a per-model tombstone.
    /// Monotonically increasing, so a decision captured before such a clear
    /// is rejected without ever creating a per-model generation entry.
    pub epoch: u64,
    /// Per-model generation captured at decision time.
    pub generation: u64,
    /// Config `routes_version` captured at decision time.
    pub routes_version: u64,
}

impl StickyToken {
    /// Construct a token.
    pub fn new(epoch: u64, generation: u64, routes_version: u64) -> Self {
        Self {
            epoch,
            generation,
            routes_version,
        }
    }
}

/// Sticky-failover active-target memory with per-model linearizability.
///
/// Wraps the previous `HashMap<String, (provider, real_model, recorded_at)>`
/// and adds a per-model generation counter plus a global epoch. Every
/// clearing operation bumps either the model's generation — when the model
/// has per-model state (an entry or a generation tombstone) — or the global
/// epoch — when it has none, so arbitrary clears never grow the
/// `generations` map (see [`ActiveTargetsInner::clear`]). A routing
/// decision captures both values while reading the sticky entry
/// ([`ActiveTargetsInner::generation`], [`ActiveTargetsInner::epoch`]) and
/// the success path only writes back while they still match
/// ([`ActiveTargetsInner::insert_if_generation`]). This closes the
/// clear-vs-writeback race: an in-flight request that decided its route
/// *before* a DELETE / breaker-close / targets-changed / prune can never
/// re-pin the removed entry afterwards.
///
/// The compare and the insert in
/// [`ActiveTargetsInner::insert_if_generation`] happen under the single
/// caller-held write lock, so the read-compare-write is atomic with respect
/// to every other mutation.
///
/// `Clone` lets the admin snapshot copy the whole memory under a single
/// short read lock and then compute DTOs without holding the lock.
#[derive(Debug, Clone, Default)]
pub struct ActiveTargetsInner {
    /// model → active target entry.
    pub entries: HashMap<String, ActiveTargetEntry>,
    /// model → generation counter. Bumped by every clear/removal of a model
    /// that had per-model state; entries are intentionally **not** removed
    /// here (see [`ActiveTargetsInner::retain`]) — resetting a model to the
    /// "never bumped" state would accept a stale writeback that captured
    /// exactly that state. Only models that were actually pinned (or
    /// cleared while having a tombstone) get an entry, so this map grows
    /// with distinct-model usage, never with arbitrary clear names.
    pub generations: HashMap<String, u64>,
    /// Global epoch. Bumped by [`ActiveTargetsInner::clear`] on models with
    /// no entry **and** no generation tombstone, instead of creating a
    /// per-model tombstone — this is what keeps `generations` bounded under
    /// arbitrary DELETE / breaker-close traffic. Monotonically increasing:
    /// an in-flight decision captured a lower epoch and is rejected by the
    /// epoch check, with no ABA (the epoch never decreases).
    pub epoch: u64,
}

impl ActiveTargetsInner {
    /// Create an empty memory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Current generation for `model` — the value a routing decision must
    /// capture while reading the sticky entry, then hand to
    /// [`insert_if_generation`](Self::insert_if_generation) on the success
    /// path. Every model has a generation (initialized to 0 on first
    /// access), so a clear / prune always bumps from a known base.
    pub fn generation(&self, model: &str) -> u64 {
        self.generations.get(model).copied().unwrap_or(0)
    }

    /// Current global epoch — the value a routing decision must capture
    /// alongside the per-model generation, then hand to
    /// [`insert_if_generation`](Self::insert_if_generation) on the success
    /// path, so a no-entry clear (which bumps the epoch) invalidates
    /// pre-clear decisions without a per-model tombstone.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Active target for `model`, if any.
    pub fn get(&self, model: &str) -> Option<&ActiveTargetEntry> {
        self.entries.get(model)
    }

    /// Whether `model` currently has an active target.
    pub fn contains_key(&self, model: &str) -> bool {
        self.entries.contains_key(model)
    }

    /// Iterate `(model, entry)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &ActiveTargetEntry)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Number of active entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no active entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Unconditionally record `entry` for `model`, overwriting any previous
    /// entry. Does **not** touch the generation counter — use
    /// [`insert_if_generation`](Self::insert_if_generation) from the
    /// dispatch success path so clears are respected. Kept for tests and
    /// call sites that deliberately force a pin.
    pub fn insert(&mut self, model: String, entry: ActiveTargetEntry) {
        self.entries.insert(model, entry);
    }

    /// Remove `model`'s entry (if any) and invalidate decisions that
    /// captured the pre-clear state, so an in-flight request that decided
    /// its route before this clear can never write back afterwards.
    ///
    /// Two different invalidation mechanisms keep the `generations` map
    /// bounded:
    /// - the model has per-model state (an entry or an existing generation
    ///   tombstone) → bump its per-model generation. Tombstones are only
    ///   ever created for models that were actually pinned, so this grows
    ///   with distinct-model usage, never with arbitrary clear names;
    /// - the model has neither → bump the global [`Self::epoch`] instead of
    ///   creating a permanent per-model tombstone. The epoch check in
    ///   [`insert_if_generation`](Self::insert_if_generation) then rejects
    ///   every decision captured before this clear, across all models.
    ///
    /// Used by `DELETE /api/sticky-targets`, manual circuit-breaker close,
    /// and the targets-changed invalidation in `order_targets`.
    pub fn clear(&mut self, model: &str) {
        if self.entries.contains_key(model) || self.generations.contains_key(model) {
            self.entries.remove(model);
            let generation = self.generations.entry(model.to_string()).or_insert(0);
            *generation += 1;
        } else {
            self.epoch += 1;
        }
    }

    /// Record `entry` for `model` only while `expected` still matches the
    /// current state; returns `true` when the entry was recorded. The caller
    /// must hold the write lock for the whole read-compare-write so it is
    /// atomic against concurrent clears.
    ///
    /// Three conditions must hold, mirroring the three components of
    /// [`StickyToken`]:
    /// - `expected.epoch` must equal the current global epoch — a no-entry
    ///   clear (bumping the epoch instead of a tombstone) after the decision
    ///   invalidates the writeback;
    /// - `current_routes_version` must equal `expected.routes_version` — a
    ///   config mutation after the decision invalidates the writeback;
    /// - `expected.generation` must equal the model's current generation — a
    ///   per-model clear (DELETE, breaker close, targets-changed, prune)
    ///   after the decision invalidates the writeback.
    pub fn insert_if_generation(
        &mut self,
        model: String,
        entry: ActiveTargetEntry,
        expected: &StickyToken,
        current_routes_version: u64,
    ) -> bool {
        if expected.epoch != self.epoch {
            return false;
        }
        if current_routes_version != expected.routes_version {
            return false;
        }
        if self.generation(&model) != expected.generation {
            return false;
        }
        self.entries.insert(model, entry);
        true
    }

    /// Remove entries whose `(model, entry)` does not satisfy `keep`, bumping
    /// the generation of every dropped model. `keep` receives the model name
    /// and its entry — `None` for models that only carry a generation (no
    /// entry) — e.g. a `HashSet<&str>` of current route model names.
    ///
    /// Generations are deliberately **not** removed for dropped models: a
    /// dropped model would otherwise reset to the "never bumped" state,
    /// which is indistinguishable from the state an in-flight decision may
    /// have captured, reopening the stale-writeback window. Bumping instead
    /// guarantees any pre-prune decision fails its conditional write.
    pub fn retain(&mut self, mut keep: impl FnMut(&str, Option<&ActiveTargetEntry>) -> bool) {
        let dropped: std::collections::HashSet<String> = self
            .entries
            .keys()
            .chain(self.generations.keys())
            .filter(|model| !keep(model, self.entries.get(*model)))
            .cloned()
            .collect();
        for model in dropped {
            self.entries.remove(&model);
            let generation = self.generations.entry(model).or_insert(0);
            *generation += 1;
        }
    }
}

/// Shared handle for the sticky-failover active-target memory. Shared
/// between [`AppState`] and `DispatchRuntime` via the same `Arc` so admin
/// mutations (e.g. clearing sticky memory when a breaker is manually
/// closed) are immediately visible to dispatch ordering.
pub type ActiveTargets = Arc<RwLock<ActiveTargetsInner>>;

/// A cached shuffled target order for the fill-first strategy, with its
/// creation timestamp and the config `routes_version` that produced it.
///
/// Lived in `sag-dispatch` until the admin layer needed to observe and
/// reset fill-first routing through the shared `AppState`; moved here (with
/// `FillFirstOrders`) so the admin snapshot can report the current
/// fill-first order and a manual reset can drop it without `sag-admin`
/// depending on the dispatch crate. `routes_version` is `None` for legacy /
/// test-constructed entries — treated as "no version", i.e. any request may
/// reuse or overwrite them.
#[derive(Debug, Clone)]
pub struct CachedOrder {
    /// The shuffled target list in decision order; index 0 absorbs all
    /// traffic until unavailable (fill-first), or is the base order the
    /// sticky pin is prepended to (sticky-failover).
    pub targets: Arc<[RouteTarget]>,
    /// When this order was (re-)shuffled, used for the `shuffle_cache_ttl_secs`
    /// expiry.
    pub created_at: Instant,
    /// Config `routes_version` that produced this cache entry.
    pub routes_version: Option<u64>,
}

/// Shared handle for the fill-first shuffled-order cache. Shared between
/// [`AppState`] and `DispatchRuntime` via the same `Arc` so admin handlers
/// can observe the current fill-first order and reset it (manual
/// re-shuffle) — the fill-first analogue of clearing `active_targets` for
/// sticky-failover.
pub type FillFirstOrders = Arc<RwLock<HashMap<String, CachedOrder>>>;

/// Shared application state for all Axum handlers.
///
/// Holds only the cross-cutting resources that every handler family
/// (proxy dispatch, admin CRUD, health/analytics) needs: the hot-reloadable
/// config, the provider registry, the analytics collector, and the shared
/// circuit-breaker handle. The circuit breaker is shared with
/// [`sag_dispatch::DispatchRuntime`] via the same `Arc` so admin handlers
/// can observe live breaker state without depending on the dispatch crate.
#[derive(Clone)]
pub struct AppState {
    /// Hot-reloadable gateway configuration.
    pub config: ConfigHandle,
    /// Map of provider name → initialized provider adapter.
    pub providers: Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>>,
    /// Config mutation coordinator: serialises all production writes to
    /// config + provider registry + routes_version.
    pub coordinator: Arc<ConfigMutationCoordinator>,
    /// Async call log collector for analytics.
    pub collector: Arc<CallLogCollector>,
    /// Shared route-target circuit breaker. The same `Arc` is injected into
    /// `DispatchRuntime` at startup, so this handle observes live state.
    pub circuit_breaker: Arc<RwLock<RouteCircuitBreaker>>,
    /// Sticky-failover active-target memory, shared with `DispatchRuntime`
    /// via the same `Arc`. Admin handlers clear an entry here (bumping its
    /// generation) to reset sticky routing after a manual breaker close;
    /// dispatch reads it in `order_targets` to prepend the active target,
    /// and writes the active target on a successful response only while the
    /// generation captured at decision time still matches.
    pub active_targets: ActiveTargets,
    /// Fill-first shuffled-order cache, shared with `DispatchRuntime` via
    /// the same `Arc`. Admin handlers read it for the sticky-target
    /// snapshot (which target currently absorbs all fill-first traffic) and
    /// clear it on DELETE for a fill-first route — dropping the cached
    /// order so the next request re-shuffles, the fill-first analogue of
    /// clearing `active_targets`.
    pub fill_first_orders: FillFirstOrders,
}

impl AppState {
    /// Look up a provider by name from the registry.
    pub async fn get_provider(&self, name: &str) -> Option<Arc<dyn LlmProvider>> {
        let guard = self.providers.read().await;
        guard.get(name).cloned()
    }

    /// Create or update a provider runtime adapter from the given config and
    /// insert it into the live registry.
    ///
    /// The caller should pass the up-to-date [`ProviderConfig`] that was just
    /// written to `config.providers`; this avoids an extra config read lock.
    pub async fn upsert_provider(
        &self,
        name: &str,
        config: &sag_config::ProviderConfig,
    ) -> sag_core::Result<()> {
        match sag_providers::create_provider(name.to_string(), config) {
            Ok(provider) => {
                let mut providers = self.providers.write().await;
                providers.insert(name.to_string(), provider);
                Ok(())
            }
            Err(e) => {
                tracing::error!(provider = name, error = %e, "failed to create provider adapter");
                Err(e)
            }
        }
    }

    /// Remove a provider from the runtime adapter registry.
    ///
    /// Call after removing the provider from config; in-flight requests that
    /// have already snapshotted the `Arc<dyn LlmProvider>` will continue to
    /// use it, but new lookups will see the absence.
    pub async fn remove_provider_runtime(&self, name: &str) {
        let mut providers = self.providers.write().await;
        providers.remove(name);
    }
}

// ── Testing helpers (not #[cfg(test)] so integration tests can reach them) ──

/// Shared test helpers for constructing [`AppState`].
pub mod testing {
    use std::collections::HashMap;
    use std::sync::Arc;

    use sag_config::ConfigHandle;
    use tokio::sync::RwLock;

    use super::AppState;
    use super::ConfigMutationCoordinator;

    /// Build an [`AppState`] with the given config and provider registry,
    /// backed by an in-memory analytics collector.
    pub async fn test_app_state(
        config: ConfigHandle,
        providers: Arc<RwLock<HashMap<String, Arc<dyn sag_providers::LlmProvider>>>>,
    ) -> AppState {
        let coordinator = Arc::new(ConfigMutationCoordinator::new(
            config.clone(),
            providers.clone(),
        ));
        AppState {
            config,
            providers,
            coordinator,
            collector: Arc::new(sag_analytics::CallLogCollector::new_memory().await.unwrap()),
            circuit_breaker: Arc::new(RwLock::new(sag_core::RouteCircuitBreaker::default())),
            active_targets: Arc::default(),
            fill_first_orders: Arc::default(),
        }
    }

    /// Build a provider registry from the given config handle, optionally
    /// filtering to only enabled providers. Returns the registry wrapped in
    /// an `Arc<RwLock<...>>` ready for [`test_app_state`].
    pub async fn build_provider_registry(
        handle: &ConfigHandle,
        only_enabled: bool,
    ) -> Arc<RwLock<HashMap<String, Arc<dyn sag_providers::LlmProvider>>>> {
        let cfg_read = handle.read().await;
        let mut registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
        for (name, pc) in cfg_read.providers.iter() {
            if !only_enabled || pc.enabled {
                let provider: Arc<dyn sag_providers::LlmProvider> =
                    Arc::new(sag_providers::OpenAiProvider::new(name.clone(), pc).unwrap());
                registry.insert(name.clone(), provider);
            }
        }
        drop(cfg_read);
        Arc::new(RwLock::new(registry))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(provider: &str, real_model: &str) -> ActiveTargetEntry {
        ActiveTargetEntry::new(
            provider.to_string(),
            real_model.to_string(),
            real_model.to_string(),
            Some(real_model.to_string()),
            Instant::now(),
        )
    }

    fn token(epoch: u64, generation: u64, routes_version: u64) -> StickyToken {
        StickyToken::new(epoch, generation, routes_version)
    }

    #[test]
    fn clear_bumps_generation_for_models_with_per_model_state() {
        let mut inner = ActiveTargetsInner::new();
        inner.insert("m".into(), entry("a", "m"));
        assert_eq!(inner.generation("m"), 0);
        assert_eq!(inner.epoch(), 0);

        // A model with an entry: clear removes it and bumps the per-model
        // generation (a tombstone — the model was actually used).
        inner.clear("m");
        assert!(inner.get("m").is_none(), "clear must remove the entry");
        assert_eq!(inner.generation("m"), 1);
        assert_eq!(
            inner.epoch(),
            0,
            "entry-bearing clear must not touch the epoch"
        );

        // A model that only carries a tombstone keeps bumping per-model.
        inner.clear("m");
        assert_eq!(inner.generation("m"), 2);
        assert_eq!(inner.epoch(), 0);
    }

    #[test]
    fn clear_without_entry_bumps_epoch_and_creates_no_tombstone() {
        let mut inner = ActiveTargetsInner::new();

        // A model that was never pinned and never cleared: clearing it must
        // still reject pre-clear decisions, but via the global epoch rather
        // than a permanent per-model tombstone.
        inner.clear("never-touched");
        assert_eq!(inner.epoch(), 1);
        assert_eq!(
            inner.generation("never-touched"),
            0,
            "no per-model tombstone may be created"
        );
        assert_eq!(
            inner.generations.len(),
            0,
            "arbitrary clears must not grow the generations map"
        );

        // Cross-model: the epoch bump rejects a decision captured before the
        // clear for a *different* (also never-touched) model — even though
        // its per-model generation is unchanged.
        assert!(
            !inner.insert_if_generation("other".into(), entry("a", "other"), &token(0, 0, 7), 7,),
            "pre-clear epoch must reject the writeback"
        );
        // Post-clear decisions capture the new epoch and record.
        assert!(
            inner.insert_if_generation("other".into(), entry("a", "other"), &token(1, 0, 7), 7,),
            "post-clear epoch still records"
        );

        // Repeated arbitrary clears keep bumping the epoch, never the map.
        inner.clear("never-touched");
        inner.clear("still-never-pinned");
        assert_eq!(inner.epoch(), 3);
        assert_eq!(inner.generations.len(), 0, "generations stays empty");
    }

    #[test]
    fn insert_if_generation_records_only_when_token_unchanged() {
        let mut inner = ActiveTargetsInner::new();
        let v = 7; // routes_version captured at decision time

        // Fresh memory: the decision captured (epoch 0, generation 0) — both
        // still current → the writeback records.
        assert!(inner.insert_if_generation("m".into(), entry("a", "m"), &token(0, 0, v), v));
        assert_eq!(inner.get("m").unwrap().provider, "a");

        // Another in-flight request from the same state (never cleared) may
        // also write — legitimate, not stale.
        assert!(inner.insert_if_generation("m".into(), entry("b", "m"), &token(0, 0, v), v));
        assert_eq!(inner.get("m").unwrap().provider, "b");

        // A clear bumps the generation; pre-clear decisions (captured 0)
        // must all fail.
        inner.clear("m");
        assert!(!inner.insert_if_generation("m".into(), entry("c", "m"), &token(0, 0, v), v));
        assert!(!inner.insert_if_generation("m".into(), entry("d", "m"), &token(0, 0, v), v));
        assert!(inner.get("m").is_none(), "stale writebacks must not pin");

        // A decision capturing the current generation still records.
        assert!(inner.insert_if_generation("m".into(), entry("e", "m"), &token(0, 1, v), v));
        assert_eq!(inner.get("m").unwrap().provider, "e");
    }

    #[test]
    fn insert_if_generation_rejects_stale_routes_version() {
        let mut inner = ActiveTargetsInner::new();
        let v = 7;

        // A model that NEVER had an entry or generation: the only way a
        // config change invalidates it is the routes_version component of
        // the token (there is no per-model generation to compare against).
        assert!(
            inner.insert_if_generation("m".into(), entry("a", "m"), &token(0, 0, v), v),
            "matching epoch and routes_version records even without a per-model generation"
        );

        // Config mutation moves the epoch; a decision captured before it
        // (same generation 0, older routes_version) must be rejected.
        assert!(
            !inner.insert_if_generation("m".into(), entry("b", "m"), &token(0, 0, v), v + 1),
            "stale routes_version must reject the writeback"
        );
        assert!(
            !inner.insert_if_generation("m".into(), entry("c", "m"), &token(0, 1, v), v + 1),
            "stale routes_version rejects even with a matching generation"
        );

        // A decision that captured the post-mutation routes_version records.
        assert!(inner.insert_if_generation(
            "m".into(),
            entry("d", "m"),
            &token(0, 0, v + 1),
            v + 1
        ));
        assert_eq!(inner.get("m").unwrap().provider, "d");
    }

    #[test]
    fn retain_drops_invalid_models_and_bumps_their_generations() {
        let mut inner = ActiveTargetsInner::new();
        inner.insert("keep".into(), entry("a", "keep"));
        inner.insert("drop".into(), entry("b", "drop"));
        // A tombstone without an entry — the state a model is left in after
        // it was pinned and then cleared. Simulated directly because
        // `clear` on a never-touched model now bumps the epoch instead.
        inner.generations.insert("cleared-only".into(), 1);

        inner.retain(|model, _| model == "keep");

        assert!(inner.contains_key("keep"));
        assert!(!inner.contains_key("drop"));

        // Dropped models' generations were bumped so pre-prune decisions
        // (whatever they captured) fail their conditional writeback.
        assert_eq!(inner.generation("drop"), 1);
        assert_eq!(inner.generation("cleared-only"), 2);
        assert!(!inner.insert_if_generation("drop".into(), entry("c", "drop"), &token(0, 0, 1), 1));
        assert!(!inner.insert_if_generation("drop".into(), entry("c", "drop"), &token(0, 0, 1), 1));

        // Retained models keep their generation (never-bumped → 0).
        assert_eq!(inner.generation("keep"), 0);
        assert!(inner.insert_if_generation("keep".into(), entry("d", "keep"), &token(0, 0, 1), 1));
        assert_eq!(inner.get("keep").unwrap().provider, "d");
    }

    #[test]
    fn retain_passes_entry_for_route_aware_keep() {
        let mut inner = ActiveTargetsInner::new();
        // Entry pinned by an exact route.
        inner.insert(
            "gpt-4o".into(),
            ActiveTargetEntry::new(
                "openai",
                "gpt-4o",
                "gpt-4o",
                Some("gpt-4o".into()),
                Instant::now(),
            ),
        );
        // Entry pinned by the wildcard route.
        inner.insert(
            "gpt-4.1".into(),
            ActiveTargetEntry::new("openai", "gpt-4.1", "*", None, Instant::now()),
        );

        // Route-aware keep: only entries whose recorded route_model matches
        // what the (simulated) config would resolve them to.
        inner.retain(|model, entry| match entry {
            Some(e) => e.route_model == "*" && model == "gpt-4.1",
            None => false,
        });

        assert!(
            !inner.contains_key("gpt-4o"),
            "exact-route entry must be dropped"
        );
        assert!(
            inner.contains_key("gpt-4.1"),
            "wildcard-route entry must be kept"
        );
    }

    #[test]
    fn iter_and_len_reflect_entries_only() {
        let mut inner = ActiveTargetsInner::new();
        inner.insert("a".into(), entry("p1", "a"));
        inner.insert("b".into(), entry("p2", "b"));
        inner.clear("c"); // generation only — not an entry
        assert_eq!(inner.len(), 2);
        assert!(!inner.is_empty());

        let mut models: Vec<&str> = inner.iter().map(|(m, _)| m).collect();
        models.sort_unstable();
        assert_eq!(models, vec!["a", "b"]);
        assert!(inner.contains_key("a"));
        assert!(!inner.contains_key("c"));
    }
}
