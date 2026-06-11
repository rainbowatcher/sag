use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::config::{GatewayConfig, KeyStrategy};

/// A shared, hot-reloadable configuration handle.
#[derive(Debug, Clone)]
pub struct ConfigHandle {
    inner: Arc<RwLock<GatewayConfig>>,
    config_path: Option<Arc<PathBuf>>,
}

impl ConfigHandle {
    /// Build a `ConfigHandle` from an in-memory `GatewayConfig` (for testing / programmatic use).
    pub fn from_config(config: GatewayConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(config)),
            config_path: None,
        }
    }

    /// Read the current config (acquires a read lock).
    pub async fn read(&self) -> tokio::sync::RwLockReadGuard<'_, GatewayConfig> {
        self.inner.read().await
    }

    /// Acquire a write lock on the config (for runtime mutation via admin API).
    pub async fn write(&self) -> tokio::sync::RwLockWriteGuard<'_, GatewayConfig> {
        self.inner.write().await
    }

    /// Returns the path to the config file, or `None` if the config is in-memory.
    pub fn path(&self) -> Option<&Path> {
        self.config_path.as_ref().map(|p| p.as_path())
    }

    /// Returns `true` if the config was loaded from a real file (not in-memory).
    pub fn is_file_backed(&self) -> bool {
        self.config_path.is_some()
    }
}

/// Load the initial configuration from a TOML file and return a `ConfigHandle`.
pub fn load_config(path: impl AsRef<Path>) -> Result<ConfigHandle, sag_core::AppError> {
    let path = path.as_ref().to_path_buf();
    let content = std::fs::read_to_string(&path).map_err(|e| {
        sag_core::AppError::Config(format!("failed to read config {}: {e}", path.display()))
    })?;

    let config: GatewayConfig = toml::from_str(&content)
        .map_err(|e| sag_core::AppError::Config(format!("failed to parse config: {e}")))?;

    config
        .validate()
        .map_err(|errors| sag_core::AppError::Config(errors.join("; ")))?;

    info!(?config, "configuration loaded");
    Ok(ConfigHandle {
        inner: Arc::new(RwLock::new(config)),
        config_path: Some(Arc::new(path)),
    })
}

/// Handle for the hot-reload background task, including a shutdown signal.
pub struct HotReloadHandle {
    /// The spawned tokio task.
    pub task: tokio::task::JoinHandle<()>,
    /// Send `()` to request graceful shutdown of the watcher.
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

impl HotReloadHandle {
    /// Signal the watcher to shut down gracefully and wait for it to finish.
    ///
    /// This ensures the `notify::Watcher` is properly dropped, which stops
    /// OS-level file-watching threads (FSEvents on macOS, inotify on Linux).
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), self.task).await;
    }
}

/// Spawn a background task that watches the config file for changes and reloads it.
/// The returned handle will reflect the latest config. If `handle` is in-memory
/// (no associated file path), the watcher task exits immediately.
pub fn spawn_hot_reload(handle: ConfigHandle) -> HotReloadHandle {
    let inner = handle.inner.clone();
    let path = handle.config_path.clone();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let task = tokio::spawn(async move {
        // If there is no backing file, skip the watcher entirely.
        let Some(path) = path else {
            info!("hot-reload skipped for in-memory config");
            return;
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<notify::Event, notify::Error>>(16);

        let mut watcher = match RecommendedWatcher::new(
            move |res| {
                let _ = tx.blocking_send(res);
            },
            notify::Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                error!("failed to create file watcher: {e}");
                return;
            }
        };

        if let Err(e) = watcher.watch(&path, RecursiveMode::NonRecursive) {
            error!("failed to watch config file {}: {e}", path.display());
            return;
        }

        info!("hot-reload watcher started for {}", path.display());

        // Debounce: wait 500ms after last event before reloading
        let mut debounce = tokio::time::Instant::now();

        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    info!("hot-reload watcher shutting down");
                    // Drop the watcher explicitly — this stops OS file-watch threads.
                    drop(watcher);
                    return;
                }
                event_res = rx.recv() => {
                    match event_res {
                        Some(Ok(event)) => {
                            if is_modify_event(&event) {
                                let now = tokio::time::Instant::now();
                                if now.duration_since(debounce).as_millis() < 500 {
                                    continue; // debounce
                                }
                                debounce = now;

                                // Small delay to let the file system settle
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;

                                match reload_config(&path).await {
                                    Ok(new_config) => {
                                        let mut guard = inner.write().await;
                                        *guard = new_config;
                                        info!("configuration hot-reloaded");
                                    }
                                    Err(e) => {
                                        warn!("failed to reload config: {e}");
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            error!("file watcher error: {e}");
                        }
                        None => {
                            // All senders dropped — watcher was destroyed.
                            // This is a belt-and-suspenders safety net; in
                            // practice the shutdown signal arm handles exit.
                            info!("file watcher channel closed, exiting");
                            return;
                        }
                    }
                }
            }
        }
    });

    HotReloadHandle { task, shutdown_tx }
}

