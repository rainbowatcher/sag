use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use tracing::{debug, error, info, warn};

use crate::loader::handle::ConfigHandle;

/// Trailing-debounce window: a reload is triggered only after the config file
/// has been quiet for this long. Trailing (not leading) — every relevant
/// event resets the window, so we always read the *final* version of a burst
/// of writes (e.g. an editor's staged atomic save) instead of a transient
/// intermediate state that the old leading-edge debounce could have applied
/// and then never revisited.
const DEBOUNCE: Duration = Duration::from_millis(300);

/// Extra settle time granted after the debounce window before the reload, so
/// an in-progress write has a moment to finish and so we can drain any
/// straggler events that arrive right at the boundary.
const SETTLE: Duration = Duration::from_millis(100);

/// Poll interval for the trailing-debounce ticker while an event is pending.
const DEBOUNCE_TICK: Duration = Duration::from_millis(100);

/// Bounded retries for a reload that fails transiently (a read landing on a
/// truncated/invalid intermediate state, or a temporarily-missing file during
/// an atomic replace). After `MAX_RELOAD_RETRIES` further attempts the
/// watcher gives up and waits for the next file event to try again.
const MAX_RELOAD_RETRIES: usize = 2;
const RETRY_DELAY: Duration = Duration::from_millis(200);

/// Handler invoked by the hot-reload watcher after a debounced file event.
///
/// When registered via [`spawn_hot_reload`], the watcher delegates all
/// serialized re-read, validation, and atomic runtime publication to the
/// handler. This lets the gateway coordinate config reload with the
/// provider-registry sync (see `ConfigMutationCoordinator` in `sag-state`)
/// without `sag-config` depending on `sag-state` or `sag-providers`.
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
        match tokio::time::timeout(std::time::Duration::from_secs(5), self.task).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => error!(error = %e, "hot-reload watcher task panicked"),
            // Timeout: task dropped without completing — Watcher::Drop may
            // never run and OS watch threads can block process exit.
            Err(_) => error!("hot-reload watcher did not stop within 5s"),
        }
    }
}

/// Return the directory to watch for `config_path` — its parent, defaulting
/// to `.` when the path is a bare filename with no directory component.
///
/// The watcher watches the parent **directory** rather than the file itself:
/// a file-level watch is bound to the inode, so an atomic temp-file + rename
/// save (what [`crate::loader::save::save_config_to_file`] and most editors
/// do) replaces the inode and silently kills the watch, so later external
/// edits are never seen. Directory watches survive renames and report them
/// as `Create`/`Remove` events for the path, which we filter by file name.
fn watch_dir_for(config_path: &Path) -> Option<&Path> {
    let parent = config_path.parent()?;
    if parent.as_os_str().is_empty() {
        Some(Path::new("."))
    } else {
        Some(parent)
    }
}

/// Returns `true` if `event` concerns a content change to `config_path`.
///
/// We watch the parent directory, so events arrive with paths relative to it;
/// match either the full path or the file name (robust to normalization
/// differences between how the config path was stored at load time and how
/// the OS reports the event). Read-only `Access` events are ignored — they
/// never change file content — while `Create`/`Modify`/`Remove`/`Rename`
/// (and unknown `Any`/`Other` kinds) are all treated as relevant so no
/// genuine edit is missed.
fn event_targets_config(event: &Event, config_path: &Path) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    let file_name = config_path.file_name();
    event
        .paths
        .iter()
        .any(|p| p == config_path || (file_name.is_some() && p.file_name() == file_name))
}

/// Perform a single reload of the config at `path` via the handler
/// (coordinated reload: re-read + validate + atomic publication of config,
/// provider registry, and version bump). Returns `true` on success.
async fn reload_once(handler: &Arc<dyn ConfigReloadHandler>, path: &Path) -> bool {
    // Delegated reload: handler owns re-read, validation, and atomic
    // publication of config + provider registry + version.
    match handler.reload_from_path(path).await {
        Ok(()) => {
            info!("configuration hot-reloaded");
            true
        }
        Err(e) => {
            warn!("failed to reload config: {e}");
            false
        }
    }
}

/// Outcome of consuming one watcher-channel message: keep looping or exit.
enum LoopAction {
    Continue,
    Exit,
}

