//! Chat Completions ↔ IR codec (M2).
//!
//! Pure mapping: `ChatCompletionRequest`/`Response`/`SSE` ↔ IR types.
//! Not wired into dispatch/providers yet.

use serde_json::{Map, Value, json};

use sag_core::{
    ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, IrContentBlock,
    IrFinishReason, IrMessage, IrOutputItem, IrRequest, IrResponse, IrResponseMeta, IrRole,
    IrStreamEvent, IrTool, IrToolCall, LossReason, LossScope, Reasoning, Sampling, StopSequence,
};

use crate::util::extract_usage_from_value;

/// Stateless Chat Completions codec: decode to IR and encode back.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChatCompletionsCodec;

impl ChatCompletionsCodec {
    // ── request ────────────────────────────────────────────────────

    pub fn decode_request(chat: &ChatCompletionRequest) -> IrRequest {
        let mut loss = sag_core::LossInventory::default();
        let mut raw = Map::new();

        // Preserve all flatten-extra keys (including `max_tokens`, `top_k`,
        // `user`, …) so encode can put them back. Typed non-IR fields below
        // are copied into raw as well.
        for (k, v) in &chat.extra {
            raw.insert(k.clone(), v.clone());
        }

        // Typed fields without an IR home → raw (+ loss for n > 1).
        if let Some(n) = chat.n {
            raw.insert("n".into(), json!(n));
            if n > 1 {
                loss.push_loss(
                    LossScope::Request,
                    "n",
                    LossReason::UnsupportedByIr,
                    json!(n),
                );
            }
        }
        if let Some(v) = &chat.modalities {
            raw.insert("modalities".into(), v.clone());
        }
        if let Some(v) = &chat.audio {
            raw.insert("audio".into(), v.clone());
        }
        if let Some(v) = &chat.stream_options {
            raw.insert("stream_options".into(), v.clone());
        }
        if let Some(v) = &chat.prediction {
            raw.insert("prediction".into(), v.clone());
        }
        if let Some(v) = chat.logprobs {
            raw.insert("logprobs".into(), json!(v));
        }
        if let Some(v) = chat.top_logprobs {
            raw.insert("top_logprobs".into(), json!(v));
        }
        if let Some(v) = &chat.prompt_cache_key {
            raw.insert("prompt_cache_key".into(), json!(v));
        }
        if let Some(v) = &chat.prompt_cache_retention {
            raw.insert("prompt_cache_retention".into(), json!(v));
        }
        if let Some(v) = &chat.safety_identifier {
            raw.insert("safety_identifier".into(), json!(v));
        }
        if let Some(v) = chat.store {
            raw.insert("store".into(), json!(v));
        }
        if let Some(v) = &chat.service_tier {
            raw.insert("service_tier".into(), json!(v));
        }
        if let Some(v) = &chat.verbosity {
            raw.insert("verbosity".into(), json!(v));
        }
        if let Some(v) = &chat.moderation {
            raw.insert("moderation".into(), v.clone());
        }
        if let Some(v) = &chat.web_search_options {
            raw.insert("web_search_options".into(), v.clone());
        }
        if let Some(v) = chat.parallel_tool_calls {
            raw.insert("parallel_tool_calls".into(), json!(v));
        }

        // Absorb `max_tokens` into IR (and remove it from raw) only when the
        // value is a valid u32 — so encode never double-writes. Invalid values
        // (non-integer / > u32::MAX) keep the raw original and record a loss
        // rather than being silently swallowed.
        let legacy_max_tokens = match raw.get("max_tokens") {
            None => None,
            Some(v) => match v.as_u64().filter(|&n| n <= u32::MAX as u64) {
                Some(n) => {
                    raw.remove("max_tokens");
                    Some(n as u32)
                }
                None => {
                    loss.push_loss(
                        LossScope::Request,
                        "max_tokens",
                        LossReason::UnsupportedByIr,
                        v.clone(),
                    );
                    None
                }
            },
        };
        let max_tokens = match (chat.max_completion_tokens, legacy_max_tokens) {
            // Conflict: max_completion_tokens wins; record the dropped max_tokens.
            (Some(mct), Some(mt)) if mct != mt => {
                loss.push_loss(
                    LossScope::Request,
                    "max_tokens",
                    LossReason::UnsupportedByIr,
                    json!(mt),
                );
                Some(mct)
            }
            (Some(mct), _) => Some(mct),
            (None, legacy) => legacy,
        };

        // top_k lives in extra; map into Sampling and keep in raw for encode.
        let top_k = chat
            .extra
            .get("top_k")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32);

        let stop = chat
            .stop
            .as_ref()
            .map(|s| match s {
                StopSequence::Single(v) => vec![v.clone()],
                StopSequence::Multiple(v) => v.clone(),
            })
            .unwrap_or_default();

        let tools = match &chat.tools {
            Some(tv) => decode_tools(tv, &mut raw, &mut loss),
            None => Vec::new(),
        };

        let messages = chat
            .messages
            .iter()
            .enumerate()
            .map(|(i, m)| decode_message(m, i, &mut loss))
            .collect();

