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
    config_path: Arc<PathBuf>,
}

impl ConfigHandle {
    /// Build a `ConfigHandle` from an in-memory `GatewayConfig` (for testing / programmatic use).
    /// Uses a placeholder path since no file is involved.
    pub fn from_config(config: GatewayConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(config)),
            config_path: Arc::new(PathBuf::from("<memory>")),
        }
    }

    /// Read the current config (acquires a read lock).
    pub async fn read(&self) -> tokio::sync::RwLockReadGuard<'_, GatewayConfig> {
        self.inner.read().await
    }

    /// Returns the path to the config file.
    pub fn path(&self) -> &Path {
        &self.config_path
    }
}

/// Load the initial configuration from a TOML file and return a `ConfigHandle`.
pub fn load_config(path: impl AsRef<Path>) -> Result<ConfigHandle, sag_core::AppError> {
    let path = path.as_ref().to_path_buf();
    let content = std::fs::read_to_string(&path)
        .map_err(|e| sag_core::AppError::Config(format!("failed to read config {}: {e}", path.display())))?;

    let config: GatewayConfig = toml::from_str(&content)
        .map_err(|e| sag_core::AppError::Config(format!("failed to parse config: {e}")))?;

    info!(?config, "configuration loaded");
    Ok(ConfigHandle {
        inner: Arc::new(RwLock::new(config)),
        config_path: Arc::new(path),
    })
}

/// Spawn a background task that watches the config file for changes and reloads it.
/// The returned handle will reflect the latest config.
pub fn spawn_hot_reload(handle: ConfigHandle) -> tokio::task::JoinHandle<()> {
    let inner = handle.inner.clone();
    let path = handle.config_path.clone();

    tokio::spawn(async move {
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

        while let Some(event_res) = rx.recv().await {
            match event_res {
                Ok(event) => {
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
                Err(e) => {
                    error!("file watcher error: {e}");
                }
            }
        }
    })
}

fn is_modify_event(event: &Event) -> bool {
    matches!(
        event.kind,
        EventKind::Modify(_) | EventKind::Create(_)
    )
}

async fn reload_config(path: &std::path::Path) -> Result<GatewayConfig, sag_core::AppError> {
    let content = tokio::fs::read_to_string(path).await.map_err(|e| {
        sag_core::AppError::Config(format!("failed to read config for reload: {e}"))
    })?;
    toml::from_str(&content).map_err(|e| sag_core::AppError::Config(format!("failed to parse config: {e}")))
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

[providers]
"#
    }

    #[test]
    fn load_config_success() {
        let path = temp_toml(valid_toml());
        let handle = load_config(&path).unwrap();
        assert_eq!(handle.path(), path.as_path());

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
        assert!(result.unwrap_err().to_string().contains("failed to parse config"));
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
