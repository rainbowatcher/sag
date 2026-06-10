use std::collections::HashMap;
use std::pin::Pin;

use async_trait::async_trait;
use futures_util::Stream;
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
    ) -> sag_core::Result<ChatCompletionResponse>;

    /// Send a streaming chat completion request. Returns a stream of
    /// parsed SSE chunks that the router layer can forward to the client
    /// while extracting usage for analytics.
    async fn chat_stream(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>>;

    /// Send an image generation request. Returns image URLs or base64 data.
    async fn chat_image(
        &self,
        request: &ImageGenerationRequest,
        real_model: &str,
        target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<ImageGenerationResponse>;
}
