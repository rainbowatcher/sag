use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use reqwest::RequestBuilder;
use sag_config::{KeyStrategy, ProviderConfig};
use sag_core::{AppError, ChatMessage};
use tracing::{debug, trace, warn};

use crate::{apply_body_overrides, apply_body_strip, apply_headers};

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
    /// Top-level fields to strip from the upstream body before sending.
    /// Applied after all override layers, guaranteeing absence in the
    /// final body.
    strip_fields: Vec<String>,
    /// Per-model strip fields keyed by real_model name.
    model_strip_fields: HashMap<String, Vec<String>>,
    role_mappings: HashMap<String, String>,
    /// Whether to inject `cache_control` breakpoints for Anthropic providers.
    pub anthropic_cache_control: bool,
    /// Maximum idle time between stream chunks before the connection is
    /// terminated. `None` = wait indefinitely.
    chunk_timeout_secs: Option<u64>,
    /// Maximum time to wait for the upstream response headers
    /// (time-to-first-byte) on streaming requests. `None` = use the
    /// crate default. Only used by the streaming path; once headers
    /// arrive this no longer applies.
    ttfb_timeout_secs: Option<u64>,
}

impl ProviderBase {
    /// Create a new provider base from configuration.
    pub fn new(name: String, config: &ProviderConfig) -> sag_core::Result<Self> {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .map_err(|e| AppError::Internal(format!("failed to build reqwest client: {e}")))?;

        if config.api_keys.is_empty() {
            return Err(AppError::BadRequest(format!(
                "provider '{}': api_keys is empty",
                name
            )));
        }
        let api_keys = config.api_keys.clone();

        Ok(Self {
            name,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_keys,
            key_strategy: config.key_strategy,
            key_counter: AtomicUsize::new(0),
            http_client,
            headers: config.headers.clone(),
            body_overrides: config.body_overrides.clone(),
            strip_fields: config.strip_fields.clone(),
            model_strip_fields: config.model_strip_fields.clone(),
            role_mappings: config.role_mappings.clone(),
            anthropic_cache_control: config.anthropic_cache_control,
            chunk_timeout_secs: config.chunk_timeout_secs,
            ttfb_timeout_secs: config.ttfb_timeout_secs,
        })
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

    /// Return the configured stream-chunk idle timeout, if any.
    pub fn chunk_timeout_secs(&self) -> Option<u64> {
        self.chunk_timeout_secs
    }

    /// Return the effective time-to-first-byte timeout for streaming
    /// requests, falling back to the crate default when unset.
    pub fn ttfb_timeout_secs(&self) -> u64 {
        self.ttfb_timeout_secs
            .unwrap_or_else(sag_config::default_ttfb_timeout_secs)
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
        let req = self.http_client.post(url);
        let req = apply_headers(req, extra_headers, &self.headers);
        let body = apply_body_overrides(body, &self.body_overrides);
        let body = apply_body_overrides(body, target_extra);

        // Union provider-level and model-level strip_fields (if real_model matches).
        // Note: By contract, body["model"] has already been substituted to real_model by the caller.
        let effective_strip = if let Some(model_name) = body.get("model").and_then(|v| v.as_str()) {
            if let Some(model_strips) = self.model_strip_fields.get(model_name) {
                let mut combined = self.strip_fields.clone();
                combined.extend(model_strips.clone());
                combined
            } else {
                self.strip_fields.clone()
            }
        } else {
            self.strip_fields.clone()
        };

        let body = apply_body_strip(body, &effective_strip);
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
    /// Unlike [`send_json`], streaming does **not** apply a total
    /// request deadline (that would truncate a healthy long-lived
    /// stream). Instead, only the time-to-first-byte phase — connecting
    /// through receipt of the response headers — is bounded by
    /// [`ProviderBase::ttfb_timeout_secs`]. `reqwest`'s `send()` future
    /// resolves as soon as the response headers arrive and before the
    /// body is consumed, so wrapping it in `tokio::time::timeout`
    /// precisely bounds TTFB without constraining the subsequent body
    /// stream. Once headers arrive, the body is governed solely by the
    /// per-chunk idle timeout in `chat_stream()`.
    ///
    /// A TTFB timeout maps to a retryable [`AppError::TtfbTimeout`] so the
    /// dispatcher can fall over to the next target.
    pub async fn send_json_stream(
        &self,
        url: &str,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
        extra_headers: &[(&str, &str)],
    ) -> sag_core::Result<reqwest::Response> {
        let request = self.build_request(url, body, target_extra, extra_headers);
        let ttfb = std::time::Duration::from_secs(self.ttfb_timeout_secs());
        let response = match tokio::time::timeout(ttfb, request.send()).await {
            Ok(result) => result.map_err(AppError::from)?,
            Err(_elapsed) => {
                warn!(
                    ttfb_timeout_secs = ttfb.as_secs(),
                    "upstream time-to-first-byte timeout"
                );
                return Err(AppError::TtfbTimeout {
                    provider: self.name.clone(),
                    secs: ttfb.as_secs(),
                });
            }
        };
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
            strip_fields: vec![],
            model_strip_fields: HashMap::new(),
            model_overrides: HashMap::new(),
            enabled: true,
            enabled_models: Vec::new(),
            role_mappings,
            anthropic_cache_control: true,
            chunk_timeout_secs: None,
            ttfb_timeout_secs: None,
        };
        ProviderBase::new("test".into(), &config).unwrap()
    }

    fn msg(role: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: serde_json::Value::String("content".into()),
            name: None,
            extra: serde_json::Map::new(),
        }
    }

