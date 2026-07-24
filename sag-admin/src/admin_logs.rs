//! Call-log read handlers for the admin API.
//!
//! Mounted at `/api/logs` by [`crate::admin::build_admin_routes`].
//! All routes are auth-protected by the parent router's `gateway_auth`
//! middleware, which is the same protection that wraps `/api/providers`
//! and `/api/routes`.

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use sag_analytics::LogFilter;
use sag_core::AppError;
use serde_json::json;
use tracing::{error, instrument};

use sag_state::AppState;

/// Query parameters accepted by `GET /api/logs`. Missing or empty
/// values are treated as "no filter" rather than a 400.
#[derive(Debug, serde::Deserialize)]
pub struct LogsQuery {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub real_model: Option<String>,
    #[serde(default)]
    pub status_code: Option<u16>,
    #[serde(default)]
    pub offset: u64,
    #[serde(default = "default_log_limit")]
    pub limit: u64,
}

fn default_log_limit() -> u64 {
    50
}

/// Maximum page size we will return regardless of what the caller asks
/// for. Mirrors the analytics-store clamp and keeps a runaway
/// `?limit=999999` from streaming gigabytes into the dashboard.
pub const MAX_LOG_LIMIT: u64 = 500;

fn build_filter(query: LogsQuery) -> LogFilter {
    LogFilter {
        // Empty-string params are treated as "filter not set" so the
        // dashboard's cleared filter inputs don't accidentally filter
        // everything out.
        provider: query.provider.filter(|s| !s.is_empty()),
        model: query.model.filter(|s| !s.is_empty()),
        real_model: query.real_model.filter(|s| !s.is_empty()),
        status_code: query.status_code,
        offset: query.offset,
        limit: query.limit.min(MAX_LOG_LIMIT),
    }
}

