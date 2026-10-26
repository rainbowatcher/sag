//! SAG — Smart AI Gateway
//!
//! An AI gateway that proxies chat completion requests to multiple LLM providers
//! with priority-based routing, automatic fallback, and usage analytics.
//!
//! Persistent data follows the XDG Base Directory specification (see
//! `sag_core::paths`): config in `$XDG_CONFIG_HOME/sag`, the analytics SQLite
//! database in `$XDG_DATA_HOME/sag`, and daily rotating logs in
//! `$XDG_STATE_HOME/sag/logs`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use async_trait::async_trait;
use axum::serve::ListenerExt;
use sag_config::ConfigReloadHandler;
use sag_state::ConfigMutationCoordinator;
use tokio::sync::RwLock;
use tracing::info;

mod tracing_init;

// Compiled unconditionally: `ui_routes::negotiate` is a dependency-free pure
// function so its unit tests run without the `embed-ui` feature. The embedded
// asset serving inside the module is itself `#[cfg(feature = "embed-ui")]`.
mod ui_routes;

#[tokio::main]
async fn main() {
    // 1. Resolve config path: --config <path> > positional <path> > "gateway.toml"
    let config_path = match resolve_config_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    // 2. Peek at the config file for logging.level and logging.format, then
    //    initialize tracing. The peek is done before full config load so the
    //    early startup log messages respect the configured level and format.
    //    Missing keys use defaults; an unknown `format` value is a hard error
    //    (same strict enum policy as the main loader).
    let (log_level, log_format) = match sag_config::peek_logging_config(&config_path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };
    // Declared first in main() → dropped last by Rust's reverse drop order,
    // keeping the non-blocking writer alive through `info!("all subsystems shut down")`
    // and through the `tracing::error!` emitted when startup fails.
    let _log_guard = tracing_init::init(
        &sag_core::paths::default_log_dir(),
        Some(&log_level),
        log_format,
    );

    // Remove log files older than the retention period before any new
    // log entries are written for this process.
    cleanup_old_logs();

    // 3. Run the gateway. Any startup failure is surfaced BOTH to the
    //    tracing log file (so test-env / log readers can see it) and to
    //    stderr (visible when run in the foreground). Without this, a failed
    //    startup would exit silently — e.g. when test-env redirects stderr to
    //    DEVNULL — leaving only a bare "starting SAG gateway" in the logs.
    if let Err(e) = run(&config_path).await {
        tracing::error!(error = %e, "startup failed");
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

/// Full gateway startup and serve loop. Returns `Err` on any startup failure
/// (config load, analytics init, provider registration, bind, ...) so `main`
/// can report it via tracing + stderr instead of failing silently.
async fn run(config_path: &str) -> anyhow::Result<()> {
    info!(%config_path, "starting SAG gateway");

    // 3. Load configuration with hot-reload support
    let config_handle = sag_config::load_config(config_path)?;

    // 4. Determine analytics storage DSN from config, and snapshot the
    //    configured route identities (route_name → static targets) from
    //    the same read lock.
    let (analytics_dsn, routes_identity) = snapshot_analytics_inputs(&config_handle).await;
    // XDG Base Directory: anchor relative `sqlite://` paths at the SAG data
    // dir (`$XDG_DATA_HOME/sag`) and create the parent directory — SQLite
    // creates the database file but never its parent directories. Absolute
    // paths and `:memory:` pass through; their parent dirs are also ensured
    // (idempotent) so a deep user-provided path fails with a clear error
    // instead of SQLite's bare "unable to open database file".
    let analytics_dsn = resolve_analytics_dsn_with_parent(&analytics_dsn)?;

    // 5. Initialize analytics (shared pool + identity registry + query
    //    engine + rollup scheduler + call_logs rotation scheduler + legacy
    //    collector + event collector).
    let analytics = load_analytics_stack(&analytics_dsn, routes_identity, &config_handle).await?;
    let AnalyticsStack {
        identity_registry,
        analytics_query,
        collector,
        event_collector,
        rollup_scheduler_handle,
        rotation_scheduler_handle,
    } = analytics;

    // 6. Initialize provider registry from config and compute the
    //    stale-probe threshold for the circuit breaker.
    let (providers, stale_probe_threshold) = build_provider_registry(&config_handle).await?;
    let provider_registry = Arc::new(RwLock::new(providers));

    // 7. Create config mutation coordinator (serialises all production
    //    config writes: hot-reload + Admin CRUD). The startup-created
    //    identity registry is injected so that every committed route
    //    mutation (admin CRUD via the coordinator, hot reload via the
    //    coordinator-backed handler below) reconciles the analytics
    //    identities with the live routes. Provider-only mutations never
    //    touch identities.
    let coordinator = Arc::new(
        ConfigMutationCoordinator::new(config_handle.clone(), provider_registry.clone())
            .with_identity_registry(identity_registry.clone()),
    );

    // 8. Build AppState (shared resources) + DispatchRuntime (dispatch-only state).
    //    The circuit breaker Arc is created first and injected into both
    //    objects so they share the *same* live breaker instance. The breaker
    //    is constructed with the config-derived stale-probe threshold.
    let circuit_breaker: Arc<RwLock<sag_core::RouteCircuitBreaker>> = Arc::new(RwLock::new(
        sag_core::RouteCircuitBreaker::new(stale_probe_threshold),
    ));
    // Sticky-failover active-target memory. Shared between `AppState` and
    // `DispatchRuntime` via the same `Arc` so that admin mutations — notably
    // clearing sticky routing when a breaker is manually closed — are
    // immediately visible to dispatch ordering.
    let active_targets: sag_router::ActiveTargets = Arc::default();
    // Fill-first shuffled-order cache. Shared the same way so admin handlers
    // can observe the current fill-first order and reset it (manual
    // re-shuffle) through the sticky-targets endpoints.
    let fill_first_orders: sag_router::FillFirstOrders = Arc::default();
    // Session-affinity memory for `session_sticky` routes. Shared the same
    // way so admin handlers can observe and clear session pins.
    let session_affinity: sag_state::SessionAffinity = Arc::default();
    // Round-robin counters. Shared the same way so the admin-side route
    // prune drops a deleted route's counters immediately, instead of waiting
    // for the next dispatch request to observe the `routes_version` change.
    let route_counters: sag_router::RouteCounters = Arc::default();

    let app_state = sag_router::AppState {
        config: config_handle.clone(),
        providers: provider_registry,
        coordinator: coordinator.clone(),
        collector,
        event_collector: event_collector.clone(),
        analytics_query: Some(Arc::new(analytics_query)),
        circuit_breaker: circuit_breaker.clone(),
        active_targets: active_targets.clone(),
        fill_first_orders: fill_first_orders.clone(),
        route_counters: route_counters.clone(),
        session_affinity: session_affinity.clone(),
    };
    // `with_shared_state` shares the same Arcs (rather than creating
    // defaults) so AppState and DispatchRuntime observe the same breaker,
    // the same sticky memory, the same fill-first order cache, and the same
    // round-robin counters.
    let dispatch_runtime = sag_router::DispatchRuntime::with_shared_state(
        circuit_breaker.clone(),
        active_targets,
        fill_first_orders,
        route_counters,
        session_affinity,
    );

    // 9. Start config hot-reload watcher with a coordinator-backed handler
    //    that re-reads the file, validates it, and atomically publishes
    //    config + provider registry + routes_version.
    let handler: Arc<dyn ConfigReloadHandler> = Arc::new(GatewayReloadHandler {
        coordinator: coordinator.clone(),
        runtime: dispatch_runtime.clone(),
        config: config_handle.clone(),
    });
    let hot_reload_handle = sag_config::spawn_hot_reload(config_handle.clone(), handler);

    // 10. Spawn a watcher that recomputes the stale-probe threshold whenever
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
        circuit_breaker,
        initial_routes_version,
    );

    // 11. Build router
    let app = sag_router::build_router(app_state, dispatch_runtime);

    // 11a. When the `embed-ui` feature is on, route any unmatched path
    //     through the embedded SPA (assets + index.html fallback).
    #[cfg(feature = "embed-ui")]
    let app = app.fallback(crate::ui_routes::ui_handler);

    // 12. Determine listen address
    let addr = parse_listen_addr(&config_handle).await?;
    info!(%addr, "gateway listening");

    // 13. Start server with graceful shutdown
    let listener = tokio::net::TcpListener::bind(addr).await?;
    // Disable Nagle's algorithm on every accepted downstream connection.
    // `axum::serve` does not set TCP_NODELAY itself, and the default
    // (Nagle enabled) adds avoidable latency to the small streaming
    // SSE chunks produced by chat completions.
    let listener = listener.tap_io(|stream| {
        if let Err(err) = stream.set_nodelay(true) {
            tracing::warn!("failed to set TCP_NODELAY on incoming connection: {err:#}");
        }
    });
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("gateway shut down");

    // 14–15. Shut down watchers / rollup / rotation, then drain the event
    //        queue.
    graceful_shutdown(
        rollup_scheduler_handle,
        rotation_scheduler_handle,
        hot_reload_handle,
        threshold_watcher_handle,
        event_collector,
    )
    .await;

    Ok(())
}

/// Phase 4: Determine analytics storage DSN from config, and snapshot the
/// configured route identities (route_name → static targets) from
/// the same read lock.
///
/// The fallback DSN stays in relative form (`sqlite://analytics.db`); it is
/// anchored at the XDG data dir by [`resolve_analytics_dsn_with_parent`] in
/// `run` — the same anchoring every configured DSN goes through.
async fn snapshot_analytics_inputs(
    config_handle: &sag_config::ConfigHandle,
) -> (String, Vec<sag_analytics::RouteIdentity>) {
    let config = config_handle.read().await;
    let analytics_dsn = config
        .analytics
        .as_ref()
        .map(|a| a.storage_dsn.clone())
        .unwrap_or_else(|| "sqlite://analytics.db".into());
    let routes_identity: Vec<sag_analytics::RouteIdentity> = config
        .routes
        .iter()
        .map(|route| sag_analytics::RouteIdentity {
            route_name: route.model_name.clone(),
            targets: route
                .targets
                .iter()
                .map(|t| sag_analytics::TargetIdentity {
                    provider: t.provider.clone(),
                    real_model: t.real_model.clone(),
                })
                .collect(),
        })
        .collect();
    (analytics_dsn, routes_identity)
}

/// XDG Base Directory: anchor relative `sqlite://` DSN paths at the SAG data
/// dir and ensure the parent directory exists.
///
/// - Relative `sqlite://analytics.db` → `$XDG_DATA_HOME/sag/analytics.db`
///   (old configs migrate without any edit).
/// - Absolute paths and `:memory:` pass through.
/// - The parent directory is created idempotently for every resolved file
///   path so SQLite's bare "unable to open database file" never masks a
///   missing-directory problem.
fn resolve_analytics_dsn_with_parent(raw_dsn: &str) -> anyhow::Result<String> {
    let dsn = sag_core::paths::resolve_analytics_dsn(raw_dsn);
    if let Some(path) = sag_core::paths::sqlite_dsn_file_path(&dsn)
        && let Some(parent) = path.parent()
    {
        std::fs::create_dir_all(parent).map_err(|e| {
            anyhow::anyhow!(
                "failed to create analytics data directory {}: {e}",
                parent.display()
            )
        })?;
    }
    if raw_dsn != dsn {
        tracing::debug!(raw = %raw_dsn, resolved = %dsn, "relative analytics DSN anchored at the XDG data dir");
    }
    Ok(dsn)
}

/// Analytics subsystem handles assembled over ONE shared pool.
///
/// `event_collector`, `rollup_scheduler_handle` and
/// `rotation_scheduler_handle` are consumed by [`graceful_shutdown`]; the
/// rest are handed to `AppState` / the coordinator. The pool itself is not
/// retained here — it is moved into the [`sag_analytics::EventWriter`] and
/// shared via `Arc` clones held by the registry / query engine / rollup
/// engine / rotation scheduler / legacy collector.
struct AnalyticsStack {
    identity_registry: Arc<sag_analytics::IdentityRegistry>,
    analytics_query: sag_analytics::AnalyticsQueryEngine,
    collector: Arc<sag_analytics::CallLogCollector>,
    event_collector: Arc<sag_analytics::EventCollector>,
    rollup_scheduler_handle: RollupSchedulerHandle,
    rotation_scheduler_handle: RotationSchedulerHandle,
}

/// Phases 5–5b: Initialize analytics. The formal path (EventCollector →
/// EventWriter + IdentityRegistry) opens ONE shared, migrated pool
/// on the analytics DSN — the same SQLite file the legacy
/// call_logs store writes to; the writer is built over that shared
/// pool (`EventWriter::new`), never over a private one. The
/// identity registry is synced against the configured routes
/// *before* startup proceeds (config-sync-first); a failed sync
/// aborts startup. The exact-detail query engine (`/v1/analytics/v2/*`)
/// is assembled over the same shared pool, so it observes exactly
/// what the writer commits. The legacy CallLogCollector is kept for
/// the `/v1/analytics/*` and `/api/logs` paths and is built over
/// the SAME shared pool (`new_shared`) — one pool for writer /
/// registry / query / rollup / legacy store, no second pool on the
/// same file.
async fn load_analytics_stack(
    analytics_dsn: &str,
    routes_identity: Vec<sag_analytics::RouteIdentity>,
    config_handle: &sag_config::ConfigHandle,
) -> anyhow::Result<AnalyticsStack> {
    let analytics_pool = sag_analytics::open_analytics_pool(analytics_dsn).await?;
    let identity_registry = Arc::new(sag_analytics::IdentityRegistry::new(analytics_pool.clone()));
    identity_registry.sync(&routes_identity).await?;
    let analytics_query = sag_analytics::AnalyticsQueryEngine::new(analytics_pool.clone());
    // 5a. The legacy CallLogCollector (backing `/v1/analytics/*` and
    //     `/api/logs`) is built over the SAME shared pool — `new_shared`,
    //     never `CallLogCollector::new(dsn)` which would open a second,
    //     independent pool on the same file and close it on shutdown,
    //     racing the formal pool. The store does not own the pool, so its
    //     shutdown is a no-op for the pool itself. Cloned here before the
    //     pool is moved into the EventWriter below.
    //
    //     Ordering: opening the store is also what *prepares* the schema the
    //     background writers below depend on — `from_pool` runs
    //     `ensure_schema_consistency` (the `ALTER TABLE … ADD COLUMN`
    //     catches) and then the `refresh_views(pool, None)` full-history
    //     fallback. Both schedulers are therefore spawned only *after* this
    //     point (spec §D: ensure does not run concurrently with the first
    //     rotation round). Spawning first would let the first round capture
    //     pre-ALTER DDL for its replay and/or overwrite the fallback view
    //     with a narrowed one, leaving the startup view shape up to the
    //     runtime's scheduling (and, in the worst interleaving, a narrowed
    //     window that no round corrects for another 24h).
    let collector =
        Arc::new(sag_analytics::CallLogCollector::new_shared(analytics_pool.clone()).await?);
    //     Failure boundary: every fallible step of this function — the pool
    //     open, the identity sync and the store open above — runs *before*
    //     the spawns below, and no step after them can return `Err`. That is
    //     deliberate: dropping a `*SchedulerHandle` only drops its
    //     `JoinHandle`, and `JoinHandle::drop` *detaches* (does not abort) the
    //     task, so a `?` placed after the spawns would leak a live
    //     maintenance task on the startup error path. New fallible work must
    //     go above this point (or abort the handles explicitly on failure).
    // 5b. 1h rollup engine over the SAME shared pool (one pool for writer /
    //     registry / query / rollup — a failed sync aborts startup above,
    //     so the rollup can only ever see a migrated, consistent DB). The
    //     single-worker scheduler is spawned immediately but only
    //     *registers* missing hours at startup (scan → `pending`, never
    //     compute) in the background while the rest of the gateway comes
    //     up — registration cannot block service startup, and a failed
    //     scan is warned and retried by every hourly cycle.
    let rollup_scheduler_handle = spawn_rollup_scheduler(
        Arc::new(sag_analytics::RollupEngine::new(analytics_pool.clone())),
        ROLLUP_INTERVAL,
    );
    // 5c. `call_logs` hot/cold rotation over the SAME shared pool, next
    //     to the rollup scheduler: both are background DB maintenance,
    //     both are stopped before the event drain, and neither can fail
    //     startup (the task logs and retries on the next round). The
    //     task's first action is one complete round, so the full-history
    //     view shape left by the store-open `refresh_views(None)`
    //     fallback narrows to the real retention window right after
    //     startup instead of one interval later.
    let rotation_scheduler_handle =
        spawn_rotation_scheduler(analytics_pool.clone(), config_handle.clone());
    let event_collector = Arc::new(sag_analytics::EventCollector::new(
        sag_analytics::EventWriter::new(analytics_pool),
    ));

    Ok(AnalyticsStack {
        identity_registry,
        analytics_query,
        collector,
        event_collector,
        rollup_scheduler_handle,
        rotation_scheduler_handle,
    })
}

/// Phase 6: Initialize provider registry from config and compute the
/// stale-probe threshold for the circuit breaker in the same read
/// lock (avoiding an extra lock acquisition).
async fn build_provider_registry(
    config_handle: &sag_config::ConfigHandle,
) -> anyhow::Result<(
    HashMap<String, Arc<dyn sag_providers::LlmProvider>>,
    Duration,
)> {
    let config = config_handle.read().await;
    let mut providers: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();

    for (name, provider_config) in &config.providers {
        // Resolve this provider's effective proxy (global `proxy_url` + its
        // `proxy_mode`) and build the adapter with it — the same resolution
        // every construction path uses, so startup, hot reload and admin
        // mutations agree on the proxy policy. Resolution errors follow the
        // existing startup handling for provider construction: logged and the
        // provider is skipped (never unwrap/panic). With enabled providers
        // present, an invalid global proxy / unsatisfiable `enabled` mode then
        // surfaces below as "no provider adapters could be initialized", which
        // `load_config`'s validation would already have rejected. No proxy URL
        // or credential is ever logged.
        let effective = match sag_state::config_mutation::effective_proxy_for(
            &config,
            provider_config,
        ) {
            Ok(effective) => effective,
            Err(e) => {
                tracing::error!(name, error = %e, "invalid proxy configuration, skipping provider");
                continue;
            }
        };
        match sag_providers::create_provider_with_proxy(
            name.clone(),
            provider_config,
            effective.as_ref(),
        ) {
            Ok(provider) => {
                // Catalog form: the protocol keys the provider declares plus
                // one representative node address (no credentials ever).
                let apis_keys: Vec<&str> =
                    provider_config.apis.keys().map(|k| k.as_str()).collect();
                let base_url = provider_config
                    .apis
                    .values()
                    .next()
                    .map(|api| api.base_url.as_str())
                    .unwrap_or_default();
                info!(name, apis = ?apis_keys, base_url, "provider registered");
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

    Ok((providers, stale_probe_threshold))
}

/// Phase 12: Determine listen address.
async fn parse_listen_addr(config_handle: &sag_config::ConfigHandle) -> anyhow::Result<SocketAddr> {
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
    drop(config);
    Ok(addr)
}

/// Phases 14–15: Shut down background watchers and drain the analytics
/// event queue after the server has stopped.
///
/// Shutdown order is load-bearing:
/// 1. hot-reload watcher, then threshold watcher — the hot-reload watcher
///    uses `notify` OS threads whose `Drop` must run (see below);
/// 2. rollup scheduler, then the `call_logs` rotation scheduler, BEFORE the
///    event drain — the drain must be the last writer to touch the analytics
///    DB, and the rotation join is what guarantees its in-flight DDL
///    transaction has committed (or rolled back) before the drain starts
///    (on a join timeout the task is aborted instead; see
///    [`shutdown_analytics_writers`]);
/// 3. event collector drain last.
async fn graceful_shutdown(
    rollup_scheduler_handle: RollupSchedulerHandle,
    rotation_scheduler_handle: RotationSchedulerHandle,
    hot_reload_handle: sag_config::HotReloadHandle,
    threshold_watcher_handle: ThresholdWatcherHandle,
    event_collector: Arc<sag_analytics::EventCollector>,
) {
    // 14. Gracefully shut down the hot-reload watcher and the threshold
    //     watcher. These MUST happen after the server stops because the
    //     hot-reload watcher uses `notify` which spawns OS-level threads
    //     (FSEvents on macOS). Using `abort()` on the JoinHandle would skip
    //     the Watcher's Drop implementation, leaving those threads alive and
    //     blocking process exit.
    //
    //     The router (and its internal AppState) was dropped when axum::serve
    //     completed, which already closes the analytics channel and lets
    //     the SQLite background task exit.
    hot_reload_handle.shutdown().await;
    threshold_watcher_handle.shutdown().await;

    // 14a. Stop the analytics background writers BEFORE draining the event
    //      queue: rollup first, then `call_logs` rotation. Both shut-downs
    //      signal *and* join, so when both joins succeed neither task can
    //      start a new transaction and the rotation's in-flight DDL
    //      transaction has committed or rolled back — the drain below is the
    //      last writer to touch the analytics DB. On a join timeout the task
    //      is aborted (its in-flight transaction rolls back), but the drain
    //      may still race that rollback briefly; the timeout is logged as an
    //      error. The ordering lives in one helper so the invariant is
    //      exercised by the test suite as production code.
    shutdown_analytics_writers(rollup_scheduler_handle, rotation_scheduler_handle).await;

    // 15. Drain the formal analytics event queue: close the sender, wait
    //     for the writer to persist every pending event before exiting.
    //     `EventCollector::shutdown` runs on a shared handle (`&self`)
    //     and is idempotent, so the drain does not depend on this `Arc`
    //     being the last strong reference — extra clones (e.g. the
    //     `AppState` handle) can never cause the drain to be skipped.
    event_collector.shutdown().await;
    info!("analytics event collector drained");

    info!("all subsystems shut down");
}

/// Stop the analytics background writers in their load-bearing order: the
/// rollup scheduler first, then the `call_logs` rotation scheduler.
///
/// Both shut-downs signal *and* join. **When both joins succeed**, no
/// background transaction is in flight and no background task can start a new
/// one, so the caller's next step (the event drain) is the last writer on the
/// analytics DB. On a join timeout the task is aborted — its in-flight
/// transaction rolls back (SQLite atomicity) and it cannot start another
/// round — but the abort is not awaited, so the drain may still race that
/// rollback briefly; the timeout is logged as an error, never silently
/// ignored. Any rollup bucket left `processing` by a hard kill is reclaimed
/// via the lease at next startup, and an interrupted rotation DDL transaction
/// leaves the hot table and both views intact.
///
/// This helper's ONLY call site is [`graceful_shutdown`], and it must stay
/// before `event_collector.shutdown()` — that position is the whole reason the
/// helper exists (the drain must be the last writer).
async fn shutdown_analytics_writers(
    rollup_scheduler_handle: RollupSchedulerHandle,
    rotation_scheduler_handle: RotationSchedulerHandle,
) {
    rollup_scheduler_handle.shutdown().await;
    rotation_scheduler_handle.shutdown().await;
}

/// Handler that wraps the [`ConfigMutationCoordinator`] for the hot-reload
/// watcher. Each file event triggers a coordinated re-read, validation, and
/// atomic publication of config + provider registry + routes_version.
///
/// After a successful reload the shared dispatch runtime is pruned so stale
/// per-model caches (notably sticky-failover pins for routes that switched
/// strategy or were removed) are dropped immediately, without waiting for the
/// next dispatch to notice the `routes_version` bump.
struct GatewayReloadHandler {
    coordinator: Arc<ConfigMutationCoordinator>,
    runtime: sag_router::DispatchRuntime,
    config: sag_config::ConfigHandle,
}

#[async_trait]
impl ConfigReloadHandler for GatewayReloadHandler {
    async fn reload_from_path(&self, path: &Path) -> sag_core::Result<()> {
        match self.coordinator.reload_from_path(path).await {
            Ok(report) => {
                // The coordinator released every lock (mutation lock, config
                // RwLock) before returning; clone the routes under a short
                // config read and prune with no config guard held, so no
                // lock-ordering interaction with dispatch is possible.
                let routes = self.config.read().await.routes.clone();
                self.runtime.prune_stale_entries(&routes).await;
                tracing::info!(
                    added = report.providers_added.len(),
                    rebuilt = report.providers_rebuilt.len(),
                    removed = report.providers_removed.len(),
                    "config hot-reloaded via coordinator"
                );
                Ok(())
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("unstable") {
                    // File is still changing within the retry window.
                    // Schedule a one-shot fallback reload after 2s so
                    // the editor has time to finish writing.
                    tracing::warn!("config file unstable, scheduling fallback reload in 2s");
                    let coord = self.coordinator.clone();
                    let runtime = self.runtime.clone();
                    let config = self.config.clone();
                    let path = path.to_path_buf();
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        match coord.reload_from_path(&path).await {
                            Ok(r) => {
                                // Same strategy-switch cleanup as the main
                                // reload path: the coordinator only commits
                                // (it has released every lock before
                                // returning), pruning stale per-model caches
                                // is the handler's job. Clone the routes
                                // under a short config read and prune with no
                                // guard held — identical to the main path,
                                // so no lock-ordering interaction with
                                // dispatch is possible here either.
                                let routes = config.read().await.routes.clone();
                                runtime.prune_stale_entries(&routes).await;
                                tracing::info!(
                                    added = r.providers_added.len(),
                                    rebuilt = r.providers_rebuilt.len(),
                                    removed = r.providers_removed.len(),
                                    "fallback reload succeeded"
                                )
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "fallback reload also failed, waiting for next file event")
                            }
                        }
                    });
                    return Err(e);
                }
                tracing::warn!(error = %e, "config reload rejected");
                Err(e)
            }
        }
    }
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
    circuit_breaker: Arc<RwLock<sag_core::RouteCircuitBreaker>>,
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
            let mut breaker = circuit_breaker.write().await;
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
    /// Unlike [`sag_config::HotReloadHandle::shutdown`] this task owns no OS file-watch
    /// threads, so the 5s timeout is purely a safety bound against a poll
    /// that happens to be mid-sleep.
    async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        match tokio::time::timeout(std::time::Duration::from_secs(5), self.task).await {
            Ok(Ok(())) => info!("threshold watcher shut down"),
            Ok(Err(e)) => tracing::error!(error = %e, "threshold watcher task panicked"),
            Err(_) => tracing::warn!("threshold watcher did not stop within 5s"),
        }
    }
}

