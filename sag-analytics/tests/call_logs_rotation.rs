//! Integration tests for the `call_logs` hot/cold rotation maintenance loop
//! (spec §F tests 12–16).
//!
//! These live at crate level (not inline in `rotate.rs`) because they exercise
//! the *round* — the composition of `retire_indexes` → `rotate_once` →
//! `refresh_views` — together with the query layer (`SqliteLogStore`), over a
//! real file-backed pool shared by several tasks (WAL, `busy_timeout`, one
//! pool like production).
//!
//! The scheduler that drives the round in production lives in `sag-gateway`,
//! which depends on this crate (never the other way round), so the round is
//! driven here through the shared public entry point
//! [`run_rotation_round`] — the same function the gateway scheduler calls, with
//! retention passed as an argument instead of read from the live config. The
//! "spawned task runs its first round immediately" half of §0 is covered by
//! `sag-gateway/src/main.rs::rotation_scheduler_shuts_down_before_event_drain`.

use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use sag_analytics::{
    ALL_VIEW, ARCH_PREFIX, HOT_TABLE, HOT_VIEW, LogFilter, LogStore, RoundOutcome, SqliteLogStore,
    SqlitePool, StatsFilter, TrendFilter, open_analytics_pool, refresh_views, run_rotation_round,
};
use sag_core::{CallLog, Usage};
use uuid::Uuid;

const DAY_MS: i64 = 86_400_000;

/// Production default (`AnalyticsConfig::call_log_retention_days`).
const RETENTION_DAYS: u32 = 90;

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// A throwaway directory holding one SQLite file.
struct TempDb {
    dir: std::path::PathBuf,
}

