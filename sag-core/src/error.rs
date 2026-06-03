use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Unified error type for the entire gateway.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// Missing or invalid configuration.
    #[error("configuration error: {0}")]
    Config(String),

    /// Gateway client authentication failure.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// Upstream provider returned an error.
    #[error("upstream provider error: provider={provider}, status={status}, body={body}")]
    Upstream {
        provider: String,
        status: u16,
        body: String,
    },

    /// Request validation failed (unsupported parameters, etc.).
    #[error("bad request: {0}")]
    BadRequest(String),

    /// Model not found in routing table.
    #[error("model not found: {0}")]
    ModelNotFound(String),

    /// All providers exhausted (retries / fallbacks failed).
    #[error("all providers exhausted for model={model}: {reason}")]
    AllProvidersExhausted { model: String, reason: String },

    /// Network / transport error.
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    /// Generic internal error.
    #[error("internal error: {0}")]
    Internal(String),

    /// Convert anyhow errors explicitly (no auto-From).
    #[error("{0}")]
    Anyhow(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            AppError::Config(msg) => (StatusCode::INTERNAL_SERVER_ERROR, "CONFIG_ERROR", msg.clone()),
            AppError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, "UNAUTHORIZED", msg.clone()),
            AppError::Upstream { status, body, .. } => {
                let s = StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY);
                (s, "UPSTREAM_ERROR", body.clone())
            }
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "BAD_REQUEST", msg.clone()),
            AppError::ModelNotFound(msg) => (StatusCode::NOT_FOUND, "MODEL_NOT_FOUND", msg.clone()),
            AppError::AllProvidersExhausted { reason, .. } => {
                (StatusCode::BAD_GATEWAY, "ALL_PROVIDERS_EXHAUSTED", reason.clone())
            }
            AppError::Network(e) => (StatusCode::BAD_GATEWAY, "NETWORK_ERROR", e.to_string()),
            AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", msg.clone()),
            AppError::Anyhow(e) => (StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", e.to_string()),
        };

        let body = json!({
            "error": {
                "code": code,
                "message": message,
            }
        });

        (status, Json(body)).into_response()
    }
}