/// Interval between rollup scheduler cycles: one compute run per hour
/// (spec: 每小时执行一轮). Combined with [`tokio::time::MissedTickBehavior::Skip`], a
/// cycle that overruns its hour delays the next run instead of bursting
/// into catch-up runs — the engine's lease-based claim is safe only for a
/// single worker, and one run per hour is the contract.
const ROLLUP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Registration lookback for [`sag_analytics::RollupEngine::register_missing_hours`]: each
/// cycle scans facts in `[now - window, now)` and registers the hours that
/// need rollup as `pending` (missing / dirty / failed / expired-lease). The
/// window bounds the scan cost (index-backed distinct on `completed_at`);
/// buckets outside it are still answered correctly by the query engine's
/// exact-detail fallback, so it only bounds how much history gets
/// accelerated — never correctness.
const ROLLUP_REGISTRATION_LOOKBACK: Duration = Duration::from_secs(90 * 24 * 60 * 60);

/// Spawn the single-worker 1h rollup scheduler over the shared analytics
/// pool.
///
/// Startup contract (spec): registration only — scan the registration
/// window and mark candidate hours `pending`, never compute. The task is
/// spawned at analytics init and registers in the background while the
/// rest of the gateway comes up, so a slow scan (or a transient DB error)
/// cannot delay serving: failures are warned and retried by every hourly
/// cycle. The first *compute* run happens a full `interval` later (the
/// immediate first tick of a tokio interval is consumed, never executed).
///
/// Compute contract: one run per `interval` (production: 1h), single
/// worker only. Each cycle re-registers first (retries a failed startup
/// registration; picks up new / dirty / late-fact hours that appeared
/// since), then runs one batch of up to
/// [`sag_analytics::rollup::MAX_BUCKETS_PER_RUN`] buckets. `run_once` and
/// `register_missing_hours` never panic on DB errors — they return
/// `Err`, which is logged here and retried next cycle.
fn spawn_rollup_scheduler(
    engine: Arc<sag_analytics::RollupEngine>,
    interval: Duration,
) -> RollupSchedulerHandle {
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        // Startup: registration only (never compute).
        register_rollup_hours(&engine).await;

        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // A tokio interval's first tick fires immediately. Consume it so
        // the first compute run happens a full interval after startup —
        // startup is registration-only by spec.
        ticker.tick().await;

        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    info!("rollup scheduler stopped");
                    return;
                }
                _ = ticker.tick() => {}
            }
            register_rollup_hours(&engine).await;
            match engine.run_once().await {
                Ok(report) => info!(
                    claimed = report.claimed,
                    completed = report.completed,
                    failed = report.failed,
                    "rollup cycle done"
                ),
                Err(e) => {
                    tracing::warn!(error = %e, "rollup cycle failed; retrying next cycle");
                }
            }
        }
    });
    RollupSchedulerHandle { task, shutdown_tx }
}