        IrRequest {
            model: chat.model.clone(),
            messages,
            sampling: Sampling {
                temperature: chat.temperature,
                top_p: chat.top_p,
                top_k,
                seed: chat.seed,
                presence_penalty: chat.presence_penalty,
                frequency_penalty: chat.frequency_penalty,
                logit_bias: chat.logit_bias.clone(),
            },
            max_tokens,
            stream: chat.stream.unwrap_or(false),
            tools,
            tool_choice: chat.tool_choice.clone().unwrap_or(Value::Null),
            response_format: chat.response_format.clone().unwrap_or(Value::Null),
            reasoning: Reasoning {
                effort: chat.reasoning_effort.clone(),
                budget_tokens: None,
                thinking: None,
            },
            stop,
            metadata: chat.metadata.clone().unwrap_or(Value::Null),
            raw,
            loss,
        }
    }

    pub fn encode_request(ir: &IrRequest) -> ChatCompletionRequest {
        Self::encode_request_with_loss(ir).0
    }

    /// Encode IR → Chat request, returning any encode-side loss entries
    /// (non-text tool_result content drops). Production callers should use
    /// this (or equivalent) so structural drops are never silent (B3).
    pub fn encode_request_with_loss(
        ir: &IrRequest,
    ) -> (ChatCompletionRequest, sag_core::LossInventory) {
        let mut raw = ir.raw.clone();
        let mut loss = ir.loss.clone();

        // tools: prefer original raw value (decode failure path); else rebuild.
        let tools = match raw.remove("tools") {
            Some(v) => Some(v),
            None if !ir.tools.is_empty() => Some(encode_tools(&ir.tools)),
            None => None,
        };

        let messages: Vec<ChatMessage> = ir
            .messages
            .iter()
            .enumerate()
            .flat_map(|(i, m)| encode_message(m, i, &mut loss))
            .collect();

        // Restore typed non-IR fields from raw; remaining keys → extra.
        let request = ChatCompletionRequest {
            model: ir.model.clone(),
            messages,
            temperature: ir.sampling.temperature,
            top_p: ir.sampling.top_p,
            n: raw.remove("n").and_then(|v| v.as_u64()).map(|v| v as u32),
            // Strategy: always write max_completion_tokens from IR when present.
            stream: ir.stream.then_some(true),
            stop: if ir.stop.is_empty() {
                None
            } else if ir.stop.len() == 1 {
                Some(StopSequence::Single(ir.stop[0].clone()))
            } else {
                Some(StopSequence::Multiple(ir.stop.clone()))
            },
            presence_penalty: ir.sampling.presence_penalty,
            frequency_penalty: ir.sampling.frequency_penalty,
            logit_bias: ir.sampling.logit_bias.clone(),
            max_completion_tokens: ir.max_tokens,
            modalities: take_raw_value(&mut raw, "modalities"),
            audio: take_raw_value(&mut raw, "audio"),
            stream_options: take_raw_value(&mut raw, "stream_options"),
            reasoning_effort: ir.reasoning.effort.clone(),
            metadata: null_to_none(ir.metadata.clone()),
            parallel_tool_calls: take_raw_bool(&mut raw, "parallel_tool_calls"),
            prediction: take_raw_value(&mut raw, "prediction"),
            tools,
            tool_choice: null_to_none(ir.tool_choice.clone()),
            response_format: null_to_none(ir.response_format.clone()),
            seed: ir.sampling.seed,
            logprobs: take_raw_bool(&mut raw, "logprobs"),
            top_logprobs: raw.remove("top_logprobs").and_then(|v| v.as_i64()),
            prompt_cache_key: take_raw_string(&mut raw, "prompt_cache_key"),
            prompt_cache_retention: take_raw_string(&mut raw, "prompt_cache_retention"),
            safety_identifier: take_raw_string(&mut raw, "safety_identifier"),
            store: take_raw_bool(&mut raw, "store"),
            service_tier: take_raw_string(&mut raw, "service_tier"),
            verbosity: take_raw_string(&mut raw, "verbosity"),
            moderation: take_raw_value(&mut raw, "moderation"),
            web_search_options: take_raw_value(&mut raw, "web_search_options"),
            extra: raw,
        };
        (request, loss)
    }

    // ── response ───────────────────────────────────────────────────

    pub fn decode_response(chat: &ChatCompletionResponse) -> IrResponse {
        let mut loss = sag_core::LossInventory::default();
        let mut raw = Map::new();
        let mut output = Vec::new();

        // Multi-choice: IR is single-path; preserve full choices + loss for index > 0.
        if chat.choices.len() > 1 {
            if let Ok(v) = serde_json::to_value(&chat.choices) {
                raw.insert("choices".into(), v);
            }
            for (i, choice) in chat.choices.iter().enumerate().skip(1) {
                if let Ok(v) = serde_json::to_value(choice) {
                    loss.push_loss(
                        LossScope::Response,
                        format!("choices[{i}]"),
                        LossReason::UnsupportedByIr,
                        v,
                    );
                }
            }
        }

        let mut finish_reason = None;
        if let Some(choice) = chat.choices.first() {
            finish_reason = choice
                .finish_reason
                .as_deref()
                .map(IrFinishReason::from_raw);

            let msg = &choice.message;

            // reasoning_content → Reasoning item
            if let Some(rc) = msg.extra.get("reasoning_content").and_then(|v| v.as_str()) {
                output.push(IrOutputItem::Reasoning {
                    text: rc.to_string(),
                });
            }

            // content → Text / Raw items
            decode_response_content(&msg.content, &mut output, &mut loss);

            // tool_calls → ToolCall items
            if let Some(tcs) = msg.extra.get("tool_calls").and_then(|v| v.as_array()) {
                for (i, tc) in tcs.iter().enumerate() {
                    if let Some(parsed) = decode_tool_call(tc, i) {
                        output.push(IrOutputItem::ToolCall(parsed));
                    }
                }
            }

            // Unmapped message-level extras → raw only (loss-free passthrough:
            // refusal/function_call/... round-trip intact; LossInventory only
            // records structural drops, matching the request-side policy).
            let mut message_extra = Map::new();
            for (k, v) in &msg.extra {
                if k == "reasoning_content" || k == "tool_calls" {
                    continue;
                }
                message_extra.insert(k.clone(), v.clone());
            }
            if !message_extra.is_empty() {
                raw.insert("message_extra".into(), Value::Object(message_extra));
            }
        }

        // Usage is already the IR-canonical shape on ChatCompletionResponse
        // (openai.rs patches nested details into flat fields before dispatch).
        // Do not re-run extract_usage_from_value here — it only understands
        // nested/raw JSON shapes and would drop flat cache_creation_tokens.
        let usage = chat.usage.clone();

        IrResponse {
            id: chat.id.clone(),
            created: Some(chat.created),
            model: chat.model.clone(),
            output,
            finish_reason,
            usage,
            raw,
            loss,
        }
    }

    pub fn encode_response(ir: &IrResponse) -> ChatCompletionResponse {
        let mut raw = ir.raw.clone();

        // Multi-choice preserved in raw → restore verbatim.
        let choices: Vec<ChatChoice> = match raw.remove("choices") {
            Some(v) => serde_json::from_value(v).unwrap_or_default(),
            None => vec![encode_response_choice(ir, &mut raw)],
        };

        ChatCompletionResponse {
            id: ir.id.clone(),
            object: "chat.completion".into(),
            created: ir.created.unwrap_or(0),
            model: ir.model.clone(),
            choices,
            usage: ir.usage.clone(),
        }
    }

    // ── stream ─────────────────────────────────────────────────────

    /// Decode a single SSE frame into 0..n IR stream events.
    pub fn decode_stream_frame(frame: &[u8]) -> Vec<IrStreamEvent> {
        let text = String::from_utf8_lossy(frame);
        let mut events = Vec::new();
        let mut saw_data = false;

        for line in text.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("data:") else {
                continue;
            };
            saw_data = true;
            let json_str = rest.trim_start();

            if json_str == "[DONE]" {
                events.push(IrStreamEvent::End);
                continue;
            }

            let Ok(val) = serde_json::from_str::<Value>(json_str) else {
                events.push(IrStreamEvent::Raw(frame.to_vec()));
                continue;
            };

            // Top-level error object. Emit Error + Raw (client-visible frame),
            // aligned with the Anthropic codec (B4).
            if let Some(err) = val.get("error").filter(|e| !e.is_null()) {
                let msg = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or_else(|| err.as_str().unwrap_or("upstream error"))
                    .to_string();
                events.push(IrStreamEvent::Error(msg));
                events.push(IrStreamEvent::Raw(frame.to_vec()));
                continue;
            }

            let mut frame_events = Vec::new();

            if let Some(uv) = val.get("usage").filter(|u| !u.is_null()) {
                frame_events.push(IrStreamEvent::Usage(extract_usage_from_value(uv)));
            }

            if let Some(choices) = val.get("choices").and_then(|c| c.as_array()) {
                for (ci, choice) in choices.iter().enumerate() {
                    let index = choice
                        .get("index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(ci as u64) as u32;

                    let delta = choice.get("delta");
                    let mut delta_mapped = false;

                    if let Some(delta) = delta {
                        if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                            // Empty first-frame content ("") is metadata-only:
                            // skip the no-op TextDelta so the frame falls
                            // through to Start(id/model/created) detection.
                            if !content.is_empty() {
                                frame_events.push(IrStreamEvent::TextDelta {
                                    index,
                                    text: content.to_string(),
                                });
                                delta_mapped = true;
                            }
                        }
                        if let Some(rc) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
                            frame_events.push(IrStreamEvent::ReasoningDelta {
                                index,
                                text: rc.to_string(),
                            });
                            delta_mapped = true;
                        }
                        if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                            for (ti, tc) in tcs.iter().enumerate() {
                                let tc_index = tc
                                    .get("index")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(ti as u64)
                                    as u32;
                                let id = tc.get("id").and_then(|v| v.as_str());
                                let name = tc
                                    .get("function")
                                    .and_then(|f| f.get("name"))
                                    .and_then(|v| v.as_str());
                                if id.is_some() || name.is_some() {
                                    frame_events.push(IrStreamEvent::ToolCallStart {
                                        index: tc_index,
                                        id: id.map(|s| s.to_string()),
                                        name: name.map(|s| s.to_string()),
                                    });
                                    delta_mapped = true;
                                }
                                if let Some(args) = tc
                                    .get("function")
                                    .and_then(|f| f.get("arguments"))
                                    .and_then(|v| v.as_str())
                                    .filter(|a| !a.is_empty())
                                {
                                    frame_events.push(IrStreamEvent::ToolCallDelta {
                                        index: tc_index,
                                        arguments_delta: args.to_string(),
                                    });
                                    delta_mapped = true;
                                }
                            }
                        }
                    }

                    if let Some(fr) = choice.get("finish_reason") {
                        if let Some(s) = fr.as_str() {
                            frame_events.push(IrStreamEvent::Finish {
                                index,
                                finish_reason: Some(IrFinishReason::from_raw(s)),
                            });
                        } else if fr.is_null() && !delta_mapped {
                            // Terminal-ish frame with explicit null finish and no payload.
                            // Only emit Finish(None) when the delta is empty/absent so
                            // intermediate content chunks are not misclassified.
                            // Id-bearing frames are start-like metadata → skip Finish
                            // so the frame falls through to Start(id/model/created).
                            let delta_empty = delta
                                .map(|d| d.as_object().is_none_or(Map::is_empty))
                                .unwrap_or(true);
                            let has_id = val.get("id").is_some_and(|v| !v.is_null());
                            if delta_empty && !has_id {
                                frame_events.push(IrStreamEvent::Finish {
                                    index,
                                    finish_reason: None,
                                });
                            }
                        }
                    }
                }
            }

            if frame_events.is_empty() {
                // Start-like chunk (id/model, no mappable payload) vs unknown JSON.
                if val.get("id").is_some_and(|v| !v.is_null()) {
                    frame_events.push(IrStreamEvent::Start(IrResponseMeta {
                        id: val.get("id").and_then(|v| v.as_str()).map(String::from),
                        created: val.get("created").and_then(|v| v.as_u64()),
                        model: val.get("model").and_then(|v| v.as_str()).map(String::from),
                    }));
                } else {
                    frame_events.push(IrStreamEvent::Raw(frame.to_vec()));
                }
            }

            events.extend(frame_events);
        }

        if !saw_data && events.is_empty() && !frame.is_empty() {
            // Non-SSE / comment-only frame: surface as Raw for pass-through.
            // Empty frames yield no events.
            if text.trim().is_empty() {
                return events;
            }
            // Lines without data: prefix → Raw so nothing is silently dropped.
            events.push(IrStreamEvent::Raw(frame.to_vec()));
        }

        events
    }

    /// Encode one IR stream event as Chat Completions SSE bytes.
    pub fn encode_stream_event(ev: &IrStreamEvent) -> Vec<u8> {
        let json = match ev {
            IrStreamEvent::Start(meta) => json!({
                "id": meta.id.clone().unwrap_or_default(),
                "object": "chat.completion.chunk",
                "created": meta.created.unwrap_or(0),
                "model": meta.model.clone().unwrap_or_default(),
                "choices": [],
            }),
            IrStreamEvent::TextDelta { index, text } => json!({
                "choices": [{
                    "index": index,
                    "delta": { "content": text },
                    "finish_reason": Value::Null,
                }],
            }),
            IrStreamEvent::ReasoningDelta { index, text } => json!({
                "choices": [{
                    "index": index,
                    "delta": { "reasoning_content": text },
                    "finish_reason": Value::Null,
                }],
            }),
            IrStreamEvent::ToolCallStart { index, id, name } => {
                let mut tc = Map::new();
                tc.insert("index".into(), json!(index));
                if id.is_some() || name.is_some() {
                    tc.insert("type".into(), json!("function"));
                }
                if let Some(id) = id {
                    tc.insert("id".into(), json!(id));
                }
                let mut func = Map::new();
                if let Some(name) = name {
                    func.insert("name".into(), json!(name));
                }
                func.insert("arguments".into(), json!(""));
                tc.insert("function".into(), Value::Object(func));
                json!({
                    "choices": [{
                        "index": 0,
                        "delta": { "tool_calls": [tc] },
                        "finish_reason": Value::Null,
                    }],
                })
            }
            IrStreamEvent::ToolCallDelta {
                index,
                arguments_delta,
            } => json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": index,
                            "function": { "arguments": arguments_delta },
                        }],
                    },
                    "finish_reason": Value::Null,
                }],
            }),
            IrStreamEvent::Finish {
                index,
                finish_reason,
            } => {
                let fr = match finish_reason {
                    Some(r) => serde_json::to_value(r).unwrap_or(Value::Null),
                    None => Value::Null,
                };
                json!({
                    "choices": [{
                        "index": index,
                        "delta": {},
                        "finish_reason": fr,
                    }],
                })
            }
            // Align with StreamChunk::to_sse_bytes usage shape.
            IrStreamEvent::Usage(u) => json!({ "usage": u }),
            // Align with StreamChunk::to_sse_bytes error shape.
            IrStreamEvent::Error(e) => json!({
                "error": { "code": "STREAM_ERROR", "message": e },
            }),
            IrStreamEvent::End => return b"data: [DONE]\n\n".to_vec(),
            IrStreamEvent::Raw(bytes) => return bytes.clone(),
        };
        format!("data: {}\n\n", json).into_bytes()
    }
}

