use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sag_core::CallLog;
use serde::Serialize;

use crate::session_registry::SessionRegistryFact;

/// Persistence abstraction for analytics data.
///
/// Implementations include [`crate::SqliteLogStore`] (local) and potentially
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

    /// Upsert one `session_registry` row. First-write-wins: an existing row
    /// only has its `last_seen` advanced.
    async fn upsert_session(&self, fact: &SessionRegistryFact) -> sag_core::Result<()>;

    /// Fetch the registry record for a hashed session identity
    /// (`CallLog::session_key`). `None` when the session was never recorded,
    /// predates the registry, or has already been pruned.
    async fn get_session(&self, key: &str) -> sag_core::Result<Option<SessionRecord>>;

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

/// Status code filter for paginated call log queries.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum StatusCodeFilter {
    /// Exact HTTP status code (e.g. 200, 404, 502).
    Exact(u16),
    /// Named filter category such as "non_2xx", "2xx", "4xx", "5xx".
    Named(String),
}

impl StatusCodeFilter {
    pub fn parse(s: &str) -> Option<Self> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return None;
        }
        match trimmed.to_lowercase().as_str() {
            "non_2xx" | "non-2xx" | "non2xx" | "error" | "errors" => {
                Some(Self::Named("non_2xx".into()))
            }
            "2xx" => Some(Self::Named("2xx".into())),
            "4xx" => Some(Self::Named("4xx".into())),
            "5xx" => Some(Self::Named("5xx".into())),
            other => other.parse::<u16>().ok().map(Self::Exact),
        }
    }
}

impl From<u16> for StatusCodeFilter {
    fn from(code: u16) -> Self {
        Self::Exact(code)
    }
}

/// Filter for paginated call log queries.
#[derive(Debug, Clone, Default)]
pub struct LogFilter {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub real_model: Option<String>,
    /// Hashed session identity (`CallLog::session_key`) — exact match.
    pub session: Option<String>,
    pub status_code: Option<StatusCodeFilter>,
    pub offset: u64,
    pub limit: u64,
}

/// One `session_registry` row: the displayable side of a hashed session
/// identity, written earliest-requester-wins by the collector and read back
/// by the admin logs endpoint (`?session=` → envelope `session`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionRecord {
    /// 32-hex-char `SessionKey::map_id` digest (also `call_logs.session_key`).
    pub session_key: String,
    /// Extraction-chain source prefix (`meta` / `top` / `pc` / `fb`).
    pub session_source: String,
    /// Plaintext conversation id, truncated to 128 chars. `None` for the
    /// `pc` / `fb` sources, which carry no conversation id.
    pub conversation_id: Option<String>,
    /// Title-style preview of the session opener (first user message, markup
    /// stripped, whitespace collapsed, truncated to 160 chars).
    pub first_message_preview: Option<String>,
    /// First request that observed this session.
    pub first_seen: DateTime<Utc>,
    /// Most recent request that observed this session.
    pub last_seen: DateTime<Utc>,
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
