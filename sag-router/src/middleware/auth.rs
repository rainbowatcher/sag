use axum::{
    extract::{ConnectInfo, Request, State},
    http::header,
    middleware::Next,
    response::{IntoResponse, Response},
};
use sag_core::AppError;
use std::net::SocketAddr;
use tracing::warn;

use sag_state::AppState;

/// Maximum number of characters kept for a header-derived diagnostic field in
/// the auth warnings (`User-Agent`, and the proxy IP headers via
/// [`client_ip_for_log`]). Real client values are well below this; the cap only
/// bounds a hostile header.
const USER_AGENT_MAX_CHARS: usize = 128;

/// Placeholder logged when a diagnostic field is unavailable.
const UNKNOWN_FIELD: &str = "-";

/// Keep at most `max_chars` characters of `value` (never splits a char).
fn truncate_chars(value: &str, max_chars: usize) -> String {
    match value.char_indices().nth(max_chars) {
        Some((end, _)) => value[..end].to_string(),
        None => value.to_string(),
    }
}

/// Client IP used only for diagnostics in the missing-auth warning.
///
/// `ConnectInfo<SocketAddr>` wins when the server is started with
/// `into_make_service_with_connect_info` (it is not today — see
/// `sag-gateway/src/main.rs`). Otherwise the reverse-proxy headers are used,
/// in the same precedence as the chat handlers (`x-forwarded-for` first value,
/// then `x-real-ip`), and `"-"` when neither is present.
///
/// The value comes straight from a client-controllable header, so it is capped
/// at [`USER_AGENT_MAX_CHARS`] like the `User-Agent` — an unauthenticated
/// caller must not be able to inflate every warning line.
fn client_ip_for_log(request: &Request) -> String {
    if let Some(ConnectInfo(addr)) = request.extensions().get::<ConnectInfo<SocketAddr>>() {
        return addr.ip().to_string();
    }

    let headers = request.headers();
    let forwarded = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let real_ip = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty());

    forwarded
        .or(real_ip)
        .map(|value| truncate_chars(value, USER_AGENT_MAX_CHARS))
        .unwrap_or_else(|| UNKNOWN_FIELD.to_string())
}

/// `User-Agent` used only for diagnostics, truncated to
/// [`USER_AGENT_MAX_CHARS`] characters; `"-"` when absent (or not ASCII).
fn user_agent_for_log(request: &Request) -> String {
    request
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| truncate_chars(v, USER_AGENT_MAX_CHARS))
        .unwrap_or_else(|| UNKNOWN_FIELD.to_string())
}

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
                            // Diagnostics for unauthenticated traffic: which
                            // peer and which client, never which credential
                            // (there is none — that is the point of the log).
                            warn!(
                                path = %path,
                                client_ip = %client_ip_for_log(&request),
                                user_agent = %user_agent_for_log(&request),
                                "request missing Authorization header"
                            );
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
        // Same attribution as the missing-header branch: invalid keys are the
        // brute-force signal, so the peer and client matter even more here.
        warn!(
            path = %path,
            client_ip = %client_ip_for_log(&request),
            user_agent = %user_agent_for_log(&request),
            "invalid gateway API key"
        );
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

    // ── missing-Authorization warning carries client_ip / user_agent ──

    /// Minimal `MakeWriter` that appends every formatted event to a buffer.
    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Capture {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
        }

        /// The captured missing-Authorization warning line.
        fn missing_auth_line(&self) -> String {
            let text = self.text();
            text.lines()
                .find(|l| l.contains("request missing Authorization header"))
                .map(str::to_string)
                .unwrap_or_else(|| panic!("no missing-auth warning captured:\n{text}"))
        }
    }

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Scoped (thread-local) subscriber: `tracing::subscriber::set_default`
    /// keeps the capture off the process-global dispatch, so the other tests
    /// in this binary are unaffected.
    fn capture_warnings() -> (Capture, tracing::subscriber::DefaultGuard) {
        let capture = Capture::default();
        let guard = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_writer(capture.clone())
                .with_max_level(tracing::Level::WARN)
                .with_ansi(false)
                .finish(),
        );
        (capture, guard)
    }

    #[tokio::test]
    async fn missing_auth_warns_with_client_ip_and_user_agent() {
        let state = test_state(Some(vec!["secret".into()])).await;
        let app = test_app(state);
        let (capture, _guard) = capture_warnings();

        let request = Request::builder()
            .uri("/test")
            .header(header::USER_AGENT, "probe-agent/1.0")
            .header("x-forwarded-for", "203.0.113.9, 70.41.3.18")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let line = capture.missing_auth_line();
        assert!(
            line.contains("client_ip=203.0.113.9"),
            "first X-Forwarded-For value must be logged: {line}"
        );
        assert!(
            line.contains("user_agent=probe-agent/1.0"),
            "User-Agent must be logged: {line}"
        );
    }

    #[tokio::test]
    async fn missing_auth_warns_with_placeholders_when_absent() {
        let state = test_state(Some(vec!["secret".into()])).await;
        let app = test_app(state);
        let (capture, _guard) = capture_warnings();

        let request = Request::builder().uri("/test").body(Body::empty()).unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let line = capture.missing_auth_line();
        // The fmt layer prints the message first and the fields after it, so
        // a placeholder field can also end the line — compare whole tokens.
        let fields: Vec<&str> = line.split_whitespace().collect();
        assert!(
            fields.contains(&"client_ip=-"),
            "missing peer headers must log a placeholder: {line}"
        );
        assert!(
            fields.contains(&"user_agent=-"),
            "missing User-Agent must log a placeholder: {line}"
        );
    }

    #[test]
    fn client_ip_falls_back_to_x_real_ip() {
        let request = Request::builder()
            .uri("/test")
            .header("x-real-ip", "198.51.100.4")
            .body(Body::empty())
            .unwrap();
        assert_eq!(client_ip_for_log(&request), "198.51.100.4");
    }

    #[test]
    fn client_ip_is_truncated_to_the_cap() {
        // `x-forwarded-for` is client-controllable: an unauthenticated caller
        // must not be able to inflate every warning line, so the same cap as
        // `User-Agent` applies.
        let long_ip = "1".repeat(USER_AGENT_MAX_CHARS + 40);
        let request = Request::builder()
            .uri("/test")
            .header("x-forwarded-for", &long_ip)
            .body(Body::empty())
            .unwrap();
        let logged = client_ip_for_log(&request);
        assert_eq!(logged.chars().count(), USER_AGENT_MAX_CHARS);
        assert!(long_ip.starts_with(&logged));
    }

    #[test]
    fn user_agent_is_truncated_to_the_cap() {
        let long_ua = "u".repeat(USER_AGENT_MAX_CHARS + 40);
        let request = Request::builder()
            .uri("/test")
            .header(header::USER_AGENT, &long_ua)
            .body(Body::empty())
            .unwrap();
        let logged = user_agent_for_log(&request);
        assert_eq!(logged.chars().count(), USER_AGENT_MAX_CHARS);
        assert!(long_ua.starts_with(&logged));
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
