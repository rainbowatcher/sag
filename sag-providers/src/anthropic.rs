use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use reqwest::header::CONTENT_TYPE;
use sag_config::KeyStrategy;
use sag_core::{
    AppError, ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
    ImageGenerationRequest, ImageGenerationResponse, StreamChunk, Usage,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, error, instrument, warn};

use crate::{LlmProvider, apply_body_overrides, apply_headers, drain_sse_frames};

// ── Anthropic wire types ─────────────────────────────────────────

#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<ContentBlock>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
}

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequences: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[expect(dead_code)]
struct AnthropicResponse {
    id: String,
    #[serde(rename = "type")]
    resp_type: String,
    role: String,
    content: Vec<ContentBlockResp>,
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ContentBlockResp {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    #[allow(dead_code)]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

// ── Provider ─────────────────────────────────────────────────────

/// Anthropic Messages API provider adapter.
pub struct AnthropicProvider {
    name: String,
    base_url: String,
    api_keys: Vec<String>,
    key_strategy: KeyStrategy,
    key_counter: AtomicUsize,
    http_client: reqwest::Client,
    headers: HashMap<String, String>,
    body_overrides: HashMap<String, serde_json::Value>,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider from configuration.
    pub fn new(name: String, config: &sag_config::ProviderConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .expect("failed to build reqwest client");

        assert!(!config.api_keys.is_empty(), "provider '{}': api_keys is empty", name);
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

    /// Convert an OpenAI-format request to Anthropic Messages API format.
    fn to_anthropic_request(request: &ChatCompletionRequest, real_model: &str) -> AnthropicRequest {
        let system = request
            .messages
            .iter()
            .find(|m| m.role == "system")
            .and_then(|m| m.text_content().map(String::from));

        let messages: Vec<AnthropicMessage> = request
            .messages
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| AnthropicMessage {
                role: if m.role == "assistant" {
                    "assistant".into()
                } else {
                    "user".into()
                },
                content: vec![ContentBlock::Text {
                    text: m.text_content().unwrap_or_default().to_string(),
                }],
            })
            .collect();

        let max_tokens = request.max_tokens.unwrap_or(1024);

        let stop_sequences = request.stop.clone().map(|s| s.into_vec());

        AnthropicRequest {
            model: real_model.to_string(),
            messages,
            system,
            max_tokens,
            stream: request.stream,
            temperature: request.temperature,
            top_p: request.top_p,
            stop_sequences,
        }
    }

    /// Convert an Anthropic response back to the standard format.
    fn from_anthropic_response(response: AnthropicResponse, model: &str) -> ChatCompletionResponse {
        let text = response
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlockResp::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        let usage = response.usage.map(|u| Usage {
            prompt_tokens: u.input_tokens,
            completion_tokens: u.output_tokens,
            total_tokens: u.input_tokens + u.output_tokens,
        });

        ChatCompletionResponse {
            id: response.id,
            object: "chat.completion".into(),
            created: chrono::Utc::now().timestamp() as u64,
            model: model.to_string(),
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::String(text),
                    name: None,
                    extra: serde_json::Map::new(),
                },
                finish_reason: Some("stop".into()),
            }],
            usage,
        }
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
            .header("x-api-key", self.current_key())
            .header("anthropic-version", "2023-06-01")
            .header(CONTENT_TYPE, "application/json");

        let req = apply_headers(req, &self.headers);

        let body = apply_body_overrides(body, &self.body_overrides);
        let body = apply_body_overrides(body, target_extra);
        req.json(&body)
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        &self.name
    }

    #[instrument(skip(self, request), fields(provider = %self.name, model = %real_model))]
    async fn chat(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        _api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<ChatCompletionResponse> {
        let upstream = Self::to_anthropic_request(request, real_model);

        if !request.extra.is_empty() {
            let keys: Vec<&str> = request.extra.keys().map(|s| s.as_str()).collect();
            debug!(provider = %self.name, extra = ?keys, "forwarding non-standard extra parameters");
        }

        let url = format!("{}/messages", self.base_url);

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
            warn!(status, %body_text, "anthropic upstream returned error");
            return Err(AppError::Upstream {
                provider: self.name.clone(),
                status,
                body: body_text,
            });
        }

        let anthropic_resp: AnthropicResponse = serde_json::from_str(&body_text)
            .map_err(|e| AppError::Internal(format!("failed to parse anthropic response: {e}")))?;

        Ok(Self::from_anthropic_response(anthropic_resp, real_model))
    }

    #[instrument(skip(self, request), fields(provider = %self.name, model = %real_model))]
    async fn chat_stream(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        _api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        let mut upstream = Self::to_anthropic_request(request, real_model);
        upstream.stream = Some(true);

        if !request.extra.is_empty() {
            let keys: Vec<&str> = request.extra.keys().map(|s| s.as_str()).collect();
            debug!(provider = %self.name, extra = ?keys, "forwarding non-standard extra parameters");
        }

        let url = format!("{}/messages", self.base_url);

        let body = serde_json::to_value(&upstream)
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
            warn!(status = status_code, %body_text, "anthropic upstream error on stream");
            return Err(AppError::Upstream {
                provider: self.name.clone(),
                status: status_code,
                body: body_text,
            });
        }

        let (tx, rx) = mpsc::unbounded_channel::<StreamChunk>();

        tokio::spawn(async move {
            let mut byte_stream = response.bytes_stream();
            let mut buffer: Vec<u8> = Vec::new();

            loop {
                match byte_stream.next().await {
                    Some(Ok(chunk)) => {
                        buffer.extend_from_slice(&chunk);
                        drain_sse_frames(&mut buffer, &tx);
                    }
                    Some(Err(e)) => {
                        error!(?e, "error reading anthropic stream chunk");
                        break;
                    }
                    None => break,
                }
            }

            if !buffer.is_empty() {
                drain_sse_frames(&mut buffer, &tx);
            }

            let _ = tx.send(StreamChunk::Done);
        });

        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|chunk| (chunk, rx))
        });

        Ok(Box::pin(stream))
    }

    #[instrument(skip(self), fields(provider = %self.name))]
    async fn chat_image(
        &self,
        _request: &ImageGenerationRequest,
        _real_model: &str,
        _target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<ImageGenerationResponse> {
        Err(AppError::BadRequest(format!(
            "provider '{}' (anthropic) does not support image generation",
            self.name
        )))
    }
}
