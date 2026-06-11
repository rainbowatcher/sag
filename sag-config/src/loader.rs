use std::path::{Path, PathBuf};
use std::sync::Arc;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::config::GatewayConfig;

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

    config.validate().map_err(|errors| {
        sag_core::AppError::Config(errors.join("; "))
    })?;

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

    config.validate().map_err(|errors| {
        sag_core::AppError::Config(errors.join("; "))
    })?;

    Ok(config)
}

/// Persist the in-memory `GatewayConfig` back to the TOML file using `toml_edit`.
///
/// Uses `toml_edit` to parse the existing file (preserving comments and formatting)
/// and only overwrites the `[providers]` and `[[routes]]` sections with the
/// current in-memory state. All other sections (server, gateway_auth, analytics)
/// retain their original formatting and comments.
pub fn save_config_to_file(
    config: &GatewayConfig,
    path: &std::path::Path,
) -> Result<(), sag_core::AppError> {
    // Read the existing file to preserve formatting of non-mutated sections.
    // If the file doesn't exist yet, start with an empty document.
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .map_err(|e| sag_core::AppError::Config(format!("failed to parse existing config: {e}")))?;

    // ── Replace [providers] section ──
    let providers_doc = toml_edit::ser::to_document(&config.providers)
        .map_err(|e| sag_core::AppError::Config(format!("failed to serialize providers: {e}")))?;
    // DocumentMut derefs to Table — clone it for insertion
    let providers_table = (*providers_doc).clone();
    doc["providers"] = toml_edit::Item::Table(providers_table);

    // ── Replace [[routes]] section ──
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
}