/// One registration pass of the rollup scheduler: scan the registration
/// window and register every hour that needs rollup as `pending`.
///
/// Best-effort by design: a failure is warned and retried on the next
/// cycle — the query engine answers missing buckets from exact-detail
/// aggregation, so the service stays correct even while rollup lags. The
/// engine logs the registered hours itself; this helper only surfaces
/// failures.
async fn register_rollup_hours(engine: &sag_analytics::RollupEngine) {
    let now_ms = unix_now_ms();
    let from_ms = now_ms - ROLLUP_REGISTRATION_LOOKBACK.as_millis() as i64;
    match engine.register_missing_hours(from_ms, now_ms).await {
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "rollup registration failed; retrying next cycle");
        }
    }
}

/// Current unix time in milliseconds. A system clock before the UNIX epoch
/// clamps to 0 — the registration window then degenerates to a no-op scan
/// instead of panicking.
fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Interval between `call_logs` rotation rounds: one round per day.
///
/// The daily round is what keeps the retention window moving: the cutoff is
/// baked into the view bodies as a literal, so without a rebuild the hot
/// window would stay frozen at whatever the last round computed. Rotation
/// itself is *not* daily — the hot table only holds `retention_days` of rows,
/// so a round rotates roughly once per retention period (§C of the spec);
/// the everyday job is advancing the view cutoff and sweeping retired
/// indexes. `MissedTickBehavior::Skip` keeps a slow round from bursting into
/// catch-up rounds.
const ROTATION_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Upper bound on the retention window used to derive the rotation cutoff
/// (~10 years).
///
/// The cutoff arithmetic is `now - retention * 86_400_000`, and while every
/// `u32` day count fits in `i64` today (`u32::MAX` days ≈ 3.7e17 ms =
/// 371_085_174_288_000_000 ms ≪ `i64::MAX`), the clamp keeps the subtraction
/// bounded independently of the configured width: no overflow, no debug-build
/// panic, no behavior change.
/// A retention that large already means "keep everything", so clamping it to
/// ten years keeps that semantic (rotation never fires) while logging the
/// clamp at `debug!` instead of silently pretending the value was honored.
///
/// The same bound (and the same rationale) applies to `session_retention_days`:
/// both knobs are day counts feeding a cutoff subtraction in the round.
const MAX_CALL_LOG_RETENTION_DAYS: u32 = 3650;

