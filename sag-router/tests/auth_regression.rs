//! Auth-regression integration tests.
//!
//! Earlier versions of `sag-router/src/auth.rs::gateway_auth` held a read
//! lock on the config for the entire request lifecycle, which deadlocked
//! any handler that needed to acquire the write lock. These tests wrap
//! the admin routes with the real auth middleware and exercise every
//! write-locking handler with a hard timeout, so a regression would
//! surface as a clear test failure rather than a production hang.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
    middleware,
};
use sag_config::{
    ConfigHandle, GatewayAuthConfig, GatewayConfig, KeyStrategy, ProviderConfig, ProviderType,
    ProxyMode, RouteConfig, RouteTarget, Strategy,
};
use sag_router::AppState;
use sag_router::admin::build_admin_routes;
use sag_router::middleware::auth::gateway_auth;
use tokio::sync::RwLock;
use tower::ServiceExt;

async fn build_protected_state() -> AppState {
    let mut providers = HashMap::new();
    providers.insert(
        "openai".to_string(),
        ProviderConfig {
            provider_type: ProviderType::OpenAI,
            api_keys: vec!["sk-test".into()],
            key_strategy: KeyStrategy::FillFirst,
            base_url: "https://api.openai.com/v1".into(),
            proxy_mode: ProxyMode::default(),
            models_endpoint: "models".into(),
            timeout_secs: 120,
            headers: HashMap::new(),
            body_overrides: HashMap::new(),
            strip_fields: vec![],
            model_strip_fields: HashMap::new(),
            model_overrides: HashMap::new(),
            role_mappings: HashMap::new(),
            enabled_models: vec![],
            enabled: true,
            anthropic_cache_control: true,
            chunk_timeout_secs: None,
            ttfb_timeout_secs: None,
        },
    );
    let config = GatewayConfig {
        gateway_auth: Some(GatewayAuthConfig {
            api_keys: vec!["regression-key".into()],
        }),
        providers,
        routes: vec![RouteConfig {
            model_name: "gpt-4o".into(),
            api_endpoint: "/chat/completions".into(),
            retries: None,
            timeout_secs: None,
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                extra: HashMap::new(),
                retries: None,
                timeout_secs: None,
                provider: "openai".into(),
                real_model: Some("gpt-4o".into()),
                priority: 1,
            }],
        }],
        ..sag_config::gateway_config_base!()
    };
    let handle = ConfigHandle::from_config(config);
    let mut registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
    let cfg_read = handle.read().await;
    for (name, pc) in cfg_read.providers.iter() {
        let provider: Arc<dyn sag_providers::LlmProvider> =
            Arc::new(sag_providers::OpenAiProvider::new(name.clone(), pc).unwrap());
        registry.insert(name.clone(), provider);
    }
    drop(cfg_read);
    sag_router::test_app_state(handle, Arc::new(RwLock::new(registry))).await
}

fn protected_app(state: AppState) -> Router {
    // Wrap the real admin routes with the auth middleware so any
    // missing-route regression in `build_admin_routes` surfaces here.
    let admin = build_admin_routes(state.clone())
        .route_layer(middleware::from_fn_with_state(state.clone(), gateway_auth));
    Router::new().nest("/api", admin).with_state(state)
}

async fn bounded<F>(label: &str, fut: F) -> axum::response::Response
where
    F: std::future::Future<Output = axum::response::Response>,
{
    let start = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(3), fut).await;
    assert!(
        result.is_ok(),
        "[{label}] request did not complete in 3s — \
         this strongly suggests the auth middleware is still \
         holding a read lock across the handler (deadlock). \
         See sag-router/src/auth.rs."
    );
    let resp = result.unwrap();
    eprintln!("[{label}] completed in {:?}", start.elapsed());
    resp
}

#[tokio::test]
async fn auth_regression_delete_completes_through_auth_middleware() {
    let state = build_protected_state().await;
    let app = protected_app(state);
    let resp = bounded("DELETE", async {
        app.oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/providers")
                .header("Authorization", "Bearer regression-key")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"openai"}"#))
                .unwrap(),
        )
        .await
        .unwrap()
    })
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn auth_regression_add_provider_completes_through_auth_middleware() {
    let state = build_protected_state().await;
    let app = protected_app(state);
    let resp = bounded("POST", async {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/providers")
                .header("Authorization", "Bearer regression-key")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"anthropic","api_keys":["sk-ant"],"provider_type":"anthropic"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap()
    })
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn auth_regression_add_route_completes_through_auth_middleware() {
    let state = build_protected_state().await;
    let app = protected_app(state);
    let resp = bounded("POST route", async {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/routes")
                .header("Authorization", "Bearer regression-key")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model_name":"gpt-4o-mini","targets":[{"provider":"openai","real_model":"gpt-4o-mini"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap()
    })
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn auth_regression_list_providers_completes_through_auth_middleware() {
    let state = build_protected_state().await;
    let app = protected_app(state);
    let resp = bounded("GET", async {
        app.oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/providers")
                .header("Authorization", "Bearer regression-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    })
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn auth_regression_update_provider_completes_through_auth_middleware() {
    let state = build_protected_state().await;
    let app = protected_app(state);
    let resp = bounded("PUT", async {
        app.oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/providers")
                .header("Authorization", "Bearer regression-key")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"openai","base_url":"https://updated.com/v1"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap()
    })
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

// ── `/api/logs` auth wiring ───────────────────────────────────────

/// Helper: read the response body as JSON.
async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn auth_regression_logs_missing_header_returns_401_envelope() {
    let state = build_protected_state().await;
    let app = protected_app(state);

    let resp = bounded("GET /api/logs (no auth)", async {
        app.oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/logs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    })
    .await;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(
        body["error"]["code"], "UNAUTHORIZED",
        "/api/logs must reject unauthenticated requests with the connection-level 401 envelope"
    );
    assert_eq!(body["error"]["message"], "missing Authorization header");
}

#[tokio::test]
async fn auth_regression_logs_wrong_bearer_returns_401_envelope() {
    let state = build_protected_state().await;
    let app = protected_app(state);

    let resp = bounded("GET /api/logs (wrong key)", async {
        app.oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/logs")
                .header("Authorization", "Bearer not-the-real-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    })
    .await;

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let body = body_json(resp).await;
    assert_eq!(body["error"]["code"], "UNAUTHORIZED");
    assert_eq!(body["error"]["message"], "invalid API key");
}

#[tokio::test]
async fn auth_regression_logs_correct_bearer_proceeds() {
    let state = build_protected_state().await;
    let app = protected_app(state);

    let resp = bounded("GET /api/logs (correct key)", async {
        app.oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/logs")
                .header("Authorization", "Bearer regression-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    })
    .await;

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "valid Bearer must reach the handler"
    );
    let body = body_json(resp).await;
    assert!(
        body["data"].is_array(),
        "valid auth gets back the LogsPage's data envelope, not a 401"
    );
}
