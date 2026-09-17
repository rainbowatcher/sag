use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::types::{FailureReason, FinalStatus};

/// Circuit breaker open-duration hint extracted from an upstream error.
///
/// [`AppError::circuit_hint`] returns one of these
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
    /// Look up the open-duration in seconds from the configured values.
    /// `quota_secs` / `rate_secs` / `generic_secs` are typically read from
    /// the `[circuit_breaker]` config section.
    pub fn duration_secs(self, quota_secs: u64, rate_secs: u64, generic_secs: u64) -> u64 {
        match self {
            CircuitHint::QuotaExhausted => quota_secs,
            CircuitHint::RateLimited => rate_secs,
            CircuitHint::Generic => generic_secs,
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

    /// Resource not found (admin API: provider, route, etc.).
    /// Distinct from [`ModelNotFound`](Self::ModelNotFound) which carries
    /// the `MODEL_NOT_FOUND` code for the dispatch path; admin lookups use
    /// the generic `NOT_FOUND` code.
    #[error("not found: {0}")]
    NotFound(String),

    /// Conflicting state (duplicate provider/route name in admin API).
    #[error("conflict: {0}")]
    Conflict(String),

    /// All providers exhausted (retries / fallbacks failed).
    #[error("all providers exhausted for model={model}: {reason}")]
    AllProvidersExhausted { model: String, reason: String },

    /// Network / transport error.
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),

    /// Upstream did not send response headers within the time-to-first-byte
    /// budget on a streaming request. Retryable so the dispatcher can fall
    /// over to the next target.
    #[error("upstream time-to-first-byte timeout after {secs}s (provider={provider})")]
    TtfbTimeout { provider: String, secs: u64 },

    /// Upstream returned a response with zero `completion_tokens` despite a
    /// non-zero prompt. The stream (or non-stream response body) is complete
    /// but carries no generated output -- the model effectively returned an
    /// empty answer. Retryable so the dispatcher retries the same target with
    /// backoff, then falls through to the next target when retries are
    /// exhausted.
    #[error("upstream returned empty response with no content in choices (provider={provider})")]
    EmptyResponse { provider: String },

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
    /// - HTTP 429/403 with a body hinting at quota/billing → [`CircuitHint::QuotaExhausted`]
    /// - HTTP 429 without quota language → [`CircuitHint::RateLimited`]
    /// - HTTP 403 without quota language → [`CircuitHint::Generic`]
    /// - Everything else → [`CircuitHint::Generic`]
    pub fn circuit_hint(&self) -> CircuitHint {
        match self {
            AppError::Upstream {
                status: 429 | 403,
                body,
                ..
            } => {
                let lower = body.to_lowercase();
                // Check "rate limit" first: it is a narrow, high-precision
                // match. Quota keywords like "exceeded" / "insufficient"
                // are broader and would misclassify a plain rate-limit 429
                // as QuotaExhausted (1 h) instead of RateLimited (10 s).
                if lower.contains("rate limit") {
                    CircuitHint::RateLimited
                } else if lower.contains("quota")
                    || lower.contains("billing")
                    || lower.contains("insufficient")
                    || lower.contains("exceeded")
                    || lower.contains("usage limit")
                    || lower.contains("current quota")
                {
                    CircuitHint::QuotaExhausted
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

    /// Map the error variant to a machine-readable category for analytics.
    ///
    /// Used by [`crate::CallLog::error_category`] so dashboards can filter
    /// and aggregate by error type without string-matching on `error`.
    pub fn error_category(&self) -> &'static str {
        match self {
            AppError::Config(_) => "internal_error",
            AppError::Unauthorized(_) => "auth_error",
            AppError::Upstream { .. } => "provider_error",
            AppError::BadRequest(_) => "bad_request",
            AppError::ModelNotFound(_) => "model_not_found",
            AppError::NotFound(_) => "internal_error",
            AppError::Conflict(_) => "internal_error",
            AppError::AllProvidersExhausted { .. } => "all_exhausted",
            AppError::Network(_) => "network_error",
            AppError::TtfbTimeout { .. } => "timeout",
            AppError::EmptyResponse { .. } => "empty_response",
            AppError::Internal(_) => "internal_error",
            AppError::Anyhow(_) => "internal_error",
        }
    }

    /// Map the error to the analytics [`FailureReason`] for facts.
    ///
    /// `None` means this error carries no registered failure reason:
    /// - `Unauthorized` — requests failing authentication are **excluded**
    ///   from analytics entirely (no facts are emitted, per spec);
    /// - config / admin / internal variants have no registered reason.
    ///
    /// The resulting reason always has a legal [`FinalStatus`] via
    /// [`FailureReason::final_status`] — see `final_status()`.
    pub fn failure_reason(&self) -> Option<FailureReason> {
        match self {
            AppError::BadRequest(_) => Some(FailureReason::InvalidRequest),
            AppError::ModelNotFound(_) => Some(FailureReason::ModelNotFound),
            // HTTP 429 upstream → rate-limited; everything else upstream
            // (including 403 quota language) → provider error.
            AppError::Upstream { status: 429, .. } => Some(FailureReason::RateLimited),
            AppError::Upstream { .. } => Some(FailureReason::ProviderError),
            AppError::AllProvidersExhausted { .. } => Some(FailureReason::AllExhausted),
            AppError::Network(_) => Some(FailureReason::NetworkError),
            AppError::TtfbTimeout { .. } => Some(FailureReason::Timeout),
            AppError::EmptyResponse { .. } => Some(FailureReason::EmptyResponse),
            // Unauthorized is excluded from analytics; config/admin/internal
            // variants are not part of the dispatch-path classification.
            _ => None,
        }
    }

    /// Map the error to the analytics [`FinalStatus`] for the request-level
    /// fact. `None` = excluded from analytics (authentication failure) or no
    /// registered status (config/admin/internal errors).
    ///
    /// Implemented via [`AppError::failure_reason`] + the authoritative
    /// [`FailureReason::final_status`] table, so the `(failure_reason,
    /// final_status)` combination is always legal.
    pub fn final_status(&self) -> Option<FinalStatus> {
        self.failure_reason().map(FailureReason::final_status)
    }

    /// Map this error to its HTTP status code (same mapping used by
    /// `into_response`).
    ///
    /// Extracted as a public method so dispatch-layer helpers (e.g.
    /// `log_per_target_failure`) can record the real upstream status in
    /// `call_logs` instead of a hardcoded 502.
    pub fn status_code(&self) -> StatusCode {
        match self {
            AppError::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            AppError::Upstream { status, .. } => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
            }
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::ModelNotFound(_) => StatusCode::NOT_FOUND,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::AllProvidersExhausted { .. } => StatusCode::BAD_GATEWAY,
            AppError::Network(_) => StatusCode::BAD_GATEWAY,
            AppError::TtfbTimeout { .. } => StatusCode::GATEWAY_TIMEOUT,
            AppError::EmptyResponse { .. } => StatusCode::BAD_GATEWAY,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::Anyhow(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let (code, message) = match &self {
            AppError::Config(msg) => ("CONFIG_ERROR", msg.clone()),
            AppError::Unauthorized(msg) => ("UNAUTHORIZED", msg.clone()),
            AppError::Upstream { body, .. } => ("UPSTREAM_ERROR", body.clone()),
            AppError::BadRequest(msg) => ("BAD_REQUEST", msg.clone()),
            AppError::ModelNotFound(msg) => ("MODEL_NOT_FOUND", msg.clone()),
            AppError::NotFound(msg) => ("NOT_FOUND", msg.clone()),
            AppError::Conflict(msg) => ("CONFLICT", msg.clone()),
            AppError::AllProvidersExhausted { reason, .. } => {
                ("ALL_PROVIDERS_EXHAUSTED", reason.clone())
            }
            AppError::Network(e) => ("NETWORK_ERROR", e.to_string()),
            AppError::TtfbTimeout { .. } => ("TTFB_TIMEOUT", self.to_string()),
            AppError::EmptyResponse { .. } => ("EMPTY_RESPONSE", self.to_string()),
            AppError::Internal(msg) => ("INTERNAL_ERROR", msg.clone()),
            AppError::Anyhow(e) => ("INTERNAL_ERROR", e.to_string()),
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
    async fn not_found_returns_404() {
        let (status, body) = extract(AppError::NotFound("provider 'foo' not found".into())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "NOT_FOUND");
        assert_eq!(body["error"]["message"], "provider 'foo' not found");
    }

    #[tokio::test]
    async fn conflict_returns_409() {
        let (status, body) =
            extract(AppError::Conflict("provider 'foo' already exists".into())).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], "CONFLICT");
        assert_eq!(body["error"]["message"], "provider 'foo' already exists");
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
        // on localhost with a minimal timeout. `.no_proxy()` keeps the
        // test hermetic: without it a machine-level `HTTP_PROXY` env var
        // would route the request through the proxy, which answers with
        // 502 instead of failing to connect.
        let err = reqwest::Client::builder()
            .no_proxy()
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
    async fn ttfb_timeout_returns_504() {
        let (status, body) = extract(AppError::TtfbTimeout {
            provider: "openai".into(),
            secs: 60,
        })
        .await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(body["error"]["code"], "TTFB_TIMEOUT");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("time-to-first-byte")
        );
    }

    #[tokio::test]
    async fn empty_response_returns_502() {
        let (status, body) = extract(AppError::EmptyResponse {
            provider: "cpa".into(),
        })
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["error"]["code"], "EMPTY_RESPONSE");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no content in choices")
        );
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

    // ── error_category mapping tests ────────────────────────────

    #[test]
    fn error_category_model_not_found() {
        assert_eq!(
            AppError::ModelNotFound("gpt-5".into()).error_category(),
            "model_not_found"
        );
    }

    #[test]
    fn error_category_all_exhausted() {
        assert_eq!(
            AppError::AllProvidersExhausted {
                model: "gpt-4o".into(),
                reason: "all failed".into()
            }
            .error_category(),
            "all_exhausted"
        );
    }

    #[test]
    fn error_category_upstream() {
        assert_eq!(
            AppError::Upstream {
                provider: "openai".into(),
                status: 500,
                body: "boom".into()
            }
            .error_category(),
            "provider_error"
        );
    }

    #[tokio::test]
    async fn error_category_network() {
        // Build a minimal reqwest::Error. `.no_proxy()` keeps the test
        // hermetic against a machine-level `HTTP_PROXY` env var (see
        // `network_error_returns_502`).
        let req_err = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(std::time::Duration::from_millis(5))
            .timeout(std::time::Duration::from_millis(10))
            .build()
            .unwrap()
            .get("http://127.0.0.1:1")
            .send()
            .await
            .unwrap_err();
        assert_eq!(AppError::from(req_err).error_category(), "network_error");
    }

    #[test]
    fn error_category_ttfb_timeout() {
        assert_eq!(
            AppError::TtfbTimeout {
                provider: "openai".into(),
                secs: 60
            }
            .error_category(),
            "timeout"
        );
    }

    #[test]
    fn error_category_empty_response() {
        assert_eq!(
            AppError::EmptyResponse {
                provider: "cpa".into()
            }
            .error_category(),
            "empty_response"
        );
    }

    #[test]
    fn error_category_bad_request() {
        assert_eq!(
            AppError::BadRequest("bad param".into()).error_category(),
            "bad_request"
        );
    }

    #[test]
    fn error_category_unauthorized() {
        assert_eq!(
            AppError::Unauthorized("bad key".into()).error_category(),
            "auth_error"
        );
    }

    #[test]
    fn error_category_internal() {
        assert_eq!(
            AppError::Internal("boom".into()).error_category(),
            "internal_error"
        );
        assert_eq!(
            AppError::Config("bad".into()).error_category(),
            "internal_error"
        );
        assert_eq!(
            AppError::NotFound("gone".into()).error_category(),
            "internal_error"
        );
        assert_eq!(
            AppError::Conflict("dup".into()).error_category(),
            "internal_error"
        );
    }

    #[test]
    fn error_category_anyhow() {
        assert_eq!(
            AppError::Anyhow(anyhow::anyhow!("oops")).error_category(),
            "internal_error"
        );
    }

    // ── circuit_hint tests ───────────────────────────────────────

    #[test]
    fn circuit_hint_429_quota_body_is_quota_exhausted() {
        assert_eq!(
            AppError::Upstream {
                provider: "openai".into(),
                status: 429,
                body: "quota exceeded".into(),
            }
            .circuit_hint(),
            CircuitHint::QuotaExhausted
        );
    }

    #[test]
    fn circuit_hint_429_plain_rate_limit_is_rate_limited() {
        // Narrow "rate limit" match wins over the broader quota keywords.
        assert_eq!(
            AppError::Upstream {
                provider: "openai".into(),
                status: 429,
                body: "rate limit reached".into(),
            }
            .circuit_hint(),
            CircuitHint::RateLimited
        );
        // Unknown 429 (no recognizable keyword) defaults to Generic (60s),
        // not the short 10s cooldown — an unrecognised quota 429 must not
        // hammer the upstream for an hour.
        assert_eq!(
            AppError::Upstream {
                provider: "openai".into(),
                status: 429,
                body: "too many requests".into(),
            }
            .circuit_hint(),
            CircuitHint::Generic
        );
    }

    #[test]
    fn circuit_hint_403_quota_body_is_quota_exhausted() {
        // 403 sharing the 429 quota/billing keyword set → QuotaExhausted
        // (self-healing, long cooldown).
        for body in [
            "quota exceeded",
            "billing limit",
            "insufficient quota",
            "usage limit",
            "current quota",
        ] {
            assert_eq!(
                AppError::Upstream {
                    provider: "openai".into(),
                    status: 403,
                    body: body.into(),
                }
                .circuit_hint(),
                CircuitHint::QuotaExhausted,
                "403 body {body:?} should classify as QuotaExhausted"
            );
        }
    }

    #[test]
    fn circuit_hint_403_non_quota_body_is_generic() {
        assert_eq!(
            AppError::Upstream {
                provider: "openai".into(),
                status: 403,
                body: "permission denied".into(),
            }
            .circuit_hint(),
            CircuitHint::Generic
        );
    }

    #[test]
    fn circuit_hint_other_statuses_are_generic() {
        assert_eq!(
            AppError::Upstream {
                provider: "openai".into(),
                status: 400,
                body: "bad request".into(),
            }
            .circuit_hint(),
            CircuitHint::Generic
        );
        assert_eq!(
            AppError::Internal("boom".into()).circuit_hint(),
            CircuitHint::Generic
        );
    }

    // ── analytics failure_reason / final_status mapping tests ─────

    #[test]
    fn failure_reason_bad_request_is_invalid_request_client_error() {
        let err = AppError::BadRequest("unsupported param".into());
        assert_eq!(err.failure_reason(), Some(FailureReason::InvalidRequest));
        assert_eq!(err.final_status(), Some(FinalStatus::ClientError));
    }

    #[test]
    fn failure_reason_model_not_found_is_no_route() {
        let err = AppError::ModelNotFound("gpt-99".into());
        assert_eq!(err.failure_reason(), Some(FailureReason::ModelNotFound));
        assert_eq!(err.final_status(), Some(FinalStatus::NoRoute));
    }

    #[test]
    fn failure_reason_upstream_429_is_rate_limited() {
        let err = AppError::Upstream {
            provider: "openai".into(),
            status: 429,
            body: "rate limit reached".into(),
        };
        assert_eq!(err.failure_reason(), Some(FailureReason::RateLimited));
        assert_eq!(err.final_status(), Some(FinalStatus::ModelFailure));
    }

    #[test]
    fn failure_reason_upstream_other_is_provider_error() {
        for status in [400, 403, 500, 502, 503] {
            let err = AppError::Upstream {
                provider: "openai".into(),
                status,
                body: "boom".into(),
            };
            assert_eq!(
                err.failure_reason(),
                Some(FailureReason::ProviderError),
                "status {status} should map to provider_error"
            );
            assert_eq!(err.final_status(), Some(FinalStatus::ModelFailure));
        }
    }

    #[test]
    fn failure_reason_all_exhausted_is_model_failure() {
        let err = AppError::AllProvidersExhausted {
            model: "gpt-4o".into(),
            reason: "all timed out".into(),
        };
        assert_eq!(err.failure_reason(), Some(FailureReason::AllExhausted));
        assert_eq!(err.final_status(), Some(FinalStatus::ModelFailure));
    }

    #[tokio::test]
    async fn failure_reason_network_is_model_failure() {
        // Build a minimal reqwest::Error via an unreachable localhost port.
        // `.no_proxy()` keeps the test hermetic against a machine-level
        // `HTTP_PROXY` env var (see `network_error_returns_502`).
        let req_err = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(std::time::Duration::from_millis(5))
            .timeout(std::time::Duration::from_millis(10))
            .build()
            .unwrap()
            .get("http://127.0.0.1:1")
            .send()
            .await
            .unwrap_err();
        let err = AppError::from(req_err);
        assert_eq!(err.failure_reason(), Some(FailureReason::NetworkError));
        assert_eq!(err.final_status(), Some(FinalStatus::ModelFailure));
    }

    #[test]
    fn failure_reason_ttfb_timeout_is_model_failure() {
        let err = AppError::TtfbTimeout {
            provider: "openai".into(),
            secs: 60,
        };
        assert_eq!(err.failure_reason(), Some(FailureReason::Timeout));
        assert_eq!(err.final_status(), Some(FinalStatus::ModelFailure));
    }

    #[test]
    fn failure_reason_empty_response_is_model_failure() {
        let err = AppError::EmptyResponse {
            provider: "cpa".into(),
        };
        assert_eq!(err.failure_reason(), Some(FailureReason::EmptyResponse));
        assert_eq!(err.final_status(), Some(FinalStatus::ModelFailure));
    }

    #[test]
    fn failure_reason_unauthorized_is_excluded_from_analytics() {
        // Authentication failures are completely excluded: no facts, so no
        // reason or status.
        let err = AppError::Unauthorized("bad key".into());
        assert_eq!(err.failure_reason(), None);
        assert_eq!(err.final_status(), None);
    }

    #[test]
    fn failure_reason_internal_variants_have_no_registered_reason() {
        // Config / admin / internal errors are not part of the dispatch-path
        // classification table; they must not produce an illegal combination.
        for err in [
            AppError::Config("bad".into()),
            AppError::NotFound("gone".into()),
            AppError::Conflict("dup".into()),
            AppError::Internal("boom".into()),
            AppError::Anyhow(anyhow::anyhow!("oops")),
        ] {
            assert_eq!(err.failure_reason(), None, "{err:?}");
            assert_eq!(err.final_status(), None, "{err:?}");
        }
    }

    #[test]
    fn failure_reason_and_status_combinations_are_legal() {
        // Every mapped reason must agree with the authoritative
        // FailureReason::final_status table. (Network is covered separately
        // in `failure_reason_network_is_model_failure` because it needs a
        // real reqwest error.)
        for err in [
            AppError::BadRequest("x".into()),
            AppError::ModelNotFound("x".into()),
            AppError::Upstream {
                provider: "o".into(),
                status: 429,
                body: "rate limit".into(),
            },
            AppError::Upstream {
                provider: "o".into(),
                status: 500,
                body: "boom".into(),
            },
            AppError::AllProvidersExhausted {
                model: "m".into(),
                reason: "r".into(),
            },
            AppError::TtfbTimeout {
                provider: "o".into(),
                secs: 60,
            },
            AppError::EmptyResponse {
                provider: "o".into(),
            },
        ] {
            let reason = err.failure_reason().expect("mapped reason");
            assert_eq!(
                err.final_status(),
                Some(reason.final_status()),
                "final_status must follow the authoritative reason table"
            );
        }
    }
}
