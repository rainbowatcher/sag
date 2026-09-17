use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sag_analytics::{IdentityRegistry, RouteIdentity, TargetIdentity};
use sag_config::{ConfigHandle, GatewayConfig, ProviderConfig, RouteConfig};
use sag_core::AppError;
use sag_providers::LlmProvider;
use tokio::sync::RwLock;
use tracing::warn;

type ProviderRegistry = Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>>;

/// File content fingerprint for external-edit conflict detection.
///
/// Uses `std::hash::DefaultHasher` (non-cryptographic) over the full file
/// bytes, paired with the byte length. This is sufficient to detect whether
/// the file has been modified by an external editor between the time we
/// last accepted it and the time we attempt to persist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRevision {
    content_hash: u64,
    len: u64,
}

impl FileRevision {
    /// Compute a revision from the file at `path` (synchronous read;
    /// config files are small).
    fn from_path(path: &Path) -> Result<Self, AppError> {
        let bytes = std::fs::read(path).map_err(|e| {
            AppError::Config(format!("failed to read config file for revision: {e}"))
        })?;
        Ok(Self::from_bytes(&bytes))
    }

    /// Compute a revision from in-memory bytes.
    fn from_bytes(bytes: &[u8]) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut hasher);
        FileRevision {
            content_hash: hasher.finish(),
            len: bytes.len() as u64,
        }
    }
}

/// Summary of a successful mutation (no sensitive fields).
#[derive(Debug, Default)]
pub struct MutationReport {
    pub providers_added: Vec<String>,
    pub providers_rebuilt: Vec<String>,
    pub providers_removed: Vec<String>,
    pub routes_changed: usize,
    pub no_op: bool,
}

/// Pre-built adapter diff produced before acquiring any RwLock write guard.
struct PreparedProviderSync {
    to_remove: Vec<String>,
    to_insert: Vec<(String, Arc<dyn LlmProvider>)>,
    added: Vec<String>,
    rebuilt: Vec<String>,
    removed: Vec<String>,
}

impl std::fmt::Debug for PreparedProviderSync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedProviderSync")
            .field("to_remove", &self.to_remove)
            .field("to_insert_count", &self.to_insert.len())
            .field("added", &self.added)
            .field("rebuilt", &self.rebuilt)
            .field("removed", &self.removed)
            .finish()
    }
}

/// Resolve the effective proxy a provider adapter must be built with:
/// `config`'s normalized global `proxy_url` combined with `pc.proxy_mode`.
///
/// This is the single resolution used by **every** production
/// provider-construction path (gateway startup, coordinator
/// mutations/reloads, [`AppState::upsert_provider`](crate::AppState)), so
/// the proxy a provider actually uses never diverges between paths. Errors
/// (malformed global URL, `enabled` mode with no global proxy) map to
/// [`AppError::Config`] and are surfaced per each caller's error handling
/// — never unwrapped/panicked.
pub fn effective_proxy_for(
    config: &GatewayConfig,
    pc: &ProviderConfig,
) -> Result<Option<sag_config::ProxyConfig>, AppError> {
    let global = config.normalized_global_proxy().map_err(AppError::Config)?;
    sag_config::resolve_effective_proxy(pc.proxy_mode, global.as_ref()).map_err(AppError::Config)
}

/// Compare only the fields that `ProviderBase::new()` captures, so that
/// purely scheduling/display changes (`enabled`, `enabled_models`,
/// `models_endpoint`, `model_overrides`) do not trigger an adapter rebuild.
fn adapter_config_changed(old: &ProviderConfig, new: &ProviderConfig) -> bool {
    old.provider_type != new.provider_type
        || old.api_keys != new.api_keys
        || old.key_strategy != new.key_strategy
        || old.base_url != new.base_url
        || old.timeout_secs != new.timeout_secs
        || old.chunk_timeout_secs != new.chunk_timeout_secs
        || old.ttfb_timeout_secs != new.ttfb_timeout_secs
        || old.headers != new.headers
        || old.body_overrides != new.body_overrides
        || old.strip_fields != new.strip_fields
        || old.model_strip_fields != new.model_strip_fields
        || old.role_mappings != new.role_mappings
        || old.anthropic_cache_control != new.anthropic_cache_control
}

/// Count routes whose model_name differs between old and new configs
/// (symmetric difference of model-name sets).
fn compute_routes_changed(old: &GatewayConfig, new: &GatewayConfig) -> usize {
    let old_names: HashSet<&str> = old.routes.iter().map(|r| r.model_name.as_str()).collect();
    let new_names: HashSet<&str> = new.routes.iter().map(|r| r.model_name.as_str()).collect();
    old_names.symmetric_difference(&new_names).count()
}

/// Unified coordinator that serialises all production `GatewayConfig` writes
/// (hot-reload + Admin CRUD) through a single `tokio::sync::Mutex`.
///
/// ## Lock-order invariant
///
/// Every mutation follows: `mutation_lock` → `config.write()` (the config
/// commit **and** `bump_routes_version` are atomic inside this one guard) →
/// `providers.write()`. Adapter construction and file I/O happen inside
/// `mutation_lock` but **outside** all `RwLock` write guards. This order is
/// encapsulated by the coordinator; callers must not acquire
/// `config.write()` or `providers.write()` directly.
pub struct ConfigMutationCoordinator {
    config: ConfigHandle,
    providers: ProviderRegistry,
    mutation_lock: tokio::sync::Mutex<()>,
    /// Last accepted file revision; `None` for in-memory configs.
    /// Guarded by `mutation_lock` -- accessed only within its scope,
    /// so a `std::sync::Mutex` suffices for interior mutability.
    /// In `reload_from_path` this is a pure assignment. In `mutate` the
    /// guard spans conflict-check and atomic write (`FileRevision::from_path`
    /// / `save_config_to_file`); those return `Result` and are not expected
    /// to panic, and no user callbacks run while the lock is held. Poison
    /// is treated as a prior bug and surfaced as `AppError::Internal`.
    accepted_file_revision: std::sync::Mutex<Option<FileRevision>>,
    /// Optional analytics identity registry. When attached, every committed
    /// mutation that actually changes the routes list (route
    /// create/update/delete, provider-delete cascade, hot reload) reconciles
    /// the DB identities with the committed routes; a failed sync leaves a
    /// pending retry (see [`Self::pending_identity_sync`]) so the next
    /// mutation or reload retries even without a route change.
    /// Provider-only mutations (add/update) and registry-free coordinators
    /// never sync unless a retry is pending.
    identity_registry: Option<Arc<IdentityRegistry>>,
    /// Set when a committed config's identity sync failed: the config is
    /// live but the DB identities may be stale. While set, every subsequent
    /// mutation/reload commit retries the sync even when routes are
    /// unchanged (otherwise the stale identities could persist forever if
    /// no route change ever happens again). Cleared on the first successful
    /// sync. Only touched inside `mutation_lock`; an atomic suffices for
    /// interior mutability.
    pending_identity_sync: AtomicBool,
}

