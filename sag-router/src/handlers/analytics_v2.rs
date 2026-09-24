//! Unified analytics API v2 handlers (`/v1/analytics/v2/summary|trend`).
//!
//! These handlers are a thin HTTP contract over the analytics query
//! kernel (`sag_analytics::AnalyticsQueryEngine`), which owns every metric
//! definition (failure rates, tokens, cache, latency) and the span rules:
//! spans ≤ 24h are answered exactly from the facts, longer spans via the
//! mixed rollup + detail path (historical full UTC hours from the 1h
//! rollup tables, the rest backfilled from the details). The handler only:
//!
//! - parses / validates query params (`from`/`to` required, unix ms,
//!   left-closed right-open; `scope=route|target`, default `route`, other
//!   values 400; optional `route=<route_name>` filter in both scopes;
//!   `provider`/`real_model` target scope only);
//! - maps kernel errors to the standard `{"error": {code, message}}` JSON
//!   error format (4xx for bad input, 500 for database failures);
//! - trims the trend series to Top-N + `others` (default 20, server cap 50)
//!   using the kernel-provided `total_requests` — it never recomputes
//!   metrics itself.
//!
//! The query engine runs over the **shared** analytics pool wired into
//! `AppState::analytics_query` (the same pool the event writer commits to),
//! so responses always observe exactly what was persisted.

use axum::{
    Json,
    body::Body,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use sag_analytics::{
    GroupTrendSeries, QueryError, QueryParams, QueryScope, TrendBucket, TrendPoint, TrendResult,
};
use sag_core::AppError;
use sag_state::AppState;
use serde::Deserialize;
use tracing::{error, instrument};

/// Default number of leading series kept for trend responses.
pub const DEFAULT_TREND_TOP: u64 = 20;

/// Server-side cap on trend `top`: the client may request at most this
/// many named series; the remainder beyond `top` is aggregated into the
/// `others` series.
pub const MAX_TREND_TOP: u64 = 50;

/// Query parameters shared by `/v1/analytics/v2/summary` and
/// `/v1/analytics/v2/trend`.
///
/// `from` / `to` are required unix-ms timestamps (left-closed right-open
/// interval). `scope` defaults to `route`; **unknown `scope` values are
/// rejected with a 400** — silently defaulting a misspelled scope would
/// change the query's semantics. `route=<route_name>` is an optional
/// filter in **both** scopes (spec: `route=<route_name>` 可选). `provider`
/// / `real_model` are target-scope-only filters; with `scope=route` the
/// query kernel rejects them with a 4xx. `top` is trend-only and ignored
/// by summary.
///
/// Unknown extra query parameters are ignored (serde default) per the
/// gateway's forward-compat principle.
#[derive(Debug, Deserialize)]
pub struct AnalyticsV2Query {
    /// Required: interval start, unix ms (left-closed).
    pub from: Option<i64>,
    /// Required: interval end, unix ms (right-open).
    pub to: Option<i64>,
    /// `route` (default) or `target`; anything else is a 400.
    pub scope: Option<String>,
    /// Optional: keep only facts of this `route_name` (both scopes; empty
    /// string = no filter).
    pub route: Option<String>,
    /// Target scope only: keep attempts from this provider.
    pub provider: Option<String>,
    /// Target scope only: keep attempts with this real model.
    pub real_model: Option<String>,
    /// Trend only: number of leading series to keep (default 20, capped at
    /// 50 server-side).
    #[serde(default = "default_top")]
    pub top: u64,
}

const fn default_top() -> u64 {
    DEFAULT_TREND_TOP
}

/// GET `/v1/analytics/v2/summary`
///
/// Returns `overall` + `groups` + `unassigned_failures` for the span, keyed
/// by `route_name` (route scope) or `(provider, real_model)` (target
/// scope). UUID identities never appear in the response.
#[instrument(skip(state))]
pub async fn analytics_v2_summary(
    State(state): State<AppState>,
    Query(params): Query<AnalyticsV2Query>,
) -> Response<Body> {
    let query_params = match build_query_params(&params) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let Some(engine) = state.analytics_query.as_ref() else {
        error!("analytics query engine not wired into AppState");
        return AppError::Internal(
            "analytics query engine not initialized; check gateway assembly".into(),
        )
        .into_response();
    };
    match engine.query_summary(&query_params).await {
        Ok(summary) => (StatusCode::OK, Json(summary)).into_response(),
        Err(e) => map_query_error(e).into_response(),
    }
}

/// GET `/v1/analytics/v2/trend`
///
/// Returns per-UTC-hour `buckets` (overall metrics) plus per-group `series`
/// trimmed to the top `top` series by `total_requests` (default 20, server
/// cap 50); the trimmed remainder is aggregated into an `others` series
/// with per-hour sums. Trimming uses the kernel-provided `total_requests`
/// — no metrics are recomputed here.
#[instrument(skip(state))]
pub async fn analytics_v2_trend(
    State(state): State<AppState>,
    Query(params): Query<AnalyticsV2Query>,
) -> Response<Body> {
    let query_params = match build_query_params(&params) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let Some(engine) = state.analytics_query.as_ref() else {
        error!("analytics query engine not wired into AppState");
        return AppError::Internal(
            "analytics query engine not initialized; check gateway assembly".into(),
        )
        .into_response();
    };
    let top = params.top.clamp(1, MAX_TREND_TOP);
    match engine.query_trend(&query_params).await {
        Ok(trend) => {
            let response = trim_trend(trend, top);
            (StatusCode::OK, Json(response)).into_response()
        }
        Err(e) => map_query_error(e).into_response(),
    }
}

/// Response of `/v1/analytics/v2/trend` after Top-N + `others` trimming.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TrendResponse {
    pub scope: QueryScope,
    pub from_ms: i64,
    pub to_ms: i64,
    /// One bucket per UTC hour of the span, zero-filled (overall metrics).
    pub buckets: Vec<TrendBucket>,
    /// The `top` leading series by `total_requests` descending.
    pub series: Vec<GroupTrendSeries>,
    /// Aggregation of every series beyond `top` (absent when nothing was
    /// trimmed). Not a real group: no `key`, just per-hour sums.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub others: Option<OthersTrendSeries>,
}

