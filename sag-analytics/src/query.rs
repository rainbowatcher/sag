//! Analytics query kernel: exact details (spans ≤ 24h) and a mixed
//! rollup + detail path (spans > 24h).
//!
//! [`AnalyticsQueryEngine`] computes summary / trend statistics from the
//! two-level fact tables (`request_results` / `target_attempts`) and the
//! 1h rollup tables with the failure-rate, token, cache and latency
//! semantics of `docs/spec/analytics/requirements.md` /
//! `docs/spec/analytics/spec.md`:
//!
//! - `scope=route` reads **final results** (`request_results`): one request
//!   produces exactly one route-level row; `scope=target` reads **actual
//!   attempts** (`target_attempts`): one row per actually-issued upstream
//!   call.
//! - `from`/`to` are unix ms, **left-closed right-open** (`completed_at >=
//!   from AND completed_at < to`); facts are attributed to **UTC hours by
//!   completion time**.
//! - spans **≤ 24h** are answered exactly from the details. Longer spans
//!   use the mixed path: the full UTC hours inside `[from, to - 24h)` come
//!   from the 1h rollup tables, everything else (the recent 24h, the
//!   partial boundary hours and every hour whose bucket is missing or
//!   dirty) from the details. Rollup rows never silently produce zeros:
//!   hours without a clean `completed` bucket are backfilled from the
//!   details and re-registered for backfill (`pending` only, no compute).
//! - latency percentiles of the exact path are computed Rust-side from the
//!   raw values (nearest-rank; empty samples → `None`), never via SQLite
//!   extensions; mixed spans estimate them from the rollup histogram
//!   (≈1% value error, see [`LatencyMetric::approx_percentiles`]) while
//!   the average stays exact (`*_sum_ms` + detail sums).
//! - token / cached-token columns only aggregate over final-success
//!   samples; `cached_tokens = NULL` means **unknown** (distinct from
//!   miss); the Token 命中率 = Σ `COALESCE(cached_tokens, 0)` / Σ
//!   `prompt_tokens` over **all** success samples (NULL cached_tokens
//!   contributes 0 to the numerator while its prompt_tokens stays in the
//!   denominator), and NULL latency values are excluded from their own
//!   coverage denominators — nothing is ever treated as 0.
//! - outputs never expose `route_id` / `target_id` UUIDs: groups are keyed
//!   by `route_name` (route scope) or `(provider, real_model)` (target
//!   scope).
//!
//! # Example (for the future router handler)
//!
//! ```rust,ignore
//! use sag_analytics::{AnalyticsQueryEngine, QueryParams, QueryScope};
//!
//! // `pool` is the shared analytics pool (`sag_analytics::open_analytics_pool`
//! // or `EventWriter::pool()`).
//! let engine = AnalyticsQueryEngine::new(pool);
//!
//! let params = QueryParams {
//!     scope: QueryScope::Route,          // or QueryScope::Target
//!     from_ms: 1_700_000_000_000,        // left-closed, unix ms
//!     to_ms: 1_700_003_600_000,          // right-open, unix ms, <= 24h later
//!     provider: None,                    // target scope only
//!     real_model: None,                 // target scope only
//!     route: None,                       // optional route_name filter (both scopes)
//! };
//!
//! let summary = engine.query_summary(&params).await?;
//! let trend = engine.query_trend(&params).await?;
//!
//! // summary.overall.requests / success_rate / model_failure_rate / ...
//! // summary.groups            -> Vec<GroupSummary>, keyed by route_name or
//! //                              (provider, real_model); never UUIDs.
//! // summary.unassigned_failures -> no_route / client_error counts (route scope).
//! // trend.buckets             -> per-UTC-hour overall metrics (zero-filled).
//! // trend.series              -> per-group per-hour request/success/token series.
//! ```

use crate::rollup::{
    GlobalRollupRow, HistogramBins, HistogramRow, OVERFLOW_BUCKET, RollupEngine,
    bucket_to_percentile, histogram_bucket_idx, merge_histogram_buckets,
};
use serde::Serialize;
use sqlx::sqlite::SqlitePool;
use std::collections::{HashMap, HashSet};

/// Maximum span (ms) answered exactly from the fact details.
///
/// Spans longer than this use the mixed path: full UTC hours inside
/// `[from, to - 24h)` come from the 1h rollup tables, the rest (recent
/// 24h, boundary partial hours, missing / dirty hours) from the details.
pub const MAX_EXACT_SPAN_MS: i64 = 24 * HOUR_MS;

/// Milliseconds per UTC hour.
pub const HOUR_MS: i64 = 3_600_000;

/// Latency metrics with fewer than this many valid samples are flagged
/// `low_sample = true` (spec: 有效样本 < 100).
pub const LOW_SAMPLE_THRESHOLD: u64 = 100;

/// Fallback display name if a `route_id` joins no `analytics_routes` row
/// (data anomaly; foreign keys normally prevent this).
const UNKNOWN_ROUTE: &str = "<unknown-route>";

/// Error type of the exact-detail query kernel.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// Invalid query parameters (from >= to, route scope with target-only
    /// filters, ...).
    #[error("invalid analytics query: {0}")]
    InvalidQuery(String),

    /// Reserved for API-layer compatibility: the mixed rollup path handles
    /// spans > 24h now, so this error is never produced by the kernel —
    /// the router handler still maps it to a client error for older
    /// kernels.
    #[error(
        "analytics query span {span_ms} ms exceeds the {max_ms} ms exact-detail limit; \
         spans > 24h require the 1h rollup path, which is not implemented yet"
    )]
    SpanTooLong { span_ms: i64, max_ms: i64 },

    /// Database failure while running a query.
    #[error("analytics query failed: {0}")]
    Database(String),
}

impl From<sqlx::Error> for QueryError {
    fn from(e: sqlx::Error) -> Self {
        QueryError::Database(e.to_string())
    }
}

/// Result alias of the query kernel.
pub type QueryResult<T> = Result<T, QueryError>;

/// Statistical scope: route-level final results or target-level attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryScope {
    /// Read `request_results` final results (one per request).
    #[default]
    Route,
    /// Read `target_attempts` actual attempts (one per upstream call).
    Target,
}

/// Parameters of a summary / trend query.
///
/// `from_ms` / `to_ms` are unix ms and define a **left-closed right-open**
/// interval: `completed_at >= from_ms AND completed_at < to_ms`. Spans
/// `<= 24h` are answered exactly from the details; longer spans use the
/// mixed rollup + detail path (see [`MAX_EXACT_SPAN_MS`]).
///
/// `provider` / `real_model` are **target-scope-only** filters; passing
/// them with `scope = route` is rejected with [`QueryError::InvalidQuery`].
/// `route` works in **both** scopes. An empty string is treated as "no
/// filter".
#[derive(Debug, Clone, Default)]
pub struct QueryParams {
    pub scope: QueryScope,
    /// Left-closed interval start, unix ms.
    pub from_ms: i64,
    /// Right-open interval end, unix ms.
    pub to_ms: i64,
    /// Target scope only: keep attempts whose `provider` equals this.
    pub provider: Option<String>,
    /// Target scope only: keep attempts whose `real_model` equals this
    /// (the API-surface name; the physical column stays `actual_model`).
    pub real_model: Option<String>,
    /// Optional: keep only facts whose route (`analytics_routes.route_name`)
    /// equals this, in **both** scopes. Route scope: `request_results`
    /// rows of that route (rows without a route identity, i.e. `no_route`
    /// / `client_error`, never match). Target scope: attempts whose
    /// **owning route** is this one.
    pub route: Option<String>,
}

/// Group key of an attributable group.
///
/// UUID identities (`route_id` / `target_id`) are never exposed: route
/// groups are keyed by the stable business identity `route_name`, target
/// groups by the `(provider, real_model)` tuple.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(untagged)]
pub enum GroupKey {
    /// Route scope: `{ "route_name": "..." }`.
    Route { route_name: String },
    /// Target scope: `{ "provider": "...", "real_model": "..." }`.
    Target {
        provider: String,
        real_model: String,
    },
}

/// Latency statistics of one dimension (e2e / ttft / upstream).
///
/// `coverage` is `samples / population` where the population is the
/// dimension's sample universe (route scope: requests that entered
/// scheduling — `success` / `model_failure` / `all_disabled` /
/// `client_cancelled`; target scope: all actual attempts). NULL values are
/// excluded from `samples` and never treated as 0.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LatencyMetric {
    /// Valid (non-NULL) samples.
    pub samples: u64,
    /// Coverage denominator (the metric's sample universe).
    pub population: u64,
    /// `samples / population`; `None` when the population is 0.
    pub coverage: Option<f64>,
    /// Arithmetic mean over valid samples, ms.
    pub avg_ms: Option<f64>,
    /// Nearest-rank 50th percentile, ms.
    pub p50_ms: Option<f64>,
    /// Nearest-rank 95th percentile, ms.
    pub p95_ms: Option<f64>,
    /// Nearest-rank 99th percentile, ms.
    pub p99_ms: Option<f64>,
    /// `true` when `samples < 100` (spec: 有效样本 < 100 → low_sample).
    pub low_sample: bool,
    /// `true` when the percentiles were estimated from the 1h rollup
    /// histogram (≈1% value error per spec) instead of exact raw samples —
    /// i.e. this block spans rollup (historical) data. Overflow-bucket
    /// estimates (latency beyond the histogram capacity) have no
    /// guaranteed precision. Serialized only when `true`, so exact queries
    /// keep emitting the legacy payload.
    #[serde(skip_serializing_if = "is_false")]
    pub approx_percentiles: bool,
}

/// Serialization guard for [`LatencyMetric::approx_percentiles`].
fn is_false(b: &bool) -> bool {
    !*b
}

/// One aggregated metric block (used for `overall` and per-group metrics).
///
/// Rate fields are `None` when their denominator is 0. Token / cache
/// fields only aggregate **final success** samples (route: `success`
/// requests; target: `success` attempts); failure / cancelled rows carry
/// NULL tokens which are never treated as 0.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MetricSummary {
    /// Route scope: `request_results` rows in the span. Target scope: 0.
    pub requests: u64,
    /// Target scope: `target_attempts` rows in the span. Route scope: 0.
    pub attempts: u64,

    /// Final-success samples (route and target scopes).
    pub success: u64,
    /// Route scope statuses (target scope keeps these at 0).
    pub client_error: u64,
    pub no_route: u64,
    pub all_disabled: u64,
    pub model_failure: u64,
    pub client_cancelled: u64,
    /// Target scope attempt statuses (route scope keeps these at 0).
    pub attempt_failure: u64,
    pub attempt_cancelled: u64,

    /// Non-success count (route scope: `requests - success`; target scope:
    /// `attempts - success`, i.e. `attempt_failure + attempt_cancelled`).
    pub failure: u64,

    /// 整体失败率 = non-success / all (route: non-success requests / all
    /// requests; target: non-success attempts / all attempts).
    pub overall_failure_rate: Option<f64>,
    /// 客户端错误率 = `client_error` / all requests (route scope only).
    pub client_error_rate: Option<f64>,
    /// 路由/配置失败率 = (`no_route` + `all_disabled`) / (all - `client_error`)
    /// (route scope only).
    pub route_config_failure_rate: Option<f64>,
    /// 模型失败率 = `model_failure` / (`success` + `model_failure`) (route
    /// scope only; `client_error` / `no_route` / `all_disabled` /
    /// `client_cancelled` are excluded from the denominator).
    pub model_failure_rate: Option<f64>,
    /// 成功率. Route scope **overall** blocks: `success` / all requests
    /// under analysis (every status included, so `success_rate` +
    /// `overall_failure_rate` = 1). Route scope **group** blocks: `success`
    /// / matched non-client-error requests. Target scope: `success` / all
    /// attempts.
    pub success_rate: Option<f64>,

    /// Token sums over final-success samples only.
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,

    /// Rates per wall-clock second of the query span (request / 输入 Token
    /// / 输出 Token 吞吐). `None` when the span is 0 (never, `to > from`).
    pub request_throughput_per_sec: Option<f64>,
    pub input_token_throughput_per_sec: Option<f64>,
    pub output_token_throughput_per_sec: Option<f64>,

    /// Cache statistics over success samples. `cache_known` = samples with
    /// non-NULL `cached_tokens` (the hit-rate denominator); `cache_hit` =
    /// of those, `cached_tokens > 0`; `cache_miss` = known − hit;
    /// `cache_unknown` = success samples with NULL `cached_tokens`
    /// (unknown ≠ miss, never counted as 0).
    pub cache_known: u64,
    pub cache_hit: u64,
    pub cache_miss: u64,
    pub cache_unknown: u64,
    /// 请求命中率 = `cache_hit` / `cache_known`; unknown samples are not in
    /// the denominator.
    pub request_cache_hit_rate: Option<f64>,
    /// Token 命中率 = Σ `COALESCE(cached_tokens, 0)` / Σ `prompt_tokens`
    /// over **all** success samples; a NULL / 0 `cached_tokens` contributes
    /// 0 to the numerator while its `prompt_tokens` stays in the
    /// denominator; `None` when the total success prompt_tokens is 0.
    pub token_cache_hit_rate: Option<f64>,
    /// Σ `prompt_tokens` over **all** success samples (the token hit-rate
    /// denominator; equals `prompt_tokens`).
    pub cache_prompt_tokens: u64,
    /// Σ `COALESCE(cached_tokens, 0)` over all success samples (token
    /// hit-rate numerator; NULL cached_tokens contribute 0).
    pub cache_tokens: u64,

    /// 端到端 / 上游 / TTFT 延迟（平均 + P50/P95/P99，各自独立 coverage）。
    pub e2e: LatencyMetric,
    pub ttft: LatencyMetric,
    pub upstream: LatencyMetric,
}

/// An attributable group's summary block.
#[derive(Debug, Clone, Serialize)]
pub struct GroupSummary {
    /// Route scope: `GroupKey::Route { route_name }`; target scope:
    /// `GroupKey::Target { provider, real_model }`. Never a UUID.
    pub key: GroupKey,
    /// Group metrics; the population fields (`requests` / `attempts`) and
    /// coverage denominators are scoped to this group.
    pub metrics: MetricSummary,
}

/// A failure category that cannot be attributed to any group.
///
/// Route scope: requests whose `route_id` is NULL — `no_route` and
/// `client_error` (they are excluded from every route group). Target scope
/// has no unassigned failures (every attempt carries an identity).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnassignedFailure {
    /// `final_status` category, e.g. `no_route` / `client_error`.
    pub category: String,
    pub count: u64,
}

/// Result of [`AnalyticsQueryEngine::query_summary`].
#[derive(Debug, Clone, Serialize)]
pub struct QuerySummary {
    pub scope: QueryScope,
    /// Left-closed interval start, unix ms.
    pub from_ms: i64,
    /// Right-open interval end, unix ms.
    pub to_ms: i64,
    /// Metrics across the whole span (all statuses / all attempts).
    pub overall: MetricSummary,
    /// Per-group metrics, sorted by population descending (then by key).
    pub groups: Vec<GroupSummary>,
    /// Unassigned failure categories (route scope; empty for target scope).
    pub unassigned_failures: Vec<UnassignedFailure>,
}

/// Per-hour metric bucket of a trend query (UTC hour, completion time).
#[derive(Debug, Clone, Serialize)]
pub struct TrendBucket {
    /// UTC hour start, unix ms.
    pub hour_ms: i64,
    /// Metrics of this hour (zero for empty hours).
    pub overall: MetricSummary,
}

/// One point of a group trend series (one UTC hour).
#[derive(Debug, Clone, Serialize)]
pub struct TrendPoint {
    /// UTC hour start, unix ms.
    pub hour_ms: i64,
    /// Route scope: matched non-client-error requests; target scope:
    /// actual attempts.
    pub requests: u64,
    /// Final-success samples of this group/hour.
    pub success: u64,
    /// Token sums over success samples of this group/hour.
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// One per-group time series of a trend query.
///
/// Points are zero-filled across every hour of the span so the future API
/// layer can chart continuous series and trim to Top20 + others without
/// re-querying.
#[derive(Debug, Clone, Serialize)]
pub struct GroupTrendSeries {
    pub key: GroupKey,
    /// Sum of `requests` over all points (for Top-N trimming / sorting).
    pub total_requests: u64,
    /// One point per UTC hour of the span, in hour order.
    pub points: Vec<TrendPoint>,
}

/// Result of [`AnalyticsQueryEngine::query_trend`].
#[derive(Debug, Clone, Serialize)]
pub struct TrendResult {
    pub scope: QueryScope,
    pub from_ms: i64,
    pub to_ms: i64,
    /// One bucket per UTC hour of the span, zero-filled.
    pub buckets: Vec<TrendBucket>,
    /// Per-group series, sorted by `total_requests` descending (then key).
    pub series: Vec<GroupTrendSeries>,
}

/// Exact-detail analytics query engine over the shared analytics pool.
///
/// Build it around the same pool the [`crate::EventWriter`] /
/// [`crate::IdentityRegistry`] use (e.g. `crate::open_analytics_pool(dsn)`
/// or `EventWriter::pool().clone()`); `:memory:` pools work for tests as
/// long as the migrations were applied to them.
pub struct AnalyticsQueryEngine {
    pool: SqlitePool,
}

impl AnalyticsQueryEngine {
    /// Wrap a ready pool (migrations applied, foreign keys enabled).
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Open a pool on `dsn` with the standard analytics connect options
    /// (see [`crate::open_analytics_pool`]) and wrap it.
    pub async fn open(dsn: &str) -> Result<Self, sqlx::Error> {
        Ok(Self::new(crate::open_analytics_pool(dsn).await?))
    }

    /// Access to the underlying pool.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Query overall + per-group metrics for the span. Spans ≤ 24h are
    /// answered exactly from the details; longer spans use the mixed
    /// rollup + detail path (see module docs).
    pub async fn query_summary(&self, params: &QueryParams) -> QueryResult<QuerySummary> {
        validate(params)?;
        if params.to_ms - params.from_ms > MAX_EXACT_SPAN_MS {
            return self.query_summary_mixed(params).await;
        }
        match params.scope {
            QueryScope::Route => {
                let (rows, lats) = self
                    .fetch_route_range(params, params.from_ms, params.to_ms)
                    .await?;
                Ok(aggregate_route_rows(&rows, &lats).finish(params))
            }
            QueryScope::Target => {
                let (rows, lats) = self
                    .fetch_target_range(params, params.from_ms, params.to_ms)
                    .await?;
                Ok(aggregate_target_rows(&rows, &lats).finish(params))
            }
        }
    }

    /// Query per-UTC-hour overall buckets + per-group series. Spans ≤ 24h
    /// are answered exactly from the details; longer spans use the mixed
    /// rollup + detail path. Buckets and series are zero-filled for hours
    /// without data.
    pub async fn query_trend(&self, params: &QueryParams) -> QueryResult<TrendResult> {
        validate(params)?;
        if params.to_ms - params.from_ms > MAX_EXACT_SPAN_MS {
            return self.query_trend_mixed(params).await;
        }
        match params.scope {
            QueryScope::Route => {
                let (rows, lats) = self
                    .fetch_route_range(params, params.from_ms, params.to_ms)
                    .await?;
                let agg = aggregate_trend(&rows, &lats, route_trend_row, route_trend_lat);
                Ok(finish_trend(agg, params, |acc| {
                    acc.total_requests()
                        .saturating_sub(acc.client_error + acc.no_route)
                }))
            }
            QueryScope::Target => {
                let (rows, lats) = self
                    .fetch_target_range(params, params.from_ms, params.to_ms)
                    .await?;
                let agg = aggregate_trend(&rows, &lats, target_trend_row, target_trend_lat);
                Ok(finish_trend(agg, params, MetricAccumulator::total_attempts))
            }
        }
    }

    /// Mixed path for spans > 24h: historical full UTC hours from the 1h
    /// rollup tables, the rest from the details (see [`mixed_split`]).
    async fn query_summary_mixed(&self, params: &QueryParams) -> QueryResult<QuerySummary> {
        match params.scope {
            QueryScope::Route => Ok(self.mixed_route(params).await?.finish(params)),
            QueryScope::Target => Ok(self.mixed_target(params).await?.finish(params)),
        }
    }

