use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::RwLock;
use tracing::warn;

use crate::config::GatewayConfig;
use crate::loader::save::save_config_to_file;

#[cfg(test)]
use crate::config::{BackoffConfig, CircuitBreakerConfig, ServerConfig};
#[cfg(test)]
use std::collections::HashMap;

/// A shared, hot-reloadable configuration handle.
#[derive(Debug, Clone)]
pub struct ConfigHandle {
    pub(crate) inner: Arc<RwLock<GatewayConfig>>,
    pub(crate) config_path: Option<Arc<PathBuf>>,
    /// Monotonically increasing counter bumped on every routes mutation
    /// (admin CRUD or hot-reload). Used by the dispatch layer to skip
    /// stale-entry pruning when the route set hasn't changed.
    pub(crate) routes_version: Arc<AtomicU64>,
}

impl ConfigHandle {
    /// Build a `ConfigHandle` from an in-memory `GatewayConfig` (for testing / programmatic use).
    pub fn from_config(config: GatewayConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(config)),
            config_path: None,
            routes_version: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Read the current config (acquires a read lock).
    pub async fn read(&self) -> tokio::sync::RwLockReadGuard<'_, GatewayConfig> {
        self.inner.read().await
    }

    /// Acquire a write lock on the config (for runtime mutation via admin API).
    pub async fn write(&self) -> tokio::sync::RwLockWriteGuard<'_, GatewayConfig> {
        self.inner.write().await
    }

    /// Returns the path to the config file, or `None` if the config is in-memory.
    pub fn path(&self) -> Option<&Path> {
        self.config_path.as_ref().map(|p| p.as_path())
    }

    /// Returns `true` if the config was loaded from a real file (not in-memory).
    pub fn is_file_backed(&self) -> bool {
        self.config_path.is_some()
    }

    /// Persist the in-memory config back to disk if this handle is file-backed.
    ///
    /// Logs a warning on failure but never returns an error — persistence is
    /// best-effort so that an admin API mutation succeeds even if the disk
    /// write fails (the in-memory config is already updated).
    pub fn try_persist(&self, config: &GatewayConfig, action: &str) {
        if let Some(path) = self.path()
            && let Err(e) = save_config_to_file(config, path)
        {
            warn!(?e, "failed to persist config after {action}");
        }
    }

    /// Bump the routes version. Call after any mutation to the routes list
    /// (admin CRUD or hot-reload) so the dispatch layer can detect staleness.
    pub fn bump_routes_version(&self) {
        self.routes_version.fetch_add(1, Ordering::Release);
    }

    /// Return the current routes version (used by the dispatch prune guard).
    pub fn routes_version(&self) -> u64 {
        self.routes_version.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_handle_clone_shares_state() {
        let config = GatewayConfig {
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 9090,
            },
            analytics: None,
            gateway_auth: None,
            providers: HashMap::new(),
            backoff: BackoffConfig::default(),
            logging: crate::config::LoggingConfig::default(),
            routes: Vec::new(),
            shuffle_cache_ttl_secs: 7200,
            circuit_breaker: CircuitBreakerConfig::default(),
        };
        let h1 = ConfigHandle::from_config(config);
        let h2 = h1.clone();

        let c1 = h1.inner.try_read().unwrap();
        let c2 = h2.inner.try_read().unwrap();
        assert_eq!(c1.server.port, c2.server.port);
        // They point to the same Arc — same address
        let ptr1 = &*c1 as *const GatewayConfig;
        let ptr2 = &*c2 as *const GatewayConfig;
        assert_eq!(ptr1, ptr2, "Clone should share the same inner Arc");
    }
}
