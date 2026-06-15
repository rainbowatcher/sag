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
    routing::get,
};
use sag_analytics::CallLogCollector;
use sag_config::{
    ConfigHandle, GatewayAuthConfig, GatewayConfig, KeyStrategy, ProviderConfig, ProviderType,
    RouteConfig, RouteTarget, ServerConfig, Strategy,
};
use sag_router::AppState;
use sag_router::admin_providers::{
    add_provider, delete_provider, list_provider_models, list_providers, update_provider,
};
use sag_router::admin_routes::{create_route, delete_route, list_routes, update_route};
use sag_router::auth::gateway_auth;
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
            models_endpoint: "models".into(),
            timeout_secs: 120,
            headers: HashMap::new(),
            body_overrides: HashMap::new(),
            enabled_models: vec![],
            enabled: true,
        },
    );
    let config = GatewayConfig {
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 3000,
        },
        analytics: None,
        gateway_auth: Some(GatewayAuthConfig {
            api_keys: vec!["regression-key".into()],
        }),
        retries: 2,
        providers,
        routes: vec![RouteConfig {
            model_name: "gpt-4o".into(),
            api_endpoint: "/chat/completions".into(),
            retries: None,
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                extra: HashMap::new(),
                retries: None,
                provider: "openai".into(),
                real_model: Some("gpt-4o".into()),
                priority: 1,
            }],
        }],
    };
    let handle = ConfigHandle::from_config(config);
    let mut registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
    let cfg_read = handle.read().await;
    for (name, pc) in cfg_read.providers.iter() {
        let provider: Arc<dyn sag_providers::LlmProvider> =
            Arc::new(sag_providers::OpenAiProvider::new(name.clone(), pc));
        registry.insert(name.clone(), provider);
    }
    drop(cfg_read);
    AppState {
        config: handle,
        providers: Arc::new(RwLock::new(registry)),
        collector: Arc::new(CallLogCollector::new_memory().await.unwrap()),
        route_counters: Arc::new(RwLock::new(HashMap::new())),
    }
}

fn protected_app(state: AppState) -> Router {
    let admin = Router::new()
        .route(
            "/providers",
            get(list_providers)
                .post(add_provider)
                .put(update_provider)
                .delete(delete_provider),
        )
        .route("/providers/{name}/models", get(list_provider_models))
        .route(
            "/routes",
            get(list_routes)
                .post(create_route)
                .put(update_route)
                .delete(delete_route),
        )
        .with_state(state.clone())
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
