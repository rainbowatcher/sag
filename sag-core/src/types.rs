use serde::{Deserialize, Serialize};

/// A chat message in OpenAI-compatible format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Standard chat completion request accepted by the gateway.
/// Mirrors the OpenAI Chat Completion API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    /// ID of the model to use.
    pub model: String,
    /// A list of messages comprising the conversation.
    pub messages: Vec<ChatMessage>,
    /// What sampling temperature to use, between 0 and 2.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// An alternative to sampling with temperature, called nucleus sampling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// How many chat completion choices to generate for each input message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    /// If set, partial message deltas will be sent via SSE.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Up to 4 sequences where the API will stop generating further tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    /// The maximum number of tokens that can be generated (deprecated in favor of max_completion_tokens).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Number between -2.0 and 2.0. Positive values penalize tokens based on their existing frequency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// Number between -2.0 and 2.0. Positive values penalize new tokens based on their existing frequency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    /// An upper bound for the number of tokens that can be generated, including reasoning tokens.
    /// Replaces `max_tokens` for o-series models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    /// Options for streaming response. Only set this when you set `stream: true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<serde_json::Value>,
    /// Constrains effort on reasoning for reasoning models.
    /// Supported values: `none`, `low`, `medium`, `high`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Set of 16 key-value pairs that can be attached to the completion for structured metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Whether to enable parallel function calling during tool use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// A list of tools the model may call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<serde_json::Value>,
    /// Controls which (if any) tool is called by the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    /// An object specifying the format that the model must output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<serde_json::Value>,
    /// If specified, the system will make a best effort to sample deterministically (Deprecated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    /// Whether to return log probabilities of the output tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,
    /// An integer specifying the number of most likely tokens to return at each token position.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<i64>,
    /// A unique identifier representing your end-user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Whether to store the output for distillation or evals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    /// Specifies the processing type used for serving the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// Catch-all for non-standard / provider-specific parameters.
    /// Filtered per-provider against supported params.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A choice in a non-streaming chat completion response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatChoice {
    pub index: u32,
    pub message: ChatMessage,
    pub finish_reason: Option<String>,
}

/// Token usage information.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

/// Standard chat completion response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<ChatChoice>,
    pub usage: Option<Usage>,
}

// ═══════════════════════════════════════════════════════════════════
// Image Generation types (OpenAI Images API compatible)
// ═══════════════════════════════════════════════════════════════════

/// Image generation request (OpenAI Images API format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageGenerationRequest {
    pub model: String,
    pub prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<String>,
    /// Catch-all for provider-specific parameters.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A single generated image result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub b64_json: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revised_prompt: Option<String>,
}

/// Image generation response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageGenerationResponse {
    pub created: u64,
    pub data: Vec<ImageData>,
}

/// A parsed chunk from a streaming chat completion response.
///
/// Providers emit these; the router layer maps them into an SSE response
/// while extracting token usage for analytics.
#[derive(Debug, Clone)]
pub enum StreamChunk {
    /// SSE data line to forward to the client as-is (including `data: ` prefix
    /// and `\n\n` suffix).
    Data(Vec<u8>),
    /// Token usage extracted from the stream for analytics.
    Usage(Usage),
    /// Stream end signal. The router will log the final CallLog when this is
    /// received and close the response body.
    Done,
}

impl StreamChunk {
    /// Serialize this chunk into OpenAI SSE wire format bytes.
    pub fn to_sse_bytes(&self) -> Vec<u8> {
        match self {
            StreamChunk::Data(data) => data.clone(),
            StreamChunk::Usage(u) => {
                let json = serde_json::json!({
                    "usage": {
                        "prompt_tokens": u.prompt_tokens,
                        "completion_tokens": u.completion_tokens,
                        "total_tokens": u.total_tokens,
                    }
                });
                format!(
                    "data: {}\n\n",
                    serde_json::to_string(&json).unwrap_or_default()
                )
                .into_bytes()
            }
            StreamChunk::Done => b"data: [DONE]\n\n".to_vec(),
        }
    }
}

/// An entry in the analytics call log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallLog {
    pub request_id: uuid::Uuid,
    pub provider: String,
    pub model: String,
    pub real_model: String,
    pub stream: bool,
    pub status_code: u16,
    pub duration_ms: u64,
    pub usage: Option<Usage>,
    pub error: Option<String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}
