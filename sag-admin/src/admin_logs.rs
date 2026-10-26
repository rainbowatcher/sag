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
use sag_analytics::{LogFilter, SessionRecord};
use sag_core::{AppError, CallLog};
use tracing::{error, instrument};

use sag_state::AppState;

/// Maximum page size we will return regardless of what the caller asks
/// for. Re-exported from `sag-analytics` (single source of truth with the
/// store clamp) so a runaway `?limit=999999` cannot stream gigabytes into
/// the dashboard.
pub use sag_analytics::MAX_LOG_LIMIT;

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
    /// Hashed session identity — exact match on `call_logs.session_key`.
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub status_code: Option<String>,
    #[serde(default)]
    pub offset: u64,
    #[serde(default = "default_log_limit")]
    pub limit: u64,
}

fn default_log_limit() -> u64 {
    50
}

fn build_filter(query: LogsQuery) -> LogFilter {
    LogFilter {
        // Empty-string params are treated as "filter not set" so the
        // dashboard's cleared filter inputs don't accidentally filter
        // everything out.
        provider: query.provider.filter(|s| !s.is_empty()),
        model: query.model.filter(|s| !s.is_empty()),
        real_model: query.real_model.filter(|s| !s.is_empty()),
        session: query.session.filter(|s| !s.is_empty()),
        status_code: query
            .status_code
            .as_deref()
            .and_then(sag_analytics::StatusCodeFilter::parse),
        offset: query.offset,
        limit: query.limit.min(MAX_LOG_LIMIT),
    }
}

/// Response envelope for `GET /api/logs`: `{ "data": [...], "total": n }`
/// plus the optional `session` detail for a `?session=` filter.
///
/// The doubled `Option` encodes the three shapes the frontend must tell
/// apart:
///
/// | request | `session` field |
/// |---|---|
/// | no `?session=`, or empty value | **absent** (legacy envelope preserved) |
/// | `?session=<key>` with no registry row | `null` |
/// | `?session=<key>` with a registry row | [`SessionRecord`] object |
///
/// Timestamps use the same `chrono::DateTime<Utc>` serde representation as
/// [`CallLog::timestamp`] (RFC3339 / ISO8601).
#[derive(Debug, serde::Serialize)]
pub struct LogsResponse {
    pub data: Vec<CallLog>,
    pub total: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<Option<SessionRecord>>,
}

