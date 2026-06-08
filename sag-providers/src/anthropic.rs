use async_trait::async_trait;
use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use futures_util::StreamExt;
use reqwest::header::CONTENT_TYPE;
use sag_core::{
    AppError, ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage,
    ImageGenerationRequest, ImageGenerationResponse, Usage,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, instrument, warn};

use crate::LlmProvider;

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
    api_key: String,
    http_client: reqwest::Client,
    supported_params: Vec<String>,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider from configuration.
    pub fn new(name: String, config: &sag_config::ProviderConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .expect("failed to build reqwest client");

        Self {
            name,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_key: config.api_key.clone(),
            http_client,
            supported_params: config.supported_params.clone(),
        }
    }

    /// Validate extra parameters against supported whitelist.
    fn validate_params(&self, request: &ChatCompletionRequest) -> Result<(), AppError> {
        if self.supported_params.is_empty() {
            return Ok(());
        }
        let supported_set: std::collections::HashSet<&str> =
            self.supported_params.iter().map(|s| s.as_str()).collect();

        let unsupported: Vec<String> = request
            .extra
            .keys()
            .filter(|k| !supported_set.contains(k.as_str())).cloned()
            .collect();

        if !unsupported.is_empty() {
            return Err(AppError::BadRequest(format!(
                "provider '{}' does not support parameters: {}",
                self.name,
                unsupported.join(", ")
            )));
        }
        Ok(())
    }

    /// Convert an OpenAI-format request to Anthropic Messages API format.
    fn to_anthropic_request(request: &ChatCompletionRequest, real_model: &str) -> AnthropicRequest {
        let system = request
            .messages
            .iter()
            .find(|m| m.role == "system")
            .map(|m| m.content.clone());

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
                    text: m.content.clone(),
                }],
            })
            .collect();

        let max_tokens = request.max_tokens.unwrap_or(1024);

        let stop_sequences = request.stop.clone();

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
                    content: text,
                    name: None,
                },
                finish_reason: Some("stop".into()),
            }],
            usage,
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn supported_params(&self) -> &[String] {
        &self.supported_params
    }

    #[instrument(skip(self, request), fields(provider = %self.name, model = %real_model))]
    async fn chat(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        _api_endpoint: &str,
    ) -> sag_core::Result<ChatCompletionResponse> {
        self.validate_params(request)?;

        let upstream = Self::to_anthropic_request(request, real_model);
        let url = format!("{}/messages", self.base_url);
        debug!(%url, "sending anthropic non-streaming request");

        let response = self
            .http_client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header(CONTENT_TYPE, "application/json")
            .json(&upstream)
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
    ) -> sag_core::Result<Response<Body>> {
        self.validate_params(request)?;

        let mut upstream = Self::to_anthropic_request(request, real_model);
        upstream.stream = Some(true);

        let url = format!("{}/messages", self.base_url);
        debug!(%url, "sending anthropic streaming request");

        let response = self
            .http_client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header(CONTENT_TYPE, "application/json")
            .json(&upstream)
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

        let response_builder = Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .header("Connection", "keep-alive");

        let byte_stream = response.bytes_stream().map(|result| {
            result.map_err(|e| {
                error!(?e, "error reading anthropic stream chunk");
                std::io::Error::other(e)
            })
        });

        let body = Body::from_stream(byte_stream);

        response_builder
            .body(body)
            .map_err(|e| AppError::Internal(format!("failed to build stream response: {e}")))
    }

    #[instrument(skip(self), fields(provider = %self.name))]
    async fn chat_image(
        &self,
        _request: &ImageGenerationRequest,
        _real_model: &str,
    ) -> sag_core::Result<ImageGenerationResponse> {
        Err(AppError::BadRequest(format!(
            "provider '{}' (anthropic) does not support image generation",
            self.name
        )))
    }
}
