use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use sag_core::{AppError, CallLog, Usage};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::collections::HashSet;
use tracing::warn;

use crate::store::{
    CacheStatsRow, DistributionRow, LogFilter, LogStore, StatsFilter, StatsRow, StatusCodeFilter,
    TrendFilter, TrendRow,
};

/// Stage-0.4 extension columns added to `call_logs` beyond the original
/// 13-column schema, plus the M7 protocol/lane dimensions. Each entry is
/// `(column_name, sqlite_type_declaration)`. Kept in one place so
/// `ensure_schema_consistency` and its tests share the same list and a
/// rename cannot silently drift out of sync.
const STAGE_04_COLUMNS: &[(&str, &str)] = &[
    ("reasoning_effort", "TEXT"),
    ("upstream_duration_ms", "INTEGER"),
    ("ttft_ms", "INTEGER"),
    ("cached_tokens", "INTEGER"),
    ("cache_creation_tokens", "INTEGER"),
    ("reasoning_tokens", "INTEGER"),
    // M7 (also covered by migrations/20260814…; listed here so a
    // schema-drift database that already recorded the migration still
    // gets the columns repaired).
    ("ingress_protocol", "TEXT"),
    ("upstream_protocol", "TEXT"),
    ("lane", "TEXT"),
    // M8 LossInventory summary (also covered by migrations/20260815…).
    ("loss_summary", "TEXT"),
];

/// Maximum page size for `query_logs`. Shared with the admin API layer
/// (via `sag-admin`) so a runaway `?limit=` cannot stream gigabytes into
/// the dashboard.
pub const MAX_LOG_LIMIT: u64 = 500;

/// Idempotent schema-drift repair for `call_logs`. Runs at every store
/// startup. Missing extension columns are added with `ALTER TABLE … ADD COLUMN`
/// as nullable, matching the `Option<…>` Rust types.
async fn ensure_schema_consistency(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    // Acquire a single connection so the ALTER-TABLE sequence is
    // serialized against any other pool client. With `max_connections=4`,
    // naively issuing checks and ALTERs through separate `.execute`
    // calls would risk a "database is locked" race on a busy store.
    let mut conn = pool.acquire().await?;

    // Read the current column set once instead of one round-trip per
    // expected column.
    let existing: HashSet<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('call_logs')")
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .collect();

    for (name, decl) in STAGE_04_COLUMNS {
        if existing.contains(*name) {
            continue;
        }
        // SQLite `ALTER TABLE … ADD COLUMN` does not accept `NOT NULL`
        // without a `DEFAULT`, and adding a constraint to an existing
        // table is unsupported. New columns are intentionally nullable
        // so pre-existing rows land on `NULL`.
        let sql = format!("ALTER TABLE call_logs ADD COLUMN {} {}", name, decl);
        sqlx::query(&sql).execute(&mut *conn).await?;
        tracing::info!(column = name, decl, "added missing call_logs column");
    }
    Ok(())
}

/// Run `op` and, if it fails with a SQLite schema-change error
/// (`SQLITE_SCHEMA` / "database schema has changed"), log a warning and run
/// it **exactly once** more.
///
/// Rotation rewrites the `call_logs` schema (rename + DDL replay) and
/// rebuilds the read views, which bumps SQLite's schema cookie. sqlx caches
/// prepared statements per connection, so a statement prepared before the DDL
/// surfaces a one-shot `SQLITE_SCHEMA` on its next use — re-preparing on a
/// second attempt succeeds. A second failure is propagated unchanged: the
/// caller maps it to a 500-style [`AppError`], never a panic.
///
/// `op` must be callable twice: it re-builds and re-executes the statement
/// from scratch (sqlx `Query` consumes its arguments on execute). That is why
/// the retry granularity is the *whole* statement or method body — see the
/// call sites — rather than a half-consumed query value.
async fn with_schema_retry<T, F, Fut>(op: F) -> Result<T, sqlx::Error>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T, sqlx::Error>>,
{
    match op().await {
        Err(e) if crate::rotate::is_schema_change(&e) => {
            warn!(
                error = %e,
                "prepared statement invalidated by schema change (SQLITE_SCHEMA); retrying once"
            );
            op().await
        }
        other => other,
    }
}

/// SQLite-backed implementation of [`LogStore`].
pub struct SqliteLogStore {
    pool: SqlitePool,
    /// Whether `shutdown` closes the pool. DSN-constructed stores
    /// ([`SqliteLogStore::new`]) own the pool they create; stores built
    /// over a caller-provided shared pool ([`SqliteLogStore::from_pool`])
    /// must NOT close it — production wires the legacy collector over the
    /// same pool as the formal writer / rollup / query engine, and closing
    /// it would break the whole analytics subsystem.
    owns_pool: bool,
}

impl SqliteLogStore {
    /// Create a new SQLite store. `dsn` may be a file path, `:memory:`, or a
    /// `sqlite://...` URI. The schema is created/updated via `sqlx::migrate!`.
    ///
    /// Connections share the same connect options as the formal analytics
    /// pool ([`crate::open_analytics_pool`]): foreign keys on, an explicit
    /// 5s `busy_timeout`, and — for file DSNs — WAL journal mode with
    /// `synchronous = NORMAL`, so writes to the same physical file as the
    /// formal analytics pool (rollup background transactions, fact writes)
    /// wait for the lock instead of failing immediately with SQLITE_BUSY.
    ///
    /// The returned store owns its pool: [`LogStore::shutdown`] closes it.
    pub async fn new(dsn: &str) -> Result<Self, sqlx::Error> {
        let options = crate::analytics_connect_options(dsn)?;

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;

        // Run migrations to create/update schema
        sqlx::migrate!("./migrations").run(&pool).await?;

        // Repair databases where `CREATE TABLE IF NOT EXISTS` left a
        // pre-stage-0.4 table in place while the migration recorded itself
        // as applied. The repair is idempotent and short-circuits on fresh
        // installs where `pragma_table_info` sees the columns already.
        ensure_schema_consistency(&pool).await?;

        // Existence fallback for the read views, **after** the column repair:
        // the view column list is taken from the hot table's
        // `pragma_table_info`, so building it before the ALTERs would bake the
        // pre-repair shape into the view. `None` builds the full-history
        // shape, which is a superset of the retention window — the scheduler's
        // first rotation round narrows it to the real cutoff.
        crate::rotate::refresh_views(&pool, None).await?;

        Ok(Self {
            pool,
            owns_pool: true,
        })
    }

    /// Wrap a caller-provided pool (migrations already applied). The store
    /// does NOT own the pool: [`LogStore::shutdown`] is a no-op for the
    /// pool itself, so the caller (e.g. the gateway, which shares one
    /// analytics pool across the formal writer / rollup / query engine /
    /// legacy collector) keeps full control of its lifetime.
    ///
    /// The schema-drift repair still runs (idempotent, short-circuits on a
    /// consistent schema) so the legacy store keeps its invariant that the
    /// `call_logs` table is always in the expected shape.
    pub async fn from_pool(pool: SqlitePool) -> Result<Self, sqlx::Error> {
        ensure_schema_consistency(&pool).await?;
        // Same order invariant as [`Self::new`]: repair columns first, then
        // (re)build the read views from the repaired hot-table shape.
        crate::rotate::refresh_views(&pool, None).await?;
        Ok(Self {
            pool,
            owns_pool: false,
        })
    }

    /// Create an in-memory SQLite store (for testing).
    pub async fn new_memory() -> Result<Self, sqlx::Error> {
        Self::new(":memory:").await
    }
}

