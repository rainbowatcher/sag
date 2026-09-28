use std::collections::HashMap;
use std::pin::Pin;

use async_trait::async_trait;
use futures_util::Stream;
use sag_core::AppError;
use sag_core::types::{
    ChatCompletionRequest, ChatCompletionResponse, ImageGenerationRequest, ImageGenerationResponse,
    StreamChunk,
};

/// Unified trait for LLM provider adapters.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Return the provider name (e.g. "openai", "anthropic").
    fn name(&self) -> &str;

    /// Send a non-streaming chat completion request. Returns a standardized response.
    async fn chat(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
        timeout_secs: u64,
    ) -> sag_core::Result<ChatCompletionResponse>;

    /// Send a streaming chat completion request. Returns a stream of
    /// parsed SSE chunks that the router layer can forward to the client
    /// while extracting usage for analytics.
    ///
    /// `timeout_secs` is a **no-op for a successful stream**: the total
    /// wall-clock duration of a healthy stream is intentionally unbounded
    /// by it (it only covers the non-streaming `chat` round-trip). The
    /// streaming path bounds the time-to-first-byte via
    /// `ProviderBase::ttfb_timeout_secs` and per-chunk idle time via
    /// `chunk_timeout_secs`.
    async fn chat_stream(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
        timeout_secs: u64,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>>;

    /// Send an image generation request. Returns image URLs or base64 data.
    ///
    /// `api_endpoint` is the route-declared upstream path (e.g.
    /// `/images/generations`) appended to the provider's
    /// `apis.image` (or `apis.chat_completions`) `base_url`.
    ///
    /// Default implementation returns `BadRequest` — override for providers
    /// that support image generation (e.g. OpenAI).
    async fn chat_image(
        &self,
        _request: &ImageGenerationRequest,
        _real_model: &str,
        _api_endpoint: &str,
        _target_extra: &HashMap<String, serde_json::Value>,
        _timeout_secs: u64,
    ) -> sag_core::Result<ImageGenerationResponse> {
        Err(AppError::BadRequest(format!(
            "provider '{}' does not support image generation",
            self.name()
        )))
    }

    /// Native Anthropic Messages call (M5): send an Anthropic-shaped body
    /// and return the raw JSON response.
    ///
    /// Default: unsupported. `AnthropicProvider` overrides this.
    async fn anthropic_messages(
        &self,
        _body: serde_json::Value,
        _target_extra: &HashMap<String, serde_json::Value>,
        _timeout_secs: u64,
    ) -> sag_core::Result<serde_json::Value> {
        Err(AppError::BadRequest(format!(
            "provider '{}' does not support native Anthropic messages",
            self.name()
        )))
    }

    /// Native Anthropic Messages streaming: Anthropic-shaped body in, raw
    /// Anthropic SSE frames as `StreamChunk::Data` out.
    ///
    /// Default: unsupported. `AnthropicProvider` overrides this.
    async fn anthropic_messages_stream(
        &self,
        _body: serde_json::Value,
        _target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        Err(AppError::BadRequest(format!(
            "provider '{}' does not support native Anthropic streaming",
            self.name()
        )))
    }

    /// Native Responses call (M6): send a Responses-shaped body to
    /// `api_endpoint` (typically `/responses`) and return the raw JSON.
    ///
    /// Default: unsupported. `OpenAiProvider` overrides this.
    async fn responses_native(
        &self,
        _body: serde_json::Value,
        _target_extra: &HashMap<String, serde_json::Value>,
        _api_endpoint: &str,
        _timeout_secs: u64,
    ) -> sag_core::Result<serde_json::Value> {
        // Align with D1 rejects on /v1/responses (rendered as 422).
        Err(AppError::BadRequest(format!(
            "native Responses is not supported (provider '{}')",
            self.name()
        )))
    }

    /// Native Responses streaming: Responses-shaped body in, raw Responses
    /// SSE frames as `StreamChunk::Data` out (usage side-channeled from
    /// `response.completed`).
    ///
    /// Default: unsupported. `OpenAiProvider` overrides this.
    async fn responses_stream_native(
        &self,
        _body: serde_json::Value,
        _target_extra: &HashMap<String, serde_json::Value>,
        _api_endpoint: &str,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        // Align with D1 rejects on /v1/responses (rendered as 422).
        Err(AppError::BadRequest(format!(
            "native Responses streaming is not supported (provider '{}')",
            self.name()
        )))
    }
}
