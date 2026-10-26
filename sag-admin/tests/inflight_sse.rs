//! Integration tests for `GET /api/inflight` — the SSE stream of per-route
//! in-flight counters.
//!
//! The endpoint is part of the auth-protected admin surface, so these tests
//! mount [`build_admin_routes`] under `/api` behind the **real**
//! `gateway_auth` middleware (the same wiring `sag-router::build_router`
//! uses) instead of calling the handler directly. They pin:
//!
//! - the 401 envelope for a connection without `x-api-key`;
//! - the connect-time snapshot frame (`seq` / `total` / `routes`);
//! - that a manual `InFlightRegistry::acquire` (no dispatch involved) reaches
//!   the stream as a new snapshot, and that dropping the guard publishes the
//!   emptied snapshot;
//! - that the body is an infinite stream: frames are read one at a time, each
//!   read bounded by a timeout, and the body is dropped once enough frames
//!   arrived — dropping it must not disturb the counters, because the endpoint
//!   itself never counts.
//!
//! They cross crate boundaries (admin API → auth middleware → shared
//! `AppState` watch channel) and drive a full request lifecycle, so they live
//! in the integration-test directory per the project's testing conventions
//! (AGENTS.md).

use std::collections::HashMap;
use std::time::Duration;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    middleware,
};
use http_body_util::BodyExt;
use sag_config::{ConfigHandle, GatewayAuthConfig, GatewayConfig};
use sag_router::middleware::auth::gateway_auth;
use sag_state::AppState;
use tower::ServiceExt;

/// API key the fixture gateway requires.
const API_KEY: &str = "inflight-key";

/// Every frame read in these tests must arrive well inside this bound, so a
/// handler that forgets to emit the connect-time snapshot fails loudly
/// instead of hanging the suite.
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

/// In-memory `AppState` (empty provider registry — no upstream is contacted)
/// plus the admin router wrapped in the real `gateway_auth` middleware.
async fn test_state_and_app() -> (AppState, Router) {
    let config = GatewayConfig {
        gateway_auth: Some(GatewayAuthConfig {
            api_keys: vec![API_KEY.into()],
        }),
        ..sag_config::gateway_config_base!()
    };
    let handle = ConfigHandle::from_config(config);
    let registry: HashMap<String, std::sync::Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
    let state = sag_state::testing::test_app_state(
        handle,
        std::sync::Arc::new(tokio::sync::RwLock::new(registry)),
    )
    .await;
    let app = Router::new()
        .nest("/api", sag_admin::build_admin_routes(state.clone()))
        .route_layer(middleware::from_fn_with_state(state.clone(), gateway_auth))
        .with_state(state.clone());
    (state, app)
}

