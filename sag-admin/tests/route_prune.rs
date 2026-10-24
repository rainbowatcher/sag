//! Integration tests for the routing-memory prune that follows a committed
//! admin route mutation.
//!
//! `prune_stale_routing_state` must keep every shared routing cache in step
//! with the new config *on the admin protocol surface* — not only on the
//! dispatch path (which re-prunes lazily on the next request when
//! `routes_version` changed). This file covers the two request-model-keyed
//! halves — session-affinity pins (deleting a `session_sticky` route must
//! drop the pins recorded under it, and must leave the pins of every other
//! live session-sticky route alone) and round-robin counters (same rule for
//! `route_counters`).
//!
//! These tests cross crate boundaries (admin API → coordinator → shared
//! `AppState` memory) and drive the full request lifecycle, so they live in
//! the integration-test directory per the project's testing conventions
//! (AGENTS.md).

use std::collections::HashMap;
use std::time::Instant;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use sag_config::{
    ApiConfig, ApiKind, ConfigHandle, GatewayConfig, KeyStrategy, ProviderConfig, ProxyMode,
    RouteConfig, RouteTarget, Strategy,
};
use sag_state::{AppState, SessionPin, SessionWriteback, StickyToken};
use tower::ServiceExt;

/// Routes version used when seeding pins. Every pin and token in these
/// tests is recorded at the same version, so the version gate in
/// `insert_if_generation` never rejects the seed.
const SEED_VERSION: u64 = 0;

fn provider_config() -> ProviderConfig {
    let mut apis = indexmap::IndexMap::new();
    apis.insert(
        ApiKind::ChatCompletions,
        ApiConfig {
            base_url: "https://api.openai.com/v1".into(),
            ..ApiConfig::default()
        },
    );
    ProviderConfig {
        api_keys: vec!["sk-test".into()],
        key_strategy: KeyStrategy::FillFirst,
        proxy_mode: ProxyMode::default(),
        timeout_secs: 120,
        apis,
        ..ProviderConfig::default()
    }
}

fn session_sticky_route(model_name: &str) -> RouteConfig {
    RouteConfig {
        model_name: model_name.into(),
        pipeline: sag_config::Pipeline::Chat,
        api_endpoint: None,
        strategy: Strategy::SessionSticky,
        retries: None,
        timeout_secs: None,
        session_ttl_secs: None,
        session_max_entries: None,
        targets: vec![RouteTarget {
            provider: "openai".into(),
            real_model: None,
            retries: None,
            timeout_secs: None,
            priority: 1,
            extra: HashMap::new(),
            capabilities: None,
        }],
    }
}

/// Same route shape as [`session_sticky_route`] but on `round_robin`, the
/// only strategy that maintains a `route_counters` entry.
fn round_robin_route(model_name: &str) -> RouteConfig {
    RouteConfig {
        strategy: Strategy::RoundRobin,
        ..session_sticky_route(model_name)
    }
}

/// In-memory `AppState` + admin router (no auth middleware, like the
/// gateway's own unit-test wiring).
async fn test_state_and_app(routes: Vec<RouteConfig>) -> (AppState, Router) {
    let mut providers = HashMap::new();
    providers.insert("openai".to_string(), provider_config());
    let config = GatewayConfig {
        providers,
        routes,
        ..sag_config::gateway_config_base!()
    };
    let handle = ConfigHandle::from_config(config);
    let registry = sag_state::testing::build_provider_registry(&handle, false).await;
    let state = sag_state::testing::test_app_state(handle, registry).await;
    let app = Router::new()
        .nest("/api", sag_admin::build_admin_routes(state.clone()))
        .with_state(state.clone());
    (state, app)
}

/// Seed one session pin for `(request_model, map_id)` owned by
/// `route_model` through the real write-back API.
async fn seed_pin(state: &AppState, request_model: &str, map_id: &str, route_model: &str) {
    let mut guard = state.session_affinity.write().await;
    let (_, token) =
        guard.get_with_token(request_model, map_id, 3600, SEED_VERSION, Instant::now());
    let wb = SessionWriteback {
        map_id: map_id.into(),
        model: request_model.into(),
        token: StickyToken::new(token.epoch, token.generation, SEED_VERSION),
        capacity: 100,
    };
    let pin = SessionPin::new(
        "openai",
        request_model,
        route_model,
        None,
        Instant::now(),
        SEED_VERSION,
    );
    assert!(
        guard.insert_if_generation(&wb, pin, SEED_VERSION),
        "seed write-back must be accepted"
    );
}

