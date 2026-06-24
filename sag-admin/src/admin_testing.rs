//! Provider testing utilities: discover available models on a provider
//! (`fetch_models`) and verify a specific model is reachable with a given
//! key (`model_test`). These are unauthenticated-by-default convenience
//! endpoints that take a temporary provider config and short-lived
//! reqwest client, so callers don't need a registered provider entry
//! to test connectivity.

use std::collections::HashMap;

use axum::{Json, http::StatusCode, response::IntoResponse};
use reqwest::header::CONTENT_TYPE;
use sag_config::ProviderType;
use serde_json::json;
use tracing::{instrument, warn};

#[derive(serde::Deserialize)]
pub struct FetchModelsRequest {
    pub base_url: String,
    #[serde(default)]
    pub api_keys: Vec<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    pub provider_type: Option<ProviderType>,
    /// Custom endpoint path for listing models (default: `models`).
    /// When empty, falls back to `models` appended to `base_url`.
    #[serde(default)]
    pub models_endpoint: String,
}

fn build_models_url(base_url: &str, models_endpoint: &str) -> String {
    let endpoint = sag_config::normalize_models_endpoint(models_endpoint);
    format!("{}/{}", base_url.trim_end_matches('/'), endpoint)
}

/// POST /api/model/test — test a specific model by sending a real chat completion.
/// POST /api/providers/fetch-models — fetch upstream models using a temporary provider config.
#[instrument(skip(req), fields(url))]
pub async fn fetch_models(Json(req): Json<FetchModelsRequest>) -> impl IntoResponse {
    let Some(api_key) = req.api_keys.first() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": { "code": "MISSING_API_KEYS", "message": "at least one api_key required" }
            })),
        )
            .into_response();
    };

    let models_url = build_models_url(&req.base_url, &req.models_endpoint);
    tracing::Span::current().record("url", &models_url);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("valid reqwest client with timeout");
    let mut http_req = client
        .get(&models_url)
        .header("Authorization", format!("Bearer {}", api_key));

    // Anthropic uses x-api-key instead of Bearer
    if req.provider_type == Some(ProviderType::Anthropic) {
        http_req = client
            .get(&models_url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01");
    }

    for (key, value) in &req.headers {
        http_req = http_req.header(key.as_str(), value.as_str());
    }

    match http_req.send().await {
        Ok(resp) => {
            if !resp.status().is_success() {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({
                        "error": {
                            "code": "UPSTREAM_ERROR",
                            "message": format!("upstream returned status {}", resp.status()),
                        }
                    })),
                )
                    .into_response();
            }

            match resp.json::<serde_json::Value>().await {
                Ok(mut body) => {
                    let models = body
                        .get_mut("data")
                        .map(std::mem::take)
                        .unwrap_or(serde_json::Value::Array(vec![]));

                    Json(json!({ "models": models })).into_response()
                }
                Err(e) => {
                    warn!(?e, "failed to parse upstream models response");
                    (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({
                            "error": {
                                "code": "PARSE_ERROR",
                                "message": format!("failed to parse upstream response: {e}"),
                            }
                        })),
                    )
                        .into_response()
                }
            }
        }
        Err(e) => {
            warn!(?e, "failed to reach upstream models endpoint");
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({
                    "error": {
                        "code": "CONNECTION_ERROR",
                        "message": format!("failed to connect to upstream: {e}"),
                    }
                })),
            )
                .into_response()
        }
    }
}

#[derive(serde::Deserialize)]
pub struct ModelTestRequest {
    pub model: String,
    #[serde(default)]
    pub api_keys: Vec<String>,
    pub base_url: String,
    pub provider_type: Option<ProviderType>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

pub async fn model_test(Json(req): Json<ModelTestRequest>) -> impl IntoResponse {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap();

    if req.api_keys.is_empty() {
        return Json(json!({ "results": [] })).into_response();
    }

    let mut results = Vec::new();

    for key in &req.api_keys {
        let result = test_single_key(&client, &req, key).await;
        let prefix = if key.len() > 12 {
            format!("{}…", &key[..12])
        } else {
            key.clone()
        };
        results.push(json!({
            "key_prefix": prefix,
            "success": result.0,
            "message": result.1,
        }));
    }

    Json(json!({ "results": results })).into_response()
}

async fn test_single_key(
    client: &reqwest::Client,
    req: &ModelTestRequest,
    api_key: &str,
) -> (bool, String) {
    let (url, body, auth_header, auth_value): (String, serde_json::Value, &str, String) =
        if req.provider_type == Some(ProviderType::Anthropic) {
            (
                format!("{}/messages", req.base_url.trim_end_matches('/')),
                serde_json::json!({
                    "model": req.model,
                    "messages": [{"role": "user", "content": "say 'hi'"}],
                    "max_tokens": 1
                }),
                "x-api-key",
                api_key.to_string(),
            )
        } else {
            (
                format!("{}/chat/completions", req.base_url.trim_end_matches('/')),
                serde_json::json!({
                    "model": req.model,
                    "messages": [{"role": "user", "content": "say 'hi'"}],
                    "max_tokens": 1
                }),
                "Authorization",
                format!("Bearer {api_key}"),
            )
        };

    let mut http_req = client
        .post(&url)
        .header(auth_header, &auth_value)
        .header(CONTENT_TYPE, "application/json");

    // Anthropic also needs the version header
    if req.provider_type == Some(ProviderType::Anthropic) {
        http_req = http_req.header("anthropic-version", "2023-06-01");
    }

    for (key, value) in &req.headers {
        http_req = http_req.header(key.as_str(), value.as_str());
    }

    http_req = http_req.json(&body);

    match http_req.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if (200..300).contains(&status) {
                (true, "OK".into())
            } else if status == 401 || status == 403 {
                let body = resp.text().await.unwrap_or_default();
                (false, format!("Authentication failed: {body}"))
            } else if status == 404 {
                let body = resp.text().await.unwrap_or_default();
                (false, format!("Model '{}' not found: {body}", req.model))
            } else {
                let body = resp.text().await.unwrap_or_default();
                (false, format!("upstream returned status {status}: {body}"))
            }
        }
        Err(e) => {
            if e.is_timeout() {
                (false, "request timed out after 30s".into())
            } else {
                (false, format!("connection failed: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_models_url_normalizes_endpoint_path() {
        assert_eq!(
            build_models_url("https://example.com/v1", "models"),
            "https://example.com/v1/models"
        );
        assert_eq!(
            build_models_url("https://example.com/v1/", "/models"),
            "https://example.com/v1/models"
        );
        assert_eq!(
            build_models_url("https://example.com/v1", "v1/models/"),
            "https://example.com/v1/v1/models"
        );
    }
}