impl ConfigMutationCoordinator {
    /// Create a new coordinator.
    ///
    /// If `config` is file-backed, the initial file revision is computed
    /// eagerly (best-effort -- if the file is unreadable, falls back to
    /// `None` so mutations can still proceed with conflict detection
    /// disabled until the first successful persist).
    pub fn new(config: ConfigHandle, providers: ProviderRegistry) -> Self {
        let accepted = config.path().and_then(|p| FileRevision::from_path(p).ok());
        ConfigMutationCoordinator {
            config,
            providers,
            mutation_lock: tokio::sync::Mutex::new(()),
            accepted_file_revision: std::sync::Mutex::new(accepted),
            identity_registry: None,
            pending_identity_sync: AtomicBool::new(false),
        }
    }

    /// Attach an analytics [`IdentityRegistry`] (optional).
    ///
    /// When attached, each committed mutation that changes the routes list
    /// reconciles the registry with the live routes (see
    /// [`Self::sync_identity_if_routes_changed`]). A failed sync surfaces as
    /// an explicit half-commit error and leaves a pending retry: the next
    /// mutation or reload — even a provider-only one — retries the sync.
    /// Existing `new` call sites stay registry-free and behave exactly as
    /// before.
    pub fn with_identity_registry(mut self, registry: Arc<IdentityRegistry>) -> Self {
        self.identity_registry = Some(registry);
        self
    }

    /// Unified mutation protocol entry point.
    ///
    /// The closure `f` runs inside `mutation_lock`, **outside** all
    /// `RwLock` guards. The coordinator handles adapter pre-building,
    /// optional persistence, and atomic config/registry/version commit.
    ///
    /// When `persist` is `true` and the config is file-backed, the method
    /// performs external-edit conflict detection before writing and
    /// updates the accepted file revision on success.
    async fn mutate<F>(&self, persist: bool, f: F) -> Result<MutationReport, AppError>
    where
        F: FnOnce(&mut GatewayConfig) -> Result<(), AppError>,
    {
        // Step 1: Acquire mutation lock (serialises all writers).
        let _guard = self.mutation_lock.lock().await;

        // Step 2: Read current config, clone candidate and old_config.
        let current = self.config.read().await;
        let mut candidate = current.clone();
        let old_config = current.clone();
        drop(current);

        // Step 3: Apply the caller's mutation to the candidate.
        f(&mut candidate)?;

        // Step 4: Pre-build adapters (all RwLock guards are dropped).
        let sync = self.prepare_provider_sync(&old_config, &candidate)?;

        // Step 5: Persist (if requested and file-backed).
        if persist && let Some(path) = self.config.path() {
            let mut accepted_guard = self.accepted_file_revision.lock().map_err(|e| {
                AppError::Internal(format!("accepted_file_revision mutex poisoned: {e}"))
            })?;
            // Conflict detection: file must still match our accepted revision.
            let current_rev = FileRevision::from_path(path)?;
            if let Some(ref accepted_rev) = *accepted_guard
                && &current_rev != accepted_rev
            {
                return Err(AppError::Conflict(
                    "config file changed outside SAG; reload and retry".into(),
                ));
            }
            // Atomic write.
            sag_config::save_config_to_file(&candidate, path)?;
            // Update accepted revision to the newly written file's revision.
            let new_rev = FileRevision::from_path(path)?;
            *accepted_guard = Some(new_rev);
            // accepted_guard dropped here.
        }

        // Pre-compute routes_changed before candidate is consumed.
        let routes_changed = compute_routes_changed(&old_config, &candidate);

        // Step 6: Commit -- config, then providers. The `routes_version`
        // bump happens *inside* the config write guard so the new config
        // and the bumped version are atomic w.r.t. readers: a dispatch
        // writeback holding `config.read()` across its version read +
        // `active_targets` write can never observe the new config with the
        // old version (or vice versa) — it either linearizes before this
        // mutation (the mutation waits for the read guard) or is rejected
        // on a version mismatch.
        {
            let mut config_guard = self.config.write().await;
            *config_guard = candidate;
            self.config.bump_routes_version();
        }
        {
            let mut providers_guard = self.providers.write().await;
            for name in &sync.to_remove {
                providers_guard.remove(name);
            }
            for (name, provider) in &sync.to_insert {
                providers_guard.insert(name.clone(), provider.clone());
            }
        }

        // Step 6b: Reconcile analytics identities with the committed routes
        // when the routes list actually changed (route create/update/delete,
        // provider-delete cascade) — or when a previous sync failed and left
        // a pending retry (a provider-only mutation then retries it). Runs
        // inside `mutation_lock` but outside all RwLock guards, snapshotting
        // the committed config (see `sync_identity_if_routes_changed` for the
        // lock-order rationale and the explicit half-commit error contract).
        let committed = self.config.read().await;
        self.sync_identity_if_routes_changed("mutation", &old_config, &committed)
            .await?;
        drop(committed);

        // Step 7: Build report.
        let no_op = sync.added.is_empty()
            && sync.rebuilt.is_empty()
            && sync.removed.is_empty()
            && routes_changed == 0;

        Ok(MutationReport {
            providers_added: sync.added,
            providers_rebuilt: sync.rebuilt,
            providers_removed: sync.removed,
            routes_changed,
            no_op,
        })
    }

