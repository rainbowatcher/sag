use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Circuit breaker open-duration hint extracted from an upstream error.
///
/// [`AppError::circuit_open_duration_secs`] returns one of these
/// categories so the dispatch layer can choose an appropriate cooldown
/// without string-matching on error bodies at every call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitHint {
    /// Quota / billing exhausted — long cooldown (default 1 hour).
    QuotaExhausted,
    /// Rate-limited (HTTP 429) — short cooldown (default 10 seconds).
    RateLimited,
    /// Generic retryable failure — default cooldown (60 seconds).
    Generic,
}

impl CircuitHint {
    /// Default open-duration in seconds for each hint category.
    pub fn default_duration_secs(self) -> u64 {
        match self {
            CircuitHint::QuotaExhausted => 3600,
            CircuitHint::RateLimited => 10,
            CircuitHint::Generic => 60,
        }
    }
}

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

impl AppError {
    /// Classify the error for circuit-breaker open-duration selection.
    ///
    /// - HTTP 429 with a body hinting at quota/billing → [`CircuitHint::QuotaExhausted`]
    /// - HTTP 429 without quota language → [`CircuitHint::RateLimited`]
    /// - Everything else → [`CircuitHint::Generic`]
    pub fn circuit_hint(&self) -> CircuitHint {
        match self {
            AppError::Upstream {
                status: 429, body, ..
            } => {
                let lower = body.to_lowercase();
                if lower.contains("quota")
                    || lower.contains("billing")
                    || lower.contains("insufficient")
                    || lower.contains("exceeded")
                    || lower.contains("usage limit")
                    || lower.contains("current quota")
                {
                    CircuitHint::QuotaExhausted
                } else if lower.contains("rate limit") {
                    CircuitHint::RateLimited
                } else {
                    // Unknown 429 — default to 60 s. A 10 s cooldown for
                    // a quota-exhaustion 429 we didn't recognise would
                    // hammer the upstream for an hour straight.
                    CircuitHint::Generic
                }
            }
            _ => CircuitHint::Generic,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            AppError::Config(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "CONFIG_ERROR",
                msg.clone(),
            ),
            AppError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, "UNAUTHORIZED", msg.clone()),
            AppError::Upstream { status, body, .. } => {
                let s = StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY);
                (s, "UPSTREAM_ERROR", body.clone())
            }
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, "BAD_REQUEST", msg.clone()),
            AppError::ModelNotFound(msg) => (StatusCode::NOT_FOUND, "MODEL_NOT_FOUND", msg.clone()),
            AppError::AllProvidersExhausted { reason, .. } => (
                StatusCode::BAD_GATEWAY,
                "ALL_PROVIDERS_EXHAUSTED",
                reason.clone(),
            ),
            AppError::Network(e) => (StatusCode::BAD_GATEWAY, "NETWORK_ERROR", e.to_string()),
            AppError::Internal(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                msg.clone(),
            ),
            AppError::Anyhow(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                e.to_string(),
            ),
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    /// Helper: extract status code and JSON body from an `AppError` response.
    async fn extract(app_error: AppError) -> (StatusCode, serde_json::Value) {
        let response: Response = app_error.into_response();
        let status = response.status();
        let body_bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        (status, body)
    }

    #[tokio::test]
    async fn config_error_returns_500() {
        let (status, body) = extract(AppError::Config("missing field".into())).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"]["code"], "CONFIG_ERROR");
        assert_eq!(body["error"]["message"], "missing field");
    }

    #[tokio::test]
    async fn unauthorized_returns_401() {
        let (status, body) = extract(AppError::Unauthorized("bad token".into())).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "UNAUTHORIZED");
        assert_eq!(body["error"]["message"], "bad token");
    }

    #[tokio::test]
    async fn bad_request_returns_400() {
        let (status, body) = extract(AppError::BadRequest("unsupported param seed".into())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "BAD_REQUEST");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("unsupported")
        );
    }

    #[tokio::test]
    async fn model_not_found_returns_404() {
        let (status, body) = extract(AppError::ModelNotFound("unknown-model".into())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "MODEL_NOT_FOUND");
        assert_eq!(body["error"]["message"], "unknown-model");
    }

    #[tokio::test]
    async fn upstream_error_preserves_status() {
        let (status, body) = extract(AppError::Upstream {
            provider: "openai".into(),
            status: 429,
            body: "rate limited".into(),
        })
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body["error"]["code"], "UPSTREAM_ERROR");
        assert_eq!(body["error"]["message"], "rate limited");

        // Invalid status code (0 is not a valid HTTP status) falls back to 502
        let (status, _) = extract(AppError::Upstream {
            provider: "openai".into(),
            status: 0,
            body: "weird".into(),
        })
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn all_providers_exhausted_returns_502() {
        let (status, body) = extract(AppError::AllProvidersExhausted {
            model: "gpt-4".into(),
            reason: "all timed out".into(),
        })
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["error"]["code"], "ALL_PROVIDERS_EXHAUSTED");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("all timed out")
        );
    }

    #[tokio::test]
    async fn network_error_returns_502() {
        // Construct a reqwest error by connecting to an unreachable port
        // on localhost with a minimal timeout.
        let err = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_millis(5))
            .timeout(std::time::Duration::from_millis(10))
            .build()
            .unwrap()
            .get("http://127.0.0.1:1")
            .send()
            .await
            .unwrap_err();
        let app_err = AppError::from(err);
        let (status, body) = extract(app_err).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["error"]["code"], "NETWORK_ERROR");
        assert!(!body["error"]["message"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn internal_error_returns_500() {
        let (status, body) = extract(AppError::Internal("unexpected state".into())).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"]["code"], "INTERNAL_ERROR");
        assert_eq!(body["error"]["message"], "unexpected state");
    }

    #[tokio::test]
    async fn anyhow_error_returns_500() {
        let anyhow_err = anyhow::anyhow!("something went wrong");
        let app_err = AppError::from(anyhow_err);
        let (status, body) = extract(app_err).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"]["code"], "INTERNAL_ERROR");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("something went wrong")
        );
    }

    #[test]
    fn display_format_includes_context() {
        let err = AppError::ModelNotFound("claude-99".into());
        assert!(err.to_string().contains("claude-99"));
        assert!(err.to_string().contains("model not found"));

        let err2 = AppError::Upstream {
            provider: "anthropic".into(),
            status: 500,
            body: "boom".into(),
        };
        let msg = err2.to_string();
        assert!(msg.contains("anthropic"));
        assert!(msg.contains("500"));
        assert!(msg.contains("boom"));
    }
}