/// Per-hour sums over the series trimmed from the trend response.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct OthersTrendSeries {
    /// Σ `total_requests` of the trimmed series.
    pub total_requests: u64,
    /// Per-hour sums of requests / success / tokens across trimmed series.
    pub points: Vec<TrendPoint>,
}

/// Validate and translate HTTP query params into kernel [`QueryParams`].
fn build_query_params(params: &AnalyticsV2Query) -> Result<QueryParams, AppError> {
    let from_ms = params
        .from
        .ok_or_else(|| AppError::BadRequest("analytics query requires 'from' (unix ms)".into()))?;
    let to_ms = params
        .to
        .ok_or_else(|| AppError::BadRequest("analytics query requires 'to' (unix ms)".into()))?;
    // `scope` defaults to `route`; any other explicit value is rejected —
    // silently falling back on a misspelled scope would change the query's
    // semantics instead of telling the client about the typo.
    let scope = match params.scope.as_deref() {
        None | Some("route") => QueryScope::Route,
        Some("target") => QueryScope::Target,
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "invalid analytics scope '{other}': expected 'route' or 'target'"
            )));
        }
    };
    Ok(QueryParams {
        scope,
        from_ms,
        to_ms,
        provider: params.provider.clone(),
        real_model: params.real_model.clone(),
        route: params.route.clone(),
    })
}