#[async_trait]
impl LogStore for SqliteLogStore {
    async fn insert_batch(&self, logs: &[CallLog]) -> sag_core::Result<()> {
        // Fast path: an empty batch is a no-op (no need to open a write
        // transaction for it).
        if logs.is_empty() {
            return Ok(());
        }

        // Retry granularity = the WHOLE batch body: a failing statement aborts
        // the transaction (dropping it rolls back), so a retry starts from a
        // clean slate and can never duplicate rows.
        with_schema_retry(|| async {
            // Persist the whole batch in ONE `BEGIN IMMEDIATE` transaction:
            // atomicity (a batch either fully lands or not at all) and a single
            // short write-lock hold instead of one lock acquisition per row.
            // `BEGIN IMMEDIATE` takes the write lock up front so the deferred
            // `BEGIN` upgrade race (SQLITE_BUSY_SNAPSHOT) under concurrent
            // writers in WAL mode is avoided; the pool's busy_timeout covers
            // the wait.
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            for log in logs {
                insert_call_log_row(&mut tx, log).await?;
            }
            tx.commit().await
        })
        .await
        .map_err(|e| AppError::Internal(format!("insert_batch failed: {e}")))
    }

    async fn query_stats(&self, filter: StatsFilter) -> sag_core::Result<Vec<StatsRow>> {
        let mut sql = String::from(
            r#"
            SELECT
                provider,
                model,
                real_model,
                COUNT(*)                                   AS call_count,
                COALESCE(SUM(prompt_tokens), 0)            AS total_prompt_tokens,
                COALESCE(SUM(completion_tokens), 0)        AS total_completion_tokens,
                COALESCE(SUM(total_tokens), 0)             AS total_tokens,
                ROUND(AVG(duration_ms), 1)                 AS avg_duration_ms,
                SUM(CASE WHEN error IS NOT NULL THEN 1 ELSE 0 END) AS error_count
            FROM call_logs_hot
            "#,
        );

        // Collect predicates separately so we can bind parameters in the
        // matching order. Using typed bind (rather than push-to-Vec<String>
        // tricks) keeps SQLite type affinity honest and avoids accidental
        // stringification of numeric values.
        let mut conditions: Vec<&'static str> = Vec::new();
        if filter.provider.as_deref().is_some_and(|p| !p.is_empty()) {
            conditions.push("provider = ?");
        }
        if filter.model.as_deref().is_some_and(|m| !m.is_empty()) {
            conditions.push("model = ?");
        }
        if filter.real_model.as_deref().is_some_and(|m| !m.is_empty()) {
            conditions.push("real_model = ?");
        }
        if filter.from.is_some() {
            conditions.push("timestamp >= ?");
        }
        if filter.to.is_some() {
            conditions.push("timestamp < ?");
        }
        if !conditions.is_empty() {
            sql.push_str("WHERE ");
            sql.push_str(&conditions.join(" AND "));
            sql.push('\n');
        }
        sql.push_str(
            r#"
            GROUP BY provider, model, real_model
            ORDER BY call_count DESC
            "#,
        );

        with_schema_retry(|| async {
            let mut query = sqlx::query_as::<_, StatsRow>(&sql);
            if let Some(p) = filter.provider.as_deref().filter(|p| !p.is_empty()) {
                query = query.bind(p);
            }
            if let Some(m) = filter.model.as_deref().filter(|m| !m.is_empty()) {
                query = query.bind(m);
            }
            if let Some(rm) = filter.real_model.as_deref().filter(|rm| !rm.is_empty()) {
                query = query.bind(rm);
            }
            if let Some(from) = filter.from {
                query = query.bind(from.timestamp_millis());
            }
            if let Some(to) = filter.to {
                query = query.bind(to.timestamp_millis());
            }
            query.fetch_all(&self.pool).await
        })
        .await
        .map_err(|e| AppError::Internal(format!("query_stats failed: {e}")))
    }

    async fn query_logs(&self, filter: LogFilter) -> sag_core::Result<Vec<CallLog>> {
        let mut sql = String::from(
            r#"
            SELECT
                request_id, provider, model, real_model, stream,
                status_code, duration_ms,
                prompt_tokens, completion_tokens, total_tokens,
                error, timestamp,
                reasoning_effort,
                upstream_duration_ms, ttft_ms,
                cached_tokens, cache_creation_tokens, reasoning_tokens,
                user_agent, client_ip,
                error_category,
                ingress_protocol, upstream_protocol, lane,
                loss_summary
            FROM call_logs_hot
            "#,
        );

        let mut conditions: Vec<&'static str> = Vec::new();
        if filter.provider.as_deref().is_some_and(|p| !p.is_empty()) {
            conditions.push("provider = ?");
        }
        if filter.model.as_deref().is_some_and(|m| !m.is_empty()) {
            conditions.push("model = ?");
        }
        if filter.real_model.as_deref().is_some_and(|m| !m.is_empty()) {
            conditions.push("real_model = ?");
        }
        if let Some(sc) = &filter.status_code {
            match sc {
                StatusCodeFilter::Exact(_) => conditions.push("status_code = ?"),
                StatusCodeFilter::Named(name) => match name.as_str() {
                    "non_2xx" => conditions.push("(status_code < 200 OR status_code >= 300)"),
                    "2xx" => conditions.push("(status_code >= 200 AND status_code < 300)"),
                    "4xx" => conditions.push("(status_code >= 400 AND status_code < 500)"),
                    "5xx" => conditions.push("(status_code >= 500 AND status_code < 600)"),
                    _ => {}
                },
            }
        }
        if !conditions.is_empty() {
            sql.push_str("WHERE ");
            sql.push_str(&conditions.join(" AND "));
            sql.push('\n');
        }
        sql.push_str(" ORDER BY timestamp DESC LIMIT ? OFFSET ?");

        let rows = with_schema_retry(|| async {
            let mut query = sqlx::query_as::<_, LogRow>(&sql);
            if let Some(p) = filter.provider.as_deref().filter(|p| !p.is_empty()) {
                query = query.bind(p);
            }
            if let Some(m) = filter.model.as_deref().filter(|m| !m.is_empty()) {
                query = query.bind(m);
            }
            if let Some(rm) = filter.real_model.as_deref().filter(|rm| !rm.is_empty()) {
                query = query.bind(rm);
            }
            if let Some(StatusCodeFilter::Exact(sc)) = &filter.status_code {
                query = query.bind(*sc as i32);
            }
            let limit = filter.limit.min(MAX_LOG_LIMIT);
            query
                .bind(limit as i64)
                .bind(filter.offset as i64)
                .fetch_all(&self.pool)
                .await
        })
        .await
        .map_err(|e| AppError::Internal(format!("query_logs failed: {e}")))?;

        Ok(rows.into_iter().map(|r| r.into_call_log()).collect())
    }

    async fn count_logs(&self, filter: LogFilter) -> sag_core::Result<u64> {
        let mut sql = String::from("SELECT COUNT(*) FROM call_logs_hot");

        let mut conditions: Vec<&'static str> = Vec::new();
        if filter.provider.as_deref().is_some_and(|p| !p.is_empty()) {
            conditions.push("provider = ?");
        }
        if filter.model.as_deref().is_some_and(|m| !m.is_empty()) {
            conditions.push("model = ?");
        }
        if filter.real_model.as_deref().is_some_and(|m| !m.is_empty()) {
            conditions.push("real_model = ?");
        }
        if let Some(sc) = &filter.status_code {
            match sc {
                StatusCodeFilter::Exact(_) => conditions.push("status_code = ?"),
                StatusCodeFilter::Named(name) => match name.as_str() {
                    "non_2xx" => conditions.push("(status_code < 200 OR status_code >= 300)"),
                    "2xx" => conditions.push("(status_code >= 200 AND status_code < 300)"),
                    "4xx" => conditions.push("(status_code >= 400 AND status_code < 500)"),
                    "5xx" => conditions.push("(status_code >= 500 AND status_code < 600)"),
                    _ => {}
                },
            }
        }
        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }

        let count = with_schema_retry(|| async {
            let mut query = sqlx::query_scalar::<_, i64>(&sql);
            if let Some(p) = filter.provider.as_deref().filter(|p| !p.is_empty()) {
                query = query.bind(p);
            }
            if let Some(m) = filter.model.as_deref().filter(|m| !m.is_empty()) {
                query = query.bind(m);
            }
            if let Some(rm) = filter.real_model.as_deref().filter(|rm| !rm.is_empty()) {
                query = query.bind(rm);
            }
            if let Some(StatusCodeFilter::Exact(sc)) = &filter.status_code {
                query = query.bind(*sc as i32);
            }
            query.fetch_one(&self.pool).await
        })
        .await
        .map_err(|e| AppError::Internal(format!("count_logs failed: {e}")))?;