async fn pinned_models(state: &AppState) -> Vec<String> {
    let guard = state.session_affinity.read().await;
    let mut models: Vec<String> = guard.snapshot().into_iter().map(|e| e.model).collect();
    models.sort();
    models
}

#[tokio::test]
async fn delete_route_prunes_session_affinity_pins() {
    let (state, app) = test_state_and_app(vec![
        session_sticky_route("gpt-4o"),
        session_sticky_route("gpt-4o-mini"),
    ])
    .await;

    seed_pin(&state, "gpt-4o", "hash-deleted", "gpt-4o").await;
    seed_pin(&state, "gpt-4o-mini", "hash-kept", "gpt-4o-mini").await;
    assert_eq!(
        pinned_models(&state).await,
        vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()]
    );

    let req = Request::builder()
        .method("DELETE")
        .uri("/api/routes")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"model_name":"gpt-4o"}"#))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The deleted route's pins are gone; the surviving session_sticky
    // route's pin is untouched (no over-pruning).
    assert_eq!(pinned_models(&state).await, vec!["gpt-4o-mini".to_string()]);
}

#[tokio::test]
async fn strategy_switch_prunes_session_affinity_pins() {
    let (state, app) = test_state_and_app(vec![session_sticky_route("gpt-4o")]).await;

    seed_pin(&state, "gpt-4o", "hash-switched", "gpt-4o").await;
    assert_eq!(pinned_models(&state).await, vec!["gpt-4o".to_string()]);

    // Switching the route away from `session_sticky` leaves pins the new
    // config would never consult.
    let req = Request::builder()
        .method("PUT")
        .uri("/api/routes")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"model_name":"gpt-4o","strategy":"fill_first"}"#,
        ))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    assert!(
        pinned_models(&state).await.is_empty(),
        "pins of a route that switched away from session_sticky must be dropped"
    );
}

#[tokio::test]
async fn delete_route_prunes_round_robin_counters() {
    let (state, app) = test_state_and_app(vec![
        round_robin_route("gpt-4o"),
        round_robin_route("gpt-4o-mini"),
    ])
    .await;

    // Seed the counters the dispatch side would have produced for both
    // routes' request models.
    {
        let mut counters = state.route_counters.write().await;
        counters.insert("gpt-4o".into(), 3);
        counters.insert("gpt-4o-mini".into(), 1);
    }

    let req = Request::builder()
        .method("DELETE")
        .uri("/api/routes")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"model_name":"gpt-4o"}"#))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // The deleted route's counter is gone; the surviving round_robin
    // route's counter is untouched (no over-pruning).
    let counters = state.route_counters.read().await;
    assert!(
        !counters.contains_key("gpt-4o"),
        "a deleted route's round-robin counter must be pruned, got {counters:?}"
    );
    assert_eq!(
        counters.get("gpt-4o-mini"),
        Some(&1),
        "a live route's round-robin counter must survive the prune"
    );
}

#[tokio::test]
async fn create_image_route_with_session_sticky_is_rejected() {
    // `session_sticky` on an image route would silently degrade to
    // weighted-random ordering (image dispatch performs no session
    // write-back), so the admin create path must surface the config
    // validation error as a 400 instead of committing the route.
    let (state, app) = test_state_and_app(vec![session_sticky_route("gpt-4o")]).await;

    let req = Request::builder()
        .method("POST")
        .uri("/api/routes")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"model_name":"dall-e-3","pipeline":"image","api_endpoint":"/images/generations","strategy":"session_sticky","targets":[{"provider":"openai","priority":1}]}"#,
        ))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert!(
        body.contains("is not supported when pipeline = \\\"image\\\""),
        "error must explain the rejection, got: {body}"
    );

    // Nothing was committed.
    assert!(
        !state
            .config
            .read()
            .await
            .routes
            .iter()
            .any(|r| r.model_name == "dall-e-3"),
        "rejected route must not be persisted"
    );
}
