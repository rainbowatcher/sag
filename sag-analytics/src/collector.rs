use sag_core::CallLog;
use sqlx::sqlite::SqlitePool;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{error, trace, warn};

use crate::event_collector::DEFAULT_EVENT_QUEUE_CAPACITY;
use crate::session_registry::SessionRegistryFact;
use crate::sqlite_store::SqliteLogStore;
use crate::store::LogStore;

/// Internal command sent through the collector's MPSC channel.
enum CollectorCommand {
    /// Persist a call log entry.
    Log(Box<CallLog>),
    /// Upsert one `session_registry` row (earliest-requester-wins).
    UpsertSession(Box<SessionRegistryFact>),
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
        Self::with_store_and_capacity(store, DEFAULT_EVENT_QUEUE_CAPACITY).await
    }

    /// Build a collector around an existing [`LogStore`] with an explicit
    /// channel capacity. Tests shrink it so a saturated channel — the path
    /// that must not drop session facts — is reachable.
    async fn with_store_and_capacity(
        store: Arc<dyn LogStore>,
        capacity: usize,
    ) -> Result<Self, sqlx::Error> {
        let (tx, mut rx) = mpsc::channel::<CollectorCommand>(capacity);
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
                    CollectorCommand::UpsertSession(fact) => {
                        let session_key = fact.session_key.clone();
                        if let Err(e) = store_clone.upsert_session(&fact).await {
                            error!(
                                session_key = %session_key,
                                error = ?e,
                                "failed to persist session registry entry"
                            );
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

    /// Send a session registry fact asynchronously. Non-blocking, and it
    /// shares the [`Self::log`] channel, so a fact accepted by the channel is
    /// covered by FIFO ordering and by [`Self::flush`].
    ///
    /// The fallback path below bypasses that FIFO, so [`Self::flush`] does
    /// **not** cover it: a fact written that way may land after a later
    /// `flush` returns. Correctness there rests on the upsert's merge rule
    /// (the earliest requester wins), not on queue ordering.
    ///
    /// Unlike a call log, a fact must never be dropped: the row's opener
    /// fields and `first_seen` are decided by the *earliest* request, so
    /// losing that fact means the row keeps a later request's
    /// `first_message_preview` and an inflated `first_seen` — the next
    /// request of the same session only merges into the row and cannot repair
    /// it. A full / closed channel therefore falls back to a detached direct
    /// write through the collector's own store handle, so the hot path still
    /// never awaits. That fallback stays correct because the upsert merges
    /// with `MIN(first_seen)` / `MAX(last_seen)` and lets only a strictly
    /// smaller `first_seen` overwrite the opener fields, so the resulting row
    /// does not depend on arrival order.
    pub fn log_session(&self, fact: SessionRegistryFact) {
        match self
            .tx
            .try_send(CollectorCommand::UpsertSession(Box::new(fact)))
        {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(cmd)) => {
                Self::write_session_directly(&self.store, cmd, "full");
            }
            Err(mpsc::error::TrySendError::Closed(cmd)) => {
                Self::write_session_directly(&self.store, cmd, "closed");
            }
        }
    }

    /// Fallback for [`Self::log_session`] when the queue is unavailable:
    /// spawn a detached task that writes the fact straight to the store. Only
    /// a missing runtime (process shutting down) loses the fact.
    fn write_session_directly(
        store: &Arc<dyn LogStore>,
        cmd: CollectorCommand,
        channel_state: &'static str,
    ) {
        let CollectorCommand::UpsertSession(fact) = cmd else {
            // `log_session` only ever sends `UpsertSession`.
            return;
        };
        let session_key = fact.session_key.clone();
        let store = Arc::clone(store);

        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            error!(
                session_key = %session_key,
                channel_state,
                "analytics runtime unavailable, dropping session registry entry"
            );
            return;
        };

        warn!(
            session_key = %session_key,
            channel_state,
            "analytics channel unavailable, persisting session registry entry directly"
        );

        runtime.spawn(async move {
            if let Err(e) = store.upsert_session(&fact).await {
                error!(
                    session_key = %session_key,
                    error = ?e,
                    "failed to persist session registry entry (direct write)"
                );
            }
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{LogFilter, SessionRecord, StatsFilter, StatsRow};
    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ts(ms: i64) -> chrono::DateTime<Utc> {
        Utc.timestamp_millis_opt(ms).unwrap()
    }

    fn fact(key: &str, preview: &str, at_ms: i64) -> SessionRegistryFact {
        SessionRegistryFact {
            session_key: key.to_string(),
            session_source: "meta".to_string(),
            conversation_id: Some("conv-1".to_string()),
            first_message_preview: Some(preview.to_string()),
            first_seen: ts(at_ms),
            last_seen: ts(at_ms),
        }
    }

    /// [`LogStore`] wrapper that parks every `upsert_session` on a semaphore
    /// (zero permits initially) so a test can hold the collector worker
    /// inside the store, saturate the channel deterministically, and then
    /// release writes one at a time. All other methods delegate unchanged.
    struct GatedStore {
        inner: SqliteLogStore,
        gate: tokio::sync::Semaphore,
        entered: AtomicUsize,
        completed: AtomicUsize,
    }

    impl GatedStore {
        async fn new_memory() -> Self {
            Self {
                inner: SqliteLogStore::new_memory().await.unwrap(),
                gate: tokio::sync::Semaphore::new(0),
                entered: AtomicUsize::new(0),
                completed: AtomicUsize::new(0),
            }
        }

        async fn wait_entered(&self, n: usize) {
            wait_for(&self.entered, n).await;
        }

        async fn wait_completed(&self, n: usize) {
            wait_for(&self.completed, n).await;
        }

        /// Grant exactly one permit: tokio's semaphore is fair, so the
        /// longest-waiting write proceeds next.
        fn release_one(&self) {
            self.gate.add_permits(1);
        }

        fn release_all(&self) {
            self.gate.add_permits(64);
        }
    }

    async fn wait_for(counter: &AtomicUsize, target: usize) {
        for _ in 0..10_000 {
            if counter.load(Ordering::SeqCst) >= target {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("timed out waiting for the store counter to reach {target}");
    }

    #[async_trait]
    impl LogStore for GatedStore {
        async fn insert_batch(&self, logs: &[CallLog]) -> sag_core::Result<()> {
            self.inner.insert_batch(logs).await
        }

        async fn query_stats(&self, filter: StatsFilter) -> sag_core::Result<Vec<StatsRow>> {
            self.inner.query_stats(filter).await
        }

        async fn query_logs(&self, filter: LogFilter) -> sag_core::Result<Vec<CallLog>> {
            self.inner.query_logs(filter).await
        }

        async fn count_logs(&self, filter: LogFilter) -> sag_core::Result<u64> {
            self.inner.count_logs(filter).await
        }

        async fn upsert_session(&self, fact: &SessionRegistryFact) -> sag_core::Result<()> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.gate
                .acquire()
                .await
                .expect("gated store semaphore is never closed")
                .forget();
            let res = self.inner.upsert_session(fact).await;
            self.completed.fetch_add(1, Ordering::SeqCst);
            res
        }

        async fn get_session(&self, key: &str) -> sag_core::Result<Option<SessionRecord>> {
            self.inner.get_session(key).await
        }

        async fn shutdown(&self) {
            self.inner.shutdown().await
        }
    }

    /// The session registry write travels the same channel as call logs, so
    /// `flush()` covers it: after `log_session` + `flush`, the row is
    /// readable through the store and a second fact only advances
    /// `last_seen`.
    #[tokio::test]
    async fn log_session_is_flushed_and_upserts_first_write_wins() {
        let collector = CallLogCollector::new_memory().await.unwrap();
        let ts = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let first = SessionRegistryFact {
            session_key: "key-1".to_string(),
            session_source: "meta".to_string(),
            conversation_id: Some("conv-1".to_string()),
            first_message_preview: Some("opener".to_string()),
            first_seen: ts,
            last_seen: ts,
        };
        collector.log_session(first);
        collector.flush().await;

        let rec = collector
            .store()
            .get_session("key-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec.session_source, "meta");
        assert_eq!(rec.conversation_id.as_deref(), Some("conv-1"));
        assert_eq!(rec.first_message_preview.as_deref(), Some("opener"));
        assert_eq!(rec.first_seen, ts);

        let later = ts + chrono::Duration::seconds(30);
        let second = SessionRegistryFact {
            session_key: "key-1".to_string(),
            session_source: "top".to_string(),
            conversation_id: None,
            first_message_preview: None,
            first_seen: later,
            last_seen: later,
        };
        collector.log_session(second);
        collector.flush().await;

        let rec = collector
            .store()
            .get_session("key-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec.last_seen, later);
        assert_eq!(rec.first_seen, ts);
        assert_eq!(rec.session_source, "meta");
        assert_eq!(rec.conversation_id.as_deref(), Some("conv-1"));

        assert!(
            collector
                .store()
                .get_session("unknown")
                .await
                .unwrap()
                .is_none(),
            "unknown session keys report a miss"
        );
        collector.shutdown().await;
    }

    /// A saturated queue must not swallow the session opener: `log_session`
    /// falls back to a direct store write, so the first fact still owns
    /// `first_message_preview` / `first_seen` and the row can never freeze a
    /// mid-conversation message instead.
    #[tokio::test]
    async fn full_channel_still_persists_the_session_opener() {
        let gated = Arc::new(GatedStore::new_memory().await);
        let store: Arc<dyn LogStore> = gated.clone();
        // Capacity 1: one queued command already saturates the channel.
        let collector = CallLogCollector::with_store_and_capacity(store, 1)
            .await
            .unwrap();

        // 1. Park the worker inside the store, so nothing drains the channel.
        collector.log_session(fact("filler-key", "filler", 1_700_000_000_000));
        gated.wait_entered(1).await;

        // 2. Occupy the single slot.
        collector.log_session(fact("key-1", "queued", 1_700_000_000_005));

        // 3. The opener no longer fits -> `Full` -> detached direct write.
        collector.log_session(fact("key-1", "opener", 1_700_000_000_000));
        gated.wait_entered(2).await;

        // 4. Release the filler, then the fallback write. The semaphore is
        //    fair, so the opener is committed before the queued fact.
        gated.release_one();
        gated.wait_completed(1).await;
        gated.release_one();
        gated.wait_completed(2).await;

        gated.release_all();
        collector.flush().await;

        let rec = collector
            .store()
            .get_session("key-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            rec.first_message_preview.as_deref(),
            Some("opener"),
            "the opener must win even when the channel was full"
        );
        assert_eq!(rec.first_seen, ts(1_700_000_000_000));
        assert_eq!(
            rec.last_seen,
            ts(1_700_000_000_005),
            "the queued fact is still applied afterwards"
        );
        collector.shutdown().await;
    }
}
