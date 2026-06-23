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
use serde_json::json;
use tracing::{error, instrument};

use crate::state::AppState;

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
/// optional filters. The response envelope `{ "data": [CallLog...] }`
/// matches the shape the Vue LogsPage already expects in
/// `src/stores/analytics.ts`.
///
/// Rows are ordered `ORDER BY timestamp DESC` (newest first) — this
/// matches what the LogsPage renders top-down. The frontend's
/// `limit + 1` probe sentinel detects whether a next page exists.
#[instrument(skip(state))]
pub async fn list_logs(
    State(state): State<AppState>,
    Query(params): Query<LogsQuery>,
) -> impl IntoResponse {
    let filter = build_filter(params);
    match state.collector.store().query_logs(filter).await {
        Ok(rows) => (StatusCode::OK, Json(json!({ "data": rows }))).into_response(),
        Err(e) => {
            error!(?e, "logs query failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": { "code": "INTERNAL_ERROR", "message": e.to_string() }
                })),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
    };
    use chrono::Utc;
    use sag_analytics::CallLogCollector;
    use sag_config::{BackoffConfig, ConfigHandle, GatewayAuthConfig, GatewayConfig, ServerConfig};
    use sag_core::{CallLog, Usage};
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tower::ServiceExt;
    use uuid::Uuid;

    async fn build_state() -> AppState {
        let config = GatewayConfig {
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 3000,
            },
            analytics: None,
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["test-key".into()],
            }),
            retries: 2,
            backoff: BackoffConfig::default(),
            providers: HashMap::new(),
            routes: vec![],
        };

        AppState {
            config: ConfigHandle::from_config(config),
            providers: Arc::new(RwLock::new(HashMap::new())),
            collector: Arc::new(CallLogCollector::new_memory().await.unwrap()),
            route_counters: Arc::new(RwLock::new(HashMap::new())),
            route_circuit_breaker: Arc::new(RwLock::new(Default::default())),
        }
    }

    /// Build a test app mounting the admin routes without auth middleware.
    fn test_app(state: AppState) -> Router {
        Router::new()
            .nest("/api", crate::admin::build_admin_routes(state.clone()))
            .with_state(state)
    }

    fn make_log(provider: &str, model: &str, real_model: &str, status_code: u16) -> CallLog {
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
            error: if status_code >= 400 {
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
        let state = build_state().await;
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
    }

    #[tokio::test]
    async fn list_logs_filters_by_provider() {
        let state = build_state().await;
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
        let state = build_state().await;
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
        let state = build_state().await;
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
        let state = build_state().await;
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
        let state = build_state().await;
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
        let state = build_state().await;
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
        let state = build_state().await;
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
        let state = build_state().await;
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