/// Map kernel errors onto the standard JSON error format.
///
/// Database failures are **redacted**: the raw sqlx message is logged
/// (`error!(?e, ...)`) but never sent to the client — the response carries
/// a generic internal-error message instead.
fn map_query_error(e: QueryError) -> Response<Body> {
    match &e {
        // Invalid input (from >= to, route scope with target-only filters)
        // is a client error; the kernel message spells out which one.
        // SpanTooLong is reserved for API-layer compatibility with older
        // kernels — the current kernel answers spans > 24h via the mixed
        // rollup + detail path (200, with approximate percentiles on the
        // rollup region), so this branch should never fire in practice.
        QueryError::InvalidQuery(_) | QueryError::SpanTooLong { .. } => {
            AppError::BadRequest(e.to_string()).into_response()
        }
        QueryError::Database(_) => {
            error!(?e, "analytics v2 query failed");
            AppError::Internal(
                "analytics query failed due to an internal database error; see server logs".into(),
            )
            .into_response()
        }
    }
}

/// Keep the `top` leading series (by `total_requests` desc, kernel-sorted)
/// and aggregate the remainder into an `others` series.
fn trim_trend(trend: TrendResult, top: u64) -> TrendResponse {
    let total = trend.series.len();
    let keep = (top as usize).min(total);
    let (leading, rest) = trend.series.split_at(keep);
    let others = if rest.is_empty() {
        None
    } else {
        Some(aggregate_others(rest))
    };
    TrendResponse {
        scope: trend.scope,
        from_ms: trend.from_ms,
        to_ms: trend.to_ms,
        buckets: trend.buckets,
        series: leading.to_vec(),
        others,
    }
}