    /// Compute the diff between the provider sets of the `old` and `new` full
    /// gateway configs and pre-build any new or changed adapters. Returns a
    /// `PreparedProviderSync` ready for commit.
    ///
    /// A provider is (re)built when:
    /// - it is new in `new`;
    /// - any adapter-captured field changed ([`adapter_config_changed`]);
    /// - its `proxy_mode` changed; or
    /// - its **effective** proxy changed — the resolved combination of the
    ///   gateway-level `proxy_url` and its `proxy_mode`. Comparing resolved
    ///   effective proxies (rather than the raw `proxy_url`) is what makes a
    ///   top-level proxy change rebuild only the providers it actually
    ///   affects: `inherit`/`enabled` providers whose effective proxy moved
    ///   are rebuilt, while `disabled` providers (whose effective proxy is
    ///   always `None`) are left untouched.
    ///
    /// Adapter construction / proxy-resolution failures (e.g. empty
    /// `api_keys`, `enabled` mode without a global proxy, malformed global
    /// URL) are propagated immediately, preventing any partial commit.
    fn prepare_provider_sync(
        &self,
        old: &GatewayConfig,
        new: &GatewayConfig,
    ) -> Result<PreparedProviderSync, AppError> {
        let mut to_remove: Vec<String> = Vec::new();
        let mut to_insert: Vec<(String, Arc<dyn LlmProvider>)> = Vec::new();
        let mut added: Vec<String> = Vec::new();
        let mut rebuilt: Vec<String> = Vec::new();
        let mut removed: Vec<String> = Vec::new();

        // Providers that existed in old but are gone from new -> remove.
        for name in old.providers.keys() {
            if !new.providers.contains_key(name) {
                to_remove.push(name.clone());
                removed.push(name.clone());
            }
        }

        // Providers in the new config.
        for (name, new_pc) in new.providers.iter() {
            match old.providers.get(name) {
                None => {
                    // New provider: resolve its effective proxy under the new
                    // config and build the adapter with it.
                    let effective = effective_proxy_for(new, new_pc)?;
                    let provider = sag_providers::create_provider_with_proxy(
                        name.clone(),
                        new_pc,
                        effective.as_ref(),
                    )?;
                    to_insert.push((name.clone(), provider));
                    added.push(name.clone());
                }
                Some(old_pc) => {
                    let adapter_changed = adapter_config_changed(old_pc, new_pc);
                    let mode_changed = old_pc.proxy_mode != new_pc.proxy_mode;
                    // Compare the effective proxies each side resolves to (old
                    // config + old mode vs new config + new mode).
                    let old_effective = effective_proxy_for(old, old_pc)?;
                    let new_effective = effective_proxy_for(new, new_pc)?;
                    if adapter_changed || mode_changed || old_effective != new_effective {
                        // Rebuild with the new effective proxy.
                        let provider = sag_providers::create_provider_with_proxy(
                            name.clone(),
                            new_pc,
                            new_effective.as_ref(),
                        )?;
                        to_insert.push((name.clone(), provider));
                        rebuilt.push(name.clone());
                    }
                    // else: no adapter change needed.
                }
            }
        }

        Ok(PreparedProviderSync {
            to_remove,
            to_insert,
            added,
            rebuilt,
            removed,
        })
    }

    /// Snapshot the committed routes as analytics route identities, exactly
    /// as the identity registry expects them: `route_name` is the immutable
    /// `model_name`; passthrough targets (`real_model = None`) are carried
    /// through and skipped by the registry's static registration.
    fn snapshot_routes_identity(config: &GatewayConfig) -> Vec<RouteIdentity> {
        config
            .routes
            .iter()
            .map(|route| RouteIdentity {
                route_name: route.model_name.clone(),
                targets: route
                    .targets
                    .iter()
                    .map(|t| TargetIdentity {
                        provider: t.provider.clone(),
                        real_model: t.real_model.clone(),
                    })
                    .collect(),
            })
            .collect()
    }

    /// Reconcile the analytics identity registry with the committed routes.
    ///
    /// **Call discipline**: invoked only *after* the config commit (persist +
    /// in-memory commit + `routes_version` bump), while still holding
    /// `mutation_lock` and outside all `RwLock` guards. The snapshot is
    /// taken from the committed handle, so it always reflects exactly the
    /// config that is now live. `what` labels the caller ("mutation" /
    /// "reload") in the error message.
    ///
    /// On success the pending flag is cleared; on failure it is set and an
    /// explicit half-commit error is returned (the config stays applied, the
    /// error is never silently swallowed). The pending flag makes the very
    /// next mutation or reload retry the sync even when routes did not
    /// change in that event — see
    /// [`Self::sync_identity_if_routes_changed`].
    ///
    /// **Lock trade-off** (deliberate): the DB sync runs inside
    /// `mutation_lock`, serialising concurrent config mutations for the
    /// duration of the reconciliation (a local-SQLite transaction,
    /// millisecond-scale; config mutations are rare admin/hot-reload
    /// events). Syncing *outside* the lock would let a later mutation
    /// commit between our commit and our sync, and its own sync could then
    /// finish first — leaving the DB reconciled against a stale config
    /// with no guaranteed convergence. Serialising is therefore the minimal
    /// safe scheme.
    async fn sync_identity(&self, what: &str, committed: &GatewayConfig) -> Result<(), AppError> {
        let Some(registry) = &self.identity_registry else {
            return Ok(());
        };
        let snapshot = Self::snapshot_routes_identity(committed);
        match registry.sync(&snapshot).await {
            Ok(()) => {
                self.pending_identity_sync.store(false, Ordering::Release);
                Ok(())
            }
            Err(e) => {
                self.pending_identity_sync.store(true, Ordering::Release);
                Err(AppError::Internal(format!(
                    "config {what} applied, but analytics identity sync failed: {e}"
                )))
            }
        }
    }

    /// Reconcile the analytics identity registry with the committed routes,
    /// but only when the routes list actually changed **or** a previous
    /// sync is pending. Route create/update/delete, the provider-delete
    /// cascade (which rewrites route targets), and hot reloads that change
    /// routes always trigger it; provider-only mutations trigger it only to
    /// retry a pending sync — with no pending and unchanged routes they
    /// skip it entirely, so healthy provider-only mutations never cause
    /// wasteful syncs.
    ///
    /// **Call discipline**: invoked only *after* the config commit (persist +
    /// in-memory commit + `routes_version` bump), while still holding
    /// `mutation_lock` and outside all `RwLock` guards (see
    /// [`Self::sync_identity`] for the lock rationale and the explicit
    /// half-commit error contract). A pending retry always syncs the
    /// *current* committed snapshot, so a pending sync retried after
    /// further commits converges on the latest config.
    async fn sync_identity_if_routes_changed(
        &self,
        what: &str,
        old: &GatewayConfig,
        committed: &GatewayConfig,
    ) -> Result<(), AppError> {
        if self.identity_registry.is_none() {
            return Ok(());
        }
        let pending = self.pending_identity_sync.load(Ordering::Acquire);
        if old.routes == committed.routes && !pending {
            return Ok(());
        }
        self.sync_identity(what, committed).await
    }

    // ── Public API ────────────────────────────────────────────────────