    /// Mixed path for trend queries (see [`mixed_split`]).
    async fn query_trend_mixed(&self, params: &QueryParams) -> QueryResult<TrendResult> {
        let split = self.mixed_split(params).await?;
        let mut agg = TrendAggregates::default();
        match params.scope {
            QueryScope::Route => {
                for (from, to) in &split.detail_intervals {
                    let (rows, lats) = self.fetch_route_range(params, *from, *to).await?;
                    agg.merge(aggregate_trend(
                        &rows,
                        &lats,
                        route_trend_row,
                        route_trend_lat,
                    ));
                }
                if !split.rollup_hours.is_empty() {
                    let hours = &split.rollup_hours;
                    let r_rows = self.fetch_rollup_route(params, hours).await?;
                    let route_ids: Vec<&str> = r_rows.iter().map(|r| r.route_id.as_str()).collect();
                    let filtered = params.route.as_deref().is_some_and(|r| !r.is_empty());
                    let (globals, hist) = if filtered {
                        (
                            Vec::new(),
                            self.rollup()
                                .histogram_rows(hours, "route", Some(&route_ids))
                                .await
                                .map_err(app_err)?,
                        )
                    } else {
                        (
                            self.rollup().global_rows(hours).await.map_err(app_err)?,
                            self.rollup()
                                .global_histogram_rows(hours)
                                .await
                                .map_err(app_err)?,
                        )
                    };
                    agg.merge(aggregate_rollup_route_trend(&r_rows, &globals, &hist));
                }
                Ok(finish_trend(agg, params, |acc| {
                    acc.total_requests()
                        .saturating_sub(acc.client_error + acc.no_route)
                }))
            }
            QueryScope::Target => {
                for (from, to) in &split.detail_intervals {
                    let (rows, lats) = self.fetch_target_range(params, *from, *to).await?;
                    agg.merge(aggregate_trend(
                        &rows,
                        &lats,
                        target_trend_row,
                        target_trend_lat,
                    ));
                }
                if !split.rollup_hours.is_empty() {
                    let hours = &split.rollup_hours;
                    let rows = self.fetch_rollup_target(params, hours).await?;
                    let target_ids: Vec<&str> = rows.iter().map(|r| r.target_id.as_str()).collect();
                    let filtered = target_scope_filtered(params);
                    let hist = self
                        .rollup()
                        .histogram_rows(
                            hours,
                            "target",
                            if filtered { Some(&target_ids) } else { None },
                        )
                        .await
                        .map_err(app_err)?;
                    agg.merge(aggregate_rollup_target_trend(&rows, &hist));
                }
                Ok(finish_trend(agg, params, MetricAccumulator::total_attempts))
            }
        }
    }

    /// Shared mixed split: rollup hours + detail intervals + registration.
    ///
    /// The rollup region covers the full UTC hours fully inside
    /// `[from, to - 24h)` (see [`rollup_region`]); everything else — the
    /// boundary partial hours, the recent 24h and every hour whose bucket
    /// is not `completed` and clean — is answered from the facts. Missing /
    /// dirty / failed hours are re-registered for backfill (pending only,
    /// never computed here).
    async fn mixed_split(&self, params: &QueryParams) -> QueryResult<MixedSplit> {
        let (r_from, r_to) = rollup_region(params.from_ms, params.to_ms);
        let mut split = MixedSplit {
            rollup_hours: Vec::new(),
            detail_intervals: Vec::new(),
        };
        if params.from_ms < r_from {
            split.detail_intervals.push((params.from_ms, r_from));
        }
        if r_to < params.to_ms {
            split.detail_intervals.push((r_to, params.to_ms));
        }
        if r_from < r_to {
            let hours = bucket_hours(r_from, r_to);
            let completed: HashSet<i64> = self
                .rollup()
                .completed_hours(r_from, r_to)
                .await
                .map_err(app_err)?
                .into_iter()
                .collect();
            let missing: Vec<i64> = hours
                .iter()
                .copied()
                .filter(|h| !completed.contains(h))
                .collect();
            for (run_from, run_to) in contiguous_hour_runs(&missing) {
                split.detail_intervals.push((run_from, run_to));
                self.rollup()
                    .register_missing_hours(run_from, run_to)
                    .await
                    .map_err(app_err)?;
            }
            // Only the completed-and-clean hours are answered from the
            // rollup tables; missing / dirty hours never read their (stale)
            // rollup rows — they come from the detail backfill above.
            split.rollup_hours = hours
                .into_iter()
                .filter(|h| completed.contains(h))
                .collect();
        }
        Ok(split)
    }

    /// Mixed route-scope aggregation: rollup part + detail backfill.
    async fn mixed_route(&self, params: &QueryParams) -> QueryResult<RouteAggregates> {
        let split = self.mixed_split(params).await?;
        let mut agg = RouteAggregates::default();
        for (from, to) in &split.detail_intervals {
            let (rows, lats) = self.fetch_route_range(params, *from, *to).await?;
            agg.merge(aggregate_route_rows(&rows, &lats));
        }
        if !split.rollup_hours.is_empty() {
            let hours = &split.rollup_hours;
            let r_rows = self.fetch_rollup_route(params, hours).await?;
            let route_ids: Vec<&str> = r_rows.iter().map(|r| r.route_id.as_str()).collect();
            let filtered = params.route.as_deref().is_some_and(|r| !r.is_empty());
            if filtered {
                let hist = self
                    .rollup()
                    .histogram_rows(hours, "route", Some(&route_ids))
                    .await
                    .map_err(app_err)?;
                if !r_rows.is_empty() {
                    agg.merge(aggregate_rollup_route(&r_rows, &[], &hist, &hist));
                }
            } else {
                let globals = self.rollup().global_rows(hours).await.map_err(app_err)?;
                let overall_hist = self
                    .rollup()
                    .global_histogram_rows(hours)
                    .await
                    .map_err(app_err)?;
                let group_hist = self
                    .rollup()
                    .histogram_rows(hours, "route", Some(&route_ids))
                    .await
                    .map_err(app_err)?;
                // No rollup data at all (e.g. an empty historical region) must
                // not fabricate an (empty) histogram block.
                if !r_rows.is_empty() || !globals.is_empty() {
                    agg.merge(aggregate_rollup_route(
                        &r_rows,
                        &globals,
                        &overall_hist,
                        &group_hist,
                    ));
                }
            }
        }
        Ok(agg)
    }

    /// Mixed target-scope aggregation: rollup part + detail backfill.
    async fn mixed_target(&self, params: &QueryParams) -> QueryResult<TargetAggregates> {
        let split = self.mixed_split(params).await?;
        let mut agg = TargetAggregates::default();
        for (from, to) in &split.detail_intervals {
            let (rows, lats) = self.fetch_target_range(params, *from, *to).await?;
            agg.merge(aggregate_target_rows(&rows, &lats));
        }
        if !split.rollup_hours.is_empty() {
            let hours = &split.rollup_hours;
            let rows = self.fetch_rollup_target(params, hours).await?;
            let target_ids: Vec<&str> = rows.iter().map(|r| r.target_id.as_str()).collect();
            let hist = self
                .rollup()
                .histogram_rows(
                    hours,
                    "target",
                    if target_scope_filtered(params) {
                        Some(&target_ids)
                    } else {
                        None
                    },
                )
                .await
                .map_err(app_err)?;
            if !rows.is_empty() {
                agg.merge(aggregate_rollup_target(&rows, &hist));
            }
        }
        Ok(agg)
    }

    /// A [`RollupEngine`] over the same shared pool (stateless handle).
    fn rollup(&self) -> RollupEngine {
        RollupEngine::new(self.pool.clone())
    }

    async fn fetch_route_range(
        &self,
        params: &QueryParams,
        from_ms: i64,
        to_ms: i64,
    ) -> QueryResult<(Vec<RouteAggRow>, Vec<RouteLatRow>)> {
        let (where_clause, binds) = route_where_clause(params);
        let agg_sql = format!(
            "{}{where_clause}\nGROUP BY hour_ms, rr.route_id, rr.final_status",
            route_agg_sql_base()
        );
        let lat_sql = format!("{}{where_clause}", route_lat_sql_base());

        let mut agg_q = sqlx::query_as::<_, RouteAggRow>(&agg_sql);
        agg_q = agg_q.bind(from_ms).bind(to_ms);
        let mut lat_q = sqlx::query_as::<_, RouteLatRow>(&lat_sql);
        lat_q = lat_q.bind(from_ms).bind(to_ms);
        for b in binds {
            agg_q = agg_q.bind(b);
            lat_q = lat_q.bind(b);
        }
        Ok((
            agg_q.fetch_all(&self.pool).await?,
            lat_q.fetch_all(&self.pool).await?,
        ))
    }

    async fn fetch_target_range(
        &self,
        params: &QueryParams,
        from_ms: i64,
        to_ms: i64,
    ) -> QueryResult<(Vec<TargetAggRow>, Vec<TargetLatRow>)> {
        let (where_clause, binds) = target_where_clause(params);
        let agg_sql = format!(
            "{}{where_clause}\nGROUP BY hour_ms, art.provider, ta.actual_model, ta.status",
            target_agg_sql_base()
        );
        let lat_sql = format!("{}{where_clause}", target_lat_sql_base());

        let mut agg_q = sqlx::query_as::<_, TargetAggRow>(&agg_sql);
        agg_q = agg_q.bind(from_ms).bind(to_ms);
        let mut lat_q = sqlx::query_as::<_, TargetLatRow>(&lat_sql);
        lat_q = lat_q.bind(from_ms).bind(to_ms);
        for b in binds {
            agg_q = agg_q.bind(b);
            lat_q = lat_q.bind(b);
        }
        Ok((
            agg_q.fetch_all(&self.pool).await?,
            lat_q.fetch_all(&self.pool).await?,
        ))
    }

    /// Historical route rows (rollup_route_hourly JOIN analytics_routes for
    /// the display name), filtered like the detail path.
    async fn fetch_rollup_route(
        &self,
        params: &QueryParams,
        hours: &[i64],
    ) -> QueryResult<Vec<RollupRouteRow>> {
        if hours.is_empty() {
            return Ok(Vec::new());
        }
        let (where_clause, binds) = route_where_clause(params);
        let sql = format!(
            "{ROLLUP_ROUTE_SQL_BASE}\nWHERE rr.hour IN ({}){where_clause}\nORDER BY rr.hour, rr.route_id",
            placeholders(hours.len())
        );
        let mut q = sqlx::query_as::<_, RollupRouteRow>(&sql);
        for h in hours {
            q = q.bind(*h);
        }
        for b in binds {
            q = q.bind(b);
        }
        Ok(q.fetch_all(&self.pool).await?)
    }

    /// Historical target rows (rollup_target_hourly JOIN
    /// analytics_route_targets for provider / real_model), filtered like
    /// the detail path.
    async fn fetch_rollup_target(
        &self,
        params: &QueryParams,
        hours: &[i64],
    ) -> QueryResult<Vec<RollupTargetRow>> {
        if hours.is_empty() {
            return Ok(Vec::new());
        }
        let (where_clause, binds) = target_rollup_filter(params);
        let sql = format!(
            "{ROLLUP_TARGET_SQL_BASE}\nWHERE rt.hour IN ({}){where_clause}\nORDER BY rt.hour, rt.target_id",
            placeholders(hours.len())
        );
        let mut q = sqlx::query_as::<_, RollupTargetRow>(&sql);
        for h in hours {
            q = q.bind(*h);
        }
        for b in binds {
            q = q.bind(b);
        }
        Ok(q.fetch_all(&self.pool).await?)
    }
}

/// `?` placeholders for `n` bind values.
fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

/// Convert a rollup (sag_core) error into a kernel [`QueryError`].
fn app_err(e: sag_core::AppError) -> QueryError {
    QueryError::Database(format!("analytics rollup: {e}"))
}

/// Whether any target-scope filter (provider / real_model / route) is
/// active (empty strings count as no filter, matching the detail path).
fn target_scope_filtered(params: &QueryParams) -> bool {
    params.provider.as_deref().is_some_and(|p| !p.is_empty())
        || params.real_model.as_deref().is_some_and(|m| !m.is_empty())
        || params.route.as_deref().is_some_and(|r| !r.is_empty())
}

/// Validate query parameters (interval shape, scope filters). The span
/// limit is checked by the callers: ≤ 24h takes the exact path, > 24h the
/// mixed rollup + detail path.
fn validate(params: &QueryParams) -> QueryResult<()> {
    if params.from_ms >= params.to_ms {
        return Err(QueryError::InvalidQuery(format!(
            "from_ms ({}) must be < to_ms ({})",
            params.from_ms, params.to_ms
        )));
    }
    if params.scope == QueryScope::Route
        && (params.provider.is_some() || params.real_model.is_some())
    {
        return Err(QueryError::InvalidQuery(
            "provider / real_model filters are only supported for scope = target".to_string(),
        ));
    }
    Ok(())
}

/// Route scope: per-(hour, route, status) counts, tokens and cache sums.
#[derive(sqlx::FromRow)]
struct RouteAggRow {
    hour_ms: i64,
    route_id: Option<String>,
    route_name: Option<String>,
    status: String,
    cnt: i64,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    total_tokens: Option<i64>,
    cache_tokens_sum: Option<i64>,
    cache_prompt_tokens: Option<i64>,
    cache_known: i64,
    cache_hit: i64,
    cache_unknown: i64,
}

/// Route scope: latency values of the e2e population (requests that
/// entered scheduling), with their hour for trend bucketing.
#[derive(sqlx::FromRow)]
struct RouteLatRow {
    hour_ms: i64,
    route_id: Option<String>,
    duration_ms: Option<i64>,
    ttft_ms: Option<i64>,
    upstream_duration_ms: Option<i64>,
}

/// Target scope: per-(hour, provider, actual_model, status) aggregates.
#[derive(sqlx::FromRow)]
struct TargetAggRow {
    hour_ms: i64,
    provider: String,
    actual_model: String,
    status: String,
    cnt: i64,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    total_tokens: Option<i64>,
    cache_tokens_sum: Option<i64>,
    cache_prompt_tokens: Option<i64>,
    cache_known: i64,
    cache_hit: i64,
    cache_unknown: i64,
}

/// Target scope: latency values of every actual attempt.
#[derive(sqlx::FromRow)]
struct TargetLatRow {
    hour_ms: i64,
    provider: String,
    actual_model: String,
    duration_ms: Option<i64>,
    ttft_ms: Option<i64>,
    upstream_duration_ms: Option<i64>,
}

/// SQL text is built via `format!` at each call site; interpolating
/// `HOUR_MS` here (instead of hardcoding `3600000`) keeps the hour-bucket
/// expression in sync with the crate constant.
fn route_agg_sql_base() -> String {
    format!(
        r#"
SELECT
    (rr.completed_at / {HOUR_MS}) * {HOUR_MS} AS hour_ms,
    rr.route_id,
    ar.route_name,
    rr.final_status AS status,
    COUNT(*) AS cnt,
    SUM(CASE WHEN rr.final_status = 'success' THEN rr.prompt_tokens END)     AS prompt_tokens,
    SUM(CASE WHEN rr.final_status = 'success' THEN rr.completion_tokens END) AS completion_tokens,
    SUM(CASE WHEN rr.final_status = 'success' THEN rr.total_tokens END)      AS total_tokens,
    SUM(CASE WHEN rr.final_status = 'success' AND rr.cached_tokens IS NOT NULL
             THEN rr.cached_tokens END)                                       AS cache_tokens_sum,
    SUM(CASE WHEN rr.final_status = 'success'
             THEN rr.prompt_tokens END)                                       AS cache_prompt_tokens,
    SUM(CASE WHEN rr.final_status = 'success' AND rr.cached_tokens IS NOT NULL
             THEN 1 ELSE 0 END)                                               AS cache_known,
    SUM(CASE WHEN rr.final_status = 'success' AND rr.cached_tokens > 0
             THEN 1 ELSE 0 END)                                               AS cache_hit,
    SUM(CASE WHEN rr.final_status = 'success' AND rr.cached_tokens IS NULL
             THEN 1 ELSE 0 END)                                               AS cache_unknown
FROM request_results rr
LEFT JOIN analytics_routes ar ON ar.route_id = rr.route_id
WHERE rr.completed_at >= ? AND rr.completed_at < ?
"#
    )
}

fn route_lat_sql_base() -> String {
    format!(
        r#"
SELECT
    (rr.completed_at / {HOUR_MS}) * {HOUR_MS} AS hour_ms,
    rr.route_id,
    rr.duration_ms,
    rr.ttft_ms,
    rr.upstream_duration_ms
FROM request_results rr
LEFT JOIN analytics_routes ar ON ar.route_id = rr.route_id
WHERE rr.completed_at >= ? AND rr.completed_at < ?
  AND rr.final_status IN ('success', 'model_failure', 'all_disabled', 'client_cancelled')
"#
    )
}

fn target_agg_sql_base() -> String {
    format!(
        r#"
SELECT
    (ta.completed_at / {HOUR_MS}) * {HOUR_MS} AS hour_ms,
    art.provider,
    ta.actual_model,
    ta.status,
    COUNT(*) AS cnt,
    SUM(CASE WHEN ta.status = 'success' THEN ta.prompt_tokens END)     AS prompt_tokens,
    SUM(CASE WHEN ta.status = 'success' THEN ta.completion_tokens END) AS completion_tokens,
    SUM(CASE WHEN ta.status = 'success' THEN ta.total_tokens END)      AS total_tokens,
    SUM(CASE WHEN ta.status = 'success' AND ta.cached_tokens IS NOT NULL
             THEN ta.cached_tokens END)                                 AS cache_tokens_sum,
    SUM(CASE WHEN ta.status = 'success'
             THEN ta.prompt_tokens END)                                 AS cache_prompt_tokens,
    SUM(CASE WHEN ta.status = 'success' AND ta.cached_tokens IS NOT NULL
             THEN 1 ELSE 0 END)                                         AS cache_known,
    SUM(CASE WHEN ta.status = 'success' AND ta.cached_tokens > 0
             THEN 1 ELSE 0 END)                                         AS cache_hit,
    SUM(CASE WHEN ta.status = 'success' AND ta.cached_tokens IS NULL
             THEN 1 ELSE 0 END)                                         AS cache_unknown
FROM target_attempts ta
JOIN analytics_route_targets art ON art.target_id = ta.target_id
WHERE ta.completed_at >= ? AND ta.completed_at < ?
"#
    )
}

fn target_lat_sql_base() -> String {
    format!(
        r#"
SELECT
    (ta.completed_at / {HOUR_MS}) * {HOUR_MS} AS hour_ms,
    art.provider,
    ta.actual_model,
    ta.duration_ms,
    ta.ttft_ms,
    ta.upstream_duration_ms
FROM target_attempts ta
JOIN analytics_route_targets art ON art.target_id = ta.target_id
WHERE ta.completed_at >= ? AND ta.completed_at < ?
"#
    )
}

/// Extra `WHERE` predicate for the optional route-name filter (both
/// scopes), with the bound value in matching order. Empty strings are
/// treated as no filter.
///
/// The predicate matches `analytics_routes.route_name`, so rows without a
/// route identity (`no_route` / `client_error`) never match.
fn route_where_clause(params: &QueryParams) -> (String, Vec<&str>) {
    let mut clause = String::new();
    let mut binds = Vec::new();
    if let Some(r) = params.route.as_deref().filter(|r| !r.is_empty()) {
        clause.push_str(" AND ar.route_name = ?");
        binds.push(r);
    }
    (clause, binds)
}

/// Extra `WHERE` predicates for target-scope filters, with the bound
/// values in matching order. Empty strings are treated as no filter.
fn target_where_clause(params: &QueryParams) -> (String, Vec<&str>) {
    let mut clause = String::new();
    let mut binds = Vec::new();
    if let Some(p) = params.provider.as_deref().filter(|p| !p.is_empty()) {
        clause.push_str(" AND art.provider = ?");
        binds.push(p);
    }
    if let Some(m) = params.real_model.as_deref().filter(|m| !m.is_empty()) {
        clause.push_str(" AND ta.actual_model = ?");
        binds.push(m);
    }
    // The route filter follows the attempt's owning route: target_attempts
    // carries no route identity, so resolve `route_name -> route_id` through
    // the identity registry (UNIQUE index on route_name).
    if let Some(r) = params.route.as_deref().filter(|r| !r.is_empty()) {
        clause.push_str(
            " AND art.route_id = (SELECT route_id FROM analytics_routes WHERE route_name = ?)",
        );
        binds.push(r);
    }
    (clause, binds)
}

// ═══════════════════════════════════════════════════════════════════
// Assembly
// ═══════════════════════════════════════════════════════════════════

/// Numeric row data shared by both scopes (tokens / cache sums only
/// aggregate over success, see the SQL).
#[derive(Debug, Clone, Copy, Default)]
struct RowCounts {
    cnt: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    cache_tokens_sum: u64,
    cache_prompt_tokens: u64,
    cache_known: u64,
    cache_hit: u64,
    cache_unknown: u64,
}

impl From<&RouteAggRow> for RowCounts {
    fn from(r: &RouteAggRow) -> Self {
        Self {
            cnt: r.cnt.max(0) as u64,
            prompt_tokens: r.prompt_tokens.unwrap_or(0).max(0) as u64,
            completion_tokens: r.completion_tokens.unwrap_or(0).max(0) as u64,
            total_tokens: r.total_tokens.unwrap_or(0).max(0) as u64,
            cache_tokens_sum: r.cache_tokens_sum.unwrap_or(0).max(0) as u64,
            cache_prompt_tokens: r.cache_prompt_tokens.unwrap_or(0).max(0) as u64,
            cache_known: r.cache_known.max(0) as u64,
            cache_hit: r.cache_hit.max(0) as u64,
            cache_unknown: r.cache_unknown.max(0) as u64,
        }
    }
}

impl From<&TargetAggRow> for RowCounts {
    fn from(r: &TargetAggRow) -> Self {
        Self {
            cnt: r.cnt.max(0) as u64,
            prompt_tokens: r.prompt_tokens.unwrap_or(0).max(0) as u64,
            completion_tokens: r.completion_tokens.unwrap_or(0).max(0) as u64,
            total_tokens: r.total_tokens.unwrap_or(0).max(0) as u64,
            cache_tokens_sum: r.cache_tokens_sum.unwrap_or(0).max(0) as u64,
            cache_prompt_tokens: r.cache_prompt_tokens.unwrap_or(0).max(0) as u64,
            cache_known: r.cache_known.max(0) as u64,
            cache_hit: r.cache_hit.max(0) as u64,
            cache_unknown: r.cache_unknown.max(0) as u64,
        }
    }
}

/// Accumulates one metric block (overall, per group or per hour).
#[derive(Debug, Default, Clone)]
struct MetricAccumulator {
    success: u64,
    client_error: u64,
    no_route: u64,
    all_disabled: u64,
    model_failure: u64,
    client_cancelled: u64,
    target_failure: u64,
    target_cancelled: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
    cache_tokens_sum: u64,
    cache_prompt_tokens: u64,
    cache_known: u64,
    cache_hit: u64,
    cache_unknown: u64,
}

impl MetricAccumulator {
    /// Fold one aggregated row. Both `final_status` and target `status`
    /// strings are accepted; unknown values (impossible under the DB CHECK
    /// constraints) are ignored defensively.
    fn add(&mut self, status: &str, rc: RowCounts) {
        match status {
            "success" => self.success += rc.cnt,
            "client_error" => self.client_error += rc.cnt,
            "no_route" => self.no_route += rc.cnt,
            "all_disabled" => self.all_disabled += rc.cnt,
            "model_failure" => self.model_failure += rc.cnt,
            "client_cancelled" => self.client_cancelled += rc.cnt,
            "failure" => self.target_failure += rc.cnt,
            "cancelled" => self.target_cancelled += rc.cnt,
            other => tracing::debug!(status = other, "ignoring unknown analytics status row"),
        }
        self.prompt_tokens += rc.prompt_tokens;
        self.completion_tokens += rc.completion_tokens;
        self.total_tokens += rc.total_tokens;
        self.cache_tokens_sum += rc.cache_tokens_sum;
        self.cache_prompt_tokens += rc.cache_prompt_tokens;
        self.cache_known += rc.cache_known;
        self.cache_hit += rc.cache_hit;
        self.cache_unknown += rc.cache_unknown;
    }

