use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use sag_core::{
    ApiKind, AppError, ChatCompletionRequest, ChatCompletionResponse, IrStreamEvent, StreamChunk,
    api::anthropic::{AnthropicContentBlock, AnthropicMessagesResponse},
};
use tokio::sync::mpsc;
use tracing::{error, instrument, trace, warn};

use crate::{
    AnthropicMessagesCodec, ChatCompletionsCodec, LlmProvider, ProviderBase, drain_sse_frames_with,
    emit_idle_timeout, flush_sse_buffer_with, inject_cache_control_value, spawn_in_current_span,
};

// ── SSE normalization (Chat bridge) ────────────────────────────────

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

/// Drain complete SSE frames from the buffer, normalizing Anthropic events
/// into OpenAI-compatible SSE chunks via the codec.
///
/// Returns `true` when a frame carried a 200-delivered upstream error.
fn drain_anthropic_sse_frames(
    buffer: &mut Vec<u8>,
    tx: &mpsc::UnboundedSender<StreamChunk>,
    state: &mut AnthropicStreamState,
) -> bool {
    drain_sse_frames_with(buffer, tx, |frame, tx| {
        process_anthropic_sse_frame(frame, tx, state)
    })
}

/// Drain remaining frames and flush any un-delimited trailing frame upon stream end.
fn flush_anthropic_sse_buffer(
    buffer: &mut Vec<u8>,
    tx: &mpsc::UnboundedSender<StreamChunk>,
    state: &mut AnthropicStreamState,
) -> bool {
    flush_sse_buffer_with(buffer, tx, |frame, tx| {
        process_anthropic_sse_frame(frame, tx, state)
    })
}