/// Open the SSE stream. Returns the response — the body is left to the caller
/// so it can be polled lazily.
async fn open_stream(app: Router) -> axum::response::Response {
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/inflight")
                // `/api/*` is Bearer-only: `gateway_auth` ignores `x-api-key`
                // outside `/v1/messages`, so the stream authenticates exactly
                // like every other admin route.
                .header("Authorization", format!("Bearer {API_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers();
    assert_eq!(
        headers.get("content-type").unwrap(),
        "text/event-stream",
        "the endpoint must answer with an SSE content type"
    );
    assert_eq!(
        headers.get("cache-control").unwrap(),
        "no-cache, no-transform",
        "an intermediate proxy must not buffer or transform the stream"
    );
    assert_eq!(headers.get("x-accel-buffering").unwrap(), "no");
    response
}

/// Read the next body frame, bounded by [`FRAME_TIMEOUT`].
async fn next_frame(body: &mut Body) -> String {
    let frame = tokio::time::timeout(FRAME_TIMEOUT, body.frame())
        .await
        .expect("a frame must arrive within the timeout")
        .expect("the stream must not end")
        .expect("the stream must not error");
    let data = frame.into_data().expect("every frame must be a data frame");
    String::from_utf8(data.to_vec()).unwrap()
}

/// Parse the JSON payload of one `event: inflight` frame.
fn snapshot_of(frame: &str) -> serde_json::Value {
    let mut data = None;
    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("event: ") {
            assert_eq!(rest, "inflight", "unexpected SSE event name in {frame:?}");
        } else if let Some(rest) = line.strip_prefix("data: ") {
            data = Some(rest.to_string());
        }
    }
    let data = data.unwrap_or_else(|| panic!("frame carries no data field: {frame:?}"));
    serde_json::from_str(&data).unwrap_or_else(|e| panic!("bad JSON payload {data:?}: {e}"))
}

#[tokio::test]
async fn inflight_requires_api_key() {
    let (_state, app) = test_state_and_app().await;

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/inflight")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "the SSE endpoint must sit behind the same auth as every other /api/* route"
    );
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"], "UNAUTHORIZED");
    assert_eq!(json["error"]["message"], "missing Authorization header");
}

#[tokio::test]
async fn inflight_rejects_x_api_key_like_other_api_routes() {
    // `x-api-key` is an Anthropic-ingress credential: it unlocks
    // `/v1/messages` only. The SSE endpoint must not widen that surface.
    let (_state, app) = test_state_and_app().await;

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/inflight")
                .header("x-api-key", API_KEY)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "x-api-key must not unlock /api/inflight"
    );
}

#[tokio::test]
async fn inflight_emits_connect_time_snapshot() {
    let (state, app) = test_state_and_app().await;
    // Seed a count *before* connecting so the first frame is provably the
    // current state rather than the registry's default.
    let guard = state.in_flight.acquire("gpt-4o");
    let mut body = open_stream(app).await.into_body();

    let snapshot = snapshot_of(&next_frame(&mut body).await);
    assert_eq!(snapshot["total"], 1);
    assert!(snapshot["seq"].as_u64().unwrap() >= 1);
    assert_eq!(snapshot["routes"][0]["model_name"], "gpt-4o");
    assert_eq!(snapshot["routes"][0]["count"], 1);

    drop(body);
    drop(guard);
}

#[tokio::test]
async fn inflight_streams_acquire_and_release_snapshots() {
    let (state, app) = test_state_and_app().await;
    let mut body = open_stream(app).await.into_body();

    // Empty registry: the connect-time frame says so.
    let first = snapshot_of(&next_frame(&mut body).await);
    assert_eq!(first["total"], 0);
    assert_eq!(first["routes"].as_array().unwrap().len(), 0);

    // A manual acquire — no dispatch involved — must publish a new snapshot.
    let guard = state.in_flight.acquire("gpt-4o");
    let busy = snapshot_of(&next_frame(&mut body).await);
    assert_eq!(busy["total"], 1);
    assert_eq!(busy["routes"][0]["model_name"], "gpt-4o");
    assert_eq!(busy["routes"][0]["count"], 1);
    assert!(
        busy["seq"].as_u64().unwrap() > first["seq"].as_u64().unwrap(),
        "seq must advance with each published snapshot"
    );

    // Releasing the guard publishes the emptied snapshot (zero-count routes
    // are dropped from the map, so `routes` is empty again).
    drop(guard);
    let idle = snapshot_of(&next_frame(&mut body).await);
    assert_eq!(idle["total"], 0);
    assert_eq!(idle["routes"].as_array().unwrap().len(), 0);
    assert!(idle["seq"].as_u64().unwrap() > busy["seq"].as_u64().unwrap());

    // Dropping the body of an infinite stream must not touch the counters:
    // the endpoint itself holds no in-flight slot.
    drop(body);
    let snapshot = state.in_flight.snapshot();
    assert_eq!(snapshot.total, 0);
    assert!(snapshot.routes.is_empty());
}

#[tokio::test]
async fn inflight_body_is_an_infinite_stream() {
    let (state, app) = test_state_and_app().await;
    let mut body = open_stream(app).await.into_body();

    // Three consecutive publications: the stream keeps going after each
    // frame (it is not a one-shot snapshot response).
    let mut seqs = Vec::new();
    seqs.push(
        snapshot_of(&next_frame(&mut body).await)["seq"]
            .as_u64()
            .unwrap(),
    );
    for name in ["gpt-4o", "gpt-4o-mini"] {
        let guard = state.in_flight.acquire(name);
        seqs.push(
            snapshot_of(&next_frame(&mut body).await)["seq"]
                .as_u64()
                .unwrap(),
        );
        drop(guard);
        seqs.push(
            snapshot_of(&next_frame(&mut body).await)["seq"]
                .as_u64()
                .unwrap(),
        );
    }
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "every frame must carry a strictly newer snapshot, got {seqs:?}"
    );

    drop(body);
    assert_eq!(state.in_flight.snapshot().total, 0);
}