/// `source` label of the `session registry prune armed` line when the session
/// retention is the built-in default — which is also the "key omitted" case,
/// since `#[serde(default)]` cannot tell the two apart.
const SESSION_RETENTION_SOURCE_DEFAULT: &str = "default";

/// `source` label of the same line for an explicitly configured (non-default)
/// session retention.
const SESSION_RETENTION_SOURCE_CONFIG: &str = "config";

/// Remembers the `(retention_days, source)` pair of the last emitted
/// `session registry prune armed` line.
///
/// The round repeats on a fixed interval whose very first action is one round,
/// so a stateless `info!` would either be a startup-only line an operator can
/// never re-read or a once-a-day repeat. Tracking the last logged pair lets the
/// line fire on the first round and on every change — exactly when the
/// delete-vs-archive policy of `session_retention_days` changes under the
/// operator's feet.
#[derive(Debug, Default)]
struct SessionPruneLogState {
    last: Option<(u32, &'static str)>,
}

impl SessionPruneLogState {
    /// Record what a round is about to use. Returns `true` when that differs
    /// from the last logged pair (the caller emits the line), `false` when the
    /// round merely repeats it.
    fn observe(&mut self, retention_days: u32, source: &'static str) -> bool {
        let current = (retention_days, source);
        if self.last == Some(current) {
            return false;
        }
        self.last = Some(current);
        true
    }
}

/// Map the live config onto the two retention knobs a round works with: the
/// `call_logs` rotation cutoff, the session registry prune window, and the
/// `source` label of the latter for the arm line.
///
/// A missing `[analytics]` section means every knob sits at its default. When
/// the section is present, `#[serde(default)]` makes "key omitted" and "key
/// spelled out with the default value" indistinguishable at the type level, so
/// an explicit `7` is reported as the default. Coarse on purpose: the arm line
/// surfaces *which policy is in force* (and that deletion is armed at all), it
/// does not reconstruct the config file.
fn round_retention(config: &sag_config::GatewayConfig) -> (u32, u32, &'static str) {
    match config.analytics.as_ref() {
        None => (
            sag_config::DEFAULT_CALL_LOG_RETENTION_DAYS,
            sag_config::DEFAULT_SESSION_RETENTION_DAYS,
            SESSION_RETENTION_SOURCE_DEFAULT,
        ),
        Some(analytics) => (
            analytics.call_log_retention_days,
            analytics.session_retention_days,
            if analytics.session_retention_days == sag_config::DEFAULT_SESSION_RETENTION_DAYS {
                SESSION_RETENTION_SOURCE_DEFAULT
            } else {
                SESSION_RETENTION_SOURCE_CONFIG
            },
        ),
    }
}

/// Spawn the `call_logs` rotation scheduler over the shared analytics pool.
///
/// Startup contract: the task's **first action is one complete round** — it
/// does not wait for the 24h tick. The store-open `refresh_views(pool, None)`
/// fallback leaves both views in their full-history shape, which is a
/// *superset* of the real retention window (queries may see extra history,
/// never a missing row and never an error), and this round narrows them to the
/// real cutoff. Callers must spawn this **after** the store has been opened
/// (`CallLogCollector::new_shared` / `SqliteLogStore::from_pool`), so the
/// `ensure_schema_consistency` column catches and the fallback view are already
/// in place when the first round starts (§D: ensure never runs concurrently
/// with the first round). Waiting a full interval would leave that superset in
/// place for a day; running the round synchronously inside
/// `load_analytics_stack` would put maintenance DDL on the startup critical
/// path and add a startup failure mode, so the round runs in the background
/// task instead (sub-second in practice: a few DDL statements plus scalar
/// queries).
///
/// Round contract: retire indexes → rotate → refresh views (see
/// [`rotation_round`]). DB errors are warned about and retried on the next
/// round — maintenance never panics and never takes the gateway down.
fn spawn_rotation_scheduler(
    pool: sag_analytics::SqlitePool,
    config: sag_config::ConfigHandle,
) -> RotationSchedulerHandle {
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        // Round-to-round log state: the session registry arm line fires on the
        // first round and whenever the effective value changes after that.
        let mut session_prune_log = SessionPruneLogState::default();
        // First action of the task: one complete round, immediately (§0).
        rotation_round(&pool, &config, &mut session_prune_log).await;

        let mut ticker = tokio::time::interval(ROTATION_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // A tokio interval's first tick fires immediately. Consume it — the
        // round above already ran, and the next one belongs a full interval
        // later.
        ticker.tick().await;

        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    info!("rotation scheduler stopped");
                    return;
                }
                _ = ticker.tick() => {}
            }
            rotation_round(&pool, &config, &mut session_prune_log).await;
        }
    });
    RotationSchedulerHandle { task, shutdown_tx }
}

