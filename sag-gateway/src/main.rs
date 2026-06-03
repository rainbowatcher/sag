//! SAG — Smart AI Gateway
//!
//! An AI gateway that proxies chat completion requests to multiple LLM providers
//! with priority-based routing, automatic fallback, and usage analytics.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::RwLock;
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

    // 4. Start config hot-reload watcher
    let hot_reload_task = sag_config::spawn_hot_reload(config_handle.clone());

    // 5. Determine analytics storage DSN from config
    let config = config_handle.read().await;
    let analytics_dsn = config
        .analytics
        .as_ref()
        .map(|a| a.storage_dsn.clone())
        .unwrap_or_else(|| "sqlite://analytics.db".into());
    drop(config);

    // 6. Initialize analytics collector with SQLite persistence
    let collector = Arc::new(
        sag_analytics::CallLogCollector::new(&analytics_dsn).await?,
    );

    // 7. Initialize provider registry from config
    let config = config_handle.read().await;
    let mut providers: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();

    for (name, provider_config) in &config.providers {
        let provider: Arc<dyn sag_providers::LlmProvider> = match provider_config.provider_type.as_str() {
            "anthropic" => Arc::new(sag_providers::AnthropicProvider::new(name.clone(), provider_config)),
            _ => Arc::new(sag_providers::OpenAiProvider::new(name.clone(), provider_config)),
        };
        providers.insert(name.clone(), provider);
        info!(name, provider_type = %provider_config.provider_type, base_url = %provider_config.base_url, "provider registered");
    }
    drop(config);

    let provider_registry = Arc::new(RwLock::new(providers));

    // 8. Build AppState
    let app_state = sag_router::AppState {
        config: config_handle.clone(),
        providers: provider_registry,
        collector,
    };

    // 9. Build router
    let app = sag_router::build_router(app_state);

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

    // 12. Check hot-reload task for panics
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