    fn make_base_with(base_url: String, ttfb_timeout_secs: Option<u64>) -> ProviderBase {
        let config = ProviderConfig {
            api_keys: vec!["sk-test".into()],
            provider_type: sag_config::ProviderType::OpenAI,
            key_strategy: sag_config::KeyStrategy::FillFirst,
            base_url,
            models_endpoint: "models".into(),
            timeout_secs: 30,
            headers: HashMap::new(),
            body_overrides: HashMap::new(),
            strip_fields: vec![],
            model_strip_fields: HashMap::new(),
            model_overrides: HashMap::new(),
            enabled: true,
            enabled_models: Vec::new(),
            role_mappings: HashMap::new(),
            anthropic_cache_control: true,
            chunk_timeout_secs: None,
            ttfb_timeout_secs,
        };
        ProviderBase::new("test".into(), &config).unwrap()
    }

    fn make_base_with_body_overrides(
        body_overrides: HashMap<String, serde_json::Value>,
    ) -> ProviderBase {
        let config = ProviderConfig {
            api_keys: vec!["sk-test".into()],
            provider_type: sag_config::ProviderType::OpenAI,
            key_strategy: sag_config::KeyStrategy::FillFirst,
            base_url: "https://example.com".into(),
            models_endpoint: "models".into(),
            timeout_secs: 30,
            headers: HashMap::new(),
            body_overrides,
            strip_fields: vec![],
            model_strip_fields: HashMap::new(),
            model_overrides: HashMap::new(),
            enabled: true,
            enabled_models: Vec::new(),
            role_mappings: HashMap::new(),
            anthropic_cache_control: true,
            chunk_timeout_secs: None,
            ttfb_timeout_secs: None,
        };
        ProviderBase::new("test".into(), &config).unwrap()
    }

    fn extract_json_body(req: reqwest::Request) -> serde_json::Value {
        let body_bytes = req.body().unwrap().as_bytes().unwrap();
        serde_json::from_slice(body_bytes).unwrap()
    }

    #[test]
    fn ttfb_timeout_secs_falls_back_to_default() {
        let base = make_base_with("https://example.com".into(), None);
        assert_eq!(base.ttfb_timeout_secs(), 60);
        let base = make_base_with("https://example.com".into(), Some(5));
        assert_eq!(base.ttfb_timeout_secs(), 5);
    }