// ── helpers ────────────────────────────────────────────────────────

fn null_to_none(v: Value) -> Option<Value> {
    match v {
        Value::Null => None,
        other => Some(other),
    }
}

fn take_raw_value(raw: &mut Map<String, Value>, key: &str) -> Option<Value> {
    raw.remove(key).filter(|v| !v.is_null())
}

fn take_raw_bool(raw: &mut Map<String, Value>, key: &str) -> Option<bool> {
    raw.remove(key).and_then(|v| v.as_bool())
}

fn take_raw_string(raw: &mut Map<String, Value>, key: &str) -> Option<String> {
    raw.remove(key).and_then(|v| v.as_str().map(String::from))
}

fn decode_message(msg: &ChatMessage, idx: usize, loss: &mut sag_core::LossInventory) -> IrMessage {
    let mut raw = Map::new();
    for (k, v) in &msg.extra {
        match k.as_str() {
            "tool_calls" | "tool_call_id" => {}
            _ => {
                raw.insert(k.clone(), v.clone());
            }
        }
    }

    let tool_call_id = msg
        .extra
        .get("tool_call_id")
        .and_then(|v| v.as_str())
        .map(String::from);

    let tool_calls = msg
        .extra
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .enumerate()
                .filter_map(|(i, tc)| decode_tool_call(tc, i))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    // Unparseable tool_calls array → loss (still empty tool_calls).
    if msg.extra.contains_key("tool_calls")
        && tool_calls.is_empty()
        && let Some(v) = msg.extra.get("tool_calls")
        && v.is_array()
        && !v.as_array().map(|a| a.is_empty()).unwrap_or(true)
    {
        loss.push_loss(
            LossScope::Message,
            format!("messages[{idx}].extra.tool_calls"),
            LossReason::UnsupportedByIr,
            v.clone(),
        );
    }

    let content = decode_content(&msg.content, idx, loss);

    IrMessage {
        role: IrRole::from_raw(msg.role.as_str()),
        content,
        name: msg.name.clone(),
        tool_call_id,
        tool_calls,
        raw,
    }
}

fn decode_content(
    content: &Value,
    msg_idx: usize,
    loss: &mut sag_core::LossInventory,
) -> Vec<IrContentBlock> {
    match content {
        Value::String(s) => vec![IrContentBlock::Text { text: s.clone() }],
        Value::Array(parts) => parts
            .iter()
            .enumerate()
            .map(|(pi, part)| {
                let ty = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match ty {
                    "text" => {
                        let text = part
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        IrContentBlock::Text { text }
                    }
                    "image_url" => IrContentBlock::Image { data: part.clone() },
                    "input_audio" => IrContentBlock::Audio { data: part.clone() },
                    "file" => IrContentBlock::Document { data: part.clone() },
                    _ => {
                        loss.push_loss(
                            LossScope::ContentBlock,
                            format!("messages[{msg_idx}].content[{pi}]"),
                            LossReason::UnsupportedByIr,
                            part.clone(),
                        );
                        IrContentBlock::Raw(part.clone())
                    }
                }
            })
            .collect(),
        Value::Null => Vec::new(),
        other => {
            loss.push_loss(
                LossScope::ContentBlock,
                format!("messages[{msg_idx}].content"),
                LossReason::UnsupportedByIr,
                other.clone(),
            );
            vec![IrContentBlock::Raw(other.clone())]
        }
    }
}

fn decode_response_content(
    content: &Value,
    output: &mut Vec<IrOutputItem>,
    loss: &mut sag_core::LossInventory,
) {
    match content {
        Value::String(s) => {
            if !s.is_empty() {
                output.push(IrOutputItem::Text { text: s.clone() });
            }
        }
        Value::Array(parts) => {
            for (pi, part) in parts.iter().enumerate() {
                let ty = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match ty {
                    "text" | "output_text" => {
                        let text = part
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        output.push(IrOutputItem::Text { text });
                    }
                    _ => {
                        loss.push_loss(
                            LossScope::ContentBlock,
                            format!("choices[0].message.content[{pi}]"),
                            LossReason::UnsupportedByIr,
                            part.clone(),
                        );
                        output.push(IrOutputItem::Raw(part.clone()));
                    }
                }
            }
        }
        Value::Null => {}
        other => {
            loss.push_loss(
                LossScope::ContentBlock,
                "choices[0].message.content",
                LossReason::UnsupportedByIr,
                other.clone(),
            );
            output.push(IrOutputItem::Raw(other.clone()));
        }
    }
}

/// Parse a Chat tool_call entry. `function.arguments` is a JSON string:
/// parse when possible, otherwise keep as `Value::String`.
fn decode_tool_call(tc: &Value, index: usize) -> Option<IrToolCall> {
    let id = tc
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let name = tc
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if id.is_empty() && name.is_empty() {
        return None;
    }
    let arguments = match tc.get("function").and_then(|f| f.get("arguments")) {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s)
            .ok()
            .filter(|v| !v.is_string())
            .unwrap_or_else(|| Value::String(s.clone())),
        Some(other) => other.clone(),
        None => json!({}),
    };
    let idx = tc
        .get("index")
        .and_then(|v| v.as_u64())
        .unwrap_or(index as u64) as u32;
    Some(IrToolCall {
        id,
        name,
        arguments,
        index: Some(idx),
    })
}

fn encode_tool_call(tc: &IrToolCall) -> Value {
    let arguments = match &tc.arguments {
        Value::String(s) => Value::String(s.clone()),
        other => Value::String(serde_json::to_string(other).unwrap_or_else(|_| "{}".into())),
    };
    json!({
        "id": tc.id,
        "type": "function",
        "function": {
            "name": tc.name,
            "arguments": arguments,
        },
    })
}