/// Sum every trimmed series per hour (all series are zero-filled across the
/// same hour set, so indexing by position is safe).
fn aggregate_others(rest: &[GroupTrendSeries]) -> OthersTrendSeries {
    let hour_count = rest.first().map(|s| s.points.len()).unwrap_or(0);
    let mut points: Vec<TrendPoint> = Vec::with_capacity(hour_count);
    for i in 0..hour_count {
        let mut p = TrendPoint {
            hour_ms: rest[0].points[i].hour_ms,
            requests: 0,
            success: 0,
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        };
        for series in rest {
            let pt = &series.points[i];
            p.requests += pt.requests;
            p.success += pt.success;
            p.prompt_tokens += pt.prompt_tokens;
            p.completion_tokens += pt.completion_tokens;
            p.total_tokens += pt.total_tokens;
        }
        points.push(p);
    }
    OthersTrendSeries {
        total_requests: rest.iter().map(|s| s.total_requests).sum(),
        points,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use sag_analytics::AnalyticsEvent;
    use sag_core::{
        FailureReason, FinalStatus, RequestResultFact, TargetAttemptFact, TargetStatus, Usage,
    };
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    /// Fixed reference instant (unix ms) inside a single UTC hour.
    const REF_TS_MS: i64 = 1_700_000_000_000;

    fn at(offset_ms: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp_millis(REF_TS_MS + offset_ms).unwrap()
    }

    fn result(
        request_id: uuid::Uuid,
        status: FinalStatus,
        route_name: Option<&str>,
    ) -> RequestResultFact {
        RequestResultFact {
            request_id,
            route_name: route_name.map(str::to_string),
            requested_model: "test-model".to_string(),
            final_status: status,
            failure_reason: None,
            stream: false,
            started_at: at(0),
            completed_at: at(250),
            usage: None,
            cached_tokens: None,
            cache_creation_tokens: None,
            ttft_ms: None,
            upstream_duration_ms: None,
            ingress_protocol: None,
            lane: None,
        }
    }

    fn success_usage() -> Usage {
        Usage {
            prompt_tokens: 10,
            completion_tokens: 20,
            total_tokens: 30,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        }
    }

    /// A successful route-level event with one attempt per `(provider,
    /// real_model)` tuple and success token usage.
    fn success_event(route_name: &str, attempts: Vec<(&str, &str)>) -> AnalyticsEvent {
        let mut r = result(uuid::Uuid::now_v7(), FinalStatus::Success, Some(route_name));
        r.usage = Some(success_usage());
        AnalyticsEvent {
            result: r,
            targets: attempts
                .into_iter()
                .map(|(provider, model)| TargetAttemptFact {
                    provider: provider.to_string(),
                    real_model: model.to_string(),
                    status: TargetStatus::Success,
                    failure_reason: None,
                    started_at: at(0),
                    completed_at: at(200),
                    usage: Some(success_usage()),
                    cached_tokens: None,
                    ttft_ms: None,
                    upstream_duration_ms: None,
                    upstream_protocol: None,
                    lane: None,
                })
                .collect(),
        }
    }

    /// A model-failure event (no usage, no attempts).
    fn failure_event(route_name: &str) -> AnalyticsEvent {
        let mut r = result(
            uuid::Uuid::now_v7(),
            FinalStatus::ModelFailure,
            Some(route_name),
        );
        r.failure_reason = Some(FailureReason::ProviderError);
        AnalyticsEvent::new(r)
    }

    /// A client-error event (no route identity → unassigned).
    fn client_error_event() -> AnalyticsEvent {
        let mut r = result(uuid::Uuid::now_v7(), FinalStatus::ClientError, None);
        r.failure_reason = Some(FailureReason::InvalidRequest);
        AnalyticsEvent::new(r)
    }

    /// Build an [`AppState`] with the query engine wired over the same
    /// in-memory pool the event writer commits to (same wiring as
    /// `sag_state::testing::test_app_state`, minus the hidden pool).
    async fn build_state() -> (AppState, sqlx::SqlitePool) {
        let config = sag_config::GatewayConfig {
            ..sag_config::gateway_config_base!()
        };
        let handle = sag_config::ConfigHandle::from_config(config);
        let provider_registry = sag_state::testing::build_provider_registry(&handle, false).await;
        let pool = sag_analytics::open_analytics_pool(":memory:")
            .await
            .unwrap();
        let state = AppState {
            config: handle.clone(),
            providers: provider_registry.clone(),
            coordinator: Arc::new(sag_state::ConfigMutationCoordinator::new(
                handle,
                provider_registry,
            )),
            collector: Arc::new(sag_analytics::CallLogCollector::new_memory().await.unwrap()),
            event_collector: Arc::new(sag_analytics::EventCollector::new(
                sag_analytics::EventWriter::new(pool.clone()),
            )),
            analytics_query: Some(Arc::new(sag_analytics::AnalyticsQueryEngine::new(
                pool.clone(),
            ))),
            circuit_breaker: Arc::new(RwLock::new(sag_core::RouteCircuitBreaker::default())),
            active_targets: Arc::default(),
            fill_first_orders: Arc::default(),
        };
        (state, pool)
    }

    /// Seed events and drain the collector so the writer commits before
    /// any query runs.
    async fn seed(state: &AppState, events: Vec<AnalyticsEvent>) {
        for event in events {
            state.event_collector.try_emit(event).unwrap();
        }
        state.event_collector.shutdown().await;
    }

    /// Minimal router exposing exactly the v2 handlers under test.
    fn v2_app(state: AppState) -> Router {
        Router::new()
            .route("/v1/analytics/v2/summary", get(analytics_v2_summary))
            .route("/v1/analytics/v2/trend", get(analytics_v2_trend))
            .with_state(state)
    }

    async fn get_json(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn summary_route_scope_returns_overall_groups_and_unassigned() {
        let (state, pool) = build_state().await;
        seed(
            &state,
            vec![
                success_event("gpt-4o", vec![("openai", "gpt-4o")]),
                success_event("gpt-4o", vec![("openai", "gpt-4o")]),
                failure_event("dall-e-3"),
                client_error_event(),
            ],
        )
        .await;
        let app = v2_app(state);
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/summary?from=1700000000000&to=1700003600000",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["scope"], "route");
        assert_eq!(json["overall"]["requests"], 4);
        assert_eq!(json["overall"]["success"], 2);
        assert_eq!(json["overall"]["model_failure"], 1);
        assert_eq!(json["overall"]["client_error"], 1);

        // Groups keyed by business route_name — never UUIDs.
        let groups = json["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        let names: Vec<String> = groups
            .iter()
            .map(|g| g["key"]["route_name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"gpt-4o".to_string()));
        assert!(names.contains(&"dall-e-3".to_string()));
        assert!(groups.iter().all(|g| g["key"].get("route_id").is_none()));

        // Unassigned failures: client_error has no route identity.
        let unassigned = json["unassigned_failures"].as_array().unwrap();
        assert!(
            unassigned
                .iter()
                .any(|u| { u["category"] == "client_error" && u["count"] == 1 })
        );

        // No UUID leaks anywhere in the response body.
        let body = serde_json::to_string(&json).unwrap();
        let route_ids: Vec<String> = sqlx::query_scalar("SELECT route_id FROM analytics_routes")
            .fetch_all(&pool)
            .await
            .unwrap();
        for id in &route_ids {
            assert!(!body.contains(id), "route_id {id} leaked into the response");
        }
        let target_ids: Vec<String> =
            sqlx::query_scalar("SELECT target_id FROM analytics_route_targets")
                .fetch_all(&pool)
                .await
                .unwrap();
        for id in &target_ids {
            assert!(
                !body.contains(id),
                "target_id {id} leaked into the response"
            );
        }
    }

    #[tokio::test]
    async fn trend_target_scope_supports_provider_filter() {
        let (state, _pool) = build_state().await;
        seed(
            &state,
            vec![
                success_event("gpt-4o", vec![("openai", "gpt-4o")]),
                success_event("claude", vec![("anthropic", "claude-3.5-sonnet")]),
                success_event("mixed", vec![("openai", "gpt-4o")]),
            ],
        )
        .await;
        let app = v2_app(state);
        // Hour-aligned span: [1_699_999_200_000, 1_700_002_800_000) is
        // exactly one UTC hour, so bucketing is deterministic.
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/trend?from=1699999200000&to=1700002800000&scope=target&provider=openai",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["scope"], "target");
        assert_eq!(json["from_ms"].as_i64(), Some(1_699_999_200_000));
        assert_eq!(json["to_ms"].as_i64(), Some(1_700_002_800_000));
        assert_eq!(json["buckets"].as_array().unwrap().len(), 1);

        // Only the openai (provider, real_model) group survives the filter.
        let series = json["series"].as_array().unwrap();
        assert_eq!(series.len(), 1);
        assert_eq!(series[0]["key"]["provider"], "openai");
        assert_eq!(series[0]["key"]["real_model"], "gpt-4o");
        assert!(series[0]["key"].get("target_id").is_none());
        assert_eq!(series[0]["total_requests"], 2);
        assert_eq!(series[0]["points"][0]["requests"], 2);
        assert_eq!(series[0]["points"][0]["total_tokens"], 60);
        // Nothing trimmed: no `others` key at all.
        assert!(json.get("others").is_none());
    }

    #[tokio::test]
    async fn missing_from_or_to_returns_400_json_error() {
        let (state, _pool) = build_state().await;
        let app = v2_app(state.clone());
        let (status, json) = get_json(app, "/v1/analytics/v2/summary?to=1700003600000").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"]["code"], "BAD_REQUEST");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("'from'")
        );

        let app = v2_app(state);
        let (status, json) = get_json(app, "/v1/analytics/v2/summary?from=1700000000000").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"]["code"], "BAD_REQUEST");
        assert!(json["error"]["message"].as_str().unwrap().contains("'to'"));
    }

    #[tokio::test]
    async fn from_gte_to_returns_400_json_error() {
        let (state, _pool) = build_state().await;
        let app = v2_app(state);
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/summary?from=1700000000000&to=1700000000000",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"]["code"], "BAD_REQUEST");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("must be < to_ms")
        );
    }

    #[tokio::test]
    async fn span_over_24h_returns_200_mixed_rollup_summary_and_trend() {
        let (state, _pool) = build_state().await;
        // 25 h span (90_000_000 ms), hour-aligned: [h0, h0 + 25h). The
        // seeded facts all land in the historical region, but with an
        // empty rollup table (in-memory pool, no hours rolled up) the
        // mixed path backfills those hours from the details, so every fact
        // is still counted — no SpanTooLong 400 anymore.
        seed(
            &state,
            vec![
                success_event("gpt-4o", vec![("openai", "gpt-4o")]),
                success_event("gpt-4o", vec![("openai", "gpt-4o")]),
                failure_event("dall-e-3"),
            ],
        )
        .await;
        let app = v2_app(state.clone());
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/summary?from=1699999200000&to=1700089200000",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["scope"], "route");
        assert_eq!(json["from_ms"].as_i64(), Some(1_699_999_200_000));
        assert_eq!(json["to_ms"].as_i64(), Some(1_700_089_200_000));
        assert_eq!(json["overall"]["requests"], 3);
        assert_eq!(json["overall"]["success"], 2);
        assert_eq!(json["overall"]["model_failure"], 1);
        let groups = json["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|g| g["key"].get("route_id").is_none()));
        // Every fact came from the detail backfill (no rollup rows in this
        // pool), so percentiles are exact and the approx marker is absent.
        assert!(
            json["overall"]["e2e"].get("approx_percentiles").is_none(),
            "detail-only facts must not be flagged approximate: {json}"
        );

        let app = v2_app(state);
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/trend?from=1699999200000&to=1700089200000",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let buckets = json["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 25, "one zero-filled bucket per UTC hour");
        assert_eq!(buckets[0]["overall"]["requests"], 3);
        assert!(buckets[1..].iter().all(|b| b["overall"]["requests"] == 0));
        let series = json["series"].as_array().unwrap();
        assert_eq!(series.len(), 2);
        assert_eq!(series[0]["key"]["route_name"], "gpt-4o");
        assert_eq!(series[0]["total_requests"], 2);
        assert_eq!(series[0]["points"][0]["requests"], 2);
    }

    #[tokio::test]
    async fn span_over_48h_empty_db_returns_zeroed_200_result() {
        let (state, _pool) = build_state().await;
        let app = v2_app(state.clone());
        // 48 h span on an empty database: the mixed rollup + detail path
        // answers with a zero-filled, exact-style result (200), matching
        // the kernel's empty-DB semantics — never a 400.
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/summary?from=1699999200000&to=1700172000000",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["overall"]["requests"], 0);
        assert_eq!(json["overall"]["e2e"]["samples"], 0);
        assert!(json["overall"]["e2e"]["coverage"].is_null());
        assert!(json["overall"]["e2e"].get("approx_percentiles").is_none());
        assert!(json["groups"].as_array().unwrap().is_empty());
        assert!(json["unassigned_failures"].as_array().unwrap().is_empty());

        let app = v2_app(state);
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/trend?from=1699999200000&to=1700172000000",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let buckets = json["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 48, "one zero-filled bucket per UTC hour");
        assert!(buckets.iter().all(|b| b["overall"]["requests"] == 0));
        assert!(json["series"].as_array().unwrap().is_empty());
        assert!(json.get("others").is_none());
    }

    #[tokio::test]
    async fn route_scope_rejects_provider_filter_with_4xx() {
        let (state, _pool) = build_state().await;
        let app = v2_app(state);
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/summary?from=1700000000000&to=1700003600000&provider=openai",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"]["code"], "BAD_REQUEST");
        assert!(
            json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("only supported for scope = target")
        );
    }

    #[tokio::test]
    async fn trend_trims_to_top20_plus_others_by_total_requests() {
        let (state, _pool) = build_state().await;
        let events: Vec<AnalyticsEvent> = (0..25)
            .map(|i| success_event(&format!("route-{i:02}"), vec![("openai", "gpt-4o")]))
            .collect();
        seed(&state, events).await;
        let app = v2_app(state.clone());
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/trend?from=1699999200000&to=1700002800000",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let series = json["series"].as_array().unwrap();
        assert_eq!(series.len(), 20, "default top must keep 20 named series");
        let others = json["others"].as_object().unwrap();
        assert_eq!(others["total_requests"], 5);
        let points = others["points"].as_array().unwrap();
        assert_eq!(points.len(), 1, "one UTC hour in the span");
        assert_eq!(points[0]["requests"], 5);
        assert_eq!(points[0]["success"], 5);
        assert_eq!(points[0]["total_tokens"], 5 * 30);

        // When everything fits, `others` is absent.
        let app = v2_app(state);
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/trend?from=1699999200000&to=1700002800000&top=100",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["series"].as_array().unwrap().len(), 25);
        assert!(json.get("others").is_none());
    }

    #[tokio::test]
    async fn trend_top_is_capped_at_50_series_server_side() {
        let (state, _pool) = build_state().await;
        let events: Vec<AnalyticsEvent> = (0..55)
            .map(|i| success_event(&format!("route-{i:02}"), vec![("openai", "gpt-4o")]))
            .collect();
        seed(&state, events).await;
        let app = v2_app(state);
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/trend?from=1699999200000&to=1700002800000&top=100",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json["series"].as_array().unwrap().len(),
            50,
            "server must cap top at 50"
        );
        assert_eq!(json["others"]["total_requests"], 5);
    }

    #[tokio::test]
    async fn route_filter_isolates_route_scope_summary_and_trend() {
        let (state, _pool) = build_state().await;
        seed(
            &state,
            vec![
                success_event("route-a", vec![("openai", "gpt-4o")]),
                success_event("route-a", vec![("openai", "gpt-4o")]),
                success_event("route-b", vec![("openai", "gpt-4o")]),
                // Unassigned client_error: never matches a route filter.
                client_error_event(),
            ],
        )
        .await;
        let app = v2_app(state.clone());
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/summary?from=1700000000000&to=1700003600000&route=route-a",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["overall"]["requests"], 2);
        assert_eq!(json["overall"]["client_error"], 0);
        let groups = json["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "route=b data leaked: {json}");
        assert_eq!(groups[0]["key"]["route_name"], "route-a");
        assert_eq!(groups[0]["metrics"]["requests"], 2);
        assert!(
            json["unassigned_failures"].as_array().unwrap().is_empty(),
            "unassigned failures must not leak through a route filter: {json}"
        );

        // The trend endpoint honors the same filter (route=a → 2 requests,
        // no route=b series).
        let app = v2_app(state);
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/trend?from=1699999200000&to=1700002800000&route=route-a",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let series = json["series"].as_array().unwrap();
        assert_eq!(series.len(), 1, "route=b data leaked: {json}");
        assert_eq!(series[0]["key"]["route_name"], "route-a");
        assert_eq!(series[0]["total_requests"], 2);
        assert_eq!(series[0]["points"][0]["requests"], 2);
    }

    #[tokio::test]
    async fn target_scope_route_filter_keeps_only_owning_routes_attempts() {
        let (state, _pool) = build_state().await;
        // Both routes use the same (provider, real_model) tuple, so the
        // attempt count per group proves the route filter works.
        seed(
            &state,
            vec![
                success_event("route-a", vec![("openai", "gpt-4o")]),
                success_event("route-b", vec![("openai", "gpt-4o")]),
            ],
        )
        .await;
        let app = v2_app(state);
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/summary?from=1700000000000&to=1700003600000&scope=target&route=route-a",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["overall"]["attempts"], 1);
        let groups = json["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "route=b attempts leaked: {json}");
        assert_eq!(groups[0]["key"]["provider"], "openai");
        assert_eq!(groups[0]["key"]["real_model"], "gpt-4o");
        assert_eq!(groups[0]["metrics"]["attempts"], 1);
    }

    #[tokio::test]
    async fn invalid_scope_value_returns_400_not_silent_fallback() {
        let (state, _pool) = build_state().await;
        for bogus in ["bogus", "Route", "Target", "routes"] {
            let app = v2_app(state.clone());
            let (status, json) = get_json(
                app,
                &format!(
                    "/v1/analytics/v2/summary?from=1700000000000&to=1700003600000&scope={bogus}"
                ),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "scope={bogus}");
            assert_eq!(json["error"]["code"], "BAD_REQUEST", "scope={bogus}");
            let message = json["error"]["message"].as_str().unwrap();
            assert!(message.contains(bogus), "scope={bogus}: {message}");
            assert!(
                message.contains("route") && message.contains("target"),
                "scope={bogus}: {message}"
            );
        }

        // Explicit valid scopes still work.
        let app = v2_app(state.clone());
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/summary?from=1700000000000&to=1700003600000&scope=route",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["scope"], "route");

        // Missing scope defaults to route (unchanged).
        let app = v2_app(state);
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/summary?from=1700000000000&to=1700003600000",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["scope"], "route");
    }

    #[tokio::test]
    async fn target_scope_summary_supports_real_model_filter() {
        let (state, _pool) = build_state().await;
        seed(
            &state,
            vec![
                success_event("gpt-4o", vec![("openai", "gpt-4o")]),
                success_event("claude", vec![("anthropic", "claude-3.5-sonnet")]),
            ],
        )
        .await;
        let app = v2_app(state);
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/summary?from=1700000000000&to=1700003600000&scope=target&real_model=claude-3.5-sonnet",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["overall"]["attempts"], 1);
        assert_eq!(json["overall"]["success"], 1);
        assert_eq!(json["overall"]["total_tokens"], 30);
        let groups = json["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["key"]["provider"], "anthropic");
        assert_eq!(groups[0]["key"]["real_model"], "claude-3.5-sonnet");
    }

    #[tokio::test]
    async fn trend_top_zero_clamps_to_one_series() {
        let (state, _pool) = build_state().await;
        let events: Vec<AnalyticsEvent> = (0..3)
            .map(|i| success_event(&format!("route-{i}"), vec![("openai", "gpt-4o")]))
            .collect();
        seed(&state, events).await;
        let app = v2_app(state.clone());
        // top=0 must not drop every series: clamped to 1, the rest folds
        // into `others`.
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/trend?from=1699999200000&to=1700002800000&top=0",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let series = json["series"].as_array().unwrap();
        assert_eq!(series.len(), 1, "top=0 must clamp to 1 series");
        assert_eq!(json["others"]["total_requests"], 2);

        // top=2 keeps exactly two named series.
        let app = v2_app(state);
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/trend?from=1699999200000&to=1700002800000&top=2",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["series"].as_array().unwrap().len(), 2);
        assert_eq!(json["others"]["total_requests"], 1);
    }

    #[tokio::test]
    async fn database_failure_returns_generic_500_without_raw_sqlx_message() {
        let (state, pool) = build_state().await;
        // Force a database failure by closing the shared pool out from
        // under the engine; capture the raw kernel error for the leak check.
        pool.close().await;
        let engine = state.analytics_query.clone().unwrap();
        let raw = engine
            .query_summary(&QueryParams {
                scope: QueryScope::Route,
                from_ms: 1_700_000_000_000,
                to_ms: 1_700_003_600_000,
                provider: None,
                real_model: None,
                route: None,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(!raw.is_empty(), "closed pool must produce an error");

        let app = v2_app(state);
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/summary?from=1700000000000&to=1700003600000",
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(json["error"]["code"], "INTERNAL_ERROR");
        let message = json["error"]["message"].as_str().unwrap();
        assert!(
            !message.contains(&raw),
            "raw database error leaked to the client: {message}"
        );
        assert!(
            message.contains("internal"),
            "expected a generic internal message, got: {message}"
        );
    }

    #[tokio::test]
    async fn missing_query_engine_returns_generic_500() {
        let (state, _pool) = build_state().await;
        // Simulate a broken gateway assembly: the engine slot is empty.
        let mut state = state;
        state.analytics_query = None;
        let app = v2_app(state);
        let (status, json) = get_json(
            app,
            "/v1/analytics/v2/summary?from=1700000000000&to=1700003600000",
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(json["error"]["code"], "INTERNAL_ERROR");
        let message = json["error"]["message"].as_str().unwrap();
        assert!(
            message.contains("not initialized") && !message.contains("sqlx"),
            "unexpected message: {message}"
        );
    }
}
