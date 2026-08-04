use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Arc;

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
    accepted_file_revision: std::sync::Mutex<Option<FileRevision>>,
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
        }
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
        let sync = self.prepare_provider_sync(&old_config.providers, &candidate.providers)?;

        // Step 5: Persist (if requested and file-backed).
        if persist && let Some(path) = self.config.path() {
            let mut accepted_guard = self.accepted_file_revision.lock().unwrap();
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

    /// Compute the diff between `old` and `new` provider maps and pre-build
    /// any new or changed adapters. Returns a `PreparedProviderSync` ready for
    /// commit.
    ///
    /// Adapter construction failures (e.g. empty `api_keys`) are propagated
    /// immediately, preventing any partial commit.
    fn prepare_provider_sync(
        &self,
        old: &HashMap<String, ProviderConfig>,
        new: &HashMap<String, ProviderConfig>,
    ) -> Result<PreparedProviderSync, AppError> {
        let mut to_remove: Vec<String> = Vec::new();
        let mut to_insert: Vec<(String, Arc<dyn LlmProvider>)> = Vec::new();
        let mut added: Vec<String> = Vec::new();
        let mut rebuilt: Vec<String> = Vec::new();
        let mut removed: Vec<String> = Vec::new();

        // Providers that existed in old but are gone from new -> remove.
        for name in old.keys() {
            if !new.contains_key(name) {
                to_remove.push(name.clone());
                removed.push(name.clone());
            }
        }

        // Providers in the new config.
        for (name, new_pc) in new.iter() {
            match old.get(name) {
                None => {
                    // New provider: build adapter.
                    let provider = sag_providers::create_provider(name.clone(), new_pc)?;
                    to_insert.push((name.clone(), provider));
                    added.push(name.clone());
                }
                Some(old_pc) => {
                    if adapter_config_changed(old_pc, new_pc) {
                        // Adapter-captured fields changed: rebuild.
                        let provider = sag_providers::create_provider(name.clone(), new_pc)?;
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
            let sync = self.prepare_provider_sync(&old_config.providers, &candidate.providers)?;

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
                *self.accepted_file_revision.lock().unwrap() = Some(rev2);

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
        // mutate succeeded, so the closure ran and cascade_result was set.
        Ok((report, cascade_result.unwrap()))
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use sag_config::{
        KeyStrategy, ProviderConfig, ProviderType, RouteConfig, Strategy, gateway_config_base,
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

    #[test]
    fn prepare_sync_new_provider() {
        let (coord, _config) = make_coordinator();
        let old: HashMap<String, ProviderConfig> = HashMap::new();
        let mut new: HashMap<String, ProviderConfig> = HashMap::new();
        new.insert("openai".into(), base_provider());

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
        let mut old: HashMap<String, ProviderConfig> = HashMap::new();
        old.insert("openai".into(), base_provider());
        let new: HashMap<String, ProviderConfig> = HashMap::new();

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

        let sync = coord.prepare_provider_sync(&map, &map).unwrap();
        assert!(sync.added.is_empty());
        assert!(sync.rebuilt.is_empty());
        assert!(sync.removed.is_empty());
        assert!(sync.to_insert.is_empty());
    }

    #[test]
    fn prepare_sync_rebuild_on_adapter_config_change() {
        let (coord, _config) = make_coordinator();
        let mut old: HashMap<String, ProviderConfig> = HashMap::new();
        old.insert("openai".into(), base_provider());
        let mut new: HashMap<String, ProviderConfig> = HashMap::new();
        let mut changed = base_provider();
        changed.base_url = "https://new.example.com".into();
        new.insert("openai".into(), changed);

        let sync = coord.prepare_provider_sync(&old, &new).unwrap();
        assert_eq!(sync.rebuilt, vec!["openai"]);
        assert!(sync.added.is_empty());
        assert!(sync.removed.is_empty());
        assert_eq!(sync.to_insert.len(), 1);
    }

    #[test]
    fn prepare_sync_skip_rebuild_on_non_adapter_change() {
        let (coord, _config) = make_coordinator();
        let mut old: HashMap<String, ProviderConfig> = HashMap::new();
        old.insert("openai".into(), base_provider());
        let mut new: HashMap<String, ProviderConfig> = HashMap::new();
        let mut changed = base_provider();
        changed.enabled = false;
        changed.model_overrides.insert(
            "gpt-4".into(),
            HashMap::from([("temperature".into(), serde_json::json!(0.5))]),
        );
        new.insert("openai".into(), changed);

        let sync = coord.prepare_provider_sync(&old, &new).unwrap();
        assert!(sync.added.is_empty());
        assert!(sync.rebuilt.is_empty());
        assert!(sync.removed.is_empty());
        assert!(sync.to_insert.is_empty());
    }

    #[test]
    fn prepare_sync_fails_on_empty_api_keys() {
        let (coord, _config) = make_coordinator();
        let old: HashMap<String, ProviderConfig> = HashMap::new();
        let mut new: HashMap<String, ProviderConfig> = HashMap::new();
        let mut bad = base_provider();
        bad.api_keys.clear();
        new.insert("broken".into(), bad);

        let err = coord.prepare_provider_sync(&old, &new).unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
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