fn encode_message(
    msg: &IrMessage,
    idx: usize,
    loss: &mut sag_core::LossInventory,
) -> Vec<ChatMessage> {
    let mut out = Vec::new();
    let mut extra = msg.raw.clone();

    // tool role → OpenAI `role:"tool"` message (already carries tool_call_id).
    if msg.role == IrRole::Tool {
        if let Some(id) = &msg.tool_call_id {
            extra.insert("tool_call_id".into(), json!(id));
        }
        if !msg.tool_calls.is_empty() {
            let tcs: Vec<Value> = msg.tool_calls.iter().map(encode_tool_call).collect();
            extra.insert("tool_calls".into(), Value::Array(tcs));
        }
        let content = encode_content_lossy(&msg.content, idx, 0, loss);
        let role = serde_json::to_value(&msg.role)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "tool".into());
        out.push(ChatMessage {
            role,
            content,
            name: msg.name.clone(),
            extra,
        });
        return out;
    }

    // Split content: ToolResult → independent tool messages (B2);
    // ToolUse → message-level tool_calls only (never content array).
    let mut content_blocks: Vec<IrContentBlock> = Vec::new();
    let mut tool_results: Vec<(String, Map<String, Value>)> = Vec::new();
    for (bi, block) in msg.content.iter().enumerate() {
        match block {
            IrContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
                id: _,
            } => {
                let mut text = String::new();
                for (ci, inner) in content.iter().enumerate() {
                    match inner {
                        IrContentBlock::Text { text: t } => text.push_str(t),
                        other => {
                            loss.push_loss(
                                LossScope::ContentBlock,
                                format!("messages[{idx}].content[{bi}].content[{ci}]"),
                                LossReason::UnsupportedByIr,
                                serde_json::to_value(other).unwrap_or(Value::Null),
                            );
                        }
                    }
                }
                let mut tr_extra = Map::new();
                tr_extra.insert("tool_call_id".into(), json!(tool_call_id));
                if *is_error {
                    tr_extra.insert("is_error".into(), json!(true));
                }
                tool_results.push((text, tr_extra));
            }
            IrContentBlock::ToolUse { .. } => {
                // Not content — folded into tool_calls below (B2).
            }
            other => content_blocks.push(other.clone()),
        }
    }

    // tool_calls = message-level mirror ∪ content ToolUse blocks (OpenAI shape).
    let mut tool_calls: Vec<IrToolCall> = msg.tool_calls.clone();
    for block in &msg.content {
        if let IrContentBlock::ToolUse {
            id,
            name,
            arguments,
        } = block
            && !tool_calls.iter().any(|tc| &tc.id == id)
        {
            tool_calls.push(IrToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
                index: Some(tool_calls.len() as u32),
            });
        }
    }

    if !tool_calls.is_empty() {
        let tcs: Vec<Value> = tool_calls.iter().map(encode_tool_call).collect();
        extra.insert("tool_calls".into(), Value::Array(tcs));
    }

    let content = encode_content_lossy(&content_blocks, idx, 0, loss);

    let role = serde_json::to_value(&msg.role)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "user".into());

    // Emit the primary message when it has content/tool_calls/name, or when
    // there is nothing else to emit. Pure tool_result turns skip an empty
    // wrapper. Assistant with tool_calls may keep null/empty content (B2).
    let has_payload = !content_blocks.is_empty() || !tool_calls.is_empty() || msg.name.is_some();
    if has_payload || tool_results.is_empty() {
        out.push(ChatMessage {
            role,
            content,
            name: msg.name.clone(),
            extra,
        });
    }

    // ToolResult blocks → independent `role:"tool"` messages after the
    // primary message (OpenAI requires them after the tool_calls turn).
    for (text, tr_extra) in tool_results {
        out.push(ChatMessage {
            role: "tool".into(),
            content: Value::String(text),
            name: None,
            extra: tr_extra,
        });
    }

    out
}

/// Like [`encode_content`] but records loss for non-representable blocks
/// that still reach the content path (defensive; ToolUse/ToolResult are
/// split out before this in the request encode path).
fn encode_content_lossy(
    blocks: &[IrContentBlock],
    msg_idx: usize,
    block_idx: usize,
    loss: &mut sag_core::LossInventory,
) -> Value {
    for (bi, b) in blocks.iter().enumerate() {
        if matches!(
            b,
            IrContentBlock::ToolUse { .. } | IrContentBlock::ToolResult { .. }
        ) {
            loss.push_loss(
                LossScope::ContentBlock,
                format!("messages[{msg_idx}].content[{block_idx}][{bi}]"),
                LossReason::UnsupportedByIr,
                serde_json::to_value(b).unwrap_or(Value::Null),
            );
        }
    }
    encode_content(blocks)
}

fn encode_content(blocks: &[IrContentBlock]) -> Value {
    if blocks.is_empty() {
        return Value::Null;
    }
    // Single text block → string form (string ↔ single-text parts are equivalent).
    if blocks.len() == 1
        && let IrContentBlock::Text { text } = &blocks[0]
    {
        return Value::String(text.clone());
    }
    let parts: Vec<Value> = blocks
        .iter()
        .filter_map(|b| match b {
            IrContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
            IrContentBlock::Image { data } => Some(data.clone()),
            IrContentBlock::Audio { data } => Some(data.clone()),
            IrContentBlock::Document { data } => Some(data.clone()),
            IrContentBlock::Raw(v) => Some(v.clone()),
            IrContentBlock::Reasoning { text } => Some(json!({"type": "text", "text": text})),
            // ToolUse / ToolResult never enter the OpenAI content array —
            // they travel as message-level tool_calls / role:tool (B2).
            IrContentBlock::ToolUse { .. } | IrContentBlock::ToolResult { .. } => None,
        })
        .collect();
    if parts.is_empty() {
        return Value::Null;
    }
    Value::Array(parts)
}

fn decode_tools(
    v: &Value,
    raw: &mut Map<String, Value>,
    loss: &mut sag_core::LossInventory,
) -> Vec<IrTool> {
    let Some(arr) = v.as_array() else {
        raw.insert("tools".into(), v.clone());
        loss.push_loss(
            LossScope::Request,
            "tools",
            LossReason::UnsupportedByIr,
            v.clone(),
        );
        return Vec::new();
    };

    let mut tools = Vec::with_capacity(arr.len());
    for (i, item) in arr.iter().enumerate() {
        match parse_tool(item) {
            Some(t) => tools.push(t),
            None => {
                // Unparseable tool → keep whole original tools value in raw + loss.
                raw.insert("tools".into(), v.clone());
                loss.push_loss(
                    LossScope::Request,
                    format!("tools[{i}]"),
                    LossReason::UnsupportedByIr,
                    item.clone(),
                );
                return Vec::new();
            }
        }
    }
    tools
}

/// `IrTool.raw` key layout (pinned by tests): tool-level and function-level
/// extras are stored separately so encode can restore each to its original
/// position —
/// `{"__tool_extra": {...}, "__function_extra": {...}}`.
/// Both keys are always present (objects may be empty). The original `type`
/// (any value, including `"function"`) lives in `__tool_extra` so encode
/// restores it verbatim — never invents `"function"` for a type-less tool.
/// `strict` lives in `__function_extra` (OpenAI puts it inside `function`).
/// `parameters` maps to `input_schema`; when absent on the wire,
/// `input_schema` is [`Value::Null`] and encode must NOT inject a default
/// schema.
fn parse_tool(v: &Value) -> Option<IrTool> {
    let obj = v.as_object()?;
    if obj.get("type").and_then(|t| t.as_str()) == Some("hosted")
        || (obj.contains_key("type") && !obj.contains_key("function"))
    {
        return None;
    }
    let func = obj.get("function")?.as_object()?;
    let name = func.get("name")?.as_str()?.to_string();
    let description = func
        .get("description")
        .and_then(|d| d.as_str())
        .map(String::from);
    // No default injection: absent `parameters` → Null → encode omits the key.
    let input_schema = func.get("parameters").cloned().unwrap_or(Value::Null);

    let mut tool_extra = Map::new();
    for (k, val) in obj {
        // Keep original `type` (any value, including "function") so encode can
        // restore it verbatim; absent `type` stays absent (never invent one).
        if k != "function" {
            tool_extra.insert(k.clone(), val.clone());
        }
    }
    let mut function_extra = Map::new();
    for (k, val) in func {
        if k != "name" && k != "description" && k != "parameters" {
            function_extra.insert(k.clone(), val.clone());
        }
    }

    let mut raw = Map::new();
    raw.insert("__tool_extra".into(), Value::Object(tool_extra));
    raw.insert("__function_extra".into(), Value::Object(function_extra));

    Some(IrTool {
        name,
        description,
        input_schema,
        raw,
    })
}

fn encode_tools(tools: &[IrTool]) -> Value {
    let arr: Vec<Value> = tools
        .iter()
        .map(|t| {
            let mut func = Map::new();
            func.insert("name".into(), json!(t.name));
            if let Some(d) = &t.description {
                func.insert("description".into(), json!(d));
            }
            // Preserve absence: only write `parameters` when the source had it.
            if !t.input_schema.is_null() {
                func.insert("parameters".into(), t.input_schema.clone());
            }
            if let Some(Value::Object(function_extra)) = t.raw.get("__function_extra") {
                for (k, v) in function_extra {
                    func.insert(k.clone(), v.clone());
                }
            }
            let mut top = Map::new();
            top.insert("function".into(), Value::Object(func));
            // `type` comes only from tool_extra (original value if present);
            // never invent `"function"` when the source had no `type`.
            if let Some(Value::Object(tool_extra)) = t.raw.get("__tool_extra") {
                for (k, v) in tool_extra {
                    top.insert(k.clone(), v.clone());
                }
            }
            Value::Object(top)
        })
        .collect();
    Value::Array(arr)
}

