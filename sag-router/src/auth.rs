use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use sag_config::ConfigHandle;
use tracing::warn;

/// Axum middleware: validates the `Authorization: Bearer <api_key>` header
/// against the configured gateway API keys.
pub async fn gateway_auth(
    State(config): State<ConfigHandle>,
    request: Request,
    next: Next,
) -> Response {
    let auth_header = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string());

    let config_guard = config.read().await;

    match &config_guard.gateway_auth {
        Some(auth_config) => {
            let provided_key = match auth_header {
                Some(key) => key,
                None => {
                    warn!("request missing Authorization header");
                    return (StatusCode::UNAUTHORIZED, Json(json!({
                        "error": { "code": "UNAUTHORIZED", "message": "missing Authorization header" }
                    })))
                    .into_response();
                }
            };

            if !auth_config.api_keys.iter().any(|k| k == &provided_key) {
                warn!("invalid gateway API key");
                return (StatusCode::UNAUTHORIZED, Json(json!({
                    "error": { "code": "UNAUTHORIZED", "message": "invalid API key" }
                })))
                .into_response();
            }
        }
        None => {
            // No gateway auth configured — allow all requests
        }
    }

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{header, Request, StatusCode},
        middleware,
        routing::get,
        Router,
    };
    use sag_config::ConfigHandle;
    use tower::ServiceExt;

    /// Helper: build a minimal GatewayConfig for the test, wrap in ConfigHandle.
    fn test_config_handle(
        auth_keys: Option<Vec<String>>,
    ) -> ConfigHandle {
        let config = sag_config::GatewayConfig {
            server: sag_config::ServerConfig {
                host: "127.0.0.1".into(),
                port: 3000,
            },
            analytics: None,
            gateway_auth: auth_keys.map(|keys| sag_config::GatewayAuthConfig {
                api_keys: keys,
            }),
            providers: std::collections::HashMap::new(),
            routes: vec![],
        };
        ConfigHandle::from_config(config)
    }

    /// Build a small test app: a single `GET /test` route protected by `gateway_auth`.
    /// Returns a Router ready for `axum_test_helper`-style test requests.
    fn test_app(config: ConfigHandle) -> Router {
        async fn handler() -> &'static str {
            "ok"
        }
        Router::new()
            .route("/test", get(handler))
            .route_layer(middleware::from_fn_with_state(config.clone(), gateway_auth))
            .with_state(config)
    }

    #[tokio::test]
    async fn auth_missing_header_returns_401() {
        let config = test_config_handle(Some(vec!["secret".into()]));
        let app = test_app(config);

        let request = Request::builder()
            .uri("/test")
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
    async fn auth_invalid_key_returns_401() {
        let config = test_config_handle(Some(vec!["secret".into()]));
        let app = test_app(config);

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
        let config = test_config_handle(Some(vec!["secret".into(), "other".into()]));
        let app = test_app(config);

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
        let config = test_config_handle(Some(vec!["key-a".into(), "key-b".into()]));
        let app = test_app(config);

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
        let config = test_config_handle(None);
        let app = test_app(config);

        let request = Request::builder()
            .uri("/test")
            .body(Body::empty())
            .unwrap();

        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_bearer_with_extra_whitespace() {
        let config = test_config_handle(Some(vec!["my-key".into()]));
        let app = test_app(config);

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
    async fn auth_empty_bearer_returns_401() {
        let config = test_config_handle(Some(vec!["key".into()]));
        let app = test_app(config);

        let request = Request::builder()
            .uri("/test")
            .header(header::AUTHORIZATION, "Bearer ")
            .body(Body::empty())
            .unwrap();

        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