    /// Fold another accumulator's counts into this one (rollup + detail
    /// merge of the mixed query path).
    fn merge(&mut self, other: &Self) {
        self.success += other.success;
        self.client_error += other.client_error;
        self.no_route += other.no_route;
        self.all_disabled += other.all_disabled;
        self.model_failure += other.model_failure;
        self.client_cancelled += other.client_cancelled;
        self.target_failure += other.target_failure;
        self.target_cancelled += other.target_cancelled;
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
        self.cache_tokens_sum += other.cache_tokens_sum;
        self.cache_prompt_tokens += other.cache_prompt_tokens;
        self.cache_known += other.cache_known;
        self.cache_hit += other.cache_hit;
        self.cache_unknown += other.cache_unknown;
    }

    fn total_requests(&self) -> u64 {
        self.success
            + self.client_error
            + self.no_route
            + self.all_disabled
            + self.model_failure
            + self.client_cancelled
    }

    fn total_attempts(&self) -> u64 {
        self.success + self.target_failure + self.target_cancelled
    }

    /// Render the accumulated counts into a [`MetricSummary`].
    ///
    /// `population` is the latency coverage denominator for this block
    /// (route overall: matched non-client-error requests; groups: the
    /// group's requests; target scope: attempts).
    ///
    /// `overall` selects the route-scope `success_rate` semantics of this
    /// block: overall blocks (span-wide or per-hour) count **every**
    /// request under analysis — `no_route` / `client_error` included — so
    /// that `success_rate` complements `overall_failure_rate` over the same
    /// population; group blocks keep the route view (`success` / matched
    /// non-client-error requests). Target scope is unaffected by the flag.
    fn finish(
        &self,
        scope: QueryScope,
        span_ms: i64,
        latency: &LatencySamples,
        population: u64,
        overall: bool,
    ) -> MetricSummary {
        let (requests, attempts) = match scope {
            QueryScope::Route => (self.total_requests(), 0),
            QueryScope::Target => (0, self.total_attempts()),
        };
        let total = if scope == QueryScope::Route {
            requests
        } else {
            attempts
        };
        let success = self.success;
        let failure = total - success;

        let success_rate = match (scope, overall) {
            // Overall: every request under analysis is the denominator, so
            // success_rate + overall_failure_rate = 1 (no_route /
            // client_error stay in the population).
            (QueryScope::Route, true) => rate(success, total),
            // Group: route view — matched non-client-error requests only.
            (QueryScope::Route, false) => rate(
                success,
                total.saturating_sub(self.client_error + self.no_route),
            ),
            (QueryScope::Target, _) => rate(success, total),
        };

        let span_secs = span_ms as f64 / 1000.0;
        let rate_per_sec = |v: u64| (span_secs > 0.0).then_some(v as f64 / span_secs);

        MetricSummary {
            requests,
            attempts,
            success,
            client_error: self.client_error,
            no_route: self.no_route,
            all_disabled: self.all_disabled,
            model_failure: self.model_failure,
            client_cancelled: self.client_cancelled,
            attempt_failure: self.target_failure,
            attempt_cancelled: self.target_cancelled,
            failure,
            overall_failure_rate: rate(failure, total),
            client_error_rate: match scope {
                QueryScope::Route => rate(self.client_error, total),
                QueryScope::Target => None,
            },
            route_config_failure_rate: match scope {
                QueryScope::Route => rate(
                    self.no_route + self.all_disabled,
                    total.saturating_sub(self.client_error),
                ),
                QueryScope::Target => None,
            },
            model_failure_rate: match scope {
                QueryScope::Route => rate(self.model_failure, self.success + self.model_failure),
                QueryScope::Target => None,
            },
            success_rate,
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            total_tokens: self.total_tokens,
            request_throughput_per_sec: rate_per_sec(total),
            input_token_throughput_per_sec: rate_per_sec(self.prompt_tokens),
            output_token_throughput_per_sec: rate_per_sec(self.completion_tokens),
            cache_known: self.cache_known,
            cache_hit: self.cache_hit,
            cache_miss: self.cache_known - self.cache_hit,
            cache_unknown: self.cache_unknown,
            request_cache_hit_rate: rate(self.cache_hit, self.cache_known),
            token_cache_hit_rate: rate(self.cache_tokens_sum, self.cache_prompt_tokens),
            cache_prompt_tokens: self.cache_prompt_tokens,
            cache_tokens: self.cache_tokens_sum,
            e2e: latency.e2e.finish(population),
            ttft: latency.ttft.finish(population),
            upstream: latency.upstream.finish(population),
        }
    }
}

/// One dimension's latency source: exact raw values (detail path) or the
/// rollup histogram (approximate path). Blocks mixing both degrade to the
/// histogram (≈1% value error per spec); the average stays exact because
/// the rollup rows carry `*_sum_ms`.
#[derive(Debug, Clone)]
enum LatencySource {
    /// Exact latency values of the fact details.
    Raw(Vec<i64>),
    /// Rollup histogram bins plus the exact per-dimension sum (avg only).
    Hist { bins: HistogramBins, sum_ms: i64 },
}

impl Default for LatencySource {
    fn default() -> Self {
        LatencySource::Raw(Vec::new())
    }
}

impl LatencySource {
    /// Push one exact value; only valid for `Raw` sources (the detail
    /// aggregation never touches an already-histogram source).
    fn push(&mut self, v: i64) {
        match self {
            LatencySource::Raw(values) => values.push(v),
            LatencySource::Hist { .. } => {
                debug_assert!(false, "push on a histogram latency source");
            }
        }
    }

    /// Fold `other` into `self`; any histogram component makes the result
    /// approximate (raw values are re-bucketed into the histogram).
    fn merge(&mut self, other: LatencySource) {
        let this = std::mem::take(self);
        *self = match (this, other) {
            (LatencySource::Raw(mut a), LatencySource::Raw(b)) => {
                a.extend(b);
                LatencySource::Raw(a)
            }
            (LatencySource::Raw(a), LatencySource::Hist { bins, sum_ms }) => {
                let (bins, sum) = raw_to_bins(&a, sum_ms, bins);
                LatencySource::Hist { bins, sum_ms: sum }
            }
            (LatencySource::Hist { bins, sum_ms }, LatencySource::Raw(b)) => {
                let (bins, sum) = raw_to_bins(&b, sum_ms, bins);
                LatencySource::Hist { bins, sum_ms: sum }
            }
            (
                LatencySource::Hist {
                    bins: mut a,
                    sum_ms: sa,
                },
                LatencySource::Hist {
                    bins: b,
                    sum_ms: sb,
                },
            ) => {
                for (idx, count) in b.counts {
                    *a.counts.entry(idx).or_default() += count;
                }
                a.overflow += b.overflow;
                LatencySource::Hist {
                    bins: a,
                    sum_ms: sa + sb,
                }
            }
        };
    }

    /// Total valid samples (bins include the overflow bucket).
    fn samples(&self) -> u64 {
        match self {
            LatencySource::Raw(values) => values.len() as u64,
            LatencySource::Hist { bins, .. } => bins.counts.values().sum::<u64>() + bins.overflow,
        }
    }

    fn avg_ms(&self) -> Option<f64> {
        let samples = self.samples();
        (samples > 0).then(|| match self {
            LatencySource::Raw(values) => {
                values.iter().map(|&v| v as f64).sum::<f64>() / samples as f64
            }
            LatencySource::Hist { sum_ms, .. } => *sum_ms as f64 / samples as f64,
        })
    }

    fn percentile(&self, p: f64) -> Option<f64> {
        match self {
            LatencySource::Raw(values) => {
                let mut sorted = values.clone();
                sorted.sort_unstable();
                nearest_rank_percentile(&sorted, p)
            }
            LatencySource::Hist { bins, .. } => bucket_to_percentile(bins, p),
        }
    }

    fn approx(&self) -> bool {
        matches!(self, LatencySource::Hist { .. })
    }

    /// Render into a [`LatencyMetric`]; `population` is the coverage
    /// denominator of the block.
    fn finish(&self, population: u64) -> LatencyMetric {
        let samples = self.samples();
        LatencyMetric {
            samples,
            population,
            coverage: (population > 0).then_some(samples as f64 / population as f64),
            avg_ms: self.avg_ms(),
            p50_ms: self.percentile(50.0),
            p95_ms: self.percentile(95.0),
            p99_ms: self.percentile(99.0),
            low_sample: samples < LOW_SAMPLE_THRESHOLD,
            // The flag describes the *percentiles*: with no samples there
            // are none, so an empty rollup block renders like an empty
            // detail block (both exact-style, all percentiles `None`).
            approx_percentiles: samples > 0 && self.approx(),
        }
    }
}

/// Convert raw latency values into histogram bins (bucketed per the rollup
/// convention), adding their sum.
fn raw_to_bins(values: &[i64], sum_ms: i64, mut bins: HistogramBins) -> (HistogramBins, i64) {
    let mut sum = sum_ms;
    for &v in values {
        sum += v;
        let idx = histogram_bucket_idx(v);
        if idx == OVERFLOW_BUCKET {
            bins.overflow += 1;
        } else {
            *bins.counts.entry(idx).or_default() += 1;
        }
    }
    (bins, sum)
}

/// Latency values of one block, per dimension: exact raw samples (detail
/// path) or the rollup histogram (approximate path).
#[derive(Debug, Default)]
struct LatencySamples {
    e2e: LatencySource,
    ttft: LatencySource,
    upstream: LatencySource,
}

impl LatencySamples {
    /// Push one fact's latency values; NULLs stay excluded (never 0).
    fn push(&mut self, duration_ms: Option<i64>, ttft_ms: Option<i64>, upstream_ms: Option<i64>) {
        if let Some(v) = duration_ms {
            self.e2e.push(v);
        }
        if let Some(v) = ttft_ms {
            self.ttft.push(v);
        }
        if let Some(v) = upstream_ms {
            self.upstream.push(v);
        }
    }

