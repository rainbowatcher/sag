//! SAG — Smart AI Gateway
//!
//! An AI gateway that proxies chat completion requests to multiple LLM providers
//! with priority-based routing, automatic fallback, and usage analytics.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::info;

mod tracing_init;

#[cfg(feature = "embed-ui")]
mod ui_routes;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Initialize tracing (structured logging)
    tracing_init::init();

    // 2. Resolve config path: --config <path> > positional <path> > "gateway.toml"
    let config_path = resolve_config_path()?;
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

    // 7. Initialize provider registry from config
    let config = config_handle.read().await;
    let mut providers: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();

    for (name, provider_config) in &config.providers {
        let provider = sag_providers::create_provider(name.clone(), provider_config);
        info!(name, provider_type = %provider_config.provider_type.as_str(), base_url = %provider_config.base_url, "provider registered");
        providers.insert(name.clone(), provider);
    }
    drop(config);

    let provider_registry = Arc::new(RwLock::new(providers));

    // 8. Build AppState
    let app_state = sag_router::AppState {
        config: config_handle.clone(),
        providers: provider_registry,
        collector,
        route_counters: Arc::new(RwLock::new(HashMap::new())),
        route_circuit_breaker: Arc::new(RwLock::new(Default::default())),
    };

    // 9. Build router
    let app = sag_router::build_router(app_state);

    // 9a. When the `embed-ui` feature is on, route any unmatched path
    //     through the embedded SPA (assets + index.html fallback).
    #[cfg(feature = "embed-ui")]
    let app = app.fallback(crate::ui_routes::ui_handler);

    // 10. Determine listen address
    let config = config_handle.read().await;
    let addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
        .parse()
        .expect("invalid server address");

    info!(%addr, "gateway listening");
    drop(config);

    // 11. Start server with graceful shutdown
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("gateway shut down");

    // 12. Gracefully shut down the hot-reload watcher.
    //     This MUST happen after the server stops because the watcher
    //     uses `notify` which spawns OS-level threads (FSEvents on macOS).
    //     Using `abort()` on the JoinHandle would skip the Watcher's Drop
    //     implementation, leaving those threads alive and blocking process exit.
    //
    //     The router (and its internal AppState) was dropped when axum::serve
    //     completed above, which already closes the analytics channel and lets
    //     the SQLite background task exit.
    hot_reload_handle.shutdown().await;

    info!("all subsystems shut down");

    Ok(())
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
