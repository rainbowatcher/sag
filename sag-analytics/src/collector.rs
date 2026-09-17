use sag_core::CallLog;
use sqlx::sqlite::SqlitePool;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{error, trace, warn};

use crate::sqlite_store::SqliteLogStore;
use crate::store::LogStore;

/// Internal command sent through the collector's MPSC channel.
enum CollectorCommand {
    /// Persist a call log entry.
    Log(Box<CallLog>),
    /// Signal completion after all prior `Log` commands have been persisted.
    /// The oneshot sender is invoked by the background task once it reaches
    /// this command, guaranteeing FIFO ordering with preceding logs.
    Flush(oneshot::Sender<()>),
}

/// Async collector that receives `CallLog` events via an MPSC channel
/// and persists them to a [`LogStore`] backend.
pub struct CallLogCollector {
    tx: mpsc::Sender<CollectorCommand>,
    store: Arc<dyn LogStore>,
    handle: JoinHandle<()>,
}

impl CallLogCollector {
    /// Create a new collector backed by SQLite and spawn background persistence.
    /// `dsn` is a simple path (e.g. `analytics.db`) or `:memory:` for in-memory.
    /// The store owns its pool: [`Self::shutdown`] closes it.
    pub async fn new(dsn: &str) -> Result<Self, sqlx::Error> {
        let store = Arc::new(SqliteLogStore::new(dsn).await?);
        Self::with_store(store).await
    }

    /// Create a new collector over an existing shared pool (the production
    /// path: the same pool backing [`crate::EventWriter`], the identity
    /// registry, the rollup engine and the query engine). The store does
    /// NOT own the pool, so [`Self::shutdown`] never closes it — closing
    /// it would break every other analytics subsystem sharing the pool.
    pub async fn new_shared(pool: SqlitePool) -> Result<Self, sqlx::Error> {
        let store = Arc::new(SqliteLogStore::from_pool(pool).await?);
        Self::with_store(store).await
    }

    /// Create a new collector with an in-memory SQLite database (for testing).
    pub async fn new_memory() -> Result<Self, sqlx::Error> {
        let store = Arc::new(SqliteLogStore::new_memory().await?);
        Self::with_store(store).await
    }

    /// Build a collector around an existing [`LogStore`].
    async fn with_store(store: Arc<dyn LogStore>) -> Result<Self, sqlx::Error> {
        let (tx, mut rx) = mpsc::channel::<CollectorCommand>(4096);
        let store_clone = Arc::clone(&store);

        let handle = tokio::spawn(async move {
            trace!("analytics collector started");
            let mut count: u64 = 0;
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    CollectorCommand::Log(log) => {
                        count += 1;
                        let provider = log.provider.clone();
                        let model = log.model.clone();

                        if let Err(e) = store_clone.insert_batch(&[*log]).await {
                            error!(
                                provider = %provider,
                                model = %model,
                                error = ?e,
                                "failed to persist call log"
                            );
                        }

                        if count.is_multiple_of(10) {
                            trace!(count, "analytics: persisted {} call logs", count);
                        }
                    }
                    CollectorCommand::Flush(done) => {
                        // Signal the caller that all prior logs have been processed.
                        let _ = done.send(());
                    }
                }
            }
            trace!("analytics collector shut down");
        });

        Ok(Self { tx, store, handle })
    }

    /// Send a call log asynchronously. Non-blocking.
    pub fn log(&self, entry: CallLog) {
        match self.tx.try_send(CollectorCommand::Log(Box::new(entry))) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                error!("analytics channel full, dropping call log");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                error!("analytics channel closed, dropping call log");
            }
        }
    }

    /// Wait until all previously sent call logs have been persisted.
    /// Useful in tests to avoid races between `log()` and subsequent `query_stats()`.
    /// In production this can be used for graceful shutdown.
    pub async fn flush(&self) {
        let (done, rx) = oneshot::channel();
        if self.tx.send(CollectorCommand::Flush(done)).await.is_ok() {
            let _ = rx.await;
        }
    }

    /// Return a reference to the underlying [`LogStore`] for direct queries.
    pub fn store(&self) -> &Arc<dyn LogStore> {
        &self.store
    }

    /// Gracefully shut down the collector: drop the channel, wait for the
    /// background task to finish, and close the store.
    pub async fn shutdown(self) {
        drop(self.tx);
        // Use a timeout so we don't block forever in case the background
        // task panicked or is stuck; a timeout or panic is logged (on
        // timeout the store is still shut down below, with the task
        // possibly still running).
        match tokio::time::timeout(std::time::Duration::from_secs(5), self.handle).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => error!(error = %e, "analytics collector task panicked"),
            Err(_) => warn!("analytics collector did not stop within 5s"),
        }
        self.store.shutdown().await;
    }
}
