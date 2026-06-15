use sag_core::CallLog;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info};

/// Internal command sent through the collector's MPSC channel.
enum CollectorCommand {
    /// Persist a call log entry.
    Log(CallLog),
    /// Signal completion after all prior `Log` commands have been persisted.
    /// The oneshot sender is invoked by the background task once it reaches
    /// this command, guaranteeing FIFO ordering with preceding logs.
    Flush(oneshot::Sender<()>),
}

/// Async collector that receives `CallLog` events via an MPSC channel
/// and persists them to SQLite.
pub struct CallLogCollector {
    tx: mpsc::Sender<CollectorCommand>,
    pool: SqlitePool,
}

impl CallLogCollector {
    /// Create a new collector with a SQLite pool and spawn background persistence.
    /// `dsn` is a simple path (e.g. `analytics.db`) or `:memory:` for in-memory.
    pub async fn new(dsn: &str) -> Result<Self, sqlx::Error> {
        let options = if dsn.starts_with("sqlite:") || dsn == ":memory:" {
            SqliteConnectOptions::from_str(dsn)?
        } else {
            SqliteConnectOptions::new()
                .filename(dsn)
                .create_if_missing(true)
        };

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;

        // Auto-create schema
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS call_logs (
                id            INTEGER PRIMARY KEY AUTOINCREMENT,
                request_id    TEXT NOT NULL,
                provider      TEXT NOT NULL DEFAULT '',
                model         TEXT NOT NULL DEFAULT '',
                real_model    TEXT NOT NULL DEFAULT '',
                stream        INTEGER NOT NULL DEFAULT 0,
                status_code   INTEGER NOT NULL DEFAULT 0,
                duration_ms   INTEGER NOT NULL DEFAULT 0,
                prompt_tokens     INTEGER,
                completion_tokens INTEGER,
                total_tokens      INTEGER,
                error         TEXT,
                timestamp     TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_call_logs_provider ON call_logs(provider);
            CREATE INDEX IF NOT EXISTS idx_call_logs_model    ON call_logs(model);
            "#,
        )
        .execute(&pool)
        .await?;

        let (tx, mut rx) = mpsc::channel::<CollectorCommand>(4096);
        let pool_clone = pool.clone();

        tokio::spawn(async move {
            info!("analytics collector started (sqlite)");
            let mut count: u64 = 0;
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    CollectorCommand::Log(log) => {
                        count += 1;

                        let (prompt, completion, total) = match &log.usage {
                            Some(u) => (
                                Some(u.prompt_tokens as i64),
                                Some(u.completion_tokens as i64),
                                Some(u.total_tokens as i64),
                            ),
                            None => (None, None, None),
                        };

                        let result = sqlx::query(
                            r#"
                            INSERT INTO call_logs
                                (request_id, provider, model, real_model, stream,
                                 status_code, duration_ms,
                                 prompt_tokens, completion_tokens, total_tokens,
                                 error, timestamp)
                            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                            "#,
                        )
                        .bind(log.request_id.to_string())
                        .bind(&log.provider)
                        .bind(&log.model)
                        .bind(&log.real_model)
                        .bind(log.stream as i32)
                        .bind(log.status_code as i32)
                        .bind(log.duration_ms as i64)
                        .bind(prompt)
                        .bind(completion)
                        .bind(total)
                        .bind(&log.error)
                        .bind(log.timestamp.to_rfc3339())
                        .execute(&pool_clone)
                        .await;

                        if let Err(e) = result {
                            error!(
                                request_id = %log.request_id,
                                provider = %log.provider,
                                model = %log.model,
                                error = ?e,
                                "failed to persist call log"
                            );
                        }

                        if count.is_multiple_of(10) {
                            info!(count, "analytics: persisted {count} call logs");
                        }
                    }
                    CollectorCommand::Flush(done) => {
                        // Signal the caller that all prior logs have been processed.
                        // Ignore send errors (caller dropped the receiver).
                        let _ = done.send(());
                    }
                }
            }
            info!("analytics collector shut down");
        });

        Ok(Self { tx, pool })
    }

    /// Create a new collector with an in-memory SQLite database (for testing / development).
    pub async fn new_memory() -> Result<Self, sqlx::Error> {
        Self::new(":memory:").await
    }

    /// Return a cloneable sender for other crates to push call logs.
    pub fn sender(&self) -> mpsc::Sender<CollectorCommand> {
        self.tx.clone()
    }

    /// Send a call log asynchronously. Non-blocking.
    pub fn log(&self, entry: CallLog) {
        match self.tx.try_send(CollectorCommand::Log(entry)) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                error!("analytics channel full, dropping call log");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                error!("analytics channel closed, dropping call log");
            }
        }
    }

    /// Wait until all previously sent call logs have been persisted to SQLite.
    /// Useful in tests to avoid races between `log()` and subsequent `query_stats()`.
    /// In production this can be used for graceful shutdown.
    pub async fn flush(&self) {
        let (done, rx) = oneshot::channel();
        // Use `send().await` (not `try_send`) so that a full channel blocks
        // until the background task drains it. This guarantees FIFO ordering
        // with all prior `Log` commands before `Flush` is delivered.
        if self.tx.send(CollectorCommand::Flush(done)).await.is_ok() {
            // Ignore errors: the background task may have shut down.
            let _ = rx.await;
        }
    }

    /// Return a reference to the SQLite pool for direct queries.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

