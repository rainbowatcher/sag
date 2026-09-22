//! SAG — Smart AI Gateway
//!
//! An AI gateway that proxies chat completion requests to multiple LLM providers
//! with priority-based routing, automatic fallback, and usage analytics.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use async_trait::async_trait;
use sag_config::ConfigReloadHandler;
use sag_state::ConfigMutationCoordinator;
use tokio::sync::RwLock;
use tracing::info;

mod tracing_init;

#[cfg(feature = "embed-ui")]
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
    //    If the file can't be read or the keys are missing, defaults apply.
    let (log_level, log_format) = sag_config::peek_logging_config(&config_path);
    // Declared first in main() → dropped last by Rust's reverse drop order,
    // keeping the non-blocking writer alive through `info!("all subsystems shut down")`
    // and through the `tracing::error!` emitted when startup fails.
    let _log_guard = tracing_init::init(Some(&log_level), log_format);

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

    // 5. Initialize analytics (shared pool + identity registry + query
    //    engine + rollup scheduler + legacy collector + event collector).
    let analytics = load_analytics_stack(&analytics_dsn, routes_identity).await?;
    let AnalyticsStack {
        identity_registry,
        analytics_query,
        collector,
        event_collector,
        rollup_scheduler_handle,
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
    };
    // `with_shared_state` shares the same Arcs (rather than creating
    // defaults) so AppState and DispatchRuntime observe the same breaker,
    // the same sticky memory, and the same fill-first order cache.
    let dispatch_runtime = sag_router::DispatchRuntime::with_shared_state(
        circuit_breaker.clone(),
        active_targets,
        fill_first_orders,
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
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("gateway shut down");

    // 14–15. Shut down watchers / rollup, then drain the event queue.
    graceful_shutdown(
        rollup_scheduler_handle,
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

/// Analytics subsystem handles assembled over ONE shared pool.
///
/// `event_collector` and `rollup_scheduler_handle` are consumed by
/// [`graceful_shutdown`]; the rest are handed to `AppState` / the
/// coordinator. The pool itself is not retained here — it is moved into
/// the [`sag_analytics::EventWriter`] and shared via `Arc` clones held by
/// the registry / query engine / rollup engine / legacy collector.
struct AnalyticsStack {
    identity_registry: Arc<sag_analytics::IdentityRegistry>,
    analytics_query: sag_analytics::AnalyticsQueryEngine,
    collector: Arc<sag_analytics::CallLogCollector>,
    event_collector: Arc<sag_analytics::EventCollector>,
    rollup_scheduler_handle: RollupSchedulerHandle,
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
) -> anyhow::Result<AnalyticsStack> {
    let analytics_pool = sag_analytics::open_analytics_pool(analytics_dsn).await?;
    let identity_registry = Arc::new(sag_analytics::IdentityRegistry::new(analytics_pool.clone()));
    identity_registry.sync(&routes_identity).await?;
    let analytics_query = sag_analytics::AnalyticsQueryEngine::new(analytics_pool.clone());
    // 5a. 1h rollup engine over the SAME shared pool (one pool for writer /
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
    // 5b. The legacy CallLogCollector (backing `/v1/analytics/*` and
    //     `/api/logs`) is built over the SAME shared pool — `new_shared`,
    //     never `CallLogCollector::new(dsn)` which would open a second,
    //     independent pool on the same file and close it on shutdown,
    //     racing the formal pool. The store does not own the pool, so its
    //     shutdown is a no-op for the pool itself. Cloned here before the
    //     pool is moved into the EventWriter below.
    let collector =
        Arc::new(sag_analytics::CallLogCollector::new_shared(analytics_pool.clone()).await?);
    let event_collector = Arc::new(sag_analytics::EventCollector::new(
        sag_analytics::EventWriter::new(analytics_pool),
    ));

    Ok(AnalyticsStack {
        identity_registry,
        analytics_query,
        collector,
        event_collector,
        rollup_scheduler_handle,
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
/// 2. rollup scheduler BEFORE the event drain — the drain must be the last
///    writer to touch the analytics DB;
/// 3. event collector drain last.
async fn graceful_shutdown(
    rollup_scheduler_handle: RollupSchedulerHandle,
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

    // 14a. Stop the rollup scheduler BEFORE draining the event queue. The
    //      signal + join guarantees the scheduler's in-flight bucket
    //      compute transaction has ended (a bucket is all-or-nothing: one
    //      transaction across every rollup table) and the task has
    //      stopped, so the drain below is the last writer to touch the
    //      analytics DB and no background task can outlive the shared
    //      pool / process. A timeout or panicked task is logged as an
    //      error — never silently ignored. Any bucket left `processing`
    //      by a hard kill is reclaimed via the lease at next startup.
    rollup_scheduler_handle.shutdown().await;

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

/// Handle for the rollup scheduler background task.
struct RollupSchedulerHandle {
    task: tokio::task::JoinHandle<()>,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

impl RollupSchedulerHandle {
    /// Signal the scheduler to stop and wait up to 5s for it to exit.
    ///
    /// The join is the only way to know the scheduler's current bucket
    /// compute transaction has ended and the task has stopped — dropping
    /// or aborting the handle would leak the task or silently skip the
    /// stop, so a timeout or a panicked task is logged as an error. SQLite
    /// transactions stay atomic either way (a hard kill rolls back), but
    /// the shutdown-order guarantee (drain happens last, no background
    /// writer after it) requires the join to succeed.
    async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        match tokio::time::timeout(Duration::from_secs(5), self.task).await {
            Ok(Ok(())) => info!("rollup scheduler shut down"),
            Ok(Err(e)) => tracing::error!(error = %e, "rollup scheduler task panicked"),
            Err(_) => tracing::error!("rollup scheduler did not stop within 5s"),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use sag_analytics::BucketState;
    use sag_core::{FinalStatus, RequestResultFact};

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
}
