//! SAG — Smart AI Gateway
//!
//! An AI gateway that proxies chat completion requests to multiple LLM providers
//! with priority-based routing, automatic fallback, and usage analytics.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::RwLock;
use tracing::info;

mod tracing_init;

#[cfg(feature = "embed-ui")]
mod ui_routes;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Resolve config path: --config <path> > positional <path> > "gateway.toml"
    let config_path = resolve_config_path()?;

    // 2. Peek at the config file for logging.level and logging.format, then
    //    initialize tracing. The peek is done before full config load so the
    //    early startup log messages respect the configured level and format.
    //    If the file can't be read or the keys are missing, defaults apply.
    let (log_level, log_format) = sag_config::peek_logging_config(&config_path);
    // Declared first in main() → dropped last by Rust's reverse drop order,
    // keeping the non-blocking writer alive through `info!("all subsystems shut down")`.
    let _log_guard = tracing_init::init(Some(&log_level), log_format);

    // Remove log files older than the retention period before any new
    // log entries are written for this process.
    cleanup_old_logs();

    info!(%config_path, "starting SAG gateway");

    // 3. Load configuration with hot-reload support
    let config_handle = sag_config::load_config(&config_path)?;

    // 4. Start config hot-reload watcher
    let hot_reload_handle = sag_config::spawn_hot_reload(config_handle.clone());

    // 5. Determine analytics storage DSN from config
    let config = config_handle.read().await;
    let analytics_dsn = config
        .analytics
        .as_ref()
        .map(|a| a.storage_dsn.clone())
        .unwrap_or_else(|| "sqlite://analytics.db".into());
    drop(config);

    // 6. Initialize analytics collector with SQLite persistence
    let collector = Arc::new(sag_analytics::CallLogCollector::new(&analytics_dsn).await?);

    // 7. Initialize provider registry from config and compute the
    //    stale-probe threshold for the circuit breaker in the same read
    //    lock (avoiding an extra lock acquisition).
    let config = config_handle.read().await;
    let mut providers: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();

    for (name, provider_config) in &config.providers {
        match sag_providers::create_provider(name.clone(), provider_config) {
            Ok(provider) => {
                info!(name, provider_type = %provider_config.provider_type.as_str(), base_url = %provider_config.base_url, "provider registered");
                providers.insert(name.clone(), provider);
            }
            Err(e) => {
                tracing::error!(name, error = %e, "failed to create provider adapter, skipping");
            }
        }
    }

    // Ensure at least one enabled provider adapter is available
    let enabled_count = config.providers.values().filter(|p| p.enabled).count();
    if providers.is_empty() && enabled_count > 0 {
        return Err(anyhow::anyhow!(
            "no provider adapters could be initialized; check provider configuration and credentials"
        ));
    }

    // Compute the stale-probe threshold from the current config snapshot.
    // The threshold is derived from the worst-case (route, target) timeout /
    // retries / ttfb across all configured targets.
    let stale_probe_threshold = sag_router::stale_probe_threshold_from_config(&config);
    drop(config);

    let provider_registry = Arc::new(RwLock::new(providers));

    // 8. Build AppState (shared resources) + DispatchRuntime (dispatch-only state).
    //    The circuit breaker Arc is created first and injected into both
    //    objects so they share the *same* live breaker instance. The breaker
    //    is constructed with the config-derived stale-probe threshold.
    let route_circuit_breaker: Arc<RwLock<sag_core::RouteCircuitBreaker>> = Arc::new(RwLock::new(
        sag_core::RouteCircuitBreaker::new(stale_probe_threshold),
    ));

    let app_state = sag_router::AppState {
        config: config_handle.clone(),
        providers: provider_registry,
        collector,
        circuit_breaker: route_circuit_breaker.clone(),
    };
    // `with_circuit_breaker` shares the same Arc (rather than creating a
    // default one) so AppState and DispatchRuntime observe the same breaker.
    let dispatch_runtime =
        sag_router::DispatchRuntime::with_circuit_breaker(route_circuit_breaker.clone());

    // 8a. Spawn a watcher that recomputes the stale-probe threshold whenever
    //     the config changes (hot-reload of `gateway.toml` or admin API
    //     mutation). It polls the config's `routes_version` counter (bumped by
    //     every config write path) and updates the shared breaker in place —
    //     in-flight probe state is preserved, only the threshold parameter is
    //     refreshed. This keeps the threshold aligned with the live provider /
    //     route timeout settings without a full process restart.
    //     Seed the watcher's baseline version from the same config snapshot the
    //     startup threshold was computed from, so no config change that
    //     occurred between the two can be missed.
    let initial_routes_version = config_handle.routes_version();
    let threshold_watcher_handle = spawn_threshold_watcher(
        config_handle.clone(),
        route_circuit_breaker,
        initial_routes_version,
    );

    // 9. Build router
    let app = sag_router::build_router(app_state, dispatch_runtime);

    // 9a. When the `embed-ui` feature is on, route any unmatched path
    //     through the embedded SPA (assets + index.html fallback).
    #[cfg(feature = "embed-ui")]
    let app = app.fallback(crate::ui_routes::ui_handler);

    // 10. Determine listen address
    let config = config_handle.read().await;
    let addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
        .parse()
        .map_err(|e| {
            anyhow::anyhow!(
                "invalid server address '{}:{}': {e}",
                config.server.host,
                config.server.port
            )
        })?;

    info!(%addr, "gateway listening");
    drop(config);

    // 11. Start server with graceful shutdown
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("gateway shut down");

    // 12. Gracefully shut down the hot-reload watcher and the threshold
    //     watcher. These MUST happen after the server stops because the
    //     hot-reload watcher uses `notify` which spawns OS-level threads
    //     (FSEvents on macOS). Using `abort()` on the JoinHandle would skip
    //     the Watcher's Drop implementation, leaving those threads alive and
    //     blocking process exit.
    //
    //     The router (and its internal AppState) was dropped when axum::serve
    //     completed above, which already closes the analytics channel and lets
    //     the SQLite background task exit.
    hot_reload_handle.shutdown().await;
    threshold_watcher_handle.shutdown().await;

    info!("all subsystems shut down");

    Ok(())
}

