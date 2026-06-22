use std::collections::HashMap;
use std::pin::Pin;

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use sag_core::{
    AppError, ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, StreamChunk,
    Usage,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{error, instrument, trace};

use crate::{LlmProvider, ProviderBase, find_sse_boundary};

// ── SSE normalization ───────────────────────────────────────────────

/// Tracks state across Anthropic SSE events to produce OpenAI-format chunks.
#[derive(Default)]
struct AnthropicStreamState {
    /// Message ID from the `message_start` event.
    id: Option<String>,
    /// Model name from the `message_start` event.
    model: Option<String>,
    /// Unix timestamp set when the stream begins.
    created: u64,
}

/// Extract and send token usage from an Anthropic usage JSON object.
fn extract_anthropic_usage(usage: &serde_json::Value, tx: &mpsc::UnboundedSender<StreamChunk>) {
    if let (Some(input), Some(output)) = (
        usage.get("input_tokens").and_then(|v| v.as_u64()),
        usage.get("output_tokens").and_then(|v| v.as_u64()),
    ) {
        let _ = tx.send(StreamChunk::Usage(Usage {
            prompt_tokens: input as u32,
            completion_tokens: output as u32,
            total_tokens: (input + output) as u32,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        }));
    }
}

/// Map Anthropic's `stop_reason` to OpenAI's `finish_reason`.
fn map_stop_reason(reason: &str) -> &str {
    match reason {
        "end_turn" => "stop",
        "max_tokens" => "length",
        "stop_sequence" => "stop",
        other => other,
    }
}

/// Drain complete SSE frames from the buffer, normalizing Anthropic events
/// into OpenAI-compatible SSE chunks.
fn drain_anthropic_sse_frames(
    buffer: &mut Vec<u8>,
    tx: &mpsc::UnboundedSender<StreamChunk>,
    state: &mut AnthropicStreamState,
) {
    while let Some(end) = find_sse_boundary(buffer) {
        let frame: Vec<u8> = buffer.drain(..end).collect();
        let delim_len = if buffer.len() >= 4 && buffer[..4] == [b'\r', b'\n', b'\r', b'\n'] {
            4
        } else {
            2
        };
        buffer.drain(..delim_len);
        process_anthropic_sse_frame(&frame, tx, state);
    }
}

/// Parse a single Anthropic SSE frame and emit normalized OpenAI-format chunks.
fn process_anthropic_sse_frame(
    frame: &[u8],
    tx: &mpsc::UnboundedSender<StreamChunk>,
    state: &mut AnthropicStreamState,
) {
    let text = String::from_utf8_lossy(frame);

    for line in text.lines() {
        let line = line.trim();
        let Some(json_str) = line.strip_prefix("data: ") else {
            continue;
        };
        let json_str = json_str.trim();

        let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) else {
            // Forward non-JSON payloads (e.g. `[DONE]`) as-is
            if json_str == "[DONE]" {
                let _ = tx.send(StreamChunk::Data(b"data: [DONE]\n\n".to_vec()));
            }
            continue;
        };

        let Some(event_type) = val["type"].as_str() else {
            // No "type" field — forward as-is for safety
            let raw = format!("data: {json_str}\n\n").into_bytes();
            let _ = tx.send(StreamChunk::Data(raw));
            continue;
        };

        match event_type {
            "message_start" => {
                // Capture message metadata for use in subsequent chunks
                if let Some(msg) = val.get("message") {
                    if state.id.is_none() {
                        state.id = msg["id"].as_str().map(String::from);
                    }
                    if state.model.is_none() {
                        state.model = msg["model"].as_str().map(String::from);
                    }
                    // Extract initial usage (input tokens)
                    if let Some(usage) = msg.get("usage") {
                        extract_anthropic_usage(usage, tx);
                    }
                }
            }
            "content_block_delta" => {
                // Convert text delta to OpenAI chunk
                if let Some(text) = val["delta"]["text"].as_str() {
                    let index = val["index"].as_u64().unwrap_or(0);
                    let chunk = serde_json::json!({
                        "id": state.id.as_deref().unwrap_or(""),
                        "object": "chat.completion.chunk",
                        "created": state.created,
                        "model": state.model.as_deref().unwrap_or(""),
                        "choices": [{
                            "index": index,
                            "delta": {"content": text},
                            "finish_reason": serde_json::Value::Null,
                        }],
                    });
                    let raw = format!(
                        "data: {}\n\n",
                        serde_json::to_string(&chunk).unwrap_or_default()
                    )
                    .into_bytes();
                    let _ = tx.send(StreamChunk::Data(raw));
                }
            }
            "message_delta" => {
                // Convert stop reason to OpenAI finish_reason chunk
                let finish_reason = val["delta"]["stop_reason"].as_str().map(map_stop_reason);
                let chunk = serde_json::json!({
                    "id": state.id.as_deref().unwrap_or(""),
                    "object": "chat.completion.chunk",
                    "created": state.created,
                    "model": state.model.as_deref().unwrap_or(""),
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": finish_reason,
                    }],
                });
                let raw = format!(
                    "data: {}\n\n",
                    serde_json::to_string(&chunk).unwrap_or_default()
                )
                .into_bytes();
                let _ = tx.send(StreamChunk::Data(raw));

                // Extract output token usage
                if let Some(usage) = val.get("usage") {
                    extract_anthropic_usage(usage, tx);
                }
            }
            "message_stop" => {
                // Anthropic's end-of-stream → OpenAI [DONE]
                let _ = tx.send(StreamChunk::Data(b"data: [DONE]\n\n".to_vec()));
            }
            // Events not forwarded to the client
            "content_block_start" | "content_block_stop" | "ping" => {}
            // Unknown events — forward as-is for forward-compatibility
            _ => {
                let raw = format!("data: {json_str}\n\n").into_bytes();
                let _ = tx.send(StreamChunk::Data(raw));
            }
        }
    }
}

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
    base: ProviderBase,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider from configuration.
    pub fn new(name: String, config: &sag_config::ProviderConfig) -> Self {
        Self {
            base: ProviderBase::new(name, config),
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
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
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
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        &self.base.name
    }

    #[instrument(skip_all, fields(provider = %self.base.name, model = %real_model))]
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
            trace!(extra = ?keys, "forwarding non-standard extra parameters");
        }

        let url = self.base.build_url("/messages");

        let body = serde_json::to_value(&upstream)
            .map_err(|e| AppError::Internal(format!("failed to serialize request: {e}")))?;

        let key = self.base.current_key();
        let body_text = self
            .base
            .send_json(
                &url,
                body,
                target_extra,
                &[("x-api-key", key), ("anthropic-version", "2023-06-01")],
            )
            .await?;

        let anthropic_resp: AnthropicResponse = serde_json::from_str(&body_text)
            .map_err(|e| AppError::Internal(format!("failed to parse anthropic response: {e}")))?;

        Ok(Self::from_anthropic_response(anthropic_resp, real_model))
    }

    #[instrument(skip_all, fields(provider = %self.base.name, model = %real_model))]
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
            trace!(extra = ?keys, "forwarding non-standard extra parameters");
        }

        let url = self.base.build_url("/messages");

        let body = serde_json::to_value(&upstream)
            .map_err(|e| AppError::Internal(format!("failed to serialize request: {e}")))?;

        let key = self.base.current_key();
        let response = self
            .base
            .send_json_stream(
                &url,
                body,
                target_extra,
                &[("x-api-key", key), ("anthropic-version", "2023-06-01")],
            )
            .await?;

        let (tx, rx) = mpsc::unbounded_channel::<StreamChunk>();

        tokio::spawn(async move {
            let mut byte_stream = response.bytes_stream();
            let mut buffer: Vec<u8> = Vec::new();
            let mut had_error = false;
            let mut state = AnthropicStreamState {
                created: chrono::Utc::now().timestamp() as u64,
                ..Default::default()
            };

            loop {
                // Stop reading if the client has disconnected
                if tx.is_closed() {
                    break;
                }

                match byte_stream.next().await {
                    Some(Ok(chunk)) => {
                        trace!(received_bytes = chunk.len(), "received anthropic SSE chunk");
                        buffer.extend_from_slice(&chunk);
                        drain_anthropic_sse_frames(&mut buffer, &tx, &mut state);
                    }
                    Some(Err(e)) => {
                        error!(?e, "error reading anthropic stream chunk");
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
                drain_anthropic_sse_frames(&mut buffer, &tx, &mut state);
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
}
