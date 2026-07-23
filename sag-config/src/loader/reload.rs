use std::path::Path;
use std::sync::Arc;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use tracing::{error, info, warn};

use crate::loader::handle::ConfigHandle;
use crate::loader::load::reload_config;

/// Handler invoked by the hot-reload watcher after a debounced file event.
///
/// When registered via [`spawn_hot_reload`], the watcher delegates all
/// serialized re-read, validation, and atomic runtime publication to the
/// handler. This lets the gateway coordinate config reload with the
/// provider-registry sync (see `ConfigMutationCoordinator` in `sag-state`)
/// without `sag-config` depending on `sag-state` or `sag-providers`.
///
/// When `None` is passed to `spawn_hot_reload`, the watcher falls back to
/// the legacy behavior of replacing only the in-memory config and bumping
/// `routes_version`.
#[async_trait::async_trait]
pub trait ConfigReloadHandler: Send + Sync {
    /// Re-read, validate, and atomically publish the config at `path`.
    ///
    /// Called after the watcher has debounced an event. The handler owns
    /// serialized re-read/validation and atomic runtime publication
    /// (config + provider registry + version bump). On `Err` the watcher
    /// logs the rejection and leaves the running config untouched.
    async fn reload_from_path(&self, path: &Path) -> sag_core::Result<()>;
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
///
/// If `handler` is `Some`, file events are delegated to the handler which
/// owns the full reload protocol (re-read, validate, publish config + registry).
/// If `handler` is `None`, the watcher falls back to legacy config-only reload
/// (replaces the in-memory `GatewayConfig` and bumps `routes_version`).
///
/// The returned handle will reflect the latest config. If `handle` is in-memory
/// (no associated file path), the watcher task exits immediately.
pub fn spawn_hot_reload(
    handle: ConfigHandle,
    handler: Option<Arc<dyn ConfigReloadHandler>>,
) -> HotReloadHandle {
    let inner = handle.inner.clone();
    let path = handle.config_path.clone();
    let routes_version = handle.routes_version.clone();
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
                    // Drop the watcher explicitly - this stops OS file-watch threads.
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

                                if let Some(ref handler) = handler {
                                    // Delegated reload: handler owns re-read,
                                    // validation, and atomic publication of
                                    // config + provider registry + version.
                                    match handler.reload_from_path(&path).await {
                                        Ok(()) => {
                                            info!("configuration hot-reloaded");
                                        }
                                        Err(e) => {
                                            warn!("failed to reload config: {e}");
                                        }
                                    }
                                } else {
                                    // Legacy config-only reload (backward compat,
                                    // tests, and in-memory scenarios that still
                                    // have a file path).
                                    match reload_config(&path).await {
                                        Ok(new_config) => {
                                            let mut guard = inner.write().await;
                                            *guard = new_config;
                                            routes_version.fetch_add(1, std::sync::atomic::Ordering::Release);
                                            info!("configuration hot-reloaded");
                                        }
                                        Err(e) => {
                                            warn!("failed to reload config: {e}");
                                        }
                                    }
                                }
                            }
                        }
                        Some(Err(e)) => {
                            error!("file watcher error: {e}");
                        }
                        None => {
                            // All senders dropped - watcher was destroyed.
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

#[cfg(test)]
mod tests {
    use super::*;

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