/// Emit an OpenAI-shaped content chunk from an IR stream event.
fn emit_openai_content_chunk(
    tx: &mpsc::UnboundedSender<StreamChunk>,
    state: &AnthropicStreamState,
    delta: serde_json::Value,
    index: u64,
) {
    let chunk = serde_json::json!({
        "id": state.id.as_deref().unwrap_or(""),
        "object": "chat.completion.chunk",
        "created": state.created,
        "model": state.model.as_deref().unwrap_or(""),
        "choices": [{
            "index": index,
            "delta": delta,
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

/// Parse a single Anthropic SSE frame via the codec and emit normalized
/// OpenAI-format chunks for the Chat bridge.
///
/// Returns `true` when the frame carried a 200-delivered upstream error.
fn process_anthropic_sse_frame(
    frame: &[u8],
    tx: &mpsc::UnboundedSender<StreamChunk>,
    state: &mut AnthropicStreamState,
) -> bool {
    let events = AnthropicMessagesCodec::decode_stream_frame(frame);
    let mut saw_error = false;

    for ev in events {
        match ev {
            IrStreamEvent::Start(meta) => {
                if state.id.is_none() {
                    state.id = meta.id;
                }
                if state.model.is_none() {
                    state.model = meta.model;
                }
            }
            IrStreamEvent::Usage(u) => {
                if u.prompt_tokens > 0 || u.completion_tokens > 0 {
                    let _ = tx.send(StreamChunk::Usage(u));
                }
            }
            IrStreamEvent::TextDelta { index, text } => {
                emit_openai_content_chunk(
                    tx,
                    state,
                    serde_json::json!({ "content": text }),
                    index as u64,
                );
            }
            IrStreamEvent::ReasoningDelta { index, text } => {
                emit_openai_content_chunk(
                    tx,
                    state,
                    serde_json::json!({ "reasoning_content": text }),
                    index as u64,
                );
            }
            IrStreamEvent::ToolCallStart { index, id, name } => {
                let mut tc = serde_json::Map::new();
                tc.insert("index".into(), serde_json::json!(index));
                tc.insert("type".into(), serde_json::json!("function"));
                if let Some(id) = id {
                    tc.insert("id".into(), serde_json::json!(id));
                }
                let mut func = serde_json::Map::new();
                if let Some(name) = name {
                    func.insert("name".into(), serde_json::json!(name));
                }
                func.insert("arguments".into(), serde_json::json!(""));
                tc.insert("function".into(), serde_json::Value::Object(func));
                emit_openai_content_chunk(tx, state, serde_json::json!({ "tool_calls": [tc] }), 0);
            }
            IrStreamEvent::ToolCallDelta {
                index,
                arguments_delta,
            } => {
                let tc = serde_json::json!({
                    "index": index,
                    "function": { "arguments": arguments_delta },
                });
                emit_openai_content_chunk(tx, state, serde_json::json!({ "tool_calls": [tc] }), 0);
            }
            IrStreamEvent::Finish {
                index,
                finish_reason,
            } => {
                let fr = finish_reason
                    .as_ref()
                    .map(|r| match r {
                        sag_core::IrFinishReason::Stop => "stop".to_string(),
                        sag_core::IrFinishReason::Length => "length".to_string(),
                        sag_core::IrFinishReason::ToolCalls => "tool_calls".to_string(),
                        sag_core::IrFinishReason::ContentFilter => "content_filter".to_string(),
                        sag_core::IrFinishReason::FunctionCall => "function_call".to_string(),
                        sag_core::IrFinishReason::Cancelled => "stop".to_string(),
                        sag_core::IrFinishReason::Other(s) => s.clone(),
                    })
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null);
                let chunk = serde_json::json!({
                    "id": state.id.as_deref().unwrap_or(""),
                    "object": "chat.completion.chunk",
                    "created": state.created,
                    "model": state.model.as_deref().unwrap_or(""),
                    "choices": [{
                        "index": index,
                        "delta": {},
                        "finish_reason": fr,
                    }],
                });
                let raw = format!(
                    "data: {}\n\n",
                    serde_json::to_string(&chunk).unwrap_or_default()
                )
                .into_bytes();
                let _ = tx.send(StreamChunk::Data(raw));
            }
            IrStreamEvent::End => {
                let _ = tx.send(StreamChunk::Data(b"data: [DONE]\n\n".to_vec()));
            }
            IrStreamEvent::Error(msg) => {
                warn!(status = 200, %msg, "upstream returned error");
                // Error BEFORE Data so the dispatch loop records the error
                // state before the client receives the forwarded frame.
                let _ = tx.send(StreamChunk::Error(msg));
                saw_error = true;
            }
            IrStreamEvent::Raw(bytes) => {
                // Forward unknown / error frames verbatim (the codec already
                // emitted Error for error events; Raw carries the frame).
                let _ = tx.send(StreamChunk::Data(bytes));
            }
        }
    }
    saw_error
}

// ── Provider ─────────────────────────────────────────────────────

/// Anthropic Messages API provider adapter.
///
/// URLs resolve from `ProviderConfig::apis` per protocol key
/// (`anthropic_messages`).
pub struct AnthropicProvider {
    base: Arc<ProviderBase>,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider from configuration.
    pub fn new(name: String, config: &sag_config::ProviderConfig) -> sag_core::Result<Self> {
        Self::new_with_proxy(name, config, None)
    }

    /// Create a new Anthropic provider with an explicit effective proxy.
    pub fn new_with_proxy(
        name: String,
        config: &sag_config::ProviderConfig,
        proxy: Option<&sag_config::ProxyConfig>,
    ) -> sag_core::Result<Self> {
        Ok(Self::from_base(Arc::new(ProviderBase::new_with_proxy(
            name, config, proxy,
        )?)))
    }

    /// Wrap an existing (possibly shared) provider base.
    pub(crate) fn from_base(base: Arc<ProviderBase>) -> Self {
        Self { base }
    }

    /// Convert an OpenAI-format request to an Anthropic Messages body
    /// (via Chat → IR → Anthropic codec). Returns a JSON `Value`.
    fn to_anthropic_request(
        request: &ChatCompletionRequest,
        real_model: &str,
        enable_cache_control: bool,
    ) -> serde_json::Value {
        let ir = ChatCompletionsCodec::decode_request(request);
        let (body, loss) = AnthropicMessagesCodec::encode_request_with_loss(&ir, real_model);
        if !loss.is_empty() {
            warn!(loss = ?loss.entries, "anthropic encode recorded losses");
        }
        let mut body = serde_json::to_value(&body)
            .expect("AnthropicMessagesRequest always serializes to JSON");
        if enable_cache_control {
            inject_cache_control_value(&mut body);
        }
        body
    }

    /// Convert an Anthropic response to the standard Chat format
    /// (via Anthropic → IR → Chat codec).
    fn from_anthropic_response(body: &serde_json::Value, model: &str) -> ChatCompletionResponse {
        let mut ir = AnthropicMessagesCodec::decode_response(&parse_anthropic_response(body));
        // Chat egress reports the real_model we requested (parity with
        // pre-refactor behavior); upstream id/content/usage stay intact.
        if !model.is_empty() {
            ir.model = model.to_string();
        }
        // ChatCompletionResponse.created is non-optional; IR leaves it
        // None for Anthropic (never fabricated from upstream). Use 0 here
        // only for the Chat wire shape — Anthropic ingress egress never
        // sees this field.
        if ir.created.is_none() {
            ir.created = Some(0);
        }
        ChatCompletionsCodec::encode_response(&ir)
    }

    /// Build the upstream Anthropic request body for a Chat request.
    fn build_upstream_body(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        force_stream: bool,
    ) -> serde_json::Value {
        let cache_control = self
            .base
            .effective_anthropic_cache_control(ApiKind::AnthropicMessages);
        let effective = if self.base.has_role_mappings(ApiKind::AnthropicMessages) {
            let mut req = request.clone();
            self.base
                .apply_role_mappings(ApiKind::AnthropicMessages, &mut req.messages);
            Self::to_anthropic_request(&req, real_model, cache_control)
        } else {
            Self::to_anthropic_request(request, real_model, cache_control)
        };
        let mut body = effective;
        if force_stream {
            body["stream"] = serde_json::json!(true);
        }
        body
    }

    /// Detect a 200-delivered error object in a non-stream response body.
    fn check_200_error(&self, body_text: &str) -> sag_core::Result<()> {
        if let Ok(raw) = serde_json::from_str::<serde_json::Value>(body_text)
            && crate::base::body_has_error_object(&raw)
        {
            let err_obj = raw.get("error").expect("checked error object");
            let body = crate::base::body_error_message(err_obj);
            let status = if crate::base::body_error_is_rate_limited(err_obj) {
                429
            } else {
                502
            };
            warn!(status = 200, %body_text, "upstream returned error");
            return Err(AppError::Upstream {
                provider: self.base.name.clone(),
                status,
                body,
            });
        }
        Ok(())
    }

    /// Native Anthropic Messages call (M5 native lane): send an
    /// Anthropic-shaped body, return the raw JSON response.
    ///
    /// Path is fixed to `/messages` (D11); `api_endpoint` is ignored.
    /// Applies provider `role_mappings` and, when enabled, injects
    /// `cache_control` breakpoints (B5 — parity with the translated path).
    pub async fn messages_native(
        &self,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
        timeout_secs: u64,
    ) -> sag_core::Result<serde_json::Value> {
        let body = self.prepare_native_body(body);
        let url = self.base.protocol_url(ApiKind::AnthropicMessages)?;
        let timeout = self
            .base
            .effective_timeout(ApiKind::AnthropicMessages, timeout_secs);
        let body_text = self
            .base
            .send_json(
                &url,
                ApiKind::AnthropicMessages,
                body,
                target_extra,
                Some(timeout),
            )
            .await?;
        self.check_200_error(&body_text)?;
        serde_json::from_str(&body_text)
            .map_err(|e| AppError::Internal(format!("failed to parse anthropic response: {e}")))
    }

    /// Apply role mappings and optional cache_control to a native
    /// Anthropic-shaped body before it is sent upstream (B5).
    fn prepare_native_body(&self, mut body: serde_json::Value) -> serde_json::Value {
        if self.base.has_role_mappings(ApiKind::AnthropicMessages)
            && let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut())
        {
            for msg in msgs.iter_mut() {
                if let Some(role) = msg.get("role").and_then(|r| r.as_str()).map(String::from)
                    && let Some(mapped) = self
                        .base
                        .role_mapping_for(ApiKind::AnthropicMessages, &role)
                {
                    msg["role"] = serde_json::json!(mapped);
                }
            }
        }
        if self
            .base
            .effective_anthropic_cache_control(ApiKind::AnthropicMessages)
        {
            inject_cache_control_value(&mut body);
        }
        body
    }

    /// Native Anthropic Messages streaming call: send an Anthropic-shaped
    /// body with `stream: true`, return raw Anthropic SSE frames as
    /// `StreamChunk::Data` (native passthrough — not OpenAI-normalized).
    pub async fn messages_stream_native(
        &self,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        let body = {
            let mut body = self.prepare_native_body(body);
            body["stream"] = serde_json::json!(true);
            body
        };
        let url = self.base.protocol_url(ApiKind::AnthropicMessages)?;
        let response = self
            .base
            .send_json_stream(&url, ApiKind::AnthropicMessages, body, target_extra)
            .await?;

        let (tx, rx) = mpsc::unbounded_channel::<StreamChunk>();
        let chunk_timeout = self
            .base
            .chunk_timeout_secs(ApiKind::AnthropicMessages)
            .map(std::time::Duration::from_secs);

        // The reader outlives this call: attach the provider/model span so
        // its chunk-read errors stay correlatable with the request.
        spawn_in_current_span(async move {
            use crate::util::find_sse_boundary;
            let mut byte_stream = response.bytes_stream();
            let mut buffer: Vec<u8> = Vec::new();
            let mut had_error = false;

            // Side-channel usage extraction from Anthropic frames.
            // Returns true when an in-stream error event was seen.
            let extract_usage = |frame: &[u8]| -> bool {
                let mut saw = false;
                let text = String::from_utf8_lossy(frame);
                for line in text.lines() {
                    let Some(rest) = line.trim().strip_prefix("data:") else {
                        continue;
                    };
                    let Ok(val) = serde_json::from_str::<serde_json::Value>(rest.trim_start())
                    else {
                        continue;
                    };
                    match val.get("type").and_then(|t| t.as_str()) {
                        Some("message_start") => {
                            if let Some(u) = val
                                .get("message")
                                .and_then(|m| m.get("usage"))
                                .filter(|u| !u.is_null())
                            {
                                emit_anthropic_usage(u, &tx);
                            }
                        }
                        Some("message_delta") => {
                            if let Some(u) = val.get("usage").filter(|u| !u.is_null()) {
                                emit_anthropic_usage(u, &tx);
                            }
                        }
                        Some("error") => {
                            saw = true;
                            let msg = val
                                .get("error")
                                .map(crate::base::body_error_message)
                                .unwrap_or_else(|| "upstream error".into());
                            warn!(status = 200, %msg, "upstream returned error");
                            let _ = tx.send(StreamChunk::Error(msg));
                        }
                        _ => {}
                    }
                }
                saw
            };

            loop {
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
                        while let Some(end) = find_sse_boundary(&buffer) {
                            let frame: Vec<u8> = buffer.drain(..end).collect();
                            let delim_len = if buffer.starts_with(b"\r\n\r\n") {
                                4
                            } else {
                                2
                            };
                            buffer.drain(..delim_len);
                            if extract_usage(&frame) {
                                had_error = true;
                            }
                            // Forward the raw Anthropic frame (native lane).
                            let mut data = b"data: ".to_vec();
                            // Frame may already include event:/data: lines —
                            // pass through as-is when it looks like SSE.
                            if frame.starts_with(b"data:") || frame.starts_with(b"event:") {
                                data = frame;
                                if !data.ends_with(b"\n\n") {
                                    data.extend_from_slice(b"\n\n");
                                }
                            } else {
                                data.extend_from_slice(&frame);
                                data.extend_from_slice(b"\n\n");
                            }
                            if !tx.is_closed() {
                                let _ = tx.send(StreamChunk::Data(data));
                            }
                            if had_error {
                                buffer.clear();
                                break;
                            }
                        }
                        if had_error {
                            break;
                        }
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
                        had_error = emit_idle_timeout(&tx, "anthropic", &chunk_timeout);
                        break;
                    }
                }
            }

            if !buffer.is_empty() && !had_error {
                if extract_usage(&buffer) {
                    had_error = true;
                }
                let mut data = buffer.clone();
                if !data.ends_with(b"\n\n") {
                    data.extend_from_slice(b"\n\n");
                }
                if !tx.is_closed() {
                    let _ = tx.send(StreamChunk::Data(data));
                }
                buffer.clear();
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

/// Deserialize an upstream Anthropic message body into the typed DTO.
///
/// The previous `Value`-based path never failed, so a body that does not match
/// the schema is decoded leniently instead of yielding an empty message: the
/// envelope fields are read directly, every content block that cannot be
/// parsed rides through as `Unknown` (the codec turns it into a loss + `Raw`),
/// and the unmodelled top-level fields stay in `extra` — the fallback fires
/// exactly when the body carries a shape the DTO cannot express, so it must not
/// be the path that drops fields.
fn parse_anthropic_response(body: &serde_json::Value) -> AnthropicMessagesResponse {
    if let Ok(parsed) = serde_json::from_value::<AnthropicMessagesResponse>(body.clone()) {
        return parsed;
    }
    warn!("upstream anthropic body does not match the typed schema; decoding leniently");

    // NOTE: mirrors `decode_anthropic_response` in
    // `sag-dispatch/src/dispatch/chat/mod.rs` — keep the two in sync (the
    // duplication is known; a shared helper is a separate change). Both keep
    // the whole body in `extra`; the overlap with typed fields is harmless
    // because `decode_response` re-inserts only the fields it typed and
    // `encode_response` skips the known keys, while the dispatch twin needs the
    // full body anyway for the native passthrough lane (`ir.raw`).

    let text = |key: &str| {
        body.get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    AnthropicMessagesResponse {
        id: text("id").unwrap_or_default(),
        kind: text("type").unwrap_or_else(|| "message".to_string()),
        role: text("role").unwrap_or_else(|| "assistant".to_string()),
        content: body
            .get("content")
            .and_then(serde_json::Value::as_array)
            .map(|blocks| blocks.iter().cloned().map(lenient_content_block).collect())
            .unwrap_or_default(),
        model: text("model").unwrap_or_default(),
        stop_reason: text("stop_reason"),
        stop_sequence: text("stop_sequence"),
        stop_details: body
            .get("stop_details")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        container: body
            .get("container")
            .and_then(|v| serde_json::from_value(v.clone()).ok()),
        usage: body
            .get("usage")
            .and_then(|u| serde_json::from_value(u.clone()).ok()),
        extra: body.as_object().cloned().unwrap_or_default(),
    }
}

/// Parse one content block, keeping the raw payload when the DTO cannot
/// express it.
fn lenient_content_block(value: serde_json::Value) -> AnthropicContentBlock {
    serde_json::from_value(value.clone()).unwrap_or(AnthropicContentBlock::Unknown(value))
}

/// Emit a Usage chunk from an Anthropic usage object (native stream path).
fn emit_anthropic_usage(usage: &serde_json::Value, tx: &mpsc::UnboundedSender<StreamChunk>) {
    let input = usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let output = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    if input == 0 && output == 0 {
        return;
    }
    let cached_tokens = usage
        .get("cache_read_input_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .or((input > 0).then_some(0));
    let cache_creation_tokens = usage
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .or((input > 0).then_some(0));
    let reasoning_tokens = usage
        .get("thinking_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let _ = tx.send(StreamChunk::Usage(sag_core::Usage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: input.saturating_add(output),
        cached_tokens,
        cache_creation_tokens,
        reasoning_tokens,
    }));
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
        let body = self.build_upstream_body(request, real_model, false);

        if !request.extra.is_empty() {
            let keys: Vec<&str> = request.extra.keys().map(|s| s.as_str()).collect();
            trace!(extra = ?keys, "forwarding non-standard extra parameters");
        }

        // Path fixed to apis.anthropic_messages (D11); api_endpoint ignored.
        let url = self.base.protocol_url(ApiKind::AnthropicMessages)?;
        let timeout = self
            .base
            .effective_timeout(ApiKind::AnthropicMessages, timeout_secs);
        let body_text = self
            .base
            .send_json(
                &url,
                ApiKind::AnthropicMessages,
                body,
                target_extra,
                Some(timeout),
            )
            .await?;

        self.check_200_error(&body_text)?;

        let raw: serde_json::Value = serde_json::from_str(&body_text)
            .map_err(|e| AppError::Internal(format!("failed to parse anthropic response: {e}")))?;

        Ok(Self::from_anthropic_response(&raw, real_model))
    }

    #[instrument(skip_all, fields(provider = %self.base.name, model = %real_model))]
    async fn chat_stream(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        _api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
        _timeout_secs: u64,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        let body = self.build_upstream_body(request, real_model, true);

        if !request.extra.is_empty() {
            let keys: Vec<&str> = request.extra.keys().map(|s| s.as_str()).collect();
            trace!(extra = ?keys, "forwarding non-standard extra parameters");
        }

        let url = self.base.protocol_url(ApiKind::AnthropicMessages)?;
        let response = self
            .base
            .send_json_stream(&url, ApiKind::AnthropicMessages, body, target_extra)
            .await?;

        let (tx, rx) = mpsc::unbounded_channel::<StreamChunk>();
        let chunk_timeout = self
            .base
            .chunk_timeout_secs(ApiKind::AnthropicMessages)
            .map(std::time::Duration::from_secs);

        // The reader outlives this call: attach the provider/model span so
        // its chunk-read errors stay correlatable with the request.
        spawn_in_current_span(async move {
            let mut byte_stream = response.bytes_stream();
            let mut buffer: Vec<u8> = Vec::new();
            let mut had_error = false;
            let mut state = AnthropicStreamState {
                created: chrono::Utc::now().timestamp() as u64,
                ..Default::default()
            };

            loop {
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
                        if drain_anthropic_sse_frames(&mut buffer, &tx, &mut state) {
                            buffer.clear();
                            had_error = true;
                            break;
                        }
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
                        had_error = emit_idle_timeout(&tx, "anthropic", &chunk_timeout);
                        break;
                    }
                }
            }

            if !buffer.is_empty()
                && !had_error
                && flush_anthropic_sse_buffer(&mut buffer, &tx, &mut state)
            {
                had_error = true;
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

    async fn anthropic_messages(
        &self,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
        timeout_secs: u64,
    ) -> sag_core::Result<serde_json::Value> {
        self.messages_native(body, target_extra, timeout_secs).await
    }

    async fn anthropic_messages_stream(
        &self,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        self.messages_stream_native(body, target_extra).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sag_core::ChatMessage;

    // ── to_anthropic_request (codec-based) ───────────────────────

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
        let body = AnthropicProvider::to_anthropic_request(&req, "claude-3", false);
        // Without role mapping, developer is not extracted as system.
        assert!(body.get("system").is_none());
        // developer falls through to user role in Anthropic encoding.
        assert_eq!(body["messages"][0]["role"], "user");
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
        let body = AnthropicProvider::to_anthropic_request(&req, "claude-3", false);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn to_anthropic_request_extracts_system_from_mapped_developer() {
        let req = ChatCompletionRequest {
            messages: vec![ChatMessage {
                role: "system".into(),
                content: serde_json::Value::String("you are helpful".into()),
                name: None,
                extra: serde_json::Map::new(),
            }],
            ..Default::default()
        };
        let body = AnthropicProvider::to_anthropic_request(&req, "claude-3", false);
        assert_eq!(body["system"], "you are helpful");
        // System message is lifted out of messages.
        assert!(
            body["messages"]
                .as_array()
                .map(|m| m.is_empty())
                .unwrap_or(true)
                || body["messages"]
                    .as_array()
                    .is_some_and(|m| m.iter().all(|x| x["role"] != "system"))
        );
    }

    #[test]
    fn to_anthropic_request_merges_multiple_system_messages_into_one_field() {
        // Multiple system messages: first becomes `system`, rest are not
        // double-injected as system (IR system messages are concatenated
        // into one `system` field).
        let req = ChatCompletionRequest {
            messages: vec![
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
            ],
            ..Default::default()
        };
        let body = AnthropicProvider::to_anthropic_request(&req, "claude-3", false);
        // Codec joins system blocks; at minimum first system is present.
        let sys = body["system"].clone();
        let sys_str = match &sys {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(arr) => arr
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(""),
            other => panic!("unexpected system shape: {other}"),
        };
        assert!(sys_str.contains("first system"));
    }

    // ── tools conversion (via codec) ─────────────────────────────

    #[test]
    fn to_anthropic_request_forwards_tools() {
        let req = ChatCompletionRequest {
            messages: vec![ChatMessage {
                role: "user".into(),
                content: serde_json::Value::String("what's the weather?".into()),
                name: None,
                extra: serde_json::Map::new(),
            }],
            tools: Some(serde_json::json!([
                {
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "parameters": {"type": "object", "properties": {}}
                    }
                }
            ])),
            ..Default::default()
        };

        let body = AnthropicProvider::to_anthropic_request(&req, "claude-3", false);
        assert_eq!(body["tools"][0]["name"], "get_weather");
        assert!(body["tools"][0]["input_schema"].is_object());
    }

    #[test]
    fn to_anthropic_request_tools_with_cache_control() {
        let req = ChatCompletionRequest {
            messages: vec![
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
            ],
            tools: Some(serde_json::json!([
                {"type": "function", "function": {"name": "tool_a"}},
                {"type": "function", "function": {"name": "tool_b"}},
            ])),
            ..Default::default()
        };

        let body = AnthropicProvider::to_anthropic_request(&req, "claude-3", true);
        // Only the last tool should have cache_control.
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn to_anthropic_request_injects_cache_control_when_enabled() {
        let req = ChatCompletionRequest {
            messages: vec![
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
            ],
            ..Default::default()
        };

        let body = AnthropicProvider::to_anthropic_request(&req, "claude-3", true);
        // System converted to blocks with cache_control on the block.
        assert!(body["system"].is_array());
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    }

    // ── max_tokens resolution ────────────────────────────────────

    #[test]
    fn to_anthropic_request_prefers_max_completion_tokens() {
        let req = ChatCompletionRequest {
            messages: vec![ChatMessage {
                role: "user".into(),
                content: serde_json::Value::String("hello".into()),
                name: None,
                extra: serde_json::Map::new(),
            }],
            max_completion_tokens: Some(10000),
            ..Default::default()
        };
        let body = AnthropicProvider::to_anthropic_request(&req, "claude-3", false);
        assert_eq!(body["max_tokens"], 10000);
    }

    #[test]
    fn to_anthropic_request_max_completion_tokens_wins_over_extra_max_tokens() {
        let req = ChatCompletionRequest {
            messages: vec![ChatMessage {
                role: "user".into(),
                content: serde_json::Value::String("hello".into()),
                name: None,
                extra: serde_json::Map::new(),
            }],
            max_completion_tokens: Some(10000),
            extra: serde_json::Map::from_iter([(
                "max_tokens".to_string(),
                serde_json::json!(2000),
            )]),
            ..Default::default()
        };
        let body = AnthropicProvider::to_anthropic_request(&req, "claude-3", false);
        assert_eq!(body["max_tokens"], 10000);
    }

    #[test]
    fn to_anthropic_request_dirty_extra_max_tokens_falls_back_to_default() {
        for dirty in [
            serde_json::json!("abc"),
            serde_json::json!(-1),
            serde_json::json!(12.5),
            serde_json::json!(u64::from(u32::MAX) + 1),
        ] {
            let req = ChatCompletionRequest {
                messages: vec![ChatMessage {
                    role: "user".into(),
                    content: serde_json::Value::String("hello".into()),
                    name: None,
                    extra: serde_json::Map::new(),
                }],
                extra: serde_json::Map::from_iter([("max_tokens".to_string(), dirty.clone())]),
                ..Default::default()
            };
            let body = AnthropicProvider::to_anthropic_request(&req, "claude-3", false);
            // Codec: dirty max_tokens → loss + documented default 1024.
            assert_eq!(
                body["max_tokens"], 1024,
                "dirty max_tokens={dirty} should fall back to default"
            );
        }
    }

    #[test]
    fn to_anthropic_request_missing_max_tokens_uses_documented_default() {
        let req = ChatCompletionRequest {
            messages: vec![ChatMessage {
                role: "user".into(),
                content: serde_json::Value::String("hello".into()),
                name: None,
                extra: serde_json::Map::new(),
            }],
            ..Default::default()
        };
        let body = AnthropicProvider::to_anthropic_request(&req, "claude-3", false);
        assert_eq!(
            body["max_tokens"], 1024,
            "missing max_tokens must use documented default (not silent)"
        );
    }

    // ── thinking integration ─────────────────────────────────────

    fn req_with_reasoning(effort: &str, max_tokens: u32) -> ChatCompletionRequest {
        let mut extra = serde_json::Map::new();
        extra.insert("max_tokens".into(), serde_json::json!(max_tokens));
        ChatCompletionRequest {
            model: "claude-sonnet-4-20250514".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: serde_json::Value::String("hello".into()),
                name: None,
                extra: serde_json::Map::new(),
            }],
            reasoning_effort: Some(effort.to_string()),
            extra,
            ..Default::default()
        }
    }

    #[test]
    fn to_anthropic_request_enables_thinking_for_high_with_room() {
        let req = req_with_reasoning("high", 10000);
        let body = AnthropicProvider::to_anthropic_request(&req, "claude-sonnet-4", false);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 8192);
    }

    #[test]
    fn to_anthropic_request_no_thinking_when_reasoning_effort_none() {
        let req = ChatCompletionRequest {
            model: "claude-sonnet-4-20250514".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: serde_json::Value::String("hello".into()),
                name: None,
                extra: serde_json::Map::new(),
            }],
            extra: serde_json::Map::from_iter([(
                "max_tokens".to_string(),
                serde_json::json!(10000),
            )]),
            ..Default::default()
        };
        let body = AnthropicProvider::to_anthropic_request(&req, "claude-sonnet-4", false);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn to_anthropic_request_no_thinking_for_none_effort() {
        let req = req_with_reasoning("none", 10000);
        let body = AnthropicProvider::to_anthropic_request(&req, "claude-sonnet-4", false);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn to_anthropic_request_no_thinking_when_downgraded_out() {
        let req = req_with_reasoning("high", 1024);
        let body = AnthropicProvider::to_anthropic_request(&req, "claude-sonnet-4", false);
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn to_anthropic_request_no_thinking_for_unknown_effort() {
        let req = req_with_reasoning("ultra", 10000);
        let body = AnthropicProvider::to_anthropic_request(&req, "claude-sonnet-4", false);
        assert!(body.get("thinking").is_none());
    }

    // ── from_anthropic_response (codec-based) ────────────────────

    #[test]
    fn from_anthropic_response_preserves_tool_use_and_thinking() {
        let raw = serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-3",
            "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "hi"},
                {"type": "tool_use", "id": "tu_1", "name": "fn", "input": {"a": 1}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        let chat = AnthropicProvider::from_anthropic_response(&raw, "claude-3");
        assert_eq!(chat.id, "msg_1");
        assert_eq!(chat.choices[0].finish_reason.as_deref(), Some("tool_calls"));
        let extra = &chat.choices[0].message.extra;
        let tcs = extra
            .get("tool_calls")
            .and_then(|v| v.as_array())
            .expect("tool_calls");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0]["function"]["name"], "fn");
        assert_eq!(
            extra.get("reasoning_content").and_then(|v| v.as_str()),
            Some("hmm")
        );
        let usage = chat.usage.expect("usage");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
    }

    #[test]
    fn from_anthropic_response_maps_stop_reasons() {
        for (stop, expect) in [
            ("end_turn", "stop"),
            ("max_tokens", "length"),
            ("tool_use", "tool_calls"),
        ] {
            let raw = serde_json::json!({
                "id": "x", "type": "message", "role": "assistant", "model": "m",
                "content": [{"type": "text", "text": "t"}],
                "stop_reason": stop,
                "usage": {"input_tokens": 1, "output_tokens": 1}
            });
            let chat = AnthropicProvider::from_anthropic_response(&raw, "m");
            assert_eq!(
                chat.choices[0].finish_reason.as_deref(),
                Some(expect),
                "stop={stop}"
            );
        }
    }

    /// The typed response parse must not turn a schema mismatch into an empty
    /// message: a body whose `usage` has wrong types still surfaces its text.
    #[test]
    fn from_anthropic_response_decodes_leniently_on_schema_mismatch() {
        let raw = serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "m",
            "content": [{"type": "text", "text": "still here"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": "10", "output_tokens": "5"}
        });
        let chat = AnthropicProvider::from_anthropic_response(&raw, "m");
        assert_eq!(chat.id, "msg_1");
        assert_eq!(
            chat.choices[0].message.content,
            serde_json::json!("still here")
        );
        assert_eq!(chat.choices[0].finish_reason.as_deref(), Some("stop"));
    }

    /// The lenient fallback fires exactly when the body carries a shape the
    /// DTO cannot express, so it must not be the path that drops fields:
    /// `container`, `stop_details` and unmodelled top-level keys must all reach
    /// `ir.raw` (they used to be hardcoded to `None` / dropped with `extra`).
    #[test]
    fn parse_anthropic_response_lenient_path_keeps_unmodelled_fields() {
        let raw = serde_json::json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "m",
            "content": [{"type": "text", "text": "still here"}],
            "stop_reason": "end_turn",
            "container": {"id": "c_1"},
            "stop_details": {"type": "refusal", "category": "x"},
            "some_future_field": {"k": 1},
            "usage": {"input_tokens": "10", "output_tokens": "5"}
        });
        let ir = AnthropicMessagesCodec::decode_response(&parse_anthropic_response(&raw));
        assert_eq!(
            ir.raw.get("container"),
            Some(&serde_json::json!({"id": "c_1"})),
            "container must survive the lenient path"
        );
        assert_eq!(
            ir.raw.get("stop_details"),
            Some(&serde_json::json!({"type": "refusal", "category": "x"})),
            "stop_details must survive the lenient path"
        );
        assert_eq!(
            ir.raw.get("some_future_field"),
            Some(&serde_json::json!({"k": 1})),
            "unmodelled top-level keys must survive the lenient path"
        );
    }

    // ── 200-delivered upstream error detection in SSE frames ────
    #[test]
    fn anthropic_sse_error_event_surfaces_in_stream_error() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();
        let mut state = AnthropicStreamState::default();

        let frame = b"data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"upstream overloaded\"}}\n\n";
        let saw_error = process_anthropic_sse_frame(frame, &tx, &mut state);
        drop(tx);

        assert!(saw_error, "an error event must signal an in-stream error");

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }
        // Error + forwarded Data (Raw after Error).
        assert!(chunks.len() >= 2, "expected Error + Data, got {chunks:?}");
        match &chunks[0] {
            StreamChunk::Error(msg) => assert_eq!(msg, "upstream overloaded"),
            other => panic!("expected an Error chunk first, got {other:?}"),
        }
        assert!(
            matches!(&chunks[1], StreamChunk::Data(d) if String::from_utf8_lossy(d).contains("upstream overloaded")),
            "the error frame must still be forwarded: {:?}",
            chunks[1]
        );
    }

    #[test]
    fn anthropic_sse_normal_frames_unaffected_by_error_detection() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();
        let mut state = AnthropicStreamState::default();

        let start = b"data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-3\",\"usage\":{\"input_tokens\":10}}}\n\n";
        assert!(!process_anthropic_sse_frame(start, &tx, &mut state));

        let delta = b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n";
        assert!(!process_anthropic_sse_frame(delta, &tx, &mut state));

        let done = b"data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n";
        assert!(!process_anthropic_sse_frame(done, &tx, &mut state));

        drop(tx);
        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }
        assert!(
            !chunks.iter().any(|c| matches!(c, StreamChunk::Error(_))),
            "normal frames must never produce an Error chunk"
        );
        assert!(
            chunks.iter().any(|c| matches!(c, StreamChunk::Usage(_))),
            "usage frames must still emit Usage chunks"
        );
        assert!(
            chunks.iter().any(|c| matches!(c, StreamChunk::Data(_))),
            "content frames must still be forwarded"
        );
    }

    #[test]
    fn anthropic_sse_thinking_delta_maps_to_reasoning_content() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();
        let mut state = AnthropicStreamState::default();
        let frame = b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"let me think\"}}\n\n";
        assert!(!process_anthropic_sse_frame(frame, &tx, &mut state));
        drop(tx);
        let mut found = false;
        while let Ok(chunk) = rx.try_recv() {
            if let StreamChunk::Data(d) = chunk {
                let s = String::from_utf8_lossy(&d);
                if s.contains("reasoning_content") && s.contains("let me think") {
                    found = true;
                }
            }
        }
        assert!(found, "thinking delta must map to reasoning_content");
    }

    #[test]
    fn anthropic_sse_tool_use_start_maps_to_tool_calls() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();
        let mut state = AnthropicStreamState::default();
        let frame = br#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu_1","name":"get_weather","input":{}}}"#;
        assert!(!process_anthropic_sse_frame(frame, &tx, &mut state));
        drop(tx);
        let mut found = false;
        while let Ok(chunk) = rx.try_recv() {
            if let StreamChunk::Data(d) = chunk {
                let s = String::from_utf8_lossy(&d);
                if s.contains("tool_calls") && s.contains("get_weather") && s.contains("tu_1") {
                    found = true;
                }
            }
        }
        assert!(found, "tool_use start must map to tool_calls delta");
    }

    #[test]
    fn drain_anthropic_sse_frames_stops_on_error_event() {
        let (tx, _rx) = mpsc::unbounded_channel::<StreamChunk>();
        let mut state = AnthropicStreamState::default();

        let mut buffer =
            b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"a\"}}\n\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"boom\"}}\n\n".to_vec();
        let saw_error = drain_anthropic_sse_frames(&mut buffer, &tx, &mut state);
        assert!(saw_error, "mid-buffer error event must be detected");

        let mut trailing = b"data: {\"type\":\"error\",\"error\":{\"message\":\"boom\"}}".to_vec();
        let saw_trailing = flush_anthropic_sse_buffer(&mut trailing, &tx, &mut state);
        assert!(saw_trailing, "trailing error must be detected");
        assert!(trailing.is_empty());
    }

    #[test]
    fn flush_anthropic_sse_buffer_flushes_un_delimited_trailing_frame() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();
        let mut state = AnthropicStreamState::default();
        let mut buffer = b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}".to_vec();

        flush_anthropic_sse_buffer(&mut buffer, &tx, &mut state);
        drop(tx);

        assert!(buffer.is_empty());

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            if let StreamChunk::Usage(u) = chunk {
                chunks.push(u);
            }
        }

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].completion_tokens, 42);
    }

    // ── chunk idle timeout (local HTTP mock) ─────────────────────

    fn anthropic_provider(
        base_url: &str,
        timeout_secs: u64,
        chunk_timeout_secs: Option<u64>,
    ) -> AnthropicProvider {
        AnthropicProvider::new(
            "test".into(),
            &crate::test_support::test_config(base_url, timeout_secs, Some(60), chunk_timeout_secs),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn chat_stream_continuous_chunks_survive_past_timeout_secs() {
        use tokio::io::AsyncWriteExt;
        let base_url = crate::test_support::spawn_server(|mut stream| async move {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            stream.flush().await.unwrap();
            for i in 0..4 {
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                let frame = format!(
                    "data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"chunk {i}\"}}}}\n\n"
                );
                stream.write_all(frame.as_bytes()).await.unwrap();
                stream.flush().await.unwrap();
            }
            stream
                .write_all(b"data: {\"type\":\"message_stop\"}\n\n")
                .await
                .unwrap();
            stream.flush().await.unwrap();
            let _ = stream.shutdown().await;
            drop(stream);
        })
        .await;

        let provider = anthropic_provider(&base_url, 1, Some(2));
        let mut stream = provider
            .chat_stream(
                &crate::test_support::chat_request(),
                "claude-3",
                "/messages",
                &HashMap::new(),
                1,
            )
            .await
            .expect("stream should be established");

        let mut data_frames: Vec<Vec<u8>> = Vec::new();
        let mut saw_done = false;
        let mut saw_error = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                StreamChunk::Data(d) => data_frames.push(d),
                StreamChunk::Error(_) => saw_error = true,
                StreamChunk::Done => saw_done = true,
                StreamChunk::Usage(_) => {}
            }
        }

        assert!(!saw_error, "continuous chunks must not trip idle timeout");
        assert!(saw_done, "stream must end with Done");
        assert_eq!(
            data_frames.len(),
            5,
            "expected 4 normalized text frames + 1 [DONE] frame, got {}",
            data_frames.len()
        );
        for (i, frame) in data_frames[..4].iter().enumerate() {
            let text = String::from_utf8(frame.clone()).unwrap();
            let json: serde_json::Value = text
                .strip_prefix("data: ")
                .and_then(|s| s.strip_suffix("\n\n"))
                .map(|s| serde_json::from_str(s).unwrap())
                .expect("normalized frame must be a `data: <json>` SSE frame");
            assert_eq!(
                json["object"], "chat.completion.chunk",
                "frame {i} must be normalized to OpenAI chunk format"
            );
            assert_eq!(
                json["choices"][0]["delta"]["content"],
                format!("chunk {i}"),
                "normalized frame {i} must carry the upstream text delta"
            );
        }
        assert_eq!(
            data_frames[4],
            b"data: [DONE]\n\n".to_vec(),
            "message_stop must be normalized to the [DONE] Data chunk"
        );
    }

    #[tokio::test]
    async fn chat_stream_chunk_idle_timeout_emits_error_not_done() {
        use tokio::io::AsyncWriteExt;
        let base_url = crate::test_support::spawn_server(|mut stream| async move {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            stream.flush().await.unwrap();
            stream
                .write_all(
                    b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"a\"}}\n\n",
                )
                .await
                .unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            stream
                .write_all(
                    b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"b\"}}\n\n",
                )
                .await
                .unwrap();
            stream.flush().await.unwrap();
            drop(stream);
        })
        .await;

        let provider = anthropic_provider(&base_url, 30, Some(1));
        let start = std::time::Instant::now();
        let mut stream = provider
            .chat_stream(
                &crate::test_support::chat_request(),
                "claude-3",
                "/messages",
                &HashMap::new(),
                30,
            )
            .await
            .expect("stream should be established");

        let mut saw_error = false;
        let mut saw_done = false;
        let mut saw_data = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                StreamChunk::Data(_) => saw_data = true,
                StreamChunk::Error(_) => saw_error = true,
                StreamChunk::Done => saw_done = true,
                StreamChunk::Usage(_) => {}
            }
        }
        let elapsed = start.elapsed();

        assert!(saw_data, "expected the first chunk before the silence");
        assert!(saw_error, "chunk idle timeout must emit an Error chunk");
        assert!(!saw_done, "a timed-out stream must not emit Done");
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "idle timeout should fire at ~1s: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn chat_stream_error_event_emits_error_chunk_and_no_done() {
        use tokio::io::AsyncWriteExt;
        let base_url = crate::test_support::spawn_server(|mut stream| async move {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            stream.flush().await.unwrap();
            stream
                .write_all(
                    b"data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
                )
                .await
                .unwrap();
            stream
                .write_all(
                    b"data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"upstream overloaded\"}}\n\n",
                )
                .await
                .unwrap();
            stream.flush().await.unwrap();
            stream
                .write_all(b"data: {\"type\":\"message_stop\"}\n\n")
                .await
                .unwrap();
            stream.flush().await.unwrap();
            drop(stream);
        })
        .await;

        let provider = anthropic_provider(&base_url, 30, None);
        let mut stream = provider
            .chat_stream(
                &crate::test_support::chat_request(),
                "claude-3",
                "/messages",
                &HashMap::new(),
                30,
            )
            .await
            .expect("stream should be established");

        let mut saw_error = false;
        let mut saw_done = false;
        let mut saw_forwarded_error_frame = false;
        let mut saw_partial = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                StreamChunk::Data(d) => {
                    let text = String::from_utf8_lossy(&d).to_string();
                    if text.contains("upstream overloaded") {
                        saw_forwarded_error_frame = true;
                    } else if text.contains("partial") {
                        saw_partial = true;
                    }
                }
                StreamChunk::Error(msg) => {
                    saw_error = true;
                    assert_eq!(msg, "upstream overloaded");
                }
                StreamChunk::Done => saw_done = true,
                StreamChunk::Usage(_) => {}
            }
        }

        assert!(saw_partial, "the delta before the error must be forwarded");
        assert!(saw_error, "the in-stream error must emit an Error chunk");
        assert!(
            saw_forwarded_error_frame,
            "the error frame must still be forwarded"
        );
        assert!(!saw_done, "an errored stream must never emit Done");
    }

    // ── 200-delivered upstream error detection (non-streaming) ──

    #[tokio::test]
    async fn chat_200_body_with_error_object_returns_upstream_error() {
        use tokio::io::AsyncWriteExt;
        let body = r#"{"type":"error","error":{"message":"upstream overloaded","code":"overloaded_error"}}"#;
        let base_url = crate::test_support::spawn_server(move |mut stream| {
            let body = body.to_string();
            async move {
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(resp.as_bytes()).await.unwrap();
                stream.flush().await.unwrap();
            }
        })
        .await;

        let provider = anthropic_provider(&base_url, 30, None);
        let err = provider
            .chat(
                &crate::test_support::chat_request(),
                "claude-3",
                "/messages",
                &HashMap::new(),
                30,
            )
            .await
            .unwrap_err();
        match err {
            AppError::Upstream { status, body, .. } => {
                assert_eq!(status, 502, "200-delivered error maps to 502");
                assert_eq!(body, "upstream overloaded", "message must be extracted");
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    // ── native messages path ─────────────────────────────────────

    #[tokio::test]
    async fn messages_native_returns_anthropic_json() {
        use tokio::io::AsyncWriteExt;
        let body = r#"{"id":"msg_n","type":"message","role":"assistant","model":"claude-3","content":[{"type":"text","text":"native"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2}}"#;
        let base_url = crate::test_support::spawn_server(move |mut stream| {
            let body = body.to_string();
            async move {
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(resp.as_bytes()).await.unwrap();
                stream.flush().await.unwrap();
            }
        })
        .await;

        let provider = anthropic_provider(&base_url, 30, None);
        let req_body = serde_json::json!({
            "model": "claude-3",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let resp = provider
            .messages_native(req_body, &HashMap::new(), 30)
            .await
            .expect("native messages ok");
        assert_eq!(resp["id"], "msg_n");
        assert_eq!(resp["content"][0]["text"], "native");
        assert_eq!(resp["usage"]["input_tokens"], 3);
    }
}