/// Watch the config's `routes_version` counter and recompute the circuit
/// breaker's stale-probe threshold when it changes.
///
/// Every config mutation path (hot-reload of `gateway.toml` and admin API
/// CRUD for routes *and* providers) bumps `routes_version`, so polling that
/// counter is a single, low-overhead way to detect any config change without
/// coupling this task to each individual mutation site.
///
/// On change the task acquires a read lock on the config, recomputes the
/// threshold via [`sag_router::stale_probe_threshold_from_config`], and
/// updates the shared breaker in place via
/// [`sag_core::RouteCircuitBreaker::set_stale_probe_threshold`]. In-flight
/// probe state (entries, `probe_in_flight`) is fully preserved — only the
/// threshold parameter used by `try_acquire_probe` is refreshed.
///
/// Poll interval for the threshold watcher. Cheap atomic load; config
/// changes are rare. For a threshold on the order of 600–3600s this
/// granularity is negligible.
const THRESHOLD_WATCHER_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

fn spawn_threshold_watcher(
    config_handle: sag_config::ConfigHandle,
    route_circuit_breaker: Arc<RwLock<sag_core::RouteCircuitBreaker>>,
    initial_routes_version: u64,
) -> ThresholdWatcherHandle {
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        let mut last_version = initial_routes_version;
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    info!("threshold watcher shutting down");
                    return;
                }
                _ = tokio::time::sleep(THRESHOLD_WATCHER_POLL_INTERVAL) => {}
            }
            let current_version = config_handle.routes_version();
            if current_version == last_version {
                continue;
            }
            last_version = current_version;
            let config = config_handle.read().await;
            let new_threshold = sag_router::stale_probe_threshold_from_config(&config);
            drop(config);
            let mut breaker = route_circuit_breaker.write().await;
            breaker.set_stale_probe_threshold(new_threshold);
            info!(
                ?new_threshold,
                "stale-probe threshold updated after config change"
            );
        }
    });
    ThresholdWatcherHandle { task, shutdown_tx }
}