    /// Hot-reload: re-read, validate, and atomically publish the config at
    /// `path`. The file is the source of truth -- no write-back occurs.
    ///
    /// To handle concurrent file writes, this method reads the file revision
    /// before and after parsing; if the revision differs, the file changed
    /// during the operation and a retry (up to 3 attempts, 200ms apart) is
    /// performed. If the file remains unstable after all retries, an
    /// `AppError::Config` is returned and the caller (the
    /// `ConfigReloadHandler` implementation in the gateway) is responsible
    /// for scheduling a deferred fallback reload.
    pub async fn reload_from_path(&self, path: &Path) -> Result<MutationReport, AppError> {
        let _guard = self.mutation_lock.lock().await;

        let max_retries = 3;
        let retry_delay = std::time::Duration::from_millis(200);

        for attempt in 0..max_retries {
            // a) Read file revision before touching the file.
            let rev1 = FileRevision::from_path(path)?;

            // b) Parse, validate, and load the candidate config.
            let candidate = sag_config::reload_config(path).await?;

            // c) Clone the current running config.
            let current = self.config.read().await;
            let old_config = current.clone();
            drop(current);

            // d) Pre-build adapters (all RwLock guards are dropped).
            let sync = self.prepare_provider_sync(&old_config, &candidate)?;

            // e) Read file revision again to check stability.
            let rev2 = FileRevision::from_path(path)?;

            if rev1 == rev2 {
                // File was stable during the operation -- commit.
                let routes_changed = compute_routes_changed(&old_config, &candidate);

                // Commit: config (with the routes_version bump inside the
                // write guard — see `mutate` for the linearization
                // guarantee this ordering provides) -> providers.
                {
                    let mut config_guard = self.config.write().await;
                    *config_guard = candidate;
                    self.config.bump_routes_version();
                }
                {
                    let mut providers_guard = self.providers.write().await;
                    for name in &sync.to_remove {
                        providers_guard.remove(name);
                    }
                    for (name, provider) in &sync.to_insert {
                        providers_guard.insert(name.clone(), provider.clone());
                    }
                }

                // Update accepted file revision.
                *self.accepted_file_revision.lock().map_err(|e| {
                    AppError::Internal(format!("accepted_file_revision mutex poisoned: {e}"))
                })? = Some(rev2);

                // Reconcile analytics identities with the committed routes
                // when the routes list actually changed (same gate and lock
                // discipline as `mutate`; a failed sync returns an explicit
                // half-commit error and leaves a pending retry, so the next
                // mutation or reload — even one with unchanged routes —
                // re-syncs and heals).
                let committed = self.config.read().await;
                self.sync_identity_if_routes_changed("reload", &old_config, &committed)
                    .await?;
                drop(committed);

                let no_op = sync.added.is_empty()
                    && sync.rebuilt.is_empty()
                    && sync.removed.is_empty()
                    && routes_changed == 0;

                return Ok(MutationReport {
                    providers_added: sync.added,
                    providers_rebuilt: sync.rebuilt,
                    providers_removed: sync.removed,
                    routes_changed,
                    no_op,
                });
            }

            // File changed while we were reading -- retry if attempts remain.
            if attempt < max_retries - 1 {
                warn!(
                    attempt = attempt + 1,
                    "config file changed during reload, retrying"
                );
                tokio::time::sleep(retry_delay).await;
            }
        }

        Err(AppError::Config(
            "config file unstable after retries, fallback reload should be scheduled".into(),
        ))
    }

    /// Add a new provider. Returns `AppError::Conflict` if a provider with
    /// the same name already exists.
    pub async fn add_provider(
        &self,
        name: String,
        config: ProviderConfig,
    ) -> Result<MutationReport, AppError> {
        self.mutate(true, |candidate| {
            if candidate.providers.contains_key(&name) {
                return Err(AppError::Conflict(format!(
                    "provider '{name}' already exists"
                )));
            }
            candidate.providers.insert(name.clone(), config);
            Ok(())
        })
        .await
    }

    /// Replace a provider's full config. Returns `AppError::NotFound` if the
    /// provider does not exist.
    pub async fn update_provider(
        &self,
        name: &str,
        new_config: ProviderConfig,
    ) -> Result<MutationReport, AppError> {
        let name = name.to_string();
        self.mutate(true, |candidate| {
            if !candidate.providers.contains_key(&name) {
                return Err(AppError::NotFound(format!("provider '{name}' not found")));
            }
            candidate.providers.insert(name.clone(), new_config);
            Ok(())
        })
        .await
    }

    /// Delete a provider and cascade-remove its affected route targets.
    /// Returns the report together with a `CascadeResult` describing which
    /// routes/targets were touched.
    pub async fn delete_provider(
        &self,
        name: &str,
    ) -> Result<(MutationReport, sag_config::CascadeResult), AppError> {
        let name = name.to_string();
        let mut cascade_result: Option<sag_config::CascadeResult> = None;
        let report = self
            .mutate(true, |candidate| match candidate.remove_provider(&name) {
                Some(cr) => {
                    cascade_result = Some(cr);
                    Ok(())
                }
                None => Err(AppError::NotFound(format!("provider '{name}' not found"))),
            })
            .await?;
        // mutate returned Ok, so the closure ran and the only Ok(())
        // path sets cascade_result.
        Ok((
            report,
            cascade_result
                .expect("mutate returned Ok so remove_provider ran and set cascade_result"),
        ))
    }

    /// Create a new route. Returns `AppError::Conflict` if a route with the
    /// same `model_name` already exists.
    pub async fn create_route(&self, route: RouteConfig) -> Result<MutationReport, AppError> {
        let model_name = route.model_name.clone();
        self.mutate(true, |candidate| {
            if candidate.routes.iter().any(|r| r.model_name == model_name) {
                return Err(AppError::Conflict(format!(
                    "route '{model_name}' already exists"
                )));
            }
            candidate.routes.push(route);
            Ok(())
        })
        .await
    }

    /// Replace a route's full config, keyed by `model_name`.
    /// Returns `AppError::NotFound` if no route with that name exists.
    pub async fn update_route(
        &self,
        model_name: &str,
        route: RouteConfig,
    ) -> Result<MutationReport, AppError> {
        let name = model_name.to_string();
        self.mutate(true, |candidate| {
            let pos = candidate.routes.iter().position(|r| r.model_name == name);
            match pos {
                Some(idx) => {
                    candidate.routes[idx] = route;
                    Ok(())
                }
                None => Err(AppError::NotFound(format!("route '{name}' not found"))),
            }
        })
        .await
    }

    /// Delete a route by `model_name`. Returns `AppError::NotFound` if no
    /// route with that name exists.
    pub async fn delete_route(&self, model_name: &str) -> Result<MutationReport, AppError> {
        let name = model_name.to_string();
        self.mutate(true, |candidate| {
            let before = candidate.routes.len();
            candidate.routes.retain(|r| r.model_name != name);
            if candidate.routes.len() == before {
                return Err(AppError::NotFound(format!("route '{name}' not found")));
            }
            Ok(())
        })
        .await
    }

