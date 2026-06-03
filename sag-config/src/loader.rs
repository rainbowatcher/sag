use std::path::{Path, PathBuf};
use std::sync::Arc;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::config::GatewayConfig;

/// A shared, hot-reloadable configuration handle.
#[derive(Clone)]
pub struct ConfigHandle {
    inner: Arc<RwLock<GatewayConfig>>,
    config_path: Arc<PathBuf>,
}

impl ConfigHandle {
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