/// Handle for the threshold-watcher background task.
struct ThresholdWatcherHandle {
    task: tokio::task::JoinHandle<()>,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

impl ThresholdWatcherHandle {
    /// Signal the watcher to shut down and wait up to 5s for it to exit.
    /// Unlike [`HotReloadHandle::shutdown`] this task owns no OS file-watch
    /// threads, so the 5s timeout is purely a safety bound against a poll
    /// that happens to be mid-sleep.
    async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), self.task).await;
    }
}

/// Resolves the configuration file path from CLI arguments.
///
/// Precedence:
/// 1. `--config <path>` flag (most explicit)
/// 2. First non-flag positional argument (legacy `sag <path>` shape)
/// 3. Default: `gateway.toml`
///
/// Both shapes are accepted so existing invocations like
/// `./target/release/sag /etc/sag/gateway.toml` keep working, while
/// container / docker flows can use `./sag --config /etc/sag/gateway.toml`.
fn resolve_config_path() -> anyhow::Result<String> {
    let mut explicit: Option<String> = None;
    let mut positional: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--config" {
            // `--config <path>`: explicit path wins over any positional.
            let value = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("--config requires a path argument"))?;
            explicit = Some(value);
        } else if let Some(value) = arg.strip_prefix("--config=") {
            // `--config=<path>` form (handy for `docker run` / systemd units).
            explicit = Some(value.to_string());
        } else if arg.starts_with('-') {
            // Unknown flag-like token: skip silently so future flag additions
            // don't break older invocations. Examples: `--verbose`, `-v`.
            continue;
        } else if positional.is_none() {
            positional = Some(arg);
        }
        // Non-`--config` positional tokens after the first are dropped to keep
        // parsing deterministic.
    }
    Ok(explicit
        .or(positional)
        .unwrap_or_else(|| "gateway.toml".to_string()))
}

/// Number of days to retain rotating log files (logs/sag.*.log).
/// Files with a modification time older than this are deleted on startup.
const LOG_RETENTION_DAYS: u64 = 7;

/// Delete log files in `logs/` older than [`LOG_RETENTION_DAYS`].
///
/// Best-effort: errors on individual files are logged and skipped;
/// a missing `logs/` directory is a silent no-op.
fn cleanup_old_logs() {
    let log_dir = std::path::Path::new("logs");
    let dir = match std::fs::read_dir(log_dir) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read logs directory for cleanup");
            return;
        }
    };

    let cutoff = SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(LOG_RETENTION_DAYS * 86_400))
        .unwrap_or(SystemTime::UNIX_EPOCH);

    for entry in dir {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, "cannot read log directory entry during cleanup");
                continue;
            }
        };

        let path = entry.path();

        // Only touch sag.*.log files — leave other files in logs/ alone.
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if !name.starts_with("sag.") || !name.ends_with(".log") {
            continue;
        }

        let modified = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "cannot read log file metadata, skipping cleanup");
                continue;
            }
        };

        if modified < cutoff {
            match std::fs::remove_file(&path) {
                Ok(()) => tracing::debug!(path = %path.display(), "removed old log file"),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "failed to remove old log file")
                }
            }
        }
    }
}

/// Waits for SIGTERM or SIGINT (Ctrl+C).
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    let terminate = {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
        async move { sigterm.recv().await }
    };

    #[cfg(unix)]
    tokio::select! {
        _ = ctrl_c => {
            info!("received SIGINT, shutting down");
        }
        _ = terminate => {
            info!("received SIGTERM, shutting down");
        }
    }

    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
        info!("received SIGINT, shutting down");
    }
}