    /// Fold another block's latency into this one (rollup + detail merge).
    fn merge(&mut self, other: LatencySamples) {
        self.e2e.merge(other.e2e);
        self.ttft.merge(other.ttft);
        self.upstream.merge(other.upstream);
    }
}

/// Nearest-rank percentile: rank `R = ceil(p/100 * N)` (1-based), value =
/// sorted[R-1]. Empty samples → `None`; `p = 100` → max.
fn nearest_rank_percentile(sorted: &[i64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let n = sorted.len();
    let rank = ((p / 100.0) * n as f64).ceil() as usize;
    Some(sorted[rank.saturating_sub(1).min(n - 1)] as f64)
}

/// `num / den`, or `None` when the denominator is 0.
fn rate(num: u64, den: u64) -> Option<f64> {
    (den > 0).then_some(num as f64 / den as f64)
}

/// UTC hour start of `ms` (floor division; negative-safe).
fn hour_start(ms: i64) -> i64 {
    ms.div_euclid(HOUR_MS) * HOUR_MS
}

/// All UTC hour starts intersecting `[from, to)`, in order.
fn bucket_hours(from_ms: i64, to_ms: i64) -> Vec<i64> {
    let mut hours = Vec::new();
    let mut h = hour_start(from_ms);
    let last = hour_start(to_ms - 1);
    while h <= last {
        hours.push(h);
        h += HOUR_MS;
    }
    hours
}

/// The 1h-rollup region of a mixed query (span > 24h): the full UTC hours
/// fully inside `[from, to - 24h)`. Partial boundary hours — and the
/// recent 24h — are answered from the fact details instead, so the split
/// never double-counts or drops a request (spec: 边界/缺失处必须由明细补齐).
/// Returns `(from_ms, from_ms)` when no full hour fits (the query then
/// degrades to a full-detail scan, still exact).
fn rollup_region(from_ms: i64, to_ms: i64) -> (i64, i64) {
    let detail_from = to_ms - MAX_EXACT_SPAN_MS;
    let mut first = hour_start(from_ms);
    if first < from_ms {
        first += HOUR_MS;
    }
    let last = hour_start(detail_from - HOUR_MS);
    if first > last {
        return (from_ms, from_ms);
    }
    (first, last + HOUR_MS)
}

/// Split sorted hour starts into contiguous runs (a run is a maximal
/// sequence of consecutive UTC hours); returns `[run_start, run_end)`.
fn contiguous_hour_runs(missing: &[i64]) -> Vec<(i64, i64)> {
    let mut runs = Vec::new();
    let mut iter = missing.iter().copied();
    let Some(mut start) = iter.next() else {
        return runs;
    };
    let mut prev = start;
    for h in iter {
        if h > prev + HOUR_MS {
            runs.push((start, prev + HOUR_MS));
            start = h;
        }
        prev = h;
    }
    runs.push((start, prev + HOUR_MS));
    runs
}

/// The hour split of a mixed query (span > 24h).
struct MixedSplit {
    /// UTC hours answered from the 1h rollup tables (empty when the
    /// historical region is narrower than a full UTC hour).
    rollup_hours: Vec<i64>,
    /// Fact-detail intervals covering everything else: the boundary
    /// partial hours, the recent 24h and every missing / dirty hour.
    detail_intervals: Vec<(i64, i64)>,
}

/// Stable ordering key of a group key (for deterministic output).
fn key_sort_key(key: &GroupKey) -> (&str, &str) {
    match key {
        GroupKey::Route { route_name } => (route_name.as_str(), ""),
        GroupKey::Target {
            provider,
            real_model,
        } => (provider.as_str(), real_model.as_str()),
    }
}

/// `route_id → route_name` display map collected from the agg rows.
fn route_names_by_id(rows: &[RouteAggRow]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for row in rows {
        if let (Some(rid), Some(rname)) = (&row.route_id, &row.route_name) {
            names.entry(rid.clone()).or_insert_with(|| rname.clone());
        }
    }
    names
}

// ═══════════════════════════════════════════════════════════════════
// Rollup row types (historical path of mixed queries)
// ═══════════════════════════════════════════════════════════════════

/// One `rollup_route_hourly` row JOINed with its stable display name.
#[derive(Debug, Clone, sqlx::FromRow)]
struct RollupRouteRow {
    hour: i64,
    route_id: String,
    route_name: Option<String>,
    success: i64,
    model_failure: i64,
    all_disabled: i64,
    client_cancelled: i64,
    prompt_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    cached_tokens: i64,
    cached_known_count: i64,
    cached_hit_count: i64,
    e2e_sum_ms: i64,
    ttft_sum_ms: i64,
    upstream_sum_ms: i64,
}

/// One `rollup_target_hourly` row JOINed with its provider / real model.
#[derive(Debug, Clone, sqlx::FromRow)]
struct RollupTargetRow {
    hour: i64,
    target_id: String,
    provider: String,
    actual_model: String,
    success: i64,
    failure: i64,
    cancelled: i64,
    prompt_tokens: i64,
    completion_tokens: i64,
    total_tokens: i64,
    cached_tokens: i64,
    cached_known_count: i64,
    cached_hit_count: i64,
    e2e_sum_ms: i64,
    ttft_sum_ms: i64,
    upstream_sum_ms: i64,
}

const ROLLUP_ROUTE_SQL_BASE: &str = r#"
SELECT rr.hour, rr.route_id, ar.route_name, rr.success, rr.model_failure,
       rr.all_disabled, rr.client_cancelled, rr.prompt_tokens, rr.completion_tokens,
       rr.total_tokens, rr.cached_tokens, rr.cached_known_count, rr.cached_hit_count,
       rr.e2e_sum_ms, rr.ttft_sum_ms, rr.upstream_sum_ms
FROM rollup_route_hourly rr
LEFT JOIN analytics_routes ar ON ar.route_id = rr.route_id
"#;

const ROLLUP_TARGET_SQL_BASE: &str = r#"
SELECT rt.hour, rt.target_id, art.provider, art.real_model AS actual_model, rt.success,
       rt.failure, rt.cancelled, rt.prompt_tokens, rt.completion_tokens, rt.total_tokens,
       rt.cached_tokens, rt.cached_known_count, rt.cached_hit_count,
       rt.e2e_sum_ms, rt.ttft_sum_ms, rt.upstream_sum_ms
FROM rollup_target_hourly rt
JOIN analytics_route_targets art ON art.target_id = rt.target_id
"#;

/// Extra `WHERE` predicates for the historical target rows. `actual_model`
/// lives on `analytics_route_targets.real_model` in the rollup path (the
/// target rows carry only `target_id`), unlike the detail path's
/// `ta.actual_model`.
fn target_rollup_filter(params: &QueryParams) -> (String, Vec<&str>) {
    let mut clause = String::new();
    let mut binds = Vec::new();
    if let Some(p) = params.provider.as_deref().filter(|p| !p.is_empty()) {
        clause.push_str(" AND art.provider = ?");
        binds.push(p);
    }
    if let Some(m) = params.real_model.as_deref().filter(|m| !m.is_empty()) {
        clause.push_str(" AND art.real_model = ?");
        binds.push(m);
    }
    if let Some(r) = params.route.as_deref().filter(|r| !r.is_empty()) {
        clause.push_str(
            " AND art.route_id = (SELECT route_id FROM analytics_routes WHERE route_name = ?)",
        );
        binds.push(r);
    }
    (clause, binds)
}

// ═══════════════════════════════════════════════════════════════════
// Assembly
// ═══════════════════════════════════════════════════════════════════

/// Latency sums of one rollup block — the average stays exact across the
/// rollup + detail merge (`*_sum_ms`), only percentiles degrade to the
/// histogram approximation.
#[derive(Debug, Clone, Copy, Default)]
struct RollupLatSums {
    e2e: i64,
    ttft: i64,
    upstream: i64,
}

/// Build a latency block from the dimension's histogram rows and the
/// rollup rows' exact sums.
fn hist_to_latency<'a>(
    rows: impl IntoIterator<Item = &'a HistogramRow>,
    sums: &RollupLatSums,
) -> LatencySamples {
    let merged = merge_histogram_buckets(rows);
    LatencySamples {
        e2e: LatencySource::Hist {
            bins: merged.get("e2e").cloned().unwrap_or_default(),
            sum_ms: sums.e2e,
        },
        ttft: LatencySource::Hist {
            bins: merged.get("ttft").cloned().unwrap_or_default(),
            sum_ms: sums.ttft,
        },
        upstream: LatencySource::Hist {
            bins: merged.get("upstream").cloned().unwrap_or_default(),
            sum_ms: sums.upstream,
        },
    }
}

/// Feed a `rollup_global_hourly` row into an accumulator (success carries
/// the token / cache sums; the remaining statuses are plain counts).
fn feed_global_counts(acc: &mut MetricAccumulator, g: &GlobalRollupRow) {
    let success = g.success.max(0) as u64;
    let known = g.cached_known_count.max(0) as u64;
    acc.add(
        "success",
        RowCounts {
            cnt: success,
            prompt_tokens: g.prompt_tokens.max(0) as u64,
            completion_tokens: g.completion_tokens.max(0) as u64,
            total_tokens: g.total_tokens.max(0) as u64,
            cache_tokens_sum: g.cached_tokens.max(0) as u64,
            cache_prompt_tokens: g.prompt_tokens.max(0) as u64,
            cache_known: known,
            cache_hit: g.cached_hit_count.max(0) as u64,
            cache_unknown: success.saturating_sub(known),
        },
    );
    for (status, cnt) in [
        ("client_error", g.client_error),
        ("no_route", g.no_route),
        ("all_disabled", g.all_disabled),
        ("model_failure", g.model_failure),
        ("client_cancelled", g.client_cancelled),
    ] {
        if cnt > 0 {
            acc.add(
                status,
                RowCounts {
                    cnt: cnt.max(0) as u64,
                    ..Default::default()
                },
            );
        }
    }
}

/// Feed a `rollup_route_hourly` row into an accumulator (matched routes
/// only; no `client_error` / `no_route` exist in these rows).
fn feed_route_counts(acc: &mut MetricAccumulator, r: &RollupRouteRow) {
    let success = r.success.max(0) as u64;
    let known = r.cached_known_count.max(0) as u64;
    acc.add(
        "success",
        RowCounts {
            cnt: success,
            prompt_tokens: r.prompt_tokens.max(0) as u64,
            completion_tokens: r.completion_tokens.max(0) as u64,
            total_tokens: r.total_tokens.max(0) as u64,
            cache_tokens_sum: r.cached_tokens.max(0) as u64,
            cache_prompt_tokens: r.prompt_tokens.max(0) as u64,
            cache_known: known,
            cache_hit: r.cached_hit_count.max(0) as u64,
            cache_unknown: success.saturating_sub(known),
        },
    );
    for (status, cnt) in [
        ("model_failure", r.model_failure),
        ("all_disabled", r.all_disabled),
        ("client_cancelled", r.client_cancelled),
    ] {
        if cnt > 0 {
            acc.add(
                status,
                RowCounts {
                    cnt: cnt.max(0) as u64,
                    ..Default::default()
                },
            );
        }
    }
}

/// Feed a `rollup_target_hourly` row into an accumulator.
fn feed_target_counts(acc: &mut MetricAccumulator, t: &RollupTargetRow) {
    let success = t.success.max(0) as u64;
    let known = t.cached_known_count.max(0) as u64;
    acc.add(
        "success",
        RowCounts {
            cnt: success,
            prompt_tokens: t.prompt_tokens.max(0) as u64,
            completion_tokens: t.completion_tokens.max(0) as u64,
            total_tokens: t.total_tokens.max(0) as u64,
            cache_tokens_sum: t.cached_tokens.max(0) as u64,
            cache_prompt_tokens: t.prompt_tokens.max(0) as u64,
            cache_known: known,
            cache_hit: t.cached_hit_count.max(0) as u64,
            cache_unknown: success.saturating_sub(known),
        },
    );
    if t.failure > 0 {
        acc.add(
            "failure",
            RowCounts {
                cnt: t.failure.max(0) as u64,
                ..Default::default()
            },
        );
    }
    if t.cancelled > 0 {
        acc.add(
            "cancelled",
            RowCounts {
                cnt: t.cancelled.max(0) as u64,
                ..Default::default()
            },
        );
    }
}

/// Route-scope aggregates shared by the exact and the mixed path.
#[derive(Debug, Default)]
struct RouteAggregates {
    overall: MetricAccumulator,
    groups: HashMap<GroupKey, MetricAccumulator>,
    unassigned: HashMap<String, u64>,
    lat_overall: LatencySamples,
    lat_by_group: HashMap<GroupKey, LatencySamples>,
}

impl RouteAggregates {
    /// Fold another aggregation into this one (rollup + detail merge).
    fn merge(&mut self, other: RouteAggregates) {
        self.overall.merge(&other.overall);
        for (key, acc) in other.groups {
            self.groups.entry(key).or_default().merge(&acc);
        }
        for (category, count) in other.unassigned {
            *self.unassigned.entry(category).or_default() += count;
        }
        self.lat_overall.merge(other.lat_overall);
        for (key, lat) in other.lat_by_group {
            self.lat_by_group.entry(key).or_default().merge(lat);
        }
    }

    /// Render the aggregates into a [`QuerySummary`].
    fn finish(mut self, params: &QueryParams) -> QuerySummary {
        let span_ms = params.to_ms - params.from_ms;
        let requests = self.overall.total_requests();
        let e2e_population =
            requests.saturating_sub(self.overall.client_error + self.overall.no_route);
        let overall_metrics = self.overall.finish(
            QueryScope::Route,
            span_ms,
            &self.lat_overall,
            e2e_population,
            true,
        );

        let mut groups_out: Vec<GroupSummary> = self
            .groups
            .into_iter()
            .map(|(key, acc)| {
                let population = acc.total_requests();
                let latency = self.lat_by_group.remove(&key).unwrap_or_default();
                GroupSummary {
                    key,
                    metrics: acc.finish(QueryScope::Route, span_ms, &latency, population, false),
                }
            })
            .collect();
        groups_out.sort_by(|a, b| {
            b.metrics
                .requests
                .cmp(&a.metrics.requests)
                .then_with(|| key_sort_key(&a.key).cmp(&key_sort_key(&b.key)))
        });

        let mut unassigned_out: Vec<UnassignedFailure> = self
            .unassigned
            .into_iter()
            .map(|(category, count)| UnassignedFailure { category, count })
            .collect();
        unassigned_out.sort_by(|a, b| {
            b.count
                .cmp(&a.count)
                .then_with(|| a.category.cmp(&b.category))
        });

        QuerySummary {
            scope: QueryScope::Route,
            from_ms: params.from_ms,
            to_ms: params.to_ms,
            overall: overall_metrics,
            groups: groups_out,
            unassigned_failures: unassigned_out,
        }
    }
}

/// Aggregate the route-scope detail rows of one interval (exact).
fn aggregate_route_rows(rows: &[RouteAggRow], lats: &[RouteLatRow]) -> RouteAggregates {
    let mut agg = RouteAggregates::default();

    for row in rows {
        let rc = RowCounts::from(row);
        agg.overall.add(&row.status, rc);
        match (&row.route_id, &row.route_name) {
            (Some(_), rname) => {
                let name = rname.clone().unwrap_or_else(|| UNKNOWN_ROUTE.to_string());
                agg.groups
                    .entry(GroupKey::Route { route_name: name })
                    .or_default()
                    .add(&row.status, rc);
            }
            // no_route / client_error: no route attribution.
            (None, _) => *agg.unassigned.entry(row.status.clone()).or_default() += rc.cnt,
        }
    }

    let names = route_names_by_id(rows);
    for lat in lats {
        agg.lat_overall
            .push(lat.duration_ms, lat.ttft_ms, lat.upstream_duration_ms);
        if let Some(rid) = &lat.route_id {
            let name = names
                .get(rid)
                .cloned()
                .unwrap_or_else(|| UNKNOWN_ROUTE.to_string());
            agg.lat_by_group
                .entry(GroupKey::Route { route_name: name })
                .or_default()
                .push(lat.duration_ms, lat.ttft_ms, lat.upstream_duration_ms);
        }
    }
    agg
}

/// Aggregate the historical (rollup) part of a mixed route query.
///
/// Unfiltered queries take `overall` (and the unassigned `no_route` /
/// `client_error` counts) from the global rows; filtered queries rebuild
/// `overall` from the filtered route rows so the filters apply uniformly
/// (the global rows are unfiltered by definition).
fn aggregate_rollup_route(
    rows: &[RollupRouteRow],
    globals: &[GlobalRollupRow],
    overall_hist: &[HistogramRow],
    group_hist: &[HistogramRow],
) -> RouteAggregates {
    let mut agg = RouteAggregates::default();
    let unfiltered = !globals.is_empty();
    let mut overall_sums = RollupLatSums::default();
    for g in globals {
        feed_global_counts(&mut agg.overall, g);
        if g.no_route > 0 {
            *agg.unassigned.entry("no_route".to_string()).or_default() += g.no_route as u64;
        }
        if g.client_error > 0 {
            *agg.unassigned
                .entry("client_error".to_string())
                .or_default() += g.client_error as u64;
        }
        overall_sums.e2e += g.e2e_sum_ms;
        overall_sums.ttft += g.ttft_sum_ms;
        overall_sums.upstream += g.upstream_sum_ms;
    }
    for row in rows {
        let key = GroupKey::Route {
            route_name: row
                .route_name
                .clone()
                .unwrap_or_else(|| UNKNOWN_ROUTE.to_string()),
        };
        if !unfiltered {
            feed_route_counts(&mut agg.overall, row);
            overall_sums.e2e += row.e2e_sum_ms;
            overall_sums.ttft += row.ttft_sum_ms;
            overall_sums.upstream += row.upstream_sum_ms;
        }
        feed_route_counts(agg.groups.entry(key.clone()).or_default(), row);
        let sums = RollupLatSums {
            e2e: row.e2e_sum_ms,
            ttft: row.ttft_sum_ms,
            upstream: row.upstream_sum_ms,
        };
        agg.lat_by_group
            .entry(key)
            .or_default()
            .merge(hist_to_latency(
                group_hist.iter().filter(|h| h.owner_id == row.route_id),
                &sums,
            ));
    }
    agg.lat_overall = hist_to_latency(overall_hist, &overall_sums);
    agg
}

/// Target-scope aggregates shared by the exact and the mixed path.
#[derive(Debug, Default)]
struct TargetAggregates {
    overall: MetricAccumulator,
    groups: HashMap<GroupKey, MetricAccumulator>,
    lat_overall: LatencySamples,
    lat_by_group: HashMap<GroupKey, LatencySamples>,
}

impl TargetAggregates {
    /// Fold another aggregation into this one (rollup + detail merge).
    fn merge(&mut self, other: TargetAggregates) {
        self.overall.merge(&other.overall);
        for (key, acc) in other.groups {
            self.groups.entry(key).or_default().merge(&acc);
        }
        self.lat_overall.merge(other.lat_overall);
        for (key, lat) in other.lat_by_group {
            self.lat_by_group.entry(key).or_default().merge(lat);
        }
    }