fn encode_response_choice(ir: &IrResponse, raw: &mut Map<String, Value>) -> ChatChoice {
    let mut reasoning: Vec<String> = Vec::new();
    let mut content_blocks: Vec<IrContentBlock> = Vec::new();
    let mut tool_calls: Vec<IrToolCall> = Vec::new();

    for item in &ir.output {
        match item {
            IrOutputItem::Text { text } => {
                content_blocks.push(IrContentBlock::Text { text: text.clone() });
            }
            IrOutputItem::Reasoning { text } => reasoning.push(text.clone()),
            IrOutputItem::ToolCall(tc) => tool_calls.push(tc.clone()),
            IrOutputItem::Raw(v) => {
                // Unknown response content part: keep as content part.
                content_blocks.push(IrContentBlock::Raw(v.clone()));
            }
        }
    }

    let mut extra: Map<String, Value> = match raw.remove("message_extra") {
        Some(Value::Object(m)) => m,
        _ => Map::new(),
    };

    if !reasoning.is_empty() {
        extra.insert("reasoning_content".into(), json!(reasoning.join("")));
    }
    if !tool_calls.is_empty() {
        let tcs: Vec<Value> = tool_calls.iter().map(encode_tool_call).collect();
        extra.insert("tool_calls".into(), Value::Array(tcs));
    }

    // Response message content: single Text → string; else parts.
    let content = if content_blocks.is_empty() {
        Value::String(String::new())
    } else {
        encode_content(&content_blocks)
    };
    // encode_content returns Null for empty — response messages use "".
    let content = match content {
        Value::Null => Value::String(String::new()),
        other => other,
    };

    ChatChoice {
        index: 0,
        message: ChatMessage {
            role: "assistant".into(),
            content,
            name: None,
            extra,
        },
        finish_reason: ir.finish_reason.as_ref().map(|r| {
            serde_json::to_value(r)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default()
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sag_core::ChatCompletionRequest;

    /// Canonicalize JSON for semantic equality:
    /// - `max_tokens` only → treat as `max_completion_tokens`
    /// - both `max_tokens` keys present → keep both (so a double-write fails
    ///   the comparison; encode must emit at most one of them)
    /// - `stop` string → single-element array
    /// - `stream: false` / absent → absent
    /// - message `content` single `[{type:text,text}]` → string
    /// - message `content: null` → `""` when comparing response/request empties
    /// - tool_call `function.arguments` JSON strings → parsed values
    ///   (wire form is asserted separately: string stays string)
    fn normalize(v: Value) -> Value {
        match v {
            Value::Object(mut m) => {
                // max_tokens equivalence: only rewrite when max_completion_tokens is absent.
                if !m.contains_key("max_completion_tokens")
                    && let Some(mt) = m.remove("max_tokens")
                {
                    m.insert("max_completion_tokens".into(), mt);
                }
                // stream: false → absent
                if m.get("stream").and_then(|s| s.as_bool()) == Some(false) {
                    m.remove("stream");
                }
                // stop string → array
                if let Some(Value::String(s)) = m.get("stop").cloned() {
                    m.insert("stop".into(), json!([s]));
                }
                // messages: normalize content + tool_call arguments
                if let Some(msgs) = m.get_mut("messages").and_then(|x| x.as_array_mut()) {
                    for msg in msgs {
                        normalize_message(msg);
                    }
                }
                // choices[].message for responses
                if let Some(choices) = m.get_mut("choices").and_then(|x| x.as_array_mut()) {
                    for choice in choices {
                        if let Some(msg) = choice.get_mut("message") {
                            normalize_message(msg);
                        }
                    }
                }
                Value::Object(m.into_iter().map(|(k, val)| (k, normalize(val))).collect())
            }
            Value::Array(a) => Value::Array(a.into_iter().map(normalize).collect()),
            other => other,
        }
    }

    fn normalize_message(msg: &mut Value) {
        let Some(obj) = msg.as_object_mut() else {
            return;
        };
        if let Some(content) = obj.get("content").cloned() {
            let nc = match content {
                Value::Null => Value::String(String::new()),
                Value::Array(ref parts)
                    if parts.len() == 1
                        && parts[0].get("type").and_then(|t| t.as_str()) == Some("text") =>
                {
                    Value::String(
                        parts[0]
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string(),
                    )
                }
                other => other,
            };
            obj.insert("content".into(), nc);
        }
        if let Some(tcs) = obj.get_mut("tool_calls").and_then(|x| x.as_array_mut()) {
            for tc in tcs {
                if let Some(func) = tc.get_mut("function").and_then(|f| f.as_object_mut())
                    && let Some(args) = func.get("arguments").cloned()
                {
                    let parsed = match args {
                        Value::String(s) => {
                            serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s))
                        }
                        other => other,
                    };
                    func.insert("arguments".into(), parsed);
                }
            }
        }
    }

    fn parse_request(body: Value) -> ChatCompletionRequest {
        serde_json::from_value(body).expect("valid chat request")
    }

    fn roundtrip_request(body: Value) -> (IrRequest, ChatCompletionRequest) {
        let chat = parse_request(body);
        let ir = ChatCompletionsCodec::decode_request(&chat);
        let back = ChatCompletionsCodec::encode_request(&ir);
        (ir, back)
    }

    // ── request round-trips (5+ golden cases) ─────────────────────

    #[test]
    fn request_roundtrip_plain_string_content() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hello"}
            ],
            "temperature": 0.2,
            "stream": false
        });
        let (ir, back) = roundtrip_request(body.clone());
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert_eq!(
            normalize(serde_json::to_value(&back).unwrap()),
            normalize(body)
        );
    }

    #[test]
    fn request_roundtrip_content_parts_array() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "what is this"},
                    {"type": "image_url", "image_url": {"url": "https://x/y.png"}}
                ]
            }]
        });
        let (ir, back) = roundtrip_request(body.clone());
        assert!(ir.loss.is_empty());
        assert_eq!(ir.messages[0].content.len(), 2);
        assert!(matches!(
            ir.messages[0].content[1],
            IrContentBlock::Image { .. }
        ));
        assert_eq!(
            normalize(serde_json::to_value(&back).unwrap()),
            normalize(body)
        );
    }

    #[test]
    fn request_roundtrip_unknown_content_part_records_loss() {
        let body = json!({
            "model": "m",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "refusal", "detail": "nope"}
                ]
            }]
        });
        let (ir, back) = roundtrip_request(body.clone());
        assert_eq!(ir.loss.len(), 1);
        assert_eq!(ir.loss.entries[0].field, "messages[0].content[1]");
        assert_eq!(ir.loss.entries[0].reason, LossReason::UnsupportedByIr);
        // Raw block still carries the part → structure preserved.
        assert_eq!(
            normalize(serde_json::to_value(&back).unwrap()),
            normalize(body)
        );
    }

    /// B2: multi-turn Anthropic tool_use + tool_result → Chat encode must
    /// produce a legal OpenAI body: `role:"tool"` + `tool_call_id`, assistant
    /// `tool_calls` OpenAI shape, no tool blocks in content arrays.
    #[test]
    fn request_multiturn_anthropic_tools_encode_openai_shape() {
        let anth = json!({
            "model": "claude-sonnet-4",
            "max_tokens": 2048,
            "messages": [
                {"role": "user", "content": "weather in SF?"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_1", "name": "get_weather",
                     "input": {"city": "SF"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_1", "content": "72F"}
                ]},
                {"role": "assistant", "content": "It is 72F."}
            ],
            "tools": [{
                "name": "get_weather",
                "description": "Get weather",
                "input_schema": {"type": "object"}
            }]
        });
        let ir = crate::codec::AnthropicMessagesCodec::decode_request(&anth);
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        let (req, loss) = ChatCompletionsCodec::encode_request_with_loss(&ir);
        assert!(loss.is_empty(), "encode loss: {:?}", loss);
        let body = serde_json::to_value(&req).expect("serialize chat request");
        let msgs = body["messages"].as_array().expect("messages array");

        // user
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "weather in SF?");
        // assistant: OpenAI tool_calls shape, content has no tool_use block
        assert_eq!(msgs[1]["role"], "assistant");
        let tc = &msgs[1]["tool_calls"][0];
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["id"], "tu_1");
        assert_eq!(tc["function"]["name"], "get_weather");
        assert!(tc["function"]["arguments"].is_string());
        let c1 = &msgs[1]["content"];
        if let Some(arr) = c1.as_array() {
            assert!(
                arr.iter().all(|p| p["type"] != "tool_use"),
                "tool_use must not sit in content: {arr:?}"
            );
        }
        // tool result → independent role:"tool" message with tool_call_id
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "tu_1");
        assert_eq!(msgs[2]["content"], "72F");
        // closing assistant
        assert_eq!(msgs[3]["role"], "assistant");
        assert_eq!(msgs[3]["content"], "It is 72F.");
        // No content array anywhere carries tool_result either.
        for m in msgs {
            if let Some(arr) = m["content"].as_array() {
                assert!(
                    arr.iter().all(|p| p["type"] != "tool_result"),
                    "tool_result must not sit in content: {m}"
                );
            }
        }
    }

    #[test]
    fn request_roundtrip_tool_calls_and_tool_call_id() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "user", "content": "weather in SF?"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"city\":\"SF\"}"}
                    }]
                },
                {
                    "role": "tool",
                    "content": "72F",
                    "tool_call_id": "call_1"
                }
            ]
        });
        let (ir, back) = roundtrip_request(body.clone());
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert_eq!(ir.messages[1].tool_calls.len(), 1);
        assert_eq!(ir.messages[1].tool_calls[0].name, "get_weather");
        assert_eq!(
            ir.messages[1].tool_calls[0].arguments,
            json!({"city": "SF"})
        );
        assert_eq!(ir.messages[2].tool_call_id.as_deref(), Some("call_1"));
        // Wire form: arguments stays a string when the input was a string
        // (normalize still treats JSON-equivalent strings/objects as equal).
        let encoded = serde_json::to_value(&back).unwrap();
        let back_args = encoded["messages"][1]["tool_calls"][0]["function"]["arguments"].clone();
        assert!(
            back_args.is_string(),
            "arguments must remain a JSON string on the wire, got {back_args:?}"
        );
        assert_eq!(normalize(encoded), normalize(body));
    }

    #[test]
    fn request_roundtrip_tools_tool_choice_response_format() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "fn",
                    "description": "d",
                    "parameters": {"type": "object", "properties": {"a": {"type": "string"}}}
                }
            }],
            "tool_choice": "auto",
            "response_format": {"type": "json_object"}
        });
        let (ir, back) = roundtrip_request(body.clone());
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert_eq!(ir.tools.len(), 1);
        assert_eq!(ir.tools[0].name, "fn");
        assert_eq!(ir.tool_choice, json!("auto"));
        assert_eq!(
            normalize(serde_json::to_value(&back).unwrap()),
            normalize(body)
        );
    }

    /// Golden (a): `strict` lives inside `function` — round-trip must not
    /// grow a top-level `strict`, and raw uses the pinned key layout.
    #[test]
    fn request_roundtrip_tool_strict_stays_in_function() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "fn",
                    "description": "d",
                    "parameters": {"type": "object", "properties": {"a": {"type": "string"}}},
                    "strict": true
                }
            }]
        });
        let (ir, back) = roundtrip_request(body.clone());
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        // Pinned raw layout: both reserved keys always present; original
        // `type` is retained in `__tool_extra` for verbatim restore.
        assert_eq!(
            ir.tools[0].raw.get("__tool_extra"),
            Some(&json!({"type": "function"}))
        );
        assert_eq!(
            ir.tools[0].raw.get("__function_extra"),
            Some(&json!({"strict": true}))
        );
        assert!(!ir.tools[0].raw.contains_key("strict"));

        let encoded = serde_json::to_value(&back).unwrap();
        let tool = &encoded["tools"][0];
        assert!(
            tool.get("strict").is_none(),
            "strict must not appear at tool top-level: {tool:?}"
        );
        assert_eq!(tool["function"]["strict"], json!(true));
        assert_eq!(
            normalize(encoded),
            normalize(body),
            "function-level keys return to function"
        );
    }

    /// Golden (b): absent `parameters` must not grow a default schema.
    #[test]
    fn request_roundtrip_tool_without_parameters_stays_absent() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "type": "function",
                "function": {"name": "no_params"}
            }]
        });
        let (ir, back) = roundtrip_request(body.clone());
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert_eq!(
            ir.tools[0].input_schema,
            Value::Null,
            "no default injection"
        );

        let encoded = serde_json::to_value(&back).unwrap();
        assert!(
            encoded["tools"][0]["function"].get("parameters").is_none(),
            "parameters must stay absent: {:?}",
            encoded["tools"][0]["function"]
        );
        assert_eq!(normalize(encoded), normalize(body));
    }

    /// Golden (c): tool-level and function-level extras return to their
    /// original positions via `__tool_extra` / `__function_extra`.
    #[test]
    fn request_roundtrip_tool_extras_return_to_original_positions() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "type": "function",
                "custom": 1,
                "function": {
                    "name": "fn",
                    "fn_extra": "a",
                    "strict": false
                }
            }]
        });
        let (ir, back) = roundtrip_request(body.clone());
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert_eq!(
            ir.tools[0].raw.get("__tool_extra"),
            Some(&json!({"type": "function", "custom": 1}))
        );
        assert_eq!(
            ir.tools[0].raw.get("__function_extra"),
            Some(&json!({"fn_extra": "a", "strict": false}))
        );

        let encoded = serde_json::to_value(&back).unwrap();
        let tool = &encoded["tools"][0];
        assert_eq!(tool.get("custom"), Some(&json!(1)), "tool-level stays top");
        assert_eq!(
            tool["function"].get("fn_extra"),
            Some(&json!("a")),
            "function-level stays in function"
        );
        assert_eq!(tool["function"].get("strict"), Some(&json!(false)));
        assert_eq!(normalize(encoded), normalize(body));
    }

    /// Odd-shaped tool: non-`"function"` `type` with a function key must
    /// round-trip verbatim (not be rewritten to `"function"`).
    #[test]
    fn request_roundtrip_tool_custom_type_preserved() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "type": "custom",
                "function": {"name": "fn", "description": "d"}
            }]
        });
        let (ir, back) = roundtrip_request(body.clone());
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert_eq!(ir.tools.len(), 1);
        assert_eq!(
            ir.tools[0].raw.get("__tool_extra"),
            Some(&json!({"type": "custom"}))
        );

        let encoded = serde_json::to_value(&back).unwrap();
        assert_eq!(
            encoded["tools"][0]["type"],
            json!("custom"),
            "type must round-trip verbatim, not be rewritten to function"
        );
        assert_eq!(normalize(encoded), normalize(body));
    }

    /// Tool with no `type` key at all: encode must not invent `"function"`.
    #[test]
    fn request_roundtrip_tool_absent_type_stays_absent() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "function": {"name": "fn"}
            }]
        });
        let (ir, back) = roundtrip_request(body.clone());
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        let encoded = serde_json::to_value(&back).unwrap();
        assert!(
            encoded["tools"][0].get("type").is_none(),
            "absent type must not grow an invented \"function\": {:?}",
            encoded["tools"][0]
        );
        assert_eq!(normalize(encoded), normalize(body));
    }

    #[test]
    fn request_roundtrip_raw_extra_and_stream_options() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
            "stream_options": {"include_usage": true},
            "modalities": ["text"],
            "store": true,
            "service_tier": "auto",
            "max_tokens": 256,
            "user": "u-1",
            "custom_vendor_field": {"k": 1}
        });
        let (ir, back) = roundtrip_request(body.clone());
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert!(ir.stream);
        assert_eq!(ir.max_tokens, Some(256));
        assert_eq!(
            ir.raw.get("stream_options"),
            Some(&json!({"include_usage": true}))
        );
        assert_eq!(ir.raw.get("user"), Some(&json!("u-1")));
        assert_eq!(ir.raw.get("custom_vendor_field"), Some(&json!({"k": 1})));
        // encode writes only max_completion_tokens — no dual-key double-write.
        let encoded = serde_json::to_value(&back).unwrap();
        assert_eq!(encoded.get("max_completion_tokens"), Some(&json!(256)));
        assert!(
            encoded.get("max_tokens").is_none(),
            "max_tokens must be absorbed into IR, not re-emitted on the wire"
        );
        assert_eq!(normalize(encoded), normalize(body));
    }

    #[test]
    fn request_n2_records_loss_and_does_not_panic() {
        let body = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "n": 2
        });
        let (ir, back) = roundtrip_request(body.clone());
        assert_eq!(ir.loss.len(), 1);
        assert_eq!(ir.loss.entries[0].field, "n");
        assert_eq!(ir.loss.entries[0].value, json!(2));
        // n still round-trips via raw (loss is advisory, not a drop).
        let encoded = serde_json::to_value(&back).unwrap();
        assert_eq!(encoded.get("n"), Some(&json!(2)));
        assert_eq!(normalize(encoded), normalize(body));
    }

    #[test]
    fn request_max_completion_tokens_preferred_over_max_tokens() {
        let body = json!({
            "model": "m",
            "messages": [],
            "max_completion_tokens": 100,
            "max_tokens": 999
        });
        let (ir, back) = roundtrip_request(body);
        assert_eq!(ir.max_tokens, Some(100));
        // Conflicting values: max_completion_tokens wins, max_tokens is a
        // structural conflict drop recorded in LossInventory.
        assert!(!ir.loss.is_empty(), "conflict must record loss");
        assert_eq!(ir.loss.entries[0].field, "max_tokens");
        assert_eq!(ir.loss.entries[0].reason, LossReason::UnsupportedByIr);
        assert_eq!(ir.loss.entries[0].value, json!(999));
        // Wire carries at most one max_tokens-family key.
        let encoded = serde_json::to_value(&back).unwrap();
        assert_eq!(encoded.get("max_completion_tokens"), Some(&json!(100)));
        assert!(encoded.get("max_tokens").is_none());
        let max_family_keys = ["max_tokens", "max_completion_tokens"]
            .iter()
            .filter(|k| encoded.get(**k).is_some())
            .count();
        assert!(
            max_family_keys <= 1,
            "at most one max_tokens key on the wire, got {encoded:?}"
        );
    }

    /// Equal-path: `max_tokens == max_completion_tokens` → no conflict loss,
    /// encode emits a single `max_completion_tokens` key.
    #[test]
    fn request_equal_max_tokens_paths_no_loss_single_wire_key() {
        let body = json!({
            "model": "m",
            "messages": [],
            "max_tokens": 100,
            "max_completion_tokens": 100
        });
        let (ir, back) = roundtrip_request(body);
        assert_eq!(ir.max_tokens, Some(100));
        assert!(
            ir.loss.is_empty(),
            "equal values must not loss: {:?}",
            ir.loss
        );

        let encoded = serde_json::to_value(&back).unwrap();
        assert_eq!(encoded.get("max_completion_tokens"), Some(&json!(100)));
        assert!(encoded.get("max_tokens").is_none());
        let max_family_keys = ["max_tokens", "max_completion_tokens"]
            .iter()
            .filter(|k| encoded.get(**k).is_some())
            .count();
        assert_eq!(max_family_keys, 1, "wire must carry exactly one key");
    }

    /// Non-integer `max_tokens`: absorption fails → loss recorded, raw value
    /// kept (not silently swallowed).
    #[test]
    fn request_invalid_max_tokens_non_integer_kept_with_loss() {
        let body = json!({
            "model": "m",
            "messages": [],
            "max_tokens": "lots"
        });
        let (ir, back) = roundtrip_request(body);
        assert_eq!(ir.max_tokens, None, "non-integer must not absorb into IR");
        assert_eq!(ir.loss.len(), 1, "loss: {:?}", ir.loss);
        assert_eq!(ir.loss.entries[0].field, "max_tokens");
        assert_eq!(ir.loss.entries[0].value, json!("lots"));

        let encoded = serde_json::to_value(&back).unwrap();
        assert_eq!(
            encoded.get("max_tokens"),
            Some(&json!("lots")),
            "raw max_tokens must be preserved on absorption failure"
        );
        assert!(encoded.get("max_completion_tokens").is_none());
    }

    /// `max_tokens` exceeding u32: absorption fails → loss + raw preserved
    /// (no silent `as u32` truncation).
    #[test]
    fn request_max_tokens_over_u32_kept_with_loss() {
        let over_u32 = (u32::MAX as u64) + 1;
        let body = json!({
            "model": "m",
            "messages": [],
            "max_tokens": over_u32
        });
        let (ir, back) = roundtrip_request(body);
        assert_eq!(ir.max_tokens, None, "over-u32 must not absorb into IR");
        assert_eq!(ir.loss.len(), 1, "loss: {:?}", ir.loss);
        assert_eq!(ir.loss.entries[0].field, "max_tokens");
        assert_eq!(ir.loss.entries[0].value, json!(over_u32));

        let encoded = serde_json::to_value(&back).unwrap();
        assert_eq!(encoded.get("max_tokens"), Some(&json!(over_u32)));
        assert!(encoded.get("max_completion_tokens").is_none());
    }

    // ── response round-trips ──────────────────────────────────────

    fn parse_response(body: Value) -> ChatCompletionResponse {
        serde_json::from_value(body).expect("valid chat response")
    }

    #[test]
    fn response_roundtrip_plain_text() {
        let body = json!({
            "id": "resp-1",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Hello!"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        });
        let chat = parse_response(body.clone());
        let ir = ChatCompletionsCodec::decode_response(&chat);
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert_eq!(
            ir.output,
            vec![IrOutputItem::Text {
                text: "Hello!".into()
            }]
        );
        assert_eq!(ir.finish_reason, Some(IrFinishReason::Stop));
        let back = ChatCompletionsCodec::encode_response(&ir);
        assert_eq!(
            normalize(serde_json::to_value(&back).unwrap()),
            normalize(body)
        );
    }

    #[test]
    fn response_roundtrip_reasoning_and_tool_calls() {
        let body = json!({
            "id": "resp-2",
            "object": "chat.completion",
            "created": 1700000001,
            "model": "o4",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "done",
                    "reasoning_content": "think hard",
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": {"name": "fn", "arguments": "{\"x\":1}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": null
        });
        let chat = parse_response(body.clone());
        let ir = ChatCompletionsCodec::decode_response(&chat);
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert!(matches!(ir.output[0], IrOutputItem::Reasoning { .. }));
        assert!(matches!(ir.output[1], IrOutputItem::Text { .. }));
        assert!(matches!(ir.output[2], IrOutputItem::ToolCall(_)));
        assert_eq!(ir.finish_reason, Some(IrFinishReason::ToolCalls));
        let back = ChatCompletionsCodec::encode_response(&ir);
        assert_eq!(
            normalize(serde_json::to_value(&back).unwrap()),
            normalize(body)
        );
    }

    /// Unknown message fields (`refusal`, `function_call`, vendor extras)
    /// round-trip loss-free: they are raw/extra passthrough, not structural loss.
    #[test]
    fn response_roundtrip_message_refusal_extra() {
        let body = json!({
            "id": "resp-x",
            "object": "chat.completion",
            "created": 1700000002,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "hi",
                    "refusal": null,
                    "function_call": null,
                    "audio": {"id": "a1"},
                    "vendor_flag": true
                },
                "finish_reason": "stop"
            }],
            "usage": null
        });
        let chat = parse_response(body.clone());
        let ir = ChatCompletionsCodec::decode_response(&chat);
        assert!(
            ir.loss.is_empty(),
            "message extras round-trip loss-free, got: {:?}",
            ir.loss
        );
        let back = ChatCompletionsCodec::encode_response(&ir);
        let msg = &back.choices[0].message;
        assert_eq!(msg.extra.get("refusal"), Some(&Value::Null));
        assert_eq!(msg.extra.get("function_call"), Some(&Value::Null));
        assert_eq!(msg.extra.get("vendor_flag"), Some(&json!(true)));
        assert_eq!(
            normalize(serde_json::to_value(&back).unwrap()),
            normalize(body)
        );
    }

    #[test]
    fn response_roundtrip_usage_nested_details() {
        // Build usage via the shared extractor (nested details → flat fields),
        // as openai.rs does after parse.
        let nested = json!({
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "total_tokens": 150,
            "prompt_tokens_details": {"cached_tokens": 30, "cache_creation_input_tokens": 70},
            "completion_tokens_details": {"reasoning_tokens": 15}
        });
        let usage = extract_usage_from_value(&nested);
        assert_eq!(usage.cached_tokens, Some(30));
        assert_eq!(usage.cache_creation_tokens, Some(70));
        assert_eq!(usage.reasoning_tokens, Some(15));

        let body = json!({
            "id": "u1",
            "object": "chat.completion",
            "created": 1,
            "model": "m",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": usage
        });
        let chat = parse_response(body.clone());
        let ir = ChatCompletionsCodec::decode_response(&chat);
        let u = ir.usage.clone().expect("usage");
        assert_eq!(u.cached_tokens, Some(30));
        assert_eq!(u.cache_creation_tokens, Some(70));
        assert_eq!(u.reasoning_tokens, Some(15));
        let back = ChatCompletionsCodec::encode_response(&ir);
        assert_eq!(
            normalize(serde_json::to_value(&back).unwrap()),
            normalize(body)
        );
    }

    #[test]
    fn response_multiple_choices_records_loss() {
        let body = json!({
            "id": "m1",
            "object": "chat.completion",
            "created": 1,
            "model": "m",
            "choices": [
                {"index": 0, "message": {"role": "assistant", "content": "a"}, "finish_reason": "stop"},
                {"index": 1, "message": {"role": "assistant", "content": "b"}, "finish_reason": "stop"}
            ],
            "usage": null
        });
        let chat = parse_response(body.clone());
        let ir = ChatCompletionsCodec::decode_response(&chat);
        assert_eq!(ir.loss.len(), 1);
        assert_eq!(ir.loss.entries[0].field, "choices[1]");
        assert_eq!(ir.output.len(), 1);
        // Full choices preserved in raw → encode restores both.
        let back = ChatCompletionsCodec::encode_response(&ir);
        assert_eq!(back.choices.len(), 2);
        assert_eq!(
            normalize(serde_json::to_value(&back).unwrap()),
            normalize(body)
        );
    }

    // ── stream round-trips ────────────────────────────────────────

    fn rt_event(ev: IrStreamEvent) {
        let bytes = ChatCompletionsCodec::encode_stream_event(&ev);
        let decoded = ChatCompletionsCodec::decode_stream_frame(&bytes);
        assert!(
            decoded.contains(&ev),
            "round-trip failed for {ev:?}: got {decoded:?} from bytes {:?}",
            String::from_utf8_lossy(&bytes)
        );
    }

    #[test]
    fn stream_text_delta_sequence_roundtrip() {
        for (i, text) in ["Hel", "lo", "!"].iter().enumerate() {
            rt_event(IrStreamEvent::TextDelta {
                index: i as u32,
                text: text.to_string(),
            });
        }
        // Decode a realistic multi-frame sequence.
        let f1 =
            br#"data: {"choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}"#;
        let f2 =
            br#"data: {"choices":[{"index":0,"delta":{"content":" there"},"finish_reason":null}]}"#;
        let e1 = ChatCompletionsCodec::decode_stream_frame(f1);
        let e2 = ChatCompletionsCodec::decode_stream_frame(f2);
        assert_eq!(
            e1,
            vec![IrStreamEvent::TextDelta {
                index: 0,
                text: "Hi".into()
            }]
        );
        assert_eq!(
            e2,
            vec![IrStreamEvent::TextDelta {
                index: 0,
                text: " there".into()
            }]
        );
    }

    #[test]
    fn stream_tool_call_incremental_roundtrip() {
        rt_event(IrStreamEvent::ToolCallStart {
            index: 0,
            id: Some("call_1".into()),
            name: Some("fn".into()),
        });
        rt_event(IrStreamEvent::ToolCallDelta {
            index: 0,
            arguments_delta: "{\"a\":".into(),
        });
        rt_event(IrStreamEvent::ToolCallDelta {
            index: 0,
            arguments_delta: "1}".into(),
        });

        // Decode a realistic start frame with id+name+empty args → Start only
        // (empty arguments filtered), then args fragment → Delta.
        let start = br#"data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"fn","arguments":""}}]},"finish_reason":null}]}"#;
        let evs = ChatCompletionsCodec::decode_stream_frame(start);
        assert_eq!(
            evs,
            vec![IrStreamEvent::ToolCallStart {
                index: 0,
                id: Some("c1".into()),
                name: Some("fn".into()),
            }]
        );
    }

    #[test]
    fn stream_usage_frame_roundtrip_with_nested_details() {
        // Nested details (aligned with util tests).
        let nested = br#"data: {"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150,"prompt_tokens_details":{"cached_tokens":30},"completion_tokens_details":{"reasoning_tokens":12}}}"#;
        let evs = ChatCompletionsCodec::decode_stream_frame(nested);
        assert_eq!(evs.len(), 1);
        let IrStreamEvent::Usage(u) = &evs[0] else {
            panic!("expected Usage, got {evs:?}");
        };
        assert_eq!(u.cached_tokens, Some(30));
        assert_eq!(u.reasoning_tokens, Some(12));
        rt_event(evs[0].clone());

        // Empty prompt_tokens_details object → cached_tokens Some(0)
        // (details-shape quirk, NOT anomalous usage).
        let empty_details = br#"data: {"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150,"prompt_tokens_details":{}}}"#;
        let evs = ChatCompletionsCodec::decode_stream_frame(empty_details);
        let IrStreamEvent::Usage(u) = &evs[0] else {
            panic!("expected Usage");
        };
        assert_eq!(u.cached_tokens, Some(0), "empty details object → Some(0)");
        rt_event(evs[0].clone());
    }

    /// Anomalous usage per util.rs `usage_is_anomalous`
    /// (`completion_tokens == 0 && prompt_tokens > 0`) and the fallback
    /// behavior of `extract_usage_from_value`:
    /// - `total > prompt` → derive `completion = total - prompt`
    /// - `total == prompt` (or absent) → completion stays 0 (stays anomalous)
    #[test]
    fn stream_usage_zero_completion_fallback_semantics() {
        // Case 1: ct==0 on the wire, total > pt → fallback derives ct.
        let derive =
            br#"data: {"usage":{"prompt_tokens":100,"completion_tokens":0,"total_tokens":150}}"#;
        let evs = ChatCompletionsCodec::decode_stream_frame(derive);
        let IrStreamEvent::Usage(u) = &evs[0] else {
            panic!("expected Usage, got {evs:?}");
        };
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(u.completion_tokens, 50, "fallback: total − prompt");
        assert_eq!(u.total_tokens, 150);
        rt_event(evs[0].clone());

        // Case 2: ct==0 && pt>0 with total == pt → stays 0 (still anomalous).
        let stuck =
            br#"data: {"usage":{"prompt_tokens":100,"completion_tokens":0,"total_tokens":100}}"#;
        let evs = ChatCompletionsCodec::decode_stream_frame(stuck);
        let IrStreamEvent::Usage(u) = &evs[0] else {
            panic!("expected Usage, got {evs:?}");
        };
        assert_eq!(u.prompt_tokens, 100);
        assert_eq!(
            u.completion_tokens, 0,
            "no total>pt fallback → stays anomalous (ct==0, pt>0)"
        );
        assert_eq!(u.total_tokens, 100);
        rt_event(evs[0].clone());
    }

    /// Real OpenAI first frame: id/model/created metadata with
    /// `delta.content: ""` must map to `Start` — metadata must not be
    /// silently dropped behind a no-op empty TextDelta.
    #[test]
    fn stream_first_frame_empty_content_maps_to_start() {
        let frame = br#"data: {"id":"chatcmpl-abc","object":"chat.completion.chunk","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#;
        let evs = ChatCompletionsCodec::decode_stream_frame(frame);
        assert_eq!(
            evs,
            vec![IrStreamEvent::Start(IrResponseMeta {
                id: Some("chatcmpl-abc".into()),
                created: Some(1700000000),
                model: Some("gpt-4o".into()),
            })],
            "empty-content first frame must preserve id/model/created as Start"
        );

        // encode → decode keeps the metadata.
        let bytes = ChatCompletionsCodec::encode_stream_event(&evs[0]);
        let back = ChatCompletionsCodec::decode_stream_frame(&bytes);
        assert!(
            back.contains(&evs[0]),
            "Start metadata must survive round-trip"
        );
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("chatcmpl-abc"));
        assert!(text.contains("gpt-4o"));

        // Empty delta + metadata (no content key) also maps to Start.
        let empty_delta = br#"data: {"id":"chatcmpl-def","object":"chat.completion.chunk","created":1700000001,"model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":null}]}"#;
        let evs = ChatCompletionsCodec::decode_stream_frame(empty_delta);
        match &evs[..] {
            [IrStreamEvent::Start(meta)] => {
                assert_eq!(meta.id.as_deref(), Some("chatcmpl-def"));
                assert_eq!(meta.model.as_deref(), Some("gpt-4o"));
                assert_eq!(meta.created, Some(1700000001));
            }
            // Finish(None) alone would also drop id — must not happen without Start.
            other => panic!("expected lone Start, got {other:?}"),
        }
    }

    #[test]
    fn stream_done_and_error_roundtrip() {
        let evs = ChatCompletionsCodec::decode_stream_frame(b"data: [DONE]\n\n");
        assert_eq!(evs, vec![IrStreamEvent::End]);
        rt_event(IrStreamEvent::End);

        let err = br#"data: {"error":{"message":"burst limit","code":429}}"#;
        let evs = ChatCompletionsCodec::decode_stream_frame(err);
        // B4: error frames surface Error + Raw so clients see the envelope.
        assert_eq!(
            evs,
            vec![
                IrStreamEvent::Error("burst limit".into()),
                IrStreamEvent::Raw(err.to_vec()),
            ]
        );
        rt_event(IrStreamEvent::Error("burst limit".into()));

        // StreamChunk-shaped error also round-trips.
        rt_event(IrStreamEvent::Error("boom".into()));
    }

    #[test]
    fn stream_finish_and_reasoning_roundtrip() {
        rt_event(IrStreamEvent::Finish {
            index: 0,
            finish_reason: Some(IrFinishReason::Stop),
        });
        rt_event(IrStreamEvent::Finish {
            index: 0,
            finish_reason: None,
        });
        rt_event(IrStreamEvent::ReasoningDelta {
            index: 0,
            text: "hmm".into(),
        });

        let finish = br#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}"#;
        let evs = ChatCompletionsCodec::decode_stream_frame(finish);
        assert_eq!(
            evs,
            vec![IrStreamEvent::Finish {
                index: 0,
                finish_reason: Some(IrFinishReason::Length),
            }]
        );
    }

    #[test]
    fn stream_invalid_json_yields_raw() {
        let frame = b"data: {not-json\n\n";
        let evs = ChatCompletionsCodec::decode_stream_frame(frame);
        assert_eq!(evs, vec![IrStreamEvent::Raw(frame.to_vec())]);
        // Raw encodes as-is.
        let bytes = ChatCompletionsCodec::encode_stream_event(&evs[0]);
        assert_eq!(bytes, frame.to_vec());
    }

    // ── usage extraction regression (shared helper) ───────────────

    #[test]
    fn extract_usage_from_value_matches_prior_stream_semantics() {
        // Fallback: missing completion derived from total − prompt.
        let u = extract_usage_from_value(&json!({"prompt_tokens": 80, "completion_tokens": 20}));
        assert_eq!(u.total_tokens, 100);

        // reasoning_tokens floors completion_tokens.
        let u = extract_usage_from_value(
            &json!({"prompt_tokens": 100, "completion_tokens": 10, "completion_tokens_details": {"reasoning_tokens": 50}}),
        );
        assert_eq!(u.completion_tokens, 50);
        assert_eq!(u.reasoning_tokens, Some(50));
    }

    // ── M8 audio / file content round-trips ───────────────────────

    #[test]
    fn input_audio_and_file_roundtrip() {
        let chat: ChatCompletionRequest = serde_json::from_value(serde_json::json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "look"},
                    {"type": "file", "file": {"file_data": "data:application/pdf;base64,AA=="}},
                    {"type": "input_audio", "input_audio": {"data": "AAAA", "format": "wav"}}
                ]
            }]
        }))
        .expect("valid chat request");
        let ir = ChatCompletionsCodec::decode_request(&chat);
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert_eq!(ir.messages[0].content.len(), 3);
        assert!(
            matches!(ir.messages[0].content[1], IrContentBlock::Document { .. }),
            "file → Document"
        );
        assert!(
            matches!(ir.messages[0].content[2], IrContentBlock::Audio { .. }),
            "input_audio → Audio"
        );
        let (back, loss) = ChatCompletionsCodec::encode_request_with_loss(&ir);
        assert!(loss.is_empty(), "encode loss: {:?}", loss);
        let parts = back.messages[0]
            .content
            .as_array()
            .expect("content parts array");
        assert_eq!(parts[1]["type"], "file");
        assert_eq!(parts[2]["type"], "input_audio");
    }
}
