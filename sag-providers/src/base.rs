use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use reqwest::RequestBuilder;
use reqwest::header::CONTENT_TYPE;
use sag_config::{KeyStrategy, ProviderConfig};
use sag_core::{AppError, ChatMessage};
use tracing::{debug, trace, warn};

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
    role_mappings: HashMap<String, String>,
    /// Whether to inject `cache_control` breakpoints for Anthropic providers.
    pub anthropic_cache_control: bool,
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
            role_mappings: config.role_mappings.clone(),
            anthropic_cache_control: config.anthropic_cache_control,
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
        debug!(url, "sending upstream request");
        trace!(url, body = %body_str, "upstream request body");
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
    ///
    /// `timeout_secs` overrides the client-level timeout for this request
    /// when `Some`. When `None`, the client-level timeout applies.
    pub async fn send_json(
        &self,
        url: &str,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
        extra_headers: &[(&str, &str)],
        timeout_secs: Option<u64>,
    ) -> sag_core::Result<String> {
        let mut request = self.build_request(url, body, target_extra, extra_headers);
        if let Some(secs) = timeout_secs {
            request = request.timeout(std::time::Duration::from_secs(secs));
        }
        let response = request.send().await.map_err(AppError::from)?;
        let body_text = self.check_upstream_error(response).await?;
        trace!(response_body = %body_text, "upstream response body");
        Ok(body_text)
    }

    /// Send a JSON POST request for streaming and return the `Response`.
    ///
    /// Combines `build_request` + `send` + `ensure_success`. Used by
    /// `chat_stream()`.
    ///
    /// `timeout_secs` overrides the client-level timeout for this request
    /// when `Some`. When `None`, the client-level timeout applies.
    pub async fn send_json_stream(
        &self,
        url: &str,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
        extra_headers: &[(&str, &str)],
        timeout_secs: Option<u64>,
    ) -> sag_core::Result<reqwest::Response> {
        let mut request = self.build_request(url, body, target_extra, extra_headers);
        if let Some(secs) = timeout_secs {
            request = request.timeout(std::time::Duration::from_secs(secs));
        }
        let response = request.send().await.map_err(AppError::from)?;
        let response = self.ensure_success(response).await?;
        trace!("upstream stream connection established");
        debug!(
            status = response.status().as_u16(),
            "upstream stream established"
        );
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

    /// Apply role mappings to a mutable slice of chat messages.
    ///
    /// Messages whose role matches a key in `role_mappings` get their
    /// role replaced with the corresponding value. Non-matching messages
    /// are unchanged. Early-returns when role_mappings is empty.
    /// Returns `true` if any role mappings are configured.
    pub fn has_role_mappings(&self) -> bool {
        !self.role_mappings.is_empty()
    }

    pub fn apply_role_mappings(&self, messages: &mut [ChatMessage]) {
        if self.role_mappings.is_empty() {
            return;
        }
        for msg in messages.iter_mut() {
            if let Some(new_role) = self.role_mappings.get(&msg.role) {
                msg.role = new_role.clone();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_base(role_mappings: HashMap<String, String>) -> ProviderBase {
        let config = ProviderConfig {
            api_keys: vec!["sk-test".into()],
            provider_type: sag_config::ProviderType::OpenAI,
            key_strategy: sag_config::KeyStrategy::FillFirst,
            base_url: "https://example.com".into(),
            models_endpoint: "models".into(),
            timeout_secs: 30,
            headers: HashMap::new(),
            body_overrides: HashMap::new(),
            enabled: true,
            enabled_models: Vec::new(),
            role_mappings,
            anthropic_cache_control: true,
        };
        ProviderBase::new("test".into(), &config)
    }

    fn msg(role: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: serde_json::Value::String("content".into()),
            name: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn empty_mappings_does_nothing() {
        let base = make_base(HashMap::new());
        let mut msgs = vec![msg("developer")];
        base.apply_role_mappings(&mut msgs);
        assert_eq!(msgs[0].role, "developer");
    }

    #[test]
    fn replaces_matching_role() {
        let base = make_base([("developer".into(), "system".into())].into());
        let mut msgs = vec![msg("developer")];
        base.apply_role_mappings(&mut msgs);
        assert_eq!(msgs[0].role, "system");
    }

    #[test]
    fn skips_non_matching_roles() {
        let base = make_base([("developer".into(), "system".into())].into());
        let mut msgs = vec![msg("user"), msg("assistant")];
        base.apply_role_mappings(&mut msgs);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
    }

    #[test]
    fn partial_map_leaves_others_unchanged() {
        let base = make_base([("developer".into(), "system".into())].into());
        let mut msgs = vec![msg("developer"), msg("user")];
        base.apply_role_mappings(&mut msgs);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[1].role, "user");
    }
}
