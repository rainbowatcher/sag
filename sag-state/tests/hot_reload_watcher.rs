//! Regression tests for the hot-reload watcher ↔ `ConfigMutationCoordinator`
//! integration.
//!
//! These pin the fix for: after the user manually edits `gateway.toml`, SAG
//! must detect the change in real time and reload it, so a subsequent UI
//! mutation (e.g. adding models to a provider) does not fail with
//! "config file changed outside SAG; reload and retry" because the
//! coordinator's `accepted_file_revision` still reflects the pre-edit file.
//!
//! The watcher (in `sag-config`) must:
//! - survive atomic temp-file + rename saves (it watches the parent
//!   directory, not the file inode — a file-level watch dies on rename);
//! - merge bursts of file events with a trailing debounce and load the final
//!   version rather than a transient intermediate;
//! - preserve the safety property that a genuine external edit which has NOT
//!   been reloaded is still refused — never silently overwritten.
//!
//! These touch the filesystem and cross crate boundaries (sag-config watcher
//! ↔ sag-state coordinator), so they live in the integration-test directory
//! per the project conventions (AGENTS.md). All waits are polling-based to
//! avoid fragile fixed sleeps.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use sag_config::{ConfigHandle, KeyStrategy, ProviderConfig, ProviderType};
use sag_providers::LlmProvider;
use sag_state::ConfigMutationCoordinator;
use tokio::sync::RwLock;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_temp_path(prefix: &str) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir()
        .canonicalize()
        .unwrap_or_else(|_| std::env::temp_dir());
    dir.join(format!("{prefix}_{}_{n}.toml", std::process::id()))
}

