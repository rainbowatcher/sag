pub mod collector;
pub mod event;
pub mod event_collector;
pub mod event_writer;
pub mod query;
pub mod registry;
pub mod rollup;
pub mod sqlite_store;
pub mod store;

pub use collector::*;
pub use event::*;
pub use event_collector::*;
pub use event_writer::*;
pub use query::*;
pub use registry::*;
// Selective re-exports: `rollup::HOUR_MS` intentionally stays module-scoped
// (it would collide with the crate-root `query::HOUR_MS` glob export).
pub use rollup::{
    BucketState, BucketStatus, GlobalRollupRow, HistogramBins, HistogramRow, RollupEngine,
    RollupRunReport, RouteRollupRow, TargetRollupRow, bucket_lower_bound_ms, bucket_to_percentile,
    histogram_bucket_idx, merge_histogram_buckets,
};
pub use sqlite_store::SqliteLogStore;
pub use store::*;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;
use std::time::Duration;

/// Open the shared SQLite pool for the formal analytics path.
///
/// This is the pool backing [`EventWriter`] and [`IdentityRegistry`] — both
/// take a pool via [`EventWriter::new`] / [`IdentityRegistry::new`] — and
/// it points at the **same physical database** the legacy
/// [`SqliteLogStore`] / [`CallLogCollector`] write to, with foreign keys
/// enabled and the analytics migrations applied. The production assembly
/// passes the file-backed `analytics.storage_dsn` from config; the formal
/// writer never opens its own private pool (see
/// [`EventWriter::open`], which rejects in-memory DSNs for exactly this
/// split-brain hazard).
///
/// `:memory:` is accepted for tests only; such pools use a single
/// connection so every pool client observes the same private database
/// (each connection to `:memory:` would otherwise be its own database).
///
/// Every connection gets an explicit 5s `busy_timeout`: the pool is
/// shared by [`EventWriter`], [`IdentityRegistry`], [`RollupEngine`] and
/// [`AnalyticsQueryEngine`], and background rollup write transactions
/// contend with fact writes on the same SQLite file. Relying on
/// sqlx/libsqlite defaults is fragile (they have changed across
/// versions), so the lock wait is pinned explicitly. Journal mode and
/// synchronous level are intentionally left untouched.
pub async fn open_analytics_pool(dsn: &str) -> Result<SqlitePool, sqlx::Error> {
    let options = if dsn.starts_with("sqlite:") || dsn == ":memory:" {
        SqliteConnectOptions::from_str(dsn)?
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5))
    } else {
        SqliteConnectOptions::new()
            .filename(dsn)
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5))
    };

    let max_connections = if dsn == ":memory:" { 1 } else { 4 };
    let pool = SqlitePoolOptions::new()
        .max_connections(max_connections)
        .connect_with(options)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use uuid::Uuid;

    /// Open a file-backed pool via the production construction path and
    /// assert the connection-level `busy_timeout` is the explicit 5s we
    /// pin (not whatever sqlx/libsqlite happens to default to).
    #[tokio::test]
    async fn open_analytics_pool_pins_explicit_busy_timeout() {
        let dir = std::env::temp_dir().join(format!("sag-analytics-lib-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("busy-timeout.db");
        let dsn = path.to_str().unwrap();

        let pool = open_analytics_pool(dsn).await.unwrap();
        let timeout_ms: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            timeout_ms, 5000,
            "open_analytics_pool must set an explicit 5s busy_timeout, got {timeout_ms}ms"
        );

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Behavioral guard for the rollup-vs-fact-writer lock contention the
    /// busy_timeout exists for: while one connection holds an exclusive
    /// write lock, a second writer from the same pool must *wait* and then
    /// succeed, not fail immediately with SQLITE_BUSY.
    #[tokio::test]
    async fn concurrent_writer_waits_for_db_lock_instead_of_failing() {
        let dir = std::env::temp_dir().join(format!("sag-analytics-lib-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("contention.db");
        let dsn = path.to_str().unwrap();

        let pool = open_analytics_pool(dsn).await.unwrap();
        let mut lock_conn = pool.acquire().await.unwrap();
        // Take an exclusive lock: any other writer must now wait (rollup
        // background transactions contend with fact writes exactly this way).
        sqlx::query("BEGIN EXCLUSIVE")
            .execute(&mut *lock_conn)
            .await
            .unwrap();

        let writer_pool = pool.clone();
        let writer = tokio::spawn(async move {
            // With busy_timeout = 0 (or libsqlite's default) this fails
            // immediately with SQLITE_BUSY; with our 5s pin it blocks
            // until the lock is released and then succeeds.
            sqlx::query("CREATE TABLE contention_probe (id INTEGER PRIMARY KEY)")
                .execute(&writer_pool)
                .await
                .unwrap();
        });

        // Give the writer time to start and hit the lock before releasing.
        tokio::time::sleep(Duration::from_millis(300)).await;
        sqlx::query("COMMIT")
            .execute(&mut *lock_conn)
            .await
            .unwrap();
        writer.await.unwrap();

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
