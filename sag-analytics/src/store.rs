use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sag_core::CallLog;
use serde::Serialize;

/// Persistence abstraction for analytics data.
///
/// Implementations include [`SqliteLogStore`] (local) and potentially
/// `PostgresLogStore` or `ClickHouseLogStore` for commercial deployments.
/// The trait is designed so that collector and query layers never need to
/// know which backend is in use.
#[async_trait]
pub trait LogStore: Send + Sync {
    /// Persist one or more call log entries.
    async fn insert_batch(&self, logs: &[CallLog]) -> sag_core::Result<()>;

    /// Query aggregated statistics grouped by `(provider, model, real_model)`.
    async fn query_stats(&self, filter: StatsFilter) -> sag_core::Result<Vec<StatsRow>>;

    /// Query individual call log entries with pagination.
    async fn query_logs(&self, filter: LogFilter) -> sag_core::Result<Vec<CallLog>>;

    /// Gracefully shut down the store, flushing any pending writes.
    async fn shutdown(&self);
}

/// Filter for aggregated statistics queries.
#[derive(Debug, Clone, Default)]
pub struct StatsFilter {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub real_model: Option<String>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

/// Filter for paginated call log queries.
#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub real_model: Option<String>,
    pub status_code: Option<u16>,
    pub offset: u64,
    pub limit: u64,
}

/// Aggregated statistics row, grouped by `(provider, model, real_model)`.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct StatsRow {
    pub provider: String,
    pub model: String,
    pub real_model: String,
    pub call_count: i64,
    pub total_prompt_tokens: i64,
    pub total_completion_tokens: i64,
    pub total_tokens: i64,
    pub avg_duration_ms: f64,
    pub error_count: i64,
}
