//! Bounded, backpressured event queue with a background transactional writer.
//!
//! Formal analytics events must never be silently dropped: the fast path
//! [`EventCollector::try_emit`] refuses when the queue is full (the caller
//! decides), and [`EventCollector::emit`] awaits — backpressure — instead of
//! discarding. Graceful [`EventCollector::shutdown`] — safe on any shared
//! handle (`&self`), idempotent — closes the sender first, then drains
//! pending events before exiting, waiting as long as the writer needs (no
//! hard timeout). A write failure is retried with backoff and, if it
//! it still fails, reported at error level before the event is dropped — a
//! successful write is never silently marked as failed, and a failed write
//! is never silently marked as done.
//!
//! The legacy [`crate::CallLogCollector`] keeps its droppable diagnostic
//! semantics; this collector is the formal-facts path only.

use crate::event::AnalyticsEvent;
use crate::event_writer::EventWriter;
use async_trait::async_trait;
use std::fmt;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, trace, warn};

/// Default bounded queue capacity for analytics events (matches the spec).
pub const DEFAULT_EVENT_QUEUE_CAPACITY: usize = 4096;

/// Retry policy applied by the background writer when persisting an event
/// fails. Attempt *n* sleeps `base_delay * 2^(n-1)` before retrying.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total write attempts per event (1 = no retry).
    pub max_attempts: u32,
    /// Base backoff delay.
    pub base_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_millis(100),
        }
    }
}

/// Failure modes of enqueueing an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventQueueError {
    /// The bounded queue is full; use [`EventCollector::emit`] to await
    /// backpressure instead of dropping.
    Full,
    /// The collector is shut down; the event was not enqueued.
    Closed,
}

impl fmt::Display for EventQueueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EventQueueError::Full => write!(f, "analytics event queue full"),
            EventQueueError::Closed => write!(f, "analytics event collector shut down"),
        }
    }
}

impl std::error::Error for EventQueueError {}

/// Internal sink abstraction so tests can inject blocking / failing writers.
#[async_trait]
trait EventSink: Send + Sync {
    async fn write(&self, event: &AnalyticsEvent) -> sag_core::Result<()>;
}

#[async_trait]
impl EventSink for EventWriter {
    async fn write(&self, event: &AnalyticsEvent) -> sag_core::Result<()> {
        EventWriter::write(self, event.clone()).await
    }
}

/// Bounded queue + background writer for [`AnalyticsEvent`]s.
///
/// All state lives behind interior mutability so the drain can be
/// triggered from any shared handle: [`Self::shutdown`] takes `&self`,
/// closes the sender, and awaits the writer exactly once — it never
/// depends on this collector being the last strong reference.
pub struct EventCollector {
    /// Sender slot; `None` once [`Self::shutdown`] has closed it.
    tx: std::sync::Mutex<Option<mpsc::Sender<AnalyticsEvent>>>,
    /// Writer join handle; taken exactly once by [`Self::shutdown`].
    handle: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl EventCollector {
    /// Create a collector with the default queue capacity and retry policy.
    pub fn new(writer: EventWriter) -> Self {
        Self::with_capacity_and_policy(writer, DEFAULT_EVENT_QUEUE_CAPACITY, RetryPolicy::default())
    }

    /// Create a collector with a custom queue capacity.
    pub fn with_capacity(writer: EventWriter, capacity: usize) -> Self {
        Self::with_capacity_and_policy(writer, capacity, RetryPolicy::default())
    }

    /// Create a collector with a custom queue capacity and retry policy.
    pub fn with_capacity_and_policy(
        writer: EventWriter,
        capacity: usize,
        policy: RetryPolicy,
    ) -> Self {
        Self::with_sink(Box::new(writer), capacity, policy)
    }

    fn with_sink(sink: Box<dyn EventSink>, capacity: usize, policy: RetryPolicy) -> Self {
        let (tx, rx) = mpsc::channel::<AnalyticsEvent>(capacity);
        let handle = tokio::spawn(writer_loop(rx, sink, policy));
        Self {
            tx: std::sync::Mutex::new(Some(tx)),
            handle: tokio::sync::Mutex::new(Some(handle)),
        }
    }

    /// Fast path: enqueue without blocking. Returns
    /// [`EventQueueError::Full`] when the queue is full — callers that must
    /// not drop the event should fall back to [`Self::emit`] (backpressure).
    pub fn try_emit(&self, event: AnalyticsEvent) -> std::result::Result<(), EventQueueError> {
        let guard = self.tx.lock().expect("analytics tx mutex poisoned");
        match guard.as_ref() {
            Some(tx) => tx.try_send(event).map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => EventQueueError::Full,
                mpsc::error::TrySendError::Closed(_) => EventQueueError::Closed,
            }),
            None => Err(EventQueueError::Closed),
        }
    }

    /// Enqueue with backpressure: awaits while the queue is full, never
    /// silently drops. Only fails when the collector is already shut down.
    pub async fn emit(&self, event: AnalyticsEvent) -> std::result::Result<(), EventQueueError> {
        // Clone the sender out of the slot so the lock is not held across
        // the await; the channel itself enforces backpressure unchanged.
        let sender = self.tx.lock().expect("analytics tx mutex poisoned").clone();
        match sender {
            Some(tx) => tx.send(event).await.map_err(|_| EventQueueError::Closed),
            None => Err(EventQueueError::Closed),
        }
    }

    /// Graceful shutdown, safe on any shared handle (`&self`) and
    /// idempotent: closes the sender first (no new events accepted), then
    /// waits as long as it takes for the writer to drain all pending
    /// events and exit — no hard timeout, so pending events are never
    /// silently abandoned. A concurrent second call awaits the same drain
    /// instead of double-consuming the writer task. A small window of
    /// uncommitted events is acceptable on abnormal crash only; normal
    /// shutdown always drains. If the writer task panicked, the failure
    /// is logged at error level instead of being silently ignored.
    pub async fn shutdown(&self) {
        // Close the sender. In-flight `emit` callers that already cloned
        // the sender may still enqueue until the channel is truly closed;
        // the writer drains everything before exiting.
        self.tx.lock().expect("analytics tx mutex poisoned").take();
        // Take the writer handle exactly once. The guard is held across
        // the await, so a concurrent second shutdown waits for the same
        // drain; the writer loop never acquires this lock, so this
        // cannot deadlock.
        let mut guard = self.handle.lock().await;
        let Some(handle) = guard.take() else {
            return; // already shut down; the writer has exited or is being awaited
        };
        if let Err(e) = handle.await {
            error!(
                error = ?e,
                "analytics event writer task panicked during shutdown (pending events not drained)"
            );
        }
    }
}