/// One `call_logs` rotation round: **retire → rotate → refresh**, in that
/// order (the order is load-bearing: the index sweep must run against the cold
/// tables the previous round produced, and the refresh must be the last step so
/// the views end up matching the cutoff the round just computed). The sequence
/// itself is [`sag_analytics::run_rotation_round`] — the single definition
/// shared with the integration tests — so this wrapper only supplies the
/// per-round config read, the clamp and the scheduler logging.
///
/// Retention is re-read from the live config on **every** round, so a hot
/// reload / admin change takes effect on the next round without a restart.
/// `call_log_retention_days == 0` disables the feature: no retirement, no
/// rotation and no window narrowing (the full-history view shape is the
/// terminal state) — the shared round reports that as
/// [`sag_analytics::RoundOutcome::Skipped`] without touching the database.
///
/// `refresh_views` runs unconditionally inside the shared round, including on
/// rounds that rotate nothing, because the retention cutoff is a literal inside
/// the view bodies — an un-refreshed window would never move forward.
///
/// Session registry retention rides the same round: `session_registry` is
/// *not* a rotation target (rotation only renames `call_logs` /
/// `call_logs_arch_*`), so its rows are swept by
/// [`sag_analytics::prune_session_registry`] at the end of every round, with
/// its own `session_retention_days` re-read from the live config. `0` means
/// "keep forever" there too — the prune then reports zero deletions and
/// issues no DELETE. The two halves stay independent: a failed rotation still
/// sweeps the registry, and a failed sweep never fails the round.
///
/// Because that sweep *deletes* rows instead of archiving them, the effective
/// session retention and its source are announced once — on the first round and
/// again on every change (see [`SessionPruneLogState`]). An omitted key falling
/// back to the default 7-day window is otherwise invisible until the first
/// sweep drops data.
///
/// Every failure is warned about and retried on the next round: maintenance
/// never panics and never takes the gateway down.
async fn rotation_round(
    pool: &sag_analytics::SqlitePool,
    config: &sag_config::ConfigHandle,
    session_prune_log: &mut SessionPruneLogState,
) {
    // One config read per round feeds both retention knobs: the rotation
    // cutoff below and the session registry sweep at the end. It also tells the
    // arm line below whether the session retention is the built-in default or
    // an explicit setting.
    let (retention, session_retention, session_retention_source) = {
        let cfg = config.read().await;
        round_retention(&cfg)
    };
    // Clamp before the round computes its cutoff: the retention decides *when*
    // a row leaves the hot window, so a value beyond the clamp is already
    // "rotate nothing". The clamp keeps the cutoff in a sane domain (a
    // `u32::MAX` day count would otherwise ask for a window ≈ 11.7 million
    // years long); the round's own arithmetic is saturating on top of this.
    let effective_retention = retention.min(MAX_CALL_LOG_RETENTION_DAYS);
    if effective_retention != retention {
        tracing::debug!(
            requested_days = retention,
            effective_days = effective_retention,
            "call_log_retention_days exceeds the supported maximum; clamping the rotation cutoff"
        );
    }

    match sag_analytics::run_rotation_round(pool, effective_retention).await {
        Ok(outcome) => {
            if let Some(arch) = outcome.rotated() {
                tracing::info!(arch, "rotation round moved call_logs into archive");
            }
        }
        // A failed round rolled back atomically; the next round retries every
        // step (retirement, rotation and refresh).
        Err(e) => tracing::warn!(error = %e, "rotation round failed; retrying next cycle"),
    }

    // Clamp the session retention with the rotation bound, for the same
    // reason: the prune cutoff is `now - days * 1 day`, so keeping the day
    // count in a sane domain keeps that subtraction bounded (the prune is
    // saturating on top of this). `0` passes through untouched — it means
    // "keep forever" and short-circuits inside the prune.
    let effective_session_retention = session_retention.min(MAX_CALL_LOG_RETENTION_DAYS);
    if effective_session_retention != session_retention {
        tracing::debug!(
            requested_days = session_retention,
            effective_days = effective_session_retention,
            "session_retention_days exceeds the supported maximum; clamping the prune cutoff"
        );
    }

    // Announce the armed policy before the first sweep, and again whenever the
    // effective value (or its source) changes under a hot reload / admin
    // mutation. Rounds that repeat the same pair stay silent so the daily round
    // is not noise; the per-round outcome below keeps its own log level.
    if session_prune_log.observe(effective_session_retention, session_retention_source) {
        tracing::info!(
            retention_days = effective_session_retention,
            source = session_retention_source,
            // `0` is the one value that is *not* armed: it keeps rows forever.
            keep_forever = effective_session_retention == 0,
            "session registry prune armed"
        );
    }

    match sag_analytics::prune_session_registry(
        pool,
        effective_session_retention,
        chrono::Utc::now(),
    )
    .await
    {
        // Nothing expired (and always, for the `0` = keep-forever setting):
        // stay at `debug!` so the daily round is not noise.
        Ok(0) => tracing::debug!(
            retention_days = effective_session_retention,
            "no expired session_registry rows to prune"
        ),
        Ok(deleted) => tracing::info!(
            deleted,
            retention_days = effective_session_retention,
            "pruned expired session_registry rows"
        ),
        // Independent of the rotation outcome above; retried next cycle.
        Err(e) => {
            tracing::warn!(error = %e, "session_registry prune failed; retrying next cycle")
        }
    }
}

/// Handle for the `call_logs` rotation scheduler background task.
struct RotationSchedulerHandle {
    task: tokio::task::JoinHandle<()>,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

impl RotationSchedulerHandle {
    /// Signal the scheduler to stop and wait up to 5s for it to exit.
    ///
    /// The join is the only way to know the scheduler's in-flight DDL
    /// transaction has ended and the task has stopped. Dropping the handle
    /// would *detach* the task (`JoinHandle::drop` does not abort), leaving it
    /// free to run further rounds behind the caller's back, so a timeout is
    /// logged as an error **and** the task is aborted: the abort cancels it at
    /// its next await point and an in-flight DDL transaction rolls back
    /// (SQLite atomicity). The abort is not awaited, so on the timeout path the
    /// shutdown-order guarantee (drain happens last, no background writer after
    /// it) only holds approximately — the drain may race the rollback briefly.
    /// SQLite transactions stay atomic either way.
    async fn shutdown(self) {
        self.shutdown_with_timeout(Duration::from_secs(5)).await;
    }

    /// The body of [`Self::shutdown`] with an injectable join bound.
    ///
    /// Production always passes 5s; the parameter exists so the timeout →
    /// abort path is testable in milliseconds instead of taking the full
    /// bound (that path is the one that decides whether a hung task can
    /// outlive shutdown). Behavior is identical to `shutdown()` otherwise.
    async fn shutdown_with_timeout(self, join_timeout: Duration) {
        // `timeout` consumes the `JoinHandle`, so grab the abort handle first.
        let abort = self.task.abort_handle();
        let _ = self.shutdown_tx.send(());
        match tokio::time::timeout(join_timeout, self.task).await {
            Ok(Ok(())) => info!("rotation scheduler shut down"),
            Ok(Err(e)) => tracing::error!(error = %e, "rotation scheduler task panicked"),
            Err(_) => {
                tracing::error!(
                    timeout = ?join_timeout,
                    "rotation scheduler did not stop in time; aborting it"
                );
                abort.abort();
            }
        }
    }
}

/// Handle for the rollup scheduler background task.
struct RollupSchedulerHandle {
    task: tokio::task::JoinHandle<()>,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

impl RollupSchedulerHandle {
    /// Signal the scheduler to stop and wait up to 5s for it to exit.
    ///
    /// The join is the only way to know the scheduler's current bucket
    /// compute transaction has ended and the task has stopped. Dropping the
    /// handle would *detach* the task (`JoinHandle::drop` does not abort),
    /// leaving it free to compute buckets behind the caller's back, so a
    /// timeout is logged as an error **and** the task is aborted: the abort
    /// cancels it at its next await point and an in-flight compute transaction
    /// rolls back (SQLite atomicity; the bucket stays `pending` until the lease
    /// is reclaimed). The abort is not awaited, so on the timeout path the
    /// shutdown-order guarantee (drain happens last, no background writer after
    /// it) only holds approximately — the drain may race the rollback briefly.
    async fn shutdown(self) {
        self.shutdown_with_timeout(Duration::from_secs(5)).await;
    }

    /// The body of [`Self::shutdown`] with an injectable join bound.
    ///
    /// Production always passes 5s; the parameter exists so the timeout →
    /// abort path is testable in milliseconds instead of taking the full
    /// bound (that path is the one that decides whether a hung task can
    /// outlive shutdown). Behavior is identical to `shutdown()` otherwise.
    async fn shutdown_with_timeout(self, join_timeout: Duration) {
        // `timeout` consumes the `JoinHandle`, so grab the abort handle first.
        let abort = self.task.abort_handle();
        let _ = self.shutdown_tx.send(());
        match tokio::time::timeout(join_timeout, self.task).await {
            Ok(Ok(())) => info!("rollup scheduler shut down"),
            Ok(Err(e)) => tracing::error!(error = %e, "rollup scheduler task panicked"),
            Err(_) => {
                tracing::error!(
                    timeout = ?join_timeout,
                    "rollup scheduler did not stop in time; aborting it"
                );
                abort.abort();
            }
        }
    }
}

/// Resolves the configuration file path from CLI arguments.
///
/// Precedence:
/// 1. `--config <path>` flag (most explicit)
/// 2. First non-flag positional argument (legacy `sag <path>` shape)
/// 3. `./gateway.toml` when it exists — project-local layout kept working so
///    existing checkouts need no migration (dev track)
/// 4. Default: `$XDG_CONFIG_HOME/sag/gateway.toml` (XDG Base Directory spec)
///
/// Both CLI shapes are accepted so existing invocations like
/// `./target/release/sag /etc/sag/gateway.toml` keep working, while
/// container / docker flows can use `./sag --config /etc/sag/gateway.toml`.
/// Unknown flag-like tokens are warned about on stderr and ignored (not a
/// hard error) so future flag additions don't break older invocations.
fn resolve_config_path() -> anyhow::Result<String> {
    let (explicit, positional) = parse_config_args(std::env::args().skip(1))?;
    if let Some(path) = explicit.or(positional) {
        return Ok(path);
    }
    // Project-local fallback: an existing `gateway.toml` next to the CWD
    // wins over the XDG default (backward compatibility with the previous
    // CWD-relative default).
    if Path::new(sag_core::CONFIG_FILE_NAME).exists() {
        return Ok(sag_core::CONFIG_FILE_NAME.to_string());
    }
    Ok(sag_core::paths::default_config_path()
        .to_string_lossy()
        .into_owned())
}

/// Parse CLI args into `(explicit, positional)` config path candidates.
///
/// `--config <path>` / `--config=<path>` fill `explicit`; the first non-flag
/// token fills `positional`; unknown flag-like tokens are warned about on
/// stderr and dropped. Pure over the iterator so the policy is unit-testable.
fn parse_config_args<I>(args: I) -> anyhow::Result<(Option<String>, Option<String>)>
where
    I: Iterator<Item = String>,
{
    let mut explicit: Option<String> = None;
    let mut positional: Option<String> = None;
    let mut args = args;
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
            // Unknown flag-like token: warn and ignore so future flag additions
            // don't break older invocations. Examples: `--verbose`, `-v`.
            // Tracing is not initialized yet at this point — use stderr.
            eprintln!("warning: unknown flag '{arg}' ignored");
            continue;
        } else if positional.is_none() {
            positional = Some(arg);
        }
        // Non-`--config` positional tokens after the first are dropped to keep
        // parsing deterministic.
    }
    Ok((explicit, positional))
}

/// Number of days to retain rotating log files (XDG state dir sag.log.*).
/// Files with a modification time older than this are deleted on startup.
const LOG_RETENTION_DAYS: u64 = 7;