        Ok(count.max(0) as u64)
    }

    async fn query_cache_stats(&self, filter: StatsFilter) -> sag_core::Result<Vec<CacheStatsRow>> {
        let mut sql = String::from(
            r#"
            SELECT
                provider,
                model,
                real_model,
                COUNT(*)                                   AS total_calls,
                COALESCE(SUM(prompt_tokens), 0)            AS total_prompt_tokens,
                COALESCE(SUM(cached_tokens), 0)            AS total_cached_tokens,
                COALESCE(SUM(cache_creation_tokens), 0)    AS total_cache_creation_tokens
            FROM call_logs_hot
            "#,
        );

        let mut conditions: Vec<&'static str> = Vec::new();
        if filter.provider.as_deref().is_some_and(|p| !p.is_empty()) {
            conditions.push("provider = ?");
        }
        if filter.model.as_deref().is_some_and(|m| !m.is_empty()) {
            conditions.push("model = ?");
        }
        if filter.real_model.as_deref().is_some_and(|m| !m.is_empty()) {
            conditions.push("real_model = ?");
        }
        if filter.from.is_some() {
            conditions.push("timestamp >= ?");
        }
        if filter.to.is_some() {
            conditions.push("timestamp < ?");
        }
        if !conditions.is_empty() {
            sql.push_str("WHERE ");
            sql.push_str(&conditions.join(" AND "));
            sql.push('\n');
        }
        sql.push_str(
            r#"
            GROUP BY provider, model, real_model
            ORDER BY total_calls DESC
            "#,
        );

        with_schema_retry(|| async {
            let mut query = sqlx::query_as::<_, CacheStatsRow>(&sql);
            if let Some(p) = filter.provider.as_deref().filter(|p| !p.is_empty()) {
                query = query.bind(p);
            }
            if let Some(m) = filter.model.as_deref().filter(|m| !m.is_empty()) {
                query = query.bind(m);
            }
            if let Some(rm) = filter.real_model.as_deref().filter(|rm| !rm.is_empty()) {
                query = query.bind(rm);
            }
            if let Some(from) = filter.from {
                query = query.bind(from.timestamp_millis());
            }
            if let Some(to) = filter.to {
                query = query.bind(to.timestamp_millis());
            }
            query.fetch_all(&self.pool).await
        })
        .await
        .map_err(|e| AppError::Internal(format!("query_cache_stats failed: {e}")))
    }

    async fn query_trend(&self, filter: TrendFilter) -> sag_core::Result<Vec<TrendRow>> {
        let bucket_ms = (filter.bucket_seconds * 1000) as i64;
        let mut sql = String::from(
            r#"
            SELECT
                timestamp / ? * ? AS bucket,
                COUNT(*)                          AS call_count,
                COALESCE(SUM(prompt_tokens), 0)   AS prompt_tokens,
                COALESCE(SUM(completion_tokens), 0) AS completion_tokens,
                SUM(CASE WHEN status_code = 200 THEN 1 ELSE 0 END) AS success_count
            FROM call_logs_hot
            WHERE timestamp >= ?
            "#,
        );

        if filter.provider.is_some() {
            sql.push_str(" AND provider = ?\n");
        }
        if filter.to.is_some() {
            sql.push_str(" AND timestamp < ?\n");
        }

        sql.push_str(" GROUP BY bucket ORDER BY bucket");

        with_schema_retry(|| async {
            let mut query = sqlx::query_as::<_, TrendRow>(&sql)
                .bind(bucket_ms)
                .bind(bucket_ms)
                .bind(filter.from.timestamp_millis());

            if let Some(p) = &filter.provider {
                query = query.bind(p);
            }
            if let Some(to) = filter.to {
                query = query.bind(to.timestamp_millis());
            }

            query.fetch_all(&self.pool).await
        })
        .await
        .map_err(|e| AppError::Internal(format!("query_trend failed: {e}")))
    }

    async fn query_distribution(
        &self,
        filter: StatsFilter,
    ) -> sag_core::Result<Vec<DistributionRow>> {
        // Reuse query_stats for the grouping, then compute proportions.
        let stats = self.query_stats(filter).await?;
        let total: i64 = stats.iter().map(|r| r.call_count).sum();
        Ok(stats
            .into_iter()
            .map(|r| DistributionRow {
                provider: r.provider,
                model: r.model,
                real_model: r.real_model,
                call_count: r.call_count,
                percentage: if total > 0 {
                    (r.call_count as f64 / total as f64) * 100.0
                } else {
                    0.0
                },
            })
            .collect())
    }

    async fn shutdown(&self) {
        // Only close the pool when this store owns it. A store built over a
        // shared pool (`from_pool`) must leave the caller's pool untouched —
        // closing it would kill the formal writer / rollup / query engine
        // sharing the same pool.
        if self.owns_pool {
            self.pool.close().await;
        }
    }
}

