//! 1h rollup engine (stage 3): persistent hour-bucket state machine and
//! per-bucket recompute over the permanent fact tables.
//!
//! [`RollupEngine`] is a **single-worker** batch processor. Facts
//! (`request_results` / `target_attempts`) are append-only, and
//! `rollup_bucket_state` drives which UTC hours need (re)aggregation:
//!
//! ```text
//! [*] -> pending       start-up registration — only missing / dirty /
//!                      failed / expired-processing hours are registered,
//!                      never computed here
//! pending/failed -> processing   conditional claim (lease, attempt + 1)
//! processing -> completed        one transaction with all rollup tables
//! processing -> failed           compute error (tx rolled back, retryable)
//! processing -> pending          lease expiry (crash recovery)
//! completed -> pending           late fact (mark_dirty) re-registered
//! ```
//!
//! One [`RollupEngine::run_once`] claims at most [`MAX_BUCKETS_PER_RUN`]
//! buckets and recomputes each bucket's `rollup_global_hourly` /
//! `rollup_route_hourly` / `rollup_target_hourly` / `rollup_latency_hist`
//! rows from the raw facts, committing them **in one transaction** — a
//! bucket is all-or-nothing (a failed compute rolls back to the previous
//! good state and marks the bucket `failed` for retry), and reruns are
//! idempotent (full replace, never accumulate).
//!
//! Aggregation semantics mirror `query.rs` (the exact-detail kernel):
//! - route level reads `request_results` final results; target level reads
//!   `target_attempts` actual attempts;
//! - token / cache columns only aggregate over final-success samples;
//!   `cached_tokens = NULL` means **unknown** (counted in
//!   `cached_known_count` only when non-NULL; `cached_hit_count` requires
//!   `> 0`), and NULL latency never enters sums or histograms — nothing
//!   is ever coerced to 0;
//! - route e2e latency population = `final_status IN (success,
//!   model_failure, all_disabled, client_cancelled)` (requests that
//!   entered scheduling); target e2e population = every actual attempt;
//!   ttft / upstream are the non-NULL subsets of the same rows;
//! - latencies are bucketed into a sparse log-scale histogram:
//!   `bucket_idx = 0` is the 0ms bucket, `1..N` are 1.01x log buckets
//!   starting at 1ms, values beyond capacity go to [`OVERFLOW_BUCKET`],
//!   NULL never enters.
//!
//! Global histogram rows are materialized under `scope='route'` with
//! `owner_id = GLOBAL_HIST_OWNER` — the migration's `scope` CHECK only
//! allows `route | target`, and a real `route_id` (UUID) can never
//! collide with the sentinel. Because e2e-population rows always carry a
//! route (writer-enforced), the global histogram equals the sum of the
//! per-route histograms of the same hour; the read helpers keep the
//! sentinel out of per-route merges.
//!
//! Everything here uses SQLx **runtime queries** (no compile-time `query!`
//! macros). The pool must have foreign keys enabled and the analytics
//! migrations applied (see [`crate::open_analytics_pool`]); file-backed
//! pools additionally run WAL journal mode with `synchronous = NORMAL`,
//! and the busy timeout is pinned explicitly to 5s at pool construction,
//! so rollup write transactions waiting on a fact-writer lock block
//! instead of failing with SQLITE_BUSY. The bucket recompute transaction
//! itself starts with `BEGIN IMMEDIATE` (it is a write transaction
//! upgrading a deferred `BEGIN` could race under concurrent writers).
//!
//! # Scheduler contract (not implemented here)
//!
//! Call [`RollupEngine::register_missing_hours`] once at startup, then
//! [`RollupEngine::run_once`] hourly (or after any [`RollupEngine::mark_dirty`]).
//! Single worker only: concurrent `run_once` calls are not supported (the
//! lease still makes a crash recoverable).

use sag_core::AppError;
use sqlx::sqlite::SqlitePool;
use std::collections::{BTreeMap, HashMap};

/// Milliseconds per UTC hour (`hour = completed_at / HOUR_MS`, floor).
pub const HOUR_MS: i64 = 3_600_000;

/// Maximum hour buckets claimed per run (spec: 每轮最多 24 个小时桶).
pub const MAX_BUCKETS_PER_RUN: i64 = 24;

/// Lease timeout for a `processing` bucket. A bucket whose
/// `processing_started_at` is older than this is considered crashed and
/// becomes claimable again.
pub const PROCESSING_LEASE_TIMEOUT_MS: i64 = 30 * 60 * 1000;

/// Number of regular histogram buckets: `0` (the 0ms bucket) plus log
/// buckets `1..MAX_HISTOGRAM_BUCKETS` (1.01x growth from 1ms). Values
/// whose index would exceed this capacity go to [`OVERFLOW_BUCKET`].
pub const MAX_HISTOGRAM_BUCKETS: i64 = 4096;

/// `bucket_idx` of the overflow bucket (samples beyond the fixed capacity).
pub const OVERFLOW_BUCKET: i64 = -1;

/// Log-scale base of the latency histogram (≈1% value error per bucket).
pub const HISTOGRAM_BASE: f64 = 1.01;

/// `owner_id` of the materialized global histogram rows (`scope='route'`).
/// The schema CHECK only allows `scope IN ('route','target')`, so the
/// global histogram is expressed with the route scope plus this sentinel
/// owner; UUID route ids can never collide with it.
pub const GLOBAL_HIST_OWNER: &str = "__global__";

/// `final_status` values that enter the route-level e2e latency population
/// (requests that actually entered scheduling; `no_route` / `client_error`
/// are excluded — spec 延迟指标).
const E2E_STATUSES: &str = "'success', 'model_failure', 'all_disabled', 'client_cancelled'";

/// State of one hour bucket in `rollup_bucket_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketState {
    /// Registered, waiting to be claimed.
    Pending,
    /// Claimed by a run; lease may expire (crash recovery).
    Processing,
    /// Successfully aggregated; only recomputed when marked dirty.
    Completed,
    /// Last compute failed; retryable.
    Failed,
}

impl BucketState {
    fn from_db(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "processing" => Self::Processing,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            other => {
                tracing::warn!(
                    state = other,
                    "unknown rollup bucket state in DB; treating as pending"
                );
                Self::Pending
            }
        }
    }
}

/// Persisted state of one hour bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketStatus {
    pub hour: i64,
    pub state: BucketState,
    /// Completed bucket received a late fact (`mark_dirty`).
    pub dirty: bool,
    /// Idempotent retry counter (incremented at each claim).
    pub attempt: i64,
    pub processing_started_at: Option<i64>,
    pub updated_at: i64,
}

/// Raw `rollup_bucket_state` row as stored.
#[derive(sqlx::FromRow)]
struct BucketStateRow {
    hour: i64,
    state: String,
    dirty: i64,
    attempt: i64,
    processing_started_at: Option<i64>,
    updated_at: i64,
}

impl BucketStatus {
    fn from_row(row: BucketStateRow) -> Self {
        Self {
            hour: row.hour,
            state: BucketState::from_db(&row.state),
            dirty: row.dirty != 0,
            attempt: row.attempt,
            processing_started_at: row.processing_started_at,
            updated_at: row.updated_at,
        }
    }
}

/// One `rollup_global_hourly` row.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct GlobalRollupRow {
    pub hour: i64,
    pub requests: i64,
    pub success: i64,
    pub client_error: i64,
    pub no_route: i64,
    pub all_disabled: i64,
    pub model_failure: i64,
    pub client_cancelled: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub cached_tokens: i64,
    pub cache_creation_tokens: i64,
    pub cached_known_count: i64,
    pub cached_hit_count: i64,
    pub prompt_tokens_cached_known: i64,
    pub e2e_count: i64,
    pub e2e_sum_ms: i64,
    pub ttft_count: i64,
    pub ttft_sum_ms: i64,
    pub upstream_count: i64,
    pub upstream_sum_ms: i64,
}

/// One `rollup_route_hourly` row (matched routes only).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct RouteRollupRow {
    pub hour: i64,
    pub route_id: String,
    pub requests: i64,
    pub success: i64,
    pub model_failure: i64,
    pub all_disabled: i64,
    pub client_cancelled: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub cached_tokens: i64,
    pub cached_known_count: i64,
    pub cached_hit_count: i64,
    pub prompt_tokens_cached_known: i64,
    pub e2e_count: i64,
    pub e2e_sum_ms: i64,
    pub ttft_count: i64,
    pub ttft_sum_ms: i64,
    pub upstream_count: i64,
    pub upstream_sum_ms: i64,
}

/// One `rollup_target_hourly` row (per target of actual attempts).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct TargetRollupRow {
    pub hour: i64,
    pub target_id: String,
    pub attempts: i64,
    pub success: i64,
    pub failure: i64,
    pub cancelled: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub cached_tokens: i64,
    pub cached_known_count: i64,
    pub cached_hit_count: i64,
    pub prompt_tokens_cached_known: i64,
    pub e2e_count: i64,
    pub e2e_sum_ms: i64,
    pub ttft_count: i64,
    pub ttft_sum_ms: i64,
    pub upstream_count: i64,
    pub upstream_sum_ms: i64,
}

/// One sparse `rollup_latency_hist` row.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct HistogramRow {
    pub hour: i64,
    pub scope: String,
    pub owner_id: String,
    pub dim: String,
    pub bucket_idx: i64,
    pub count: i64,
}

/// Merged histogram counts for one dimension (see
/// [`merge_histogram_buckets`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistogramBins {
    /// `bucket_idx -> count` for regular buckets (`0..MAX_HISTOGRAM_BUCKETS`).
    pub counts: BTreeMap<i64, u64>,
    /// Samples beyond the fixed capacity (`bucket_idx = OVERFLOW_BUCKET`).
    pub overflow: u64,
}

/// Outcome of one [`RollupEngine::run_once`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RollupRunReport {
    pub claimed: usize,
    pub completed: usize,
    pub failed: usize,
}

/// The 1h rollup batch processor over a shared SQLite pool.
///
/// Single worker: claim → recompute per bucket (one transaction per
/// bucket, all rollup tables together) → completed/failed.
pub struct RollupEngine {
    pool: SqlitePool,
}

impl RollupEngine {
    /// Wrap a ready pool (migrations applied, foreign keys enabled).
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Open a pool on `dsn` with the standard analytics connect options
    /// (see [`crate::open_analytics_pool`]) and wrap it.
    pub async fn open(dsn: &str) -> Result<Self, sqlx::Error> {
        Ok(Self::new(crate::open_analytics_pool(dsn).await?))
    }