/// Classify a watcher-channel message: update `last_event` on relevant
/// config events, log watcher errors, and signal exit when the channel closes.
fn classify_watcher_event(
    event_res: Option<Result<Event, notify::Error>>,
    path: &Path,
    last_event: &mut Option<tokio::time::Instant>,
) -> LoopAction {
    match event_res {
        Some(Ok(event)) => {
            if event_targets_config(&event, path) {
                // Trailing debounce: every relevant event pushes the reload
                // deadline out so we never read an intermediate version.
                *last_event = Some(tokio::time::Instant::now());
            }
            LoopAction::Continue
        }
        Some(Err(e)) => {
            error!("file watcher error: {e}");
            LoopAction::Continue
        }
        None => {
            // All senders dropped - watcher was destroyed.
            info!("file watcher channel closed, exiting");
            LoopAction::Exit
        }
    }
}

/// Debounce-tick body: once the file has been quiet for `DEBOUNCE`, settle,
/// drain/rearm on straggler events, then reload with bounded retries.
/// Returns early (no-op) while the debounce window is still open.
async fn try_debounced_reload(
    last_event: &mut Option<tokio::time::Instant>,
    rx: &mut tokio::sync::mpsc::Receiver<Result<Event, notify::Error>>,
    handler: &Arc<dyn ConfigReloadHandler>,
    path: &Path,
) {
    let since = last_event
        .expect("debounce arm guarded by last_event.is_some()")
        .elapsed();
    if since < DEBOUNCE {
        return;
    }
    *last_event = None;

    // Let the filesystem settle, then drain any events that arrived right
    // at the boundary. A new relevant event during settle means the file is
    // still being written — re-arm instead of reloading an intermediate state.
    tokio::time::sleep(SETTLE).await;
    let mut rearm = false;
    while let Ok(event_res) = rx.try_recv() {
        match event_res {
            Ok(event) if event_targets_config(&event, path) => {
                rearm = true;
            }
            Err(e) => {
                // Must not be silently dropped, but must not re-arm either.
                error!("file watcher error: {e}");
            }
            Ok(_) => {}
        }
    }
    if rearm {
        *last_event = Some(tokio::time::Instant::now());
        return;
    }

    // Bounded retry for transient failures (truncated intermediate state,
    // file briefly absent during an atomic replace).
    let mut attempts = 1;
    let mut ok = reload_once(handler, path).await;
    while !ok && attempts <= MAX_RELOAD_RETRIES {
        tokio::time::sleep(RETRY_DELAY).await;
        ok = reload_once(handler, path).await;
        attempts += 1;
    }
    if !ok {
        warn!("config reload failed after {attempts} attempts; will retry on next file event");
    }
}

