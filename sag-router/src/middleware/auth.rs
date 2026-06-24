use axum::{
    Json,
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde_json::json;
use tracing::warn;

use crate::state::AppState;

/// Axum middleware: validates the `Authorization: Bearer *** header
/// against the configured gateway API keys.
pub async fn gateway_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let trace_id = request
        .extensions()
        .get::<uuid::Uuid>()
        .map(|id| id.to_string())
        .unwrap_or_default();

    let auth_header = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string());

    // Auth check: take the read lock only for the duration of the lookup,
    // then release it. Holding the lock across `next.run` would deadlock
    // any handler that needs to acquire a write lock (e.g. the admin
    // `POST/DELETE` endpoints that mutate `config.providers`).
    let key_valid: bool = {
        let config_guard = state.config.read().await;
        match &config_guard.gateway_auth {
            Some(auth_config) => {
                if auth_config.api_keys.is_empty() {
                    // Empty api_keys list means "no auth" — same as removing
                    // the [gateway_auth] section entirely.
                    true
                } else {
                    let provided_key = match auth_header {
                        Some(key) => key,
                        None => {
                            warn!(trace_id, "request missing Authorization header");
                            return (StatusCode::UNAUTHORIZED, Json(json!({
                                "error": { "code": "UNAUTHORIZED", "message": "missing Authorization header" }
                            })))
                            .into_response();
                        }
                    };
                    auth_config.api_keys.iter().any(|k| k == &provided_key)
                }
            }
            None => {
                // No gateway auth configured — allow all requests
                true
            }
        }
    };

    if !key_valid {
        warn!(trace_id, "invalid gateway API key");
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": { "code": "UNAUTHORIZED", "message": "invalid API key" }
            })),
        )
            .into_response();
    }

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode, header},
        middleware,
        routing::get,
    };
    use sag_config::ConfigHandle;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    /// Helper: build an AppState for the test with optional auth keys.
    async fn test_state(auth_keys: Option<Vec<String>>) -> AppState {
        let config = sag_config::GatewayConfig {
            server: sag_config::ServerConfig {
                host: "127.0.0.1".into(),
                port: 3000,
            },
            analytics: None,
            gateway_auth: auth_keys.map(|keys| sag_config::GatewayAuthConfig { api_keys: keys }),
            retries: 2,
            backoff: sag_config::BackoffConfig::default(),
            providers: HashMap::new(),
            logging: sag_config::LoggingConfig::default(),
            routes: vec![],
            shuffle_cache_ttl_secs: 7200,
        };
        let handle = ConfigHandle::from_config(config);
        let registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
        crate::testing::test_app_state(handle, Arc::new(RwLock::new(registry))).await
    }

    /// Build a small test app: a single `GET /test` route protected by `gateway_auth`.
    fn test_app(state: AppState) -> Router {
        async fn handler() -> &'static str {
            "ok"
        }
        Router::new()
            .route("/test", get(handler))
            .route_layer(middleware::from_fn_with_state(state.clone(), gateway_auth))
            .with_state(state)
    }

    #[tokio::test]
    async fn auth_missing_header_returns_401() {
        let state = test_state(Some(vec!["secret".into()])).await;
        let app = test_app(state);

        let request = Request::builder().uri("/test").body(Body::empty()).unwrap();

        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "UNAUTHORIZED");
    }

    #[tokio::test]
    async fn auth_invalid_key_returns_401() {
        let state = test_state(Some(vec!["secret".into()])).await;
        let app = test_app(state);

        let request = Request::builder()
            .uri("/test")
            .header(header::AUTHORIZATION, "Bearer wrong-key")
            .body(Body::empty())
            .unwrap();

        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "UNAUTHORIZED");
    }

    #[tokio::test]
    async fn auth_valid_key_proceeds() {
        let state = test_state(Some(vec!["secret".into(), "other".into()])).await;
        let app = test_app(state);

        let request = Request::builder()
            .uri("/test")
            .header(header::AUTHORIZATION, "Bearer secret")
            .body(Body::empty())
            .unwrap();

        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(&body[..], b"ok");
    }

    #[tokio::test]
    async fn auth_second_key_proceeds() {
        let state = test_state(Some(vec!["key-a".into(), "key-b".into()])).await;
        let app = test_app(state);

        let request = Request::builder()
            .uri("/test")
            .header(header::AUTHORIZATION, "Bearer key-b")
            .body(Body::empty())
            .unwrap();

        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_no_config_allows_all() {
        // No gateway_auth section — all requests should pass through
        let state = test_state(None).await;
        let app = test_app(state);

        let request = Request::builder().uri("/test").body(Body::empty()).unwrap();

        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_bearer_with_extra_whitespace() {
        let state = test_state(Some(vec!["my-key".into()])).await;
        let app = test_app(state);

        // "Bearer   my-key  " — extra spaces should be trimmed
        let request = Request::builder()
            .uri("/test")
            .header(header::AUTHORIZATION, "Bearer   my-key  ")
            .body(Body::empty())
            .unwrap();

        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_empty_key_list_allows_all() {
        let state = test_state(Some(vec![])).await;
        let app = test_app(state);

        let request = Request::builder().uri("/test").body(Body::empty()).unwrap();

        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_empty_bearer_returns_401() {
        let state = test_state(Some(vec!["key".into()])).await;
        let app = test_app(state);

        let request = Request::builder()
            .uri("/test")
            .header(header::AUTHORIZATION, "Bearer ")
            .body(Body::empty())
            .unwrap();

        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