/// Delete daily rotating log files in the XDG state log dir
/// (`$XDG_STATE_HOME/sag/logs`) older than [`LOG_RETENTION_DAYS`].
///
/// Best-effort: errors on individual files are logged and skipped;
/// a missing log directory is a silent no-op.
fn cleanup_old_logs() {
    let log_dir = sag_core::paths::default_log_dir();
    let dir = match std::fs::read_dir(&log_dir) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(error = %e, "cannot read log directory for cleanup");
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

#[cfg(test)]
mod tests {
    use super::*;
    use sag_analytics::BucketState;
    use sag_core::{FinalStatus, RequestResultFact};

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    /// CLI precedence: `--config <path>` wins over a positional token.
    #[test]
    fn parse_config_args_explicit_flag_wins_over_positional() {
        let (explicit, positional) = parse_config_args(
            args(&["gateway.toml", "--config", "/etc/sag/gateway.toml"]).into_iter(),
        )
        .unwrap();
        assert_eq!(explicit.as_deref(), Some("/etc/sag/gateway.toml"));
        assert_eq!(positional.as_deref(), Some("gateway.toml"));
    }

    /// The `--config=<path>` form is accepted (docker / systemd convenience).
    #[test]
    fn parse_config_args_accepts_eq_form() {
        let (explicit, positional) =
            parse_config_args(args(&["--config=/etc/sag/gateway.toml"]).into_iter()).unwrap();
        assert_eq!(explicit.as_deref(), Some("/etc/sag/gateway.toml"));
        assert_eq!(positional, None);
    }

    /// Unknown flag-like tokens are dropped (with a stderr warning); the
    /// first non-flag token is the positional config path.
    #[test]
    fn parse_config_args_drops_unknown_flags() {
        let (explicit, positional) =
            parse_config_args(args(&["--verbose", "-v", "gateway.toml"]).into_iter()).unwrap();
        assert_eq!(explicit, None);
        assert_eq!(positional.as_deref(), Some("gateway.toml"));
    }

    /// A bare `--config` without a value is a hard error.
    #[test]
    fn parse_config_args_requires_value_after_flag() {
        assert!(parse_config_args(args(&["--config"]).into_iter()).is_err());
    }

    /// Build one `client_error` fact (no route, no attempts — the simplest
    /// fact that still exercises the full write → register → compute
    /// path) at `completed_at` (unix ms) through the formal public API.
    fn client_error_event(completed_at: i64) -> sag_analytics::AnalyticsEvent {
        use chrono::{TimeZone, Utc};
        let completed_at = Utc.timestamp_millis_opt(completed_at).unwrap();
        let started_at = completed_at - chrono::Duration::milliseconds(250);
        sag_analytics::AnalyticsEvent::new(RequestResultFact {
            request_id: uuid::Uuid::new_v4(),
            route_name: None,
            requested_model: "test-model".to_string(),
            final_status: FinalStatus::ClientError,
            failure_reason: None,
            stream: false,
            started_at,
            completed_at,
            usage: None,
            cached_tokens: None,
            cache_creation_tokens: None,
            ttft_ms: None,
            upstream_duration_ms: None,
            ingress_protocol: None,
            lane: None,
        })
    }

    /// Startup contract against a real (in-memory) analytics DB: the
    /// scheduler's background registration marks the fact's hour `pending`
    /// — and never computes it (the first cycle is a full hour away).
    /// Shutdown stops the task with no leak, and the bucket stays `pending`.
    #[tokio::test]
    async fn rollup_scheduler_startup_registers_never_computes() {
        let pool = sag_analytics::open_analytics_pool(":memory:")
            .await
            .unwrap();
        let engine = Arc::new(sag_analytics::RollupEngine::new(pool));

        // One fact in the current hour bucket (via the formal writer).
        // `completed_at` is deliberately 1 ms in the past: the scheduler's
        // registration scan bounds its hour query by the *current* time
        // (right-open: `completed_at < to_ms`), so a fact stamped with the
        // exact same millisecond as the scan would fall outside the window
        // and never be registered (flaky). One millisecond of skew makes
        // `completed_at < to_ms` hold unconditionally.
        let fact_completed_at = unix_now_ms() - 1;
        let hour =
            fact_completed_at / sag_analytics::rollup::HOUR_MS * sag_analytics::rollup::HOUR_MS;
        sag_analytics::EventWriter::new(engine.pool().clone())
            .write(client_error_event(fact_completed_at))
            .await
            .unwrap();

        // Production-like interval: a full hour — only registration may run.
        let handle = spawn_rollup_scheduler(engine.clone(), Duration::from_secs(3600));

        // Registration is async in the background task; poll for it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match engine.bucket_status(hour).await.unwrap() {
                Some(status) => {
                    assert_eq!(
                        status.state,
                        BucketState::Pending,
                        "startup registration must never compute"
                    );
                    assert_eq!(status.attempt, 0, "no claim may happen at startup");
                    break;
                }
                None => {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "startup registration never landed"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }

        // Shutdown stops the task promptly and the join reports no panic.
        handle.shutdown().await;

        let status = engine.bucket_status(hour).await.unwrap().unwrap();
        assert_eq!(status.state, BucketState::Pending);
    }

    // ─── call_logs rotation scheduler ────────────────────────────────

    const DAY_MS: i64 = 86_400_000;

    /// Minimal config for maintenance tests: `analytics` is present so the
    /// retention reads (`call_log_retention_days` / `session_retention_days`)
    /// are exercised; everything else is the empty / default shape (no
    /// providers, no routes).
    fn maintenance_config(retention_days: u32) -> sag_config::ConfigHandle {
        maintenance_config_with_session_retention(
            retention_days,
            sag_config::DEFAULT_SESSION_RETENTION_DAYS,
        )
    }

    /// [`maintenance_config`] with the session registry retention spelled out.
    fn maintenance_config_with_session_retention(
        call_log_retention_days: u32,
        session_retention_days: u32,
    ) -> sag_config::ConfigHandle {
        sag_config::ConfigHandle::from_config(sag_config::GatewayConfig {
            server: sag_config::ServerConfig {
                host: "127.0.0.1".into(),
                port: 3000,
            },
            analytics: Some(sag_config::AnalyticsConfig {
                enabled: true,
                storage_dsn: "sqlite://analytics.db".into(),
                call_log_retention_days,
                session_retention_days,
            }),
            gateway_auth: None,
            providers: HashMap::new(),
            backoff: sag_config::BackoffConfig::default(),
            circuit_breaker: sag_config::CircuitBreakerConfig::default(),
            shuffle_cache_ttl_secs: 7200,
            sticky_failover_reset_secs: 7200,
            routes: Vec::new(),
            logging: sag_config::LoggingConfig::default(),
            proxy_url: None,
            privacy: sag_config::PrivacyConfig::default(),
        })
    }

    /// One legacy call log stamped `timestamp_ms`.
    fn call_log(model: &str, timestamp_ms: i64) -> sag_core::CallLog {
        use chrono::{TimeZone, Utc};
        sag_core::CallLog {
            request_id: uuid::Uuid::new_v4(),
            provider: "openai".to_string(),
            model: model.to_string(),
            real_model: model.to_string(),
            stream: false,
            status_code: 200,
            duration_ms: 10,
            usage: None,
            error: None,
            timestamp: Utc.timestamp_millis_opt(timestamp_ms).unwrap(),
            reasoning_effort: None,
            upstream_duration_ms: None,
            ttft_ms: None,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
            user_agent: None,
            client_ip: None,
            error_category: None,
            route_id: None,
            target_id: None,
            ingress_protocol: None,
            upstream_protocol: None,
            lane: None,
            loss_summary: None,
            session_key: None,
        }
    }

    /// One `session_registry` fact seen at `at_ms` (source `meta`, no
    /// conversation id / preview: only the timestamps matter to the prune).
    fn session_fact(key: &str, at_ms: i64) -> sag_analytics::SessionRegistryFact {
        use chrono::{TimeZone, Utc};
        let at = Utc.timestamp_millis_opt(at_ms).unwrap();
        sag_analytics::SessionRegistryFact {
            session_key: key.to_string(),
            session_source: "meta".to_string(),
            conversation_id: None,
            first_message_preview: None,
            first_seen: at,
            last_seen: at,
        }
    }

    /// `SELECT COUNT(*)` of a scalar query.
    async fn scalar(pool: &sag_analytics::SqlitePool, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
    }

    /// Sends when the task's future is dropped — i.e. when the abort lands.
    struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    /// A fake scheduler task that ignores its shutdown signal (only the abort
    /// can end it) plus the pieces a scheduler handle needs. `dropped_rx`
    /// resolves when the task's future is dropped, which is how the abort is
    /// observed.
    fn pending_scheduler_task() -> (
        tokio::task::JoinHandle<()>,
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _guard = DropSignal(Some(dropped_tx));
            // The fake scheduler never observes the shutdown signal: only the
            // abort can end it. Holding the receiver here keeps the send
            // succeeding (that it is ignored is the point).
            let _shutdown_rx = shutdown_rx;
            std::future::pending::<()>().await;
        });
        (task, shutdown_tx, dropped_rx)
    }

    /// Assert that a scheduler `shutdown_with_timeout` call returns at the
    /// injected bound (instead of waiting for a task that never finishes) and
    /// **aborts** the task rather than detaching it — the abort drops the
    /// task's future, which is what runs `DropSignal::drop`.
    async fn assert_shutdown_aborts_on_timeout<F>(
        shutdown: F,
        timeout: Duration,
        dropped_rx: tokio::sync::oneshot::Receiver<()>,
    ) where
        F: std::future::Future<Output = ()>,
    {
        let started = tokio::time::Instant::now();
        shutdown.await;
        let elapsed = started.elapsed();

        // ① The call returned on the timeout instead of waiting for a task that
        //    never finishes (and it really did take the timeout path).
        assert!(
            elapsed >= timeout,
            "shutdown returned before the join bound ({elapsed:?})"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "shutdown must not block past the bound ({elapsed:?})"
        );

        // ② The task was aborted, not detached: its future is dropped. The
        //    abort is fire-and-forget, so poll with a bound rather than
        //    assuming it lands synchronously.
        tokio::time::timeout(Duration::from_secs(5), dropped_rx)
            .await
            .expect("aborted task was never dropped (task leaked)")
            .expect("drop signal sender vanished");
    }

    /// RAII temporary directory: `Drop` removes the tree, so a panicking
    /// assertion still cleans up instead of leaking `sag-gateway-*` dirs into
    /// the system temp dir (the same contract `sag-analytics`' `TempDb` has).
    struct TempDir {
        path: std::path::PathBuf,
    }

    impl TempDir {
        /// Unique per call (`uuid` suffix), so tests running in parallel can
        /// never collide on the same path.
        fn new(prefix: &str) -> Self {
            let path = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("failed to create temp dir");
            Self { path }
        }

        fn join(&self, name: &str) -> std::path::PathBuf {
            self.path.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// §F test 17 — shutdown ordering. The rotation scheduler runs one
    /// complete round as its first action (no 24h wait), `shutdown` joins it
    /// so no DDL transaction is in flight, and both analytics writers are
    /// stopped through [`shutdown_analytics_writers`] (the exact production
    /// helper `graceful_shutdown` uses) **before** the event drain — which is
    /// what makes the drain the last writer on the analytics DB.
    #[tokio::test]
    async fn rotation_scheduler_shuts_down_before_event_drain() {
        let dir = TempDir::new("sag-gateway-rotation");
        let db = dir.join("analytics.db");
        let pool = sag_analytics::open_analytics_pool(db.to_str().unwrap())
            .await
            .unwrap();
        // Production assembly: the legacy collector shares the one pool.
        let collector = Arc::new(
            sag_analytics::CallLogCollector::new_shared(pool.clone())
                .await
                .unwrap(),
        );

        // A row 400 days old sits outside the 90-day window, so the
        // scheduler's first round must rotate it.
        collector.log(call_log("backlog", unix_now_ms() - 400 * DAY_MS));
        collector.flush().await;

        // Both analytics writers, wired in the production order that
        // `load_analytics_stack` uses: the store is opened first (its
        // `ensure_schema_consistency` + full-history fallback view run to
        // completion), and only then are the schedulers spawned — so the first
        // rotation round always starts on a prepared schema (§D).
        let rollup = spawn_rollup_scheduler(
            Arc::new(sag_analytics::RollupEngine::new(pool.clone())),
            ROLLUP_INTERVAL,
        );
        let rotation = spawn_rotation_scheduler(pool.clone(), maintenance_config(90));

        // The first round is the task's first action (§0): it must land
        // without waiting for the 24h tick.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let cold = scalar(
                &pool,
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
                 AND name LIKE 'call_logs_arch_%'",
            )
            .await;
            if cold == 1 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the scheduler's first round never rotated the backlog"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // The round completed, not half-applied: the hot table is the fresh
        // empty one and both views were rebuilt against the real cutoff.
        assert_eq!(scalar(&pool, "SELECT COUNT(*) FROM call_logs").await, 0);
        assert_eq!(
            scalar(&pool, "SELECT COUNT(*) FROM call_logs_hot").await,
            0,
            "the rotated row is outside the 90-day window"
        );
        assert_eq!(
            scalar(&pool, "SELECT COUNT(*) FROM call_logs_all").await,
            1,
            "history stays reachable through call_logs_all"
        );

        // Production shutdown order: rollup, then rotation (both joined).
        shutdown_analytics_writers(rollup, rotation).await;

        // The join really ended the in-flight DDL: a write issued now lands in
        // the fresh hot table and is immediately visible through the rebuilt
        // view (a still-running rename / view rebuild would break either).
        let marker = call_log("after-shutdown", unix_now_ms());
        let marker_id = marker.request_id;
        collector.log(marker);
        collector.flush().await;
        let in_hot: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM call_logs WHERE request_id = ?1")
                .bind(marker_id.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(in_hot, 1, "post-shutdown writes land in the hot table");
        assert_eq!(
            scalar(&pool, "SELECT COUNT(*) FROM call_logs_hot").await,
            1,
            "the rebuilt view sees the post-shutdown write"
        );
        assert_eq!(
            scalar(
                &pool,
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
                 AND name LIKE 'call_logs_arch_%'"
            )
            .await,
            1,
            "no round may rotate after the scheduler was joined"
        );

        // 15. Drain last: the formal event queue is flushed only now, so it is
        // the final writer — it must not disturb the rotated call_logs world.
        let event_collector =
            sag_analytics::EventCollector::new(sag_analytics::EventWriter::new(pool.clone()));
        event_collector
            .emit(client_error_event(unix_now_ms()))
            .await
            .unwrap();
        event_collector.shutdown().await;
        assert_eq!(
            scalar(&pool, "SELECT COUNT(*) FROM request_results").await,
            1,
            "the drain persisted its queued event after the writers stopped"
        );
        assert_eq!(
            scalar(&pool, "SELECT COUNT(*) FROM call_logs_hot").await,
            1,
            "the drain must not touch the rotated call_logs views"
        );

        pool.close().await;
        // `dir`'s `Drop` removes the directory — including on an assertion
        // panic above, which is why it is not a manual `remove_dir_all` here.
    }

    /// §F 17 (timeout half) — a task that ignores the shutdown signal must not
    /// hang process exit: `shutdown_with_timeout` returns at the bound and
    /// aborts the task. The short injected bound keeps the test at ~50ms
    /// instead of the production 5s; `shutdown()` still passes 5s.
    #[tokio::test]
    async fn rotation_scheduler_shutdown_aborts_task_on_join_timeout() {
        let (task, shutdown_tx, dropped_rx) = pending_scheduler_task();
        let handle = RotationSchedulerHandle { task, shutdown_tx };

        let timeout = Duration::from_millis(50);
        assert_shutdown_aborts_on_timeout(
            handle.shutdown_with_timeout(timeout),
            timeout,
            dropped_rx,
        )
        .await;
    }

    /// Retention clamp: a huge `call_log_retention_days` must not panic on the
    /// cutoff arithmetic and keeps the "keep everything" semantics (no
    /// rotation, the in-window row stays in the hot table).
    #[tokio::test]
    async fn rotation_round_clamps_extreme_retention_without_panicking() {
        let pool = sag_analytics::open_analytics_pool(":memory:")
            .await
            .unwrap();
        let collector = sag_analytics::CallLogCollector::new_shared(pool.clone())
            .await
            .unwrap();
        collector.log(call_log("recent", unix_now_ms()));
        collector.flush().await;

        rotation_round(
            &pool,
            &maintenance_config(u32::MAX),
            &mut SessionPruneLogState::default(),
        )
        .await;

        assert_eq!(
            scalar(
                &pool,
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
                 AND name LIKE 'call_logs_arch_%'"
            )
            .await,
            0,
            "a clamped (effectively infinite) retention must not rotate"
        );
        assert_eq!(
            scalar(&pool, "SELECT COUNT(*) FROM call_logs").await,
            1,
            "the row stays in the hot table"
        );
        assert_eq!(
            scalar(&pool, "SELECT COUNT(*) FROM call_logs_hot").await,
            1,
            "the clamped cutoff still covers every row"
        );

        pool.close().await;
    }

    /// §F 15 (config half) — retention is re-read from the live config on
    /// *every* round: the same handle that disables rotation at `0` rotates
    /// once it is mutated to 90 days, with no restart and no re-spawn. This
    /// pins [`rotation_round`]'s per-round `config.read()` (the hot-reload /
    /// admin-mutation path); the integration suite replays the round with the
    /// retention passed as an argument, so it structurally cannot see it.
    #[tokio::test]
    async fn rotation_round_rereads_retention_from_live_config() {
        let pool = sag_analytics::open_analytics_pool(":memory:")
            .await
            .unwrap();
        // Store open = the production startup path (`ensure_schema_consistency`
        // + full-history fallback view).
        let collector = sag_analytics::CallLogCollector::new_shared(pool.clone())
            .await
            .unwrap();
        // One row 400 days old: outside the 90-day window, inside the
        // full-history startup shape.
        collector.log(call_log("backlog", unix_now_ms() - 400 * DAY_MS));
        collector.flush().await;

        // retention = 0 disables the round entirely.
        let config = maintenance_config(0);
        let mut session_prune_log = SessionPruneLogState::default();
        rotation_round(&pool, &config, &mut session_prune_log).await;
        assert_eq!(
            scalar(
                &pool,
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
                 AND name LIKE 'call_logs_arch_%'"
            )
            .await,
            0,
            "retention 0 must not rotate"
        );
        assert_eq!(
            scalar(&pool, "SELECT COUNT(*) FROM call_logs_hot").await,
            1,
            "retention 0 keeps the full-history view shape"
        );

        // Hot reload / admin mutation: same handle, new value — the next round
        // must pick it up in-round (nothing is cached at spawn time).
        config.write().await.analytics = Some(sag_config::AnalyticsConfig {
            enabled: true,
            storage_dsn: "sqlite://analytics.db".into(),
            call_log_retention_days: 90,
            session_retention_days: sag_config::DEFAULT_SESSION_RETENTION_DAYS,
        });
        rotation_round(&pool, &config, &mut session_prune_log).await;
        assert_eq!(
            scalar(
                &pool,
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
                 AND name LIKE 'call_logs_arch_%'"
            )
            .await,
            1,
            "the next round reads the mutated retention and rotates"
        );
        assert_eq!(
            scalar(&pool, "SELECT COUNT(*) FROM call_logs_hot").await,
            0,
            "the window narrowed to the new 90-day cutoff"
        );

        pool.close().await;
    }

    /// Session registry retention rides the same round: expired rows are
    /// swept with the live `session_retention_days`, a hot reload to `0`
    /// stops the sweep entirely (keep forever), and the sweep never reaches
    /// the `call_logs` side of the round.
    #[tokio::test]
    async fn rotation_round_prunes_session_registry_from_live_config() {
        let pool = sag_analytics::open_analytics_pool(":memory:")
            .await
            .unwrap();
        let collector = sag_analytics::CallLogCollector::new_shared(pool.clone())
            .await
            .unwrap();
        // A 400-day-old call log must survive the round: `call_logs` retention
        // is 0 (rotation disabled) and the session sweep touches a different
        // table.
        collector.log(call_log("backlog", unix_now_ms() - 400 * DAY_MS));
        collector.flush().await;

        let now = unix_now_ms();
        sag_analytics::upsert_session(&pool, &session_fact("expired", now - 30 * DAY_MS))
            .await
            .unwrap();
        sag_analytics::upsert_session(&pool, &session_fact("fresh", now))
            .await
            .unwrap();

        // The round's per-round config read covers both retentions: 7 days
        // puts the 30-day-old session outside the window.
        let config = maintenance_config_with_session_retention(0, 7);
        let mut session_prune_log = SessionPruneLogState::default();
        rotation_round(&pool, &config, &mut session_prune_log).await;
        assert!(
            sag_analytics::get_session(&pool, "expired")
                .await
                .unwrap()
                .is_none(),
            "a session outside the retention window is pruned"
        );
        assert!(
            sag_analytics::get_session(&pool, "fresh")
                .await
                .unwrap()
                .is_some(),
            "a session inside the window survives"
        );
        assert_eq!(
            scalar(&pool, "SELECT COUNT(*) FROM call_logs").await,
            1,
            "the session sweep must not delete call_logs rows"
        );

        // Hot reload / admin mutation: same handle, `session_retention_days = 0`
        // means "keep forever" — not even a row stamped at the epoch goes.
        sag_analytics::upsert_session(&pool, &session_fact("ancient", 1))
            .await
            .unwrap();
        config.write().await.analytics = Some(sag_config::AnalyticsConfig {
            enabled: true,
            storage_dsn: "sqlite://analytics.db".into(),
            call_log_retention_days: 0,
            session_retention_days: 0,
        });
        rotation_round(&pool, &config, &mut session_prune_log).await;
        assert!(
            sag_analytics::get_session(&pool, "ancient")
                .await
                .unwrap()
                .is_some(),
            "session_retention_days 0 must not prune"
        );
        assert!(
            sag_analytics::get_session(&pool, "fresh")
                .await
                .unwrap()
                .is_some(),
            "an in-window session is untouched"
        );

        pool.close().await;
    }

    /// Session retention clamp: `u32::MAX` days must not panic on the cutoff
    /// arithmetic, and clamping to the ten-year bound keeps the "effectively
    /// keep everything" semantic (only rows older than ten years are swept).
    #[tokio::test]
    async fn rotation_round_clamps_extreme_session_retention_without_panicking() {
        let pool = sag_analytics::open_analytics_pool(":memory:")
            .await
            .unwrap();
        let now = unix_now_ms();
        sag_analytics::upsert_session(&pool, &session_fact("recent", now))
            .await
            .unwrap();
        sag_analytics::upsert_session(&pool, &session_fact("beyond-clamp", now - 4000 * DAY_MS))
            .await
            .unwrap();

        rotation_round(
            &pool,
            &maintenance_config_with_session_retention(0, u32::MAX),
            &mut SessionPruneLogState::default(),
        )
        .await;

        assert!(
            sag_analytics::get_session(&pool, "recent")
                .await
                .unwrap()
                .is_some(),
            "the clamped cutoff still covers recent sessions"
        );
        assert!(
            sag_analytics::get_session(&pool, "beyond-clamp")
                .await
                .unwrap()
                .is_none(),
            "clamping to ten years sweeps rows the clamp cannot cover"
        );

        pool.close().await;
    }

    /// The arm line fires on the first round and then only on a change: the
    /// repeating daily round must not re-announce the same policy, but a hot
    /// reload that moves the window (or flips it to/from the default) must.
    #[test]
    fn session_prune_log_state_logs_first_round_and_each_change_only() {
        let mut state = SessionPruneLogState::default();

        assert!(
            state.observe(7, SESSION_RETENTION_SOURCE_DEFAULT),
            "the first round always announces the armed policy"
        );
        assert!(
            !state.observe(7, SESSION_RETENTION_SOURCE_DEFAULT),
            "an unchanged value stays silent so the daily round is not noise"
        );
        assert!(
            state.observe(30, SESSION_RETENTION_SOURCE_CONFIG),
            "a hot-reloaded retention is announced"
        );
        assert!(
            !state.observe(30, SESSION_RETENTION_SOURCE_CONFIG),
            "repeating the new value stays silent"
        );
        assert!(
            state.observe(30, SESSION_RETENTION_SOURCE_DEFAULT),
            "a change of source alone is a change of meaning"
        );
        assert!(
            state.observe(7, SESSION_RETENTION_SOURCE_DEFAULT),
            "returning to an earlier value is announced again, not deduped away"
        );
        assert!(
            state.observe(0, SESSION_RETENTION_SOURCE_CONFIG),
            "0 (keep forever) is announced: it is the difference between deleting and not"
        );
    }

    /// The arm line's `source` field: an explicit non-default value is
    /// `config`, the default (and a missing `[analytics]` section) is
    /// `default` — the case an operator needs to spot, because omission
    /// deletes rows after 7 days rather than archiving them.
    #[tokio::test]
    async fn round_retention_reports_the_session_retention_source() {
        // Non-default explicit value → `config`.
        let handle = maintenance_config_with_session_retention(0, 30);
        let (_, session, source) = round_retention(&*handle.read().await);
        assert_eq!(
            (session, source),
            (30, SESSION_RETENTION_SOURCE_CONFIG),
            "an explicit non-default retention is reported as configured"
        );

        // `0` = keep forever is explicitly configured, not the default.
        let handle = maintenance_config_with_session_retention(0, 0);
        let (_, session, source) = round_retention(&*handle.read().await);
        assert_eq!(
            (session, source),
            (0, SESSION_RETENTION_SOURCE_CONFIG),
            "0 (keep forever) must not be mistaken for the default"
        );

        // Spelled-out default → indistinguishable from an omitted key.
        let handle = maintenance_config(sag_config::DEFAULT_CALL_LOG_RETENTION_DAYS);
        let (call_logs, session, source) = round_retention(&*handle.read().await);
        assert_eq!(
            (call_logs, session, source),
            (
                sag_config::DEFAULT_CALL_LOG_RETENTION_DAYS,
                sag_config::DEFAULT_SESSION_RETENTION_DAYS,
                SESSION_RETENTION_SOURCE_DEFAULT
            ),
            "an explicit default counts as default: omitted and default are indistinguishable"
        );

        // No `[analytics]` section at all → every knob at its default.
        let mut config = maintenance_config(0).read().await.clone();
        config.analytics = None;
        assert_eq!(
            round_retention(&config),
            (
                sag_config::DEFAULT_CALL_LOG_RETENTION_DAYS,
                sag_config::DEFAULT_SESSION_RETENTION_DAYS,
                SESSION_RETENTION_SOURCE_DEFAULT
            ),
            "no [analytics] section falls back to the defaults, deletions included"
        );
    }

    /// Compute contract: the first run happens after one interval, claims
    /// the pending bucket once, materializes the rollup rows, and
    /// `shutdown` stops the task.
    #[tokio::test]
    async fn rollup_scheduler_first_cycle_computes_bucket() {
        let pool = sag_analytics::open_analytics_pool(":memory:")
            .await
            .unwrap();
        let engine = Arc::new(sag_analytics::RollupEngine::new(pool));

        // One fact in the current hour bucket (via the formal writer).
        // 1 ms skew — see `rollup_scheduler_startup_registers_never_computes`
        // for why the fact must strictly precede the scan's `to_ms`.
        let fact_completed_at = unix_now_ms() - 1;
        let hour =
            fact_completed_at / sag_analytics::rollup::HOUR_MS * sag_analytics::rollup::HOUR_MS;
        sag_analytics::EventWriter::new(engine.pool().clone())
            .write(client_error_event(fact_completed_at))
            .await
            .unwrap();

        let handle = spawn_rollup_scheduler(engine.clone(), Duration::from_millis(100));

        // First compute run happens after the interval — poll for it.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            match engine.bucket_status(hour).await.unwrap() {
                Some(status) if status.state == BucketState::Completed => {
                    // `attempt` counts failures (spec state machine), and
                    // this bucket never failed — first claim + compute
                    // leave it at 0.
                    assert_eq!(status.attempt, 0, "one claim per run, single worker");
                    break;
                }
                _ => {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "rollup never completed the bucket"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }

        // Rollup rows materialized for the hour.
        let rows = engine.global_rows(&[hour]).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].requests, 1);
        assert_eq!(rows[0].client_error, 1);

        // Shutdown: task stops promptly and the join reports no panic.
        handle.shutdown().await;
    }

    /// §F 17 (rollup timeout half) — the same guard as the rotation scheduler:
    /// a rollup task that ignores the shutdown signal must not hang process
    /// exit. `shutdown_with_timeout` returns at the injected bound and aborts
    /// the task; `shutdown()` still passes the production 5s.
    #[tokio::test]
    async fn rollup_scheduler_shutdown_aborts_task_on_join_timeout() {
        let (task, shutdown_tx, dropped_rx) = pending_scheduler_task();
        let handle = RollupSchedulerHandle { task, shutdown_tx };

        let timeout = Duration::from_millis(50);
        assert_shutdown_aborts_on_timeout(
            handle.shutdown_with_timeout(timeout),
            timeout,
            dropped_rx,
        )
        .await;
    }
}
