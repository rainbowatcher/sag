use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use reqwest::RequestBuilder;
use reqwest::header::CONTENT_TYPE;
use sag_config::{KeyStrategy, ProviderConfig};
use sag_core::AppError;
use tracing::{debug, warn};

use crate::{apply_body_overrides, apply_headers};

/// Shared state and helper methods for all provider adapters.
///
/// Each provider struct (e.g. `OpenAiProvider`, `AnthropicProvider`)
/// composes this as a field and delegates common operations to it.
pub struct ProviderBase {
    /// Human-readable provider name (e.g. `"openai"`, `"anthropic"`).
    pub name: String,
    api_keys: Vec<String>,
    key_strategy: KeyStrategy,
    key_counter: AtomicUsize,
    /// The upstream base URL (without trailing slash).
    base_url: String,
    /// Reusable HTTP client with timeout.
    pub http_client: reqwest::Client,
    headers: HashMap<String, String>,
    body_overrides: HashMap<String, serde_json::Value>,
}

impl ProviderBase {
    /// Create a new provider base from configuration.
    pub fn new(name: String, config: &ProviderConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .expect("failed to build reqwest client");

        assert!(
            !config.api_keys.is_empty(),
            "provider '{}': api_keys is empty",
            name
        );
        let api_keys = config.api_keys.clone();

        Self {
            name,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_keys,
            key_strategy: config.key_strategy,
            key_counter: AtomicUsize::new(0),
            http_client,
            headers: config.headers.clone(),
            body_overrides: config.body_overrides.clone(),
        }
    }

    /// Select the current API key based on the configured strategy.
    pub fn current_key(&self) -> &str {
        match self.key_strategy {
            KeyStrategy::FillFirst => &self.api_keys[0],
            KeyStrategy::RoundRobin => {
                let idx = self.key_counter.fetch_add(1, Ordering::Relaxed) % self.api_keys.len();
                &self.api_keys[idx]
            }
        }
    }

    /// Build the full upstream URL from base URL and an API endpoint path.
    ///
    /// Ensures exactly one `/` between base and path, regardless of
    /// whether either has a leading/trailing slash.
    pub(crate) fn build_url(&self, api_endpoint: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = api_endpoint.trim_start_matches('/');
        format!("{}/{}", base, path)
    }

    /// Build a reqwest POST request with default + custom headers and body
    /// overrides applied.
    ///
    /// `Content-Type: application/json` is set automatically.
    /// `extra_headers` are applied first, then `self.headers`, then
    /// `self.body_overrides`, then `target_extra`.
    pub(crate) fn build_request(
        &self,
        url: &str,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
        extra_headers: &[(&str, &str)],
    ) -> RequestBuilder {
        let mut req = self
            .http_client
            .post(url)
            .header(CONTENT_TYPE, "application/json");
        for (k, v) in extra_headers {
            req = req.header(*k, *v);
        }
        let req = apply_headers(req, &self.headers);
        let body = apply_body_overrides(body, &self.body_overrides);
        let body = apply_body_overrides(body, target_extra);
        let body_str = serde_json::to_string(&body).unwrap_or_default();
        debug!(provider = %self.name, url, body = %body_str, "upstream request");
        req.json(&body)
    }

    /// Check upstream response status for **non-streaming** requests.
    ///
    /// On success returns the response body text for further parsing.
    /// On error returns `AppError::Upstream`.
    pub(crate) async fn check_upstream_error(
        &self,
        response: reqwest::Response,
    ) -> sag_core::Result<String> {
        let status = response.status().as_u16();
        let body_text = response.text().await.map_err(AppError::from)?;

        if !(200..300).contains(&status) {
            warn!(status, %body_text, "upstream returned error");
            return Err(AppError::Upstream {
                provider: self.name.clone(),
                status,
                body: body_text,
            });
        }

        Ok(body_text)
    }

    /// Send a JSON POST request and return the response body text on success.
    ///
    /// Combines `build_request` + `send` + `check_upstream_error`. Used by
    /// `chat()` and `chat_image()`.
    pub async fn send_json(
        &self,
        url: &str,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
        extra_headers: &[(&str, &str)],
    ) -> sag_core::Result<String> {
        let response = self
            .build_request(url, body, target_extra, extra_headers)
            .send()
            .await
            .map_err(AppError::from)?;
        let body_text = self.check_upstream_error(response).await?;
        debug!(provider = %self.name, response_body = %body_text, "upstream response");
        Ok(body_text)
    }

    /// Send a JSON POST request for streaming and return the `Response`.
    ///
    /// Combines `build_request` + `send` + `ensure_success`. Used by
    /// `chat_stream()`.
    pub async fn send_json_stream(
        &self,
        url: &str,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
        extra_headers: &[(&str, &str)],
    ) -> sag_core::Result<reqwest::Response> {
        let response = self
            .build_request(url, body, target_extra, extra_headers)
            .send()
            .await
            .map_err(AppError::from)?;
        let response = self.ensure_success(response).await?;
        debug!(provider = %self.name, status = response.status().as_u16(), "upstream stream established");
        Ok(response)
    }

    /// Check upstream response status for **streaming** requests.
    ///
    /// On success, returns the `Response` so the caller can consume the byte
    /// stream. On error, consumes the response to read the error body and
    /// returns `AppError::Upstream`.
    pub(crate) async fn ensure_success(
        &self,
        response: reqwest::Response,
    ) -> sag_core::Result<reqwest::Response> {
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.map_err(AppError::from)?;
            warn!(status, %body, "upstream returned error on stream");
            return Err(AppError::Upstream {
                provider: self.name.clone(),
                status,
                body,
            });
        }
        Ok(response)
    }
}
