use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use tracing::{error, info, warn};

use crate::loader::handle::ConfigHandle;
use crate::loader::load::reload_config;

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
                                        routes_version.fetch_add(1, std::sync::atomic::Ordering::Release);
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
                            // This is a belt-and-suspendors safety net; in
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