    /// Spawn a one-shot TCP server. `handler` receives the accepted stream
    /// after the request bytes are (best-effort) drained, and drives the
    /// response however the test needs. Returns the bound `http://` base URL.
    async fn spawn_server<F, Fut>(handler: F) -> String
    where
        F: FnOnce(tokio::net::TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Drain the request headers so the client's send side completes.
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            handler(stream).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn send_json_stream_times_out_before_headers() {
        // Server accepts the connection but never sends response headers.
        let base_url = spawn_server(|stream| async move {
            // Hold the connection open well past the TTFB budget.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            drop(stream);
        })
        .await;

        let base = make_base_with(base_url.clone(), Some(1));
        let url = base.build_url("chat/completions");
        let start = std::time::Instant::now();
        let result = base
            .send_json_stream(&url, serde_json::json!({}), &HashMap::new(), &[])
            .await;
        let elapsed = start.elapsed();

        match result {
            Err(sag_core::AppError::TtfbTimeout { secs, .. }) => assert_eq!(secs, 1),
            other => panic!("expected TtfbTimeout, got: {other:?}"),
        }
        // Fired at ~1s, well before any total-request deadline.
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "TTFB timeout fired too late: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn send_json_stream_does_not_time_out_on_slow_body() {
        use tokio::io::AsyncWriteExt;
        // Headers arrive immediately; the body then dribbles out over a
        // span longer than the TTFB budget. TTFB must not truncate it.
        let base_url = spawn_server(|mut stream| async move {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            stream.flush().await.unwrap();
            for _ in 0..3 {
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                stream.write_all(b"data: {}\n\n").await.unwrap();
                stream.flush().await.unwrap();
            }
            drop(stream);
        })
        .await;

        // TTFB budget (1s) is shorter than the total body duration (~1.2s).
        let base = make_base_with(base_url.clone(), Some(1));
        let url = base.build_url("chat/completions");
        let response = base
            .send_json_stream(&url, serde_json::json!({}), &HashMap::new(), &[])
            .await
            .expect("headers arrived within TTFB budget");

        // Fully drain the slow body; the absence of any total deadline means
        // this completes without a timeout error.
        use futures_util::StreamExt;
        let mut stream = response.bytes_stream();
        let mut total = 0usize;
        while let Some(chunk) = stream.next().await {
            total += chunk.expect("body chunk read without timeout").len();
        }
        assert!(total > 0, "expected to receive streamed body bytes");
    }

    #[test]
    fn target_extra_overrides_body_override_for_shared_key() {
        // body_overrides sets temperature=0.5;
        // target_extra (already merged model_override + target.extra) sets temperature=0.7.
        // The target_extra value must win.
        let mut body_overrides = HashMap::new();
        body_overrides.insert("temperature".into(), serde_json::json!(0.5));

        let mut target_extra = HashMap::new();
        target_extra.insert("temperature".into(), serde_json::json!(0.7));

        let base = make_base_with_body_overrides(body_overrides);
        let req = base
            .build_request(
                "https://example.com/v1/chat/completions",
                serde_json::json!({ "model": "gpt-4o", "temperature": 1.0 }),
                &target_extra,
                &[],
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        assert_eq!(
            body.get("temperature").and_then(|v| v.as_f64()),
            Some(0.7),
            "target_extra should override body_overrides for shared key"
        );
        assert_eq!(
            body.get("model").unwrap(),
            "gpt-4o",
            "client key should be preserved"
        );
    }

    #[test]
    fn body_override_alone_overrides_client_key() {
        // Only body_overrides is set; it should override the client-provided value.
        let mut body_overrides = HashMap::new();
        body_overrides.insert("temperature".into(), serde_json::json!(0.5));

        let base = make_base_with_body_overrides(body_overrides);
        let req = base
            .build_request(
                "https://example.com/v1/chat/completions",
                serde_json::json!({ "model": "gpt-4o", "temperature": 1.0 }),
                &HashMap::new(),
                &[],
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        assert_eq!(
            body.get("temperature").and_then(|v| v.as_f64()),
            Some(0.5),
            "body_overrides alone should override client key"
        );
        assert_eq!(body.get("model").unwrap(), "gpt-4o");
    }

    #[test]
    fn target_extra_alone_overrides_client_key() {
        // Only target_extra is set (simulating merged model_overrides + target.extra);
        // it should override the client-provided value.
        let mut target_extra = HashMap::new();
        target_extra.insert("temperature".into(), serde_json::json!(0.7));

        let base = make_base_with_body_overrides(HashMap::new());
        let req = base
            .build_request(
                "https://example.com/v1/chat/completions",
                serde_json::json!({ "model": "gpt-4o", "temperature": 1.0 }),
                &target_extra,
                &[],
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        assert_eq!(
            body.get("temperature").and_then(|v| v.as_f64()),
            Some(0.7),
            "target_extra alone should override client key"
        );
        assert_eq!(body.get("model").unwrap(), "gpt-4o");
    }

    #[test]
    fn three_tier_same_key_body_override_then_model_override_then_target_extra() {
        // Simulates the full three-tier chain end-to-end:
        //   client body: { temperature: 1.0 }
        //   → body_overrides: { temperature: 0.5 }         (tier 1)
        //   → merged target_extra:
        //       model_override: { temperature: 0.6 }       (tier 2)
        //       target.extra:   { temperature: 0.7 }       (tier 3)
        //       merged result:  { temperature: 0.7 }
        //   → final body: { temperature: 0.7 }
        let mut body_overrides = HashMap::new();
        body_overrides.insert("temperature".into(), serde_json::json!(0.5));

        let mut target_extra = HashMap::new();
        target_extra.insert("temperature".into(), serde_json::json!(0.7));

        let base = make_base_with_body_overrides(body_overrides);
        let req = base
            .build_request(
                "https://example.com/v1/chat/completions",
                serde_json::json!({ "model": "gpt-4o", "temperature": 1.0 }),
                &target_extra,
                &[],
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        assert_eq!(
            body.get("temperature").and_then(|v| v.as_f64()),
            Some(0.7),
            "in three-tier chain, target.extra (via merged target_extra) should win"
        );
    }

    #[test]
    fn distinct_keys_across_tiers_accumulate() {
        // body_overrides has max_tokens; target_extra (merged) has temperature and top_p.
        // All keys should be present in the final body.
        let mut body_overrides = HashMap::new();
        body_overrides.insert("max_tokens".into(), serde_json::json!(4096));

        let mut target_extra = HashMap::new();
        target_extra.insert("temperature".into(), serde_json::json!(0.7));
        target_extra.insert("top_p".into(), serde_json::json!(0.9));

        let base = make_base_with_body_overrides(body_overrides);
        let req = base
            .build_request(
                "https://example.com/v1/chat/completions",
                serde_json::json!({ "model": "gpt-4o" }),
                &target_extra,
                &[],
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        assert_eq!(
            body.get("max_tokens").and_then(|v| v.as_i64()),
            Some(4096),
            "body_overrides-only key should be present"
        );
        assert_eq!(
            body.get("temperature").and_then(|v| v.as_f64()),
            Some(0.7),
            "target_extra-only key should be present"
        );
        assert_eq!(
            body.get("top_p").and_then(|v| v.as_f64()),
            Some(0.9),
            "target_extra-only key should be present"
        );
        assert_eq!(
            body.get("model").unwrap(),
            "gpt-4o",
            "client key should be preserved"
        );
    }

    #[test]
    fn nested_object_replaced_wholly_by_target_extra() {
        // body_overrides sets thinking = { type: "enabled" };
        // target_extra sets thinking = { budget_tokens: 1600 }.
        // The target_extra nested object replaces the body_overrides one entirely.
        let mut body_overrides = HashMap::new();
        body_overrides.insert("thinking".into(), serde_json::json!({ "type": "enabled" }));

        let mut target_extra = HashMap::new();
        target_extra.insert(
            "thinking".into(),
            serde_json::json!({ "budget_tokens": 1600 }),
        );

        let base = make_base_with_body_overrides(body_overrides);
        let req = base
            .build_request(
                "https://example.com/v1/chat/completions",
                serde_json::json!({ "model": "gpt-4o" }),
                &target_extra,
                &[],
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        let thinking = body.get("thinking").unwrap();
        assert_eq!(
            thinking.get("budget_tokens").and_then(|v| v.as_i64()),
            Some(1600),
            "target-extra nested object should win"
        );
        assert!(
            thinking.get("type").is_none(),
            "nested object should be replaced wholly, not deep-merged"
        );
    }

    #[test]
    fn empty_overrides_short_circuits() {
        // Both body_overrides and target_extra are empty.
        // The original body should pass through unchanged.
        let base = make_base_with_body_overrides(HashMap::new());
        let original = serde_json::json!({ "model": "gpt-4o", "temperature": 1.0 });

        let req = base
            .build_request(
                "https://example.com/v1/chat/completions",
                original.clone(),
                &HashMap::new(),
                &[],
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        assert_eq!(body, original, "empty overrides should not mutate the body");
    }

    #[test]
    fn strip_fields_removes_field_after_all_overrides() {
        // strip_fields removes chat_template_kwargs even after
        // body_overrides and target_extra insert it.
        let mut body_overrides: HashMap<String, serde_json::Value> = HashMap::new();
        body_overrides.insert("temperature".into(), serde_json::json!(0.5));

        let mut target_extra = HashMap::new();
        target_extra.insert(
            "chat_template_kwargs".into(),
            serde_json::json!({ "enable_thinking": false }),
        );

        let config = ProviderConfig {
            api_keys: vec!["sk-test".into()],
            provider_type: sag_config::ProviderType::OpenAI,
            key_strategy: sag_config::KeyStrategy::FillFirst,
            base_url: "https://example.com".into(),
            models_endpoint: "models".into(),
            timeout_secs: 30,
            headers: HashMap::new(),
            body_overrides,
            strip_fields: vec!["chat_template_kwargs".to_string()],
            model_strip_fields: HashMap::new(),
            model_overrides: HashMap::new(),
            enabled: true,
            enabled_models: Vec::new(),
            role_mappings: HashMap::new(),
            anthropic_cache_control: true,
            chunk_timeout_secs: None,
            ttfb_timeout_secs: None,
        };
        let base = ProviderBase::new("test".into(), &config).unwrap();
        let req = base
            .build_request(
                "https://example.com/v1/chat/completions",
                serde_json::json!({ "model": "gpt-4o" }),
                &target_extra,
                &[],
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        // chat_template_kwargs must be absent despite target_extra inserting it
        assert!(
            body.get("chat_template_kwargs").is_none(),
            "strip_fields must remove chat_template_kwargs even if target_extra inserted it"
        );
        // Other overrides still apply
        assert_eq!(
            body.get("temperature").and_then(|v| v.as_f64()),
            Some(0.5),
            "body_overrides should still be applied"
        );
        assert_eq!(body.get("model").unwrap(), "gpt-4o");
    }

    #[test]
    fn model_strip_fields_combines_with_provider_strip_fields() {
        let mut model_strip = HashMap::new();
        model_strip.insert(
            "glm-5.2".to_string(),
            vec!["chat_template_kwargs".to_string()],
        );

        let config = ProviderConfig {
            api_keys: vec!["sk-test".into()],
            provider_type: sag_config::ProviderType::OpenAI,
            key_strategy: sag_config::KeyStrategy::FillFirst,
            base_url: "https://example.com".into(),
            models_endpoint: "models".into(),
            timeout_secs: 30,
            headers: HashMap::new(),
            body_overrides: HashMap::new(),
            strip_fields: vec!["global_bad_field".to_string()],
            model_strip_fields: model_strip,
            model_overrides: HashMap::new(),
            enabled: true,
            enabled_models: Vec::new(),
            role_mappings: HashMap::new(),
            anthropic_cache_control: true,
            chunk_timeout_secs: None,
            ttfb_timeout_secs: None,
        };
        let base = ProviderBase::new("test".into(), &config).unwrap();

        // Request for glm-5.2 -> strips both global_bad_field and chat_template_kwargs
        let req = base
            .build_request(
                "https://example.com/v1/chat/completions",
                serde_json::json!({
                    "model": "glm-5.2",
                    "global_bad_field": 123,
                    "chat_template_kwargs": { "foo": "bar" },
                    "keep_me": "yes"
                }),
                &HashMap::new(),
                &[],
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        assert!(body.get("global_bad_field").is_none());
        assert!(body.get("chat_template_kwargs").is_none());
        assert_eq!(body.get("keep_me").unwrap(), "yes");

        // Request for gpt-4o -> only strips global_bad_field
        let req2 = base
            .build_request(
                "https://example.com/v1/chat/completions",
                serde_json::json!({
                    "model": "gpt-4o",
                    "global_bad_field": 123,
                    "chat_template_kwargs": { "foo": "bar" },
                }),
                &HashMap::new(),
                &[],
            )
            .build()
            .unwrap();

        let body2 = extract_json_body(req2);
        assert!(body2.get("global_bad_field").is_none());
        assert!(body2.get("chat_template_kwargs").is_some());
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
