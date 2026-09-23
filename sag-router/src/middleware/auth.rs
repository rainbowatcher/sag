use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
};
use sag_core::AppError;
use tracing::warn;

use sag_state::AppState;

/// Whether this request path accepts Anthropic SDK `x-api-key` auth (D9).
///
/// **Only** `/v1/messages` accepts `x-api-key` (equivalent to
/// `gateway_auth.api_keys`). Every other path ignores `x-api-key` and
/// requires `Authorization: Bearer` — an `x-api-key` must never unlock
/// `/v1/chat/completions` or `/api/*`.
fn accepts_x_api_key(path: &str) -> bool {
    path == "/v1/messages" || path == "/v1/messages/"
}

/// Axum middleware: validates the gateway API key against
/// `gateway_auth.api_keys`.
///
/// - Bearer is accepted on every path.
/// - `x-api-key` is accepted **only** on `/v1/messages` (D9).
/// - `anthropic-version` is not enforced.
///
/// Auth failures on `/v1/messages` render the Anthropic error envelope
/// (`{"type":"error","error":{...}}`, §8.2); other paths keep the OpenAI
/// `{"error":{...}}` shape.
pub async fn gateway_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let auth_header = request
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|s| s.trim().to_string());

    let path = request.uri().path().to_string();
    let is_messages = accepts_x_api_key(&path);
    let x_api_key = if is_messages {
        request
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    } else {
        // Non-/v1/messages paths: x-api-key is ignored entirely.
        None
    };

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
                    let provided_key = match auth_header.or(x_api_key) {
                        Some(key) => key,
                        None => {
                            warn!(path = %path, "request missing Authorization header");
                            let err = AppError::Unauthorized(if is_messages {
                                "missing x-api-key or Authorization header".into()
                            } else {
                                "missing Authorization header".into()
                            });
                            return if is_messages {
                                err.into_anthropic_response()
                            } else {
                                err.into_response()
                            };
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
        warn!(path = %path, "invalid gateway API key");
        let err = AppError::Unauthorized("invalid API key".into());
        return if is_messages {
            err.into_anthropic_response()
        } else {
            err.into_response()
        };
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
            gateway_auth: auth_keys.map(|keys| sag_config::GatewayAuthConfig { api_keys: keys }),
            ..sag_config::gateway_config_base!()
        };
        let handle = ConfigHandle::from_config(config);
        let registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
        sag_state::testing::test_app_state(handle, Arc::new(RwLock::new(registry))).await
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

#[cfg(test)]
mod anthropic_x_api_key_tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        middleware,
        routing::{get, post},
    };
    use sag_config::ConfigHandle;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    async fn test_state(keys: Option<Vec<String>>) -> AppState {
        let config = sag_config::GatewayConfig {
            gateway_auth: keys.map(|api_keys| sag_config::GatewayAuthConfig { api_keys }),
            ..sag_config::gateway_config_base!()
        };
        let handle = ConfigHandle::from_config(config);
        let registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
        sag_state::testing::test_app_state(handle, Arc::new(RwLock::new(registry))).await
    }

    /// App with protected routes covering the three acceptance paths.
    fn protected_app(state: AppState) -> Router {
        async fn handler() -> &'static str {
            "ok"
        }
        Router::new()
            .route("/v1/messages", post(handler))
            .route("/v1/chat/completions", post(handler))
            .route("/api/providers", get(handler))
            .route_layer(middleware::from_fn_with_state(state.clone(), gateway_auth))
            .with_state(state)
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // ── D9 positive: x-api-key unlocks /v1/messages ────────────────

    #[tokio::test]
    async fn x_api_key_accepted_on_v1_messages() {
        let state = test_state(Some(vec!["secret".into()])).await;
        let app = protected_app(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("x-api-key", "secret")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"model":"m","max_tokens":1,"messages":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "valid x-api-key must unlock /v1/messages"
        );
    }

    #[tokio::test]
    async fn bearer_still_accepted_on_v1_messages() {
        let state = test_state(Some(vec!["secret".into()])).await;
        let app = protected_app(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("Authorization", "Bearer secret")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"model":"m","max_tokens":1,"messages":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn invalid_x_api_key_on_messages_returns_401_anthropic_envelope() {
        let state = test_state(Some(vec!["secret".into()])).await;
        let app = protected_app(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("x-api-key", "wrong")
                    .header("content-type", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let json = body_json(resp).await;
        // Anthropic error envelope (§8.2).
        assert_eq!(json["type"], "error");
        assert_eq!(json["error"]["type"], "authentication_error");
        assert!(
            json["error"]["message"].is_string(),
            "message must be present: {json}"
        );
    }

    // ── D9 negative: x-api-key must NOT unlock other paths ─────────

    #[tokio::test]
    async fn x_api_key_rejected_on_chat_completions() {
        let state = test_state(Some(vec!["secret".into()])).await;
        let app = protected_app(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("x-api-key", "secret")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"model":"m","messages":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "x-api-key must not unlock /v1/chat/completions"
        );
        let json = body_json(resp).await;
        // Chat path keeps the OpenAI error shape.
        assert_eq!(json["error"]["code"], "UNAUTHORIZED");
        assert!(
            json.get("type").is_none(),
            "chat path must stay OpenAI envelope: {json}"
        );
    }

    #[tokio::test]
    async fn x_api_key_rejected_on_api_paths() {
        let state = test_state(Some(vec!["secret".into()])).await;
        let app = protected_app(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/providers")
                    .header("x-api-key", "secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "x-api-key must not unlock /api/*"
        );
        let json = body_json(resp).await;
        assert_eq!(json["error"]["code"], "UNAUTHORIZED");
    }

    #[tokio::test]
    async fn missing_auth_on_messages_returns_anthropic_envelope() {
        let state = test_state(Some(vec!["secret".into()])).await;
        let app = protected_app(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let json = body_json(resp).await;
        assert_eq!(json["type"], "error");
        assert_eq!(json["error"]["type"], "authentication_error");
    }

    #[tokio::test]
    async fn accepts_x_api_key_path_predicate() {
        assert!(accepts_x_api_key("/v1/messages"));
        assert!(accepts_x_api_key("/v1/messages/"));
        assert!(!accepts_x_api_key("/v1/chat/completions"));
        assert!(!accepts_x_api_key("/api/providers"));
        assert!(!accepts_x_api_key("/v1/models"));
        assert!(!accepts_x_api_key("/health"));
    }
}