/// Append one [`CallLog`] row inside the caller's batch transaction.
///
/// Extracted out of [`LogStore::insert_batch`] so the retry-once wrapper can
/// re-run the whole batch body: a closure cannot carry a `&mut Transaction`
/// borrow into the future it returns.
async fn insert_call_log_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    log: &CallLog,
) -> Result<(), sqlx::Error> {
    let (prompt, completion, total) = match &log.usage {
        Some(u) => (
            Some(u.prompt_tokens as i64),
            Some(u.completion_tokens as i64),
            Some(u.total_tokens as i64),
        ),
        None => (None, None, None),
    };

    sqlx::query(
        r#"
                INSERT INTO call_logs
                    (request_id, provider, model, real_model, stream,
                     status_code, duration_ms,
                     prompt_tokens, completion_tokens, total_tokens,
                     error, timestamp,
                     reasoning_effort,
                     upstream_duration_ms, ttft_ms,
                     cached_tokens, cache_creation_tokens, reasoning_tokens,
                     user_agent, client_ip,
                     error_category,
                     ingress_protocol, upstream_protocol, lane,
                     loss_summary)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                        ?, ?, ?, ?, ?, ?,
                        ?, ?,
                        ?,
                        ?, ?, ?,
                        ?)
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
    .bind(log.timestamp.timestamp_millis())
    // Extended CallLog fields.
    .bind(log.reasoning_effort.as_ref())
    .bind(log.upstream_duration_ms.map(|v| v as i64))
    .bind(log.ttft_ms.map(|v| v as i64))
    .bind(log.cached_tokens.map(|v| v as i64))
    .bind(log.cache_creation_tokens.map(|v| v as i64))
    .bind(log.reasoning_tokens.map(|v| v as i64))
    .bind(log.user_agent.as_ref())
    .bind(log.client_ip.as_ref())
    .bind(log.error_category.as_ref())
    // M7 protocol / lane dimensions.
    .bind(log.ingress_protocol.as_ref())
    .bind(log.upstream_protocol.as_ref())
    .bind(log.lane.as_ref())
    // M8 LossInventory summary.
    .bind(log.loss_summary.as_ref())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Raw row from the `call_logs` table (timestamp stored as INTEGER unix ms).
#[derive(Debug, sqlx::FromRow)]
struct LogRow {
    request_id: String,
    provider: String,
    model: String,
    real_model: String,
    stream: i32,
    status_code: i32,
    duration_ms: i64,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    total_tokens: Option<i64>,
    error: Option<String>,
    timestamp: i64,
    reasoning_effort: Option<String>,
    upstream_duration_ms: Option<i64>,
    ttft_ms: Option<i64>,
    cached_tokens: Option<i64>,
    cache_creation_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    user_agent: Option<String>,
    client_ip: Option<String>,
    error_category: Option<String>,
    ingress_protocol: Option<String>,
    upstream_protocol: Option<String>,
    lane: Option<String>,
    loss_summary: Option<String>,
}

impl LogRow {
    fn into_call_log(self) -> CallLog {
        // The rows we read back are rows we wrote in the first place, so
        // parse failures here mean corruption. Log + fall back to a
        // sentinel rather than panic so analytics queries don't crash
        // the server, but make sure the operator sees the warning.
        let request_id = uuid::Uuid::parse_str(&self.request_id).unwrap_or_else(|e| {
            warn!(
                row_request_id = %self.request_id,
                error = %e,
                "invalid UUID in call_logs row; using nil sentinel"
            );
            uuid::Uuid::nil()
        });

        let timestamp = DateTime::from_timestamp_millis(self.timestamp).unwrap_or_else(|| {
            warn!(
                row_ts_ms = self.timestamp,
                "invalid millisecond timestamp in call_logs row; using epoch"
            );
            Utc.timestamp_millis_opt(0).unwrap()
        });

        let usage = match (
            self.prompt_tokens,
            self.completion_tokens,
            self.total_tokens,
        ) {
            (Some(p), Some(c), Some(t)) => Some(Usage {
                prompt_tokens: p as u32,
                completion_tokens: c as u32,
                total_tokens: t as u32,
                cached_tokens: self.cached_tokens.map(|v| v as u32),
                cache_creation_tokens: self.cache_creation_tokens.map(|v| v as u32),
                reasoning_tokens: self.reasoning_tokens.map(|v| v as u32),
            }),
            _ => None,
        };

        CallLog {
            request_id,
            provider: self.provider,
            model: self.model,
            real_model: self.real_model,
            stream: self.stream != 0,
            status_code: self.status_code as u16,
            duration_ms: self.duration_ms as u64,
            usage,
            error: self.error,
            timestamp,
            reasoning_effort: self.reasoning_effort,
            upstream_duration_ms: self.upstream_duration_ms.map(|v| v as u64),
            ttft_ms: self.ttft_ms.map(|v| v as u64),
            cached_tokens: self.cached_tokens.map(|v| v as u32),
            cache_creation_tokens: self.cache_creation_tokens.map(|v| v as u32),
            reasoning_tokens: self.reasoning_tokens.map(|v| v as u32),
            user_agent: self.user_agent,
            client_ip: self.client_ip,
            error_category: self.error_category,
            // Identity columns are not persisted yet; legacy rows stay NULL
            // (no retroactive backfill, per the analytics spec). M7
            // protocol/lane dimensions round-trip when present.
            route_id: None,
            target_id: None,
            ingress_protocol: self.ingress_protocol,
            upstream_protocol: self.upstream_protocol,
            lane: self.lane,
            loss_summary: self.loss_summary,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::CallLogCollector;
    use crate::open_analytics_pool;
    use sag_core::Usage;
    use sqlx::sqlite::SqliteConnectOptions;
    use std::sync::Arc;
    use uuid::Uuid;

    fn make_log(provider: &str, model: &str, real_model: &str) -> CallLog {
        CallLog {
            request_id: Uuid::new_v4(),
            provider: provider.to_string(),
            model: model.to_string(),
            real_model: real_model.to_string(),
            stream: false,
            status_code: 200,
            duration_ms: 150,
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            error: None,
            timestamp: Utc::now(),
            reasoning_effort: None,
            upstream_duration_ms: None,
            ttft_ms: None,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
            user_agent: None,
            client_ip: None,
            error_category: None,
            // Internal analytics identity fields: not populated in tests.
            route_id: None,
            target_id: None,
            ingress_protocol: None,
            upstream_protocol: None,
            lane: None,
            loss_summary: None,
        }
    }

    // ─── Read views (rotation query layer) ───────────────────────────

    /// The store-open path must leave both read views in place — every store
    /// read goes through `call_logs_hot`, so a missing view would turn every
    /// analytics query into a 500. Both constructors run it: the DSN path
    /// (`new`) and the shared-pool path (`from_pool`).
    #[tokio::test]
    async fn store_open_creates_read_views() {
        async fn view_names(pool: &SqlitePool) -> Vec<String> {
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'view' ORDER BY name")
                .fetch_all(pool)
                .await
                .unwrap()
        }

        let expected = vec![
            crate::rotate::ALL_VIEW.to_string(),
            crate::rotate::HOT_VIEW.to_string(),
        ];

        let owned = SqliteLogStore::new_memory().await.unwrap();
        assert_eq!(view_names(&owned.pool).await, expected, "`new` path");

        let pool = crate::event::test_support::test_pool().await;
        assert!(view_names(&pool).await.is_empty(), "precondition: no views");
        let shared = SqliteLogStore::from_pool(pool.clone()).await.unwrap();
        assert_eq!(view_names(&shared.pool).await, expected, "`from_pool` path");
    }

    /// With no cold tables the hot view is a single-table passthrough, so
    /// view-backed reads must return exactly what the base table holds.
    #[tokio::test]
    async fn hot_view_reads_match_base_table_without_cold_tables() {
        let store = SqliteLogStore::new_memory().await.unwrap();

        let now = Utc::now();
        let logs: Vec<CallLog> = (0..4)
            .map(|i| {
                let mut log = make_log("openai", &format!("model-{i}"), "real");
                log.timestamp = now - chrono::Duration::milliseconds(i * 1_000);
                log
            })
            .collect();
        store.insert_batch(&logs).await.unwrap();

        let filter = LogFilter {
            limit: 10,
            ..Default::default()
        };

        // query_logs: same rows, same order, same fields as a direct read.
        let via_view = store.query_logs(filter.clone()).await.unwrap();
        let direct: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT request_id, model, timestamp FROM call_logs \
             ORDER BY timestamp DESC LIMIT 10",
        )
        .fetch_all(&store.pool)
        .await
        .unwrap();
        assert_eq!(via_view.len(), direct.len());
        assert_eq!(via_view.len(), 4);
        for (row, (request_id, model, timestamp)) in via_view.iter().zip(direct.iter()) {
            assert_eq!(&row.request_id.to_string(), request_id);
            assert_eq!(&row.model, model);
            assert_eq!(row.timestamp.timestamp_millis(), *timestamp);
        }

        // count_logs: same total as a direct `COUNT(*)`.
        let via_view_count = store.count_logs(filter.clone()).await.unwrap();
        let direct_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM call_logs")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(via_view_count, direct_count as u64);
        assert_eq!(via_view_count, 4);

        // Filtered count also matches the base table.
        let by_model = LogFilter {
            model: Some("model-2".into()),
            ..filter
        };
        assert_eq!(store.count_logs(by_model).await.unwrap(), 1);
    }

    /// `refresh_views(pool, None)` — what the store-open path runs — builds the
    /// full-history shape, so rows older than any retention cutoff stay
    /// visible. A windowed rebuild (the scheduler's real cutoff) hides them,
    /// which is exactly why the open path must not bake a cutoff in.
    #[tokio::test]
    async fn store_open_refresh_covers_full_history() {
        let store = SqliteLogStore::new_memory().await.unwrap();

        let now = Utc::now();
        let mut legacy = make_log("openai", "legacy", "legacy");
        legacy.timestamp = now - chrono::Duration::days(400);
        let mut fresh = make_log("openai", "fresh", "fresh");
        fresh.timestamp = now - chrono::Duration::hours(1);
        store.insert_batch(&[legacy, fresh]).await.unwrap();

        // Windowed rebuild: the view body bakes the cutoff literal, so the
        // 400-day-old row falls out of `call_logs_hot`.
        let cutoff = (now - chrono::Duration::days(90)).timestamp_millis();
        crate::rotate::refresh_views(&store.pool, Some(cutoff))
            .await
            .unwrap();
        assert_eq!(
            store.count_logs(LogFilter::default()).await.unwrap(),
            1,
            "a windowed rebuild must hide out-of-window rows"
        );

        // Store-open semantics: `None` = full-history shape → both rows.
        crate::rotate::refresh_views(&store.pool, None)
            .await
            .unwrap();
        assert_eq!(
            store.count_logs(LogFilter::default()).await.unwrap(),
            2,
            "the full-history shape must expose every row"
        );
        let rows = store
            .query_logs(LogFilter {
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r.model == "legacy"));
    }

    // ─── Retry-once wrapper (SQLITE_SCHEMA) ─────────────────────────

    /// A schema-change error re-runs the statement exactly once — never more.
    #[tokio::test]
    async fn schema_change_error_is_retried_exactly_once() {
        let calls = std::cell::Cell::new(0);
        let result: Result<&'static str, sqlx::Error> = with_schema_retry(|| async {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Err(sqlx::Error::Protocol(
                    "database schema has changed".to_string(),
                ))
            } else {
                Ok("ok")
            }
        })
        .await;

        assert_eq!(result.unwrap(), "ok");
        assert_eq!(calls.get(), 2, "first attempt + exactly one retry");
    }

    /// A retry that fails again is propagated (no third attempt), and
    /// unrelated errors are not retried at all.
    #[tokio::test]
    async fn schema_retry_gives_up_after_one_retry_and_skips_other_errors() {
        let calls = std::cell::Cell::new(0);
        let result: Result<(), sqlx::Error> = with_schema_retry(|| async {
            calls.set(calls.get() + 1);
            Err(sqlx::Error::Protocol(
                "(code: 17) SQLITE_SCHEMA".to_string(),
            ))
        })
        .await;
        assert!(result.is_err(), "a second failure must be propagated");
        assert_eq!(calls.get(), 2, "at most one retry");

        let calls = std::cell::Cell::new(0);
        let result: Result<(), sqlx::Error> = with_schema_retry(|| async {
            calls.set(calls.get() + 1);
            Err(sqlx::Error::Protocol("no such table: nope".to_string()))
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.get(), 1, "unrelated errors must not be retried");
    }

    #[tokio::test]
    async fn insert_and_query_stats_round_trip() {
        let store = SqliteLogStore::new_memory().await.unwrap();

        store
            .insert_batch(&[make_log("openai", "gpt-4o", "gpt-4o")])
            .await
            .unwrap();

        let rows = store.query_stats(StatsFilter::default()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, "openai");
        assert_eq!(rows[0].model, "gpt-4o");
        assert_eq!(rows[0].real_model, "gpt-4o");
        assert_eq!(rows[0].call_count, 1);
        assert_eq!(rows[0].total_prompt_tokens, 10);
        assert_eq!(rows[0].total_completion_tokens, 20);
        assert_eq!(rows[0].total_tokens, 30);
        assert_eq!(rows[0].error_count, 0);
    }

    #[tokio::test]
    async fn query_stats_groups_by_real_model() {
        let store = SqliteLogStore::new_memory().await.unwrap();

        store
            .insert_batch(&[
                make_log("openai", "gpt-4o", "gpt-4o"),
                make_log("openai", "gpt-4o", "gpt-4o-mini"),
            ])
            .await
            .unwrap();

        let rows = store.query_stats(StatsFilter::default()).await.unwrap();
        assert_eq!(rows.len(), 2);

        let gpt4o = rows.iter().find(|r| r.real_model == "gpt-4o").unwrap();
        assert_eq!(gpt4o.call_count, 1);

        let mini = rows.iter().find(|r| r.real_model == "gpt-4o-mini").unwrap();
        assert_eq!(mini.call_count, 1);
    }

    #[tokio::test]
    async fn query_stats_with_provider_filter() {
        let store = SqliteLogStore::new_memory().await.unwrap();

        store
            .insert_batch(&[
                make_log("openai", "gpt-4o", "gpt-4o"),
                make_log("anthropic", "claude", "claude"),
            ])
            .await
            .unwrap();

        let filter = StatsFilter {
            provider: Some("openai".into()),
            ..Default::default()
        };
        let rows = store.query_stats(filter).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, "openai");
    }

    #[tokio::test]
    async fn query_stats_with_special_characters_in_filter() {
        let store = SqliteLogStore::new_memory().await.unwrap();

        store
            .insert_batch(&[make_log("o'brien", "gpt-4'oh", "real")])
            .await
            .unwrap();

        let filter = StatsFilter {
            provider: Some("o'brien".into()),
            ..Default::default()
        };
        let rows = store.query_stats(filter).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, "o'brien");
    }

    #[tokio::test]
    async fn insert_empty_usage() {
        let store = SqliteLogStore::new_memory().await.unwrap();

        let log = CallLog {
            usage: None,
            error_category: None,
            ingress_protocol: None,
            upstream_protocol: None,
            lane: None,
            loss_summary: None,
            ..make_log("openai", "gpt-4o", "gpt-4o")
        };
        store.insert_batch(&[log]).await.unwrap();

        let rows = store.query_stats(StatsFilter::default()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].total_prompt_tokens, 0);
        assert_eq!(rows[0].total_completion_tokens, 0);
        assert_eq!(rows[0].total_tokens, 0);
    }

    #[tokio::test]
    async fn query_logs_pagination() {
        let store = SqliteLogStore::new_memory().await.unwrap();

        let logs: Vec<CallLog> = (0..5)
            .map(|i| make_log("openai", &format!("model-{}", i), "real"))
            .collect();
        store.insert_batch(&logs).await.unwrap();

        // First page: 3 items
        let filter = LogFilter {
            limit: 3,
            offset: 0,
            ..Default::default()
        };
        let page1 = store.query_logs(filter).await.unwrap();
        assert_eq!(page1.len(), 3);

        // Second page: remaining 2 items
        let filter = LogFilter {
            limit: 3,
            offset: 3,
            ..Default::default()
        };
        let page2 = store.query_logs(filter).await.unwrap();
        assert_eq!(page2.len(), 2);
    }

    #[tokio::test]
    async fn query_logs_with_status_code_filters() {
        let store = SqliteLogStore::new_memory().await.unwrap();

        let mut log200 = make_log("openai", "gpt-4o", "gpt-4o");
        log200.status_code = 200;
        let mut log400 = make_log("openai", "gpt-4o", "gpt-4o");
        log400.status_code = 400;
        let mut log502 = make_log("openai", "gpt-4o", "gpt-4o");
        log502.status_code = 502;

        store.insert_batch(&[log200, log400, log502]).await.unwrap();

        // non_2xx should return 400 and 502 (2 items)
        let non_2xx_filter = LogFilter {
            status_code: Some(StatusCodeFilter::Named("non_2xx".into())),
            limit: 10,
            ..Default::default()
        };
        let non_2xx_rows = store.query_logs(non_2xx_filter.clone()).await.unwrap();
        assert_eq!(non_2xx_rows.len(), 2);
        let non_2xx_count = store.count_logs(non_2xx_filter).await.unwrap();
        assert_eq!(non_2xx_count, 2);

        // exact 200 should return 1 item
        let exact_filter = LogFilter {
            status_code: Some(StatusCodeFilter::Exact(200)),
            limit: 10,
            ..Default::default()
        };
        let exact_rows = store.query_logs(exact_filter.clone()).await.unwrap();
        assert_eq!(exact_rows.len(), 1);
        assert_eq!(exact_rows[0].status_code, 200);

        // 4xx should return 1 item
        let f4xx = LogFilter {
            status_code: Some(StatusCodeFilter::Named("4xx".into())),
            limit: 10,
            ..Default::default()
        };
        let f4xx_rows = store.query_logs(f4xx).await.unwrap();
        assert_eq!(f4xx_rows.len(), 1);
        assert_eq!(f4xx_rows[0].status_code, 400);

        // 5xx should return 1 item
        let f5xx = LogFilter {
            status_code: Some(StatusCodeFilter::Named("5xx".into())),
            limit: 10,
            ..Default::default()
        };
        let f5xx_rows = store.query_logs(f5xx).await.unwrap();
        assert_eq!(f5xx_rows.len(), 1);
        assert_eq!(f5xx_rows[0].status_code, 502);
    }

    /// Every CallLog extension field must round-trip through insert/query.
    #[tokio::test]
    async fn insert_and_query_logs_round_trip_stage04_fields() {
        let store = SqliteLogStore::new_memory().await.unwrap();

        let log = CallLog {
            request_id: Uuid::new_v4(),
            provider: "openai".into(),
            model: "o1".into(),
            real_model: "o1-mini".into(),
            stream: false,
            status_code: 200,
            duration_ms: 250,
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cached_tokens: Some(80),
                cache_creation_tokens: None,
                reasoning_tokens: Some(20),
            }),
            error: None,
            timestamp: Utc::now(),
            reasoning_effort: Some("medium".into()),
            upstream_duration_ms: Some(200),
            ttft_ms: Some(35),
            cached_tokens: Some(80),
            cache_creation_tokens: None,
            reasoning_tokens: Some(20),
            user_agent: Some("curl/8.0.1".into()),
            client_ip: Some("203.0.113.7".into()),
            error_category: None,
            // Internal analytics identity fields: not populated in tests.
            route_id: None,
            target_id: None,
            // M7 dimensions must round-trip through insert/query.
            ingress_protocol: Some("chat_completions".into()),
            upstream_protocol: Some("responses".into()),
            lane: Some("native".into()),
            // M8 LossInventory summary must round-trip through insert/query.
            loss_summary: Some(
                "2 entries; tools[0]:unsupported_by_ir; n:>1:unsupported_by_ir".into(),
            ),
        };
        store
            .insert_batch(std::slice::from_ref(&log))
            .await
            .unwrap();

        let logs = store
            .query_logs(LogFilter {
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(logs.len(), 1);
        let fetched = &logs[0];
        assert_eq!(fetched.request_id, log.request_id);
        assert_eq!(fetched.real_model, "o1-mini");
        assert_eq!(fetched.reasoning_effort.as_deref(), Some("medium"));
        assert_eq!(fetched.upstream_duration_ms, Some(200));
        assert_eq!(fetched.ttft_ms, Some(35));
        assert_eq!(fetched.cached_tokens, Some(80));
        assert!(fetched.cache_creation_tokens.is_none());
        assert_eq!(fetched.reasoning_tokens, Some(20));
        assert_eq!(fetched.user_agent.as_deref(), Some("curl/8.0.1"));
        assert_eq!(fetched.client_ip.as_deref(), Some("203.0.113.7"));
        // M7 protocol/lane dimensions must round-trip through insert/query.
        assert_eq!(
            fetched.ingress_protocol.as_deref(),
            Some("chat_completions")
        );
        assert_eq!(fetched.upstream_protocol.as_deref(), Some("responses"));
        assert_eq!(fetched.lane.as_deref(), Some("native"));
        assert_eq!(
            fetched.loss_summary.as_deref(),
            Some("2 entries; tools[0]:unsupported_by_ir; n:>1:unsupported_by_ir")
        );

        let u = fetched.usage.as_ref().expect("usage round-trip");
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(u.completion_tokens, 50);
        assert_eq!(u.total_tokens, 150);
        assert_eq!(u.cached_tokens, Some(80));
        assert_eq!(u.reasoning_tokens, Some(20));
    }

    #[tokio::test]
    async fn query_logs_with_status_code_filter() {
        let store = SqliteLogStore::new_memory().await.unwrap();

        let ok = make_log("openai", "gpt-4o", "gpt-4o");
        let mut err = make_log("openai", "gpt-4o", "gpt-4o");
        err.status_code = 502;
        err.error = Some("upstream timeout".into());
        store.insert_batch(&[ok, err]).await.unwrap();

        let logs = store
            .query_logs(LogFilter {
                status_code: Some(502.into()),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].error.as_deref(), Some("upstream timeout"));
    }

    // ─── Schema-drift repair ─────────────────────────────────────────

    /// Open a file-backed SQLite pool with the pre-stage-0.4 13-column
    /// `call_logs` table. Caller owns `path` for cleanup.
    async fn open_old_schema_db(path: &std::path::Path) -> Result<SqlitePool, sqlx::Error> {
        // `create_if_missing(false)` would fail if the file isn't there;
        // we want to assert the path is clean so prior runs cannot leak
        // columns between tests.
        let _ = std::fs::remove_file(path);
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await?;
        sqlx::query(
            r#"
            CREATE TABLE call_logs (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                request_id          TEXT    NOT NULL,
                provider            TEXT    NOT NULL DEFAULT '',
                model               TEXT    NOT NULL DEFAULT '',
                real_model          TEXT    NOT NULL DEFAULT '',
                stream              INTEGER NOT NULL DEFAULT 0,
                status_code         INTEGER NOT NULL DEFAULT 0,
                duration_ms         INTEGER NOT NULL DEFAULT 0,
                prompt_tokens       INTEGER,
                completion_tokens   INTEGER,
                total_tokens        INTEGER,
                error               TEXT,
                timestamp           INTEGER NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await?;
        Ok(pool)
    }

    fn tmp_db_path(suffix: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "sag-analytics-{}-{}.db",
            suffix,
            uuid::Uuid::new_v4()
        ))
    }

    #[tokio::test]
    async fn ensure_schema_consistency_repairs_pre_stage_04_database() {
        let path = tmp_db_path("drift-repair");
        let pool = open_old_schema_db(&path).await.unwrap();

        // Sanity check: only the 13 legacy columns exist before repair.
        let pre_cols: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('call_logs') ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(
            !pre_cols.iter().any(|c| c == "reasoning_effort"),
            "pre-condition: reasoning_effort must be absent; saw {:?}",
            pre_cols
        );

        ensure_schema_consistency(&pool).await.unwrap();

        // Post-repair: every expected column lands on the table.
        let post_cols: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('call_logs') ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        for (name, _) in STAGE_04_COLUMNS {
            assert!(
                post_cols.iter().any(|c| c == *name),
                "missing after repair: {name}; saw {:?}",
                post_cols
            );
        }

        // The same `SELECT` that crashed in production must now succeed.
        // We use `fetch_optional` because the table is empty at this point —
        // the assertion is "the column exists and the query parses", not
        // "there is at least one row". `fetch_one` would error on an empty
        // result set with `RowNotFound`, which would mask the real signal
        // (a no-such-column error) we'd actually care about catching.
        let _: Option<String> =
            sqlx::query_scalar("SELECT reasoning_effort FROM call_logs LIMIT 1")
                .fetch_optional(&pool)
                .await
                .unwrap();

        // Existing rows must land on NULL (i.e. pre-existing rows are
        // untouched and new columns are NULL by default).
        sqlx::query("INSERT INTO call_logs (request_id, timestamp) VALUES (?, ?)")
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(chrono::Utc::now().timestamp_millis())
            .execute(&pool)
            .await
            .unwrap();
        let stage04_cols: Vec<(Option<String>, Option<i64>, Option<i64>)> =
            sqlx::query_as("SELECT reasoning_effort, upstream_duration_ms, ttft_ms FROM call_logs")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(stage04_cols.len(), 1);
        let (effort, upstream, ttft) = &stage04_cols[0];
        assert!(effort.is_none());
        assert!(upstream.is_none());
        assert!(ttft.is_none());

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn ensure_schema_consistency_is_idempotent() {
        let path = tmp_db_path("drift-idempotent");
        let pool = open_old_schema_db(&path).await.unwrap();

        // First call repairs; second and third must be safe no-ops.
        ensure_schema_consistency(&pool).await.unwrap();
        ensure_schema_consistency(&pool).await.unwrap();
        ensure_schema_consistency(&pool).await.unwrap();

        let final_cols: Vec<String> =
            sqlx::query_scalar("SELECT name FROM pragma_table_info('call_logs') ORDER BY name")
                .fetch_all(&pool)
                .await
                .unwrap();
        for (name, _) in STAGE_04_COLUMNS {
            // Each column appears exactly once — the idempotency test
            // would catch a stale repeated rewrite or duplicate.
            assert_eq!(
                final_cols.iter().filter(|c| c.as_str() == *name).count(),
                1,
                "column {name} must appear exactly once after idempotent repair",
            );
        }

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn ensure_schema_consistency_is_noop_on_fresh_migrated_database() {
        // On a fresh :memory: DB every column is already present; the
        // repair layer must skip every ALTER without raising.
        let store = SqliteLogStore::new_memory().await.unwrap();
        ensure_schema_consistency(&store.pool).await.unwrap();
    }

    #[tokio::test]
    async fn query_cache_stats_aggregates_cached_tokens() {
        let store = SqliteLogStore::new_memory().await.unwrap();

        let mut with_cache = make_log("openai", "gpt-4o", "gpt-4o");
        with_cache.usage = Some(Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cached_tokens: Some(30),
            cache_creation_tokens: Some(15),
            reasoning_tokens: Some(10),
        });
        with_cache.cached_tokens = Some(30);
        with_cache.cache_creation_tokens = Some(15);

        let no_cache = make_log("openai", "gpt-4o", "gpt-4o");

        store.insert_batch(&[with_cache, no_cache]).await.unwrap();

        let rows = store
            .query_cache_stats(StatsFilter::default())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, "openai");
        assert_eq!(rows[0].total_calls, 2);
        assert_eq!(rows[0].total_prompt_tokens, 110);
        assert_eq!(rows[0].total_cached_tokens, 30);
        assert_eq!(rows[0].total_cache_creation_tokens, 15);
    }

    // ─── Shared-pool lifecycle & write contention ────────────────────

    /// Blocker regression: a store built over a caller-provided shared pool
    /// (`from_pool`) must NOT close that pool on `shutdown` — production
    /// builds the legacy collector over the same pool as the formal
    /// EventWriter / rollup / query engine, and closing it would break the
    /// whole analytics subsystem.
    #[tokio::test]
    async fn shared_pool_store_shutdown_does_not_close_caller_pool() {
        let pool = crate::event::test_support::test_pool().await;
        let store = SqliteLogStore::from_pool(pool.clone()).await.unwrap();

        store.shutdown().await;

        // The caller's pool must still serve queries after the store
        // shutdown (a closed pool returns PoolClosed here).
        let one: i64 = sqlx::query_scalar("SELECT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(one, 1, "shared pool must survive store shutdown");
    }

    /// A DSN-constructed store owns its pool: `shutdown` must close it
    /// (backward-compatible with the pre-shared-pool behavior).
    #[tokio::test]
    async fn owned_dsn_store_shutdown_closes_its_own_pool() {
        let dir = std::env::temp_dir().join(format!("sag-analytics-store-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("owned.db");

        let store = SqliteLogStore::new(path.to_str().unwrap()).await.unwrap();
        let pool = store.pool.clone();

        store.shutdown().await;

        let err = sqlx::query_scalar::<_, i64>("SELECT 1")
            .fetch_one(&pool)
            .await
            .unwrap_err();
        assert!(
            matches!(err, sqlx::Error::PoolClosed),
            "owned store shutdown must close its pool, got: {err:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Temporary-file concurrent-writer regression: several writers over
    /// the SAME shared file-backed pool (WAL + busy_timeout +
    /// `synchronous = NORMAL`) must never fail with a locked-database
    /// error — each batch is one `BEGIN IMMEDIATE` transaction that waits
    /// on the WAL write lock — and every queued event must be persisted.
    #[tokio::test]
    async fn concurrent_writers_on_shared_file_pool_persist_all_events() {
        let dir = std::env::temp_dir().join(format!("sag-analytics-store-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("concurrent.db");

        let pool = open_analytics_pool(path.to_str().unwrap()).await.unwrap();

        // 4 writers × 20 batches × 5 logs each, all through separate
        // stores over the one shared pool (like the production assembly).
        const WRITERS: usize = 4;
        const BATCHES: usize = 20;
        const BATCH_SIZE: usize = 5;
        let handles: Vec<_> = (0..WRITERS)
            .map(|w| {
                let pool = pool.clone();
                tokio::spawn(async move {
                    let store = SqliteLogStore::from_pool(pool).await.unwrap();
                    for b in 0..BATCHES {
                        let logs: Vec<CallLog> = (0..BATCH_SIZE)
                            .map(|i| make_log("openai", &format!("w{w}-b{b}-m{i}"), "real"))
                            .collect();
                        store.insert_batch(&logs).await?;
                    }
                    Ok::<(), sag_core::AppError>(())
                })
            })
            .collect();

        for h in handles {
            h.await
                .expect("writer task panicked")
                .expect("insert_batch must not fail with a locked database");
        }

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM call_logs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            count,
            (WRITERS * BATCHES * BATCH_SIZE) as i64,
            "all queued events must be persisted"
        );

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Collector-level regression: `CallLogCollector::new_shared` over the
    /// caller's pool persists events and its `shutdown` must NOT close that
    /// pool (production wires the legacy collector over the same pool as
    /// the formal writer / rollup).
    #[tokio::test]
    async fn shared_pool_collector_shutdown_keeps_caller_pool_alive() {
        let dir = std::env::temp_dir().join(format!("sag-analytics-store-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("collector-shared.db");

        let pool = open_analytics_pool(path.to_str().unwrap()).await.unwrap();
        let collector = Arc::new(CallLogCollector::new_shared(pool.clone()).await.unwrap());

        for i in 0..20 {
            collector.log(make_log("openai", &format!("m{i}"), "real"));
        }
        collector.flush().await;

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM call_logs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 20, "all logged events must be persisted");

        let collector = Arc::into_inner(collector).expect("only one strong ref");
        collector.shutdown().await;

        // The shared pool must survive the collector shutdown.
        let one: i64 = sqlx::query_scalar("SELECT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(one, 1, "collector shutdown must not close the shared pool");

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─── M1 migration: FK enforcement + rollup CHECK constraints ─────

    fn assert_database_error(err: sqlx::Error, needle: &str) {
        let msg = match &err {
            sqlx::Error::Database(db) => db.message(),
            other => panic!("expected database error, got: {other:?}"),
        };
        assert!(
            msg.contains(needle),
            "expected error containing {needle:?}, got: {msg}",
        );
    }

    /// Blocker regression: every pool connection must run with
    /// `foreign_keys(true)`, otherwise the migration's REFERENCES /
    /// ON DELETE RESTRICT constraints are inert. A dangling
    /// `target_attempts.request_id` must be rejected and deleting a
    /// `request_results` row that still has attempts must be RESTRICTed.
    #[tokio::test]
    async fn migration_foreign_keys_are_enforced_on_pool_connections() {
        let store = SqliteLogStore::new_memory().await.unwrap();
        let mut conn = store.pool.acquire().await.unwrap();

        let route_id = Uuid::new_v4().to_string();
        let target_id = Uuid::new_v4().to_string();
        let request_id = Uuid::new_v4().to_string();

        // Seed the referenced identity rows.
        sqlx::query(
            "INSERT INTO analytics_routes (route_id, route_name, first_seen, last_seen)
             VALUES (?, 'test-route', 1000, 1000)",
        )
        .bind(&route_id)
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO analytics_route_targets
                 (target_id, route_id, provider, real_model, first_seen, last_seen)
             VALUES (?, ?, 'provider', 'model', 1000, 1000)",
        )
        .bind(&target_id)
        .bind(&route_id)
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO request_results
                 (request_id, final_status, started_at, completed_at, duration_ms)
             VALUES (?, 'success', 1000, 2000, 1000)",
        )
        .bind(&request_id)
        .execute(&mut *conn)
        .await
        .unwrap();

        // Dangling reference: request_id not in request_results must fail.
        let dangling = Uuid::new_v4().to_string();
        let err = sqlx::query(
            "INSERT INTO target_attempts
                 (request_id, target_id, route_id, actual_model, status,
                  started_at, completed_at, duration_ms)
             VALUES (?, ?, ?, 'model', 'success', 1000, 2000, 1000)",
        )
        .bind(&dangling)
        .bind(&target_id)
        .bind(&route_id)
        .execute(&mut *conn)
        .await
        .unwrap_err();
        assert_database_error(err, "FOREIGN KEY constraint failed");

        // Valid attempt referencing the seeded request must succeed.
        sqlx::query(
            "INSERT INTO target_attempts
                 (request_id, target_id, route_id, actual_model, status,
                  started_at, completed_at, duration_ms)
             VALUES (?, ?, ?, 'model', 'success', 1000, 2000, 1000)",
        )
        .bind(&request_id)
        .bind(&target_id)
        .bind(&route_id)
        .execute(&mut *conn)
        .await
        .unwrap();

        // Facts are permanent: deleting a request with attempts is RESTRICTed.
        let err = sqlx::query("DELETE FROM request_results WHERE request_id = ?")
            .bind(&request_id)
            .execute(&mut *conn)
            .await
            .unwrap_err();
        assert_database_error(err, "FOREIGN KEY constraint failed");
    }

    /// M7 true upgrade: apply only the first N-1 migrations → insert an
    /// old-schema row (new columns do not exist yet) → apply the final
    /// migration → assert the pre-existing row lands on NULL for the new
    /// columns and accepts writes into them.
    ///
    /// Uses sqlx's `Migrate` trait to apply migrations one-by-one so the
    /// intermediate (pre-final) schema is real, not simulated by
    /// hand-written NULLs on an already-migrated table.
    #[tokio::test]
    async fn migration_adds_protocol_lane_columns_null_for_old_rows() {
        use sqlx::migrate::{Migrate, Migrator};

        let path = tmp_db_path("true-upgrade");
        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();

        let migrator =
            Migrator::new(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"))
                .await
                .unwrap();
        let migrations: Vec<_> = migrator.iter().collect();
        // Stop BEFORE the fact-table protocol/lane migration
        // (20260813000001) so `request_results.ingress_protocol` does not
        // exist yet; the call_logs companion (20260814…) lands after it.
        let stop_before: i64 = 20260813000001;
        assert!(
            migrations.iter().any(|m| m.version == stop_before),
            "protocol/lane migration must be present"
        );
        {
            let mut conn = pool.acquire().await.unwrap();
            conn.ensure_migrations_table().await.unwrap();
            for m in &migrations {
                if m.version >= stop_before {
                    break;
                }
                if m.migration_type.is_down_migration() {
                    continue;
                }
                conn.apply(m).await.unwrap();
            }
        }

        // Pre-final schema: the protocol/lane columns must NOT exist yet.
        let has_before: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pragma_table_info('request_results') WHERE name = 'ingress_protocol'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(has_before, 0, "pre-condition: column must not exist yet");

        // Insert an old-schema row (no protocol/lane columns available).
        let route_id = Uuid::new_v4().to_string();
        let request_id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO analytics_routes (route_id, route_name, first_seen, last_seen)
             VALUES (?, 'm7-upgrade-route', 1000, 1000)",
        )
        .bind(&route_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO request_results
                 (request_id, route_id, final_status, started_at, completed_at, duration_ms)
             VALUES (?, ?, 'success', 1000, 2000, 1000)",
        )
        .bind(&request_id)
        .bind(&route_id)
        .execute(&pool)
        .await
        .unwrap();

        // Apply the remaining migrations (protocol/lane on facts +
        // call_logs companion).
        {
            let mut conn = pool.acquire().await.unwrap();
            for m in &migrations {
                if m.version < stop_before {
                    continue;
                }
                if m.migration_type.is_down_migration() {
                    continue;
                }
                conn.apply(m).await.unwrap();
            }
        }

        // Old row → new columns NULL.
        let (ingress, lane): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT ingress_protocol, lane FROM request_results WHERE request_id = ?",
        )
        .bind(&request_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            ingress.is_none(),
            "pre-existing row must have NULL ingress_protocol"
        );
        assert!(lane.is_none(), "pre-existing row must have NULL lane");

        // New write with values works.
        sqlx::query(
            "UPDATE request_results SET ingress_protocol = 'chat_completions', lane = 'native'
             WHERE request_id = ?",
        )
        .bind(&request_id)
        .execute(&pool)
        .await
        .unwrap();
        let (ingress, lane): (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT ingress_protocol, lane FROM request_results WHERE request_id = ?",
        )
        .bind(&request_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ingress.as_deref(), Some("chat_completions"));
        assert_eq!(lane.as_deref(), Some("native"));

        pool.close().await;
        let _ = std::fs::remove_file(&path);
    }

    /// Major regression: the rollup tables must carry the status-count
    /// equality CHECK constraints from the spec. Global counts every
    /// final status; route excludes client_error / no_route.
    #[tokio::test]
    async fn migration_rollup_requests_equality_checks_are_enforced() {
        let store = SqliteLogStore::new_memory().await.unwrap();
        let mut conn = store.pool.acquire().await.unwrap();

        let hour = 3_600_000_i64;

        // Global: requests = success + client_error + no_route + all_disabled
        //                + model_failure + client_cancelled.
        // 8 != 2 + 1 + 1 + 1 + 1 + 1 = 7 → rejected.
        let err = sqlx::query(
            "INSERT INTO rollup_global_hourly
                 (hour, requests, success, client_error, no_route, all_disabled,
                  model_failure, client_cancelled, prompt_tokens, completion_tokens,
                  total_tokens, cached_tokens, cache_creation_tokens,
                  cached_known_count, cached_hit_count, prompt_tokens_cached_known,
                  e2e_count, e2e_sum_ms, ttft_count, ttft_sum_ms,
                  upstream_count, upstream_sum_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(hour)
        .bind(8_i64)
        .bind(2_i64)
        .bind(1_i64)
        .bind(1_i64)
        .bind(1_i64)
        .bind(1_i64)
        .bind(1_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .execute(&mut *conn)
        .await
        .unwrap_err();
        assert_database_error(err, "CHECK constraint failed");

        // Balanced global row (7 = 2 + 1 + 1 + 1 + 1 + 1) must succeed.
        sqlx::query(
            "INSERT INTO rollup_global_hourly
                 (hour, requests, success, client_error, no_route, all_disabled,
                  model_failure, client_cancelled, prompt_tokens, completion_tokens,
                  total_tokens, cached_tokens, cache_creation_tokens,
                  cached_known_count, cached_hit_count, prompt_tokens_cached_known,
                  e2e_count, e2e_sum_ms, ttft_count, ttft_sum_ms,
                  upstream_count, upstream_sum_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(hour)
        .bind(7_i64)
        .bind(2_i64)
        .bind(1_i64)
        .bind(1_i64)
        .bind(1_i64)
        .bind(1_i64)
        .bind(1_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .execute(&mut *conn)
        .await
        .unwrap();

        // Route view excludes client_error / no_route:
        // requests = success + all_disabled + model_failure + client_cancelled.
        // 9 != 5 + 1 + 1 + 1 = 8 → rejected.
        let err = sqlx::query(
            "INSERT INTO rollup_route_hourly
                 (hour, route_id, requests, success, model_failure, all_disabled,
                  client_cancelled, prompt_tokens, completion_tokens, total_tokens,
                  cached_tokens, cached_known_count, cached_hit_count,
                  prompt_tokens_cached_known, e2e_count, e2e_sum_ms, ttft_count,
                  ttft_sum_ms, upstream_count, upstream_sum_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(hour)
        .bind("route-1")
        .bind(9_i64)
        .bind(5_i64)
        .bind(1_i64)
        .bind(1_i64)
        .bind(1_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .execute(&mut *conn)
        .await
        .unwrap_err();
        assert_database_error(err, "CHECK constraint failed");

        // Balanced route row (8 = 5 + 1 + 1 + 1) must succeed.
        sqlx::query(
            "INSERT INTO rollup_route_hourly
                 (hour, route_id, requests, success, model_failure, all_disabled,
                  client_cancelled, prompt_tokens, completion_tokens, total_tokens,
                  cached_tokens, cached_known_count, cached_hit_count,
                  prompt_tokens_cached_known, e2e_count, e2e_sum_ms, ttft_count,
                  ttft_sum_ms, upstream_count, upstream_sum_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(hour)
        .bind("route-1")
        .bind(8_i64)
        .bind(5_i64)
        .bind(1_i64)
        .bind(1_i64)
        .bind(1_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .bind(0_i64)
        .execute(&mut *conn)
        .await
        .unwrap();
    }
}
