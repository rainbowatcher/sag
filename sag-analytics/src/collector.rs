use sag_core::CallLog;
use tokio::sync::mpsc;
use tracing::{error, info};

/// Async collector that receives `CallLog` events via an MPSC channel
/// and persists them (placeholder for now).
pub struct CallLogCollector {
    tx: mpsc::Sender<CallLog>,
}

impl CallLogCollector {
    /// Create a new collector and spawn a background task to consume logs.
    pub fn new() -> Self {
        let (tx, mut rx) = mpsc::channel::<CallLog>(4096);

        tokio::spawn(async move {
            info!("analytics collector started");
            let mut count: u64 = 0;
            while let Some(log) = rx.recv().await {
                count += 1;
                tracing::debug!(
                    request_id = %log.request_id,
                    provider = %log.provider,
                    model = %log.model,
                    status = log.status_code,
                    duration_ms = log.duration_ms,
                    "call log received"
                );

                // TODO: Stage 4 — persist to SQLite / external store
                if count % 10 == 0 {
                    info!(count, "analytics: processed {count} call logs");
                }
            }
            info!("analytics collector shut down");
        });

        Self { tx }
    }

    /// Return a cloneable sender for other crates to push call logs.
    pub fn sender(&self) -> mpsc::Sender<CallLog> {
        self.tx.clone()
    }

    /// Send a call log asynchronously. Non-blocking — if the channel is full,
    /// the log is dropped with a warning.
    pub fn log(&self, entry: CallLog) {
        match self.tx.try_send(entry) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                error!("analytics channel full, dropping call log");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                error!("analytics channel closed, dropping call log");
            }
        }
    }
}