/// GET `/api/logs` — return a page of recent call-log entries with
/// optional filters. The response envelope `{ "data": [CallLog...], "total": total }`
/// matches the shape expected by the frontend pagination.
///
/// Rows are ordered `ORDER BY timestamp DESC` (newest first).
#[instrument(skip(state))]
pub async fn list_logs(
    State(state): State<AppState>,
    Query(params): Query<LogsQuery>,
) -> impl IntoResponse {
    let filter = build_filter(params);
    let store = state.collector.store();
    let logs_res = store.query_logs(filter.clone()).await;
    let count_res = store.count_logs(filter).await;

    match (logs_res, count_res) {
        (Ok(rows), Ok(total)) => (
            StatusCode::OK,
            Json(json!({
                "data": rows,
                "total": total,
            })),
        )
            .into_response(),
        (Err(e), _) | (_, Err(e)) => {
            error!(?e, "logs query or count failed");
            AppError::Internal(e.to_string()).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{build_state, test_app};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use chrono::Utc;
    use sag_core::{CallLog, Usage};
    use std::collections::HashMap;
    use tower::ServiceExt;
    use uuid::Uuid;

    fn make_log(provider: &str, model: &str, real_model: &str, status_code: u16) -> CallLog {
        let is_error = status_code >= 400;
        CallLog {
            request_id: Uuid::new_v4(),
            provider: provider.into(),
            model: model.into(),
            real_model: real_model.into(),
            stream: false,
            status_code,
            duration_ms: 150,
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            error: if is_error {
                Some("upstream failed".into())
            } else {
                None
            },
            timestamp: Utc::now(),
            reasoning_effort: None,
            upstream_duration_ms: None,
            ttft_ms: None,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
            user_agent: None,
            client_ip: None,
            error_category: if is_error {
                Some("provider_error".into())
            } else {
                None
            },
        }
    }

    async fn json_body(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 65_536)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn insert_logs(state: &AppState, logs: Vec<CallLog>) {
        state
            .collector
            .store()
            .insert_batch(&logs)
            .await
            .expect("insert_batch must succeed for test setup");
    }

    #[tokio::test]
    async fn list_logs_returns_envelope_with_data_array() {
        let state = build_state(HashMap::new(), vec![]).await;
        insert_logs(
            &state,
            vec![
                make_log("openai", "gpt-4o", "gpt-4o", 200),
                make_log("anthropic", "claude", "claude", 200),
            ],
        )
        .await;

        let app = test_app(state);
        let req = Request::builder()
            .method("GET")
            .uri("/api/logs")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        let data = body["data"].as_array().expect("data must be an array");
        assert_eq!(data.len(), 2);
        assert_eq!(body["total"], 2);
    }

    #[tokio::test]
    async fn list_logs_filters_by_provider() {
        let state = build_state(HashMap::new(), vec![]).await;
        insert_logs(
            &state,
            vec![
                make_log("openai", "gpt-4o", "gpt-4o", 200),
                make_log("anthropic", "claude", "claude", 200),
            ],
        )
        .await;

        let app = test_app(state);
        let req = Request::builder()
            .method("GET")
            .uri("/api/logs?provider=openai")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let data = json_body(response).await["data"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["provider"], "openai");
    }

    #[tokio::test]
    async fn list_logs_filters_by_status_code() {
        let state = build_state(HashMap::new(), vec![]).await;
        insert_logs(
            &state,
            vec![
                make_log("openai", "gpt-4o", "gpt-4o", 200),
                make_log("openai", "gpt-4o", "gpt-4o", 502),
            ],
        )
        .await;

        let app = test_app(state);
        let req = Request::builder()
            .method("GET")
            .uri("/api/logs?status_code=502")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let data = json_body(response).await["data"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["status_code"], 502);
    }

    /// Combination filter: `model` + `real_model` together. The Vue
    /// LogsPage exposes both filter inputs, so the contract should be
    /// locked in symmetrically with `provider` / `status_code`.
    #[tokio::test]
    async fn list_logs_filters_by_model_and_real_model() {
        let state = build_state(HashMap::new(), vec![]).await;
        insert_logs(
            &state,
            vec![
                make_log("openai", "gpt-4o", "gpt-4o", 200),
                make_log("openai", "gpt-4o", "gpt-4o-mini", 200),
                make_log("anthropic", "claude", "claude", 200),
            ],
        )
        .await;

        let app = test_app(state);
        let req = Request::builder()
            .method("GET")
            .uri("/api/logs?model=gpt-4o&real_model=gpt-4o-mini")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let data = json_body(response).await["data"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(data.len(), 1, "only the gpt-4o→gpt-4o-mini row matches");
        assert_eq!(data[0]["model"], "gpt-4o");
        assert_eq!(data[0]["real_model"], "gpt-4o-mini");
    }

    #[tokio::test]
    async fn list_logs_clamps_limit_to_max() {
        let state = build_state(HashMap::new(), vec![]).await;
        insert_logs(
            &state,
            vec![
                make_log("openai", "gpt-4o", "gpt-4o", 200),
                make_log("openai", "gpt-4o", "gpt-4o", 200),
            ],
        )
        .await;

        let app = test_app(state);
        // Ask for 9999 rows — should be silently clamped to MAX_LOG_LIMIT,
        // returning all 2 inserted rows without error.
        let req = Request::builder()
            .method("GET")
            .uri("/api/logs?limit=9999")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let data = json_body(response).await["data"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(data.len(), 2);
    }

    /// `?limit=500` should pass through unchanged (boundary of the
    /// clamp) — guards against accidentally capping one short.
    #[tokio::test]
    async fn list_logs_limit_at_max_passes_through() {
        let state = build_state(HashMap::new(), vec![]).await;
        insert_logs(&state, vec![make_log("openai", "gpt-4o", "gpt-4o", 200)]).await;

        let app = test_app(state);
        let req = Request::builder()
            .method("GET")
            .uri("/api/logs?limit=500")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let data = json_body(response).await["data"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(data.len(), 1);
    }

    /// `?limit=0` is a sanity probe — should return an empty data array,
    /// not 400 and not 500. SQLite's `LIMIT 0` returns no rows; we
    /// confirm the envelope stays consistent.
    #[tokio::test]
    async fn list_logs_limit_zero_returns_empty_envelope() {
        let state = build_state(HashMap::new(), vec![]).await;
        insert_logs(&state, vec![make_log("openai", "gpt-4o", "gpt-4o", 200)]).await;

        let app = test_app(state);
        let req = Request::builder()
            .method("GET")
            .uri("/api/logs?limit=0")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = json_body(response).await;
        let data = body["data"]
            .as_array()
            .expect("data must be array even when empty");
        assert_eq!(data.len(), 0);
    }

    #[tokio::test]
    async fn list_logs_applies_offset_pagination() {
        let state = build_state(HashMap::new(), vec![]).await;
        insert_logs(
            &state,
            vec![
                make_log("openai", "gpt-4o", "gpt-4o", 200),
                make_log("openai", "gpt-4o", "gpt-4o", 200),
                make_log("openai", "gpt-4o", "gpt-4o", 200),
            ],
        )
        .await;

        let app = test_app(state);
        let req = Request::builder()
            .method("GET")
            .uri("/api/logs?limit=2&offset=2")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let data = json_body(response).await["data"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(data.len(), 1, "3 - offset 2 = 1 remaining row");
    }

    #[tokio::test]
    async fn list_logs_empty_string_filter_is_treated_as_unset() {
        // The Vue dashboard's `logsFilterProvider.value = ""` would send
        // ?provider= and expect zero filtering. The `filter(|s| !s.is_empty())`
        // step in `build_filter` keeps semantics consistent.
        let state = build_state(HashMap::new(), vec![]).await;
        insert_logs(&state, vec![make_log("openai", "gpt-4o", "gpt-4o", 200)]).await;

        let app = test_app(state);
        let req = Request::builder()
            .method("GET")
            .uri("/api/logs?provider=")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let data = json_body(response).await["data"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(data.len(), 1, "empty filter must not exclude anything");
    }
}
