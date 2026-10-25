//! Session-affinity observation and reset for `session_sticky` routes.
//!
//! Mounted at `/api/session-affinity` by [`crate::admin::build_admin_routes`]:
//!
//! - `GET /api/session-affinity` — snapshot of the live session pins
//!   (`{ "entries": [...] }`), filterable by `model` / `key` / `limit`.
//! - `DELETE /api/session-affinity` — clear one session (`{ "key": ... }`),
//!   one request-model space (`{ "model": ... }`) or everything
//!   (`{ "all": true }`, which additionally requires `"confirm": true`).
//!
//! Keys are the **hashed** session identity (`SessionKey::map_id`, 32 hex
//! chars) — raw session values (conversation ids, prompt cache keys,
//! content hashes) never leave the dispatch path, so this API can never
//! leak them (design §4.6 / invariant 5).

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use sag_state::AppState;
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// Hard cap on the snapshot page size so a runaway `?limit=` cannot dump
/// the whole memory in one response.
pub const MAX_SESSION_ENTRIES: usize = 1_000;

/// Query parameters for `GET /api/session-affinity`. All optional; empty
/// strings are treated as "no filter".
#[derive(Debug, Deserialize)]
pub struct SessionAffinityQuery {
    /// Request-model space filter (the space key, e.g. `"gpt-4o"`).
    #[serde(default)]
    pub model: Option<String>,
    /// Exact hashed-key filter.
    #[serde(default)]
    pub key: Option<String>,
    /// Page size (default 200, clamped to [`MAX_SESSION_ENTRIES`]).
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One session pin in the snapshot.
#[derive(Debug, Serialize)]
pub struct SessionEntryDto {
    /// Hashed session identity — safe to display and to paste back as the
    /// `DELETE { "key": ... }` filter. Never the raw session value.
    key: String,
    /// Request model the session is scoped to (the space key).
    model: String,
    /// Extraction-chain source (`meta` / `top` / `pc` / `fb`).
    source: String,
    provider: String,
    /// Resolved real model that last served the session.
    real_model: String,
    /// Config route `model_name` that recorded the pin (`"*"` for wildcard).
    route_model: String,
    /// Seconds since the pin was last hit (TTL sliding clock).
    age_secs: u64,
    /// Route `session_ttl_secs` (resolved; `0` = pins never expire).
    ttl_secs: u64,
    /// Seconds until the pin expires at the current TTL policy; `null`
    /// when `ttl_secs == 0` (never).
    expires_in_secs: Option<u64>,
}

/// `GET /api/session-affinity` — `{ "entries": [SessionEntryDto, ...] }`,
/// least-recently-hit last, filterable and page-limited.
pub async fn list_session_affinity(
    State(state): State<AppState>,
    Query(query): Query<SessionAffinityQuery>,
) -> Json<serde_json::Value> {
    // One config read for TTL resolution, one memory read for the snapshot
    // (cloned under a short lock; DTOs computed outside it).
    let routes = { state.config.read().await.routes.clone() };
    let snapshot = state.session_affinity.read().await.snapshot();
    let now = Instant::now();

    let filter_model = query.model.filter(|s| !s.is_empty());
    let filter_key = query.key.filter(|s| !s.is_empty());
    let limit = query.limit.unwrap_or(200).clamp(1, MAX_SESSION_ENTRIES);

    let mut entries: Vec<SessionEntryDto> = snapshot
        .into_iter()
        .filter(|e| {
            filter_key.as_deref().is_none_or(|k| k == e.map_id)
                && filter_model.as_deref().is_none_or(|m| m == e.model)
        })
        .map(|e| {
            // TTL comes from the *current* route config (expiry policy is
            // config, not state — design §4.2).
            let ttl_secs = sag_config::resolve_model_route(&routes, &e.model)
                .and_then(|r| r.session_ttl_secs)
                .unwrap_or(sag_state::DEFAULT_SESSION_TTL_SECS);
            let age_secs = now.duration_since(e.pin.recorded_at).as_secs();
            SessionEntryDto {
                key: e.map_id,
                model: e.model,
                source: e.pin.source.clone(),
                provider: e.pin.provider.clone(),
                real_model: e.pin.real_model.clone(),
                route_model: e.pin.route_model.clone(),
                age_secs,
                ttl_secs,
                expires_in_secs: (ttl_secs > 0).then(|| ttl_secs.saturating_sub(age_secs)),
            }
        })
        .collect();
    entries.sort_by(|a, b| b.age_secs.cmp(&a.age_secs).then_with(|| a.key.cmp(&b.key)));
    entries.truncate(limit);

    Json(serde_json::json!({ "entries": entries }))
}

/// `DELETE /api/session-affinity` body — one of the three scopes.
///
/// # Scope precedence
///
/// The scopes are checked in a fixed order, first match wins: `all` →
/// `key` → `model`. Extra scope fields are therefore **ignored, not
/// rejected** (`{"all": true, "key": "…"}` clears everything — it does not
/// narrow the delete to one key), which keeps the body forward-compatible
/// with a client that always sends the whole scope set. `all: true` still
/// requires `confirm: true` even when a `key`/`model` is also present, so a
/// stray `key` can never bypass the confirmation gate. An empty string is
/// treated as "field absent".
#[derive(Debug, Deserialize)]
pub struct DeleteSessionAffinityRequest {
    /// Clear one session by hashed key (across every space).
    #[serde(default)]
    pub key: Option<String>,
    /// Clear every session of one request-model space.
    #[serde(default)]
    pub model: Option<String>,
    /// Clear the whole memory. Requires `confirm: true` — it drops the
    /// stickiness of every live session at once.
    #[serde(default)]
    pub all: Option<bool>,
    /// Mandatory `true` for `all: true` (design decision O7).
    #[serde(default)]
    pub confirm: Option<bool>,
}

/// `DELETE /api/session-affinity`
///
/// Clears session pins and invalidates every in-flight write-back captured
/// before the clear (per-key generation / global epoch bumps — see
/// [`sag_state::SessionAffinityInner`]), so a request already in flight can
/// never re-pin a deleted session. Idempotent: deleting an unknown key /
/// model is a no-op and still returns `200`.
///
/// Scope precedence is `all` → `key` → `model`; see
/// [`DeleteSessionAffinityRequest`] for why the ignored scope fields are
/// tolerated instead of rejected.
pub async fn delete_session_affinity(
    State(state): State<AppState>,
    Json(req): Json<DeleteSessionAffinityRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let all = req.all.unwrap_or(false);
    let deleted = match (
        all,
        req.key.as_deref().filter(|s| !s.is_empty()),
        req.model.as_deref().filter(|s| !s.is_empty()),
    ) {
        (true, _, _) => {
            if req.confirm != Some(true) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": {
                            "code": "CONFIRMATION_REQUIRED",
                            "message": "deleting every session pin requires \"confirm\": true",
                        }
                    })),
                )
                    .into_response();
            }
            // Count and clear inside a single write-lock critical section:
            // a pin inserted between a separate read-lock count and the
            // write-lock clear would be deleted but not reported.
            let mut guard = state.session_affinity.write().await;
            let before = guard.snapshot().len();
            guard.clear_all();
            before
        }
        (false, Some(key), _) => {
            let mut guard = state.session_affinity.write().await;
            let existed = guard.snapshot().iter().any(|e| e.map_id == key);
            guard.clear(key);
            usize::from(existed)
        }
        (false, None, Some(model)) => {
            let mut guard = state.session_affinity.write().await;
            let existed = guard.snapshot().iter().filter(|e| e.model == model).count();
            guard.clear_model(model);
            existed
        }
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": {
                        "code": "BAD_REQUEST",
                        "message": "specify one of {\"key\"}, {\"model\"} or {\"all\": true}",
                    }
                })),
            )
                .into_response();
        }
    };

    Json(serde_json::json!({ "deleted": deleted })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{build_state, test_app};
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use sag_core::SessionKey;
    use std::collections::HashMap;
    use tower::ServiceExt;

    async fn seed(state: &AppState, model: &str, key: &str, provider: &str) -> String {
        let key = SessionKey::new("meta", key);
        let map_id = key.map_id(model);
        let mut guard = state.session_affinity.write().await;
        let version = 1;
        let (_, token) = guard.get_with_token(
            model,
            &map_id,
            sag_state::DEFAULT_SESSION_TTL_SECS,
            version,
            Instant::now(),
        );
        let wb = sag_state::SessionWriteback {
            map_id: map_id.clone(),
            model: model.into(),
            token,
            capacity: sag_state::DEFAULT_SESSION_MAX_ENTRIES,
        };
        assert!(
            guard.insert_if_generation(
                &wb,
                sag_state::SessionPin::new(provider, model, model, None, Instant::now(), version)
                    .with_source("meta"),
                version,
            )
        );
        map_id
    }

    async fn get(app: &axum::Router, uri: &str) -> serde_json::Value {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn delete(
        app: &axum::Router,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/session-affinity")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn snapshot_shows_hashed_keys_never_raw_values() {
        let state = build_state(HashMap::new(), vec![]).await;
        let map_id = seed(&state, "gpt-4o", "conv-raw-secret", "first").await;
        let app = test_app(state);

        let body = get(&app, "/api/session-affinity").await;
        let entries = body["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e["key"], map_id.as_str());
        assert_eq!(e["model"], "gpt-4o");
        assert_eq!(e["source"], "meta");
        assert_eq!(e["provider"], "first");
        assert_eq!(e["ttl_secs"], 3600);
        assert!(e["age_secs"].is_number());
        // The raw session value must never appear anywhere in the response.
        let s = body.to_string();
        assert!(!s.contains("conv-raw-secret"), "PII leaked: {s}");
    }

    #[tokio::test]
    async fn filters_by_model_and_key() {
        let state = build_state(HashMap::new(), vec![]).await;
        let k1 = seed(&state, "gpt-4o", "c1", "first").await;
        let k2 = seed(&state, "claude", "c2", "second").await;
        let app = test_app(state);

        let body = get(&app, "/api/session-affinity?model=claude").await;
        let entries = body["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["key"], k2.as_str());

        let body = get(&app, &format!("/api/session-affinity?key={k1}")).await;
        let entries = body["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["key"], k1.as_str());
    }

    #[tokio::test]
    async fn delete_by_key_and_model() {
        let state = build_state(HashMap::new(), vec![]).await;
        let k1 = seed(&state, "gpt-4o", "c1", "first").await;
        seed(&state, "gpt-4o", "c2", "first").await;
        seed(&state, "claude", "c3", "second").await;
        let app = test_app(state);

        let (status, body) = delete(&app, serde_json::json!({ "key": k1 })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["deleted"], 1);
        let body = get(&app, "/api/session-affinity").await;
        assert_eq!(body["entries"].as_array().unwrap().len(), 2);

        let (status, body) = delete(&app, serde_json::json!({ "model": "gpt-4o" })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["deleted"], 1);
        let body = get(&app, "/api/session-affinity").await;
        assert_eq!(body["entries"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn delete_all_requires_confirmation() {
        let state = build_state(HashMap::new(), vec![]).await;
        seed(&state, "gpt-4o", "c1", "first").await;
        seed(&state, "claude", "c2", "second").await;
        let app = test_app(state);

        let (status, body) = delete(&app, serde_json::json!({ "all": true })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "CONFIRMATION_REQUIRED");
        // Nothing was deleted.
        let body = get(&app, "/api/session-affinity").await;
        assert_eq!(body["entries"].as_array().unwrap().len(), 2);

        let (status, body) =
            delete(&app, serde_json::json!({ "all": true, "confirm": true })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["deleted"], 2);
        let body = get(&app, "/api/session-affinity").await;
        assert_eq!(body["entries"].as_array().unwrap().len(), 0);
    }

    /// `deleted` counts every pin across every space, and the count is read
    /// inside the same write-lock critical section as the clear itself.
    #[tokio::test]
    async fn delete_all_counts_pins_across_spaces() {
        let state = build_state(HashMap::new(), vec![]).await;
        seed(&state, "gpt-4o", "c1", "first").await;
        seed(&state, "gpt-4o", "c2", "first").await;
        seed(&state, "claude", "c3", "second").await;
        seed(&state, "claude", "c4", "second").await;
        seed(&state, "gemini", "c5", "third").await;
        let app = test_app(state);

        let (status, body) =
            delete(&app, serde_json::json!({ "all": true, "confirm": true })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["deleted"], 5);
        let body = get(&app, "/api/session-affinity").await;
        assert_eq!(body["entries"].as_array().unwrap().len(), 0);

        // Idempotent: a second clear reports zero.
        let (status, body) =
            delete(&app, serde_json::json!({ "all": true, "confirm": true })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["deleted"], 0);
    }

    #[tokio::test]
    async fn delete_without_scope_is_rejected() {
        let state = build_state(HashMap::new(), vec![]).await;
        let app = test_app(state);
        let (status, body) = delete(&app, serde_json::json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "BAD_REQUEST");
    }
}
