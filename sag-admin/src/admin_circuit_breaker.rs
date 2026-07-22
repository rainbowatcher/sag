//! Circuit breaker admin endpoints.
//!
//! - `GET /api/circuit-breaker` — read-only snapshot of all non-`Closed`
//!   route targets.
//! - `POST /api/circuit-breaker/close` — force-close a single target,
//!   removing its circuit entry so the next request hits the upstream
//!   directly.
//!
//! Both are mounted under `/api` by [`crate::admin::build_admin_routes`],
//! so they are auth-protected by the `gateway_auth` middleware.

use axum::Json;
use axum::extract::State;
use sag_core::RouteTargetKey;
use sag_state::AppState;
use serde::Deserialize;

/// `GET /api/circuit-breaker`
///
/// Returns `{ "breakers": [CircuitBreakerEntry, ...] }`. The `breakers` array
/// is empty when all targets are healthy (no open or half-open circuits).
///
/// The snapshot is a point-in-time read; timestamps are UNIX seconds and the
/// frontend is expected to refresh manually (no polling).
pub async fn list_circuit_breakers(State(state): State<AppState>) -> Json<serde_json::Value> {
    let guard = state.circuit_breaker.read().await;
    // `snapshot()` returns `Vec<CircuitBreakerEntry>`; the `json!` macro
    // serializes it via serde, so no explicit type import is needed here.
    Json(serde_json::json!({ "breakers": guard.snapshot() }))
}

/// Request body for `POST /api/circuit-breaker/close`.
#[derive(Debug, Deserialize)]
pub struct CloseCircuitBreakerRequest {
    pub model: String,
    pub provider: String,
    pub real_model: String,
}

