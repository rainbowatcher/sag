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

    /// Count total call log entries matching the filter (for pagination metadata).
    async fn count_logs(&self, filter: LogFilter) -> sag_core::Result<u64>;

    /// Query cache hit-rate statistics grouped by `(provider, model, real_model)`.
    async fn query_cache_stats(&self, filter: StatsFilter) -> sag_core::Result<Vec<CacheStatsRow>>;

    /// Time-bucketed trend query. `bucket_seconds` controls the bucket width.
    async fn query_trend(&self, filter: TrendFilter) -> sag_core::Result<Vec<TrendRow>>;

    /// Distribution (proportion) query grouped by `(provider, model, real_model)`.
    async fn query_distribution(
        &self,
        filter: StatsFilter,
    ) -> sag_core::Result<Vec<DistributionRow>>;

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

/// Cache hit-rate statistics row, grouped by `(provider, model, real_model)`.
///
/// The client computes `cache_hit_rate` as `total_cached_tokens / total_prompt_tokens`
/// when `total_prompt_tokens > 0`. Raw sums are returned so the frontend can
/// aggregate or display them independently.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct CacheStatsRow {
    pub provider: String,
    pub model: String,
    pub real_model: String,
    pub total_calls: i64,
    pub total_prompt_tokens: i64,
    pub total_cached_tokens: i64,
    pub total_cache_creation_tokens: i64,
}

/// Filter for time-bucketed trend queries.
#[derive(Debug, Clone)]
pub struct TrendFilter {
    pub provider: Option<String>,
    pub bucket_seconds: u64,
    pub from: DateTime<Utc>,
    pub to: Option<DateTime<Utc>>,
}

/// Time-bucketed trend row. `bucket` is a unix-millisecond timestamp marking
/// the start of the bucket.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct TrendRow {
    pub bucket: i64,
    pub call_count: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub success_count: i64,
}

/// Distribution row with call proportion. `percentage` is `call_count / total * 100`
/// so all rows for a given query sum to 100.
#[derive(Debug, Serialize)]
pub struct DistributionRow {
    pub provider: String,
    pub model: String,
    pub real_model: String,
    pub call_count: i64,
    pub percentage: f64,
}
