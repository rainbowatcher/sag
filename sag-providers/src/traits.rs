use std::collections::HashMap;
use async_trait::async_trait;
use axum::body::Body;
use axum::response::Response;
use sag_core::types::{
    ChatCompletionRequest, ChatCompletionResponse, ImageGenerationRequest, ImageGenerationResponse,
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

    /// Send a streaming chat completion request. Returns an Axum response
    /// that streams SSE chunks back to the client.
    async fn chat_stream(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<Response<Body>>;

    /// Send an image generation request. Returns image URLs or base64 data.
    async fn chat_image(
        &self,
        request: &ImageGenerationRequest,
        real_model: &str,
        target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<ImageGenerationResponse>;
}