/// `POST /api/circuit-breaker/close`
///
/// Force-close the circuit breaker for a single route target, removing its
/// entry so the target immediately becomes `Closed` (healthy). The next
/// request will hit the upstream directly.
///
/// Also clears the [`StickyFailover`](sag_config::Strategy::StickyFailover)
/// active-target memory for `req.model`, so the next request falls back to
/// the route's priority-ordered target list instead of staying pinned to the
/// failover target. This mirrors the behaviour of
/// `sticky_failover_reset_secs` expiring — a manual close is an explicit
/// "this target is healthy again, let routing re-evaluate" signal, so the
/// sticky memory that pinned traffic to a failover target must be reset.
///
/// **Idempotent**: if the target is already `Closed` (no entry), the
/// operation is a no-op and still returns `200 OK`. Clearing a sticky entry
/// that does not exist is also a no-op, so POSTing `close` against a healthy
/// target with no open breaker is a safe way for an API user to reset sticky
/// routing without any breaker state to act on.
///
/// **Concurrency**: if a half-open probe is in flight when this is called,
/// the probe result will be silently discarded (the entry no longer exists
/// when `record_probe_result` runs). If the upstream is still down, the
/// next failed request will re-open the breaker with a fresh cooldown.
pub async fn close_circuit_breaker(
    State(state): State<AppState>,
    Json(req): Json<CloseCircuitBreakerRequest>,
) -> Json<serde_json::Value> {
    let key = RouteTargetKey::new(&req.model, &req.provider, &req.real_model);
    {
        let mut guard = state.circuit_breaker.write().await;
        guard.close(&key);
    }
    // Reset sticky-failover memory for this model so the recovered target
    // competes on priority again instead of being pinned behind the
    // failover target that absorbed traffic while the breaker was open.
    state.active_targets.write().await.remove(&req.model);
    Json(serde_json::json!({ "ok": true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use sag_core::RouteTargetKey;
    use sag_state::testing::{build_provider_registry, test_app_state};
    use tower::ServiceExt;

    /// Build a test app with admin routes mounted (no auth middleware).
    fn test_app(state: AppState) -> axum::Router {
        axum::Router::new()
            .nest("/api", crate::admin::build_admin_routes(state.clone()))
            .with_state(state)
    }

    /// Helper: send GET /api/circuit-breaker and return (status, parsed JSON).
    async fn fetch_breakers(app: axum::Router) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/circuit-breaker")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        (status, json)
    }

    /// Build an AppState with an empty config (no providers/routes needed).
    async fn empty_state() -> AppState {
        let config = sag_config::GatewayConfig {
            ..sag_config::gateway_config_base!()
        };
        let handle = sag_config::ConfigHandle::from_config(config);
        let registry = build_provider_registry(&handle, true).await;
        test_app_state(handle, registry).await
    }

    #[tokio::test]
    async fn empty_circuit_breaker_returns_empty_array() {
        let state = empty_state().await;
        let app = test_app(state);
        let (status, json) = fetch_breakers(app).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["breakers"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn open_breaker_appears_in_snapshot() {
        let state = empty_state().await;
        // Manually open a circuit breaker entry.
        {
            let mut guard = state.circuit_breaker.write().await;
            guard.open(RouteTargetKey::new("gpt-4o", "openai", "gpt-4o"), 3600, 60);
        }
        let app = test_app(state);
        let (status, json) = fetch_breakers(app).await;
        assert_eq!(status, StatusCode::OK);
        let breakers = json["breakers"].as_array().unwrap();
        assert_eq!(breakers.len(), 1);
        let entry = &breakers[0];
        assert_eq!(entry["model"], "gpt-4o");
        assert_eq!(entry["provider"], "openai");
        assert_eq!(entry["real_model"], "gpt-4o");
        assert_eq!(entry["state"], "open");
        assert!(entry["next_probe_at_secs"].is_number());
        assert_eq!(entry["probe_in_flight"], false);
    }

    #[tokio::test]
    async fn half_open_breaker_has_null_next_probe() {
        let state = empty_state().await;
        {
            let mut guard = state.circuit_breaker.write().await;
            // interval=0 → first probe due immediately → can transition to HalfOpen
            guard.open(
                RouteTargetKey::new("claude", "anthropic", "claude-3"),
                3600,
                0,
            );
            guard.to_half_open_if_due(&RouteTargetKey::new("claude", "anthropic", "claude-3"));
        }
        let app = test_app(state);
        let (status, json) = fetch_breakers(app).await;
        assert_eq!(status, StatusCode::OK);
        let breakers = json["breakers"].as_array().unwrap();
        assert_eq!(breakers.len(), 1);
        let entry = &breakers[0];
        assert_eq!(entry["state"], "half_open");
        assert!(entry["next_probe_at_secs"].is_null());
    }

    #[tokio::test]
    async fn open_breaker_returns_valid_timestamps() {
        // Verify the endpoint returns 200 with valid numeric timestamps
        // for a normally-opened entry. (The expired-entry panic path is
        // covered by the sag-core unit test `snapshot_expired_entry_does_not_panic`,
        // which can directly access the private `entries` map.)
        let state = empty_state().await;
        {
            let mut guard = state.circuit_breaker.write().await;
            guard.open(RouteTargetKey::new("gpt-4o", "openai", "gpt-4o"), 3600, 60);
        }
        let app = test_app(state);
        let (status, json) = fetch_breakers(app).await;
        assert_eq!(status, StatusCode::OK);
        let breakers = json["breakers"].as_array().unwrap();
        assert_eq!(breakers.len(), 1);
        assert!(breakers[0]["cooldown_until_secs"].is_number());
        assert!(breakers[0]["next_probe_at_secs"].is_number());
    }

    // ── Force Close tests ────────────────────────────────────────────

    /// Helper: send POST /api/circuit-breaker/close with a body and return
    /// (status, parsed JSON).
    async fn close_breaker(
        app: axum::Router,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/circuit-breaker/close")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        (status, json)
    }

    #[tokio::test]
    async fn force_close_removes_open_breaker() {
        let state = empty_state().await;
        {
            let mut guard = state.circuit_breaker.write().await;
            guard.open(RouteTargetKey::new("gpt-4o", "openai", "gpt-4o"), 3600, 60);
        }
        let app = test_app(state.clone());
        let (status, json) = close_breaker(
            app,
            serde_json::json!({
                "model": "gpt-4o",
                "provider": "openai",
                "real_model": "gpt-4o"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["ok"], true);

        // Verify the breaker is gone from the snapshot.
        let app2 = test_app(state);
        let (_, json2) = fetch_breakers(app2).await;
        assert_eq!(json2["breakers"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn force_close_is_idempotent() {
        // Closing a target that has no entry (already healthy) should
        // return 200 OK, not 404.
        let state = empty_state().await;
        let app = test_app(state);
        let (status, json) = close_breaker(
            app,
            serde_json::json!({
                "model": "gpt-4o",
                "provider": "openai",
                "real_model": "gpt-4o"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["ok"], true);
    }

    #[tokio::test]
    async fn force_close_only_affects_specified_target() {
        let state = empty_state().await;
        {
            let mut guard = state.circuit_breaker.write().await;
            guard.open(RouteTargetKey::new("gpt-4o", "openai", "gpt-4o"), 3600, 60);
            guard.open(
                RouteTargetKey::new("claude", "anthropic", "claude-3"),
                3600,
                60,
            );
        }
        let app = test_app(state.clone());
        let (status, _) = close_breaker(
            app,
            serde_json::json!({
                "model": "gpt-4o",
                "provider": "openai",
                "real_model": "gpt-4o"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // Only the anthropic breaker should remain.
        let app2 = test_app(state);
        let (_, json2) = fetch_breakers(app2).await;
        let breakers = json2["breakers"].as_array().unwrap();
        assert_eq!(breakers.len(), 1);
        assert_eq!(breakers[0]["provider"], "anthropic");
    }

    #[tokio::test]
    async fn force_close_clears_sticky_failover_active_target() {
        // Regression: under StickyFailover, when the high-priority target's
        // breaker opens, traffic fails over to the backup and the backup is
        // recorded in `active_targets[model]`. Manually closing the original
        // target's breaker must clear that sticky memory so the next request
        // re-evaluates priority ordering instead of staying pinned to the
        // backup. Without this, the recovered target is never tried even
        // though its breaker is closed, because the sticky memory keeps the
        // backup prepended to the front of the ordered target list.
        let state = empty_state().await;
        // Simulate: openai's breaker opened, traffic moved to anthropic, and
        // anthropic was recorded as the sticky active target for "gpt-4o".
        {
            let mut guard = state.circuit_breaker.write().await;
            guard.open(RouteTargetKey::new("gpt-4o", "openai", "gpt-4o"), 3600, 60);
        }
        state.active_targets.write().await.insert(
            "gpt-4o".to_string(),
            (
                "anthropic".to_string(),
                "claude-3".to_string(),
                std::time::Instant::now(),
            ),
        );

        let app = test_app(state.clone());
        let (status, json) = close_breaker(
            app,
            serde_json::json!({
                "model": "gpt-4o",
                "provider": "openai",
                "real_model": "gpt-4o"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["ok"], true);

        // The sticky memory for this model must be cleared so the recovered
        // high-priority target competes on priority again.
        assert!(
            state.active_targets.read().await.get("gpt-4o").is_none(),
            "force-close must clear the sticky-failover active target for the model"
        );
    }

    #[tokio::test]
    async fn force_close_on_already_closed_clears_sticky_anyway() {
        // Closing a target with no open breaker is idempotent for the breaker
        // (no-op, 200 OK). It must still clear the sticky memory for the model
        // — this is the API-level "reset sticky routing" path: an operator
        // POSTs close against any target on the route to reset sticky
        // pinning without needing a dedicated endpoint.
        let state = empty_state().await;
        state.active_targets.write().await.insert(
            "gpt-4o".to_string(),
            (
                "anthropic".to_string(),
                "claude-3".to_string(),
                std::time::Instant::now(),
            ),
        );

        let app = test_app(state.clone());
        let (status, json) = close_breaker(
            app,
            serde_json::json!({
                "model": "gpt-4o",
                "provider": "openai",
                "real_model": "gpt-4o"
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["ok"], true);
        assert!(
            state.active_targets.read().await.get("gpt-4o").is_none(),
            "close against a healthy target must still reset sticky memory (idempotent reset path)"
        );
    }
}