    /// Render the aggregates into a [`QuerySummary`].
    fn finish(mut self, params: &QueryParams) -> QuerySummary {
        let span_ms = params.to_ms - params.from_ms;
        let attempts = self.overall.total_attempts();
        let overall_metrics = self.overall.finish(
            QueryScope::Target,
            span_ms,
            &self.lat_overall,
            attempts,
            true,
        );

        let mut groups_out: Vec<GroupSummary> = self
            .groups
            .into_iter()
            .map(|(key, acc)| {
                let population = acc.total_attempts();
                let latency = self.lat_by_group.remove(&key).unwrap_or_default();
                GroupSummary {
                    key,
                    metrics: acc.finish(QueryScope::Target, span_ms, &latency, population, false),
                }
            })
            .collect();
        groups_out.sort_by(|a, b| {
            b.metrics
                .attempts
                .cmp(&a.metrics.attempts)
                .then_with(|| key_sort_key(&a.key).cmp(&key_sort_key(&b.key)))
        });

        QuerySummary {
            scope: QueryScope::Target,
            from_ms: params.from_ms,
            to_ms: params.to_ms,
            overall: overall_metrics,
            groups: groups_out,
            unassigned_failures: Vec::new(),
        }
    }
}

/// Aggregate the target-scope detail rows of one interval (exact).
fn aggregate_target_rows(rows: &[TargetAggRow], lats: &[TargetLatRow]) -> TargetAggregates {
    let mut agg = TargetAggregates::default();

    for row in rows {
        let rc = RowCounts::from(row);
        agg.overall.add(&row.status, rc);
        let key = GroupKey::Target {
            provider: row.provider.clone(),
            real_model: row.actual_model.clone(),
        };
        agg.groups.entry(key).or_default().add(&row.status, rc);
    }

    for lat in lats {
        agg.lat_overall
            .push(lat.duration_ms, lat.ttft_ms, lat.upstream_duration_ms);
        let key = GroupKey::Target {
            provider: lat.provider.clone(),
            real_model: lat.actual_model.clone(),
        };
        agg.lat_by_group.entry(key).or_default().push(
            lat.duration_ms,
            lat.ttft_ms,
            lat.upstream_duration_ms,
        );
    }
    agg
}

/// Aggregate the historical (rollup) part of a mixed target query. The
/// overall latency histogram is the given rows (all targets, or only the
/// filtered targets' rows — the caller restricted them).
fn aggregate_rollup_target(rows: &[RollupTargetRow], hist: &[HistogramRow]) -> TargetAggregates {
    let mut agg = TargetAggregates::default();
    let mut overall_sums = RollupLatSums::default();
    for row in rows {
        let key = GroupKey::Target {
            provider: row.provider.clone(),
            real_model: row.actual_model.clone(),
        };
        feed_target_counts(&mut agg.overall, row);
        feed_target_counts(agg.groups.entry(key.clone()).or_default(), row);
        overall_sums.e2e += row.e2e_sum_ms;
        overall_sums.ttft += row.ttft_sum_ms;
        overall_sums.upstream += row.upstream_sum_ms;
        let sums = RollupLatSums {
            e2e: row.e2e_sum_ms,
            ttft: row.ttft_sum_ms,
            upstream: row.upstream_sum_ms,
        };
        agg.lat_by_group
            .entry(key)
            .or_default()
            .merge(hist_to_latency(
                hist.iter().filter(|h| h.owner_id == row.target_id),
                &sums,
            ));
    }
    agg.lat_overall = hist_to_latency(hist, &overall_sums);
    agg
}

/// Per-hour trend aggregates shared by the exact and the mixed path.
#[derive(Debug, Default)]
struct TrendAggregates {
    per_hour_overall: HashMap<i64, MetricAccumulator>,
    per_hour_group: HashMap<(i64, GroupKey), MetricAccumulator>,
    per_hour_lat: HashMap<i64, LatencySamples>,
}

impl TrendAggregates {
    /// Fold another aggregation into this one (rollup + detail merge).
    fn merge(&mut self, other: TrendAggregates) {
        for (hour, acc) in other.per_hour_overall {
            self.per_hour_overall.entry(hour).or_default().merge(&acc);
        }
        for ((hour, key), acc) in other.per_hour_group {
            self.per_hour_group
                .entry((hour, key))
                .or_default()
                .merge(&acc);
        }
        for (hour, lat) in other.per_hour_lat {
            self.per_hour_lat.entry(hour).or_default().merge(lat);
        }
    }
}

/// Aggregate the detail rows of one interval into per-hour trend maps.
fn aggregate_trend<A, L, FAcc, FLat>(
    rows: &[A],
    lats: &[L],
    mut acc_row: FAcc,
    mut lat_row: FLat,
) -> TrendAggregates
where
    FAcc: FnMut(&A) -> Option<(i64, Option<GroupKey>, String, RowCounts)>,
    FLat: FnMut(&L) -> (i64, LatencySamples),
{
    let mut agg = TrendAggregates::default();
    for row in rows {
        if let Some((hour, key, status, rc)) = acc_row(row) {
            agg.per_hour_overall
                .entry(hour)
                .or_default()
                .add(&status, rc);
            if let Some(key) = key {
                agg.per_hour_group
                    .entry((hour, key))
                    .or_default()
                    .add(&status, rc);
            }
        }
    }
    for lat in lats {
        let (hour, samples) = lat_row(lat);
        agg.per_hour_lat.entry(hour).or_default().merge(samples);
    }
    agg
}

/// Render the per-hour trend maps into a [`TrendResult`]: per-hour overall
/// buckets with per-hour latency, per-group series, all zero-filled across
/// every UTC hour of the span.
fn finish_trend<FTotal>(
    mut agg: TrendAggregates,
    params: &QueryParams,
    total: FTotal,
) -> TrendResult
where
    FTotal: Fn(&MetricAccumulator) -> u64,
{
    let hours = bucket_hours(params.from_ms, params.to_ms);

    let mut buckets = Vec::with_capacity(hours.len());
    for &h in &hours {
        let acc = agg.per_hour_overall.remove(&h).unwrap_or_default();
        let population = total(&acc);
        let latency = agg.per_hour_lat.remove(&h).unwrap_or_default();
        buckets.push(TrendBucket {
            hour_ms: h,
            overall: acc.finish(params.scope, HOUR_MS, &latency, population, true),
        });
    }

    let mut series_map: HashMap<GroupKey, Vec<TrendPoint>> = HashMap::new();
    for ((h, key), acc) in agg.per_hour_group {
        series_map.entry(key).or_default().push(TrendPoint {
            hour_ms: h,
            requests: total(&acc),
            success: acc.success,
            prompt_tokens: acc.prompt_tokens,
            completion_tokens: acc.completion_tokens,
            total_tokens: acc.total_tokens,
        });
    }

    let mut series: Vec<GroupTrendSeries> = series_map
        .into_iter()
        .map(|(key, mut points)| {
            points.sort_by_key(|p| p.hour_ms);
            let by_hour: HashMap<i64, TrendPoint> =
                points.into_iter().map(|p| (p.hour_ms, p)).collect();
            let mut total_requests = 0u64;
            let mut filled = Vec::with_capacity(hours.len());
            for &h in &hours {
                let point = by_hour.get(&h).cloned().unwrap_or(TrendPoint {
                    hour_ms: h,
                    requests: 0,
                    success: 0,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    total_tokens: 0,
                });
                total_requests += point.requests;
                filled.push(point);
            }
            GroupTrendSeries {
                key,
                total_requests,
                points: filled,
            }
        })
        .collect();
    series.sort_by(|a, b| {
        b.total_requests
            .cmp(&a.total_requests)
            .then_with(|| key_sort_key(&a.key).cmp(&key_sort_key(&b.key)))
    });

    TrendResult {
        scope: params.scope,
        from_ms: params.from_ms,
        to_ms: params.to_ms,
        buckets,
        series,
    }
}

/// One `RouteAggRow` folded into per-hour trend maps (route scope).
fn route_trend_row(row: &RouteAggRow) -> Option<(i64, Option<GroupKey>, String, RowCounts)> {
    let rc = RowCounts::from(row);
    let key = match (&row.route_id, &row.route_name) {
        (Some(_), rname) => Some(GroupKey::Route {
            route_name: rname.clone().unwrap_or_else(|| UNKNOWN_ROUTE.to_string()),
        }),
        (None, _) => None,
    };
    Some((row.hour_ms, key, row.status.clone(), rc))
}

fn route_trend_lat(lat: &RouteLatRow) -> (i64, LatencySamples) {
    let mut samples = LatencySamples::default();
    samples.push(lat.duration_ms, lat.ttft_ms, lat.upstream_duration_ms);
    (lat.hour_ms, samples)
}

/// One `TargetAggRow` folded into per-hour trend maps (target scope).
fn target_trend_row(row: &TargetAggRow) -> Option<(i64, Option<GroupKey>, String, RowCounts)> {
    let rc = RowCounts::from(row);
    let key = Some(GroupKey::Target {
        provider: row.provider.clone(),
        real_model: row.actual_model.clone(),
    });
    Some((row.hour_ms, key, row.status.clone(), rc))
}

fn target_trend_lat(lat: &TargetLatRow) -> (i64, LatencySamples) {
    let mut samples = LatencySamples::default();
    samples.push(lat.duration_ms, lat.ttft_ms, lat.upstream_duration_ms);
    (lat.hour_ms, samples)
}

/// Historical (rollup) per-hour trend maps of a mixed route query.
fn aggregate_rollup_route_trend(
    rows: &[RollupRouteRow],
    globals: &[GlobalRollupRow],
    hist: &[HistogramRow],
) -> TrendAggregates {
    let mut agg = TrendAggregates::default();
    let unfiltered = !globals.is_empty();
    let mut per_hour_sums: HashMap<i64, RollupLatSums> = HashMap::new();
    for g in globals {
        feed_global_counts(agg.per_hour_overall.entry(g.hour).or_default(), g);
        let s = per_hour_sums.entry(g.hour).or_default();
        s.e2e += g.e2e_sum_ms;
        s.ttft += g.ttft_sum_ms;
        s.upstream += g.upstream_sum_ms;
    }
    for row in rows {
        let key = GroupKey::Route {
            route_name: row
                .route_name
                .clone()
                .unwrap_or_else(|| UNKNOWN_ROUTE.to_string()),
        };
        feed_route_counts(agg.per_hour_group.entry((row.hour, key)).or_default(), row);
        if !unfiltered {
            feed_route_counts(agg.per_hour_overall.entry(row.hour).or_default(), row);
            let s = per_hour_sums.entry(row.hour).or_default();
            s.e2e += row.e2e_sum_ms;
            s.ttft += row.ttft_sum_ms;
            s.upstream += row.upstream_sum_ms;
        }
    }
    let mut by_hour: HashMap<i64, Vec<&HistogramRow>> = HashMap::new();
    for hrow in hist {
        by_hour.entry(hrow.hour).or_default().push(hrow);
    }
    for (hour, rows) in by_hour {
        let sums = per_hour_sums.remove(&hour).unwrap_or_default();
        agg.per_hour_lat.insert(hour, hist_to_latency(rows, &sums));
    }
    agg
}

/// Historical (rollup) per-hour trend maps of a mixed target query.
fn aggregate_rollup_target_trend(
    rows: &[RollupTargetRow],
    hist: &[HistogramRow],
) -> TrendAggregates {
    let mut agg = TrendAggregates::default();
    let mut per_hour_sums: HashMap<i64, RollupLatSums> = HashMap::new();
    for row in rows {
        let key = GroupKey::Target {
            provider: row.provider.clone(),
            real_model: row.actual_model.clone(),
        };
        feed_target_counts(agg.per_hour_group.entry((row.hour, key)).or_default(), row);
        feed_target_counts(agg.per_hour_overall.entry(row.hour).or_default(), row);
        let s = per_hour_sums.entry(row.hour).or_default();
        s.e2e += row.e2e_sum_ms;
        s.ttft += row.ttft_sum_ms;
        s.upstream += row.upstream_sum_ms;
    }
    let mut by_hour: HashMap<i64, Vec<&HistogramRow>> = HashMap::new();
    for hrow in hist {
        by_hour.entry(hrow.hour).or_default().push(hrow);
    }
    for (hour, rows) in by_hour {
        let sums = per_hour_sums.remove(&hour).unwrap_or_default();
        agg.per_hour_lat.insert(hour, hist_to_latency(rows, &sums));
    }
    agg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AnalyticsEvent;
    use crate::event::test_support::test_pool;
    use crate::event_writer::EventWriter;
    use crate::rollup::{BucketState, RollupEngine};
    use chrono::{DateTime, Utc};
    use sag_core::{FinalStatus, RequestResultFact, TargetAttemptFact, TargetStatus, Usage};
    use uuid::Uuid;

    /// Reference instant (unix ms): mid-hour, so hour-alignment is exercised
    /// without relying on round numbers.
    const T0: i64 = 1_700_000_000_000;
    const HOUR: i64 = HOUR_MS;

    fn dt(ms: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(ms).unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn fact(
        id: Uuid,
        status: FinalStatus,
        route: Option<&str>,
        completed_ms: i64,
        duration_ms: i64,
        usage: Option<Usage>,
        cached_tokens: Option<u32>,
        ttft_ms: Option<u64>,
        upstream_ms: Option<u64>,
    ) -> RequestResultFact {
        RequestResultFact {
            request_id: id,
            route_name: route.map(str::to_string),
            requested_model: "test-model".to_string(),
            final_status: status,
            failure_reason: None,
            stream: false,
            started_at: dt(completed_ms - duration_ms),
            completed_at: dt(completed_ms),
            usage,
            cached_tokens,
            cache_creation_tokens: None,
            ttft_ms,
            upstream_duration_ms: upstream_ms,
            ingress_protocol: None,
            lane: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn attempt(
        provider: &str,
        model: &str,
        status: TargetStatus,
        completed_ms: i64,
        duration_ms: i64,
        usage: Option<Usage>,
        cached_tokens: Option<u32>,
        ttft_ms: Option<u64>,
        upstream_ms: Option<u64>,
    ) -> TargetAttemptFact {
        TargetAttemptFact {
            provider: provider.to_string(),
            real_model: model.to_string(),
            status,
            failure_reason: None,
            started_at: dt(completed_ms - duration_ms),
            completed_at: dt(completed_ms),
            usage,
            cached_tokens,
            ttft_ms,
            upstream_duration_ms: upstream_ms,
            upstream_protocol: None,
            lane: None,
        }
    }

    fn usage(prompt: u32, completion: u32, total: u32) -> Usage {
        Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: total,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        }
    }

    fn event(result: RequestResultFact, targets: Vec<TargetAttemptFact>) -> AnalyticsEvent {
        AnalyticsEvent { result, targets }
    }

    /// Simple success request: route-level fact + one successful attempt.
    fn success_request(route: &str, completed_ms: i64, duration_ms: i64) -> AnalyticsEvent {
        let id = Uuid::new_v4();
        event(
            fact(
                id,
                FinalStatus::Success,
                Some(route),
                completed_ms,
                duration_ms,
                Some(usage(10, 20, 30)),
                Some(3),
                None,
                None,
            ),
            vec![attempt(
                "openai",
                "gpt-4o",
                TargetStatus::Success,
                completed_ms,
                duration_ms - 10,
                Some(usage(10, 20, 30)),
                Some(3),
                None,
                None,
            )],
        )
    }

    async fn seed(pool: &SqlitePool, events: &[AnalyticsEvent]) {
        let writer = EventWriter::new(pool.clone());
        for e in events {
            writer.write(e.clone()).await.expect("seed event write");
        }
    }

    fn params(scope: QueryScope, from_ms: i64, to_ms: i64) -> QueryParams {
        QueryParams {
            scope,
            from_ms,
            to_ms,
            provider: None,
            real_model: None,
            route: None,
        }
    }

    /// Register + compute the rollup buckets of every hour with facts in
    /// `[from_ms, to_ms)` (mirrors the scheduler contract: startup scan
    /// registers missing hours, then `run_once` computes them).
    async fn rollup_hours(pool: &SqlitePool, from_ms: i64, to_ms: i64) {
        let engine = RollupEngine::new(pool.clone());
        engine
            .register_missing_hours(from_ms, to_ms)
            .await
            .expect("register missing hours");
        engine.run_once().await.expect("run rollup");
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    fn find_group<'a>(summary: &'a QuerySummary, key: &GroupKey) -> &'a GroupSummary {
        summary
            .groups
            .iter()
            .find(|g| &g.key == key)
            .unwrap_or_else(|| panic!("group {key:?} not found in {:?}", summary.groups))
    }

    #[tokio::test]
    async fn empty_span_returns_zeroed_summary_and_zero_filled_trend() {
        let pool = test_pool().await;
        let engine = AnalyticsQueryEngine::new(pool);

        let summary = engine
            .query_summary(&params(QueryScope::Route, T0, T0 + HOUR))
            .await
            .unwrap();
        assert_eq!(summary.overall.requests, 0);
        assert_eq!(summary.overall.attempts, 0);
        assert_eq!(summary.overall.success, 0);
        assert_eq!(summary.overall.failure, 0);
        assert_eq!(summary.overall.overall_failure_rate, None);
        assert_eq!(summary.overall.success_rate, None);
        assert_eq!(summary.overall.request_cache_hit_rate, None);
        assert_eq!(summary.overall.e2e.samples, 0);
        assert_eq!(summary.overall.e2e.avg_ms, None);
        assert_eq!(summary.overall.e2e.p50_ms, None);
        assert_eq!(summary.overall.e2e.p95_ms, None);
        assert_eq!(summary.overall.e2e.p99_ms, None);
        assert!(summary.groups.is_empty());
        assert!(summary.unassigned_failures.is_empty());

        let trend = engine
            .query_trend(&params(QueryScope::Route, T0, T0 + HOUR))
            .await
            .unwrap();
        // T0 sits mid-hour, so the span straddles two UTC hour slots; both
        // are empty and zero-filled.
        assert_eq!(trend.buckets.len(), 2);
        assert_eq!(trend.buckets[0].hour_ms, hour_start(T0));
        assert_eq!(trend.buckets[1].hour_ms, hour_start(T0) + HOUR);
        assert_eq!(trend.buckets[0].overall.requests, 0);
        assert_eq!(trend.buckets[1].overall.requests, 0);
        assert!(trend.series.is_empty());
    }

    #[tokio::test]
    async fn from_inclusive_to_exclusive_boundary() {
        let pool = test_pool().await;
        seed(
            &pool,
            &[
                success_request("chat", T0, 100),            // == from: included
                success_request("chat", T0 + HOUR / 2, 100), // inside
                success_request("chat", T0 + HOUR, 100),     // == to: excluded
            ],
        )
        .await;
        let engine = AnalyticsQueryEngine::new(pool);

        let summary = engine
            .query_summary(&params(QueryScope::Route, T0, T0 + HOUR))
            .await
            .unwrap();
        assert_eq!(summary.overall.requests, 2);

        let summary = engine
            .query_summary(&params(QueryScope::Route, T0, T0 + HOUR / 2))
            .await
            .unwrap();
        assert_eq!(summary.overall.requests, 1);
    }

    #[tokio::test]
    async fn route_a_fail_b_success_single_final_result() {
        let pool = test_pool().await;
        let id = Uuid::new_v4();
        seed(
            &pool,
            &[event(
                fact(
                    id,
                    FinalStatus::Success,
                    Some("chat"),
                    T0,
                    500,
                    Some(usage(10, 20, 30)),
                    Some(3),
                    Some(120),
                    Some(300),
                ),
                vec![
                    attempt(
                        "openai",
                        "gpt-4o",
                        TargetStatus::Failure,
                        T0,
                        200,
                        None,
                        None,
                        None,
                        None,
                    ),
                    attempt(
                        "anthropic",
                        "claude-3",
                        TargetStatus::Success,
                        T0,
                        300,
                        Some(usage(10, 20, 30)),
                        Some(3),
                        Some(120),
                        Some(300),
                    ),
                ],
            )],
        )
        .await;
        let engine = AnalyticsQueryEngine::new(pool);

        // Route scope: the request produced exactly one final result (success),
        // regardless of the failed A attempt.
        let route = engine
            .query_summary(&params(QueryScope::Route, T0, T0 + HOUR))
            .await
            .unwrap();
        assert_eq!(route.overall.requests, 1);
        assert_eq!(route.overall.success, 1);
        assert_eq!(route.overall.failure, 0);
        assert_close(route.overall.overall_failure_rate.unwrap(), 0.0);
        assert_close(route.overall.success_rate.unwrap(), 1.0);
        assert_eq!(route.overall.total_tokens, 30);
        assert_eq!(route.overall.e2e.samples, 1);
        assert_eq!(route.overall.e2e.p50_ms, Some(500.0));
        assert_eq!(route.groups.len(), 1);
        let g = find_group(
            &route,
            &GroupKey::Route {
                route_name: "chat".into(),
            },
        );
        assert_eq!(g.metrics.requests, 1);
        assert_eq!(g.metrics.success, 1);
        assert_close(g.metrics.success_rate.unwrap(), 1.0);

        // Target scope: both actual attempts appear, grouped separately.
        let target = engine
            .query_summary(&params(QueryScope::Target, T0, T0 + HOUR))
            .await
            .unwrap();
        assert_eq!(target.overall.attempts, 2);
        assert_eq!(target.overall.success, 1);
        assert_eq!(target.overall.attempt_failure, 1);
        assert_eq!(target.overall.attempt_cancelled, 0);
        assert_close(target.overall.success_rate.unwrap(), 0.5);
        assert_close(target.overall.overall_failure_rate.unwrap(), 0.5);
        assert_eq!(target.overall.total_tokens, 30); // only the success attempt
        assert_eq!(target.groups.len(), 2);
        let a = find_group(
            &target,
            &GroupKey::Target {
                provider: "openai".into(),
                real_model: "gpt-4o".into(),
            },
        );
        assert_eq!(a.metrics.attempts, 1);
        assert_eq!(a.metrics.attempt_failure, 1);
        assert_eq!(a.metrics.success, 0);
        let b = find_group(
            &target,
            &GroupKey::Target {
                provider: "anthropic".into(),
                real_model: "claude-3".into(),
            },
        );
        assert_eq!(b.metrics.attempts, 1);
        assert_eq!(b.metrics.success, 1);

        // Serialized output must never leak UUID identities.
        let json = serde_json::to_value(&route).unwrap();
        let s = json.to_string();
        assert!(
            !s.contains("route_id") && !s.contains("target_id"),
            "UUID leaked: {s}"
        );
        let groups_json = json["groups"].as_array().unwrap();
        assert_eq!(groups_json[0]["key"]["route_name"], "chat");
    }

    #[tokio::test]
    async fn unassigned_failures_no_route_all_disabled_client_error() {
        let pool = test_pool().await;
        seed(
            &pool,
            &[
                // all_disabled: matched route, no attempts.
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::AllDisabled,
                        Some("chat"),
                        T0,
                        5,
                        None,
                        None,
                        None,
                        None,
                    ),
                    vec![],
                ),
                // no_route: no route, no attempts.
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::NoRoute,
                        None,
                        T0 + 1000,
                        1,
                        None,
                        None,
                        None,
                        None,
                    ),
                    vec![],
                ),
                // client_error: no route, no attempts.
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::ClientError,
                        None,
                        T0 + 2000,
                        1,
                        None,
                        None,
                        None,
                        None,
                    ),
                    vec![],
                ),
            ],
        )
        .await;
        let engine = AnalyticsQueryEngine::new(pool);

        let summary = engine
            .query_summary(&params(QueryScope::Route, T0, T0 + HOUR))
            .await
            .unwrap();
        assert_eq!(summary.overall.requests, 3);
        assert_eq!(summary.overall.success, 0);
        assert_eq!(summary.overall.client_error, 1);
        assert_eq!(summary.overall.no_route, 1);
        assert_eq!(summary.overall.all_disabled, 1);
        assert_close(summary.overall.overall_failure_rate.unwrap(), 1.0);
        assert_close(summary.overall.client_error_rate.unwrap(), 1.0 / 3.0);
        assert_close(summary.overall.route_config_failure_rate.unwrap(), 1.0);
        assert_eq!(summary.overall.model_failure_rate, None);
        // Overall success_rate counts every request under analysis (0 / 3,
        // no_route / client_error included) and complements the failure
        // rate; the route view (0 / (3 - 2)) is not used for the overall.
        assert_close(summary.overall.success_rate.unwrap(), 0.0);
        assert_close(
            summary.overall.success_rate.unwrap() + summary.overall.overall_failure_rate.unwrap(),
            1.0,
        );

        // Only the matched route forms a group; client_error / no_route are
        // unassigned.
        assert_eq!(summary.groups.len(), 1);
        let g = find_group(
            &summary,
            &GroupKey::Route {
                route_name: "chat".into(),
            },
        );
        assert_eq!(g.metrics.requests, 1);
        assert_eq!(g.metrics.all_disabled, 1);
        // Group blocks keep the route view: 0 / 1 matched non-client-error
        // requests (the group holds no client_error / no_route rows).
        assert_close(g.metrics.success_rate.unwrap(), 0.0);
        assert_eq!(
            summary.unassigned_failures,
            vec![
                UnassignedFailure {
                    category: "client_error".to_string(),
                    count: 1
                },
                UnassignedFailure {
                    category: "no_route".to_string(),
                    count: 1
                },
            ]
        );

        // e2e latency population: only all_disabled entered scheduling.
        assert_eq!(summary.overall.e2e.samples, 1);
        assert_eq!(summary.overall.e2e.population, 1);
        assert_close(summary.overall.e2e.coverage.unwrap(), 1.0);
        assert!(summary.overall.e2e.low_sample);

        // Target scope over the same span: no attempts at all.
        let target = engine
            .query_summary(&params(QueryScope::Target, T0, T0 + HOUR))
            .await
            .unwrap();
        assert_eq!(target.overall.attempts, 0);
        assert!(target.groups.is_empty());
        assert!(target.unassigned_failures.is_empty());
    }

    #[tokio::test]
    async fn overall_success_rate_complements_failure_rate() {
        let pool = test_pool().await;
        seed(
            &pool,
            &[
                // success: matched route.
                success_request("chat", T0, 100),
                // no_route / client_error: unassigned, but still part of
                // the overall population.
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::NoRoute,
                        None,
                        T0 + 1000,
                        1,
                        None,
                        None,
                        None,
                        None,
                    ),
                    vec![],
                ),
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::ClientError,
                        None,
                        T0 + 2000,
                        1,
                        None,
                        None,
                        None,
                        None,
                    ),
                    vec![],
                ),
                // all_disabled / model_failure / client_cancelled: matched.
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::AllDisabled,
                        Some("chat"),
                        T0 + 3000,
                        5,
                        None,
                        None,
                        None,
                        None,
                    ),
                    vec![],
                ),
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::ModelFailure,
                        Some("chat"),
                        T0 + 4000,
                        50,
                        None,
                        None,
                        None,
                        None,
                    ),
                    vec![attempt(
                        "openai",
                        "gpt-4o",
                        TargetStatus::Failure,
                        T0 + 4000,
                        50,
                        None,
                        None,
                        None,
                        None,
                    )],
                ),
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::ClientCancelled,
                        Some("chat"),
                        T0 + 5000,
                        20,
                        None,
                        None,
                        None,
                        None,
                    ),
                    vec![],
                ),
            ],
        )
        .await;
        let engine = AnalyticsQueryEngine::new(pool);

        let summary = engine
            .query_summary(&params(QueryScope::Route, T0, T0 + HOUR))
            .await
            .unwrap();
        assert_eq!(summary.overall.requests, 6);
        assert_eq!(summary.overall.success, 1);
        assert_eq!(summary.overall.failure, 5);
        // Overall: 1 / 6 — every request under analysis is in the
        // denominator, so success_rate + overall_failure_rate = 1. The
        // route view (1 / (6 - 2)) is NOT used for the overall block.
        assert_close(summary.overall.success_rate.unwrap(), 1.0 / 6.0);
        assert_close(summary.overall.overall_failure_rate.unwrap(), 5.0 / 6.0);
        assert_close(
            summary.overall.success_rate.unwrap() + summary.overall.overall_failure_rate.unwrap(),
            1.0,
        );

        // Route groups keep the route view: the chat group matched 4
        // non-client-error requests (success + all_disabled + model_failure
        // + client_cancelled), so its success_rate is 1 / 4, not 1 / 6.
        let g = find_group(
            &summary,
            &GroupKey::Route {
                route_name: "chat".into(),
            },
        );
        assert_eq!(g.metrics.requests, 4);
        assert_close(g.metrics.success_rate.unwrap(), 1.0 / 4.0);
        assert_close(
            g.metrics.success_rate.unwrap() + g.metrics.overall_failure_rate.unwrap(),
            1.0,
        );
        assert_eq!(
            summary.unassigned_failures,
            vec![
                UnassignedFailure {
                    category: "client_error".to_string(),
                    count: 1
                },
                UnassignedFailure {
                    category: "no_route".to_string(),
                    count: 1
                },
            ]
        );
    }

    #[tokio::test]
    async fn failure_rate_denominators_success_model_failure_client_cancelled() {
        let pool = test_pool().await;
        seed(
            &pool,
            &[
                // success: usage counted.
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::Success,
                        Some("chat"),
                        T0,
                        100,
                        Some(usage(10, 20, 30)),
                        Some(0),
                        None,
                        None,
                    ),
                    vec![attempt(
                        "openai",
                        "gpt-4o",
                        TargetStatus::Success,
                        T0,
                        90,
                        Some(usage(10, 20, 30)),
                        Some(0),
                        None,
                        None,
                    )],
                ),
                // model_failure: carries a failed attempt; usage (if any) must
                // not be counted as 0 tokens.
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::ModelFailure,
                        Some("chat"),
                        T0 + 1000,
                        200,
                        Some(usage(100, 200, 300)),
                        None,
                        None,
                        None,
                    ),
                    vec![attempt(
                        "openai",
                        "gpt-4o",
                        TargetStatus::Failure,
                        T0 + 1000,
                        200,
                        None,
                        None,
                        None,
                        None,
                    )],
                ),
                // client_cancelled: in-flight attempt recorded as cancelled.
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::ClientCancelled,
                        Some("chat"),
                        T0 + 2000,
                        150,
                        None,
                        None,
                        None,
                        None,
                    ),
                    vec![attempt(
                        "openai",
                        "gpt-4o",
                        TargetStatus::Cancelled,
                        T0 + 2000,
                        150,
                        None,
                        None,
                        None,
                        None,
                    )],
                ),
            ],
        )
        .await;
        let engine = AnalyticsQueryEngine::new(pool);

        let route = engine
            .query_summary(&params(QueryScope::Route, T0, T0 + HOUR))
            .await
            .unwrap();
        assert_eq!(route.overall.requests, 3);
        assert_eq!(route.overall.success, 1);
        assert_eq!(route.overall.model_failure, 1);
        assert_eq!(route.overall.client_cancelled, 1);
        assert_eq!(route.overall.failure, 2);
        assert_close(route.overall.overall_failure_rate.unwrap(), 2.0 / 3.0);
        // model_failure_rate = 1 / (1 success + 1 model_failure) = 0.5
        assert_close(route.overall.model_failure_rate.unwrap(), 0.5);
        // success_rate = 1 / (1+1+1) — client_cancelled stays in the group
        assert_close(route.overall.success_rate.unwrap(), 1.0 / 3.0);
        assert_close(route.overall.client_error_rate.unwrap(), 0.0);
        assert_close(route.overall.route_config_failure_rate.unwrap(), 0.0);
        // tokens: only the success request; the failed request's usage is NULL.
        assert_eq!(route.overall.prompt_tokens, 10);
        assert_eq!(route.overall.completion_tokens, 20);
        assert_eq!(route.overall.total_tokens, 30);

        let target = engine
            .query_summary(&params(QueryScope::Target, T0, T0 + HOUR))
            .await
            .unwrap();
        assert_eq!(target.overall.attempts, 3);
        assert_eq!(target.overall.success, 1);
        assert_eq!(target.overall.attempt_failure, 1);
        assert_eq!(target.overall.attempt_cancelled, 1);
        assert_eq!(target.overall.failure, 2);
        assert_close(target.overall.success_rate.unwrap(), 1.0 / 3.0);
        assert_close(target.overall.overall_failure_rate.unwrap(), 2.0 / 3.0);
        assert_eq!(target.overall.total_tokens, 30);
    }

    #[tokio::test]
    async fn tokens_only_final_success_null_never_zero() {
        let pool = test_pool().await;
        // A success request whose usage was never reported stays NULL (0
        // counted), and a failed request with usage must not leak tokens.
        seed(
            &pool,
            &[
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::Success,
                        Some("chat"),
                        T0,
                        100,
                        None, // no usage: token columns stay NULL
                        None,
                        None,
                        None,
                    ),
                    vec![attempt(
                        "openai",
                        "gpt-4o",
                        TargetStatus::Success,
                        T0,
                        90,
                        None,
                        None,
                        None,
                        None,
                    )],
                ),
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::Success,
                        Some("chat"),
                        T0 + 1000,
                        100,
                        Some(usage(10, 20, 30)),
                        None,
                        None,
                        None,
                    ),
                    vec![attempt(
                        "openai",
                        "gpt-4o",
                        TargetStatus::Success,
                        T0 + 1000,
                        90,
                        Some(usage(10, 20, 30)),
                        None,
                        None,
                        None,
                    )],
                ),
            ],
        )
        .await;
        let engine = AnalyticsQueryEngine::new(pool);

        let summary = engine
            .query_summary(&params(QueryScope::Route, T0, T0 + HOUR))
            .await
            .unwrap();
        assert_eq!(summary.overall.requests, 2);
        assert_eq!(summary.overall.success, 2);
        assert_eq!(summary.overall.prompt_tokens, 10);
        assert_eq!(summary.overall.completion_tokens, 20);
        assert_eq!(summary.overall.total_tokens, 30);
        // cache_unknown covers the success with NULL cached_tokens.
        assert_eq!(summary.overall.cache_known, 0);
        assert_eq!(summary.overall.cache_unknown, 2);
        assert_eq!(summary.overall.request_cache_hit_rate, None);
        // Token hit rate: Σ COALESCE(cached_tokens,0) / Σ prompt_tokens over
        // ALL success — both cached_tokens are NULL (numerator 0) but the one
        // success with usage keeps its 10 prompt_tokens in the denominator.
        assert_close(summary.overall.token_cache_hit_rate.unwrap(), 0.0);
        assert_eq!(summary.overall.cache_prompt_tokens, 10);
        assert_eq!(summary.overall.cache_tokens, 0);
    }

    #[tokio::test]
    async fn cache_metrics_unknown_is_not_miss() {
        let pool = test_pool().await;
        // Three successes, all with prompt = 10: one unknown (NULL), one
        // miss (0), one hit (5).
        let mk = |cached: Option<u32>| {
            event(
                fact(
                    Uuid::new_v4(),
                    FinalStatus::Success,
                    Some("chat"),
                    T0,
                    100,
                    Some(usage(10, 5, 15)),
                    cached,
                    None,
                    None,
                ),
                vec![attempt(
                    "openai",
                    "gpt-4o",
                    TargetStatus::Success,
                    T0,
                    90,
                    Some(usage(10, 5, 15)),
                    cached,
                    None,
                    None,
                )],
            )
        };
        seed(&pool, &[mk(None), mk(Some(0)), mk(Some(5))]).await;
        let engine = AnalyticsQueryEngine::new(pool);

        let summary = engine
            .query_summary(&params(QueryScope::Route, T0, T0 + HOUR))
            .await
            .unwrap();
        assert_eq!(summary.overall.cache_known, 2);
        assert_eq!(summary.overall.cache_hit, 1);
        assert_eq!(summary.overall.cache_miss, 1);
        assert_eq!(summary.overall.cache_unknown, 1);
        assert_close(summary.overall.request_cache_hit_rate.unwrap(), 0.5);
        // Token hit rate = Σ COALESCE(cached_tokens, 0) / Σ prompt_tokens
        // over ALL success samples: 5 / (10 + 10 + 10). The unknown success
        // keeps its prompt_tokens in the denominator (only its cached
        // contribution is 0).
        assert_close(summary.overall.token_cache_hit_rate.unwrap(), 5.0 / 30.0);
        assert_eq!(summary.overall.cache_prompt_tokens, 30);
        assert_eq!(summary.overall.cache_tokens, 5);
    }

    #[tokio::test]
    async fn token_cache_hit_rate_all_success_and_detail_rollup_consistent() {
        let pool = test_pool().await;
        let h0 = hour_start(T0);
        // Four route-level facts in h0: three successes (hit > 0, known
        // miss = 0, unknown = NULL) plus one model_failure that carries
        // (would-be) usage + cached_tokens — it must be excluded from the
        // token statistics entirely.
        let mk_success = |cached: Option<u32>, prompt: u32, off_ms: i64| {
            event(
                fact(
                    Uuid::new_v4(),
                    FinalStatus::Success,
                    Some("chat"),
                    h0 + off_ms,
                    100,
                    Some(usage(prompt, 5, prompt + 5)),
                    cached,
                    None,
                    None,
                ),
                vec![attempt(
                    "openai",
                    "gpt-4o",
                    TargetStatus::Success,
                    h0 + off_ms,
                    90,
                    Some(usage(prompt, 5, prompt + 5)),
                    cached,
                    None,
                    None,
                )],
            )
        };
        seed(
            &pool,
            &[
                mk_success(Some(30), 100, 1), // hit (> 0)
                mk_success(Some(0), 50, 2),   // known miss (0)
                mk_success(None, 25, 3),      // unknown (NULL)
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::ModelFailure,
                        Some("chat"),
                        h0 + 4,
                        200,
                        Some(usage(999, 999, 1998)),
                        Some(999),
                        None,
                        None,
                    ),
                    vec![attempt(
                        "openai",
                        "gpt-4o",
                        TargetStatus::Failure,
                        h0 + 4,
                        200,
                        Some(usage(999, 999, 1998)),
                        Some(999),
                        None,
                        None,
                    )],
                ),
            ],
        )
        .await;
        let engine = AnalyticsQueryEngine::new(pool.clone());

        // Exact path (span < 24h): token rate = (30 + 0) / (100 + 50 + 25);
        // the failed request's 999 tokens / 999 cached_tokens never enter.
        let exact = engine
            .query_summary(&params(QueryScope::Route, h0, h0 + HOUR))
            .await
            .unwrap();
        assert_eq!(exact.overall.success, 3);
        assert_eq!(exact.overall.model_failure, 1);
        assert_eq!(exact.overall.prompt_tokens, 175);
        assert_eq!(exact.overall.cache_known, 2);
        assert_eq!(exact.overall.cache_hit, 1);
        assert_eq!(exact.overall.cache_unknown, 1);
        assert_close(exact.overall.request_cache_hit_rate.unwrap(), 0.5);
        assert_close(exact.overall.token_cache_hit_rate.unwrap(), 30.0 / 175.0);
        assert_eq!(exact.overall.cache_prompt_tokens, 175);
        assert_eq!(exact.overall.cache_tokens, 30);

        // Mixed path (span > 24h) with h0 rolled up must agree with the
        // exact path: rollup `prompt_tokens` is the token-hit-rate
        // denominator and `cached_tokens` the numerator.
        rollup_hours(&pool, h0, h0 + HOUR + 1).await;
        let mixed = engine
            .query_summary(&params(QueryScope::Route, h0, h0 + 25 * HOUR))
            .await
            .unwrap();
        assert_eq!(mixed.overall.prompt_tokens, 175);
        assert_close(mixed.overall.token_cache_hit_rate.unwrap(), 30.0 / 175.0);
        assert_eq!(mixed.overall.cache_prompt_tokens, 175);
        assert_eq!(mixed.overall.cache_tokens, 30);

        // Target scope: the same three success attempts plus the failed one
        // (excluded). Both exact and mixed agree.
        let exact_t = engine
            .query_summary(&params(QueryScope::Target, h0, h0 + HOUR))
            .await
            .unwrap();
        assert_eq!(exact_t.overall.attempts, 4);
        assert_eq!(exact_t.overall.success, 3);
        assert_close(exact_t.overall.token_cache_hit_rate.unwrap(), 30.0 / 175.0);
        let mixed_t = engine
            .query_summary(&params(QueryScope::Target, h0, h0 + 25 * HOUR))
            .await
            .unwrap();
        assert_eq!(mixed_t.overall.attempts, 4);
        assert_close(mixed_t.overall.token_cache_hit_rate.unwrap(), 30.0 / 175.0);
    }

    #[tokio::test]
    async fn latency_percentiles_nearest_rank_and_independent_coverage() {
        let pool = test_pool().await;
        // 10 success requests, durations 1..=10; ttft only on even-indexed
        // requests (2,4,6,8,10); upstream only on odd-indexed (20..100).
        let mut events = Vec::new();
        for i in 0..10 {
            let dur = i + 1;
            let ttft = if i % 2 == 0 {
                Some((i + 2) as u64) // 2,4,6,8,10
            } else {
                None
            };
            let upstream = if i % 2 == 1 {
                Some(10 * dur as u64)
            } else {
                None
            };
            events.push(event(
                fact(
                    Uuid::new_v4(),
                    FinalStatus::Success,
                    Some("chat"),
                    T0 + i * 1000,
                    dur,
                    Some(usage(1, 1, 2)),
                    None,
                    ttft,
                    upstream,
                ),
                vec![attempt(
                    "openai",
                    "gpt-4o",
                    TargetStatus::Success,
                    T0 + i * 1000,
                    dur - 1,
                    Some(usage(1, 1, 2)),
                    None,
                    ttft,
                    upstream,
                )],
            ));
        }
        seed(&pool, &events).await;
        let engine = AnalyticsQueryEngine::new(pool);

        let summary = engine
            .query_summary(&params(QueryScope::Route, T0, T0 + HOUR))
            .await
            .unwrap();
        let e2e = &summary.overall.e2e;
        assert_eq!(e2e.samples, 10);
        assert_eq!(e2e.population, 10);
        assert_close(e2e.coverage.unwrap(), 1.0);
        assert_close(e2e.avg_ms.unwrap(), 5.5);
        // nearest-rank: rank = ceil(p/100 * N)
        assert_eq!(e2e.p50_ms, Some(5.0)); // ceil(0.5*10)=5 -> 5th smallest
        assert_eq!(e2e.p95_ms, Some(10.0)); // ceil(0.95*10)=10 -> max
        assert_eq!(e2e.p99_ms, Some(10.0));
        assert!(e2e.low_sample);

        let ttft = &summary.overall.ttft;
        assert_eq!(ttft.samples, 5);
        assert_eq!(ttft.population, 10);
        assert_close(ttft.coverage.unwrap(), 0.5);
        assert_close(ttft.avg_ms.unwrap(), 6.0);
        assert_eq!(ttft.p50_ms, Some(6.0)); // ceil(0.5*5)=3 -> 3rd of [2,4,6,8,10]
        assert_eq!(ttft.p95_ms, Some(10.0));

        let upstream = &summary.overall.upstream;
        assert_eq!(upstream.samples, 5);
        assert_close(upstream.coverage.unwrap(), 0.5);
        assert_close(upstream.avg_ms.unwrap(), 60.0);
        assert_eq!(upstream.p50_ms, Some(60.0));

        // Target scope: same values over the attempts.
        let target = engine
            .query_summary(&params(QueryScope::Target, T0, T0 + HOUR))
            .await
            .unwrap();
        assert_eq!(target.overall.e2e.samples, 10);
        assert_eq!(target.overall.ttft.samples, 5);
        assert_eq!(target.overall.upstream.samples, 5);
    }

    #[tokio::test]
    async fn latency_low_sample_flag_at_threshold() {
        let pool = test_pool().await;
        // 100 valid samples: low_sample must be false (spec: < 100).
        let mut events = Vec::new();
        for i in 0..100 {
            let dur = i + 1;
            events.push(event(
                fact(
                    Uuid::new_v4(),
                    FinalStatus::Success,
                    Some("chat"),
                    T0 + i * 10,
                    dur,
                    Some(usage(1, 1, 2)),
                    None,
                    None,
                    None,
                ),
                vec![attempt(
                    "openai",
                    "gpt-4o",
                    TargetStatus::Success,
                    T0 + i * 10,
                    dur - 1,
                    Some(usage(1, 1, 2)),
                    None,
                    None,
                    None,
                )],
            ));
        }
        seed(&pool, &events).await;
        let engine = AnalyticsQueryEngine::new(pool);

        let summary = engine
            .query_summary(&params(QueryScope::Route, T0, T0 + HOUR))
            .await
            .unwrap();
        let e2e = &summary.overall.e2e;
        assert_eq!(e2e.samples, 100);
        assert!(!e2e.low_sample);
        assert_close(e2e.avg_ms.unwrap(), 50.5);
        assert_eq!(e2e.p50_ms, Some(50.0));
        assert_eq!(e2e.p95_ms, Some(95.0));
        assert_eq!(e2e.p99_ms, Some(99.0));
    }

    #[tokio::test]
    async fn target_scope_provider_and_real_model_filters() {
        let pool = test_pool().await;
        seed(
            &pool,
            &[event(
                fact(
                    Uuid::new_v4(),
                    FinalStatus::Success,
                    Some("chat"),
                    T0,
                    500,
                    Some(usage(10, 20, 30)),
                    None,
                    None,
                    None,
                ),
                vec![
                    attempt(
                        "openai",
                        "gpt-4o",
                        TargetStatus::Success,
                        T0,
                        100,
                        Some(usage(10, 20, 30)),
                        None,
                        None,
                        None,
                    ),
                    attempt(
                        "openai",
                        "gpt-4o-mini",
                        TargetStatus::Failure,
                        T0 + 1000,
                        200,
                        None,
                        None,
                        None,
                        None,
                    ),
                    attempt(
                        "anthropic",
                        "claude-3",
                        TargetStatus::Success,
                        T0 + 2000,
                        150,
                        Some(usage(5, 5, 10)),
                        None,
                        None,
                        None,
                    ),
                ],
            )],
        )
        .await;
        let engine = AnalyticsQueryEngine::new(pool);

        let mut p = params(QueryScope::Target, T0, T0 + HOUR);
        p.provider = Some("openai".to_string());
        let summary = engine.query_summary(&p).await.unwrap();
        assert_eq!(summary.overall.attempts, 2);
        assert_eq!(summary.overall.success, 1);
        assert_eq!(summary.groups.len(), 2);
        assert!(
            summary.groups.iter().all(
                |g| matches!(&g.key, GroupKey::Target { provider, .. } if provider == "openai")
            )
        );
        assert_eq!(summary.overall.total_tokens, 30); // anthropic success excluded

        p.provider = None;
        p.real_model = Some("gpt-4o".to_string());
        let summary = engine.query_summary(&p).await.unwrap();
        assert_eq!(summary.overall.attempts, 1);
        assert_eq!(summary.overall.success, 1);
        assert_eq!(summary.groups.len(), 1);

        p.real_model = Some("gpt-4o-mini".to_string());
        let summary = engine.query_summary(&p).await.unwrap();
        assert_eq!(summary.overall.attempts, 1);
        assert_eq!(summary.overall.success, 0);

        // Filters are rejected in route scope.
        let err = engine
            .query_summary(&QueryParams {
                scope: QueryScope::Route,
                from_ms: T0,
                to_ms: T0 + HOUR,
                provider: Some("openai".to_string()),
                real_model: None,
                route: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, QueryError::InvalidQuery(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn span_over_24h_mixed_path_empty_db_returns_zeroed_result() {
        let pool = test_pool().await;
        let engine = AnalyticsQueryEngine::new(pool);
        let h0 = hour_start(T0);

        // Spans > 24h no longer fail with SpanTooLong: the mixed path
        // answers them (here: nothing to read → zeroed, exact-style).
        let over = params(QueryScope::Route, h0, h0 + 25 * HOUR);
        let summary = engine.query_summary(&over).await.unwrap();
        assert_eq!(summary.overall.requests, 0);
        assert_eq!(summary.overall.e2e.samples, 0);
        assert_eq!(summary.overall.e2e.coverage, None);
        assert!(!summary.overall.e2e.approx_percentiles);
        assert!(summary.groups.is_empty());
        assert!(summary.unassigned_failures.is_empty());

        let trend = engine.query_trend(&over).await.unwrap();
        assert_eq!(trend.buckets.len(), 25);
        assert!(trend.buckets.iter().all(|b| b.overall.requests == 0));
        assert!(trend.series.is_empty());

        // Exactly 24h is still exact-detail territory.
        let ok = engine
            .query_summary(&params(QueryScope::Route, h0, h0 + 24 * HOUR))
            .await
            .unwrap();
        assert_eq!(ok.overall.requests, 0);
    }

    #[tokio::test]
    async fn from_gte_to_rejected() {
        let pool = test_pool().await;
        let engine = AnalyticsQueryEngine::new(pool);
        for (from, to) in [(T0, T0), (T0, T0 - 1)] {
            let err = engine
                .query_summary(&params(QueryScope::Route, from, to))
                .await
                .unwrap_err();
            assert!(matches!(err, QueryError::InvalidQuery(_)), "got {err:?}");
        }
    }

    #[tokio::test]
    async fn route_filter_isolates_route_scope_summary_and_trend() {
        let pool = test_pool().await;
        seed(
            &pool,
            &[
                success_request("route-a", T0, 100),
                success_request("route-a", T0 + 1000, 100),
                success_request("route-b", T0, 100),
                // Unassigned failures (no route identity) must never match
                // a route filter.
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::ClientError,
                        None,
                        T0,
                        1,
                        None,
                        None,
                        None,
                        None,
                    ),
                    vec![],
                ),
            ],
        )
        .await;
        let engine = AnalyticsQueryEngine::new(pool);

        // No filter: everything (3 requests + 1 unassigned client_error).
        let unfiltered = engine
            .query_summary(&params(QueryScope::Route, T0, T0 + HOUR))
            .await
            .unwrap();
        assert_eq!(unfiltered.overall.requests, 4);
        assert_eq!(unfiltered.groups.len(), 2);
        assert_eq!(unfiltered.unassigned_failures.len(), 1);

        let mut p = params(QueryScope::Route, T0, T0 + HOUR);
        p.route = Some("route-a".to_string());
        let summary = engine.query_summary(&p).await.unwrap();
        assert_eq!(summary.overall.requests, 2);
        assert_eq!(summary.overall.success, 2);
        assert_eq!(summary.groups.len(), 1);
        assert!(
            matches!(&summary.groups[0].key, GroupKey::Route { route_name } if route_name == "route-a"),
            "route=b data leaked into route=a filter: {:?}",
            summary.groups
        );
        assert_eq!(summary.groups[0].metrics.requests, 2);
        // The unassigned client_error is not attributable to any route.
        assert!(summary.unassigned_failures.is_empty());

        // route=b must not see route=a data.
        p.route = Some("route-b".to_string());
        let summary = engine.query_summary(&p).await.unwrap();
        assert_eq!(summary.overall.requests, 1);
        assert_eq!(summary.groups.len(), 1);
        assert!(
            matches!(&summary.groups[0].key, GroupKey::Route { route_name } if route_name == "route-b"),
            "route=a data leaked into route=b filter: {:?}",
            summary.groups
        );

        // Unknown route name matches nothing (never a silent no-filter).
        p.route = Some("route-nope".to_string());
        let summary = engine.query_summary(&p).await.unwrap();
        assert_eq!(summary.overall.requests, 0);
        assert!(summary.groups.is_empty());

        // Empty string = no filter (same convention as provider/real_model).
        p.route = Some(String::new());
        let summary = engine.query_summary(&p).await.unwrap();
        assert_eq!(summary.overall.requests, 4);

        // The trend path honors the same filter.
        p.route = Some("route-a".to_string());
        let trend = engine.query_trend(&p).await.unwrap();
        assert_eq!(trend.series.len(), 1);
        assert!(
            matches!(&trend.series[0].key, GroupKey::Route { route_name } if route_name == "route-a"),
            "route=b data leaked into route=a trend filter: {:?}",
            trend.series
        );
        assert_eq!(trend.series[0].total_requests, 2);
        assert_eq!(
            trend
                .buckets
                .iter()
                .map(|b| b.overall.requests)
                .sum::<u64>(),
            2
        );
    }

    #[tokio::test]
    async fn target_scope_route_filter_follows_owning_route() {
        let pool = test_pool().await;
        // Both routes share the same (provider, real_model) tuple, so the
        // group key alone cannot tell them apart — the filter must resolve
        // the owning route of each attempt.
        seed(
            &pool,
            &[
                success_request("route-a", T0, 100),
                success_request("route-b", T0, 100),
            ],
        )
        .await;
        let engine = AnalyticsQueryEngine::new(pool);

        let mut p = params(QueryScope::Target, T0, T0 + HOUR);
        p.route = Some("route-a".to_string());
        let summary = engine.query_summary(&p).await.unwrap();
        assert_eq!(summary.overall.attempts, 1);
        assert_eq!(summary.groups.len(), 1);
        assert!(
            matches!(&summary.groups[0].key, GroupKey::Target { provider, real_model }
                if provider == "openai" && real_model == "gpt-4o"),
            "unexpected group: {:?}",
            summary.groups
        );

        p.route = Some("route-b".to_string());
        let summary = engine.query_summary(&p).await.unwrap();
        assert_eq!(summary.overall.attempts, 1);
        assert_eq!(summary.groups.len(), 1);

        // Unknown route matches nothing.
        p.route = Some("route-nope".to_string());
        let summary = engine.query_summary(&p).await.unwrap();
        assert_eq!(summary.overall.attempts, 0);
        assert!(summary.groups.is_empty());

        // Trend path: the same per-route isolation.
        p.route = Some("route-a".to_string());
        let trend = engine.query_trend(&p).await.unwrap();
        assert_eq!(trend.series.len(), 1);
        assert_eq!(trend.series[0].total_requests, 1);
        assert_eq!(
            trend
                .buckets
                .iter()
                .map(|b| b.overall.attempts)
                .sum::<u64>(),
            1
        );
    }

    #[tokio::test]
    async fn trend_buckets_zero_fill_and_group_series() {
        let pool = test_pool().await;
        seed(
            &pool,
            &[
                success_request("chat", T0, 100),            // hour h0
                success_request("chat", T0 + 2 * HOUR, 100), // hour h2
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::NoRoute,
                        None,
                        T0 + 2 * HOUR + 1,
                        1,
                        None,
                        None,
                        None,
                        None,
                    ),
                    vec![],
                ), // unassigned, hour h2
            ],
        )
        .await;
        let engine = AnalyticsQueryEngine::new(pool);

        let from = T0;
        let to = T0 + 3 * HOUR;
        let trend = engine
            .query_trend(&params(QueryScope::Route, from, to))
            .await
            .unwrap();
        // T0 is mid-hour, so the span covers 4 UTC hour slots (h0..h3).
        assert_eq!(trend.buckets.len(), 4);
        let h0 = hour_start(T0);
        assert_eq!(trend.buckets[0].hour_ms, h0);
        assert_eq!(trend.buckets[1].hour_ms, h0 + HOUR);
        assert_eq!(trend.buckets[2].hour_ms, h0 + 2 * HOUR);
        assert_eq!(trend.buckets[3].hour_ms, h0 + 3 * HOUR);
        assert_eq!(trend.buckets[0].overall.requests, 1);
        assert_eq!(trend.buckets[1].overall.requests, 0); // zero-filled
        assert_eq!(trend.buckets[2].overall.requests, 2); // success + no_route
        assert_eq!(trend.buckets[3].overall.requests, 0); // zero-filled

        assert_eq!(trend.series.len(), 1);
        let series = &trend.series[0];
        assert!(matches!(&series.key, GroupKey::Route { route_name } if route_name == "chat"));
        assert_eq!(series.total_requests, 2);
        assert_eq!(series.points.len(), 4);
        assert_eq!(
            series.points.iter().map(|p| p.requests).collect::<Vec<_>>(),
            vec![1, 0, 1, 0]
        );
        assert_eq!(
            series.points.iter().map(|p| p.success).collect::<Vec<_>>(),
            vec![1, 0, 1, 0]
        );

        // Summary over the same span must equal the bucket sum.
        let summary = engine
            .query_summary(&params(QueryScope::Route, from, to))
            .await
            .unwrap();
        assert_eq!(summary.overall.requests, 3);
        assert_eq!(summary.overall.no_route, 1);

        // Target trend: only the attempts of the two success requests.
        let target_trend = engine
            .query_trend(&params(QueryScope::Target, from, to))
            .await
            .unwrap();
        assert_eq!(target_trend.buckets.len(), 4);
        assert_eq!(
            target_trend
                .buckets
                .iter()
                .map(|b| b.overall.attempts)
                .collect::<Vec<_>>(),
            vec![1, 0, 1, 0]
        );
        let series = &target_trend.series[0];
        assert!(
            matches!(&series.key, GroupKey::Target { provider, real_model } if provider == "openai" && real_model == "gpt-4o")
        );
        assert_eq!(series.total_requests, 2);
    }

    #[tokio::test]
    async fn trend_hour_buckets_align_utc_by_completion() {
        let pool = test_pool().await;
        // B is an exact UTC hour boundary: events at B-1 (previous hour) and
        // B (next hour) must land in different buckets.
        let b = hour_start(T0) + HOUR;
        seed(
            &pool,
            &[
                success_request("chat", b - 1, 100),
                success_request("chat", b, 100),
            ],
        )
        .await;
        let engine = AnalyticsQueryEngine::new(pool);

        let from = hour_start(T0);
        let trend = engine
            .query_trend(&params(QueryScope::Route, from, from + 2 * HOUR))
            .await
            .unwrap();
        assert_eq!(trend.buckets.len(), 2);
        assert_eq!(trend.buckets[0].hour_ms, from);
        assert_eq!(trend.buckets[0].overall.requests, 1);
        assert_eq!(trend.buckets[1].hour_ms, b);
        assert_eq!(trend.buckets[1].overall.requests, 1);
    }

    #[tokio::test]
    async fn trend_bucket_latency_metrics() {
        let pool = test_pool().await;
        let h0 = hour_start(T0);
        // h0: 3 successes (100/200/300 ms); h1: 1 success (50 ms).
        seed(
            &pool,
            &[
                success_request("chat", h0 + 1, 100),
                success_request("chat", h0 + 2, 200),
                success_request("chat", h0 + 3, 300),
                success_request("chat", h0 + HOUR + 1, 50),
            ],
        )
        .await;
        let engine = AnalyticsQueryEngine::new(pool);

        let trend = engine
            .query_trend(&params(QueryScope::Route, h0, h0 + 2 * HOUR))
            .await
            .unwrap();
        assert_eq!(trend.buckets.len(), 2);
        let b0 = &trend.buckets[0];
        assert_eq!(b0.overall.e2e.samples, 3);
        assert_eq!(b0.overall.e2e.population, 3);
        assert_close(b0.overall.e2e.coverage.unwrap(), 1.0);
        assert_eq!(b0.overall.e2e.avg_ms, Some(200.0));
        // nearest-rank: ceil(0.5 * 3) = 2nd smallest of [100, 200, 300].
        assert_eq!(b0.overall.e2e.p50_ms, Some(200.0));
        assert_eq!(b0.overall.e2e.p95_ms, Some(300.0));

        let b1 = &trend.buckets[1];
        assert_eq!(b1.overall.e2e.samples, 1);
        assert_close(b1.overall.e2e.coverage.unwrap(), 1.0);
        assert_eq!(b1.overall.e2e.p50_ms, Some(50.0));

        // An empty hour yields an empty latency block: coverage None (not 0),
        // no fabricated percentiles.
        let trend = engine
            .query_trend(&params(QueryScope::Route, h0, h0 + 3 * HOUR))
            .await
            .unwrap();
        let b2 = &trend.buckets[2];
        assert_eq!(b2.overall.e2e.samples, 0);
        assert_eq!(b2.overall.e2e.population, 0);
        assert_eq!(b2.overall.e2e.coverage, None);
        assert_eq!(b2.overall.e2e.p50_ms, None);
    }

    #[tokio::test]
    async fn trend_multi_group_series_sorted_and_zero_filled() {
        let pool = test_pool().await;
        let h0 = hour_start(T0);
        seed(
            &pool,
            &[
                // "chat": 3 total (2 in h0, 1 in h2).
                success_request("chat", h0 + 1, 100),
                success_request("chat", h0 + 2, 100),
                success_request("chat", h0 + 2 * HOUR + 1, 100),
                // "a": 1 in h0; "b": 1 in h1 — a key-ordered tie at 1.
                success_request("a", h0 + 3, 100),
                success_request("b", h0 + HOUR + 1, 100),
            ],
        )
        .await;
        let engine = AnalyticsQueryEngine::new(pool);

        let from = h0;
        let to = h0 + 3 * HOUR;
        let trend = engine
            .query_trend(&params(QueryScope::Route, from, to))
            .await
            .unwrap();
        // Series sorted by total_requests descending, ties by key ascending.
        let names: Vec<&str> = trend
            .series
            .iter()
            .map(|s| match &s.key {
                GroupKey::Route { route_name } => route_name.as_str(),
                _ => panic!("route scope expected"),
            })
            .collect();
        assert_eq!(names, vec!["chat", "a", "b"]);
        assert_eq!(
            trend
                .series
                .iter()
                .map(|s| s.total_requests)
                .collect::<Vec<_>>(),
            vec![3, 1, 1]
        );
        // Every series is zero-filled across all 3 UTC hour slots of the
        // (hour-aligned) span.
        let points =
            |i: usize| -> Vec<u64> { trend.series[i].points.iter().map(|p| p.requests).collect() };
        assert_eq!(points(0), vec![2, 0, 1]);
        assert_eq!(points(1), vec![1, 0, 0]);
        assert_eq!(points(2), vec![0, 1, 0]);
        // Success and token series follow the same zero-fill shape.
        assert_eq!(
            trend.series[0]
                .points
                .iter()
                .map(|p| p.success)
                .collect::<Vec<_>>(),
            vec![2, 0, 1]
        );
        assert_eq!(
            trend.series[0]
                .points
                .iter()
                .map(|p| p.total_tokens)
                .collect::<Vec<_>>(),
            vec![60, 0, 30] // 3 requests × usage(10,20,30)
        );
    }

    #[tokio::test]
    async fn target_trend_provider_and_real_model_filters() {
        let pool = test_pool().await;
        seed(
            &pool,
            &[event(
                fact(
                    Uuid::new_v4(),
                    FinalStatus::Success,
                    Some("chat"),
                    T0,
                    500,
                    Some(usage(10, 20, 30)),
                    None,
                    None,
                    None,
                ),
                vec![
                    attempt(
                        "openai",
                        "gpt-4o",
                        TargetStatus::Success,
                        T0,
                        100,
                        Some(usage(10, 20, 30)),
                        None,
                        None,
                        None,
                    ),
                    attempt(
                        "openai",
                        "gpt-4o-mini",
                        TargetStatus::Failure,
                        T0 + 1000,
                        200,
                        None,
                        None,
                        None,
                        None,
                    ),
                    attempt(
                        "anthropic",
                        "claude-3",
                        TargetStatus::Success,
                        T0 + 2000,
                        150,
                        Some(usage(5, 5, 10)),
                        None,
                        None,
                        None,
                    ),
                ],
            )],
        )
        .await;
        let engine = AnalyticsQueryEngine::new(pool);

        let mut p = params(QueryScope::Target, T0, T0 + HOUR);
        p.provider = Some("openai".to_string());
        let trend = engine.query_trend(&p).await.unwrap();
        assert_eq!(trend.buckets.len(), 2);
        assert_eq!(trend.buckets[0].overall.attempts, 2);
        assert_eq!(trend.buckets[0].overall.success, 1);
        assert_eq!(trend.buckets[0].overall.attempt_failure, 1);
        assert_eq!(trend.buckets[1].overall.attempts, 0); // zero-filled
        assert_eq!(trend.series.len(), 2);
        assert!(trend.series.iter().all(|s| matches!(
            &s.key,
            GroupKey::Target { provider, .. } if provider == "openai"
        )));
        assert_eq!(
            trend.series.iter().map(|s| s.total_requests).sum::<u64>(),
            2
        );

        p.provider = None;
        p.real_model = Some("gpt-4o".to_string());
        let trend = engine.query_trend(&p).await.unwrap();
        assert_eq!(trend.buckets[0].overall.attempts, 1);
        assert_eq!(trend.buckets[0].overall.success, 1);
        assert_eq!(trend.series.len(), 1);
        assert!(matches!(
            &trend.series[0].key,
            GroupKey::Target { provider, real_model }
                if provider == "openai" && real_model == "gpt-4o"
        ));

        // Route scope rejects target-only filters for trend queries too.
        let err = engine
            .query_trend(&QueryParams {
                scope: QueryScope::Route,
                from_ms: T0,
                to_ms: T0 + HOUR,
                provider: Some("openai".to_string()),
                route: None,
                real_model: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, QueryError::InvalidQuery(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn cache_rates_unknown_in_token_denominator_not_in_request_denominator() {
        let pool = test_pool().await;
        // Two successes with usage but cached_tokens = NULL: the request
        // hit-rate denominator (cache-known samples) is 0, so
        // `request_cache_hit_rate` is None; the token hit-rate denominator
        // is **all** success prompt_tokens, so the unknown samples DO enter
        // it (their cached contribution is 0 → rate 0.0, not None).
        let mk = |completed_ms: i64, cached: Option<u32>| {
            event(
                fact(
                    Uuid::new_v4(),
                    FinalStatus::Success,
                    Some("chat"),
                    completed_ms,
                    100,
                    Some(usage(10, 5, 15)),
                    cached,
                    None,
                    None,
                ),
                vec![attempt(
                    "openai",
                    "gpt-4o",
                    TargetStatus::Success,
                    completed_ms,
                    90,
                    Some(usage(10, 5, 15)),
                    cached,
                    None,
                    None,
                )],
            )
        };
        seed(&pool, &[mk(T0, None), mk(T0 + 1000, None)]).await;
        let engine = AnalyticsQueryEngine::new(pool.clone());

        let summary = engine
            .query_summary(&params(QueryScope::Route, T0, T0 + HOUR))
            .await
            .unwrap();
        assert_eq!(summary.overall.cache_known, 0);
        assert_eq!(summary.overall.cache_unknown, 2);
        // All-success prompt_tokens (20) is the token denominator; the NULL
        // cached_tokens contribute 0 → 0.0 (not None).
        assert_eq!(summary.overall.cache_prompt_tokens, 20);
        assert_eq!(summary.overall.cache_tokens, 0);
        assert_eq!(summary.overall.request_cache_hit_rate, None);
        assert_close(summary.overall.token_cache_hit_rate.unwrap(), 0.0);

        // Contrast: a known miss (cached_tokens = 0, prompt = 10) gives a
        // real request denominator and explicit rates — 0 is a miss, not
        // unknown; the token denominator grows to all 30 prompt_tokens.
        seed(&pool, &[mk(T0 + 2000, Some(0))]).await;
        let summary = engine
            .query_summary(&params(QueryScope::Route, T0, T0 + HOUR))
            .await
            .unwrap();
        assert_eq!(summary.overall.cache_known, 1);
        assert_eq!(summary.overall.cache_unknown, 2);
        assert_eq!(summary.overall.cache_prompt_tokens, 30);
        assert_close(summary.overall.request_cache_hit_rate.unwrap(), 0.0);
        assert_close(summary.overall.token_cache_hit_rate.unwrap(), 0.0);
    }

    // ═══════════════════════════════════════════════════════════════
    // Mixed path (spans > 24h): 1h Rollup for the historical full hours +
    // fact details for the recent 24h / boundary / missing hours.
    // ═══════════════════════════════════════════════════════════════

    #[tokio::test]
    async fn mixed_25h_rollup_plus_recent_facts() {
        let pool = test_pool().await;
        let h0 = hour_start(T0);
        // Historical hour (rollup): 2 successes + 1 no_route (unassigned).
        seed(
            &pool,
            &[
                success_request("chat", h0 + 1, 100),
                success_request("chat", h0 + 2, 200),
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::NoRoute,
                        None,
                        h0 + 3,
                        1,
                        None,
                        None,
                        None,
                        None,
                    ),
                    vec![],
                ),
            ],
        )
        .await;
        // Recent 24h (details): 2 successes in h0+H and h0+23H.
        seed(
            &pool,
            &[
                success_request("chat", h0 + HOUR + 1, 300),
                success_request("chat", h0 + 23 * HOUR + 1, 400),
            ],
        )
        .await;
        rollup_hours(&pool, h0, h0 + HOUR + 1).await;

        let engine = AnalyticsQueryEngine::new(pool.clone());
        let from = h0;
        let to = h0 + 25 * HOUR;
        let summary = engine
            .query_summary(&params(QueryScope::Route, from, to))
            .await
            .unwrap();
        // 2 rollup successes + 2 detail successes + 1 rollup no_route.
        assert_eq!(summary.overall.requests, 5);
        assert_eq!(summary.overall.success, 4);
        assert_eq!(summary.overall.no_route, 1);
        assert_eq!(summary.overall.total_tokens, 4 * 30);
        assert_eq!(summary.overall.cache_known, 4); // all 4 successes report cached_tokens
        assert_eq!(summary.overall.cache_unknown, 0);
        // Token hit rate across the rollup + detail merge: all 4 successes
        // carry cached_tokens = 3 / prompt = 10 → 12 / 40.
        assert_close(summary.overall.token_cache_hit_rate.unwrap(), 12.0 / 40.0);
        assert_eq!(summary.overall.e2e.samples, 4);
        assert_eq!(summary.overall.e2e.population, 4);
        assert_close(summary.overall.e2e.coverage.unwrap(), 1.0);
        // avg merges rollup sums + detail values exactly: (100+200+300+400)/4.
        assert_close(summary.overall.e2e.avg_ms.unwrap(), 250.0);
        // The merged block spans rollup + detail → approximate percentiles.
        assert!(summary.overall.e2e.approx_percentiles);
        let p50 = summary.overall.e2e.p50_ms.unwrap();
        assert!(
            (p50 - 200.0).abs() < 10.0,
            "p50={p50} (expected ≈200, bucket lower bound)"
        );
        assert_eq!(
            summary.unassigned_failures,
            vec![UnassignedFailure {
                category: "no_route".to_string(),
                count: 1
            }]
        );
        let g = find_group(
            &summary,
            &GroupKey::Route {
                route_name: "chat".into(),
            },
        );
        assert_eq!(g.metrics.requests, 4);
        assert_close(g.metrics.success_rate.unwrap(), 1.0);
        assert!(g.metrics.e2e.approx_percentiles);

        // Trend: 25 hourly buckets; the historical hour is approximate, the
        // recent detail hours exact; bucket sum == summary overall.
        let trend = engine
            .query_trend(&params(QueryScope::Route, from, to))
            .await
            .unwrap();
        assert_eq!(trend.buckets.len(), 25);
        assert_eq!(trend.buckets[0].overall.requests, 3);
        assert_eq!(trend.buckets[0].overall.e2e.samples, 2);
        assert!(trend.buckets[0].overall.e2e.approx_percentiles);
        assert_eq!(trend.buckets[1].overall.requests, 1);
        assert!(!trend.buckets[1].overall.e2e.approx_percentiles);
        assert_eq!(trend.buckets[23].overall.requests, 1);
        assert!(!trend.buckets[23].overall.e2e.approx_percentiles);
        assert_eq!(trend.buckets[24].overall.requests, 0); // zero-filled
        assert_eq!(
            trend
                .buckets
                .iter()
                .map(|b| b.overall.requests)
                .sum::<u64>(),
            summary.overall.requests
        );
        let series = &trend.series[0];
        assert!(matches!(&series.key, GroupKey::Route { route_name } if route_name == "chat"));
        assert_eq!(series.total_requests, 4);
        assert_eq!(series.points[0].requests, 2);
        assert_eq!(series.points[1].requests, 1);
        assert_eq!(series.points[23].requests, 1);
        assert_eq!(
            series.points.iter().map(|p| p.requests).sum::<u64>(),
            summary.overall.success
        );
    }

    #[tokio::test]
    async fn mixed_48h_target_scope_joins_route_targets() {
        let pool = test_pool().await;
        let h0 = hour_start(T0);
        seed(
            &pool,
            &[
                // h0 (rollup): chat route via openai/gpt-4o + anthropic/claude-3.
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::Success,
                        Some("chat"),
                        h0,
                        100,
                        Some(usage(10, 20, 30)),
                        None,
                        None,
                        None,
                    ),
                    vec![
                        attempt(
                            "openai",
                            "gpt-4o",
                            TargetStatus::Success,
                            h0,
                            80,
                            Some(usage(10, 20, 30)),
                            None,
                            None,
                            None,
                        ),
                        attempt(
                            "anthropic",
                            "claude-3",
                            TargetStatus::Failure,
                            h0 + 1000,
                            50,
                            None,
                            None,
                            None,
                            None,
                        ),
                    ],
                ),
                // h0 + 24h (recent detail): one more gpt-4o attempt.
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::Success,
                        Some("chat"),
                        h0 + 24 * HOUR,
                        100,
                        Some(usage(10, 20, 30)),
                        None,
                        None,
                        None,
                    ),
                    vec![attempt(
                        "openai",
                        "gpt-4o",
                        TargetStatus::Success,
                        h0 + 24 * HOUR,
                        80,
                        Some(usage(10, 20, 30)),
                        None,
                        None,
                        None,
                    )],
                ),
            ],
        )
        .await;
        rollup_hours(&pool, h0, h0 + HOUR + 1).await;

        let engine = AnalyticsQueryEngine::new(pool.clone());
        let from = h0;
        let to = h0 + 48 * HOUR;
        let summary = engine
            .query_summary(&params(QueryScope::Target, from, to))
            .await
            .unwrap();
        assert_eq!(summary.overall.attempts, 3);
        assert_eq!(summary.overall.success, 2);
        assert_eq!(summary.overall.attempt_failure, 1);
        assert_eq!(summary.overall.total_tokens, 60); // 2 successes × 30
        assert_eq!(summary.overall.e2e.samples, 3);
        assert_close(summary.overall.e2e.coverage.unwrap(), 1.0);
        assert!(summary.overall.e2e.approx_percentiles);
        let a = find_group(
            &summary,
            &GroupKey::Target {
                provider: "openai".into(),
                real_model: "gpt-4o".into(),
            },
        );
        assert_eq!(a.metrics.attempts, 2);
        assert_eq!(a.metrics.success, 2);
        let b = find_group(
            &summary,
            &GroupKey::Target {
                provider: "anthropic".into(),
                real_model: "claude-3".into(),
            },
        );
        assert_eq!(b.metrics.attempts, 1);
        assert_eq!(b.metrics.attempt_failure, 1);
        // Serialized output must never leak UUID identities.
        let json = serde_json::to_value(&summary).unwrap();
        assert!(!json.to_string().contains("target_id"));

        // Trend: 48 buckets; h0 from rollup (approx), h0+24h from details.
        let trend = engine
            .query_trend(&params(QueryScope::Target, from, to))
            .await
            .unwrap();
        assert_eq!(trend.buckets.len(), 48);
        assert_eq!(trend.buckets[0].overall.attempts, 2);
        assert!(trend.buckets[0].overall.e2e.approx_percentiles);
        assert_eq!(trend.buckets[24].overall.attempts, 1);
        assert!(!trend.buckets[24].overall.e2e.approx_percentiles);
        assert_eq!(trend.series.len(), 2);
        let s = trend
            .series
            .iter()
            .find(|s| {
                matches!(&s.key, GroupKey::Target { provider, real_model }
                    if provider == "openai" && real_model == "gpt-4o")
            })
            .unwrap();
        assert_eq!(s.total_requests, 2);
        assert_eq!(s.points[0].requests, 1);
        assert_eq!(s.points[24].requests, 1);
    }

    #[tokio::test]
    async fn mixed_missing_hour_backfilled_from_details_and_registered() {
        let pool = test_pool().await;
        let h0 = hour_start(T0);
        seed(
            &pool,
            &[
                success_request("chat", h0 + 1, 100),            // h0: rollup'd
                success_request("chat", h0 + HOUR + 1, 200),     // h0+H: NOT rollup'd
                success_request("chat", h0 + 2 * HOUR + 1, 300), // h0+2H: recent detail
            ],
        )
        .await;
        // Compute only h0; h0+H stays unregistered (missing bucket).
        rollup_hours(&pool, h0, h0 + HOUR + 1).await;

        let engine = AnalyticsQueryEngine::new(pool.clone());
        let from = h0;
        let to = h0 + 26 * HOUR; // rollup region [h0, h0+2H): h0 completed, h0+H missing
        let summary = engine
            .query_summary(&params(QueryScope::Route, from, to))
            .await
            .unwrap();
        assert_eq!(summary.overall.requests, 3);
        assert_eq!(summary.overall.success, 3);
        assert_eq!(summary.overall.total_tokens, 90);
        assert_eq!(summary.overall.e2e.samples, 3);

        // Trend: the missing hour is backfilled from the details, not zero.
        let trend = engine
            .query_trend(&params(QueryScope::Route, from, to))
            .await
            .unwrap();
        assert_eq!(trend.buckets.len(), 26);
        assert_eq!(trend.buckets[0].overall.requests, 1);
        assert_eq!(trend.buckets[1].overall.requests, 1);
        assert_eq!(trend.buckets[2].overall.requests, 1);

        // The missing hour was registered for backfill (pending, no compute).
        let status = RollupEngine::new(pool)
            .bucket_status(h0 + HOUR)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.state, BucketState::Pending);
        assert!(!status.dirty);
    }

    #[tokio::test]
    async fn mixed_dirty_hour_backfilled_from_details_and_reregistered() {
        let pool = test_pool().await;
        let h0 = hour_start(T0);
        seed(&pool, &[success_request("chat", h0 + 1, 100)]).await; // h0
        seed(&pool, &[success_request("chat", h0 + HOUR + 1, 200)]).await; // h0+H
        rollup_hours(&pool, h0, h0 + 2 * HOUR).await; // both completed
        // Late fact into h0 → the writer marks the completed bucket dirty.
        seed(&pool, &[success_request("chat", h0 + 2, 150)]).await;

        let engine = AnalyticsQueryEngine::new(pool.clone());
        let from = h0;
        let to = h0 + 26 * HOUR;
        let summary = engine
            .query_summary(&params(QueryScope::Route, from, to))
            .await
            .unwrap();
        // h0 is dirty → answered from the details (incl. the late fact);
        // h0+H is completed+clean → rollup.
        assert_eq!(summary.overall.requests, 3);
        assert_eq!(summary.overall.success, 3);
        // avg over the merged values [100, 150, 200] — exact.
        assert_close(summary.overall.e2e.avg_ms.unwrap(), 150.0);
        assert!(summary.overall.e2e.approx_percentiles);

        // The dirty hour was re-registered as pending for backfill.
        let status = RollupEngine::new(pool.clone())
            .bucket_status(h0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.state, BucketState::Pending);
        assert!(!status.dirty);
        // The clean hour is untouched.
        let status = RollupEngine::new(pool)
            .bucket_status(h0 + HOUR)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.state, BucketState::Completed);
    }

    #[tokio::test]
    async fn mixed_boundary_no_duplication_no_omission() {
        let pool = test_pool().await;
        // T0 is mid-hour: for the 48h span the rollup region is
        // [h0+H, h0+24H); the details cover [T0, h0+H) ∪ [h0+24H, T0+48h).
        let h0 = hour_start(T0);
        let from = T0;
        let to = T0 + 48 * HOUR;
        seed(
            &pool,
            &[
                success_request("chat", from, 100), // == from: included (detail)
                success_request("chat", h0 + HOUR - 1, 100), // last ms of partial hour: detail
                success_request("chat", h0 + HOUR, 100), // first full rollup hour: rollup
                success_request("chat", h0 + 24 * HOUR - 1, 100), // last ms of rollup region: rollup
                success_request("chat", h0 + 24 * HOUR, 100),     // recent 24h start: detail
                success_request("chat", to - 1, 100),             // last ms before to: detail
                success_request("chat", to, 100),                 // == to: excluded
            ],
        )
        .await;
        // Compute only the full rollup hours.
        rollup_hours(&pool, h0 + HOUR, h0 + 24 * HOUR).await;

        let engine = AnalyticsQueryEngine::new(pool.clone());
        let summary = engine
            .query_summary(&params(QueryScope::Route, from, to))
            .await
            .unwrap();
        // Exactly the 6 included events; the == to one must never appear.
        assert_eq!(summary.overall.requests, 6);
        assert_eq!(summary.overall.success, 6);

        let trend = engine
            .query_trend(&params(QueryScope::Route, from, to))
            .await
            .unwrap();
        // T0 is mid-hour, so the 48h span covers 49 UTC hour slots; the
        // `to` event (== to) must never appear anywhere.
        assert_eq!(trend.buckets.len(), 49);
        assert_eq!(
            trend
                .buckets
                .iter()
                .map(|b| b.overall.requests)
                .sum::<u64>(),
            6
        );
        assert_eq!(trend.buckets[0].overall.requests, 2); // partial first hour (details)
        assert_eq!(trend.buckets[1].overall.requests, 1); // rollup
        assert_eq!(trend.buckets[23].overall.requests, 1); // rollup
        assert_eq!(trend.buckets[24].overall.requests, 1); // recent details
        assert_eq!(trend.buckets[48].overall.requests, 1); // recent details (to - 1)
    }

    #[tokio::test]
    async fn mixed_summary_and_trend_filters() {
        let pool = test_pool().await;
        let h0 = hour_start(T0);
        seed(
            &pool,
            &[
                success_request("route-a", h0 + 1, 100),
                success_request("route-b", h0 + 2, 100),
                event(
                    fact(
                        Uuid::new_v4(),
                        FinalStatus::ClientError,
                        None,
                        h0 + 3,
                        1,
                        None,
                        None,
                        None,
                        None,
                    ),
                    vec![],
                ),
            ],
        )
        .await;
        seed(&pool, &[success_request("route-a", h0 + 24 * HOUR, 100)]).await; // recent detail
        rollup_hours(&pool, h0, h0 + HOUR + 1).await;

        let engine = AnalyticsQueryEngine::new(pool.clone());
        let from = h0;
        let to = h0 + 25 * HOUR;

        // Route filter isolates route-a across the rollup + detail parts;
        // the unassigned client_error must never match.
        let mut p = params(QueryScope::Route, from, to);
        p.route = Some("route-a".to_string());
        let summary = engine.query_summary(&p).await.unwrap();
        assert_eq!(summary.overall.requests, 2);
        assert_eq!(summary.groups.len(), 1);
        assert!(summary.unassigned_failures.is_empty());
        let trend = engine.query_trend(&p).await.unwrap();
        assert_eq!(trend.series.len(), 1);
        assert_eq!(trend.series[0].total_requests, 2);
        assert_eq!(
            trend
                .buckets
                .iter()
                .map(|b| b.overall.requests)
                .sum::<u64>(),
            2
        );

        // Target filters (provider / real_model) over rollup + details:
        // all three attempts share openai/gpt-4o.
        let mut p = params(QueryScope::Target, from, to);
        p.provider = Some("openai".to_string());
        let summary = engine.query_summary(&p).await.unwrap();
        assert_eq!(summary.overall.attempts, 3);
        assert_eq!(summary.groups.len(), 1);
        assert!(summary.groups.iter().all(|g| matches!(
            &g.key,
            GroupKey::Target { provider, .. } if provider == "openai"
        )));

        p.provider = None;
        p.real_model = Some("gpt-4o".to_string());
        let summary = engine.query_summary(&p).await.unwrap();
        assert_eq!(summary.overall.attempts, 3);
        assert_eq!(summary.groups.len(), 1);
        let trend = engine.query_trend(&p).await.unwrap();
        assert_eq!(trend.series.len(), 1);
        assert_eq!(trend.series[0].total_requests, 3);

        // Unknown route matches nothing in the rollup part either.
        let mut p = params(QueryScope::Route, from, to);
        p.route = Some("route-nope".to_string());
        let summary = engine.query_summary(&p).await.unwrap();
        assert_eq!(summary.overall.requests, 0);
        assert!(summary.groups.is_empty());
    }

    #[tokio::test]
    async fn mixed_rollup_latency_histogram_approximate_and_overflow() {
        let pool = test_pool().await;
        let h0 = hour_start(T0);
        // 10 successes with e2e 1..=10 ms (attempts share the duration —
        // the generic helper subtracts 10ms and would go negative here).
        let mut events: Vec<AnalyticsEvent> = Vec::new();
        for i in 0..10 {
            let dur = (i + 1) as i64;
            events.push(event(
                fact(
                    Uuid::new_v4(),
                    FinalStatus::Success,
                    Some("chat"),
                    h0 + i as i64 + 1,
                    dur,
                    Some(usage(1, 2, 3)),
                    Some(0),
                    None,
                    None,
                ),
                vec![attempt(
                    "openai",
                    "gpt-4o",
                    TargetStatus::Success,
                    h0 + i as i64 + 1,
                    dur,
                    Some(usage(1, 2, 3)),
                    Some(0),
                    None,
                    None,
                )],
            ));
        }
        seed(&pool, &events).await;
        // One extreme value beyond the fixed histogram capacity (overflow
        // bucket). It cannot go through the chrono-based fact builder
        // (`started_at` would fall outside chrono's range), so it is
        // inserted directly — `duration_ms` is writer-enforced, not a DB
        // CHECK.
        sqlx::query(
            "INSERT INTO request_results \
             (request_id, route_id, final_status, requested_model, started_at, completed_at, \
              duration_ms, prompt_tokens, completion_tokens, total_tokens, cached_tokens) \
             VALUES (?, (SELECT route_id FROM analytics_routes WHERE route_name = 'chat'), \
                     'success', 'test-model', 0, ?, ?, 1, 2, 3, 0)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(h0 + 100_000)
        .bind(500_000_000_000_000_000i64) // 5e17 ms → overflow bucket
        .execute(&pool)
        .await
        .unwrap();
        rollup_hours(&pool, h0, h0 + HOUR + 1).await;

        let engine = AnalyticsQueryEngine::new(pool.clone());
        let summary = engine
            .query_summary(&params(QueryScope::Route, h0, h0 + 25 * HOUR))
            .await
            .unwrap();
        let e2e = &summary.overall.e2e;
        assert_eq!(e2e.samples, 11);
        assert_eq!(e2e.population, 11);
        assert_close(e2e.coverage.unwrap(), 1.0);
        assert!(
            e2e.approx_percentiles,
            "rollup-only block must be approximate"
        );
        assert!(e2e.low_sample);
        // avg stays exact even for the overflow value: (55 + 5e17) / 11.
        assert_close(e2e.avg_ms.unwrap(), (55.0 + 5e17) / 11.0);
        // p50: nearest-rank (rank 6) over cumulative bucket counts → the
        // lower bound of the 6ms bucket (≈6, ±1% bucket error).
        let p50 = e2e.p50_ms.unwrap();
        assert!((p50 - 6.0).abs() < 0.1, "p50={p50}");
        // p99: rank 11 lands in the overflow region → the overflow bucket's
        // lower bound (precision not guaranteed, per spec).
        assert_close(
            e2e.p99_ms.unwrap(),
            1.01f64.powi((crate::rollup::MAX_HISTOGRAM_BUCKETS - 1) as i32),
        );
        // ttft / upstream stay empty (no values seeded).
        assert_eq!(summary.overall.ttft.samples, 0);
        assert_eq!(summary.overall.ttft.p50_ms, None);
    }
}
