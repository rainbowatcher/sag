use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use sag_config::KeyStrategy;
use sag_core::{
    AppError, ChatCompletionRequest, ChatCompletionResponse, ImageGenerationRequest,
    ImageGenerationResponse, StreamChunk,
};
use tokio::sync::mpsc;
use tracing::{debug, error, instrument, warn};

use crate::{LlmProvider, apply_body_overrides, apply_headers, drain_sse_frames};

/// OpenAI-compatible provider adapter.
///
/// Uses the OpenAI Chat Completions API (and any drop-in compatible endpoints).
pub struct OpenAiProvider {
    name: String,
    base_url: String,
    api_keys: Vec<String>,
    key_strategy: KeyStrategy,
    key_counter: AtomicUsize,
    http_client: reqwest::Client,
    headers: HashMap<String, String>,
    body_overrides: HashMap<String, serde_json::Value>,
}

impl OpenAiProvider {
    /// Create a new OpenAI provider from configuration.
    pub fn new(name: String, config: &sag_config::ProviderConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .expect("failed to build reqwest client");

        debug_assert!(!config.api_keys.is_empty(), "provider '{}': api_keys is empty", name);
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
    fn current_key(&self) -> &str {
        match self.key_strategy {
            KeyStrategy::FillFirst => &self.api_keys[0],
            KeyStrategy::RoundRobin => {
                let idx = self.key_counter.fetch_add(1, Ordering::Relaxed) % self.api_keys.len();
                &self.api_keys[idx]
            }
        }
    }

    /// Build the full upstream URL from the base URL and an API endpoint path.
    fn build_url(&self, api_endpoint: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = api_endpoint.trim_start_matches('/');
        format!("{}/{}", base, path)
    }

    /// Build a reqwest request with default + custom headers and optional body overrides.
    fn build_request(
        &self,
        url: &str,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
    ) -> reqwest::RequestBuilder {
        let req = self
            .http_client
            .post(url)
            .header(AUTHORIZATION, format!("Bearer {}", self.current_key()))
            .header(CONTENT_TYPE, "application/json");

        let req = apply_headers(req, &self.headers);

        let body = apply_body_overrides(body, &self.body_overrides);
        let body = apply_body_overrides(body, target_extra);
        req.json(&body)
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &str {
        &self.name
    }

    #[instrument(skip(self, request), fields(provider = %self.name, model = %real_model))]
    async fn chat(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<ChatCompletionResponse> {
        // Clone request and override model with the real model name
        let mut upstream_request = request.clone();
        upstream_request.model = real_model.to_string();

        if !request.extra.is_empty() {
            let keys: Vec<&str> = request.extra.keys().map(|s| s.as_str()).collect();
            debug!(provider = %self.name, extra = ?keys, "forwarding non-standard extra parameters");
        }

        let url = self.build_url(api_endpoint);

        let body = serde_json::to_value(&upstream_request)
            .map_err(|e| AppError::Internal(format!("failed to serialize request: {e}")))?;

        let response = self
            .build_request(&url, body, target_extra)
            .send()
            .await
            .map_err(AppError::from)?;

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

        let completion: ChatCompletionResponse = serde_json::from_str(&body_text)
            .map_err(|e| AppError::Internal(format!("failed to parse upstream response: {e}")))?;

        Ok(completion)
    }

    #[instrument(skip(self, request), fields(provider = %self.name, model = %real_model))]
    async fn chat_stream(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        let mut upstream_request = request.clone();
        upstream_request.model = real_model.to_string();
        upstream_request.stream = Some(true);

        if !request.extra.is_empty() {
            let keys: Vec<&str> = request.extra.keys().map(|s| s.as_str()).collect();
            debug!(provider = %self.name, extra = ?keys, "forwarding non-standard extra parameters");
        }

        let url = self.build_url(api_endpoint);

        let body = serde_json::to_value(&upstream_request)
            .map_err(|e| AppError::Internal(format!("failed to serialize request: {e}")))?;

        let response = self
            .build_request(&url, body, target_extra)
            .send()
            .await
            .map_err(AppError::from)?;

        let status = response.status();

        if !status.is_success() {
            let status_code = status.as_u16();
            let body_text = response.text().await.map_err(AppError::from)?;
            warn!(status = status_code, %body_text, "upstream returned error on stream");
            return Err(AppError::Upstream {
                provider: self.name.clone(),
                status: status_code,
                body: body_text,
            });
        }

        // Spawn a background task that reads the HTTP response body, parses SSE
        // frames, and sends chunks into the channel. The returned stream reads
        // from the receiver side.
        let (tx, rx) = mpsc::unbounded_channel::<StreamChunk>();

        tokio::spawn(async move {
            let mut byte_stream = response.bytes_stream();
            let mut buffer: Vec<u8> = Vec::new();
            let mut had_error = false;

            loop {
                // Stop reading if the client has disconnected
                if tx.is_closed() {
                    break;
                }

                match byte_stream.next().await {
                    Some(Ok(chunk)) => {
                        buffer.extend_from_slice(&chunk);
                        drain_sse_frames(&mut buffer, &tx);
                    }
                    Some(Err(e)) => {
                        error!(?e, "error reading upstream stream chunk");
                        if !tx.is_closed() {
                            let _ = tx.send(StreamChunk::Error(e.to_string()));
                        }
                        had_error = true;
                        break;
                    }
                    None => break,
                }
            }

            // Flush any remaining data in the buffer
            if !buffer.is_empty() {
                drain_sse_frames(&mut buffer, &tx);
            }

            if !had_error {
                let _ = tx.send(StreamChunk::Done);
            }
            // On error the sender is dropped; the router sees None after Error.
        });

        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|chunk| (chunk, rx))
        });

        Ok(Box::pin(stream))
    }

    #[instrument(skip(self, request), fields(provider = %self.name, model = %real_model))]
    async fn chat_image(
        &self,
        request: &ImageGenerationRequest,
        real_model: &str,
        target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<ImageGenerationResponse> {
        let mut upstream = request.clone();
        upstream.model = real_model.to_string();

        let url = self.build_url("/images/generations");

        let body = serde_json::to_value(&upstream)
            .map_err(|e| AppError::Internal(format!("failed to serialize request: {e}")))?;

        let response = self
            .build_request(&url, body, target_extra)
            .send()
            .await
            .map_err(AppError::from)?;

        let status = response.status().as_u16();
        let body_text = response.text().await.map_err(AppError::from)?;

        if !(200..300).contains(&status) {
            warn!(status, %body_text, "upstream image generation returned error");
            return Err(AppError::Upstream {
                provider: self.name.clone(),
                status,
                body: body_text,
            });
        }

        serde_json::from_str(&body_text).map_err(|e| {
            AppError::Internal(format!("failed to parse image generation response: {e}"))
        })
    }
}
