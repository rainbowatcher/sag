use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use sag_core::{AppError, CallLog, Usage};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::collections::HashSet;
use std::str::FromStr;
use tracing::warn;

use crate::store::{LogFilter, LogStore, StatsFilter, StatsRow};

/// Stage-0.4 extension columns added to `call_logs` beyond the original
/// 13-column schema. Each entry is `(column_name, sqlite_type_declaration)`.
/// Kept in one place so `ensure_schema_consistency` and its tests share
/// the same list and a rename cannot silently drift out of sync.
const STAGE_04_COLUMNS: &[(&str, &str)] = &[
    ("reasoning_effort", "TEXT"),
    ("upstream_duration_ms", "INTEGER"),
    ("ttft_ms", "INTEGER"),
    ("cached_tokens", "INTEGER"),
    ("cache_creation_tokens", "INTEGER"),
    ("reasoning_tokens", "INTEGER"),
];

/// Maximum page size for `query_logs`. Mirrors the route-layer clamp in
/// `sag-router/src/admin_logs.rs` so a runaway `?limit=` cannot stream
/// gigabytes into the dashboard.
const MAX_LOG_LIMIT: u64 = 500;

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

/// SQLite-backed implementation of [`LogStore`].
pub struct SqliteLogStore {
    pool: SqlitePool,
}

impl SqliteLogStore {
    /// Create a new SQLite store. `dsn` may be a file path, `:memory:`, or a
    /// `sqlite://...` URI. The schema is created/updated via `sqlx::migrate!`.
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

        // Run migrations to create/update schema
        sqlx::migrate!("./migrations").run(&pool).await?;

        // Repair databases where `CREATE TABLE IF NOT EXISTS` left a
        // pre-stage-0.4 table in place while the migration recorded itself
        // as applied. The repair is idempotent and short-circuits on fresh
        // installs where `pragma_table_info` sees the columns already.
        ensure_schema_consistency(&pool).await?;

        Ok(Self { pool })
    }

    /// Create an in-memory SQLite store (for testing).
    pub async fn new_memory() -> Result<Self, sqlx::Error> {
        Self::new(":memory:").await
    }
}

#[async_trait]
impl LogStore for SqliteLogStore {
    async fn insert_batch(&self, logs: &[CallLog]) -> sag_core::Result<()> {
        for log in logs {
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
                     cached_tokens, cache_creation_tokens, reasoning_tokens)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?,
                        ?, ?, ?, ?, ?, ?)
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
            .execute(&self.pool)
            .await
            .map_err(|e| AppError::Internal(format!("failed to insert call log: {e}")))?;
        }
        Ok(())
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
            FROM call_logs
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

        query
            .fetch_all(&self.pool)
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
                cached_tokens, cache_creation_tokens, reasoning_tokens
            FROM call_logs
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
        if filter.status_code.is_some() {
            conditions.push("status_code = ?");
        }
        if !conditions.is_empty() {
            sql.push_str("WHERE ");
            sql.push_str(&conditions.join(" AND "));
            sql.push('\n');
        }
        sql.push_str(" ORDER BY timestamp DESC LIMIT ? OFFSET ?");

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
        if let Some(sc) = filter.status_code {
            query = query.bind(sc as i32);
        }
        let limit = filter.limit.min(MAX_LOG_LIMIT);
        query = query.bind(limit as i64).bind(filter.offset as i64);

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::Internal(format!("query_logs failed: {e}")))?;

        Ok(rows.into_iter().map(|r| r.into_call_log()).collect())
    }

    async fn shutdown(&self) {
        self.pool.close().await;
    }
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sag_core::Usage;
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
        }
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
                status_code: Some(502),
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
}
