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
