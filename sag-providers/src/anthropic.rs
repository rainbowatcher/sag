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
        let cached_tokens = usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);
        let cache_creation_tokens = usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);
        let _ = tx.send(StreamChunk::Usage(Usage {
            prompt_tokens: input as u32,
            completion_tokens: output as u32,
            total_tokens: (input + output) as u32,
            cached_tokens,
            cache_creation_tokens,
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

/// Anthropic `cache_control` marker. Serializes as `{"type": "ephemeral"}`.
#[derive(Debug, Clone, PartialEq)]
struct CacheControl;

impl serde::Serialize for CacheControl {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("CacheControl", 1)?;
        s.serialize_field("type", "ephemeral")?;
        s.end()
    }
}

#[derive(Debug, PartialEq, Serialize)]
struct AnthropicMessage {
    role: String,
    content: Vec<ContentBlock>,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

/// System prompt in Anthropic format: either a plain string or an array of
/// content blocks. `#[serde(untagged)]` serializes `Text` as a bare string
/// and `Blocks` as a JSON array, matching the Anthropic Messages API.
#[derive(Debug, PartialEq, Serialize)]
#[serde(untagged)]
enum SystemContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// A tool definition in Anthropic format, converted from OpenAI's
/// `{type: "function", function: {name, description, parameters}}`.
#[derive(Debug, PartialEq, Serialize)]
struct AnthropicTool {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<SystemContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
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
    #[serde(default)]
    cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
}

// ── Provider ─────────────────────────────────────────────────────

/// Convert OpenAI-format `tools` JSON to Anthropic tool definitions.
///
/// OpenAI format: `[{type: "function", function: {name, description, parameters}}]`
/// Anthropic format: `[{name, description, input_schema}]`
fn convert_tools(tools: &serde_json::Value) -> Option<Vec<AnthropicTool>> {
    let arr = tools.as_array()?;
    if arr.is_empty() {
        return None;
    }
    let converted: Vec<AnthropicTool> = arr
        .iter()
        .filter_map(|tool| {
            let function = tool.get("function")?;
            Some(AnthropicTool {
                name: function.get("name")?.as_str()?.to_string(),
                description: function
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                input_schema: function
                    .get("parameters")
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(Default::default())),
                cache_control: None,
            })
        })
        .collect();
    if converted.is_empty() {
        None
    } else {
        Some(converted)
    }
}

/// Inject `cache_control` markers into the last content block of system,
/// the last tool, and the last content block of the second-to-last user
/// message. These breakpoints tell Anthropic to cache everything before them.
fn inject_cache_control(
    system: &mut Option<SystemContent>,
    tools: &mut Option<Vec<AnthropicTool>>,
    messages: &mut [AnthropicMessage],
) {
    // 1. System: mark the last content block. If the system prompt is a
    //    plain string, convert it to a single-element Blocks array first.
    if let Some(sys) = system {
        match sys {
            SystemContent::Text(text) => {
                let t = std::mem::take(text);
                if !t.is_empty() {
                    *sys = SystemContent::Blocks(vec![ContentBlock::Text {
                        text: t,
                        cache_control: Some(CacheControl),
                    }]);
                }
            }
            SystemContent::Blocks(blocks) => {
                if let Some(last) = blocks.last_mut() {
                    set_cache_control(last);
                }
            }
        }
    }

    // 2. Tools: mark the last tool definition
    if let Some(tool_list) = tools
        && let Some(last) = tool_list.last_mut()
    {
        last.cache_control = Some(CacheControl);
    }

    // 3. Messages: mark the last content block of the last user message
    //    before the final turn. If there's only 0-1 messages, skip.
    if messages.len() >= 2 {
        // Find the last user message that is not the very last message
        let mut user_idx: Option<usize> = None;
        for (i, msg) in messages.iter().enumerate().rev().skip(1) {
            if msg.role == "user" {
                user_idx = Some(i);
                break;
            }
        }
        if let Some(idx) = user_idx
            && let Some(msg) = messages.get_mut(idx)
            && let Some(last) = msg.content.last_mut()
        {
            set_cache_control(last);
        }
    }
}

/// Set `cache_control` on the last content block, regardless of variant.
fn set_cache_control(block: &mut ContentBlock) {
    match block {
        ContentBlock::Text { cache_control, .. } => {
            *cache_control = Some(CacheControl);
        }
    }
}

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
    fn to_anthropic_request(
        request: &ChatCompletionRequest,
        real_model: &str,
        enable_cache_control: bool,
    ) -> AnthropicRequest {
        let mut system = request
            .messages
            .iter()
            .find(|m| m.role == "system")
            .and_then(|m| m.text_content().map(|s| SystemContent::Text(s.to_string())));

        let mut messages: Vec<AnthropicMessage> = request
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
                    cache_control: None,
                }],
            })
            .collect();

        let max_tokens = request.max_tokens.unwrap_or(1024);

        let stop_sequences = request.stop.clone().map(|s| s.into_vec());

        let mut tools = request.tools.as_ref().and_then(convert_tools);

        // Inject cache_control breakpoints so Anthropic caches the
        // system prompt, tools, and conversation history.
        if enable_cache_control {
            inject_cache_control(&mut system, &mut tools, &mut messages);
        }

        AnthropicRequest {
            model: real_model.to_string(),
            messages,
            system,
            tools,
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
            cached_tokens: u.cache_read_input_tokens,
            cache_creation_tokens: u.cache_creation_input_tokens,
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
        timeout_secs: u64,
    ) -> sag_core::Result<ChatCompletionResponse> {
        let upstream = if self.base.has_role_mappings() {
            let mut req = request.clone();
            self.base.apply_role_mappings(&mut req.messages);
            Self::to_anthropic_request(&req, real_model, self.base.anthropic_cache_control)
        } else {
            Self::to_anthropic_request(request, real_model, self.base.anthropic_cache_control)
        };

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
                Some(timeout_secs),
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
        timeout_secs: u64,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        let mut upstream = if self.base.has_role_mappings() {
            let mut req = request.clone();
            self.base.apply_role_mappings(&mut req.messages);
            Self::to_anthropic_request(&req, real_model, self.base.anthropic_cache_control)
        } else {
            Self::to_anthropic_request(request, real_model, self.base.anthropic_cache_control)
        };
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
                Some(timeout_secs),
            )
            .await?;

        let (tx, rx) = mpsc::unbounded_channel::<StreamChunk>();
        let chunk_timeout = self
            .base
            .chunk_timeout_secs()
            .map(std::time::Duration::from_secs);

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

                let chunk_result: Result<_, tokio::time::error::Elapsed> = match &chunk_timeout {
                    Some(dur) => tokio::time::timeout(*dur, byte_stream.next()).await,
                    None => Ok(byte_stream.next().await),
                };

                match chunk_result {
                    Ok(Some(Ok(chunk))) => {
                        trace!(received_bytes = chunk.len(), "received anthropic SSE chunk");
                        buffer.extend_from_slice(&chunk);
                        drain_anthropic_sse_frames(&mut buffer, &tx, &mut state);
                    }
                    Ok(Some(Err(e))) => {
                        error!(?e, "error reading anthropic stream chunk");
                        if !tx.is_closed() {
                            let _ = tx.send(StreamChunk::Error(e.to_string()));
                        }
                        had_error = true;
                        break;
                    }
                    Ok(None) => break,
                    Err(_elapsed) => {
                        error!(chunk_timeout_secs = ?chunk_timeout, "anthropic stream chunk idle timeout");
                        if !tx.is_closed() {
                            let _ = tx.send(StreamChunk::Error(format!(
                                "stream chunk idle timeout after {}s",
                                chunk_timeout.unwrap().as_secs()
                            )));
                        }
                        had_error = true;
                        break;
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Extract `cache_control` from a `ContentBlock` for test assertions.
    fn cache_control_of(block: &ContentBlock) -> &Option<CacheControl> {
        match block {
            ContentBlock::Text { cache_control, .. } => cache_control,
        }
    }

    #[test]
    fn anthropic_usage_deserializes_cache_fields() {
        let json = r#"{
            "input_tokens": 1000,
            "output_tokens": 500,
            "cache_read_input_tokens": 200,
            "cache_creation_input_tokens": 50
        }"#;
        let usage: AnthropicUsage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.output_tokens, 500);
        assert_eq!(usage.cache_read_input_tokens, Some(200));
        assert_eq!(usage.cache_creation_input_tokens, Some(50));
    }

    #[test]
    fn anthropic_usage_cache_fields_default_to_none_when_absent() {
        let json = r#"{
            "input_tokens": 800,
            "output_tokens": 300
        }"#;
        let usage: AnthropicUsage = serde_json::from_str(json).unwrap();
        assert_eq!(usage.input_tokens, 800);
        assert_eq!(usage.output_tokens, 300);
        assert!(usage.cache_read_input_tokens.is_none());
        assert!(usage.cache_creation_input_tokens.is_none());
    }

    #[test]
    fn to_anthropic_request_maps_developer_to_system_when_mapped() {
        let req = ChatCompletionRequest {
            messages: vec![ChatMessage {
                role: "developer".into(),
                content: serde_json::Value::String("you are helpful".into()),
                name: None,
                extra: serde_json::Map::new(),
            }],
            ..Default::default()
        };
        let anthropic_req = AnthropicProvider::to_anthropic_request(&req, "claude-3", false);
        // Without mapping, developer would NOT be extracted as system
        assert_eq!(anthropic_req.system, None);
        // developer falls through to else → "user"
        assert_eq!(anthropic_req.messages[0].role, "user");
    }

    #[test]
    fn to_anthropic_request_developer_becomes_user_by_default() {
        let req = ChatCompletionRequest {
            messages: vec![ChatMessage {
                role: "developer".into(),
                content: serde_json::Value::String("you are helpful".into()),
                name: None,
                extra: serde_json::Map::new(),
            }],
            ..Default::default()
        };
        let anthropic_req = AnthropicProvider::to_anthropic_request(&req, "claude-3", false);
        // In the absence of role mappings, developer messages are treated as
        // "user" by Anthropic's adapter (the else branch).
        assert_eq!(anthropic_req.messages[0].role, "user");
    }

    #[test]
    fn to_anthropic_request_extracts_system_from_mapped_developer() {
        // Simulate what happens AFTER apply_role_mappings: developer → system
        let mut req = ChatCompletionRequest::default();
        req.messages = vec![ChatMessage {
            role: "system".into(), // already mapped
            content: serde_json::Value::String("you are helpful".into()),
            name: None,
            extra: serde_json::Map::new(),
        }];
        let anthropic_req = AnthropicProvider::to_anthropic_request(&req, "claude-3", false);
        assert_eq!(
            anthropic_req.system,
            Some(SystemContent::Text("you are helpful".into()))
        );
        assert!(anthropic_req.messages.is_empty());
    }

    #[test]
    fn to_anthropic_request_deduplicates_multiple_system_messages() {
        let mut req = ChatCompletionRequest::default();
        req.messages = vec![
            ChatMessage {
                role: "system".into(),
                content: serde_json::Value::String("first system".into()),
                name: None,
                extra: serde_json::Map::new(),
            },
            ChatMessage {
                role: "system".into(),
                content: serde_json::Value::String("second system".into()),
                name: None,
                extra: serde_json::Map::new(),
            },
        ];
        let anthropic_req = AnthropicProvider::to_anthropic_request(&req, "claude-3", false);
        // Only the first system message is kept
        assert_eq!(
            anthropic_req.system,
            Some(SystemContent::Text("first system".into()))
        );
    }

    // ── tools conversion ──────────────────────────────────────────

    #[test]
    fn convert_tools_parses_openai_function_tools() {
        let tools: serde_json::Value = serde_json::json!([
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get the weather for a city",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "city": {"type": "string"}
                        }
                    }
                }
            }
        ]);
        let result = convert_tools(&tools).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "get_weather");
        assert_eq!(
            result[0].description,
            Some("Get the weather for a city".into())
        );
        assert_eq!(
            result[0].input_schema,
            serde_json::json!({
                "type": "object",
                "properties": {"city": {"type": "string"}}
            })
        );
        assert!(result[0].cache_control.is_none());
    }

    #[test]
    fn convert_tools_returns_none_for_empty_array() {
        let tools = serde_json::json!([]);
        assert!(convert_tools(&tools).is_none());
    }

    #[test]
    fn convert_tools_returns_none_for_non_array() {
        let tools = serde_json::json!("not an array");
        assert!(convert_tools(&tools).is_none());
    }

    #[test]
    fn convert_tools_skips_malformed_entries() {
        let tools: serde_json::Value = serde_json::json!([
            {"type": "function", "function": {"name": "valid"}},
            {"type": "function"},  // missing function → skipped
            {"type": "function", "function": {"name": "also_valid"}},
        ]);
        let result = convert_tools(&tools).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].name, "valid");
        assert_eq!(result[1].name, "also_valid");
    }

    #[test]
    fn convert_tools_uses_empty_object_when_parameters_missing() {
        let tools: serde_json::Value = serde_json::json!([
            {"type": "function", "function": {"name": "no_params"}}
        ]);
        let result = convert_tools(&tools).unwrap();
        assert_eq!(result[0].input_schema, serde_json::json!({}));
    }

    // ── cache_control injection ────────────────────────────────────

    #[test]
    fn inject_cache_control_marks_system_text() {
        let mut system = Some(SystemContent::Text("you are helpful".into()));
        let mut tools: Option<Vec<AnthropicTool>> = None;
        let mut messages = vec![AnthropicMessage {
            role: "user".into(),
            content: vec![ContentBlock::Text {
                text: "hello".into(),
                cache_control: None,
            }],
        }];

        inject_cache_control(&mut system, &mut tools, &mut messages);

        // System should be converted to Blocks with cache_control on the block
        match &system.unwrap() {
            SystemContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                match &blocks[0] {
                    ContentBlock::Text {
                        text,
                        cache_control,
                    } => {
                        assert_eq!(text, "you are helpful");
                        assert!(cache_control.is_some());
                    }
                }
            }
            _ => panic!("expected Blocks"),
        }
    }

    #[test]
    fn inject_cache_control_marks_last_tool() {
        let mut system: Option<SystemContent> = None;
        let mut tools = Some(vec![
            AnthropicTool {
                name: "tool_a".into(),
                description: None,
                input_schema: serde_json::json!({}),
                cache_control: None,
            },
            AnthropicTool {
                name: "tool_b".into(),
                description: None,
                input_schema: serde_json::json!({}),
                cache_control: None,
            },
        ]);
        let mut messages = vec![AnthropicMessage {
            role: "user".into(),
            content: vec![ContentBlock::Text {
                text: "hello".into(),
                cache_control: None,
            }],
        }];

        inject_cache_control(&mut system, &mut tools, &mut messages);

        let tools = tools.unwrap();
        assert!(tools[0].cache_control.is_none());
        assert!(tools[1].cache_control.is_some());
    }

    #[test]
    fn inject_cache_control_marks_last_user_message_before_final_turn() {
        let mut system: Option<SystemContent> = None;
        let mut tools: Option<Vec<AnthropicTool>> = None;
        let mut messages = vec![
            AnthropicMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text {
                    text: "first question".into(),
                    cache_control: None,
                }],
            },
            AnthropicMessage {
                role: "assistant".into(),
                content: vec![ContentBlock::Text {
                    text: "first answer".into(),
                    cache_control: None,
                }],
            },
            AnthropicMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text {
                    text: "second question".into(),
                    cache_control: None,
                }],
            },
        ];

        inject_cache_control(&mut system, &mut tools, &mut messages);

        // The first user message (index 0) should be marked — it's the last
        // user message before the final turn (index 2).
        assert!(cache_control_of(&messages[0].content[0]).is_some());
        // The assistant message should NOT be marked
        assert!(cache_control_of(&messages[1].content[0]).is_none());
        // The final user message should NOT be marked
        assert!(cache_control_of(&messages[2].content[0]).is_none());
    }

    #[test]
    fn inject_cache_control_skips_when_only_one_message() {
        let mut system: Option<SystemContent> = None;
        let mut tools: Option<Vec<AnthropicTool>> = None;
        let mut messages = vec![AnthropicMessage {
            role: "user".into(),
            content: vec![ContentBlock::Text {
                text: "hello".into(),
                cache_control: None,
            }],
        }];

        inject_cache_control(&mut system, &mut tools, &mut messages);

        // Single message → nothing to cache before it
        assert!(cache_control_of(&messages[0].content[0]).is_none());
    }

    // ── end-to-end with cache_control enabled ──────────────────────

    #[test]
    fn to_anthropic_request_injects_cache_control_when_enabled() {
        let mut req = ChatCompletionRequest::default();
        req.messages = vec![
            ChatMessage {
                role: "system".into(),
                content: serde_json::Value::String("you are helpful".into()),
                name: None,
                extra: serde_json::Map::new(),
            },
            ChatMessage {
                role: "user".into(),
                content: serde_json::Value::String("hello".into()),
                name: None,
                extra: serde_json::Map::new(),
            },
        ];

        let anthropic_req = AnthropicProvider::to_anthropic_request(&req, "claude-3", true);

        // System should be converted to Blocks with cache_control
        match &anthropic_req.system.unwrap() {
            SystemContent::Blocks(blocks) => {
                assert!(cache_control_of(&blocks[0]).is_some());
            }
            _ => panic!("expected Blocks when cache_control enabled"),
        }
    }

    #[test]
    fn to_anthropic_request_forwards_tools() {
        let mut req = ChatCompletionRequest::default();
        req.messages = vec![ChatMessage {
            role: "user".into(),
            content: serde_json::Value::String("what's the weather?".into()),
            name: None,
            extra: serde_json::Map::new(),
        }];
        req.tools = Some(serde_json::json!([
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "parameters": {"type": "object", "properties": {}}
                }
            }
        ]));

        let anthropic_req = AnthropicProvider::to_anthropic_request(&req, "claude-3", false);

        let tools = anthropic_req.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "get_weather");
    }

    #[test]
    fn to_anthropic_request_tools_with_cache_control() {
        let mut req = ChatCompletionRequest::default();
        req.messages = vec![
            ChatMessage {
                role: "system".into(),
                content: serde_json::Value::String("you are helpful".into()),
                name: None,
                extra: serde_json::Map::new(),
            },
            ChatMessage {
                role: "user".into(),
                content: serde_json::Value::String("hello".into()),
                name: None,
                extra: serde_json::Map::new(),
            },
        ];
        req.tools = Some(serde_json::json!([
            {"type": "function", "function": {"name": "tool_a"}},
            {"type": "function", "function": {"name": "tool_b"}},
        ]));

        let anthropic_req = AnthropicProvider::to_anthropic_request(&req, "claude-3", true);

        let tools = anthropic_req.tools.unwrap();
        assert_eq!(tools.len(), 2);
        // Only the last tool should have cache_control
        assert!(tools[0].cache_control.is_none());
        assert!(tools[1].cache_control.is_some());
    }
}
