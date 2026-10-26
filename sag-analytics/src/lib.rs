pub mod collector;
pub mod event;
pub mod event_collector;
pub mod event_writer;
pub mod query;
pub mod registry;
pub mod rollup;
pub mod rotate;
pub mod session_registry;
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
pub use rotate::{
    ALL_VIEW, ARCH_PREFIX, HOT_TABLE, HOT_VIEW, RoundOutcome, is_schema_change, refresh_views,
    retire_indexes, rotate_once, run_rotation_round, unique_archive_name,
};
pub use session_registry::{
    SessionRegistryFact, get_session, prune_session_registry, upsert_session,
};
pub use sqlite_store::{MAX_LOG_LIMIT, SqliteLogStore};
pub use store::*;
// Re-exported so maintenance callers (the gateway's call_logs rotation
// scheduler) can name the shared pool type without taking a direct `sqlx`
// dependency of their own.
pub use sqlx::sqlite::SqlitePool;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::str::FromStr;
use std::time::Duration;

/// Build the sqlx connect options shared by **every** analytics SQLite pool
/// — the formal [`open_analytics_pool`], the legacy [`SqliteLogStore`] /
/// [`CallLogCollector`], and [`EventWriter::open`]: foreign keys on, an
/// explicit 5s `busy_timeout`, and — for file-backed databases — WAL
/// journal mode with `synchronous = NORMAL`.
///
/// WAL is what lets multiple pool connections (formal fact writer, rollup
/// engine, legacy call-log store, query engine) write the same file without
/// one-shot `SQLITE_BUSY` failures: readers never block writers and writers
/// serialize on the single WAL write lock, waiting via the busy timeout
/// instead of erroring. `synchronous = NORMAL` in WAL mode keeps fsyncs off
/// the hot path while remaining crash-consistent (SQLite's recommended
/// combination). In-memory DSNs (`:memory:` and friends, per
/// [`crate::event_writer::is_in_memory_dsn`]) skip both: SQLite does not
/// support WAL for in-memory databases.
pub(crate) fn analytics_connect_options(dsn: &str) -> Result<SqliteConnectOptions, sqlx::Error> {
    let options = if dsn.starts_with("sqlite:") || dsn == ":memory:" {
        SqliteConnectOptions::from_str(dsn)?
    } else {
        SqliteConnectOptions::new()
            .filename(dsn)
            .create_if_missing(true)
    };
    // `SqliteConnectOptions::from_str` defaults to `create_if_missing(false)`,
    // so a `sqlite://` DSN pointing at a not-yet-existing file fails with
    // SQLITE_CANTOPEN (14). That was masked historically: the CWD-relative
    // `analytics.db` always existed by the time the pool opened. With
    // XDG-anchored DSNs (see `sag_core::paths::anchor_sqlite_dsn`) the file
    // is created on first startup, so the pool must be allowed to create it
    // — unless the DSN opts out explicitly (`mode=rw` / `mode=ro`; sqlx maps
    // `mode=rwc` onto create_if_missing itself).
    let options = if dsn.contains("mode=") {
        options
    } else {
        options.create_if_missing(true)
    };

    let options = options
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(5));

    if crate::event_writer::is_in_memory_dsn(dsn) {
        Ok(options)
    } else {
        Ok(options
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal))
    }
}

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
/// versions), so the lock wait is pinned explicitly. File-backed databases
/// additionally run in WAL journal mode with `synchronous = NORMAL` (see
/// `analytics_connect_options`) so all writer connections coordinate
/// through the single WAL write lock instead of racing for the database
/// lock; `:memory:` databases keep their default journal mode.
pub async fn open_analytics_pool(dsn: &str) -> Result<SqlitePool, sqlx::Error> {
    let options = analytics_connect_options(dsn)?;

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
    /// A `sqlite://` DSN pointing at a not-yet-existing file must succeed:
    /// `SqliteConnectOptions::from_str` defaults to `create_if_missing(false)`,
    /// but production startup (XDG-anchored DSNs, see
    /// `sag_core::paths::anchor_sqlite_dsn`) always opens a fresh path on
    /// first run. `mode=rw` opts out explicitly and must still fail.
    #[tokio::test]
    async fn sqlite_dsn_creates_missing_database_file() {
        let dir = std::env::temp_dir().join(format!("sag-analytics-lib-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fresh.db");
        let dsn = format!("sqlite://{}", path.display());
        assert!(!path.exists(), "precondition: db file must not exist");

        let pool = open_analytics_pool(&dsn).await.unwrap();
        assert!(path.exists(), "pool open must create the db file");
        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `mode=rw` is the explicit opt-out: an existing file opens, a missing
    /// one must keep failing (the caller asked for read-write-no-create).
    #[tokio::test]
    async fn sqlite_dsn_mode_rw_keeps_create_disabled() {
        let dir = std::env::temp_dir().join(format!("sag-analytics-lib-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("absent.db");
        let dsn = format!("sqlite://{}?mode=rw", path.display());

        let result = open_analytics_pool(&dsn).await;
        assert!(result.is_err(), "mode=rw must not create the db file");
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// File-backed pools must report WAL journal mode, `synchronous =
    /// NORMAL` (SQLite numeric value 1) and the explicit 5s
    /// `busy_timeout` on every connection — the three settings that keep
    /// multiple analytics writers (fact writer, rollup, legacy store)
    /// from failing with one-shot `SQLITE_BUSY` on the same file.
    #[tokio::test]
    async fn file_pool_connections_report_wal_normal_and_busy_timeout() {
        let dir = std::env::temp_dir().join(format!("sag-analytics-lib-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wal-options.db");
        let dsn = path.to_str().unwrap();

        let pool = open_analytics_pool(dsn).await.unwrap();

        let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await
            .unwrap();
        let sync: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(&pool)
            .await
            .unwrap();
        let timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(&pool)
            .await
            .unwrap();

        assert_eq!(journal, "wal", "file pool must run WAL, got {journal}");
        assert_eq!(sync, 1, "file pool must run synchronous=NORMAL, got {sync}");
        assert_eq!(
            timeout, 5000,
            "file pool must keep the 5s busy_timeout, got {timeout}ms"
        );

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// In-memory pools must NOT be forced into WAL (SQLite does not
    /// support WAL for `:memory:` databases) and must keep the test
    /// construction path working unchanged.
    #[tokio::test]
    async fn memory_pool_keeps_default_journal_mode() {
        let pool = open_analytics_pool(":memory:").await.unwrap();

        let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            journal, "memory",
            "in-memory pool must keep its default journal mode, got {journal}"
        );

        pool.close().await;
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