/// Spawn a background task that watches the config file for changes and reloads it.
///
/// File events are delegated to `handler`, which owns the full reload
/// protocol (re-read, validate, publish config + provider registry + version).
///
/// The returned handle will reflect the latest config. If `handle` is in-memory
/// (no associated file path), the watcher task exits immediately.
///
/// # Watch semantics
///
/// - The parent **directory** is watched, so atomic temp-file + rename saves
///   are detected (a file-level watch is bound to the inode and silently dies
///   when the file is replaced).
/// - Events are filtered to the config file and `Access` reads are ignored.
/// - A **trailing** debounce merges bursts of events: reload fires only after
///   the file has been quiet for `DEBOUNCE`, so the final version is always
///   what gets read.
/// - A reload that fails transiently (invalid intermediate content, file
///   briefly missing mid-replace) is retried up to `MAX_RELOAD_RETRIES`
///   times before the watcher waits for the next file event.
pub fn spawn_hot_reload(
    handle: ConfigHandle,
    handler: Arc<dyn ConfigReloadHandler>,
) -> HotReloadHandle {
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
                if tx.blocking_send(res).is_err() {
                    debug!("config watcher event dropped (receiver gone or channel full)");
                }
            },
            notify::Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                error!("failed to create file watcher: {e}");
                return;
            }
        };

        // Watch the parent directory (see `watch_dir_for` for the rationale
        // — atomic renames must not silently kill the watch).
        let Some(watch_dir) = watch_dir_for(&path) else {
            error!(
                "config path {} has no parent directory to watch",
                path.display()
            );
            return;
        };
        if let Err(e) = watcher.watch(watch_dir, RecursiveMode::NonRecursive) {
            error!(
                "failed to watch config directory {} for {}: {e}",
                watch_dir.display(),
                path.display()
            );
            return;
        }

        info!(
            "hot-reload watcher started for {} (watching directory {})",
            path.display(),
            watch_dir.display()
        );

        // Trailing-debounce state: instant of the most recent relevant event,
        // or `None` when no reload is pending.
        let mut last_event: Option<tokio::time::Instant> = None;

        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    info!("hot-reload watcher shutting down");
                    // Drop the watcher explicitly - this stops OS file-watch threads.
                    drop(watcher);
                    return;
                }
                event_res = rx.recv() => {
                    if matches!(
                        classify_watcher_event(event_res, &path, &mut last_event),
                        LoopAction::Exit
                    ) {
                        return;
                    }
                }
                _ = tokio::time::sleep(DEBOUNCE_TICK), if last_event.is_some() => {
                    try_debounced_reload(
                        &mut last_event,
                        &mut rx,
                        &handler,
                        &path,
                    )
                    .await;
                }
            }
        }
    });

    HotReloadHandle { task, shutdown_tx }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn event(kind: EventKind, path: &str) -> Event {
        Event::new(kind).add_path(PathBuf::from(path))
    }

    #[test]
    fn event_targets_config_matches_exact_path() {
        let e = event(
            EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
            "/etc/sag/gateway.toml",
        );
        assert!(event_targets_config(&e, Path::new("/etc/sag/gateway.toml")));
    }

    #[test]
    fn event_targets_config_matches_create() {
        // An atomic temp-file + rename save surfaces as a Create event for
        // the config path in the watched parent directory.
        let e = event(
            EventKind::Create(notify::event::CreateKind::File),
            "/etc/sag/gateway.toml",
        );
        assert!(event_targets_config(&e, Path::new("/etc/sag/gateway.toml")));
    }

    #[test]
    fn event_targets_config_matches_remove() {
        let e = event(
            EventKind::Remove(notify::event::RemoveKind::File),
            "/etc/sag/gateway.toml",
        );
        assert!(event_targets_config(&e, Path::new("/etc/sag/gateway.toml")));
    }

    #[test]
    fn event_targets_config_matches_rename() {
        let e = event(
            EventKind::Modify(notify::event::ModifyKind::Name(
                notify::event::RenameMode::Both,
            )),
            "/etc/sag/gateway.toml",
        );
        assert!(event_targets_config(&e, Path::new("/etc/sag/gateway.toml")));
    }

    #[test]
    fn event_targets_config_matches_by_file_name() {
        // Events reported relative to the watched directory may not exactly
        // equal the stored config path; the file name must still match.
        let e = event(
            EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
            "gateway.toml",
        );
        assert!(event_targets_config(&e, Path::new("/etc/sag/gateway.toml")));
    }

    #[test]
    fn event_targets_config_ignores_access() {
        let e = event(
            EventKind::Access(notify::event::AccessKind::Close(
                notify::event::AccessMode::Write,
            )),
            "/etc/sag/gateway.toml",
        );
        assert!(!event_targets_config(
            &e,
            Path::new("/etc/sag/gateway.toml")
        ));
    }

    #[test]
    fn event_targets_config_ignores_unrelated_paths() {
        let e = event(
            EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
            "/etc/sag/other.toml",
        );
        assert!(!event_targets_config(
            &e,
            Path::new("/etc/sag/gateway.toml")
        ));
    }

    #[test]
    fn event_targets_config_ignores_temp_file_of_other_process() {
        // SAG's own atomic save writes a `.gateway.toml.tmp.<pid>` sibling
        // first; those events must not be mistaken for a change to the
        // config file itself.
        let e = event(
            EventKind::Create(notify::event::CreateKind::File),
            "/etc/sag/.gateway.toml.tmp.1234",
        );
        assert!(!event_targets_config(
            &e,
            Path::new("/etc/sag/gateway.toml")
        ));
    }

    #[test]
    fn watch_dir_for_returns_parent() {
        assert_eq!(
            watch_dir_for(Path::new("/etc/sag/gateway.toml")),
            Some(Path::new("/etc/sag"))
        );
    }

    #[test]
    fn watch_dir_for_bare_filename_defaults_to_dot() {
        assert_eq!(
            watch_dir_for(Path::new("gateway.toml")),
            Some(Path::new("."))
        );
    }

    #[test]
    fn watch_dir_for_relative_path() {
        assert_eq!(
            watch_dir_for(Path::new("config/gateway.toml")),
            Some(Path::new("config"))
        );
    }
}