    /// Access to the underlying pool (e.g. for sharing or direct
    /// verification in tests).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Register hours in `[from_ms, to_ms)` that need rollup: hours with
    /// facts but no state row, completed-but-dirty hours, `failed` hours
    /// and `processing` hours whose lease expired are all (re)registered as
    /// `pending`. Hours without facts are never created, and already-pending
    /// or completed-and-clean hours are left untouched. **Registration only
    /// registers — it never computes.**
    ///
    /// Returns the hours that were newly registered or restored (ascending).
    pub async fn register_missing_hours(
        &self,
        from_ms: i64,
        to_ms: i64,
    ) -> sag_core::Result<Vec<i64>> {
        if from_ms >= to_ms {
            return Ok(Vec::new());
        }
        // Distinct UTC hours that actually contain facts (left-closed
        // right-open). Hours without facts produce no state rows.
        let hours: Vec<i64> = sqlx::query_scalar(
            "SELECT DISTINCT (completed_at / ?) * ? FROM request_results \
             WHERE completed_at >= ? AND completed_at < ? ORDER BY 1",
        )
        .bind(HOUR_MS)
        .bind(HOUR_MS)
        .bind(from_ms)
        .bind(to_ms)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Internal(format!("rollup scan fact hours: {e}")))?;

        if hours.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders = vec!["?"; hours.len()].join(",");
        let sql = format!(
            "SELECT hour, state, dirty, attempt, processing_started_at, updated_at \
             FROM rollup_bucket_state WHERE hour IN ({placeholders})"
        );
        let mut q = sqlx::query_as::<_, BucketStateRow>(&sql);
        for h in &hours {
            q = q.bind(*h);
        }
        let existing: Vec<BucketStateRow> = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::Internal(format!("rollup read bucket state: {e}")))?;
        let by_hour: HashMap<i64, BucketStateRow> =
            existing.into_iter().map(|r| (r.hour, r)).collect();

        let now = now_ms();
        let lease_deadline = now - PROCESSING_LEASE_TIMEOUT_MS;
        let mut registered = Vec::new();
        for hour in &hours {
            match by_hour.get(hour) {
                None => {
                    sqlx::query(
                        "INSERT INTO rollup_bucket_state \
                         (hour, state, dirty, attempt, processing_started_at, updated_at) \
                         VALUES (?, 'pending', 0, 0, NULL, ?)",
                    )
                    .bind(hour)
                    .bind(now)
                    .execute(&self.pool)
                    .await
                    .map_err(|e| AppError::Internal(format!("rollup register hour={hour}: {e}")))?;
                    registered.push(*hour);
                }
                Some(row) => {
                    let recover = match row.state.as_str() {
                        "pending" => false,
                        "completed" => row.dirty != 0,
                        "failed" => true,
                        "processing" => row
                            .processing_started_at
                            .map(|ts| ts < lease_deadline)
                            .unwrap_or(false),
                        other => {
                            tracing::warn!(
                                hour,
                                state = other,
                                "unknown rollup bucket state; leaving as-is"
                            );
                            false
                        }
                    };
                    if recover {
                        sqlx::query(
                            "UPDATE rollup_bucket_state SET state = 'pending', dirty = 0, updated_at = ? \
                             WHERE hour = ?",
                        )
                        .bind(now)
                        .bind(hour)
                        .execute(&self.pool)
                        .await
                        .map_err(|e| AppError::Internal(format!("rollup restore hour={hour}: {e}")))?;
                        registered.push(*hour);
                    }
                }
            }
        }
        tracing::info!(
            registered = registered.len(),
            "rollup registered missing/dirty hours"
        );
        Ok(registered)
    }

    /// Claim up to [`MAX_BUCKETS_PER_RUN`] buckets: `pending` / `failed`
    /// buckets, plus `processing` buckets whose lease expired (crash
    /// recovery). Claim sets `state='processing'` and stamps
    /// `processing_started_at`. Returns the claimed hours (ascending).
    ///
    /// Per the spec state machine, the `attempt` counter is **not** bumped
    /// here — it counts failures (`processing --> failed: 事务失败
    /// (attempt+1)`, see `mark_failed`); a bucket that succeeds on its
    /// first claim keeps `attempt = 0`.
    ///
    /// Safe only for a single worker: the read-back matches the lease
    /// timestamp written by this call, so a concurrent claimer would
    /// produce wrong results.
    pub async fn claim_buckets(&self) -> sag_core::Result<Vec<i64>> {
        let now = now_ms();
        let lease_deadline = now - PROCESSING_LEASE_TIMEOUT_MS;
        sqlx::query(
            r#"
            UPDATE rollup_bucket_state
            SET state = 'processing',
                processing_started_at = ?, updated_at = ?
            WHERE hour IN (
                SELECT hour FROM rollup_bucket_state
                WHERE state IN ('pending', 'failed')
                   OR (state = 'processing'
                       AND processing_started_at IS NOT NULL
                       AND processing_started_at < ?)
                ORDER BY hour
                LIMIT ?
            )
            "#,
        )
        .bind(now)
        .bind(now)
        .bind(lease_deadline)
        .bind(MAX_BUCKETS_PER_RUN)
        .execute(&self.pool)
        .await
        .map_err(|e| AppError::Internal(format!("rollup claim buckets: {e}")))?;

        let claimed: Vec<i64> = sqlx::query_scalar(
            "SELECT hour FROM rollup_bucket_state \
             WHERE state = 'processing' AND processing_started_at = ? ORDER BY hour",
        )
        .bind(now)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Internal(format!("rollup read claimed buckets: {e}")))?;
        Ok(claimed)
    }

    /// Run one batch round: claim up to [`MAX_BUCKETS_PER_RUN`] buckets,
    /// recompute each from the raw facts (one transaction per bucket) and
    /// mark it `completed`; a failed compute rolls back and marks the
    /// bucket `failed` (retryable). Missing / dirty buckets must be
    /// registered first (see [`RollupEngine::register_missing_hours`]).
    pub async fn run_once(&self) -> sag_core::Result<RollupRunReport> {
        let claimed = self.claim_buckets().await?;
        let mut report = RollupRunReport {
            claimed: claimed.len(),
            completed: 0,
            failed: 0,
        };
        if claimed.is_empty() {
            tracing::debug!("rollup run: nothing to claim");
            return Ok(report);
        }
        for hour in claimed {
            match self.compute_bucket(hour).await {
                Ok(()) => {
                    report.completed += 1;
                    tracing::debug!(hour, "rollup bucket completed");
                }
                Err(e) => {
                    report.failed += 1;
                    tracing::warn!(hour, error = %e, "rollup bucket failed; marking failed for retry");
                    if let Err(mark_err) = self.mark_failed(hour).await {
                        tracing::warn!(hour, error = %mark_err, "failed to mark rollup bucket failed");
                    }
                }
            }
        }
        tracing::info!(
            claimed = report.claimed,
            completed = report.completed,
            failed = report.failed,
            "rollup run finished"
        );
        Ok(report)
    }

    /// Mark a completed bucket as dirty (a late fact arrived for it). The
    /// next `register_missing_hours` + `run_once` cycle recomputes it.
    /// Buckets that are not `completed` are left untouched — they will pick
    /// the late fact up anyway when they are computed. No state row yet
    /// means the hour was never registered (no facts at scan time); the
    /// startup scan will create it.
    pub async fn mark_dirty(&self, hour: i64) -> sag_core::Result<()> {
        let now = now_ms();
        let res = sqlx::query(
            "UPDATE rollup_bucket_state SET dirty = 1, updated_at = ? \
             WHERE hour = ? AND state = 'completed'",
        )
        .bind(now)
        .bind(hour)
        .execute(&self.pool)
        .await
        .map_err(|e| AppError::Internal(format!("rollup mark_dirty hour={hour}: {e}")))?;
        if res.rows_affected() == 0 {
            tracing::debug!(hour, "mark_dirty: bucket not completed (or unknown), no-op");
        }
        Ok(())
    }

    /// Current persisted state of one hour bucket (or `None` if never
    /// registered).
    pub async fn bucket_status(&self, hour: i64) -> sag_core::Result<Option<BucketStatus>> {
        let row: Option<BucketStateRow> = sqlx::query_as::<_, BucketStateRow>(
            "SELECT hour, state, dirty, attempt, processing_started_at, updated_at \
             FROM rollup_bucket_state WHERE hour = ?",
        )
        .bind(hour)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| AppError::Internal(format!("rollup bucket_status hour={hour}: {e}")))?;
        Ok(row.map(BucketStatus::from_row))
    }

    /// Hours in `[from_ms, to_ms)` that are `completed` and not dirty — the
    /// hours a mixed query may answer from rollup rows instead of details.
    pub async fn completed_hours(&self, from_ms: i64, to_ms: i64) -> sag_core::Result<Vec<i64>> {
        if from_ms >= to_ms {
            return Ok(Vec::new());
        }
        let hours: Vec<i64> = sqlx::query_scalar(
            "SELECT hour FROM rollup_bucket_state \
             WHERE state = 'completed' AND dirty = 0 AND hour >= ? AND hour < ? ORDER BY hour",
        )
        .bind(from_ms)
        .bind(to_ms)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Internal(format!("rollup completed_hours: {e}")))?;
        Ok(hours)
    }

    /// Read `rollup_global_hourly` rows for the given hours.
    pub async fn global_rows(&self, hours: &[i64]) -> sag_core::Result<Vec<GlobalRollupRow>> {
        if hours.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; hours.len()].join(",");
        let sql = format!(
            "SELECT hour, requests, success, client_error, no_route, all_disabled, \
             model_failure, client_cancelled, prompt_tokens, completion_tokens, total_tokens, \
             cached_tokens, cache_creation_tokens, cached_known_count, cached_hit_count, \
             prompt_tokens_cached_known, e2e_count, e2e_sum_ms, ttft_count, ttft_sum_ms, \
             upstream_count, upstream_sum_ms \
             FROM rollup_global_hourly WHERE hour IN ({placeholders}) ORDER BY hour"
        );
        let mut q = sqlx::query_as::<_, GlobalRollupRow>(&sql);
        for h in hours {
            q = q.bind(*h);
        }
        q.fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::Internal(format!("rollup global_rows: {e}")))
    }

    /// Read `rollup_route_hourly` rows for the given hours (all routes).
    pub async fn route_rows(&self, hours: &[i64]) -> sag_core::Result<Vec<RouteRollupRow>> {
        if hours.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; hours.len()].join(",");
        let sql = format!(
            "SELECT hour, route_id, requests, success, model_failure, all_disabled, \
             client_cancelled, prompt_tokens, completion_tokens, total_tokens, cached_tokens, \
             cached_known_count, cached_hit_count, prompt_tokens_cached_known, e2e_count, \
             e2e_sum_ms, ttft_count, ttft_sum_ms, upstream_count, upstream_sum_ms \
             FROM rollup_route_hourly WHERE hour IN ({placeholders}) ORDER BY hour, route_id"
        );
        let mut q = sqlx::query_as::<_, RouteRollupRow>(&sql);
        for h in hours {
            q = q.bind(*h);
        }
        q.fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::Internal(format!("rollup route_rows: {e}")))
    }

    /// Read `rollup_target_hourly` rows for the given hours (all targets).
    pub async fn target_rows(&self, hours: &[i64]) -> sag_core::Result<Vec<TargetRollupRow>> {
        if hours.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = vec!["?"; hours.len()].join(",");
        let sql = format!(
            "SELECT hour, target_id, attempts, success, failure, cancelled, prompt_tokens, \
             completion_tokens, total_tokens, cached_tokens, cached_known_count, \
             cached_hit_count, prompt_tokens_cached_known, e2e_count, e2e_sum_ms, ttft_count, \
             ttft_sum_ms, upstream_count, upstream_sum_ms \
             FROM rollup_target_hourly WHERE hour IN ({placeholders}) ORDER BY hour, target_id"
        );
        let mut q = sqlx::query_as::<_, TargetRollupRow>(&sql);
        for h in hours {
            q = q.bind(*h);
        }
        q.fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::Internal(format!("rollup target_rows: {e}")))
    }

    /// Read `rollup_latency_hist` rows for the given hours, optionally
    /// filtered to a set of owner ids. `scope` is `'route'` or `'target'`
    /// (raw filter, as stored). The global sentinel row is only returned
    /// when explicitly requested via `owner_ids` (or via
    /// [`RollupEngine::global_histogram_rows`]); per-route merges should
    /// pass explicit `route_id`s.
    pub async fn histogram_rows(
        &self,
        hours: &[i64],
        scope: &str,
        owner_ids: Option<&[&str]>,
    ) -> sag_core::Result<Vec<HistogramRow>> {
        if hours.is_empty() || owner_ids.is_some_and(|ids| ids.is_empty()) {
            return Ok(Vec::new());
        }
        let mut sql = String::from(
            "SELECT hour, scope, owner_id, dim, bucket_idx, count \
             FROM rollup_latency_hist WHERE scope = ? AND hour IN (",
        );
        sql.push_str(&vec!["?"; hours.len()].join(","));
        sql.push(')');
        if let Some(ids) = owner_ids {
            sql.push_str(" AND owner_id IN (");
            sql.push_str(&vec!["?"; ids.len()].join(","));
            sql.push(')');
        }
        sql.push_str(" ORDER BY hour, owner_id, dim, bucket_idx");

        let mut q = sqlx::query_as::<_, HistogramRow>(&sql);
        q = q.bind(scope);
        for h in hours {
            q = q.bind(*h);
        }
        if let Some(ids) = owner_ids {
            for id in ids {
                q = q.bind(*id);
            }
        }
        q.fetch_all(&self.pool)
            .await
            .map_err(|e| AppError::Internal(format!("rollup histogram_rows: {e}")))
    }

    /// Read the materialized global histogram rows (scope `'route'`,
    /// `owner_id = GLOBAL_HIST_OWNER`) for the given hours.
    pub async fn global_histogram_rows(
        &self,
        hours: &[i64],
    ) -> sag_core::Result<Vec<HistogramRow>> {
        let owner = [GLOBAL_HIST_OWNER];
        self.histogram_rows(hours, "route", Some(&owner)).await
    }

    /// Recompute one bucket from the raw facts in a single transaction:
    /// global + route + target aggregates and the sparse latency
    /// histograms, then `completed` (dirty cleared). Any error rolls back
    /// everything and leaves the previous rollup rows untouched.
    async fn compute_bucket(&self, hour: i64) -> sag_core::Result<()> {
        let mut tx = begin_compute(&self.pool, hour).await?;

        let global = global_agg(&mut tx, hour).await?;
        let routes = route_aggs(&mut tx, hour).await?;
        let targets = target_aggs(&mut tx, hour).await?;
        let hist_rows = collect_histograms(&mut tx, hour).await?;

        // Full replace: recompute is idempotent and never accumulates.
        replace_global_hourly(&mut tx, hour, &global).await?;
        replace_route_hourly(&mut tx, hour, &routes).await?;
        replace_target_hourly(&mut tx, hour, &targets).await?;
        replace_latency_hist(&mut tx, hour, &hist_rows).await?;

        mark_bucket_completed(&mut tx, hour).await?;
        commit_compute(tx, hour).await
    }

    /// Mark a claimed bucket `failed` (retryable). Called outside the
    /// bucket transaction so a compute failure can still persist its
    /// failure state. Bumps the `attempt` counter — per the spec state
    /// machine the counter counts failures (`processing --> failed:
    /// 事务失败（attempt+1）`), so a bucket that never failed keeps
    /// `attempt = 0` even after successful claims/computes.
    async fn mark_failed(&self, hour: i64) -> sag_core::Result<()> {
        sqlx::query(
            "UPDATE rollup_bucket_state SET state = 'failed', attempt = attempt + 1, updated_at = ? \
             WHERE hour = ?",
        )
        .bind(now_ms())
        .bind(hour)
        .execute(&self.pool)
        .await
        .map_err(|e| AppError::Internal(format!("rollup mark_failed hour={hour}: {e}")))?;
        Ok(())
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Open the single recompute transaction for one bucket.
///
/// `BEGIN IMMEDIATE`: take the write lock up front instead of a deferred
/// BEGIN that upgrades mid-transaction (which can fail with
/// SQLITE_BUSY_SNAPSHOT under concurrent fact writes in WAL mode). The
/// busy_timeout covers the wait.
async fn begin_compute(
    pool: &SqlitePool,
    hour: i64,
) -> sag_core::Result<sqlx::Transaction<'static, sqlx::Sqlite>> {
    pool.begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(|e| AppError::Internal(format!("rollup begin hour={hour}: {e}")))
}

