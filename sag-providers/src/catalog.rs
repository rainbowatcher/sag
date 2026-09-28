use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::Stream;
use sag_core::{
    ApiKind, AppError, ChatCompletionRequest, ChatCompletionResponse, ImageGenerationRequest,
    ImageGenerationResponse, StreamChunk,
};

use crate::{AnthropicProvider, LlmProvider, OpenAiProvider, ProviderBase};

/// Protocol-catalog provider: one adapter instance serving every API key
/// declared in `ProviderConfig::apis`.
///
/// Construction is driven by the `apis` catalog (there is no
/// `provider_type`): the OpenAI-shaped lane (chat / responses / image) and
/// the Anthropic Messages lane are delegated to the corresponding
/// sub-adapter over a **shared** [`ProviderBase`], so a provider that
/// declares both protocol families works within a single instance.
///
/// Method availability mirrors the declared catalog:
/// - `chat` / `chat_stream` → `chat_completions` when present, else
///   `anthropic_messages` (translated OpenAI→Anthropic path);
/// - `chat_image` → `image` (or `chat_completions`) node;
/// - `anthropic_messages*` → `anthropic_messages` node;
/// - `responses_native*` → `responses` node.
///
/// Unsupported combinations return `BadRequest`, matching the trait
/// defaults.
pub struct CatalogProvider {
    base: Arc<ProviderBase>,
    openai: OpenAiProvider,
    anthropic: AnthropicProvider,
}

impl CatalogProvider {
    /// Create a catalog provider from configuration with an explicit
    /// effective proxy. See [`ProviderBase::new_with_proxy`] for the
    /// proxy semantics.
    pub fn new_with_proxy(
        name: String,
        config: &sag_config::ProviderConfig,
        proxy: Option<&sag_config::ProxyConfig>,
    ) -> sag_core::Result<Self> {
        let base = Arc::new(ProviderBase::new_with_proxy(name, config, proxy)?);
        Ok(Self {
            openai: OpenAiProvider::from_base(base.clone()),
            anthropic: AnthropicProvider::from_base(base.clone()),
            base,
        })
    }

    /// Create a catalog provider connecting directly (system proxy env
    /// explicitly ignored).
    pub fn new(name: String, config: &sag_config::ProviderConfig) -> sag_core::Result<Self> {
        Self::new_with_proxy(name, config, None)
    }

    fn unsupported(&self, what: &str) -> AppError {
        AppError::BadRequest(format!(
            "provider '{}' does not support {}",
            self.base.name, what
        ))
    }
}

#[async_trait]
impl LlmProvider for CatalogProvider {
    fn name(&self) -> &str {
        &self.base.name
    }

    async fn chat(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
        timeout_secs: u64,
    ) -> sag_core::Result<ChatCompletionResponse> {
        if self.base.supports(ApiKind::ChatCompletions) {
            self.openai
                .chat(
                    request,
                    real_model,
                    api_endpoint,
                    target_extra,
                    timeout_secs,
                )
                .await
        } else if self.base.supports(ApiKind::AnthropicMessages) {
            self.anthropic
                .chat(
                    request,
                    real_model,
                    api_endpoint,
                    target_extra,
                    timeout_secs,
                )
                .await
        } else {
            Err(self.unsupported("chat completions"))
        }
    }

    async fn chat_stream(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
        timeout_secs: u64,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        if self.base.supports(ApiKind::ChatCompletions) {
            self.openai
                .chat_stream(
                    request,
                    real_model,
                    api_endpoint,
                    target_extra,
                    timeout_secs,
                )
                .await
        } else if self.base.supports(ApiKind::AnthropicMessages) {
            self.anthropic
                .chat_stream(
                    request,
                    real_model,
                    api_endpoint,
                    target_extra,
                    timeout_secs,
                )
                .await
        } else {
            Err(self.unsupported("chat completions streaming"))
        }
    }

    async fn chat_image(
        &self,
        request: &ImageGenerationRequest,
        real_model: &str,
        api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
        timeout_secs: u64,
    ) -> sag_core::Result<ImageGenerationResponse> {
        if self.base.supports(ApiKind::Image) || self.base.supports(ApiKind::ChatCompletions) {
            self.openai
                .chat_image(
                    request,
                    real_model,
                    api_endpoint,
                    target_extra,
                    timeout_secs,
                )
                .await
        } else {
            Err(self.unsupported("image generation"))
        }
    }

    async fn anthropic_messages(
        &self,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
        timeout_secs: u64,
    ) -> sag_core::Result<serde_json::Value> {
        if self.base.supports(ApiKind::AnthropicMessages) {
            self.anthropic
                .anthropic_messages(body, target_extra, timeout_secs)
                .await
        } else {
            Err(self.unsupported("native Anthropic messages"))
        }
    }

    async fn anthropic_messages_stream(
        &self,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        if self.base.supports(ApiKind::AnthropicMessages) {
            self.anthropic
                .anthropic_messages_stream(body, target_extra)
                .await
        } else {
            Err(self.unsupported("native Anthropic streaming"))
        }
    }

    async fn responses_native(
        &self,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
        api_endpoint: &str,
        timeout_secs: u64,
    ) -> sag_core::Result<serde_json::Value> {
        if self.base.supports(ApiKind::Responses) {
            self.openai
                .responses_native(body, target_extra, api_endpoint, timeout_secs)
                .await
        } else {
            // Align with D1 rejects on /v1/responses (rendered as 422).
            Err(AppError::BadRequest(format!(
                "native Responses is not supported (provider '{}')",
                self.base.name
            )))
        }
    }

    async fn responses_stream_native(
        &self,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
        api_endpoint: &str,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        if self.base.supports(ApiKind::Responses) {
            self.openai
                .responses_stream_native(body, target_extra, api_endpoint)
                .await
        } else {
            // Align with D1 rejects on /v1/responses (rendered as 422).
            Err(AppError::BadRequest(format!(
                "native Responses streaming is not supported (provider '{}')",
                self.base.name
            )))
        }
    }
}