/// Background writer loop: process one event at a time, retrying failures
/// per policy, until the sender is dropped and the queue is drained.
async fn writer_loop(
    mut rx: mpsc::Receiver<AnalyticsEvent>,
    sink: Box<dyn EventSink>,
    policy: RetryPolicy,
) {
    trace!("analytics event writer started");
    while let Some(event) = rx.recv().await {
        write_with_retry(&*sink, event, policy).await;
    }
    trace!("analytics event writer drained and exited");
}

async fn write_with_retry(sink: &dyn EventSink, event: AnalyticsEvent, policy: RetryPolicy) {
    let request_id = event.result.request_id;
    let mut attempt: u32 = 1;
    loop {
        match sink.write(&event).await {
            Ok(()) => return,
            Err(e) if attempt < policy.max_attempts => {
                warn!(
                    request_id = %request_id,
                    attempt,
                    max_attempts = policy.max_attempts,
                    error = ?e,
                    "analytics event write failed; retrying"
                );
                let delay = policy
                    .base_delay
                    .saturating_mul(1u32 << (attempt - 1).min(30));
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(e) => {
                error!(
                    request_id = %request_id,
                    attempt,
                    max_attempts = policy.max_attempts,
                    error = ?e,
                    "analytics event dropped after write retries (not marked as persisted)"
                );
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::test_support::{make_attempt, make_event, test_pool};
    use sag_core::{FinalStatus, Result, TargetStatus};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::oneshot;
    use uuid::Uuid;

    fn success_event() -> AnalyticsEvent {
        make_event(
            Uuid::new_v4(),
            FinalStatus::Success,
            Some("chat"),
            vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
        )
    }

    /// Sink that blocks its first write until released by the test, then
    /// records every written request id.
    struct BlockingSink {
        /// Signals (once) that the first write is in flight.
        started: Mutex<Option<oneshot::Sender<()>>>,
        /// Blocks the first write until the test sends on the other end.
        release: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
        written: Arc<Mutex<Vec<Uuid>>>,
    }

    #[async_trait]
    impl EventSink for BlockingSink {
        async fn write(&self, event: &AnalyticsEvent) -> Result<()> {
            if let Some(sender) = self.started.lock().unwrap().take() {
                let _ = sender.send(());
            }
            let mut guard = self.release.lock().await;
            if let Some(rx) = guard.take() {
                drop(guard);
                let _ = rx.await; // first write blocks until released
            }
            self.written.lock().unwrap().push(event.result.request_id);
            Ok(())
        }
    }

    #[tokio::test]
    async fn queue_full_backpressures_instead_of_dropping() {
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let written: Arc<Mutex<Vec<Uuid>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = BlockingSink {
            started: Mutex::new(Some(started_tx)),
            release: tokio::sync::Mutex::new(Some(release_rx)),
            written: Arc::clone(&written),
        };

        let collector = EventCollector::with_sink(Box::new(sink), 2, RetryPolicy::default());

        // First event: the writer picks it up and blocks on the sink.
        collector
            .try_emit(make_event(
                Uuid::new_v4(),
                FinalStatus::Success,
                Some("chat"),
                vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
            ))
            .unwrap();
        started_rx.await.unwrap(); // writer is now stuck in write()

        // Fill the queue (capacity 2): the writer cannot consume these.
        for _ in 0..2 {
            collector.try_emit(success_event()).unwrap();
        }

        // Queue is full now: fast path must refuse, not drop.
        assert_eq!(
            collector.try_emit(success_event()),
            Err(EventQueueError::Full)
        );

        // emit() must await (backpressure), not discard.
        let pending = collector.emit(success_event());
        let outcome = tokio::time::timeout(Duration::from_millis(50), pending).await;
        assert!(
            outcome.is_err(),
            "emit() must wait on a full queue instead of dropping"
        );

        // Release the blocked writer: capacity frees up, everything drains.
        let _ = release_tx.send(());
        let remaining = collector.emit(success_event());
        let outcome = tokio::time::timeout(Duration::from_secs(5), remaining).await;
        assert!(outcome.is_ok(), "emit() completes once capacity frees up");

        collector.shutdown().await;
        // Written: the first event (blocked then released), the two queued
        // events, and the final emit — 4 total. The try_emit that returned
        // Full was never accepted (caller-visible refusal, not a drop), and
        // the timed-out emit future was abandoned by the test itself.
        assert_eq!(
            written.lock().unwrap().len(),
            4,
            "no accepted event may be lost"
        );
    }

    #[tokio::test]
    async fn shutdown_drains_pending_events() {
        let pool = test_pool().await;
        let writer = EventWriter::new(pool.clone());
        let collector = EventCollector::new(writer);

        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        collector
            .try_emit(make_event(
                id1,
                FinalStatus::Success,
                Some("chat"),
                vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
            ))
            .unwrap();
        collector
            .try_emit(make_event(
                id2,
                FinalStatus::ClientCancelled,
                Some("chat"),
                vec![],
            ))
            .unwrap();

        collector.shutdown().await;

        let (requests, attempts): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM request_results),
                    (SELECT COUNT(*) FROM target_attempts)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(requests, 2, "shutdown must drain pending events");
        assert_eq!(attempts, 1);
    }

    #[tokio::test]
    async fn shutdown_waits_for_blocked_writer_until_drained() {
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let written: Arc<Mutex<Vec<Uuid>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = BlockingSink {
            started: Mutex::new(Some(started_tx)),
            release: tokio::sync::Mutex::new(Some(release_rx)),
            written: Arc::clone(&written),
        };
        let collector = EventCollector::with_sink(Box::new(sink), 4, RetryPolicy::default());

        let first_id = Uuid::new_v4();
        collector
            .try_emit(make_event(
                first_id,
                FinalStatus::Success,
                Some("chat"),
                vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
            ))
            .unwrap();
        started_rx.await.unwrap(); // writer is stuck mid-write on the sink

        let second_id = Uuid::new_v4();
        collector
            .try_emit(make_event(
                second_id,
                FinalStatus::Success,
                Some("chat"),
                vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
            ))
            .unwrap();

        // shutdown() must keep waiting for the blocked writer instead of
        // giving up (no hard timeout): still pending while the sink blocks.
        let shutdown_fut = collector.shutdown();
        tokio::pin!(shutdown_fut);
        let outcome = tokio::time::timeout(Duration::from_millis(200), &mut shutdown_fut).await;
        assert!(
            outcome.is_err(),
            "shutdown must not abandon events while the writer is blocked"
        );

        // Once the writer is released, shutdown completes after the queue
        // has fully drained — no event lost.
        let _ = release_tx.send(());
        shutdown_fut.await;
        assert_eq!(
            written.lock().unwrap().len(),
            2,
            "all accepted events must be written before shutdown returns"
        );
    }

    #[tokio::test]
    async fn shutdown_drains_even_with_extra_shared_handles() {
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let written: Arc<Mutex<Vec<Uuid>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = BlockingSink {
            started: Mutex::new(Some(started_tx)),
            release: tokio::sync::Mutex::new(Some(release_rx)),
            written: Arc::clone(&written),
        };
        let collector = Arc::new(EventCollector::with_sink(
            Box::new(sink),
            4,
            RetryPolicy::default(),
        ));
        // Extra shared handle — the analogue of `AppState` holding a clone.
        let app_state_clone = Arc::clone(&collector);

        let first_id = Uuid::new_v4();
        collector
            .try_emit(make_event(
                first_id,
                FinalStatus::Success,
                Some("chat"),
                vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
            ))
            .unwrap();
        started_rx.await.unwrap(); // writer is stuck mid-write on the sink

        let second_id = Uuid::new_v4();
        collector
            .try_emit(make_event(
                second_id,
                FinalStatus::Success,
                Some("chat"),
                vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
            ))
            .unwrap();

        // shutdown must still wait for the blocked writer even though an
        // extra Arc clone is alive: the drain may not depend on the
        // collector being uniquely referenced.
        let shutdown_fut = collector.shutdown();
        tokio::pin!(shutdown_fut);
        let outcome = tokio::time::timeout(Duration::from_millis(200), &mut shutdown_fut).await;
        assert!(
            outcome.is_err(),
            "shutdown must not skip the drain while a clone is still alive"
        );

        // Once the writer is released, shutdown completes after the queue
        // has fully drained — the surviving clone never blocks it.
        let _ = release_tx.send(());
        shutdown_fut.await;
        assert_eq!(
            written.lock().unwrap().len(),
            2,
            "all accepted events must be written even with extra clones alive"
        );

        // Both handles observe the closed state afterwards.
        assert_eq!(
            collector.try_emit(success_event()),
            Err(EventQueueError::Closed)
        );
        assert_eq!(
            app_state_clone.try_emit(success_event()),
            Err(EventQueueError::Closed)
        );
    }

    #[tokio::test]
    async fn repeated_shutdown_is_idempotent_and_never_deadlocks() {
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let written: Arc<Mutex<Vec<Uuid>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = BlockingSink {
            started: Mutex::new(Some(started_tx)),
            release: tokio::sync::Mutex::new(Some(release_rx)),
            written: Arc::clone(&written),
        };
        let collector = Arc::new(EventCollector::with_sink(
            Box::new(sink),
            4,
            RetryPolicy::default(),
        ));

        collector
            .try_emit(make_event(
                Uuid::new_v4(),
                FinalStatus::Success,
                Some("chat"),
                vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
            ))
            .unwrap();
        started_rx.await.unwrap(); // writer is stuck mid-write on the sink

        // Two concurrent shutdowns through distinct shared handles:
        // neither may deadlock, and both must wait for the same drain.
        let first = tokio::spawn({
            let collector = Arc::clone(&collector);
            async move { collector.shutdown().await }
        });
        let second = tokio::spawn({
            let collector = Arc::clone(&collector);
            async move { collector.shutdown().await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !first.is_finished() && !second.is_finished(),
            "concurrent shutdowns must wait for the blocked writer, not deadlock or bail"
        );

        let _ = release_tx.send(());
        tokio::time::timeout(Duration::from_secs(5), async {
            first.await.unwrap();
            second.await.unwrap();
        })
        .await
        .unwrap();

        // The event was consumed exactly once despite two shutdown calls.
        assert_eq!(
            written.lock().unwrap().len(),
            1,
            "repeated shutdown must not double-consume or lose events"
        );

        // A further shutdown returns immediately (writer already exited).
        tokio::time::timeout(Duration::from_secs(1), collector.shutdown())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn write_failure_is_retried_then_reported_not_faked() {
        struct FailingSink {
            calls: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl EventSink for FailingSink {
            async fn write(&self, _event: &AnalyticsEvent) -> Result<()> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Err(sag_core::AppError::Internal("boom".to_string()))
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let sink = FailingSink {
            calls: Arc::clone(&calls),
        };
        let policy = RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
        };
        let collector = EventCollector::with_sink(Box::new(sink), 4, policy);

        collector.emit(success_event()).await.unwrap();
        // Retries are asynchronous; poll until the sink has been attempted
        // the full number of times (or the test would hang).
        for _ in 0..100 {
            if calls.load(Ordering::SeqCst) >= policy.max_attempts as usize {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            policy.max_attempts as usize,
            "a failing write must be retried, not silently dropped"
        );

        // Shutdown still completes (retries are bounded; the event is
        // reported at error level, never marked as persisted).
        collector.shutdown().await;
    }
}