/// Aggregated statistics row returned by query_stats.
#[derive(Debug, serde::Serialize)]
pub struct StatsRow {
    pub provider: String,
    pub model: String,
    pub call_count: i64,
    pub total_prompt_tokens: i64,
    pub total_completion_tokens: i64,
    pub total_tokens: i64,
    pub avg_duration_ms: f64,
    pub error_count: i64,
}

/// Query aggregated statistics, optionally filtered by provider and/or model.
pub async fn query_stats(
    pool: &SqlitePool,
    provider_filter: Option<&str>,
    model_filter: Option<&str>,
) -> Result<Vec<StatsRow>, sqlx::Error> {
    // Build the query dynamically
    let mut conditions = Vec::new();
    if let Some(p) = provider_filter
        && !p.is_empty()
    {
        conditions.push(format!("provider = '{}'", p.replace('\'', "''")));
    }
    if let Some(m) = model_filter
        && !m.is_empty()
    {
        conditions.push(format!("model = '{}'", m.replace('\'', "''")));
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    let sql = format!(
        r#"
        SELECT
            provider,
            model,
            COUNT(*)                                   AS call_count,
            COALESCE(SUM(prompt_tokens), 0)            AS total_prompt_tokens,
            COALESCE(SUM(completion_tokens), 0)        AS total_completion_tokens,
            COALESCE(SUM(total_tokens), 0)             AS total_tokens,
            ROUND(AVG(duration_ms), 1)                 AS avg_duration_ms,
            SUM(CASE WHEN error IS NOT NULL THEN 1 ELSE 0 END) AS error_count
        FROM call_logs
        {where_clause}
        GROUP BY provider, model
        ORDER BY call_count DESC
        "#
    );

    let rows = sqlx::query_as::<_, (String, String, i64, i64, i64, i64, f64, i64)>(&sql)
        .fetch_all(pool)
        .await?;

    Ok(rows
        .into_iter()
        .map(
            |(
                provider,
                model,
                call_count,
                total_prompt_tokens,
                total_completion_tokens,
                total_tokens,
                avg_duration_ms,
                error_count,
            )| {
                StatsRow {
                    provider,
                    model,
                    call_count,
                    total_prompt_tokens,
                    total_completion_tokens,
                    total_tokens,
                    avg_duration_ms,
                    error_count,
                }
            },
        )
        .collect())
}