fn is_modify_event(event: &Event) -> bool {
    matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_))
}

async fn reload_config(path: &std::path::Path) -> Result<GatewayConfig, sag_core::AppError> {
    let content = tokio::fs::read_to_string(path).await.map_err(|e| {
        sag_core::AppError::Config(format!("failed to read config for reload: {e}"))
    })?;
    let config: GatewayConfig = toml::from_str(&content)
        .map_err(|e| sag_core::AppError::Config(format!("failed to parse config: {e}")))?;

    config
        .validate()
        .map_err(|errors| sag_core::AppError::Config(errors.join("; ")))?;

    Ok(config)
}

/// Validate that a JSON map doesn't contain null values (which can't be stored as TOML).
fn check_json_map(
    map: &HashMap<String, serde_json::Value>,
    context: &str,
) -> Result<(), sag_core::AppError> {
    for (key, val) in map {
        match val {
            serde_json::Value::Null => {
                return Err(sag_core::AppError::Config(format!(
                    "{context}.{key}: null value cannot be stored as TOML, remove the key or set an explicit value"
                )));
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    if item.is_null() {
                        return Err(sag_core::AppError::Config(format!(
                            "{context}.{key}[]: null value in array cannot be stored as TOML"
                        )));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Persist the in-memory `GatewayConfig` back to the TOML file.
///
/// Uses `toml_edit` to parse the existing file and performs fine-grained in-place
/// updates for `[providers]` section (preserving comments and formatting of
/// unaffected fields). The `[[routes]]` section is rebuilt entirely (accepting
/// comment loss on routes).
pub fn save_config_to_file(
    config: &GatewayConfig,
    path: &std::path::Path,
) -> Result<(), sag_core::AppError> {
    // Validate no null values before writing
    for (name, provider) in &config.providers {
        check_json_map(&provider.body_overrides, &format!("providers.{name}.body_overrides"))?;
    }
    for (i, route) in config.routes.iter().enumerate() {
        for (j, target) in route.targets.iter().enumerate() {
            check_json_map(&target.extra, &format!("routes[{i}].targets[{j}].extra"))?;
        }
    }

    // Read the existing file to preserve formatting of non-mutated sections.
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .map_err(|e| sag_core::AppError::Config(format!("failed to parse existing config: {e}")))?;

    // ── Update [providers] — fine-grained in-place ──
    // Remove providers no longer in memory
    if let Some(providers_table) = doc.get_mut("providers").and_then(|i| i.as_table_mut()) {
        let stale: Vec<String> = providers_table.iter().map(|(k, _)| k.to_string()).collect();
        for name in &stale {
            if !config.providers.contains_key(name) {
                providers_table.remove(name);
            }
        }
    }

    for (name, provider) in &config.providers {
        let provider_exists = doc.get("providers")
            .and_then(|t| t.as_table())
            .and_then(|t| t.get(name))
            .is_some();

        if !provider_exists {
            // New provider — serialize via to_document (no existing format to preserve)
            let pd = toml_edit::ser::to_document(provider)
                .map_err(|e| sag_core::AppError::Config(format!("failed to serialize provider '{name}': {e}")))?;
            doc["providers"][name] = toml_edit::Item::Table((*pd).clone());
        } else {
            // Existing provider — per-field update
            // api_keys
            {
                let mut arr = toml_edit::Array::new();
                for key in &provider.api_keys {
                    arr.push(key.as_str());
                }
                doc["providers"][name]["api_keys"] = toml_edit::value(arr);
            }
            // enabled_models
            {
                let mut arr = toml_edit::Array::new();
                for m in &provider.enabled_models {
                    arr.push(m.as_str());
                }
                doc["providers"][name]["enabled_models"] = toml_edit::value(arr);
            }
            // scalars
            doc["providers"][name]["base_url"] = toml_edit::value(provider.base_url.as_str());
            doc["providers"][name]["timeout_secs"] = toml_edit::value(provider.timeout_secs as i64);
            doc["providers"][name]["provider_type"] = toml_edit::value(provider.provider_type.as_str());
            doc["providers"][name]["key_strategy"] = toml_edit::value(match provider.key_strategy {
                KeyStrategy::FillFirst => "fill_first",
                KeyStrategy::RoundRobin => "round_robin",
            });
            doc["providers"][name]["enabled"] = toml_edit::value(provider.enabled);
            // headers — inline table
            {
                let mut table = toml_edit::InlineTable::new();
                for (k, v) in &provider.headers {
                    table.insert(k, v.as_str());
                }
                doc["providers"][name]["headers"] = toml_edit::value(table);
            }
            // body_overrides — inline table via serde (nulls already checked above)
            if provider.body_overrides.is_empty() {
                doc["providers"][name]
                    .as_table_mut()
                    .and_then(|t| t.remove("body_overrides"));
            } else {
                let val = toml_edit::ser::to_value(&provider.body_overrides)
                    .map_err(|e| sag_core::AppError::Config(format!("failed to serialize body_overrides for '{name}': {e}")))?;
                doc["providers"][name]["body_overrides"] = toml_edit::Item::Value(val);
            }
        }
    }

    // ── Replace [[routes]] section (full rebuild, accepting comment loss) ──
    if config.routes.is_empty() {
        doc.remove("routes");
    } else {
        let mut routes_array = toml_edit::ArrayOfTables::new();
        for route in &config.routes {
            let route_doc = toml_edit::ser::to_document(route).map_err(|e| {
                sag_core::AppError::Config(format!("failed to serialize route: {e}"))
            })?;
            let route_tbl = (*route_doc).clone();
            routes_array.push(route_tbl);
        }
        doc["routes"] = toml_edit::Item::ArrayOfTables(routes_array);
    }

    // ── Write back ──
    std::fs::write(path, doc.to_string())
        .map_err(|e| sag_core::AppError::Config(format!("failed to write config file: {e}")))?;

    info!("config persisted to {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use crate::config::*;

    static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Create a temp TOML file with the given content and return its path.
    fn temp_toml(content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let id = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let file_path = dir.join(format!("sag-test-{}-{}.toml", std::process::id(), id));
        let mut file = std::fs::File::create(&file_path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file_path
    }

    fn valid_toml() -> &'static str {
        r#"[server]
host = "127.0.0.1"
port = 9090

[gateway_auth]
api_keys = ["test-key"]

[providers.openai]
api_keys = ["sk-test"]
"#
    }

    #[test]
    fn load_config_success() {
        let path = temp_toml(valid_toml());
        let handle = load_config(&path).unwrap();
        assert_eq!(handle.path(), Some(path.as_path()));

        // Can read the config synchronously via try_read
        let config = handle.inner.try_read().unwrap();
        assert_eq!(config.server.port, 9090);
        assert_eq!(
            config.gateway_auth.as_ref().unwrap().api_keys,
            vec!["test-key"]
        );
    }

    #[test]
    fn load_config_missing_file() {
        let result = load_config("/tmp/does-not-exist-sag-test.toml");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("failed to read config"));
    }

    #[test]
    fn load_config_invalid_toml() {
        let path = temp_toml("this is not valid toml {{{ ");
        let result = load_config(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("failed to parse config"));
    }

    #[test]
    fn load_config_missing_providers() {
        let path = temp_toml("[server]\n");
        let result = load_config(&path);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("failed to parse config")
        );
    }

    #[test]
    fn config_handle_clone_shares_state() {
        let path = temp_toml(valid_toml());
        let h1 = load_config(&path).unwrap();
        let h2 = h1.clone();

        let c1 = h1.inner.try_read().unwrap();
        let c2 = h2.inner.try_read().unwrap();
        assert_eq!(c1.server.port, c2.server.port);
        // They point to the same Arc — same address
        let ptr1 = &*c1 as *const GatewayConfig;
        let ptr2 = &*c2 as *const GatewayConfig;
        assert_eq!(ptr1, ptr2, "Clone should share the same inner Arc");
    }

    #[test]
    fn is_modify_event_detects_modify() {
        let event = Event::new(EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        )));
        assert!(is_modify_event(&event));
    }

    #[test]
    fn is_modify_event_detects_create() {
        let event = Event::new(EventKind::Create(notify::event::CreateKind::File));
        assert!(is_modify_event(&event));
    }

    #[test]
    fn is_modify_event_ignores_access() {
        let event = Event::new(EventKind::Access(notify::event::AccessKind::Close(
            notify::event::AccessMode::Write,
        )));
        assert!(!is_modify_event(&event));
    }

    #[test]
    fn is_modify_event_ignores_remove() {
        let event = Event::new(EventKind::Remove(notify::event::RemoveKind::File));
        assert!(!is_modify_event(&event));
    }

    // ── save_config_to_file tests ──

    #[test]
    fn save_preserves_provider_subtable_format() {
        let content = r#"[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("[providers.openai]"),
            "sub-table format should be preserved, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_preserves_comments_in_providers() {
        let content = r#"[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
# This is a user comment
api_keys = ["sk-test"]
# Another comment
base_url = "https://custom.url"
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("# This is a user comment"),
            "comment should be preserved, got:\n{saved}"
        );
        assert!(
            saved.contains("# Another comment"),
            "second comment should be preserved, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_preserves_unrelated_provider() {
        let content = r#"[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]

[providers.anthropic]
provider_type = "anthropic"
api_keys = ["sk-ant"]
base_url = "https://api.anthropic.com"
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("[providers.openai]"),
            "openai should remain sub-table, got:\n{saved}"
        );
        assert!(
            saved.contains("[providers.anthropic]"),
            "anthropic should remain sub-table, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn none_options_not_written() {
        let path = temp_toml("[server]\nhost = \"127.0.0.1\"\nport = 9090\n\n[providers]\n");
        let config = GatewayConfig {
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 9090,
            },
            analytics: None,
            gateway_auth: None,
            providers: HashMap::from([(
                "openai".into(),
                ProviderConfig {
                    provider_type: ProviderType::OpenAI,
                    api_keys: vec!["sk-test".into()],
                    key_strategy: KeyStrategy::FillFirst,
                    base_url: "https://api.openai.com/v1".into(),
                    timeout_secs: 120,
                    headers: HashMap::new(),
                    body_overrides: HashMap::new(),
                    enabled: true,
                    enabled_models: vec![],
                },
            )]),
            retries: 2,
            routes: vec![RouteConfig {
                model_name: "gpt-4".into(),
                api_endpoint: "/chat/completions".into(),
                strategy: Strategy::FillFirst,
                retries: None,
                targets: vec![RouteTarget {
                    provider: "openai".into(),
                    real_model: None,
                    priority: 0,
                    retries: None,
                    extra: HashMap::new(),
                }],
            }],
        };

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains("real_model"),
            "real_model=None should not be written, got:\n{saved}"
        );
        assert!(
            !saved.contains("retries"),
            "retries=None should not be written, got:\n{saved}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn default_fields_not_written() {
        let path = temp_toml("[server]\nhost = \"127.0.0.1\"\nport = 9090\n\n[providers]\n");
        let config = GatewayConfig {
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 9090,
            },
            analytics: None,
            gateway_auth: None,
            providers: HashMap::from([(
                "openai".into(),
                ProviderConfig {
                    provider_type: ProviderType::OpenAI,
                    api_keys: vec!["sk-test".into()],
                    key_strategy: KeyStrategy::FillFirst,
                    base_url: "https://api.openai.com/v1".into(),
                    timeout_secs: 120,
                    headers: HashMap::new(),
                    body_overrides: HashMap::new(),
                    enabled: true,
                    enabled_models: vec![],
                },
            )]),
            retries: 2,
            routes: vec![],
        };

        save_config_to_file(&config, &path).unwrap();

        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            !saved.contains("provider_type"),
            "default provider_type should not be written, got:\n{saved}"
        );
        assert!(
            !saved.contains("key_strategy"),
            "default key_strategy should not be written"
        );
        assert!(
            !saved.contains("headers"),
            "empty headers should not be written"
        );
        assert!(
            !saved.contains("body_overrides"),
            "empty body_overrides should not be written"
        );
        assert!(
            !saved.contains("enabled_models"),
            "empty enabled_models should not be written"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let content = r#"[server]
host = "127.0.0.1"
port = 8080

[gateway_auth]
api_keys = ["key1"]

[providers.openai]
api_keys = ["sk-test"]

[providers.anthropic]
provider_type = "anthropic"
api_keys = ["sk-ant"]
base_url = "https://api.anthropic.com"
timeout_secs = 60

[[routes]]
model_name = "gpt-4o"
targets = [
    { provider = "openai", real_model = "gpt-4o" },
    { provider = "anthropic", priority = 5 },
]
"#;
        let path = temp_toml(content);
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap().clone();
        drop(handle);

        save_config_to_file(&config, &path).unwrap();

        // Reload and verify
        let reloaded_handle = load_config(&path).unwrap();
        let reloaded = reloaded_handle.inner.try_read().unwrap();

        assert_eq!(reloaded.providers.len(), config.providers.len());
        for (name, p1) in &config.providers {
            let p2 = reloaded.providers.get(name).unwrap();
            assert_eq!(p1.provider_type, p2.provider_type);
            assert_eq!(p1.api_keys, p2.api_keys);
            assert_eq!(p1.key_strategy, p2.key_strategy);
            assert_eq!(p1.base_url, p2.base_url);
            assert_eq!(p1.timeout_secs, p2.timeout_secs);
            assert_eq!(p1.headers, p2.headers);
            assert_eq!(p1.body_overrides, p2.body_overrides);
            assert_eq!(p1.enabled, p2.enabled);
            assert_eq!(p1.enabled_models, p2.enabled_models);
        }

        assert_eq!(reloaded.routes.len(), config.routes.len());
        for (r1, r2) in config.routes.iter().zip(reloaded.routes.iter()) {
            assert_eq!(r1.model_name, r2.model_name);
            assert_eq!(r1.api_endpoint, r2.api_endpoint);
            assert_eq!(r1.strategy, r2.strategy);
            assert_eq!(r1.retries, r2.retries);
            assert_eq!(r1.targets.len(), r2.targets.len());
            for (t1, t2) in r1.targets.iter().zip(r2.targets.iter()) {
                assert_eq!(t1.provider, t2.provider);
                assert_eq!(t1.real_model, t2.real_model);
                assert_eq!(t1.priority, t2.priority);
                assert_eq!(t1.retries, t2.retries);
                assert_eq!(t1.extra, t2.extra);
            }
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn extra_with_null_returns_error() {
        let path = temp_toml("[server]\nhost = \"127.0.0.1\"\nport = 9090\n\n[providers]\n");
        let config = GatewayConfig {
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 9090,
            },
            analytics: None,
            gateway_auth: None,
            providers: HashMap::from([(
                "openai".into(),
                ProviderConfig {
                    provider_type: ProviderType::OpenAI,
                    api_keys: vec!["sk-test".into()],
                    key_strategy: KeyStrategy::FillFirst,
                    base_url: "https://api.openai.com/v1".into(),
                    timeout_secs: 120,
                    headers: HashMap::new(),
                    body_overrides: HashMap::new(),
                    enabled: true,
                    enabled_models: vec![],
                },
            )]),
            retries: 2,
            routes: vec![RouteConfig {
                model_name: "gpt-4".into(),
                api_endpoint: "/chat/completions".into(),
                strategy: Strategy::FillFirst,
                retries: None,
                targets: vec![RouteTarget {
                    provider: "openai".into(),
                    real_model: None,
                    priority: 0,
                    retries: None,
                    extra: HashMap::from([("temperature".into(), serde_json::Value::Null)]),
                }],
            }],
        };

        let result = save_config_to_file(&config, &path);
        assert!(result.is_err(), "save with null in extra should fail");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("null"),
            "error should mention 'null': {err_msg}"
        );

        let _ = std::fs::remove_file(&path);
    }
}
