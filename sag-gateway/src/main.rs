//! SAG — Smart AI Gateway
//!
//! An AI gateway that proxies chat completion requests to multiple LLM providers
//! with priority-based routing, automatic fallback, and usage analytics.

use std::net::SocketAddr;

use tracing::{error, info};

mod tracing_init;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Initialize tracing (structured logging)
    tracing_init::init();

    // 2. Parse CLI args for config path
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or("gateway.toml".to_string());
    info!(%config_path, "starting SAG gateway");

    // 3. Load configuration with hot-reload support
    let config_handle = sag_config::load_config(&config_path)?;

    // 4. Start config hot-reload watcher (store handle to detect panics)
    let hot_reload_task = sag_config::spawn_hot_reload(config_handle.clone());

    // 5. Initialize analytics collector
    let _collector = sag_analytics::CallLogCollector::new();

    // 6. Build router
    let app = sag_router::build_router(config_handle.clone());

    // 7. Determine listen address
    let config = config_handle.read().await;
    let addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
        .parse()
        .expect("invalid server address");

    info!(%addr, "gateway listening");

    // Drop the read lock so hot-reload can acquire it
    drop(config);

    // 8. Start server with graceful shutdown
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("gateway shut down");

    // 9. Check hot-reload task for panics
    match hot_reload_task.await {
        Ok(()) => info!("hot-reload watcher exited cleanly"),
        Err(e) => error!("hot-reload watcher panicked: {e}"),
    }

    Ok(())
}

/// Waits for SIGTERM or SIGINT (Ctrl+C).
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    let terminate = {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");
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