impl TempDb {
    fn new(suffix: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "sag-analytics-rotation-{suffix}-{}",
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn path(&self) -> std::path::PathBuf {
        self.dir.join("analytics.db")
    }

    fn dsn(&self) -> String {
        self.path().to_str().unwrap().to_string()
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn make_log(model: &str, timestamp_ms: i64) -> CallLog {
    CallLog {
        request_id: Uuid::new_v4(),
        provider: "openai".to_string(),
        model: model.to_string(),
        real_model: model.to_string(),
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
        timestamp: Utc.timestamp_millis_opt(timestamp_ms).unwrap(),
        reasoning_effort: None,
        upstream_duration_ms: None,
        ttft_ms: None,
        cached_tokens: None,
        cache_creation_tokens: None,
        reasoning_tokens: None,
        user_agent: None,
        client_ip: None,
        error_category: None,
        route_id: None,
        target_id: None,
        ingress_protocol: None,
        upstream_protocol: None,
        lane: None,
        loss_summary: None,
    }
}

/// `SELECT COUNT(*)` on a table or view.
async fn count(pool: &SqlitePool, name: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM \"{name}\""))
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Every cold table, newest first.
async fn cold_tables(pool: &SqlitePool) -> Vec<String> {
    let pattern = format!("{}%", ARCH_PREFIX);
    sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE ?1 ORDER BY name DESC",
    )
    .bind(pattern)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn index_names(pool: &SqlitePool, table: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = ?1 AND sql IS NOT NULL \
         ORDER BY name",
    )
    .bind(table)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// A synthetic cold table shaped like a legacy snapshot (three columns).
async fn create_legacy_cold(pool: &SqlitePool, name: &str) {
    sqlx::query(&format!(
        "CREATE TABLE \"{name}\" (id INTEGER PRIMARY KEY, request_id TEXT NOT NULL, \
         timestamp INTEGER NOT NULL)"
    ))
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_legacy(pool: &SqlitePool, table: &str, request_id: &str, ts: i64) {
    sqlx::query(&format!(
        "INSERT INTO \"{table}\" (request_id, timestamp) VALUES (?1, ?2)"
    ))
    .bind(request_id)
    .bind(ts)
    .execute(pool)
    .await
    .unwrap();
}

/// Insert `logs` in batches of 500 through the production write path.
async fn insert_batches(store: &SqliteLogStore, logs: &[CallLog]) {
    for chunk in logs.chunks(500) {
        store.insert_batch(chunk).await.unwrap();
    }
}

// ─── 12. Startup backlog ─────────────────────────────────────────────

/// §C shrunk down: 3,000 rows spanning 105 days against a 90-day retention.
/// The store-open fallback view is the full-history superset; one round then
/// rotates the whole backlog (no row-level data path) and narrows
/// `call_logs_hot` to the window while `call_logs_all` keeps every row.
#[tokio::test]
async fn startup_backlog_first_round_rotates_immediately() {
    let env = TempDb::new("startup-backlog");
    let pool = open_analytics_pool(&env.dsn()).await.unwrap();
    // Store open = production startup: `ensure_schema_consistency` then
    // `refresh_views(pool, None)` (full-history shape).
    let store = SqliteLogStore::from_pool(pool.clone()).await.unwrap();

    const ROWS: usize = 3_000;
    let now = now_ms();
    // One row every ~50 minutes, newest first, spanning 105 days.
    let timestamps: Vec<i64> = (0..ROWS)
        .map(|i| now - (i as i64 * 105 * DAY_MS / ROWS as i64))
        .collect();
    let logs: Vec<CallLog> = timestamps
        .iter()
        .map(|ts| make_log("gpt-4o", *ts))
        .collect();
    insert_batches(&store, &logs).await;

    // Startup transient: the fallback view shows the whole history (a
    // superset of the window — extra rows, never a missing row).
    assert_eq!(count(&pool, HOT_TABLE).await, ROWS as i64);
    assert_eq!(count(&pool, HOT_VIEW).await, ROWS as i64);

    let cutoff_ms = now_ms() - RETENTION_DAYS as i64 * DAY_MS;
    let expected_in_window = timestamps.iter().filter(|ts| **ts >= cutoff_ms).count() as i64;
    assert!(
        expected_in_window > 0 && expected_in_window < ROWS as i64,
        "precondition: the backlog must straddle the retention window \
         (in-window {expected_in_window} of {ROWS})"
    );

    // The first round (the scheduler runs it immediately after spawn, §0).
    let outcome = run_rotation_round(&pool, RETENTION_DAYS).await.unwrap();
    let arch = outcome
        .rotated()
        .expect("the first round must rotate the backlog")
        .to_string();
    assert!(arch.starts_with(ARCH_PREFIX), "unexpected archive {arch}");

    // Exactly one cold table, empty hot table, every row preserved.
    assert_eq!(cold_tables(&pool).await, vec![arch.clone()]);
    assert_eq!(count(&pool, HOT_TABLE).await, 0);
    assert_eq!(count(&pool, &arch).await, ROWS as i64);
    // Window semantics: only the in-window rows are visible through the hot
    // view, the full history stays reachable through `call_logs_all`.
    assert_eq!(count(&pool, HOT_VIEW).await, expected_in_window);
    assert_eq!(count(&pool, ALL_VIEW).await, ROWS as i64);
}

// ─── 13. Concurrent writes during rotation rounds ────────────────────

/// Rotation rounds run while several writers keep inserting through the
/// production write path. Every round is given an expired row, so all five
/// rounds run a real `RENAME` (the DDL transaction the writers must not trip
/// over) instead of the cheap in-window no-op path. Nothing may be lost or
/// duplicated and no `busy` error may surface.
#[tokio::test]
async fn concurrent_writes_during_rotation_rounds_lose_nothing() {
    let env = TempDb::new("concurrent-rotation");
    let pool = open_analytics_pool(&env.dsn()).await.unwrap();
    let store = Arc::new(SqliteLogStore::from_pool(pool.clone()).await.unwrap());

    const ROUNDS: usize = 5;
    const WRITERS: usize = 4;
    const BATCHES: usize = 20;
    const BATCH_SIZE: usize = 5;

    // One row far outside the window: the first round must rotate it.
    store
        .insert_batch(&[make_log("backlog", now_ms() - 400 * DAY_MS)])
        .await
        .unwrap();

    let rounds = {
        let pool = pool.clone();
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            let mut rotated = Vec::new();
            for round in 0..ROUNDS {
                // Every round gets its own expired row, so **every** round runs
                // a real RENAME (an in-window `MIN(timestamp)` would take the
                // cheap no-op path and never interleave DDL with the writers).
                store
                    .insert_batch(&[make_log(
                        &format!("expired-{round}"),
                        now_ms() - 400 * DAY_MS,
                    )])
                    .await
                    .unwrap();
                let outcome = run_rotation_round(&pool, RETENTION_DAYS).await.unwrap();
                if let Some(arch) = outcome.rotated() {
                    rotated.push(arch.to_string());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            rotated
        })
    };

    let writers: Vec<_> = (0..WRITERS)
        .map(|w| {
            let store = Arc::clone(&store);
            tokio::spawn(async move {
                for b in 0..BATCHES {
                    let logs: Vec<CallLog> = (0..BATCH_SIZE)
                        .map(|i| make_log(&format!("w{w}-b{b}-m{i}"), now_ms()))
                        .collect();
                    // The production failure mode this test locks: a
                    // SQLITE_BUSY / locked-database error while rotation
                    // holds the write lock.
                    store
                        .insert_batch(&logs)
                        .await
                        .expect("insert_batch must not fail with a locked database");
                }
            })
        })
        .collect();

    for h in writers {
        h.await.expect("writer task panicked");
    }
    let rotated = rounds.await.expect("rotation task panicked");

    // Every round injected an expired row, so every round rotated — five real
    // RENAMEs interleaved with the concurrent inserts.
    assert_eq!(
        rotated.len(),
        ROUNDS,
        "every round had an expired row and must rotate: {rotated:?}"
    );
    let colds = cold_tables(&pool).await;
    assert_eq!(
        colds.len(),
        ROUNDS,
        "one cold table per rotating round: {colds:?}"
    );

    // A write issued after the round settled lands in the fresh hot table.
    let marker = make_log("marker", now_ms());
    let marker_id = marker.request_id;
    store.insert_batch(&[marker]).await.unwrap();

    // 1 backlog row + one injected expired row per round + the writers' rows +
    // the marker.
    let total = 1 + ROUNDS + WRITERS * BATCHES * BATCH_SIZE + 1;
    let hot = count(&pool, HOT_TABLE).await;
    let mut cold = 0;
    for table in &colds {
        cold += count(&pool, table).await;
    }
    assert_eq!(hot + cold, total as i64, "no row may be lost");
    assert_eq!(count(&pool, ALL_VIEW).await, total as i64);
    // No duplicates either: rows written before the rotation moved with the
    // table (rename, not copy), so each request appears exactly once.
    let distinct: i64 = sqlx::query_scalar("SELECT COUNT(DISTINCT request_id) FROM call_logs_all")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(distinct, total as i64, "no row may be duplicated");
    let marker_in_hot: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM call_logs WHERE request_id = ?1")
            .bind(marker_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(marker_in_hot, 1, "post-round writes land in the hot table");
}

// ─── 14. View atomicity for readers ──────────────────────────────────

/// Background rounds rotate / refresh while a foreground loop queries through
/// the view-backed store: readers must never see a missing view
/// (`no such table: call_logs_hot`) and never a stale (full-history) shape —
/// every visible row belongs to the current window, and no in-window row is
/// ever lost or duplicated by the view rebuild.
#[tokio::test]
async fn queries_never_see_missing_or_stale_view() {
    let env = TempDb::new("view-atomicity");
    let pool = open_analytics_pool(&env.dsn()).await.unwrap();
    let store = Arc::new(SqliteLogStore::from_pool(pool.clone()).await.unwrap());

    const IN_WINDOW: usize = 100;
    const EXPIRED: usize = 100;
    let now = now_ms();
    let in_window: Vec<CallLog> = (0..IN_WINDOW)
        .map(|i| make_log(&format!("in-{i}"), now - DAY_MS))
        .collect();
    let expired: Vec<CallLog> = (0..EXPIRED)
        .map(|i| make_log(&format!("out-{i}"), now - 200 * DAY_MS))
        .collect();
    store.insert_batch(&in_window).await.unwrap();
    store.insert_batch(&expired).await.unwrap();

    // Settle the window once, so every round the loops run with uses the same
    // cutoff (the startup superset is gone after this).
    let window_start = now_ms() - RETENTION_DAYS as i64 * DAY_MS;
    assert!(
        run_rotation_round(&pool, RETENTION_DAYS)
            .await
            .unwrap()
            .rotated()
            .is_some(),
        "the expired rows must rotate"
    );
    assert_eq!(count(&pool, HOT_VIEW).await, IN_WINDOW as i64);

    const ROUNDS: usize = 15;
    let background = {
        let pool = pool.clone();
        let store = Arc::clone(&store);
        tokio::spawn(async move {
            for round in 0..ROUNDS {
                // One expired row per round, so every round runs a real
                // RENAME instead of the cheap `MIN(timestamp)` no-op path —
                // that is the window in which readers must never observe a
                // missing view. The row is outside the window, and the
                // rotated-out in-window rows stay visible through
                // `call_logs_hot` (their cold table is still a member while
                // its `MAX(timestamp)` is in-window), so the reader
                // assertions below are unaffected.
                store
                    .insert_batch(&[make_log(&format!("bg-{round}"), now_ms() - 200 * DAY_MS)])
                    .await
                    .unwrap();
                run_rotation_round(&pool, RETENTION_DAYS).await.unwrap();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };

    for iteration in 0..150 {
        let logs = store
            .query_logs(LogFilter {
                limit: 500,
                ..Default::default()
            })
            .await
            .unwrap_or_else(|e| {
                panic!("query_logs must never fail while views are rebuilt (iteration {iteration}): {e}")
            });
        assert_eq!(
            logs.len(),
            IN_WINDOW,
            "iteration {iteration}: the window must hold exactly the in-window rows"
        );
        for log in &logs {
            assert!(
                log.timestamp.timestamp_millis() >= window_start,
                "iteration {iteration}: stale full-history row leaked into the hot view: {}",
                log.timestamp
            );
        }
        assert_eq!(
            store.count_logs(LogFilter::default()).await.unwrap(),
            IN_WINDOW as u64,
            "iteration {iteration}: count_logs must agree with query_logs"
        );
        tokio::task::yield_now().await;
    }

    background.await.expect("rotation task panicked");
    assert_eq!(
        cold_tables(&pool).await.len(),
        ROUNDS + 1,
        "the settle round plus one real RENAME per background round"
    );
    assert_eq!(
        count(&pool, ALL_VIEW).await,
        (IN_WINDOW + EXPIRED + ROUNDS) as i64,
        "in-window rows, the expired batch and one expired row per round"
    );
}

// ─── 15. retention = 0 and hot reload ────────────────────────────────

/// `call_log_retention_days = 0` is a full disable: no rotation, no index
/// retirement, and the full-history view shape is the terminal state. A later
/// round with `90` takes effect immediately (the round re-reads the config).
#[tokio::test]
async fn rotation_respects_retention_zero_and_reload() {
    let env = TempDb::new("retention-zero");
    let pool = open_analytics_pool(&env.dsn()).await.unwrap();
    let store = SqliteLogStore::from_pool(pool.clone()).await.unwrap();

    let now = now_ms();
    let logs: Vec<CallLog> = (0..10)
        .map(|i| make_log(&format!("old-{i}"), now - 200 * DAY_MS))
        .collect();
    insert_batches(&store, &logs).await;

    // A synthetic cold table with a manually added index: the disable path
    // must not sweep it (retire is part of the round, and 0 skips the round).
    let synthetic = "call_logs_arch_20200101_000000";
    create_legacy_cold(&pool, synthetic).await;
    insert_legacy(&pool, synthetic, "cold", now - 200 * DAY_MS).await;
    sqlx::query(&format!(
        "CREATE INDEX \"{synthetic}_ts\" ON \"{synthetic}\" (timestamp)"
    ))
    .execute(&pool)
    .await
    .unwrap();
    refresh_views(&pool, None).await.unwrap();
    assert_eq!(count(&pool, HOT_VIEW).await, 11, "full-history shape");

    // retention = 0: the round returns before touching anything.
    assert_eq!(
        run_rotation_round(&pool, 0).await.unwrap(),
        RoundOutcome::Skipped
    );
    assert_eq!(
        cold_tables(&pool).await,
        vec![synthetic.to_string()],
        "no rotation may happen at retention 0"
    );
    assert_eq!(count(&pool, HOT_TABLE).await, 10);
    assert_eq!(
        count(&pool, HOT_VIEW).await,
        11,
        "0 keeps the full-history view (window never narrows)"
    );
    assert_eq!(
        index_names(&pool, synthetic).await,
        vec![format!("{synthetic}_ts")],
        "no index retirement at retention 0"
    );

    // 0 → 90 takes effect on the next round: rotation + retirement + narrowing.
    let outcome = run_rotation_round(&pool, RETENTION_DAYS).await.unwrap();
    let arch = outcome
        .rotated()
        .expect("a 90-day round must rotate the 200-day-old rows")
        .to_string();
    assert_eq!(count(&pool, HOT_TABLE).await, 0);
    assert_eq!(count(&pool, &arch).await, 10);
    assert!(
        index_names(&pool, synthetic).await.is_empty(),
        "the expired synthetic cold table loses its indexes"
    );
    assert_eq!(
        count(&pool, HOT_VIEW).await,
        0,
        "window narrowed to 90 days"
    );
    assert_eq!(
        count(&pool, ALL_VIEW).await,
        11,
        "history stays reachable through call_logs_all"
    );
}

// ─── 16. Legacy stats after rotation ─────────────────────────────────

/// The legacy `/v1/analytics/*` surface reads `call_logs_hot`, so after a
/// rotation its "all time" numbers collapse to the hot window: old buckets
/// read zero and totals only count in-window rows. This is the declared
/// hot-window semantics, not a regression.
#[tokio::test]
async fn hot_window_semantics_of_legacy_stats_after_rotation() {
    let env = TempDb::new("stats-window");
    let pool = open_analytics_pool(&env.dsn()).await.unwrap();
    let store = SqliteLogStore::from_pool(pool.clone()).await.unwrap();

    let now = now_ms();
    let recent: Vec<CallLog> = (0..5).map(|_| make_log("gpt-4o", now - DAY_MS)).collect();
    let old: Vec<CallLog> = (0..5)
        .map(|_| make_log("gpt-4o", now - 200 * DAY_MS))
        .collect();
    store.insert_batch(&recent).await.unwrap();
    store.insert_batch(&old).await.unwrap();

    // Before the round the fallback view is full-history: 10 calls.
    let before = store.query_stats(StatsFilter::default()).await.unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].call_count, 10);

    assert!(
        run_rotation_round(&pool, RETENTION_DAYS)
            .await
            .unwrap()
            .rotated()
            .is_some(),
        "the 200-day-old rows must rotate"
    );

    // "All time" now means the hot window: 5 calls, 150 tokens.
    let after = store.query_stats(StatsFilter::default()).await.unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].call_count, 5);
    assert_eq!(after[0].total_prompt_tokens, 50);
    assert_eq!(after[0].total_completion_tokens, 100);
    assert_eq!(after[0].total_tokens, 150);

    // A range that only covers the rotated-out period reads empty.
    let old_range = StatsFilter {
        from: Some(Utc.timestamp_millis_opt(now - 400 * DAY_MS).unwrap()),
        to: Some(Utc.timestamp_millis_opt(now - 100 * DAY_MS).unwrap()),
        ..Default::default()
    };
    assert!(
        store.query_stats(old_range).await.unwrap().is_empty(),
        "old buckets read zero after rotation (hot-window semantics)"
    );

    // The trend query only returns in-window buckets.
    let window_start = now_ms() - RETENTION_DAYS as i64 * DAY_MS;
    let trend = store
        .query_trend(TrendFilter {
            provider: None,
            bucket_seconds: 86_400,
            from: Utc.timestamp_millis_opt(now - 400 * DAY_MS).unwrap(),
            to: None,
        })
        .await
        .unwrap();
    let trend_total: i64 = trend.iter().map(|row| row.call_count).sum();
    assert_eq!(trend_total, 5, "trend counts only the hot window");
    assert!(
        trend.iter().all(|row| row.bucket >= window_start - DAY_MS),
        "no bucket older than the window may survive: {trend:?}"
    );

    // Paginated log reads follow the same view.
    assert_eq!(store.count_logs(LogFilter::default()).await.unwrap(), 5);
    let logs = store
        .query_logs(LogFilter {
            limit: 500,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(logs.len(), 5);
    assert!(
        logs.iter()
            .all(|log| log.timestamp.timestamp_millis() >= window_start)
    );

    // The rotated rows are not gone — they are one view away.
    assert_eq!(count(&pool, ALL_VIEW).await, 10);
}