/// GET `/api/logs` — return a page of recent call-log entries with
/// optional filters. The response envelope `{ "data": [CallLog...], "total": total }`
/// matches the shape expected by the frontend pagination, extended with the
/// optional `session` field described on [`LogsResponse`].
///
/// Rows are ordered `ORDER BY timestamp DESC` (newest first).
#[instrument(skip(state))]
pub async fn list_logs(
    State(state): State<AppState>,
    Query(params): Query<LogsQuery>,
) -> impl IntoResponse {
    let filter = build_filter(params);
    // Captured before `filter` is consumed by `count_logs`.
    let session_key = filter.session.clone();
    let store = state.collector.store();
    let logs_res = store.query_logs(filter.clone()).await;
    let count_res = store.count_logs(filter).await;

    match (logs_res, count_res) {
        (Ok(rows), Ok(total)) => {
            // `?session=` present but unknown → `Some(None)` → `"session": null`.
            // A registry lookup failure degrades to `null` as well: the page
            // entry list is the primary payload and must not fail with the
            // (decorative) session card.
            let session = match session_key {
                None => None,
                Some(key) => match store.get_session(&key).await {
                    Ok(record) => Some(record),
                    Err(e) => {
                        error!(?e, %key, "session registry lookup failed");
                        Some(None)
                    }
                },
            };
            (
                StatusCode::OK,
                Json(LogsResponse {
                    data: rows,
                    total,
                    session,
                }),
            )
                .into_response()
        }
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
    use sag_analytics::SessionRegistryFact;
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
            // Internal analytics identity fields: not populated in tests.
            route_id: None,
            target_id: None,
            ingress_protocol: None,
            upstream_protocol: None,
            lane: None,
            loss_summary: None,
            session_key: None,
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

    /// Hashed session identity used by the session-detail tests. 32 hex
    /// chars, matching the `SessionKey::map_id` shape.
    const SESSION_KEY: &str = "a3f1c0de00000000000000000000beef";

    fn make_session_log(session_key: &str, status_code: u16) -> CallLog {
        CallLog {
            session_key: Some(session_key.into()),
            ..make_log("openai", "gpt-4o", "gpt-4o", status_code)
        }
    }

    /// Seed one `session_registry` row, as the dispatch layer + collector
    /// would on the session's first request.
    async fn insert_session(state: &AppState, fact: &SessionRegistryFact) {
        state
            .collector
            .store()
            .upsert_session(fact)
            .await
            .expect("upsert_session must succeed for test setup");
    }

    fn session_fact() -> SessionRegistryFact {
        let first_seen = Utc::now() - chrono::Duration::minutes(5);
        SessionRegistryFact {
            session_key: SESSION_KEY.into(),
            session_source: "meta".into(),
            conversation_id: Some("conv-8f21".into()),
            first_message_preview: Some("Summarize this quarterly report".into()),
            first_seen,
            last_seen: first_seen + chrono::Duration::minutes(4),
        }
    }

    async fn get_logs(state: AppState, uri: &str) -> serde_json::Value {
        let app = test_app(state);
        let req = Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        json_body(response).await
    }

    /// State seeded with one logged request and its registry row, i.e. the
    /// happy path a session filter runs against.
    async fn seeded_state() -> AppState {
        let state = build_state(HashMap::new(), vec![]).await;
        insert_logs(&state, vec![make_session_log(SESSION_KEY, 200)]).await;
        insert_session(&state, &session_fact()).await;
        state
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

    #[tokio::test]
    async fn list_logs_filters_by_non_2xx() {
        let state = build_state(HashMap::new(), vec![]).await;
        insert_logs(
            &state,
            vec![
                make_log("openai", "gpt-4o", "gpt-4o", 200),
                make_log("openai", "gpt-4o", "gpt-4o", 400),
                make_log("openai", "gpt-4o", "gpt-4o", 502),
            ],
        )
        .await;

        let app = test_app(state);
        let req = Request::builder()
            .method("GET")
            .uri("/api/logs?status_code=non_2xx")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let data = json_body(response).await["data"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(data.len(), 2);
        let codes: Vec<u64> = data
            .iter()
            .map(|r| r["status_code"].as_u64().unwrap())
            .collect();
        assert!(codes.contains(&400));
        assert!(codes.contains(&502));
        assert!(!codes.contains(&200));
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

    /// `?session=<known key>` → the envelope carries the registry record so
    /// the logs page can render the session info card from one response.
    #[tokio::test]
    async fn list_logs_session_filter_includes_session_record() {
        let body = get_logs(
            seeded_state().await,
            &format!("/api/logs?session={SESSION_KEY}"),
        )
        .await;

        let data = body["data"].as_array().expect("data must be an array");
        assert_eq!(data.len(), 1, "the session filter must match the row");
        assert_eq!(body["total"], 1);

        let session = body["session"]
            .as_object()
            .expect("session must be an object");
        assert_eq!(session["session_key"], SESSION_KEY);
        assert_eq!(session["session_source"], "meta");
        assert_eq!(session["conversation_id"], "conv-8f21");
        assert_eq!(
            session["first_message_preview"],
            "Summarize this quarterly report"
        );
        // Same ISO8601 shape as `CallLog::timestamp`.
        for field in ["first_seen", "last_seen"] {
            let raw = session[field]
                .as_str()
                .unwrap_or_else(|| panic!("{field} must be an ISO8601 string"));
            chrono::DateTime::parse_from_rfc3339(raw)
                .unwrap_or_else(|e| panic!("{field} is not RFC3339 ({raw}): {e}"));
        }
        let first =
            chrono::DateTime::parse_from_rfc3339(session["first_seen"].as_str().unwrap()).unwrap();
        let last =
            chrono::DateTime::parse_from_rfc3339(session["last_seen"].as_str().unwrap()).unwrap();
        assert!(first < last, "first_seen must precede last_seen");
    }

    /// `?session=<unknown key>` → `"session": null` while the entry list
    /// still comes back normally (old rows / pruned sessions).
    #[tokio::test]
    async fn list_logs_session_filter_unknown_key_yields_null_session() {
        let state = build_state(HashMap::new(), vec![]).await;
        // Log row present, registry row absent.
        insert_logs(&state, vec![make_session_log(SESSION_KEY, 200)]).await;

        let body = get_logs(state, &format!("/api/logs?session={SESSION_KEY}")).await;

        let session = body
            .get("session")
            .expect("session key must be present when filtering");
        assert!(session.is_null(), "unknown key must serialize as null");
        assert_eq!(body["data"].as_array().unwrap().len(), 1);
        assert_eq!(body["total"], 1);
    }

    /// No `?session=` at all → the legacy `{ data, total }` envelope is
    /// preserved byte-for-byte (no `session` key), keeping old clients and
    /// the current dashboard working.
    #[tokio::test]
    async fn list_logs_without_session_filter_omits_session_field() {
        let body = get_logs(seeded_state().await, "/api/logs").await;

        assert!(
            !body
                .as_object()
                .expect("envelope must be an object")
                .contains_key("session"),
            "no session filter must not add a session key: {body}"
        );
        assert_eq!(body["data"].as_array().unwrap().len(), 1);
    }

    /// `?session=` (empty value) keeps the existing "filter not set"
    /// semantics: every row matches and no `session` key is added.
    #[tokio::test]
    async fn list_logs_empty_session_filter_is_treated_as_unset() {
        let body = get_logs(seeded_state().await, "/api/logs?session=").await;

        assert!(
            !body
                .as_object()
                .expect("envelope must be an object")
                .contains_key("session"),
            "empty session filter must not add a session key: {body}"
        );
        assert_eq!(
            body["data"].as_array().unwrap().len(),
            1,
            "empty session filter must not exclude anything"
        );
    }
}
