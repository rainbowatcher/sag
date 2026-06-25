use std::collections::HashMap;
use std::pin::Pin;

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use sag_core::{
    AppError, ChatCompletionRequest, ChatCompletionResponse, ImageGenerationRequest,
    ImageGenerationResponse, StreamChunk,
};
use tokio::sync::mpsc;
use tracing::{error, instrument, trace};

use crate::{LlmProvider, ProviderBase, drain_sse_frames};

/// OpenAI-compatible provider adapter.
///
/// Uses the OpenAI Chat Completions API (and any drop-in compatible endpoints).
pub struct OpenAiProvider {
    base: ProviderBase,
}

impl OpenAiProvider {
    /// Create a new OpenAI provider from configuration.
    pub fn new(name: String, config: &sag_config::ProviderConfig) -> Self {
        Self {
            base: ProviderBase::new(name, config),
        }
    }

    fn auth_header(&self) -> (&str, String) {
        (
            "Authorization",
            format!("Bearer {}", self.base.current_key()),
        )
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &str {
        &self.base.name
    }

    #[instrument(skip_all, fields(provider = %self.base.name, model = %real_model))]
    async fn chat(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<ChatCompletionResponse> {
        let mut upstream_request = request.clone();
        self.base
            .apply_role_mappings(&mut upstream_request.messages);
        upstream_request.model = real_model.to_string();

        if !request.extra.is_empty() {
            let keys: Vec<&str> = request.extra.keys().map(|s| s.as_str()).collect();
            trace!(extra = ?keys, "forwarding non-standard extra parameters");
        }

        let url = self.base.build_url(api_endpoint);

        let body = serde_json::to_value(&upstream_request)
            .map_err(|e| AppError::Internal(format!("failed to serialize request: {e}")))?;

        let (auth_name, auth_value) = self.auth_header();
        let body_text = self
            .base
            .send_json(&url, body, target_extra, &[(auth_name, &auth_value)])
            .await?;

        let mut completion: ChatCompletionResponse = serde_json::from_str(&body_text)
            .map_err(|e| AppError::Internal(format!("failed to parse upstream response: {e}")))?;

        // Patch nested cache/reasoning token fields from the raw usage
        // object. The derived Deserialize for Usage sees a flat struct,
        // so `prompt_tokens_details.cached_tokens` and
        // `completion_tokens_details.reasoning_tokens` are silently ignored.
        // This post-deserialization patch lifts them into the top-level
        // Usage fields without requiring a custom deserializer.
        if let Some(ref mut usage) = completion.usage
            && let Ok(raw) = serde_json::from_str::<serde_json::Value>(&body_text)
            && let Some(raw_usage) = raw.get("usage")
        {
            usage.cached_tokens = raw_usage
                .get("prompt_tokens_details")
                .and_then(|v| v.get("cached_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            usage.cache_creation_tokens = raw_usage
                .get("prompt_tokens_details")
                .and_then(|v| v.get("cache_creation_input_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            usage.reasoning_tokens = raw_usage
                .get("completion_tokens_details")
                .and_then(|v| v.get("reasoning_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
        }

        Ok(completion)
    }

    #[instrument(skip_all, fields(provider = %self.base.name, model = %real_model))]
    async fn chat_stream(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        let mut upstream_request = request.clone();
        self.base
            .apply_role_mappings(&mut upstream_request.messages);
        upstream_request.model = real_model.to_string();
        upstream_request.stream = Some(true);
        // Force include_usage: SAG needs usage for CallLog stats; a client's
        // stream_options (especially include_usage: false) must not override
        // this internal requirement. OpenAI-compatible backends interpret
        // stream_options.include_usage as documented.
        upstream_request.stream_options = Some(serde_json::json!({"include_usage": true}));

        if !request.extra.is_empty() {
            let keys: Vec<&str> = request.extra.keys().map(|s| s.as_str()).collect();
            trace!(extra = ?keys, "forwarding non-standard extra parameters");
        }

        let url = self.base.build_url(api_endpoint);

        let body = serde_json::to_value(&upstream_request)
            .map_err(|e| AppError::Internal(format!("failed to serialize request: {e}")))?;

        let (auth_name, auth_value) = self.auth_header();
        let response = self
            .base
            .send_json_stream(&url, body, target_extra, &[(auth_name, &auth_value)])
            .await?;

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
                        trace!(received_bytes = chunk.len(), "received upstream SSE chunk");
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

            if !buffer.is_empty() {
                drain_sse_frames(&mut buffer, &tx);
            }

            if !had_error {
                let _ = tx.send(StreamChunk::Done);
            }
        });

        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|chunk| (chunk, rx))
        });

        Ok(Box::pin(stream))
    }

    #[instrument(skip_all, fields(provider = %self.base.name, model = %real_model))]
    async fn chat_image(
        &self,
        request: &ImageGenerationRequest,
        real_model: &str,
        target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<ImageGenerationResponse> {
        let mut upstream = request.clone();
        upstream.model = real_model.to_string();

        let url = self.base.build_url("/images/generations");

        let body = serde_json::to_value(&upstream)
            .map_err(|e| AppError::Internal(format!("failed to serialize request: {e}")))?;

        let (auth_name, auth_value) = self.auth_header();
        let body_text = self
            .base
            .send_json(&url, body, target_extra, &[(auth_name, &auth_value)])
            .await?;

        serde_json::from_str(&body_text).map_err(|e| {
            AppError::Internal(format!("failed to parse image generation response: {e}"))
        })
    }
}