    /// Update gateway-level settings. Currently supports the top-level
    /// `proxy_url`; setting it to `None` (or an empty/whitespace value)
    /// clears the global proxy.
    ///
    /// Goes through the exact same unified mutation protocol as provider /
    /// route CRUD: the mutation mutex, validation (a malformed `proxy_url` is
    /// rejected up front, and effective-proxy resolution rejects `enabled`
    /// providers without a global proxy), atomic TOML persistence, config
    /// commit + `routes_version` bump, and the provider registry sync — so
    /// providers whose effective proxy changed are rebuilt exactly as a hot
    /// reload would, while `disabled` providers are left untouched. Handlers
    /// must call this instead of writing `config` / `providers` directly.
    pub async fn update_settings(
        &self,
        proxy_url: Option<String>,
    ) -> Result<MutationReport, AppError> {
        // Normalize empty/whitespace-only values to `None` (no proxy) so the
        // in-memory config matches what the save path persists (it removes
        // the key for empty/None). Validate up front so a malformed
        // `proxy_url` is rejected even when the provider set is empty (the
        // provider sync below only resolves proxies for configured providers,
        // so an empty set would otherwise let an invalid URL through). The
        // error message never echoes the URL itself (see `normalize_proxy_url`).
        let normalized = match proxy_url.as_deref().map(str::trim) {
            Some("") => None,
            other => other.map(str::to_string),
        };
        sag_config::normalize_proxy_url(normalized.as_deref()).map_err(AppError::Config)?;
        self.mutate(true, |candidate| {
            candidate.proxy_url = normalized;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use sag_config::{
        GatewayConfig, KeyStrategy, ProviderConfig, ProviderType, ProxyMode, RouteConfig, Strategy,
        gateway_config_base,
    };
    use sag_core::AppError;

    // ── FileRevision tests ────────────────────────────────────────────

    #[test]
    fn file_revision_identical_bytes_equal() {
        let a = FileRevision::from_bytes(b"hello world");
        let b = FileRevision::from_bytes(b"hello world");
        assert_eq!(a, b);
    }

    #[test]
    fn file_revision_different_bytes_not_equal() {
        let a = FileRevision::from_bytes(b"hello");
        let b = FileRevision::from_bytes(b"world");
        assert_ne!(a, b);
    }

    #[test]
    fn file_revision_empty_bytes() {
        let rev = FileRevision::from_bytes(b"");
        assert_eq!(rev.len, 0);
    }

    // ── adapter_config_changed tests ──────────────────────────────────

    fn base_provider() -> ProviderConfig {
        ProviderConfig {
            provider_type: ProviderType::OpenAI,
            api_keys: vec!["sk-test".into()],
            key_strategy: KeyStrategy::FillFirst,
            base_url: "https://api.openai.com/v1".into(),
            proxy_mode: ProxyMode::default(),
            models_endpoint: "models".into(),
            timeout_secs: 600,
            chunk_timeout_secs: None,
            ttfb_timeout_secs: None,
            headers: HashMap::new(),
            body_overrides: HashMap::new(),
            strip_fields: vec![],
            model_strip_fields: HashMap::new(),
            model_overrides: HashMap::new(),
            role_mappings: HashMap::new(),
            enabled: true,
            enabled_models: vec![],
            anthropic_cache_control: false,
        }
    }

    #[test]
    fn same_config_not_changed() {
        let pc = base_provider();
        assert!(!adapter_config_changed(&pc, &pc));
    }

    #[test]
    fn provider_type_change_triggers_rebuild() {
        let mut a = base_provider();
        let b = base_provider();
        a.provider_type = ProviderType::Anthropic;
        assert!(adapter_config_changed(&a, &b));
    }

    #[test]
    fn api_keys_change_triggers_rebuild() {
        let mut a = base_provider();
        let b = base_provider();
        a.api_keys.push("sk-extra".into());
        assert!(adapter_config_changed(&a, &b));
    }

    #[test]
    fn base_url_change_triggers_rebuild() {
        let mut a = base_provider();
        let b = base_provider();
        a.base_url = "https://different.example.com".into();
        assert!(adapter_config_changed(&a, &b));
    }

    #[test]
    fn timeout_secs_change_triggers_rebuild() {
        let mut a = base_provider();
        let b = base_provider();
        a.timeout_secs = 30;
        assert!(adapter_config_changed(&a, &b));
    }

    #[test]
    fn chunk_timeout_change_triggers_rebuild() {
        let mut a = base_provider();
        let b = base_provider();
        a.chunk_timeout_secs = Some(10);
        assert!(adapter_config_changed(&a, &b));
    }

    #[test]
    fn ttfb_timeout_change_triggers_rebuild() {
        let mut a = base_provider();
        let b = base_provider();
        a.ttfb_timeout_secs = Some(30);
        assert!(adapter_config_changed(&a, &b));
    }

    #[test]
    fn headers_change_triggers_rebuild() {
        let mut a = base_provider();
        let b = base_provider();
        a.headers.insert("X-Custom".into(), "value".into());
        assert!(adapter_config_changed(&a, &b));
    }

    #[test]
    fn body_overrides_change_triggers_rebuild() {
        let mut a = base_provider();
        let b = base_provider();
        a.body_overrides
            .insert("max_tokens".into(), serde_json::json!(4096));
        assert!(adapter_config_changed(&a, &b));
    }

    #[test]
    fn strip_fields_change_triggers_rebuild() {
        let mut a = base_provider();
        let b = base_provider();
        a.strip_fields.push("chat_template_kwargs".into());
        assert!(adapter_config_changed(&a, &b));
    }

    #[test]
    fn model_strip_fields_change_triggers_rebuild() {
        let mut a = base_provider();
        let b = base_provider();
        a.model_strip_fields
            .insert("glm-5.2".into(), vec!["chat_template_kwargs".into()]);
        assert!(adapter_config_changed(&a, &b));
    }

    #[test]
    fn role_mappings_change_triggers_rebuild() {
        let mut a = base_provider();
        let b = base_provider();
        a.role_mappings.insert("developer".into(), "system".into());
        assert!(adapter_config_changed(&a, &b));
    }

    #[test]
    fn anthropic_cache_control_change_triggers_rebuild() {
        let mut a = base_provider();
        let b = base_provider();
        a.anthropic_cache_control = true;
        assert!(adapter_config_changed(&a, &b));
    }

    #[test]
    fn key_strategy_change_triggers_rebuild() {
        let mut a = base_provider();
        let b = base_provider();
        a.key_strategy = KeyStrategy::RoundRobin;
        assert!(adapter_config_changed(&a, &b));
    }

    // The following MUST NOT trigger a rebuild.
    #[test]
    fn enabled_change_does_not_trigger_rebuild() {
        let mut a = base_provider();
        let b = base_provider();
        a.enabled = false;
        assert!(!adapter_config_changed(&a, &b));
    }

    #[test]
    fn enabled_models_change_does_not_trigger_rebuild() {
        let mut a = base_provider();
        let b = base_provider();
        a.enabled_models = vec!["gpt-4".into()];
        assert!(!adapter_config_changed(&a, &b));
    }

    #[test]
    fn models_endpoint_change_does_not_trigger_rebuild() {
        let mut a = base_provider();
        let b = base_provider();
        a.models_endpoint = "v1/models".into();
        assert!(!adapter_config_changed(&a, &b));
    }

    #[test]
    fn model_overrides_change_does_not_trigger_rebuild() {
        let mut a = base_provider();
        let b = base_provider();
        a.model_overrides.insert(
            "gpt-4".into(),
            HashMap::from([("temperature".into(), serde_json::json!(0.5))]),
        );
        assert!(!adapter_config_changed(&a, &b));
    }

    // ── compute_routes_changed tests ──────────────────────────────────

    #[test]
    fn routes_changed_empty_configs() {
        let g = GatewayConfig {
            ..gateway_config_base!()
        };
        assert_eq!(compute_routes_changed(&g, &g), 0);
    }

    #[test]
    fn routes_changed_new_route_added() {
        let old = GatewayConfig {
            ..gateway_config_base!()
        };
        let mut new = old.clone();
        new.routes.push(RouteConfig {
            model_name: "gpt-4".into(),
            api_endpoint: "/chat/completions".into(),
            strategy: Strategy::FillFirst,
            retries: None,
            timeout_secs: None,
            targets: vec![],
        });
        assert_eq!(compute_routes_changed(&old, &new), 1);
    }

    #[test]
    fn routes_changed_route_removed() {
        let mut old = GatewayConfig {
            ..gateway_config_base!()
        };
        old.routes.push(RouteConfig {
            model_name: "gpt-4".into(),
            api_endpoint: "/chat/completions".into(),
            strategy: Strategy::FillFirst,
            retries: None,
            timeout_secs: None,
            targets: vec![],
        });
        let new = GatewayConfig {
            ..gateway_config_base!()
        };
        assert_eq!(compute_routes_changed(&old, &new), 1);
    }

    #[test]
    fn routes_changed_add_and_remove() {
        let mut old = GatewayConfig {
            ..gateway_config_base!()
        };
        old.routes.push(RouteConfig {
            model_name: "gpt-4".into(),
            api_endpoint: "/chat/completions".into(),
            strategy: Strategy::FillFirst,
            retries: None,
            timeout_secs: None,
            targets: vec![],
        });
        let mut new = GatewayConfig {
            ..gateway_config_base!()
        };
        new.routes.push(RouteConfig {
            model_name: "claude".into(),
            api_endpoint: "/chat/completions".into(),
            strategy: Strategy::FillFirst,
            retries: None,
            timeout_secs: None,
            targets: vec![],
        });
        assert_eq!(compute_routes_changed(&old, &new), 2);
    }

    // ── prepare_provider_sync tests ───────────────────────────────────

    fn make_coordinator() -> (ConfigMutationCoordinator, ConfigHandle) {
        let config = ConfigHandle::from_config(GatewayConfig {
            ..gateway_config_base!()
        });
        let providers: ProviderRegistry = Arc::new(RwLock::new(HashMap::new()));
        let coord = ConfigMutationCoordinator::new(config.clone(), providers);
        (coord, config)
    }

    /// Build a full `GatewayConfig` with the given top-level `proxy_url` and
    /// provider map (the shape `prepare_provider_sync` now compares).
    fn cfg(proxy_url: Option<&str>, providers: HashMap<String, ProviderConfig>) -> GatewayConfig {
        GatewayConfig {
            providers,
            proxy_url: proxy_url.map(str::to_string),
            ..gateway_config_base!()
        }
    }

    /// A provider with `proxy_mode = "disabled"`.
    fn disabled_provider() -> ProviderConfig {
        ProviderConfig {
            proxy_mode: ProxyMode::Disabled,
            ..base_provider()
        }
    }

    /// A provider with `proxy_mode = "enabled"`.
    fn enabled_provider() -> ProviderConfig {
        ProviderConfig {
            proxy_mode: ProxyMode::Enabled,
            ..base_provider()
        }
    }

    #[test]
    fn prepare_sync_new_provider() {
        let (coord, _config) = make_coordinator();
        let old = cfg(None, HashMap::new());
        let mut new = cfg(None, HashMap::new());
        new.providers.insert("openai".into(), base_provider());

        let sync = coord.prepare_provider_sync(&old, &new).unwrap();
        assert_eq!(sync.added, vec!["openai"]);
        assert!(sync.rebuilt.is_empty());
        assert!(sync.removed.is_empty());
        assert_eq!(sync.to_remove.len(), 0);
        assert_eq!(sync.to_insert.len(), 1);
        assert_eq!(sync.to_insert[0].0, "openai");
    }

    #[test]
    fn prepare_sync_removed_provider() {
        let (coord, _config) = make_coordinator();
        let mut old = cfg(None, HashMap::new());
        old.providers.insert("openai".into(), base_provider());
        let new = cfg(None, HashMap::new());

        let sync = coord.prepare_provider_sync(&old, &new).unwrap();
        assert_eq!(sync.removed, vec!["openai"]);
        assert!(sync.added.is_empty());
        assert!(sync.rebuilt.is_empty());
        assert_eq!(sync.to_remove, vec!["openai"]);
    }

    #[test]
    fn prepare_sync_no_change() {
        let (coord, _config) = make_coordinator();
        let mut map: HashMap<String, ProviderConfig> = HashMap::new();
        map.insert("openai".into(), base_provider());
        let both = cfg(None, map);

        let sync = coord.prepare_provider_sync(&both, &both).unwrap();
        assert!(sync.added.is_empty());
        assert!(sync.rebuilt.is_empty());
        assert!(sync.removed.is_empty());
        assert!(sync.to_insert.is_empty());
    }

    #[test]
    fn prepare_sync_rebuild_on_adapter_config_change() {
        let (coord, _config) = make_coordinator();
        let mut old = cfg(None, HashMap::new());
        old.providers.insert("openai".into(), base_provider());
        let mut new = cfg(None, HashMap::new());
        let mut changed = base_provider();
        changed.base_url = "https://new.example.com".into();
        new.providers.insert("openai".into(), changed);

        let sync = coord.prepare_provider_sync(&old, &new).unwrap();
        assert_eq!(sync.rebuilt, vec!["openai"]);
        assert!(sync.added.is_empty());
        assert!(sync.removed.is_empty());
        assert_eq!(sync.to_insert.len(), 1);
    }

    #[test]
    fn prepare_sync_skip_rebuild_on_non_adapter_change() {
        let (coord, _config) = make_coordinator();
        let mut old = cfg(None, HashMap::new());
        old.providers.insert("openai".into(), base_provider());
        let mut new = cfg(None, HashMap::new());
        let mut changed = base_provider();
        changed.enabled = false;
        changed.model_overrides.insert(
            "gpt-4".into(),
            HashMap::from([("temperature".into(), serde_json::json!(0.5))]),
        );
        new.providers.insert("openai".into(), changed);

        let sync = coord.prepare_provider_sync(&old, &new).unwrap();
        assert!(sync.added.is_empty());
        assert!(sync.rebuilt.is_empty());
        assert!(sync.removed.is_empty());
        assert!(sync.to_insert.is_empty());
    }

    #[test]
    fn prepare_sync_fails_on_empty_api_keys() {
        let (coord, _config) = make_coordinator();
        let old = cfg(None, HashMap::new());
        let mut new = cfg(None, HashMap::new());
        let mut bad = base_provider();
        bad.api_keys.clear();
        new.providers.insert("broken".into(), bad);

        let err = coord.prepare_provider_sync(&old, &new).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
    }

    // ── effective_proxy_for tests ─────────────────────────────────────
    //
    // The resolution feeding every construction path: `proxy_mode` combined
    // with the normalized global `proxy_url`.

    fn proxy_cfg(proxy_url: Option<&str>) -> GatewayConfig {
        cfg(proxy_url, HashMap::new())
    }

    #[test]
    fn effective_proxy_default_mode_without_global_is_direct() {
        // Legacy configs (no `proxy_url`, default `proxy_mode = inherit`)
        // must resolve to a direct connection — never to a system proxy.
        let config = proxy_cfg(None);
        let pc = base_provider();
        assert_eq!(effective_proxy_for(&config, &pc).unwrap(), None);
    }

    #[test]
    fn effective_proxy_inherit_uses_global() {
        let config = proxy_cfg(Some("http://proxy.example:8080"));
        let pc = base_provider();
        let effective = effective_proxy_for(&config, &pc).unwrap().unwrap();
        assert_eq!(effective.to_connect_url(), "http://proxy.example:8080");
    }

    #[test]
    fn effective_proxy_blank_global_treated_as_none() {
        // Empty / whitespace-only global values normalize to "no proxy".
        let config = proxy_cfg(Some("   "));
        let pc = base_provider();
        assert_eq!(effective_proxy_for(&config, &pc).unwrap(), None);
    }

    #[test]
    fn effective_proxy_disabled_ignores_global() {
        let config = proxy_cfg(Some("http://proxy.example:8080"));
        let pc = disabled_provider();
        assert_eq!(effective_proxy_for(&config, &pc).unwrap(), None);
    }

    #[test]
    fn effective_proxy_enabled_uses_global() {
        let config = proxy_cfg(Some("http://proxy.example:8080"));
        let pc = enabled_provider();
        let effective = effective_proxy_for(&config, &pc).unwrap().unwrap();
        assert_eq!(effective.to_connect_url(), "http://proxy.example:8080");
    }

    #[test]
    fn effective_proxy_enabled_without_global_is_error() {
        let config = proxy_cfg(None);
        let pc = enabled_provider();
        let err = effective_proxy_for(&config, &pc).unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
    }

    #[test]
    fn effective_proxy_malformed_global_is_error() {
        let config = proxy_cfg(Some("http://user:pass@proxy.example"));
        let pc = base_provider();
        let err = effective_proxy_for(&config, &pc).unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
    }

    // ── proxy-driven rebuild diff tests ───────────────────────────────

    #[test]
    fn global_proxy_added_rebuilds_inherit_provider() {
        let (coord, _config) = make_coordinator();
        let mut old = cfg(None, HashMap::new());
        old.providers.insert("openai".into(), base_provider());
        let mut new = cfg(Some("http://proxy.example:8080"), HashMap::new());
        new.providers.insert("openai".into(), base_provider());

        let sync = coord.prepare_provider_sync(&old, &new).unwrap();
        assert_eq!(sync.rebuilt, vec!["openai"]);
    }

    #[test]
    fn global_proxy_url_change_rebuilds_inherit_and_enabled() {
        let (coord, _config) = make_coordinator();
        let mut old = cfg(Some("http://proxy-a.example:8080"), HashMap::new());
        old.providers.insert("inherit".into(), base_provider());
        old.providers.insert("forced".into(), enabled_provider());
        let mut new = cfg(Some("http://proxy-b.example:8080"), HashMap::new());
        new.providers.insert("inherit".into(), base_provider());
        new.providers.insert("forced".into(), enabled_provider());

        let sync = coord.prepare_provider_sync(&old, &new).unwrap();
        let mut rebuilt = sync.rebuilt.clone();
        rebuilt.sort();
        assert_eq!(rebuilt, vec!["forced", "inherit"]);
    }

    #[test]
    fn global_proxy_change_does_not_rebuild_disabled() {
        let (coord, _config) = make_coordinator();
        let mut old = cfg(None, HashMap::new());
        old.providers.insert("direct".into(), disabled_provider());
        let mut new = cfg(Some("http://proxy.example:8080"), HashMap::new());
        new.providers.insert("direct".into(), disabled_provider());

        let sync = coord.prepare_provider_sync(&old, &new).unwrap();
        assert!(sync.rebuilt.is_empty());
        assert!(sync.added.is_empty());
        assert!(sync.to_insert.is_empty());
    }

    #[test]
    fn global_proxy_change_with_mixed_modes_rebuilds_only_affected() {
        let (coord, _config) = make_coordinator();
        let mut old = cfg(None, HashMap::new());
        old.providers.insert("inherit".into(), base_provider());
        old.providers.insert("direct".into(), disabled_provider());
        let mut new = cfg(Some("http://proxy.example:8080"), HashMap::new());
        new.providers.insert("inherit".into(), base_provider());
        new.providers.insert("direct".into(), disabled_provider());

        let sync = coord.prepare_provider_sync(&old, &new).unwrap();
        assert_eq!(sync.rebuilt, vec!["inherit"]);
        assert!(!sync.rebuilt.contains(&"direct".to_string()));
    }

    #[test]
    fn proxy_mode_switch_rebuilds_even_when_effective_unchanged() {
        // Inherit -> Disabled with no global proxy: the effective proxy is
        // `None` on both sides, but the explicit mode switch must still
        // rebuild the adapter.
        let (coord, _config) = make_coordinator();
        let mut old = cfg(None, HashMap::new());
        old.providers.insert("openai".into(), base_provider());
        let mut new = cfg(None, HashMap::new());
        new.providers.insert("openai".into(), disabled_provider());

        let sync = coord.prepare_provider_sync(&old, &new).unwrap();
        assert_eq!(sync.rebuilt, vec!["openai"]);
    }

    #[test]
    fn proxy_mode_switch_inherit_to_enabled_with_global_rebuilds() {
        let (coord, _config) = make_coordinator();
        let mut old = cfg(Some("http://proxy.example:8080"), HashMap::new());
        old.providers.insert("openai".into(), base_provider());
        let mut new = cfg(Some("http://proxy.example:8080"), HashMap::new());
        new.providers.insert("openai".into(), enabled_provider());

        let sync = coord.prepare_provider_sync(&old, &new).unwrap();
        assert_eq!(sync.rebuilt, vec!["openai"]);
    }

    #[test]
    fn new_provider_is_added_with_global_proxy_present() {
        // A provider added while a global proxy is configured resolves under
        // the new config without error and enters the insert set.
        let (coord, _config) = make_coordinator();
        let old = cfg(Some("http://proxy.example:8080"), HashMap::new());
        let mut new = cfg(Some("http://proxy.example:8080"), HashMap::new());
        new.providers.insert("openai".into(), base_provider());

        let sync = coord.prepare_provider_sync(&old, &new).unwrap();
        assert_eq!(sync.added, vec!["openai"]);
        assert_eq!(sync.to_insert.len(), 1);
    }

    #[test]
    fn enabled_without_global_rejects_mutation() {
        // A new provider in `enabled` mode with no global proxy cannot be
        // satisfied: the sync must fail (config error), never unwrap/panic.
        let (coord, _config) = make_coordinator();
        let old = cfg(None, HashMap::new());
        let mut new = cfg(None, HashMap::new());
        new.providers.insert("forced".into(), enabled_provider());

        let err = coord.prepare_provider_sync(&old, &new).unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
    }

    // ── Config read-guard / routes_version atomicity ──────────────────
    //
    // The dispatch sticky writeback holds `config.read()` across its
    // routes_version read + `active_targets` write (chat.rs / image.rs).
    // Those two tests pin the coordinator-side guarantee that protocol
    // relies on: a mutation's config commit and its version bump are atomic
    // under `config.write()`.

    fn make_arc_coordinator() -> (Arc<ConfigMutationCoordinator>, ConfigHandle) {
        let config = ConfigHandle::from_config(GatewayConfig {
            ..gateway_config_base!()
        });
        let providers: ProviderRegistry = Arc::new(RwLock::new(HashMap::new()));
        let coord = Arc::new(ConfigMutationCoordinator::new(config.clone(), providers));
        (coord, config)
    }

    fn new_route() -> RouteConfig {
        RouteConfig {
            model_name: "gpt-4o".into(),
            api_endpoint: "/chat/completions".into(),
            strategy: Strategy::FillFirst,
            retries: None,
            timeout_secs: None,
            targets: vec![],
        }
    }

    #[tokio::test]
    async fn config_read_guard_pins_content_and_routes_version_together() {
        // While any config read guard is held (as the sticky writeback
        // holds one), a concurrent mutation must be completely invisible:
        // neither the config content nor routes_version can move, and both
        // become visible together only after the guard is released. This is
        // the guarantee that makes the writeback's read-compare-insert
        // atomic against config changes.
        let (coord, config) = make_arc_coordinator();
        let version_before = config.routes_version();
        let routes_before = config.read().await.routes.clone();

        // Hold the read guard exactly like the dispatch writeback does.
        let guard = config.read().await;

        let task_coord = coord.clone();
        let handle = tokio::spawn(async move { task_coord.create_route(new_route()).await });

        // Run the mutation task until it blocks on `config.write()`.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        // While the guard is held the mutation must be fully invisible.
        // (`routes_version` is an atomic load and `guard.routes` reads
        // through the guard we already hold — a second `config.read()`
        // would deadlock, because tokio's RwLock queues new readers behind
        // the mutation's waiting `config.write()`.)
        assert!(
            !handle.is_finished(),
            "a mutation must block on config.write() while a read guard is held"
        );
        assert_eq!(
            config.routes_version(),
            version_before,
            "routes_version must not move while a config read guard is held"
        );
        assert_eq!(
            guard.routes, routes_before,
            "config content must not change while a config read guard is held"
        );

        drop(guard);

        let report = handle
            .await
            .expect("mutation task must complete")
            .expect("mutation must succeed");
        assert!(report.routes_changed >= 1);
        assert_eq!(
            config.routes_version(),
            version_before + 1,
            "commit and version bump become visible together after the guard is released"
        );
        assert!(
            config
                .read()
                .await
                .routes
                .iter()
                .any(|r| r.model_name == "gpt-4o"),
            "the route created by the mutation must be visible after release"
        );
    }

    #[tokio::test]
    async fn routes_version_bump_is_atomic_with_config_commit() {
        // The writeback protocol is only sound if a mutation's commit and
        // its routes_version bump are atomic under `config.write()` — a
        // version that lags the config commit would reopen the stale
        // writeback window (read old version → mutation commits → writeback
        // succeeds against the pre-mutation version). This test pins that
        // atomicity by parking the mutation mid-commit on `providers.write()`
        // (the step that follows `config.write()` in the commit order): the
        // version must ALREADY reflect the commit while the mutation is
        // parked, i.e. the bump happened inside the config write guard.
        //
        // The providers registry must be the SAME Arc the coordinator was
        // built with — the mutation's `providers.write()` parks only on
        // that registry.
        let config = ConfigHandle::from_config(GatewayConfig {
            ..gateway_config_base!()
        });
        let providers: ProviderRegistry = Arc::new(RwLock::new(HashMap::new()));
        let coord = Arc::new(ConfigMutationCoordinator::new(
            config.clone(),
            providers.clone(),
        ));
        let version_before = config.routes_version();

        // Hold the provider-registry write lock so the mutation parks after
        // its config commit (config.write() → providers.write()).
        let providers_guard = providers.write().await;

        let task_coord = coord.clone();
        let handle = tokio::spawn(async move { task_coord.create_route(new_route()).await });

        // All the mutation's awaits before `providers.write()` are
        // uncontended, so one yield parks it there.
        tokio::task::yield_now().await;
        assert!(
            !handle.is_finished(),
            "mutation must park on providers.write() mid-commit"
        );
        assert_eq!(
            config.routes_version(),
            version_before + 1,
            "routes_version must be bumped inside the config write guard, \
             before the provider-registry sync — a lagging version would let \
             a stale sticky writeback slip through the window"
        );

        drop(providers_guard);
        let report = handle
            .await
            .expect("mutation task must complete")
            .expect("mutation must succeed");
        assert!(report.routes_changed >= 1);
    }
}