fn base_provider() -> ProviderConfig {
    ProviderConfig {
        provider_type: ProviderType::OpenAI,
        api_keys: vec!["sk-test".into()],
        key_strategy: KeyStrategy::FillFirst,
        base_url: "https://api.openai.com/v1".into(),
        proxy_mode: sag_config::ProxyMode::default(),
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

/// Minimal file-backed config with provider `p1`.
fn initial_toml() -> &'static str {
    r#"
        [server]
        host = "127.0.0.1"
        port = 37210

        [providers.p1]
        provider_type = "openai"
        api_keys = ["sk-test"]
    "#
}

/// TOML for provider `p1` carrying the given `enabled_models` array.
fn toml_with_enabled_models(models: &[&str]) -> String {
    let quoted: Vec<String> = models.iter().map(|m| format!("\"{m}\"")).collect();
    format!(
        r#"
        [server]
        host = "127.0.0.1"
        port = 37210

        [providers.p1]
        provider_type = "openai"
        api_keys = ["sk-test"]
        enabled_models = [{}]
    "#,
        quoted.join(", ")
    )
}

async fn enabled_models(handle: &ConfigHandle, provider: &str) -> Vec<String> {
    handle
        .read()
        .await
        .providers
        .get(provider)
        .map(|p| p.enabled_models.clone())
        .unwrap_or_default()
}

/// A `ConfigReloadHandler` delegating to a coordinator — mirrors what
/// `GatewayReloadHandler` does in `sag-gateway`.
struct CoordHandler {
    coordinator: Arc<ConfigMutationCoordinator>,
}

#[async_trait::async_trait]
impl sag_config::ConfigReloadHandler for CoordHandler {
    async fn reload_from_path(&self, path: &Path) -> sag_core::Result<()> {
        self.coordinator.reload_from_path(path).await?;
        Ok(())
    }
}

/// Poll `pred` (which may be async) until it returns `true` or `timeout`
/// elapses. Polls every 20ms; no fixed sleeps.
async fn wait_until<F, Fut>(timeout: Duration, mut pred: F) -> Result<(), String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if pred().await {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for condition".into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Run `emit` (which performs a write to the config file) up to `attempts`
/// times, polling `pred` after each emission. Writing repeatedly guards
/// against two real-world races:
/// - the watcher's directory watch may not be registered yet when the first
///   write lands (the event would be missed), and
/// - a transient failed read (mid-write) may need a re-trigger.
///
/// Each attempt emits exactly once and then waits quietly, so the watcher's
/// trailing debounce can fire.
async fn emit_and_wait<F, Fut, E>(attempts: usize, mut emit: E, mut pred: F) -> Result<(), String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
    E: FnMut(),
{
    for _ in 0..attempts {
        emit();
        if wait_until(Duration::from_millis(1500), &mut pred)
            .await
            .is_ok()
        {
            return Ok(());
        }
    }
    Err("condition not satisfied after repeated config writes".into())
}

#[tokio::test]
async fn external_edit_reloaded_then_ui_mutation_succeeds() {
    let file_path = unique_temp_path("sag_hotreload_edit");
    std::fs::write(&file_path, initial_toml()).unwrap();

    let handle = sag_config::load_config(&file_path).unwrap();
    let providers: Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let coord = Arc::new(ConfigMutationCoordinator::new(handle.clone(), providers));
    let handler: Arc<dyn sag_config::ConfigReloadHandler> = Arc::new(CoordHandler {
        coordinator: coord.clone(),
    });
    let watcher = sag_config::spawn_hot_reload(handle.clone(), handler);

    // External edit: the user manually adds `enabled_models = ["gpt-4"]` to
    // provider `p1` in the file. SAG must detect and reload it.
    let edited = toml_with_enabled_models(&["gpt-4"]);
    emit_and_wait(
        5,
        || std::fs::write(&file_path, &edited).unwrap(),
        || async { enabled_models(&handle, "p1").await == vec!["gpt-4".to_string()] },
    )
    .await
    .expect("watcher should reload the externally edited config");

    // The reload must have updated `accepted_file_revision` to the edited
    // file, so a subsequent UI mutation persists without a conflict.
    let mut update = base_provider();
    update.api_keys = vec!["sk-test-2".into()];
    update.enabled_models = vec!["gpt-4".into()];
    let report = coord
        .update_provider("p1", update)
        .await
        .expect("UI mutation must succeed after the external edit was reloaded");
    assert!(!report.no_op);

    // The mutation was also persisted back to disk.
    let disk = std::fs::read_to_string(&file_path).unwrap();
    assert!(
        disk.contains("sk-test-2"),
        "mutation should be persisted, got:\n{disk}"
    );

    watcher.shutdown().await;
    let _ = std::fs::remove_file(&file_path);
}

#[tokio::test]
async fn atomic_replace_burst_reloads_final_version() {
    let file_path = unique_temp_path("sag_hotreload_atomic");
    std::fs::write(&file_path, initial_toml()).unwrap();

    let handle = sag_config::load_config(&file_path).unwrap();
    let providers: Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let coord = Arc::new(ConfigMutationCoordinator::new(handle.clone(), providers));
    let handler: Arc<dyn sag_config::ConfigReloadHandler> = Arc::new(CoordHandler {
        coordinator: coord.clone(),
    });
    let watcher = sag_config::spawn_hot_reload(handle.clone(), handler);

    // Two consecutive atomic temp-file + rename saves in a burst — exactly
    // what SAG's own persistence (`save_config_to_file`) does. The watcher
    // must survive the rename (it watches the parent directory, not the
    // inode) and load the FINAL version, never just the intermediate one.
    let cfg = handle.read().await.clone();
    let mut intermediate = cfg.clone();
    intermediate.providers.get_mut("p1").unwrap().enabled_models = vec!["gpt-4".into()];
    let mut final_cfg = cfg;
    final_cfg.providers.get_mut("p1").unwrap().enabled_models =
        vec!["gpt-4".into(), "gpt-4o".into()];

    let expected = vec!["gpt-4".to_string(), "gpt-4o".to_string()];
    emit_and_wait(
        8,
        || {
            // Write the intermediate and final versions back-to-back within a
            // single burst, as an editor's staged atomic save would. Both are
            // temp-file + rename saves. The trailing debounce must ensure only
            // the FINAL version lands in the runtime config — how many
            // intermediate reloads (if any) actually ran is not observable
            // from here, and is not what this test asserts.
            sag_config::save_config_to_file(&intermediate, &file_path).unwrap();
            sag_config::save_config_to_file(&final_cfg, &file_path).unwrap();
        },
        || async { enabled_models(&handle, "p1").await == expected },
    )
    .await
    .expect("watcher should load the final version after an atomic-replace burst");

    // The FINAL version must remain loaded (not just transiently observed):
    // the file on disk is the final config, so a settled reload reads it.
    assert_eq!(enabled_models(&handle, "p1").await, expected);

    watcher.shutdown().await;
    let _ = std::fs::remove_file(&file_path);
}

#[tokio::test]
async fn external_edit_without_reload_is_still_conflict() {
    let file_path = unique_temp_path("sag_hotreload_conflict");
    std::fs::write(&file_path, initial_toml()).unwrap();

    let handle = sag_config::load_config(&file_path).unwrap();
    let providers: Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    // Deliberately NO watcher: no reload ever runs. This is the deterministic
    // analogue of "a reload has not completed yet" — the coordinator must
    // refuse to overwrite a genuine external edit, never silently clobber it.
    let coord = ConfigMutationCoordinator::new(handle.clone(), providers);

    // External edit, never reloaded.
    std::fs::write(&file_path, toml_with_enabled_models(&["gpt-4"])).unwrap();

    let mut update = base_provider();
    update.enabled_models = vec!["gpt-4".into(), "gpt-4o".into()];
    let err = coord
        .update_provider("p1", update)
        .await
        .expect_err("mutating over a non-reloaded external edit must conflict");
    assert!(
        matches!(err, sag_core::AppError::Conflict(_)),
        "expected Conflict, got {err:?}"
    );
    assert!(
        err.to_string().contains("config file changed outside SAG"),
        "unexpected conflict message: {err}"
    );

    // The external edit must be left untouched on disk.
    let disk = std::fs::read_to_string(&file_path).unwrap();
    assert!(
        disk.contains("\"gpt-4\""),
        "external edit should be preserved:\n{disk}"
    );
    assert!(
        !disk.contains("gpt-4o"),
        "mutation must not be persisted:\n{disk}"
    );

    let _ = std::fs::remove_file(&file_path);
}

#[tokio::test]
async fn sag_own_persist_reloaded_without_false_conflict() {
    let file_path = unique_temp_path("sag_hotreload_ownsave");
    std::fs::write(&file_path, initial_toml()).unwrap();

    let handle = sag_config::load_config(&file_path).unwrap();
    let providers: Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let coord = Arc::new(ConfigMutationCoordinator::new(handle.clone(), providers));
    let handler: Arc<dyn sag_config::ConfigReloadHandler> = Arc::new(CoordHandler {
        coordinator: coord.clone(),
    });
    let watcher = sag_config::spawn_hot_reload(handle.clone(), handler);

    // Ensure the watcher has registered its watches before the admin
    // mutation, so the mutation's own atomic save is observed. A benign
    // external write + wait for its reload is a polling-based readiness
    // probe (no fragile fixed sleep); it also warms the `accepted_file_revision`
    // to the probe file.
    let probe = toml_with_enabled_models(&["probe"]);
    emit_and_wait(
        5,
        || std::fs::write(&file_path, &probe).unwrap(),
        || async { enabled_models(&handle, "p1").await == vec!["probe".to_string()] },
    )
    .await
    .expect("watcher should be ready (reload a probe write)");

    // Admin-style mutation 1: persists to disk via SAG's own atomic save.
    // The watcher will detect that write and reload it — a harmless no-op —
    // which must NOT be misconstrued as an external edit.
    let mut cfg1 = base_provider();
    cfg1.api_keys = vec!["sk-test-1".into()];
    coord
        .update_provider("p1", cfg1)
        .await
        .expect("mutation 1 should succeed");
    let version_after_mutation = handle.routes_version();

    // Wait for the watcher's no-op reload of SAG's own write (it bumps
    // routes_version). If the watcher never picks it up, this fails — which
    // would indicate the watch broke on SAG's own atomic save.
    wait_until(Duration::from_secs(10), || async {
        handle.routes_version() > version_after_mutation
    })
    .await
    .expect("watcher should reload SAG's own persist without error");

    // Mutation 2 must succeed: SAG's own reload must not have corrupted the
    // accepted file revision (no false "config file changed outside SAG").
    let mut cfg2 = base_provider();
    cfg2.api_keys = vec!["sk-test-2".into()];
    coord
        .update_provider("p1", cfg2)
        .await
        .expect("mutation 2 must succeed without a false conflict");

    watcher.shutdown().await;
    let _ = std::fs::remove_file(&file_path);
}
