use serde::{Deserialize, Serialize};

/// A chat message in OpenAI-compatible format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    /// Message content — can be a plain text string or an array of
    /// content parts (text, image_url, input_audio, file, etc.).
    pub content: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Catch-all for message-level fields not captured by the struct
    /// (e.g. `tool_calls`, `tool_call_id`, `refusal`, `audio`, `function_call`).
    /// These are forwarded transparently by OpenAI providers and ignored
    /// by providers that map to a different wire format (e.g. Anthropic).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl ChatMessage {
    /// Extract the plain-text content of this message, if it is a simple string.
    pub fn text_content(&self) -> Option<&str> {
        self.content.as_str()
    }
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
    /// Accepts either a single string or an array of strings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopSequence>,
    /// The maximum number of tokens that can be generated (deprecated in favor of max_completion_tokens).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Number between -2.0 and 2.0. Positive values penalize tokens based on their existing frequency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    /// Number between -2.0 and 2.0. Positive values penalize new tokens based on their existing frequency.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    /// Modify the likelihood of specified tokens appearing in the completion.
    /// Maps token IDs (as string keys) to bias values from -100 to 100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<serde_json::Value>,
    /// An upper bound for the number of tokens that can be generated, including reasoning tokens.
    /// Replaces `max_tokens` for o-series models.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u32>,
    /// Output types that you would like the model to generate.
    /// E.g. `["text"]` or `["text", "audio"]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modalities: Option<serde_json::Value>,
    /// Parameters for audio output. Required when audio output is requested
    /// with `modalities: ["audio"]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<serde_json::Value>,
    /// Options for streaming response. Only set this when you set `stream: true`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<serde_json::Value>,
    /// Constrains effort on reasoning for reasoning models.
    /// Supported values: `none`, `minimal`, `low`, `medium`, `high`, `xhigh`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Set of 16 key-value pairs that can be attached to the completion for structured metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Whether to enable parallel function calling during tool use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
    /// Static predicted output content for faster regeneration (Predicted Outputs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prediction: Option<serde_json::Value>,
    /// A list of tools the model may call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<serde_json::Value>,
    /// Controls which (if any) tool is called by the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    /// An object specifying the format that the model must output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<serde_json::Value>,
    /// If specified, the system will make a best effort to sample deterministically.
    /// This feature is in Beta — determinism is not guaranteed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    /// Whether to return log probabilities of the output tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,
    /// An integer specifying the number of most likely tokens to return at each token position.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<i64>,
    /// A unique identifier representing your end-user (being replaced by
    /// `safety_identifier` and `prompt_cache_key`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Used by OpenAI to cache responses for similar requests to optimize
    /// cache hit rates. Replaces `user` for caching purposes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    /// The retention policy for the prompt cache.
    /// One of `"in_memory"` or `"24h"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_retention: Option<String>,
    /// A stable identifier used to help detect users that may be violating
    /// OpenAI's usage policies. Replaces `user` for safety purposes.
    /// Max 64 characters; hashing recommended.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_identifier: Option<String>,
    /// Whether to store the output for distillation or evals.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    /// Specifies the processing type used for serving the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// Constrains the verbosity of the model's response.
    /// Supported values: `"low"`, `"medium"`, `"high"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<String>,
    /// Configuration for running moderation on the request input and
    /// generated output. E.g. `{"model": "omni-moderation-latest"}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moderation: Option<serde_json::Value>,
    /// Web search options for models that support the web search tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_search_options: Option<serde_json::Value>,
    /// Deprecated in favor of `tool_choice`. Controls which (if any)
    /// function is called by the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<serde_json::Value>,
    /// Deprecated in favor of `tools`. A list of functions the model
    /// may generate JSON inputs for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub functions: Option<serde_json::Value>,
    /// Catch-all for non-standard / provider-specific parameters.
    /// Filtered per-provider against supported params.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Represents the `stop` parameter which can be either a single string or an
/// array of up to 4 strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StopSequence {
    /// A single stop sequence, e.g. `"END"`.
    Single(String),
    /// Multiple stop sequences, e.g. `["END", "STOP"]`.
    Multiple(Vec<String>),
}

impl StopSequence {
    /// Convert to a `Vec<String>` regardless of variant.
    pub fn into_vec(self) -> Vec<String> {
        match self {
            StopSequence::Single(s) => vec![s],
            StopSequence::Multiple(v) => v,
        }
    }
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
    /// Tokens retrieved from the prompt cache (OpenAI `prompt_tokens_details.cached_tokens`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
    /// Tokens newly written to the prompt cache (Anthropic `cache_creation_input_tokens`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u32>,
    /// Tokens consumed by the model's internal reasoning (e.g. o-series `reasoning_tokens`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
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
    /// A mid-stream error from the upstream connection (network drop, timeout,
    /// etc.). The router should log this for analytics but may not be able to
    /// notify the client since response headers are already sent.
    Error(String),
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
                let json = serde_json::json!({ "usage": u });
                format!(
                    "data: {}\n\n",
                    serde_json::to_string(&json).unwrap_or_default()
                )
                .into_bytes()
            }
            StreamChunk::Error(e) => {
                let json = serde_json::json!({
                    "error": {
                        "code": "STREAM_ERROR",
                        "message": e,
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
    /// `reasoning_effort` parameter from the client request (OpenAI o-series).
    /// `None` means the client did not specify it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Duration spent waiting for and receiving the upstream response only
    /// (excluding any SAG-internal overhead). Populated in stage 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_duration_ms: Option<u64>,
    /// Time-To-First-Token in milliseconds. Populated in stage 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    /// Tokens retrieved from the prompt cache (copied from Usage at log time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
    /// Tokens newly written to the prompt cache (copied from Usage at log time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<u32>,
    /// Tokens consumed by the model's internal reasoning (copied from Usage at log time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
}