/// Full replace of `rollup_global_hourly` for one hour (DELETE + INSERT).
async fn replace_global_hourly(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    hour: i64,
    global: &GlobalRollupRow,
) -> sag_core::Result<()> {
    sqlx::query("DELETE FROM rollup_global_hourly WHERE hour = ?")
        .bind(hour)
        .execute(&mut **tx)
        .await
        .map_err(|e| AppError::Internal(format!("rollup delete global hour={hour}: {e}")))?;
    sqlx::query(
        r#"
        INSERT INTO rollup_global_hourly (
            hour, requests, success, client_error, no_route, all_disabled, model_failure,
            client_cancelled, prompt_tokens, completion_tokens, total_tokens, cached_tokens,
            cache_creation_tokens, cached_known_count, cached_hit_count,
            prompt_tokens_cached_known, e2e_count, e2e_sum_ms, ttft_count, ttft_sum_ms,
            upstream_count, upstream_sum_ms
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(hour)
    .bind(global.requests)
    .bind(global.success)
    .bind(global.client_error)
    .bind(global.no_route)
    .bind(global.all_disabled)
    .bind(global.model_failure)
    .bind(global.client_cancelled)
    .bind(global.prompt_tokens)
    .bind(global.completion_tokens)
    .bind(global.total_tokens)
    .bind(global.cached_tokens)
    .bind(global.cache_creation_tokens)
    .bind(global.cached_known_count)
    .bind(global.cached_hit_count)
    .bind(global.prompt_tokens_cached_known)
    .bind(global.e2e_count)
    .bind(global.e2e_sum_ms)
    .bind(global.ttft_count)
    .bind(global.ttft_sum_ms)
    .bind(global.upstream_count)
    .bind(global.upstream_sum_ms)
    .execute(&mut **tx)
    .await
    .map_err(|e| AppError::Internal(format!("rollup upsert global hour={hour}: {e}")))?;
    Ok(())
}

/// Full replace of `rollup_route_hourly` for one hour (DELETE + loop INSERT).
async fn replace_route_hourly(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    hour: i64,
    routes: &[RouteRollupRow],
) -> sag_core::Result<()> {
    sqlx::query("DELETE FROM rollup_route_hourly WHERE hour = ?")
        .bind(hour)
        .execute(&mut **tx)
        .await
        .map_err(|e| AppError::Internal(format!("rollup delete route hour={hour}: {e}")))?;
    for r in routes {
        sqlx::query(
            r#"
            INSERT INTO rollup_route_hourly (
                hour, route_id, requests, success, model_failure, all_disabled,
                client_cancelled, prompt_tokens, completion_tokens, total_tokens,
                cached_tokens, cached_known_count, cached_hit_count,
                prompt_tokens_cached_known, e2e_count, e2e_sum_ms, ttft_count, ttft_sum_ms,
                upstream_count, upstream_sum_ms
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(hour)
        .bind(&r.route_id)
        .bind(r.requests)
        .bind(r.success)
        .bind(r.model_failure)
        .bind(r.all_disabled)
        .bind(r.client_cancelled)
        .bind(r.prompt_tokens)
        .bind(r.completion_tokens)
        .bind(r.total_tokens)
        .bind(r.cached_tokens)
        .bind(r.cached_known_count)
        .bind(r.cached_hit_count)
        .bind(r.prompt_tokens_cached_known)
        .bind(r.e2e_count)
        .bind(r.e2e_sum_ms)
        .bind(r.ttft_count)
        .bind(r.ttft_sum_ms)
        .bind(r.upstream_count)
        .bind(r.upstream_sum_ms)
        .execute(&mut **tx)
        .await
        .map_err(|e| AppError::Internal(format!("rollup upsert route hour={hour}: {e}")))?;
    }
    Ok(())
}

/// Full replace of `rollup_target_hourly` for one hour (DELETE + loop INSERT).
async fn replace_target_hourly(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    hour: i64,
    targets: &[TargetRollupRow],
) -> sag_core::Result<()> {
    sqlx::query("DELETE FROM rollup_target_hourly WHERE hour = ?")
        .bind(hour)
        .execute(&mut **tx)
        .await
        .map_err(|e| AppError::Internal(format!("rollup delete target hour={hour}: {e}")))?;
    for t in targets {
        sqlx::query(
            r#"
            INSERT INTO rollup_target_hourly (
                hour, target_id, attempts, success, failure, cancelled, prompt_tokens,
                completion_tokens, total_tokens, cached_tokens, cached_known_count,
                cached_hit_count, prompt_tokens_cached_known, e2e_count, e2e_sum_ms,
                ttft_count, ttft_sum_ms, upstream_count, upstream_sum_ms
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(hour)
        .bind(&t.target_id)
        .bind(t.attempts)
        .bind(t.success)
        .bind(t.failure)
        .bind(t.cancelled)
        .bind(t.prompt_tokens)
        .bind(t.completion_tokens)
        .bind(t.total_tokens)
        .bind(t.cached_tokens)
        .bind(t.cached_known_count)
        .bind(t.cached_hit_count)
        .bind(t.prompt_tokens_cached_known)
        .bind(t.e2e_count)
        .bind(t.e2e_sum_ms)
        .bind(t.ttft_count)
        .bind(t.ttft_sum_ms)
        .bind(t.upstream_count)
        .bind(t.upstream_sum_ms)
        .execute(&mut **tx)
        .await
        .map_err(|e| AppError::Internal(format!("rollup upsert target hour={hour}: {e}")))?;
    }
    Ok(())
}

/// Full replace of `rollup_latency_hist` for one hour (DELETE + loop INSERT).
async fn replace_latency_hist(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    hour: i64,
    hist_rows: &[HistogramRow],
) -> sag_core::Result<()> {
    sqlx::query("DELETE FROM rollup_latency_hist WHERE hour = ?")
        .bind(hour)
        .execute(&mut **tx)
        .await
        .map_err(|e| AppError::Internal(format!("rollup delete hist hour={hour}: {e}")))?;
    for h in hist_rows {
        sqlx::query(
            "INSERT INTO rollup_latency_hist (hour, scope, owner_id, dim, bucket_idx, count) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(hour)
        .bind(&h.scope)
        .bind(&h.owner_id)
        .bind(&h.dim)
        .bind(h.bucket_idx)
        .bind(h.count)
        .execute(&mut **tx)
        .await
        .map_err(|e| AppError::Internal(format!("rollup upsert hist hour={hour}: {e}")))?;
    }
    Ok(())
}

/// Mark the bucket `completed` with `dirty` cleared, inside the compute
/// transaction.
async fn mark_bucket_completed(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    hour: i64,
) -> sag_core::Result<()> {
    sqlx::query(
        "UPDATE rollup_bucket_state SET state = 'completed', dirty = 0, updated_at = ? \
         WHERE hour = ?",
    )
    .bind(now_ms())
    .bind(hour)
    .execute(&mut **tx)
    .await
    .map_err(|e| AppError::Internal(format!("rollup mark completed hour={hour}: {e}")))?;
    Ok(())
}

/// Commit the bucket recompute transaction.
async fn commit_compute(
    tx: sqlx::Transaction<'_, sqlx::Sqlite>,
    hour: i64,
) -> sag_core::Result<()> {
    tx.commit()
        .await
        .map_err(|e| AppError::Internal(format!("rollup commit hour={hour}: {e}")))?;
    Ok(())
}

/// Global per-hour aggregate over `request_results` (one row, even for an
/// empty hour — all zeros).
async fn global_agg(
    conn: &mut sqlx::SqliteConnection,
    hour: i64,
) -> sag_core::Result<GlobalRollupRow> {
    let sql = format!(
        r#"
        SELECT
            ? AS hour,
            COUNT(*) AS requests,
            COALESCE(SUM(CASE WHEN final_status = 'success' THEN 1 ELSE 0 END), 0) AS success,
            COALESCE(SUM(CASE WHEN final_status = 'client_error' THEN 1 ELSE 0 END), 0) AS client_error,
            COALESCE(SUM(CASE WHEN final_status = 'no_route' THEN 1 ELSE 0 END), 0) AS no_route,
            COALESCE(SUM(CASE WHEN final_status = 'all_disabled' THEN 1 ELSE 0 END), 0) AS all_disabled,
            COALESCE(SUM(CASE WHEN final_status = 'model_failure' THEN 1 ELSE 0 END), 0) AS model_failure,
            COALESCE(SUM(CASE WHEN final_status = 'client_cancelled' THEN 1 ELSE 0 END), 0) AS client_cancelled,
            COALESCE(SUM(CASE WHEN final_status = 'success' THEN prompt_tokens END), 0) AS prompt_tokens,
            COALESCE(SUM(CASE WHEN final_status = 'success' THEN completion_tokens END), 0) AS completion_tokens,
            COALESCE(SUM(CASE WHEN final_status = 'success' THEN total_tokens END), 0) AS total_tokens,
            COALESCE(SUM(CASE WHEN final_status = 'success' AND cached_tokens IS NOT NULL THEN cached_tokens END), 0) AS cached_tokens,
            COALESCE(SUM(CASE WHEN final_status = 'success' AND cache_creation_tokens IS NOT NULL THEN cache_creation_tokens END), 0) AS cache_creation_tokens,
            COALESCE(SUM(CASE WHEN final_status = 'success' AND cached_tokens IS NOT NULL THEN 1 ELSE 0 END), 0) AS cached_known_count,
            COALESCE(SUM(CASE WHEN final_status = 'success' AND cached_tokens IS NOT NULL AND cached_tokens > 0 THEN 1 ELSE 0 END), 0) AS cached_hit_count,
            COALESCE(SUM(CASE WHEN final_status = 'success' AND cached_tokens IS NOT NULL THEN prompt_tokens END), 0) AS prompt_tokens_cached_known,
            COALESCE(SUM(CASE WHEN final_status IN ({E2E}) THEN 1 ELSE 0 END), 0) AS e2e_count,
            COALESCE(SUM(CASE WHEN final_status IN ({E2E}) THEN duration_ms END), 0) AS e2e_sum_ms,
            COALESCE(SUM(CASE WHEN final_status IN ({E2E}) AND ttft_ms IS NOT NULL THEN 1 ELSE 0 END), 0) AS ttft_count,
            COALESCE(SUM(CASE WHEN final_status IN ({E2E}) AND ttft_ms IS NOT NULL THEN ttft_ms END), 0) AS ttft_sum_ms,
            COALESCE(SUM(CASE WHEN final_status IN ({E2E}) AND upstream_duration_ms IS NOT NULL THEN 1 ELSE 0 END), 0) AS upstream_count,
            COALESCE(SUM(CASE WHEN final_status IN ({E2E}) AND upstream_duration_ms IS NOT NULL THEN upstream_duration_ms END), 0) AS upstream_sum_ms
        FROM request_results
        WHERE completed_at >= ? AND completed_at < ?
        "#,
        E2E = E2E_STATUSES
    );
    sqlx::query_as::<_, GlobalRollupRow>(&sql)
        .bind(hour)
        .bind(hour)
        .bind(hour + HOUR_MS)
        .fetch_one(&mut *conn)
        .await
        .map_err(|e| AppError::Internal(format!("rollup global agg hour={hour}: {e}")))
}

/// Per-route per-hour aggregate over `request_results` (matched routes
/// only; `client_error` / `no_route` never appear).
async fn route_aggs(
    conn: &mut sqlx::SqliteConnection,
    hour: i64,
) -> sag_core::Result<Vec<RouteRollupRow>> {
    let sql = format!(
        r#"
        SELECT
            ? AS hour,
            route_id,
            COALESCE(SUM(CASE WHEN final_status = 'success' THEN 1 ELSE 0 END), 0)
                + COALESCE(SUM(CASE WHEN final_status = 'all_disabled' THEN 1 ELSE 0 END), 0)
                + COALESCE(SUM(CASE WHEN final_status = 'model_failure' THEN 1 ELSE 0 END), 0)
                + COALESCE(SUM(CASE WHEN final_status = 'client_cancelled' THEN 1 ELSE 0 END), 0) AS requests,
            COALESCE(SUM(CASE WHEN final_status = 'success' THEN 1 ELSE 0 END), 0) AS success,
            COALESCE(SUM(CASE WHEN final_status = 'model_failure' THEN 1 ELSE 0 END), 0) AS model_failure,
            COALESCE(SUM(CASE WHEN final_status = 'all_disabled' THEN 1 ELSE 0 END), 0) AS all_disabled,
            COALESCE(SUM(CASE WHEN final_status = 'client_cancelled' THEN 1 ELSE 0 END), 0) AS client_cancelled,
            COALESCE(SUM(CASE WHEN final_status = 'success' THEN prompt_tokens END), 0) AS prompt_tokens,
            COALESCE(SUM(CASE WHEN final_status = 'success' THEN completion_tokens END), 0) AS completion_tokens,
            COALESCE(SUM(CASE WHEN final_status = 'success' THEN total_tokens END), 0) AS total_tokens,
            COALESCE(SUM(CASE WHEN final_status = 'success' AND cached_tokens IS NOT NULL THEN cached_tokens END), 0) AS cached_tokens,
            COALESCE(SUM(CASE WHEN final_status = 'success' AND cached_tokens IS NOT NULL THEN 1 ELSE 0 END), 0) AS cached_known_count,
            COALESCE(SUM(CASE WHEN final_status = 'success' AND cached_tokens IS NOT NULL AND cached_tokens > 0 THEN 1 ELSE 0 END), 0) AS cached_hit_count,
            COALESCE(SUM(CASE WHEN final_status = 'success' AND cached_tokens IS NOT NULL THEN prompt_tokens END), 0) AS prompt_tokens_cached_known,
            COALESCE(SUM(CASE WHEN final_status IN ({E2E}) THEN 1 ELSE 0 END), 0) AS e2e_count,
            COALESCE(SUM(CASE WHEN final_status IN ({E2E}) THEN duration_ms END), 0) AS e2e_sum_ms,
            COALESCE(SUM(CASE WHEN final_status IN ({E2E}) AND ttft_ms IS NOT NULL THEN 1 ELSE 0 END), 0) AS ttft_count,
            COALESCE(SUM(CASE WHEN final_status IN ({E2E}) AND ttft_ms IS NOT NULL THEN ttft_ms END), 0) AS ttft_sum_ms,
            COALESCE(SUM(CASE WHEN final_status IN ({E2E}) AND upstream_duration_ms IS NOT NULL THEN 1 ELSE 0 END), 0) AS upstream_count,
            COALESCE(SUM(CASE WHEN final_status IN ({E2E}) AND upstream_duration_ms IS NOT NULL THEN upstream_duration_ms END), 0) AS upstream_sum_ms
        FROM request_results
        WHERE completed_at >= ? AND completed_at < ? AND route_id IS NOT NULL
        GROUP BY route_id
        "#,
        E2E = E2E_STATUSES
    );
    sqlx::query_as::<_, RouteRollupRow>(&sql)
        .bind(hour)
        .bind(hour)
        .bind(hour + HOUR_MS)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| AppError::Internal(format!("rollup route agg hour={hour}: {e}")))
}

/// Per-target per-hour aggregate over `target_attempts` (every actual
/// attempt, including failed / cancelled).
async fn target_aggs(
    conn: &mut sqlx::SqliteConnection,
    hour: i64,
) -> sag_core::Result<Vec<TargetRollupRow>> {
    let sql = r#"
        SELECT
            ? AS hour,
            target_id,
            COALESCE(SUM(CASE WHEN status = 'success' THEN 1 ELSE 0 END), 0)
                + COALESCE(SUM(CASE WHEN status = 'failure' THEN 1 ELSE 0 END), 0)
                + COALESCE(SUM(CASE WHEN status = 'cancelled' THEN 1 ELSE 0 END), 0) AS attempts,
            COALESCE(SUM(CASE WHEN status = 'success' THEN 1 ELSE 0 END), 0) AS success,
            COALESCE(SUM(CASE WHEN status = 'failure' THEN 1 ELSE 0 END), 0) AS failure,
            COALESCE(SUM(CASE WHEN status = 'cancelled' THEN 1 ELSE 0 END), 0) AS cancelled,
            COALESCE(SUM(CASE WHEN status = 'success' THEN prompt_tokens END), 0) AS prompt_tokens,
            COALESCE(SUM(CASE WHEN status = 'success' THEN completion_tokens END), 0) AS completion_tokens,
            COALESCE(SUM(CASE WHEN status = 'success' THEN total_tokens END), 0) AS total_tokens,
            COALESCE(SUM(CASE WHEN status = 'success' AND cached_tokens IS NOT NULL THEN cached_tokens END), 0) AS cached_tokens,
            COALESCE(SUM(CASE WHEN status = 'success' AND cached_tokens IS NOT NULL THEN 1 ELSE 0 END), 0) AS cached_known_count,
            COALESCE(SUM(CASE WHEN status = 'success' AND cached_tokens IS NOT NULL AND cached_tokens > 0 THEN 1 ELSE 0 END), 0) AS cached_hit_count,
            COALESCE(SUM(CASE WHEN status = 'success' AND cached_tokens IS NOT NULL THEN prompt_tokens END), 0) AS prompt_tokens_cached_known,
            COUNT(*) AS e2e_count,
            COALESCE(SUM(duration_ms), 0) AS e2e_sum_ms,
            COALESCE(SUM(CASE WHEN ttft_ms IS NOT NULL THEN 1 ELSE 0 END), 0) AS ttft_count,
            COALESCE(SUM(CASE WHEN ttft_ms IS NOT NULL THEN ttft_ms END), 0) AS ttft_sum_ms,
            COALESCE(SUM(CASE WHEN upstream_duration_ms IS NOT NULL THEN 1 ELSE 0 END), 0) AS upstream_count,
            COALESCE(SUM(CASE WHEN upstream_duration_ms IS NOT NULL THEN upstream_duration_ms END), 0) AS upstream_sum_ms
        FROM target_attempts
        WHERE completed_at >= ? AND completed_at < ?
        GROUP BY target_id
    "#;
    sqlx::query_as::<_, TargetRollupRow>(sql)
        .bind(hour)
        .bind(hour)
        .bind(hour + HOUR_MS)
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| AppError::Internal(format!("rollup target agg hour={hour}: {e}")))
}

/// Raw route-latency source rows of the e2e population.
#[derive(sqlx::FromRow)]
struct RouteLatSource {
    route_id: Option<String>,
    duration_ms: i64,
    ttft_ms: Option<i64>,
    upstream_duration_ms: Option<i64>,
}

/// Raw target-latency source rows (every actual attempt).
#[derive(sqlx::FromRow)]
struct TargetLatSource {
    target_id: String,
    duration_ms: i64,
    ttft_ms: Option<i64>,
    upstream_duration_ms: Option<i64>,
}

/// Collect the bucket's sparse histogram rows: global (sentinel owner),
/// per-route and per-target, for the `e2e` / `ttft` / `upstream` dims.
/// NULL latency values never enter; only non-zero counts produce rows.
async fn collect_histograms(
    conn: &mut sqlx::SqliteConnection,
    hour: i64,
) -> sag_core::Result<Vec<HistogramRow>> {
    let route_rows: Vec<RouteLatSource> = sqlx::query_as::<_, RouteLatSource>(&format!(
        "SELECT route_id, duration_ms, ttft_ms, upstream_duration_ms \
         FROM request_results WHERE completed_at >= ? AND completed_at < ? \
         AND final_status IN ({E2E})",
        E2E = E2E_STATUSES
    ))
    .bind(hour)
    .bind(hour + HOUR_MS)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| AppError::Internal(format!("rollup route hist scan hour={hour}: {e}")))?;

    let target_rows: Vec<TargetLatSource> = sqlx::query_as::<_, TargetLatSource>(
        "SELECT target_id, duration_ms, ttft_ms, upstream_duration_ms \
         FROM target_attempts WHERE completed_at >= ? AND completed_at < ?",
    )
    .bind(hour)
    .bind(hour + HOUR_MS)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| AppError::Internal(format!("rollup target hist scan hour={hour}: {e}")))?;

    let mut global: BTreeMap<(String, i64), u64> = BTreeMap::new();
    let mut route: BTreeMap<(String, String, i64), u64> = BTreeMap::new();
    for row in &route_rows {
        for (dim, ms) in latency_values(row.duration_ms, row.ttft_ms, row.upstream_duration_ms) {
            // NULL latency values never enter the histogram.
            let Some(ms) = ms else { continue };
            let idx = histogram_bucket_idx(ms);
            *global.entry((dim.to_string(), idx)).or_default() += 1;
            if let Some(rid) = &row.route_id {
                *route
                    .entry((rid.clone(), dim.to_string(), idx))
                    .or_default() += 1;
            }
        }
    }
    let mut target: BTreeMap<(String, String, i64), u64> = BTreeMap::new();
    for row in &target_rows {
        for (dim, ms) in latency_values(row.duration_ms, row.ttft_ms, row.upstream_duration_ms) {
            let Some(ms) = ms else { continue };
            let idx = histogram_bucket_idx(ms);
            *target
                .entry((row.target_id.clone(), dim.to_string(), idx))
                .or_default() += 1;
        }
    }

    let mut out = Vec::with_capacity(global.len() + route.len() + target.len());
    for ((dim, idx), count) in global {
        out.push(HistogramRow {
            hour,
            scope: "route".to_string(),
            owner_id: GLOBAL_HIST_OWNER.to_string(),
            dim,
            bucket_idx: idx,
            count: count as i64,
        });
    }
    for ((owner_id, dim, idx), count) in route {
        out.push(HistogramRow {
            hour,
            scope: "route".to_string(),
            owner_id,
            dim,
            bucket_idx: idx,
            count: count as i64,
        });
    }
    for ((owner_id, dim, idx), count) in target {
        out.push(HistogramRow {
            hour,
            scope: "target".to_string(),
            owner_id,
            dim,
            bucket_idx: idx,
            count: count as i64,
        });
    }
    Ok(out)
}

/// The three latency dimensions of one source row, in stable order. `None`
/// values (missing latency) never enter the histogram.
fn latency_values(
    e2e: i64,
    ttft: Option<i64>,
    upstream: Option<i64>,
) -> [(&'static str, Option<i64>); 3] {
    [("e2e", Some(e2e)), ("ttft", ttft), ("upstream", upstream)]
}

/// Bucket index of a latency value in ms (spec: 0 = the 0ms bucket; `v >= 1`
/// → `floor(log(v) / log(1.01)) + 1`; beyond capacity →
/// [`OVERFLOW_BUCKET`]; `v <= 0` → bucket 0). NULL values are handled by
/// callers and never reach this function.
pub fn histogram_bucket_idx(ms: i64) -> i64 {
    if ms <= 0 {
        return 0;
    }
    let idx = (ms as f64).ln() / HISTOGRAM_BASE.ln();
    let idx = idx.floor() as i64 + 1;
    if idx >= MAX_HISTOGRAM_BUCKETS {
        OVERFLOW_BUCKET
    } else {
        idx
    }
}

/// Lower bound (ms) of a regular bucket: `0` → `0.0`; `k >= 1` →
/// `1.01^(k-1)`. [`OVERFLOW_BUCKET`] and out-of-range indices → `None`
/// (their bound is not representable by the fixed capacity).
pub fn bucket_lower_bound_ms(bucket_idx: i64) -> Option<f64> {
    match bucket_idx {
        OVERFLOW_BUCKET => None,
        0 => Some(0.0),
        k if (1..MAX_HISTOGRAM_BUCKETS).contains(&k) => Some(HISTOGRAM_BASE.powi((k - 1) as i32)),
        _ => None,
    }
}

/// Merge histogram rows (typically several hours / owners of one dimension)
/// into per-dim [`HistogramBins`]: regular buckets are summed by
/// `bucket_idx`, overflow samples accumulate separately. The global
/// sentinel row is included as-is — callers decide what to merge.
pub fn merge_histogram_buckets<'a>(
    rows: impl IntoIterator<Item = &'a HistogramRow>,
) -> BTreeMap<String, HistogramBins> {
    let mut out: BTreeMap<String, HistogramBins> = BTreeMap::new();
    for row in rows {
        let bins = out.entry(row.dim.clone()).or_default();
        if row.bucket_idx == OVERFLOW_BUCKET {
            bins.overflow += row.count.max(0) as u64;
        } else {
            *bins.counts.entry(row.bucket_idx).or_default() += row.count.max(0) as u64;
        }
    }
    out
}

/// Estimate the value at percentile `p` from merged bucket counts:
/// nearest-rank over cumulative counts (`R = ceil(p/100 * N)`, 1-based),
/// returning the lower bound of the bucket that first reaches `R` (spec:
/// 分位由累计计数反查桶下界得到). Empty samples → `None`; a rank inside
/// the overflow region is estimated with the overflow bucket's lower
/// bound (precision not guaranteed).
pub fn bucket_to_percentile(bins: &HistogramBins, p: f64) -> Option<f64> {
    let total: u64 = bins.counts.values().sum::<u64>() + bins.overflow;
    if total == 0 {
        return None;
    }
    let rank = ((p / 100.0) * total as f64).ceil().max(1.0) as u64;
    let mut cumulative: u64 = 0;
    for (&idx, &count) in &bins.counts {
        cumulative += count;
        if cumulative >= rank {
            return bucket_lower_bound_ms(idx);
        }
    }
    if cumulative + bins.overflow >= rank {
        // Values with idx >= MAX_HISTOGRAM_BUCKETS land here; their lower
        // bound is the lower bound of the first overflow bucket.
        Some(HISTOGRAM_BASE.powi((MAX_HISTOGRAM_BUCKETS - 1) as i32))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::test_support::test_pool;
    use crate::{AnalyticsEvent, EventWriter};
    use sag_core::{FinalStatus, RequestResultFact, TargetAttemptFact, TargetStatus, Usage};
    use uuid::Uuid;

    /// A UTC hour start (2023-11-14T22:00:00Z).
    const H0: i64 = 1_699_999_200_000;

    fn usage(prompt: u32, completion: u32) -> Usage {
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            ..Default::default()
        }
    }

    #[allow(clippy::too_many_arguments)] // test-only fact builder
    fn fact(
        status: FinalStatus,
        route: Option<&str>,
        completed_ms: i64,
        duration_ms: i64,
        usage: Option<Usage>,
        cached: Option<u32>,
        cache_creation: Option<u32>,
        ttft: Option<u64>,
        upstream: Option<u64>,
    ) -> RequestResultFact {
        use chrono::{TimeZone, Utc};
        RequestResultFact {
            request_id: Uuid::new_v4(),
            route_name: route.map(str::to_string),
            requested_model: "test-model".to_string(),
            final_status: status,
            failure_reason: None,
            stream: false,
            started_at: Utc
                .timestamp_millis_opt(completed_ms - duration_ms)
                .unwrap(),
            completed_at: Utc.timestamp_millis_opt(completed_ms).unwrap(),
            usage,
            cached_tokens: cached,
            cache_creation_tokens: cache_creation,
            ttft_ms: ttft,
            upstream_duration_ms: upstream,
        }
    }

    #[allow(clippy::too_many_arguments)] // test-only attempt builder
    fn attempt(
        provider: &str,
        model: &str,
        status: TargetStatus,
        completed_ms: i64,
        duration_ms: i64,
        usage: Option<Usage>,
        cached: Option<u32>,
        ttft: Option<u64>,
        upstream: Option<u64>,
    ) -> TargetAttemptFact {
        use chrono::{TimeZone, Utc};
        TargetAttemptFact {
            provider: provider.to_string(),
            real_model: model.to_string(),
            status,
            failure_reason: None,
            started_at: Utc
                .timestamp_millis_opt(completed_ms - duration_ms)
                .unwrap(),
            completed_at: Utc.timestamp_millis_opt(completed_ms).unwrap(),
            usage,
            cached_tokens: cached,
            ttft_ms: ttft,
            upstream_duration_ms: upstream,
        }
    }

    async fn write(pool: &SqlitePool, result: RequestResultFact, targets: Vec<TargetAttemptFact>) {
        EventWriter::new(pool.clone())
            .write(AnalyticsEvent { result, targets })
            .await
            .unwrap();
    }

    async fn register_and_run(pool: &SqlitePool, from_ms: i64, to_ms: i64) -> RollupRunReport {
        let engine = RollupEngine::new(pool.clone());
        engine.register_missing_hours(from_ms, to_ms).await.unwrap();
        engine.run_once().await.unwrap()
    }

    /// A plain success request (route `r1`, target p1/m1) at `completed_ms`.
    fn success_req(
        route: &str,
        completed_ms: i64,
        duration_ms: i64,
    ) -> (RequestResultFact, Vec<TargetAttemptFact>) {
        let u = Some(usage(10, 20));
        (
            fact(
                FinalStatus::Success,
                Some(route),
                completed_ms,
                duration_ms,
                u.clone(),
                None,
                None,
                None,
                None,
            ),
            vec![attempt(
                "p1",
                "m1",
                TargetStatus::Success,
                completed_ms,
                duration_ms,
                u,
                None,
                None,
                None,
            )],
        )
    }

    async fn engine() -> (SqlitePool, RollupEngine) {
        let pool = test_pool().await;
        (pool.clone(), RollupEngine::new(pool))
    }

    // ── registration: only hours with facts, only the missing/dirty/failed ──

    #[tokio::test]
    async fn empty_hours_are_not_registered_and_missing_is_pending() {
        let (pool, engine) = engine().await;
        let (r, t) = success_req("r1", H0 + 60_000, 250);
        write(&pool, r, t).await;

        // A range with no facts registers nothing.
        let empty = engine
            .register_missing_hours(H0 - HOUR_MS, H0)
            .await
            .unwrap();
        assert!(empty.is_empty());
        assert!(engine.bucket_status(H0 - HOUR_MS).await.unwrap().is_none());

        // The hour with facts is registered as pending (attempt 0, clean).
        let registered = engine
            .register_missing_hours(H0, H0 + HOUR_MS)
            .await
            .unwrap();
        assert_eq!(registered, vec![H0]);
        let st = engine.bucket_status(H0).await.unwrap().unwrap();
        assert_eq!(st.state, BucketState::Pending);
        assert!(!st.dirty);
        assert_eq!(st.attempt, 0);

        // Re-registration of an already pending bucket is a no-op.
        assert!(
            engine
                .register_missing_hours(H0, H0 + HOUR_MS)
                .await
                .unwrap()
                .is_empty()
        );
    }

    // ── claim: at most 24 per run, lease recovery ──

    #[tokio::test]
    async fn run_claims_and_completes_at_most_24_buckets() {
        let (pool, engine) = engine().await;
        for i in 0..30 {
            let (r, t) = success_req("r1", H0 + i as i64 * HOUR_MS + 60_000, 250);
            write(&pool, r, t).await;
        }
        let registered = engine
            .register_missing_hours(H0, H0 + 30 * HOUR_MS)
            .await
            .unwrap();
        assert_eq!(registered.len(), 30);

        let first = engine.run_once().await.unwrap();
        assert_eq!(first.claimed, 24);
        assert_eq!(first.completed, 24);
        assert_eq!(first.failed, 0);
        assert_eq!(
            engine
                .bucket_status(H0 + 23 * HOUR_MS)
                .await
                .unwrap()
                .unwrap()
                .state,
            BucketState::Completed
        );
        assert_eq!(
            engine
                .bucket_status(H0 + 24 * HOUR_MS)
                .await
                .unwrap()
                .unwrap()
                .state,
            BucketState::Pending
        );

        let second = engine.run_once().await.unwrap();
        assert_eq!(second.claimed, 6);
        assert_eq!(second.completed, 6);
        assert_eq!(second.failed, 0);

        // Nothing left to claim.
        let third = engine.run_once().await.unwrap();
        assert_eq!(third.claimed, 0);
    }

    #[tokio::test]
    async fn processing_lease_expiry_recovers_buckets() {
        let (pool, engine) = engine().await;
        let (r, t) = success_req("r1", H0 + 60_000, 250);
        write(&pool, r, t).await;
        engine
            .register_missing_hours(H0, H0 + HOUR_MS)
            .await
            .unwrap();

        // Claim once: fresh lease is not re-claimed.
        assert_eq!(engine.claim_buckets().await.unwrap(), vec![H0]);
        let st = engine.bucket_status(H0).await.unwrap().unwrap();
        assert_eq!(st.state, BucketState::Processing);
        // `attempt` counts failures (spec: processing → failed bumps it),
        // not claims — a first claim leaves it at 0.
        assert_eq!(st.attempt, 0);
        assert!(st.processing_started_at.is_some());
        assert!(engine.claim_buckets().await.unwrap().is_empty());

        // Registration leaves a fresh processing bucket alone.
        assert!(
            engine
                .register_missing_hours(H0, H0 + HOUR_MS)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            engine.bucket_status(H0).await.unwrap().unwrap().state,
            BucketState::Processing
        );

        // Crash simulation: lease expires -> registration restores to pending.
        let stale = now_ms() - PROCESSING_LEASE_TIMEOUT_MS - 1000;
        sqlx::query("UPDATE rollup_bucket_state SET processing_started_at = ? WHERE hour = ?")
            .bind(stale)
            .bind(H0)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            engine
                .register_missing_hours(H0, H0 + HOUR_MS)
                .await
                .unwrap(),
            vec![H0]
        );
        assert_eq!(
            engine.bucket_status(H0).await.unwrap().unwrap().state,
            BucketState::Pending
        );

        // The claim itself also picks up stale processing leases.
        sqlx::query("UPDATE rollup_bucket_state SET state = 'processing', processing_started_at = ? WHERE hour = ?")
            .bind(stale)
            .bind(H0)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(engine.claim_buckets().await.unwrap(), vec![H0]);
        // Claiming never bumps `attempt` — only failures do (spec state
        // machine). Re-claiming a stale lease therefore keeps it at 0.
        assert_eq!(engine.bucket_status(H0).await.unwrap().unwrap().attempt, 0);
    }

    // ── aggregation semantics (mirrors the exact-detail kernel) ──

    #[tokio::test]
    async fn all_statuses_aggregate_with_global_route_equality_and_histograms() {
        let (pool, engine) = engine().await;
        let ms = |minute: i64| H0 + minute * 60_000;

        // req1: direct success, tokens, cached unknown, ttft NULL, upstream 100.
        let u1 = Some(usage(10, 20));
        write(
            &pool,
            fact(
                FinalStatus::Success,
                Some("r1"),
                ms(1),
                250,
                u1.clone(),
                None,
                None,
                None,
                Some(100),
            ),
            vec![attempt(
                "p1",
                "m1",
                TargetStatus::Success,
                ms(1),
                200,
                u1,
                None,
                None,
                Some(100),
            )],
        )
        .await;

        // req2: success on the same route/target, cached Some(0) known miss,
        // cache_creation 5, ttft 50.
        let u2 = Some(usage(1, 2));
        write(
            &pool,
            fact(
                FinalStatus::Success,
                Some("r1"),
                ms(2),
                250,
                u2.clone(),
                Some(0),
                Some(5),
                Some(50),
                None,
            ),
            vec![attempt(
                "p1",
                "m1",
                TargetStatus::Success,
                ms(2),
                200,
                u2,
                Some(0),
                Some(50),
                None,
            )],
        )
        .await;

        // req3: fallback scenario A-failed -> B-succeeded: route success,
        // attempt A failure + attempt B success (different target).
        let u3 = Some(usage(5, 5));
        write(
            &pool,
            fact(
                FinalStatus::Success,
                Some("r1"),
                ms(3),
                250,
                u3.clone(),
                None,
                None,
                Some(30),
                None,
            ),
            vec![
                attempt(
                    "p2",
                    "m2",
                    TargetStatus::Failure,
                    ms(3),
                    150,
                    None,
                    None,
                    None,
                    None,
                ),
                attempt(
                    "p1",
                    "m1",
                    TargetStatus::Success,
                    ms(3),
                    200,
                    u3,
                    None,
                    Some(30),
                    None,
                ),
            ],
        )
        .await;

        // req4: model_failure with ttft/upstream (e2e population member).
        write(
            &pool,
            fact(
                FinalStatus::ModelFailure,
                Some("r1"),
                ms(4),
                250,
                None,
                None,
                None,
                Some(70),
                Some(200),
            ),
            vec![attempt(
                "p2",
                "m2",
                TargetStatus::Failure,
                ms(4),
                200,
                None,
                None,
                None,
                None,
            )],
        )
        .await;

        // req5: all_disabled (route, no attempts).
        write(
            &pool,
            fact(
                FinalStatus::AllDisabled,
                Some("r1"),
                ms(5),
                250,
                None,
                None,
                None,
                None,
                None,
            ),
            vec![],
        )
        .await;

        // req6: client_error and req7: no_route (no route, no attempts).
        write(
            &pool,
            fact(
                FinalStatus::ClientError,
                None,
                ms(6),
                250,
                None,
                None,
                None,
                None,
                None,
            ),
            vec![],
        )
        .await;
        write(
            &pool,
            fact(
                FinalStatus::NoRoute,
                None,
                ms(7),
                250,
                None,
                None,
                None,
                None,
                None,
            ),
            vec![],
        )
        .await;

        // req8: client_cancelled with an in-flight cancelled attempt.
        write(
            &pool,
            fact(
                FinalStatus::ClientCancelled,
                Some("r1"),
                ms(8),
                250,
                None,
                None,
                None,
                None,
                None,
            ),
            vec![attempt(
                "p3",
                "m3",
                TargetStatus::Cancelled,
                ms(8),
                200,
                None,
                None,
                None,
                None,
            )],
        )
        .await;

        register_and_run(&pool, H0, H0 + HOUR_MS).await;

        let global = &engine.global_rows(&[H0]).await.unwrap()[0];
        assert_eq!(global.requests, 8);
        assert_eq!(global.success, 3);
        assert_eq!(global.client_error, 1);
        assert_eq!(global.no_route, 1);
        assert_eq!(global.all_disabled, 1);
        assert_eq!(global.model_failure, 1);
        assert_eq!(global.client_cancelled, 1);
        // Tokens only from the three success requests: 10+1+5 / 20+2+5 / 30+3+10.
        assert_eq!(global.prompt_tokens, 16);
        assert_eq!(global.completion_tokens, 27);
        assert_eq!(global.total_tokens, 43);
        // Cache: only req2 has known cache info (Some(0) = known miss).
        assert_eq!(global.cached_tokens, 0);
        assert_eq!(global.cache_creation_tokens, 5);
        assert_eq!(global.cached_known_count, 1);
        assert_eq!(global.cached_hit_count, 0);
        assert_eq!(global.prompt_tokens_cached_known, 1);
        // e2e population: 3 success + 1 model_failure + 1 all_disabled +
        // 1 client_cancelled = 6 rows, all 250ms.
        assert_eq!(global.e2e_count, 6);
        assert_eq!(global.e2e_sum_ms, 1500);
        // ttft: req2 (50) + req3 (30) + req4 (70).
        assert_eq!(global.ttft_count, 3);
        assert_eq!(global.ttft_sum_ms, 150);
        // upstream: req1 (100) + req4 (200).
        assert_eq!(global.upstream_count, 2);
        assert_eq!(global.upstream_sum_ms, 300);

        // Route: matched-route rows only (client_error / no_route excluded);
        // requests = success + all_disabled + model_failure + client_cancelled.
        let routes = engine.route_rows(&[H0]).await.unwrap();
        assert_eq!(routes.len(), 1);
        let route = &routes[0];
        assert_eq!(route.requests, 6);
        assert_eq!(route.success, 3);
        assert_eq!(route.model_failure, 1);
        assert_eq!(route.all_disabled, 1);
        assert_eq!(route.client_cancelled, 1);
        assert_eq!(route.e2e_count, 6);
        assert_eq!(route.e2e_sum_ms, 1500);

        // Denominator equality: global = Σ route + client_error + no_route;
        // global e2e == Σ route e2e.
        assert_eq!(
            global.requests,
            routes.iter().map(|r| r.requests).sum::<i64>() + 2
        );
        assert_eq!(
            global.e2e_count,
            routes.iter().map(|r| r.e2e_count).sum::<i64>()
        );
        assert_eq!(
            global.e2e_sum_ms,
            routes.iter().map(|r| r.e2e_sum_ms).sum::<i64>()
        );

        // Targets: three-way status counts per target.
        let targets = engine.target_rows(&[H0]).await.unwrap();
        assert_eq!(targets.len(), 3);
        let t1 = targets.iter().find(|t| t.success == 3).unwrap();
        assert_eq!((t1.attempts, t1.failure, t1.cancelled), (3, 0, 0));
        assert_eq!(t1.prompt_tokens, 16);
        let t2 = targets.iter().find(|t| t.failure == 2).unwrap();
        assert_eq!((t2.attempts, t2.success, t2.cancelled), (2, 0, 0));
        let t3 = targets.iter().find(|t| t.cancelled == 1).unwrap();
        assert_eq!((t3.attempts, t3.success, t3.failure), (1, 0, 0));
        assert_eq!(
            targets.iter().map(|t| t.attempts).sum::<i64>(),
            targets
                .iter()
                .map(|t| t.success + t.failure + t.cancelled)
                .sum::<i64>()
        );

        // Histograms: global == Σ route histograms; sparse non-zero buckets.
        let global_hist = engine.global_histogram_rows(&[H0]).await.unwrap();
        let all_route_hist = engine.histogram_rows(&[H0], "route", None).await.unwrap();
        let per_route: Vec<HistogramRow> = all_route_hist
            .iter()
            .filter(|r| r.owner_id != GLOBAL_HIST_OWNER)
            .cloned()
            .collect();
        assert_eq!(
            merge_histogram_buckets(&global_hist),
            merge_histogram_buckets(&per_route)
        );

        let g = merge_histogram_buckets(&global_hist);
        let e2e = &g["e2e"];
        // 6 samples of 250ms.
        assert_eq!(e2e.counts.get(&histogram_bucket_idx(250)), Some(&6));
        assert_eq!(e2e.counts.len(), 1);
        // ttft: 30 / 50 / 70 ms.
        let ttft = &g["ttft"];
        assert_eq!(ttft.counts.values().sum::<u64>(), 3);
        for ms in [30, 50, 70] {
            assert_eq!(ttft.counts.get(&histogram_bucket_idx(ms)), Some(&1));
        }
        // upstream: 100 / 200 ms.
        let upstream = &g["upstream"];
        assert_eq!(upstream.counts.get(&histogram_bucket_idx(100)), Some(&1));
        assert_eq!(upstream.counts.get(&histogram_bucket_idx(200)), Some(&1));

        // Target histograms: every actual attempt enters e2e (6 attempts:
        // 5 at 200ms, 1 failed at 150ms).
        let target_hist = engine.histogram_rows(&[H0], "target", None).await.unwrap();
        let t = merge_histogram_buckets(&target_hist);
        assert_eq!(t["e2e"].counts.values().sum::<u64>(), 6);
        assert_eq!(t["e2e"].counts.get(&histogram_bucket_idx(200)), Some(&5));
        assert_eq!(t["e2e"].counts.get(&histogram_bucket_idx(150)), Some(&1));
    }

    #[tokio::test]
    async fn null_tokens_cache_and_latency_are_never_faked_as_zero() {
        let (pool, engine) = engine().await;

        // Success with NO usage at all: NULL token columns must not invent 0.
        write(
            &pool,
            fact(
                FinalStatus::Success,
                Some("r1"),
                H0 + 60_000,
                250,
                None,
                None,
                None,
                None,
                None,
            ),
            vec![attempt(
                "p1",
                "m1",
                TargetStatus::Success,
                H0 + 60_000,
                250,
                None,
                None,
                None,
                None,
            )],
        )
        .await;

        // Success with cached_tokens Some(0): known miss — counted as known,
        // not as hit, and its cached_tokens sum is 0 (not missing).
        let u = Some(usage(7, 7));
        write(
            &pool,
            fact(
                FinalStatus::Success,
                Some("r1"),
                H0 + 120_000,
                250,
                u.clone(),
                Some(0),
                None,
                None,
                None,
            ),
            vec![attempt(
                "p1",
                "m1",
                TargetStatus::Success,
                H0 + 120_000,
                250,
                u,
                Some(0),
                None,
                None,
            )],
        )
        .await;

        // Failure attempt carries NULL tokens; ttft Some(0) is a *valid*
        // zero value (enters counts and the 0ms histogram bucket).
        write(
            &pool,
            fact(
                FinalStatus::ModelFailure,
                Some("r1"),
                H0 + 180_000,
                250,
                None,
                None,
                None,
                Some(0),
                None,
            ),
            vec![attempt(
                "p2",
                "m2",
                TargetStatus::Failure,
                H0 + 180_000,
                250,
                None,
                None,
                None,
                None,
            )],
        )
        .await;

        register_and_run(&pool, H0, H0 + HOUR_MS).await;

        let global = &engine.global_rows(&[H0]).await.unwrap()[0];
        assert_eq!(global.requests, 3);
        assert_eq!(global.success, 2);
        // The success without usage contributes no tokens (NULL stays NULL
        // in SQL sums; the rollup stores 0 only for "no known samples").
        assert_eq!(global.prompt_tokens, 7);
        assert_eq!(global.completion_tokens, 7);
        assert_eq!(global.total_tokens, 14);
        // Unknown (None) is not part of known/hit; Some(0) is a known miss.
        assert_eq!(global.cached_known_count, 1);
        assert_eq!(global.cached_hit_count, 0);
        assert_eq!(global.prompt_tokens_cached_known, 7);
        assert_eq!(global.cached_tokens, 0);
        // ttft Some(0) is a real sample; the failure row has NULL ttft.
        assert_eq!(global.ttft_count, 1);
        assert_eq!(global.ttft_sum_ms, 0);

        // The 0ms bucket is populated by the valid ttft=0 sample.
        let g = merge_histogram_buckets(&engine.global_histogram_rows(&[H0]).await.unwrap());
        assert_eq!(g["ttft"].counts.get(&0), Some(&1));

        // Failed target: no token columns, e2e counted, ttft NULL excluded.
        let targets = engine.target_rows(&[H0]).await.unwrap();
        let t2 = targets.iter().find(|t| t.failure == 1).unwrap();
        assert_eq!(t2.prompt_tokens, 0);
        assert_eq!(t2.completion_tokens, 0);
        assert_eq!(t2.cached_known_count, 0);
        assert_eq!(t2.e2e_count, 1);
        assert_eq!(t2.ttft_count, 0);
    }

    // ── histogram bucketing rules (pure functions) ──

    #[test]
    fn histogram_bucket_boundaries() {
        // 0ms bucket, 1ms log start, ~1.01x growth.
        assert_eq!(histogram_bucket_idx(0), 0);
        assert_eq!(histogram_bucket_idx(1), 1);
        assert_eq!(histogram_bucket_idx(1000), 695); // ln(1000)/ln(1.01) ≈ 694.2
        assert_eq!(histogram_bucket_idx(1_000_000), 1389);
        assert_eq!(histogram_bucket_idx(100_000_000_000_000_000), 3934);
        // Beyond the fixed capacity -> overflow.
        assert_eq!(
            histogram_bucket_idx(1_000_000_000_000_000_000),
            OVERFLOW_BUCKET
        );
        assert_eq!(histogram_bucket_idx(i64::MAX), OVERFLOW_BUCKET);

        assert_eq!(bucket_lower_bound_ms(0), Some(0.0));
        assert_eq!(bucket_lower_bound_ms(1), Some(1.0));
        assert_eq!(bucket_lower_bound_ms(2), Some(1.01));
        assert_eq!(bucket_lower_bound_ms(555), Some(1.01_f64.powi(554)));
        assert_eq!(bucket_lower_bound_ms(OVERFLOW_BUCKET), None);
        assert_eq!(bucket_lower_bound_ms(MAX_HISTOGRAM_BUCKETS), None);
    }

    #[test]
    fn merge_and_percentile_helpers() {
        let row = |hour: i64, owner: &str, dim: &str, idx: i64, count: i64| HistogramRow {
            hour,
            scope: "route".to_string(),
            owner_id: owner.to_string(),
            dim: dim.to_string(),
            bucket_idx: idx,
            count,
        };

        // Merging across hours/owners sums regular buckets and keeps the
        // overflow bucket separate.
        let rows = vec![
            row(H0, "r1", "e2e", 555, 5),
            row(H0 + HOUR_MS, "r1", "e2e", 555, 3),
            row(H0, "r2", "e2e", 100, 1),
            row(H0, "r1", "ttft", 0, 1),
            row(H0, "r1", "e2e", OVERFLOW_BUCKET, 2),
        ];
        let merged = merge_histogram_buckets(&rows);
        let e2e = &merged["e2e"];
        assert_eq!(e2e.counts.get(&555), Some(&8));
        assert_eq!(e2e.counts.get(&100), Some(&1));
        assert_eq!(e2e.overflow, 2);
        assert_eq!(merged["ttft"].counts.get(&0), Some(&1));
        assert!(!merged.contains_key("upstream"));

        // Percentile: nearest rank over cumulative counts -> bucket lower bound.
        let mut bins = HistogramBins::default();
        bins.counts.insert(0, 1);
        bins.counts.insert(1, 1);
        bins.counts.insert(555, 2);
        assert_eq!(bucket_to_percentile(&bins, 50.0), Some(1.0));
        assert_eq!(bucket_to_percentile(&bins, 95.0), Some(1.01_f64.powi(554)));
        assert_eq!(bucket_to_percentile(&bins, 100.0), Some(1.01_f64.powi(554)));

        // Overflow: rank inside the overflow region is estimated with the
        // overflow lower bound; empty bins yield None.
        let mut ovf = HistogramBins::default();
        ovf.counts.insert(0, 1);
        ovf.overflow = 3;
        assert_eq!(bucket_to_percentile(&ovf, 25.0), Some(0.0));
        assert_eq!(
            bucket_to_percentile(&ovf, 75.0),
            Some(1.01_f64.powi((MAX_HISTOGRAM_BUCKETS - 1) as i32))
        );
        assert_eq!(bucket_to_percentile(&HistogramBins::default(), 50.0), None);
    }

    // ── atomicity / idempotency / dirty rebuild ──

    #[tokio::test]
    async fn bucket_failure_is_atomic_no_partial_rows() {
        let (pool, engine) = engine().await;
        let (r, t) = success_req("r1", H0 + 60_000, 250);
        write(&pool, r, t).await;
        register_and_run(&pool, H0, H0 + HOUR_MS).await;
        assert_eq!(
            engine.bucket_status(H0).await.unwrap().unwrap().state,
            BucketState::Completed
        );
        let hist_before = engine
            .histogram_rows(&[H0], "route", None)
            .await
            .unwrap()
            .len();

        // Inject a failure into the target rollup table.
        sqlx::query(
            "CREATE TRIGGER rollup_test_fail BEFORE INSERT ON rollup_target_hourly \
             BEGIN SELECT RAISE(ABORT, 'injected'); END",
        )
        .execute(&pool)
        .await
        .unwrap();

        // A late fact + dirty flag triggers a recompute that must fail
        // atomically: no half-written rows, previous rows intact.
        let (r2, t2) = success_req("r1", H0 + 120_000, 250);
        write(&pool, r2, t2).await;
        engine.mark_dirty(H0).await.unwrap();
        assert!(
            engine
                .completed_hours(H0, H0 + HOUR_MS)
                .await
                .unwrap()
                .is_empty()
        );
        engine
            .register_missing_hours(H0, H0 + HOUR_MS)
            .await
            .unwrap();

        let failed = engine.run_once().await.unwrap();
        assert_eq!((failed.claimed, failed.completed, failed.failed), (1, 0, 1));
        assert_eq!(
            engine.bucket_status(H0).await.unwrap().unwrap().state,
            BucketState::Failed
        );
        // `attempt` counts failures (spec: processing → failed bumps it).
        assert_eq!(engine.bucket_status(H0).await.unwrap().unwrap().attempt, 1);

        // No half-products: the old global row and old histogram survive,
        // and no duplicate rows appeared.
        let global = &engine.global_rows(&[H0]).await.unwrap()[0];
        assert_eq!(global.success, 1);
        assert_eq!(engine.global_rows(&[H0]).await.unwrap().len(), 1);
        assert_eq!(
            engine
                .histogram_rows(&[H0], "route", None)
                .await
                .unwrap()
                .len(),
            hist_before
        );

        // Remove the fault and retry: the failed bucket is claimable and
        // recomputes with the late fact.
        sqlx::query("DROP TRIGGER rollup_test_fail")
            .execute(&pool)
            .await
            .unwrap();
        let retry = engine.run_once().await.unwrap();
        assert_eq!((retry.claimed, retry.completed, retry.failed), (1, 1, 0));
        let st = engine.bucket_status(H0).await.unwrap().unwrap();
        assert_eq!(st.state, BucketState::Completed);
        assert!(!st.dirty);
        assert_eq!(engine.global_rows(&[H0]).await.unwrap()[0].success, 2);
        assert_eq!(
            engine.completed_hours(H0, H0 + HOUR_MS).await.unwrap(),
            vec![H0]
        );
    }

    #[tokio::test]
    async fn rerun_is_idempotent_and_dirty_rebuild_picks_late_facts() {
        let (pool, engine) = engine().await;
        let (r, t) = success_req("r1", H0 + 60_000, 250);
        write(&pool, r, t).await;
        register_and_run(&pool, H0, H0 + HOUR_MS).await;
        let first = engine.global_rows(&[H0]).await.unwrap()[0].clone();

        // Dirty rebuild with no new facts: identical values, single row.
        engine.mark_dirty(H0).await.unwrap();
        engine
            .register_missing_hours(H0, H0 + HOUR_MS)
            .await
            .unwrap();
        engine.run_once().await.unwrap();
        assert_eq!(engine.global_rows(&[H0]).await.unwrap().len(), 1);
        assert_eq!(engine.global_rows(&[H0]).await.unwrap()[0], first);

        // A late fact arrives: mark_dirty -> recompute picks it up.
        let (r2, t2) = success_req("r1", H0 + 120_000, 250);
        write(&pool, r2, t2).await;
        engine.mark_dirty(H0).await.unwrap();
        engine
            .register_missing_hours(H0, H0 + HOUR_MS)
            .await
            .unwrap();
        engine.run_once().await.unwrap();
        let st = engine.bucket_status(H0).await.unwrap().unwrap();
        assert_eq!(st.state, BucketState::Completed);
        assert!(!st.dirty);
        assert_eq!(engine.global_rows(&[H0]).await.unwrap()[0].success, 2);
    }

    // ── read helpers ──

    #[tokio::test]
    async fn read_helpers_return_rollup_rows() {
        let (pool, engine) = engine().await;
        // Hour 1: r1 success + client_error. Hour 2: r1 success + r2 model_failure.
        let (r, t) = success_req("r1", H0 + 60_000, 250);
        write(&pool, r, t).await;
        write(
            &pool,
            fact(
                FinalStatus::ClientError,
                None,
                H0 + 60_000,
                250,
                None,
                None,
                None,
                None,
                None,
            ),
            vec![],
        )
        .await;
        let (r1, t1) = success_req("r1", H0 + HOUR_MS + 60_000, 250);
        write(&pool, r1, t1).await;
        write(
            &pool,
            fact(
                FinalStatus::ModelFailure,
                Some("r2"),
                H0 + HOUR_MS + 60_000,
                250,
                None,
                None,
                None,
                None,
                None,
            ),
            vec![attempt(
                "p9",
                "m9",
                TargetStatus::Failure,
                H0 + HOUR_MS + 60_000,
                250,
                None,
                None,
                None,
                None,
            )],
        )
        .await;

        register_and_run(&pool, H0, H0 + 2 * HOUR_MS).await;

        assert_eq!(
            engine.completed_hours(H0, H0 + 2 * HOUR_MS).await.unwrap(),
            vec![H0, H0 + HOUR_MS]
        );
        assert_eq!(
            engine.bucket_status(H0).await.unwrap().unwrap().state,
            BucketState::Completed
        );
        assert!(engine.bucket_status(H0 + 999).await.unwrap().is_none());

        assert_eq!(engine.global_rows(&[H0]).await.unwrap().len(), 1);
        assert_eq!(
            engine.global_rows(&[H0, H0 + HOUR_MS]).await.unwrap().len(),
            2
        );
        assert_eq!(
            engine.route_rows(&[H0, H0 + HOUR_MS]).await.unwrap().len(),
            3
        );
        assert_eq!(
            engine.target_rows(&[H0, H0 + HOUR_MS]).await.unwrap().len(),
            3 // per (hour, target_id): H0×p1/m1, H1×p1/m1, H1×p9/m9
        );

        // Route histogram merge across hours.
        let h1 = engine.global_histogram_rows(&[H0]).await.unwrap();
        let h2 = engine.global_histogram_rows(&[H0 + HOUR_MS]).await.unwrap();
        let merged = merge_histogram_buckets(&[h1, h2].concat());
        let total_e2e: u64 = merged["e2e"].counts.values().sum::<u64>() + merged["e2e"].overflow;
        assert_eq!(total_e2e, 3); // r1, r1, r2 (client_error excluded)
    }
}
