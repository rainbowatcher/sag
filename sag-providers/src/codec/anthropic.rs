//! Anthropic Messages ↔ IR codec (M5).
//!
//! Pure mapping: Anthropic Messages request/response/SSE ↔ IR types.
//!
//! Ingress side: `decode_request` / `encode_response` / `encode_stream_event`.
//! Upstream side: `encode_request` / `decode_response` / `decode_stream_frame`.
//!
//! Fixes plan §1.3 defects 1–8 (tool_use loss, array content loss, system
//! blocks, silent max_tokens=1024, thinking/stream thinking, stream tool_use,
//! silent event drops, silent thinking disable).

use std::cell::RefCell;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use sag_core::{
    IrContentBlock, IrFinishReason, IrMessage, IrOutputItem, IrRequest, IrResponse, IrResponseMeta,
    IrRole, IrStreamEvent, IrTool, IrToolCall, LossReason, LossScope, Reasoning, Sampling, Usage,
    api::anthropic::{
        AnthropicContentBlock, AnthropicMessagesRequest, AnthropicMessagesResponse, AnthropicUsage,
        MessageContent, MessageParam, SystemPrompt, TextBlock, ThinkingBlock, ThinkingConfig,
        ToolChoice, ToolUnion,
    },
};

use super::tool_raw;

/// Stateless Anthropic Messages codec: decode to IR and encode back.
#[derive(Debug, Clone, Copy, Default)]
pub struct AnthropicMessagesCodec;

/// Documented default when `max_tokens` is absent and cannot be recovered
/// from `raw` (Anthropic requires the field; we never invent silently —
/// a loss entry is always pushed first).
pub const DEFAULT_MAX_TOKENS: u32 = 1024;

/// Output margin: `budget_tokens` must be ≤ `max_tokens − OUTPUT_MARGIN`
/// so the model has enough tokens left for a visible reply after thinking.
/// 1,024 is the minimum recommended by Anthropic (1,024–2,048).
pub const OUTPUT_MARGIN: u32 = 1024;

/// Effort → nominal (non-downgraded) thinking budget table.
const EFFORT_LEVELS: &[(&str, u32)] = &[
    ("max", 32768),
    ("xhigh", 16384),
    ("high", 8192),
    ("medium", 4096),
    ("low", 2048),
    ("minimal", 1024),
];

/// Map a normalized `reasoning_effort` to an Anthropic thinking budget,
/// downgrading recursively when `max_tokens` is too small.
///
/// Returns `None` when:
/// - `effort` is `"none"` or an unknown value (no thinking requested),
/// - even the smallest budget (1,024) cannot fit within `max_tokens − OUTPUT_MARGIN`.
pub fn resolve_budget(effort: &str, max_tokens: u32) -> Option<u32> {
    let available = max_tokens.saturating_sub(OUTPUT_MARGIN);
    let start = EFFORT_LEVELS.iter().position(|(l, _)| *l == effort)?;
    EFFORT_LEVELS[start..]
        .iter()
        .find(|(_, budget)| *budget <= available)
        .map(|(_, budget)| *budget)
}

/// Ideal budget for a known effort (no downgrade); `None` for unknown.
fn nominal_budget(effort: &str) -> Option<u32> {
    EFFORT_LEVELS
        .iter()
        .find(|(l, _)| *l == effort)
        .map(|(_, b)| *b)
}

/// Derive a normalized effort token from an explicit Anthropic
/// `budget_tokens` value (ingress decode → IR → capability pre-check).
fn effort_for_budget(budget: u32) -> &'static str {
    match budget {
        b if b >= 32768 => "max",
        b if b >= 16384 => "xhigh",
        b if b >= 8192 => "high",
        b if b >= 4096 => "medium",
        b if b >= 2048 => "low",
        _ => "minimal",
    }
}

/// Map Anthropic `stop_reason` → [`IrFinishReason`].
fn stop_to_finish(reason: &str) -> IrFinishReason {
    match reason {
        "end_turn" | "stop_sequence" => IrFinishReason::Stop,
        "max_tokens" => IrFinishReason::Length,
        "tool_use" => IrFinishReason::ToolCalls,
        other => IrFinishReason::Other(other.to_string()),
    }
}

/// Map [`IrFinishReason`] → Anthropic `stop_reason`.
fn finish_to_stop(reason: Option<&IrFinishReason>) -> String {
    match reason {
        None => "end_turn".into(),
        Some(IrFinishReason::Stop | IrFinishReason::FunctionCall) => "end_turn".into(),
        Some(IrFinishReason::Length) => "max_tokens".into(),
        Some(IrFinishReason::ToolCalls) => "tool_use".into(),
        Some(IrFinishReason::ContentFilter) => "end_turn".into(),
        Some(IrFinishReason::Cancelled) => "end_turn".into(),
        Some(IrFinishReason::Other(s)) => s.clone(),
    }
}

/// Extract Anthropic usage → IR [`Usage`].
///
/// Stream frames still carry raw JSON, so this delegates to the typed
/// conversion and degrades **per field** when the typed parse fails (a
/// malformed usage must never abort a stream, and one ill-typed sub-object
/// must never zero the counters that parsed fine).
fn anthropic_usage(value: &Value) -> Usage {
    match serde_json::from_value::<AnthropicUsage>(value.clone()) {
        Ok(usage) => usage_from_typed(&usage),
        Err(_) => usage_from_value_lenient(value),
    }
}

/// Per-field usage read used when the typed parse fails.
///
/// Mirrors the pre-typed `anthropic_usage` semantics field by field: a
/// missing counter is `0`, a present-but-ill-typed counter is skipped, and
/// the cache counters default to `Some(0)` whenever `input_tokens > 0`
/// (they were reported as "known zero" for real requests).
fn usage_from_value_lenient(value: &Value) -> Usage {
    let input = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    let output = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0) as u32;
    Usage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: input.saturating_add(output),
        cached_tokens: value
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32)
            .or((input > 0).then_some(0)),
        cache_creation_tokens: value
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32)
            .or((input > 0).then_some(0)),
        // Same precedence as [`usage_from_typed`]: documented nested location
        // first, legacy root-level `thinking_tokens` second.
        reasoning_tokens: value
            .pointer("/output_tokens_details/thinking_tokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32)
            .or_else(|| {
                value
                    .get("thinking_tokens")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32)
            }),
    }
}

/// Typed Anthropic usage → IR [`Usage`].
fn usage_from_typed(usage: &AnthropicUsage) -> Usage {
    let input = usage.input_tokens as u32;
    let output = usage.output_tokens as u32;
    Usage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: input.saturating_add(output),
        cached_tokens: usage
            .cache_read_input_tokens
            .map(|v| v as u32)
            .or((input > 0).then_some(0)),
        cache_creation_tokens: usage
            .cache_creation_input_tokens
            .map(|v| v as u32)
            .or((input > 0).then_some(0)),
        // Root-level `thinking_tokens` is not modelled by the DTO (it rides in
        // `extra`); `output_tokens_details` is the documented location.
        reasoning_tokens: usage
            .output_tokens_details
            .as_ref()
            .map(|d| d.thinking_tokens as u32)
            .or_else(|| {
                usage
                    .extra
                    .get("thinking_tokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32)
            }),
    }
}

/// IR [`Usage`] → Anthropic usage object.
fn usage_to_anthropic(usage: &Usage) -> AnthropicUsage {
    let mut extra = Map::new();
    // Root-level `thinking_tokens` (not `output_tokens_details`) is the shape
    // [`usage_from_typed`] reads back — keep the round-trip symmetric.
    if let Some(r) = usage.reasoning_tokens {
        extra.insert("thinking_tokens".into(), json!(r));
    }
    AnthropicUsage {
        input_tokens: u64::from(usage.prompt_tokens),
        output_tokens: u64::from(usage.completion_tokens),
        cache_creation_input_tokens: usage.cache_creation_tokens.map(u64::from),
        cache_read_input_tokens: usage.cached_tokens.map(u64::from),
        cache_creation: None,
        output_tokens_details: None,
        server_tool_use: None,
        service_tier: None,
        inference_geo: None,
        extra,
    }
}

// ── typed ↔ value bridges ─────────────────────────────────────────
//
// Encode-side shaping stays `Value`-shaped: `cache_control` merging, chat
// image URL conversion, and Audio/Raw passthrough all work on JSON objects.
// Parsing the shaped object back into the typed union keeps a single lossless
// path, with the `Unknown(Value)` variant as the escape hatch for anything the
// DTO cannot express.

/// Parse a raw value into a typed DTO, keeping it verbatim in the caller's
/// `Unknown` variant unless the typed form round-trips unchanged.
///
/// Several DTO variants carry `#[serde(default)]` fields, so a plain parse
/// can grow fields the caller never sent (`allowed_tools` / `vector_store_ids`
/// on the Responses side). The round-trip check keeps the wire shape intact
/// (R3): a body that would not survive a typed round-trip lands in `Unknown`
/// verbatim. Same guard as `parse_faithful` on the Responses side
/// (`codec/responses.rs`).
///
/// `tool_use` / `server_tool_use` no longer rely on this guard: their `input`
/// is an `Option`, so "absent" and "explicitly empty" stay distinct and both
/// round-trip unchanged. The guard still matters for the remaining
/// `#[serde(default)]` fields.
fn parse_faithful<T>(value: &Value) -> Option<T>
where
    T: DeserializeOwned + Serialize,
{
    let parsed = serde_json::from_value::<T>(value.clone()).ok()?;
    (serde_json::to_value(&parsed).ok()? == *value).then_some(parsed)
}

/// Shaped content-block object → typed union (verbatim `Unknown` fallback).
fn content_block_from_value(value: Value) -> AnthropicContentBlock {
    parse_faithful(&value).unwrap_or(AnthropicContentBlock::Unknown(value))
}

/// Typed content block → JSON object (for `ir.raw` / loss payloads).
fn content_block_to_value(block: &AnthropicContentBlock) -> Value {
    serde_json::to_value(block).unwrap_or(Value::Null)
}

/// Shaped `tools[]` object → typed union (verbatim `Unknown` fallback).
fn tool_union_from_value(value: Value) -> ToolUnion {
    parse_faithful(&value).unwrap_or(ToolUnion::Unknown(value))
}

/// Typed tool → JSON object (hosted detection / extras merging work on it).
fn tool_union_to_value(tool: &ToolUnion) -> Value {
    serde_json::to_value(tool).unwrap_or(Value::Null)
}

/// Shaped `tool_choice` object → typed union (verbatim `Unknown` fallback).
fn tool_choice_from_value(value: Value) -> ToolChoice {
    parse_faithful(&value).unwrap_or(ToolChoice::Unknown(value))
}

/// Shaped `thinking` object → typed union (verbatim `Unknown` fallback).
fn thinking_config_from_value(value: Value) -> ThinkingConfig {
    parse_faithful(&value).unwrap_or(ThinkingConfig::Unknown(value))
}

/// Put a raw field back on the typed body: into `slot` when it is still empty
/// and the value fits, otherwise verbatim into `extra` (never dropped).
///
/// Mirrors the old `Value`-based `body.entry(k).or_insert(v)` rule: a field the
/// IR already produced always wins over `raw`.
fn restore<T: DeserializeOwned>(
    slot: &mut Option<T>,
    extra: &mut Map<String, Value>,
    key: &str,
    value: &Value,
) {
    if slot.is_none() {
        if let Ok(parsed) = serde_json::from_value(value.clone()) {
            *slot = Some(parsed);
            return;
        }
        extra.insert(key.to_string(), value.clone());
    }
}

impl AnthropicMessagesCodec {
    // ═══════════════════════════════════════════════════════════
    // request decode (ingress): Anthropic body → IrRequest
    // ═══════════════════════════════════════════════════════════

    /// Decode an Anthropic Messages request body into IR.
    ///
    /// Fields the DTO does not model ride in `req.extra`; typed fields without
    /// an IR home are copied into `raw` as well, so the native passthrough
    /// chain (body → `ir.raw` → upstream body) stays lossless. Structural
    /// drops push onto `loss`.
    pub fn decode_request(req: &AnthropicMessagesRequest) -> IrRequest {
        let mut loss = sag_core::LossInventory::default();

        // Preserve every unmodelled top-level field for encode round-trip.
        let mut raw = req.extra.clone();
        if let Some(v) = &req.output_config {
            raw.insert("output_config".into(), json!(v));
        }
        if let Some(v) = &req.cache_control {
            raw.insert("cache_control".into(), json!(v));
        }
        if let Some(v) = &req.container {
            raw.insert("container".into(), json!(v));
        }
        if let Some(v) = &req.service_tier {
            raw.insert("service_tier".into(), json!(v));
        }
        if let Some(v) = &req.inference_geo {
            raw.insert("inference_geo".into(), json!(v));
        }

        let model = req.model.clone();

        // system (string | text blocks) → leading system message.
        let mut messages = Vec::new();
        if let Some(system) = &req.system {
            let content = match system {
                SystemPrompt::Text(text) => vec![IrContentBlock::Text { text: text.clone() }],
                SystemPrompt::Blocks(blocks) => blocks
                    .iter()
                    .map(|b| IrContentBlock::Text {
                        text: b.text.clone(),
                    })
                    .collect(),
            };
            messages.push(IrMessage {
                role: IrRole::System,
                content,
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
                raw: Map::new(),
            });
        }

        for (mi, m) in req.messages.iter().enumerate() {
            decode_request_message(m, mi, &mut messages, &mut loss);
        }

        // max_tokens (required by Anthropic; present on wire → IR).
        let max_tokens = req.max_tokens;

        // thinking → Reasoning (budget + derived effort for capability).
        let mut reasoning = Reasoning::default();
        if let Some(thinking) = &req.thinking {
            reasoning.thinking = Some(json!(thinking));
            // Explicitly `disabled` keeps the object for round-trip but
            // carries no effort/budget (encode restores it).
            if let ThinkingConfig::Enabled(enabled) = thinking {
                reasoning.budget_tokens = Some(enabled.budget_tokens);
                reasoning.effort = Some(effort_for_budget(enabled.budget_tokens).to_string());
            }
        }

        // tools (input_schema) → IrTool. M8: Anthropic hosted
        // `web_search` tools (`type: "web_search_20250305"`) keep their
        // type in `raw` for round-trip.
        let tools = match &req.tools {
            None => Vec::new(),
            Some(unions) => unions
                .iter()
                .enumerate()
                .filter_map(|(i, t)| decode_tool(t, i, &mut loss))
                .collect(),
        };

        let stop = req.stop_sequences.clone().unwrap_or_default();

        let metadata = req
            .metadata
            .as_ref()
            .map(|m| json!(m))
            .unwrap_or(Value::Null);
        let tool_choice = req
            .tool_choice
            .as_ref()
            .map(|c| json!(c))
            .unwrap_or(Value::Null);

        // Sampling: temperature / top_p / top_k.
        let sampling = Sampling {
            temperature: req.temperature,
            top_p: req.top_p,
            top_k: req.top_k,
            seed: None,
            presence_penalty: None,
            frequency_penalty: None,
            logit_bias: None,
        };

        // stream → IR (also kept out of raw; encode re-derives).
        let stream = req.stream.unwrap_or(false);

        IrRequest {
            model,
            messages,
            sampling,
            max_tokens,
            stream,
            tools,
            tool_choice,
            response_format: Value::Null,
            reasoning,
            stop,
            metadata,
            raw,
            loss,
        }
    }

    // ═══════════════════════════════════════════════════════════
    // request encode (upstream): IrRequest → Anthropic body
    // ═══════════════════════════════════════════════════════════

    /// Encode IR → Anthropic Messages body using `ir.model`.
    ///
    /// `cache_control` is **not** injected here (not IR) — the provider
    /// layer applies [`inject_cache_control_value`] when enabled.
    pub fn encode_request(ir: &IrRequest) -> AnthropicMessagesRequest {
        Self::encode_request_for_model(ir, &ir.model)
    }

    /// Encode IR → Anthropic body with an explicit model (real_model override).
    pub fn encode_request_for_model(ir: &IrRequest, model: &str) -> AnthropicMessagesRequest {
        // Work on a mutable loss clone so max_tokens / thinking failures
        // can be recorded without `&mut IrRequest`. Callers that need the
        // loss entries re-run decode; the provider path logs via
        // `encode_request_with_loss`.
        let mut loss = ir.loss.clone();
        Self::encode_request_inner(ir, model, &mut loss)
    }

    /// Encode and return the (possibly extended) loss inventory so callers
    /// can surface encode-side structural drops (max_tokens default,
    /// thinking budget failure).
    pub fn encode_request_with_loss(
        ir: &IrRequest,
        model: &str,
    ) -> (AnthropicMessagesRequest, sag_core::LossInventory) {
        let mut loss = ir.loss.clone();
        let body = Self::encode_request_inner(ir, model, &mut loss);
        (body, loss)
    }

    fn encode_request_inner(
        ir: &IrRequest,
        model: &str,
        loss: &mut sag_core::LossInventory,
    ) -> AnthropicMessagesRequest {
        // system: all leading System messages → Anthropic `system` field
        // (string when a single pure-text block; blocks otherwise).
        let (system_msgs, chat_msgs): (Vec<&IrMessage>, Vec<&IrMessage>) =
            ir.messages.iter().partition(|m| m.role == IrRole::System);
        let mut system_blocks: Vec<TextBlock> = Vec::new();
        if !system_msgs.is_empty() {
            for (bi, b) in system_msgs
                .iter()
                .flat_map(|m| m.content.iter())
                .enumerate()
            {
                let path = format!("system.content[{bi}]");
                match encode_content_block(b, &path, loss) {
                    AnthropicContentBlock::Text(text) => system_blocks.push(text),
                    // Anthropic's `system` field only accepts text blocks;
                    // anything else is recorded and dropped rather than sent
                    // as a foreign shape.
                    other => loss.push_loss(
                        LossScope::ContentBlock,
                        path,
                        LossReason::UnsupportedByTarget,
                        content_block_to_value(&other),
                    ),
                }
            }
        }
        let system = match system_blocks.len() {
            // Single pure-text block → bare string (Anthropic accepts both).
            1 => Some(SystemPrompt::Text(system_blocks[0].text.clone())),
            0 => None,
            _ => Some(SystemPrompt::Blocks(system_blocks)),
        };

        // messages: non-system → Anthropic user/assistant turns.
        // Tool-role messages and ToolResult blocks collapse into the
        // adjacent user turn as `tool_result` blocks (Anthropic shape).
        let messages = encode_messages(&chat_msgs, loss);

        // max_tokens: IR → raw → documented default (always with loss).
        let max_tokens = match ir.max_tokens {
            Some(n) => n,
            None => match ir.raw.get("max_tokens").and_then(|v| v.as_u64()) {
                Some(n) if n <= u32::MAX as u64 => n as u32,
                Some(dirty) => {
                    loss.push_loss(
                        LossScope::Request,
                        "max_tokens",
                        LossReason::UnsupportedByIr,
                        json!(dirty),
                    );
                    DEFAULT_MAX_TOKENS
                }
                None => {
                    loss.push_loss(
                        LossScope::Request,
                        "max_tokens",
                        LossReason::DroppedByTargetFeature,
                        json!(null),
                    );
                    DEFAULT_MAX_TOKENS
                }
            },
        };

        let mut body = AnthropicMessagesRequest {
            model: model.to_string(),
            max_tokens: Some(max_tokens),
            messages,
            system,
            metadata: None,
            stop_sequences: (!ir.stop.is_empty()).then(|| ir.stop.clone()),
            stream: ir.stream.then_some(true),
            thinking: None,
            tools: None,
            tool_choice: None,
            output_config: None,
            cache_control: None,
            container: None,
            service_tier: None,
            inference_geo: None,
            temperature: ir.sampling.temperature,
            top_p: ir.sampling.top_p,
            top_k: ir.sampling.top_k,
            extra: Map::new(),
        };

        if !ir.metadata.is_null() {
            restore(
                &mut body.metadata,
                &mut body.extra,
                "metadata",
                &ir.metadata,
            );
        }
        if let Some(v) = encode_tool_choice_for_anthropic(&ir.tool_choice, loss) {
            body.tool_choice = Some(tool_choice_from_value(v));
        }

        // tools: prefer original raw when present (decode failure path);
        // else rebuild from IrTool.
        match ir.raw.get("tools") {
            Some(Value::Array(arr)) => {
                body.tools = Some(arr.iter().cloned().map(tool_union_from_value).collect());
            }
            Some(other) => {
                body.extra.insert("tools".into(), other.clone());
            }
            None if !ir.tools.is_empty() => body.tools = Some(encode_tools(&ir.tools)),
            None => {}
        }

        // thinking from Reasoning — never silently disabled (§1.3 defect 8).
        if let Some(thinking_value) = Self::encode_thinking(ir, max_tokens, loss) {
            body.thinking = Some(thinking_config_from_value(thinking_value));
        }

        // Unknown raw fields round-trip (do not overwrite known keys).
        let mut extra = std::mem::take(&mut body.extra);
        for (k, v) in &ir.raw {
            match k.as_str() {
                // Consumed above (mapped / loss-recorded / intentionally
                // omitted); never re-inject a foreign shape from raw.
                "max_tokens" | "tool_choice" | "tools" | "model" | "messages" => {}
                "system" => restore(&mut body.system, &mut extra, k, v),
                "metadata" => restore(&mut body.metadata, &mut extra, k, v),
                "stop_sequences" => restore(&mut body.stop_sequences, &mut extra, k, v),
                "stream" => restore(&mut body.stream, &mut extra, k, v),
                "thinking" => restore(&mut body.thinking, &mut extra, k, v),
                "output_config" => restore(&mut body.output_config, &mut extra, k, v),
                "cache_control" => restore(&mut body.cache_control, &mut extra, k, v),
                "container" => restore(&mut body.container, &mut extra, k, v),
                "service_tier" => restore(&mut body.service_tier, &mut extra, k, v),
                "inference_geo" => restore(&mut body.inference_geo, &mut extra, k, v),
                "temperature" => restore(&mut body.temperature, &mut extra, k, v),
                "top_p" => restore(&mut body.top_p, &mut extra, k, v),
                "top_k" => restore(&mut body.top_k, &mut extra, k, v),
                _ => {
                    extra.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
        body.extra = extra;

        body
    }

    /// Build the `thinking` object from IR Reasoning.
    ///
    /// Priority: explicit `thinking` object → explicit `budget_tokens` →
    /// `effort` via [`resolve_budget`]. When a budget cannot fit,
    /// records a loss and returns `None` (thinking omitted — never a
    /// silent `"disabled"` injection).
    fn encode_thinking(
        ir: &IrRequest,
        max_tokens: u32,
        loss: &mut sag_core::LossInventory,
    ) -> Option<Value> {
        // 1. Explicit thinking object on IR (from Anthropic ingress).
        if let Some(thinking) = &ir.reasoning.thinking {
            if thinking.get("type").and_then(|t| t.as_str()) == Some("disabled") {
                return Some(thinking.clone());
            }
            if thinking.get("type").and_then(|t| t.as_str()) == Some("enabled") {
                let budget = thinking
                    .get("budget_tokens")
                    .and_then(|v| v.as_u64())
                    .and_then(|n| u32::try_from(n).ok())
                    .or(ir.reasoning.budget_tokens);
                if let Some(budget) = budget {
                    if budget <= max_tokens.saturating_sub(OUTPUT_MARGIN) {
                        return Some(json!({"type": "enabled", "budget_tokens": budget}));
                    }
                    loss.push_loss(
                        LossScope::Request,
                        "thinking.budget_tokens",
                        LossReason::DroppedByTargetFeature,
                        thinking.clone(),
                    );
                    return None;
                }
                // enabled without budget — fall through to effort.
            }
        }

        // 2. Explicit budget_tokens on IR.
        if let Some(budget) = ir.reasoning.budget_tokens {
            if budget <= max_tokens.saturating_sub(OUTPUT_MARGIN) {
                return Some(json!({"type": "enabled", "budget_tokens": budget}));
            }
            loss.push_loss(
                LossScope::Request,
                "thinking.budget_tokens",
                LossReason::DroppedByTargetFeature,
                json!(budget),
            );
            return None;
        }

        // 3. effort → resolve_budget.
        if let Some(effort) = ir.reasoning.effort.as_deref() {
            if effort == "none" {
                return None;
            }
            match resolve_budget(effort, max_tokens) {
                Some(budget) => {
                    // Budget downgraded (e.g. high→low) — record the loss so
                    // the silent capacity trade-off is visible to operators.
                    if let Some(nominal) = nominal_budget(effort)
                        && budget < nominal
                    {
                        loss.push_loss(
                            LossScope::Request,
                            "thinking.budget_tokens",
                            LossReason::DroppedByTargetFeature,
                            json!({
                                "effort": effort,
                                "nominal": nominal,
                                "actual": budget,
                                "max_tokens": max_tokens,
                            }),
                        );
                    }
                    Some(json!({"type": "enabled", "budget_tokens": budget}))
                }
                None => {
                    // Requested thinking but no budget fits — record loss,
                    // omit thinking (never silently close without a trace).
                    loss.push_loss(
                        LossScope::Request,
                        "thinking",
                        LossReason::DroppedByTargetFeature,
                        json!({"effort": effort, "max_tokens": max_tokens}),
                    );
                    None
                }
            }
        } else {
            None
        }
    }

    // ═══════════════════════════════════════════════════════════
    // response decode (upstream): Anthropic message → IrResponse
    // ═══════════════════════════════════════════════════════════

    /// Decode an Anthropic non-stream response into IR.
    ///
    /// Keeps **all** content blocks (text / tool_use / thinking) —
    /// fixes §1.3 defects 1 and 4 (tool_use loss, hardcoded stop,
    /// fabricated `created`).
    pub fn decode_response(resp: &AnthropicMessagesResponse) -> IrResponse {
        let mut loss = sag_core::LossInventory::default();
        let mut output = Vec::new();

        let id = resp.id.clone();
        let model = resp.model.clone();

        // Fields the DTO does not model (plus typed fields without an IR home)
        // → raw (passthrough, no loss).
        let mut raw = resp.extra.clone();
        if let Some(v) = &resp.stop_sequence {
            raw.insert("stop_sequence".into(), json!(v));
        }
        if let Some(v) = &resp.stop_details {
            raw.insert("stop_details".into(), json!(v));
        }
        if let Some(v) = &resp.container {
            raw.insert("container".into(), json!(v));
        }

        for (i, block) in resp.content.iter().enumerate() {
            match block {
                AnthropicContentBlock::Text(t) => {
                    output.push(IrOutputItem::Text {
                        text: t.text.clone(),
                    });
                }
                AnthropicContentBlock::Thinking(t) => {
                    output.push(IrOutputItem::Reasoning {
                        text: t.thinking.clone(),
                    });
                }
                AnthropicContentBlock::RedactedThinking(t) => {
                    output.push(IrOutputItem::Reasoning {
                        text: t.data.clone(),
                    });
                }
                AnthropicContentBlock::ToolUse(t) => {
                    let tc = IrToolCall {
                        id: t.id.clone(),
                        name: t.name.clone(),
                        // IR has no "arguments absent" state, so a missing
                        // `input` materialises as `{}` here (same rule as the
                        // egress side, which always emits `input`).
                        arguments: Value::Object(t.input.clone().unwrap_or_default()),
                        index: Some(output.len() as u32),
                    };
                    output.push(IrOutputItem::ToolCall(tc));
                }
                _ => {
                    // Unknown block (server_tool_use, …) — Raw + loss.
                    let value = content_block_to_value(block);
                    loss.push_loss(
                        LossScope::Response,
                        format!("content[{i}]"),
                        LossReason::UnsupportedByIr,
                        value.clone(),
                    );
                    output.push(IrOutputItem::Raw(value));
                }
            }
        }

        let finish_reason = resp.stop_reason.as_deref().map(stop_to_finish);

        let usage = resp.usage.as_ref().map(usage_from_typed);

        // `created` intentionally None — Anthropic has no timestamp; never
        // fabricate one (§1.3 defect 1).
        IrResponse {
            id,
            created: None,
            model,
            output,
            finish_reason,
            usage,
            raw,
            loss,
        }
    }

    // ═══════════════════════════════════════════════════════════
    // response encode (ingress): IrResponse → Anthropic message
    // ═══════════════════════════════════════════════════════════

    /// Encode IR → Anthropic Messages response JSON (ingress egress).
    pub fn encode_response(ir: &IrResponse) -> AnthropicMessagesResponse {
        let mut content: Vec<AnthropicContentBlock> = Vec::new();
        for item in &ir.output {
            match item {
                IrOutputItem::Text { text } => {
                    content.push(AnthropicContentBlock::Text(TextBlock {
                        kind: "text".into(),
                        text: text.clone(),
                        citations: None,
                        cache_control: None,
                        extra: Map::new(),
                    }));
                }
                IrOutputItem::Reasoning { text } => {
                    content.push(AnthropicContentBlock::Thinking(ThinkingBlock {
                        kind: "thinking".into(),
                        thinking: text.clone(),
                        signature: None,
                        extra: Map::new(),
                    }));
                }
                IrOutputItem::ToolCall(tc) => {
                    content.push(content_block_from_value(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": tc.arguments,
                    })));
                }
                IrOutputItem::Raw(v) => content.push(content_block_from_value(v.clone())),
            }
        }

        let mut body = AnthropicMessagesResponse {
            id: ir.id.clone(),
            kind: "message".into(),
            role: "assistant".into(),
            content,
            model: ir.model.clone(),
            stop_reason: Some(finish_to_stop(ir.finish_reason.as_ref())),
            stop_sequence: None,
            stop_details: None,
            container: None,
            usage: Some(match &ir.usage {
                Some(usage) => usage_to_anthropic(usage),
                None => usage_to_anthropic(&Usage::default()),
            }),
            extra: Map::new(),
        };

        // Unknown raw fields round-trip (do not overwrite known keys).
        let mut extra = Map::new();
        for (k, v) in &ir.raw {
            match k.as_str() {
                "id" | "type" | "role" | "model" | "content" | "stop_reason" | "usage" => {}
                "stop_sequence" => restore(&mut body.stop_sequence, &mut extra, k, v),
                "stop_details" => restore(&mut body.stop_details, &mut extra, k, v),
                "container" => restore(&mut body.container, &mut extra, k, v),
                _ => {
                    extra.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
        body.extra = extra;

        body
    }

    // ═══════════════════════════════════════════════════════════
    // stream decode (upstream): Anthropic SSE → IrStreamEvent(s)
    // ═══════════════════════════════════════════════════════════

    /// Decode a single Anthropic SSE frame into 0..n IR stream events.
    ///
    /// Maps the full lifecycle (defects 5–8): message_start, content_block
    /// start/delta/stop (text / thinking / tool_use), message_delta,
    /// message_stop; ping ignored; error → Error; unknown → Raw.
    pub fn decode_stream_frame(frame: &[u8]) -> Vec<IrStreamEvent> {
        let text = String::from_utf8_lossy(frame);
        let mut events = Vec::new();
        let mut saw_data = false;

        for line in text.lines() {
            let line = line.trim();
            // Anthropic also emits `event:` lines; data lines carry JSON.
            let Some(rest) = line.strip_prefix("data:") else {
                continue;
            };
            saw_data = true;
            let json_str = rest.trim_start();

            let Ok(val) = serde_json::from_str::<Value>(json_str) else {
                events.push(IrStreamEvent::Raw(frame.to_vec()));
                continue;
            };

            // Error event: `{"type":"error","error":{...}}` or top-level error.
            if val.get("type").and_then(|t| t.as_str()) == Some("error")
                || crate::base::body_has_error_object(&val)
            {
                let msg = val
                    .get("error")
                    .map(crate::base::body_error_message)
                    .unwrap_or_else(|| crate::base::body_error_message(&val));
                events.push(IrStreamEvent::Error(msg));
                // Forward the raw frame too so the client sees the
                // Anthropic error envelope (behavior-compatible).
                events.push(IrStreamEvent::Raw(frame.to_vec()));
                continue;
            }

            let Some(event_type) = val.get("type").and_then(|t| t.as_str()) else {
                events.push(IrStreamEvent::Raw(frame.to_vec()));
                continue;
            };

            match event_type {
                "message_start" => {
                    let msg = val.get("message").cloned().unwrap_or(Value::Null);
                    events.push(IrStreamEvent::Start(IrResponseMeta {
                        id: msg.get("id").and_then(|v| v.as_str()).map(String::from),
                        // Anthropic has no created timestamp.
                        created: None,
                        model: msg.get("model").and_then(|v| v.as_str()).map(String::from),
                    }));
                    if let Some(usage) = msg.get("usage").filter(|u| !u.is_null()) {
                        events.push(IrStreamEvent::Usage(anthropic_usage(usage)));
                    }
                }
                "content_block_start" => {
                    let index = val.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                    let block = val.get("content_block").cloned().unwrap_or(Value::Null);
                    match block.get("type").and_then(|t| t.as_str()) {
                        Some("tool_use") => {
                            events.push(IrStreamEvent::ToolCallStart {
                                index,
                                id: block.get("id").and_then(|v| v.as_str()).map(String::from),
                                name: block.get("name").and_then(|v| v.as_str()).map(String::from),
                            });
                        }
                        Some("thinking") => {
                            // Initial thinking text (usually empty) →
                            // ReasoningDelta so the client opens a block.
                            let text = block.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                            if !text.is_empty() {
                                events.push(IrStreamEvent::ReasoningDelta {
                                    index,
                                    text: text.to_string(),
                                });
                            }
                        }
                        Some("text") | None => {
                            // Text block start carries no payload — no event
                            // needed (first text_delta will open it).
                        }
                        _ => {
                            // Unknown block type — forward raw (no silent drop).
                            events.push(IrStreamEvent::Raw(frame.to_vec()));
                        }
                    }
                }
                "content_block_delta" => {
                    let index = val.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                    let delta = val.get("delta").cloned().unwrap_or(Value::Null);
                    match delta.get("type").and_then(|t| t.as_str()) {
                        Some("text_delta") => {
                            let text = delta.get("text").and_then(|t| t.as_str()).unwrap_or("");
                            if !text.is_empty() {
                                events.push(IrStreamEvent::TextDelta {
                                    index,
                                    text: text.to_string(),
                                });
                            }
                        }
                        Some("thinking_delta") => {
                            let text = delta.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                            if !text.is_empty() {
                                events.push(IrStreamEvent::ReasoningDelta {
                                    index,
                                    text: text.to_string(),
                                });
                            }
                        }
                        Some("input_json_delta") => {
                            let partial = delta
                                .get("partial_json")
                                .and_then(|t| t.as_str())
                                .unwrap_or("");
                            if !partial.is_empty() {
                                events.push(IrStreamEvent::ToolCallDelta {
                                    index,
                                    arguments_delta: partial.to_string(),
                                });
                            }
                        }
                        _ => {
                            events.push(IrStreamEvent::Raw(frame.to_vec()));
                        }
                    }
                }
                "content_block_stop" => {
                    // Block completion has no IR event — clients re-open on
                    // the next start/delta. Not a silent drop (nothing to
                    // represent); no Raw needed for a pure bookkeeping frame.
                }
                "message_delta" => {
                    let finish = val
                        .get("delta")
                        .and_then(|d| d.get("stop_reason"))
                        .and_then(|s| s.as_str())
                        .map(stop_to_finish);
                    if let Some(finish_reason) = finish {
                        events.push(IrStreamEvent::Finish {
                            index: 0,
                            finish_reason: Some(finish_reason),
                        });
                    }
                    if let Some(usage) = val.get("usage").filter(|u| !u.is_null()) {
                        events.push(IrStreamEvent::Usage(anthropic_usage(usage)));
                    }
                }
                "message_stop" => {
                    events.push(IrStreamEvent::End);
                }
                "ping" => {
                    // Keepalive — intentionally no event (plan §5.3).
                }
                _ => {
                    // Unknown event type — Raw passthrough + (no loss channel
                    // on streams; Raw keeps the bytes).
                    events.push(IrStreamEvent::Raw(frame.to_vec()));
                }
            }
        }

        if !saw_data && events.is_empty() && !text.trim().is_empty() {
            // Non-data frame (event: lines only / comments) — surface Raw.
            events.push(IrStreamEvent::Raw(frame.to_vec()));
        }

        events
    }

    // ═══════════════════════════════════════════════════════════
    // stream encode (ingress): IrStreamEvent → Anthropic SSE
    // ═══════════════════════════════════════════════════════════

    /// Encode one IR stream event as Anthropic SSE bytes (stateless view).
    ///
    /// Prefer [`Self::stream_encoder`] for ingress egress — it tracks
    /// content-block open/close state so `content_block_start` / `_stop`
    /// frames are emitted correctly. This fn handles Raw passthrough and
    /// simple one-shot events.
    pub fn encode_stream_event(ev: &IrStreamEvent) -> Vec<u8> {
        match ev {
            IrStreamEvent::Raw(bytes) => {
                // Suppress OpenAI `[DONE]` — Anthropic ends with message_stop.
                let text = String::from_utf8_lossy(bytes);
                if text.contains("[DONE]") {
                    return Vec::new();
                }
                bytes.clone()
            }
            IrStreamEvent::Start(meta) => {
                let mut message = Map::new();
                let meta = meta.clone();
                if let Some(id) = &meta.id {
                    message.insert("id".into(), json!(id));
                } else {
                    message.insert("id".into(), json!("msg_stream"));
                }
                message.insert("type".into(), json!("message"));
                message.insert("role".into(), json!("assistant"));
                message.insert(
                    "model".into(),
                    json!(meta.model.clone().unwrap_or_default()),
                );
                message.insert("content".into(), json!([]));
                message.insert(
                    "usage".into(),
                    json!({"input_tokens": 0, "output_tokens": 0}),
                );
                let payload = json!({"type": "message_start", "message": message});
                format!("data: {payload}\n\n").into_bytes()
            }
            IrStreamEvent::TextDelta { text, .. } => {
                let payload = json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": text},
                });
                format!("data: {payload}\n\n").into_bytes()
            }
            IrStreamEvent::ReasoningDelta { text, .. } => {
                let payload = json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "thinking_delta", "thinking": text},
                });
                format!("data: {payload}\n\n").into_bytes()
            }
            IrStreamEvent::ToolCallStart { index, id, name } => {
                let block = json!({
                    "type": "tool_use",
                    "id": id.clone().unwrap_or_default(),
                    "name": name.clone().unwrap_or_default(),
                    "input": {},
                });
                let payload = json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": block,
                });
                format!("data: {payload}\n\n").into_bytes()
            }
            IrStreamEvent::ToolCallDelta {
                index,
                arguments_delta,
            } => {
                let payload = json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": {"type": "input_json_delta", "partial_json": arguments_delta},
                });
                format!("data: {payload}\n\n").into_bytes()
            }
            IrStreamEvent::Finish { finish_reason, .. } => {
                let stop = finish_to_stop(finish_reason.as_ref());
                let payload = json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": stop},
                });
                format!("data: {payload}\n\n").into_bytes()
            }
            // Usage / Error / End are handled by the dispatch loop — never
            // client-encoded here (Usage) … End is message_stop below only
            // when it reaches encode (it normally finishes the terminal).
            IrStreamEvent::Usage(_) => Vec::new(),
            IrStreamEvent::Error(_) => Vec::new(),
            IrStreamEvent::End => b"data: {\"type\":\"message_stop\"}\n\n".to_vec(),
        }
    }

    /// Build a **stateful** ingress stream encoder that tracks the open
    /// content block so Anthropic's `content_block_start` / `_stop`
    /// lifecycle is emitted correctly.
    ///
    /// Returns a `Fn` suitable for
    /// `sag_dispatch::build_streaming_response`.
    pub fn stream_encoder() -> impl Fn(&IrStreamEvent) -> Vec<u8> + Send + 'static {
        let state = RefCell::new(StreamEncodeState::default());
        move |ev| {
            let mut st = state.borrow_mut();
            st.encode(ev)
        }
    }
}

// ── stream encoder state ──────────────────────────────────────────

#[derive(Default)]
struct StreamEncodeState {
    started: bool,
    /// Kind of the currently open content block, if any.
    open_block: Option<OpenBlock>,
    next_index: u32,
}

#[derive(Clone, Copy, PartialEq)]
enum OpenBlock {
    Text,
    Thinking,
    ToolUse,
}

impl StreamEncodeState {
    fn encode(&mut self, ev: &IrStreamEvent) -> Vec<u8> {
        match ev {
            IrStreamEvent::Raw(bytes) => {
                let text = String::from_utf8_lossy(bytes);
                if text.contains("[DONE]") {
                    return Vec::new();
                }
                bytes.clone()
            }
            IrStreamEvent::Start(_meta) => {
                self.started = true;
                self.open_block = None;
                self.next_index = 0;
                AnthropicMessagesCodec::encode_stream_event(ev)
            }
            IrStreamEvent::TextDelta { text, .. } => {
                let mut out = Vec::new();
                if !self.started {
                    out.extend_from_slice(&AnthropicMessagesCodec::encode_stream_event(
                        &IrStreamEvent::Start(IrResponseMeta::default()),
                    ));
                    self.started = true;
                }
                if self.open_block != Some(OpenBlock::Text) {
                    out.extend_from_slice(&self.close_open_block());
                    let index = self.next_index;
                    let payload = json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {"type": "text", "text": ""},
                    });
                    out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                    self.open_block = Some(OpenBlock::Text);
                    self.next_index += 1;
                }
                let payload = json!({
                    "type": "content_block_delta",
                    "index": self.index_of(OpenBlock::Text),
                    "delta": {"type": "text_delta", "text": text},
                });
                out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                out
            }
            IrStreamEvent::ReasoningDelta { text, .. } => {
                let mut out = Vec::new();
                if !self.started {
                    out.extend_from_slice(&AnthropicMessagesCodec::encode_stream_event(
                        &IrStreamEvent::Start(IrResponseMeta::default()),
                    ));
                    self.started = true;
                }
                if self.open_block != Some(OpenBlock::Thinking) {
                    out.extend_from_slice(&self.close_open_block());
                    let payload = json!({
                        "type": "content_block_start",
                        "index": self.next_index,
                        "content_block": {"type": "thinking", "thinking": ""},
                    });
                    out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                    self.open_block = Some(OpenBlock::Thinking);
                    self.next_index += 1;
                }
                let payload = json!({
                    "type": "content_block_delta",
                    "index": self.index_of(OpenBlock::Thinking),
                    "delta": {"type": "thinking_delta", "thinking": text},
                });
                out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                out
            }
            IrStreamEvent::ToolCallStart { id, name, .. } => {
                let mut out = Vec::new();
                if !self.started {
                    out.extend_from_slice(&AnthropicMessagesCodec::encode_stream_event(
                        &IrStreamEvent::Start(IrResponseMeta::default()),
                    ));
                    self.started = true;
                }
                out.extend_from_slice(&self.close_open_block());
                let index = self.next_index;
                let block = json!({
                    "type": "tool_use",
                    "id": id.clone().unwrap_or_default(),
                    "name": name.clone().unwrap_or_default(),
                    "input": {},
                });
                let payload = json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": block,
                });
                out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                self.open_block = Some(OpenBlock::ToolUse);
                self.next_index += 1;
                out
            }
            IrStreamEvent::ToolCallDelta {
                arguments_delta, ..
            } => {
                // Assumes a tool_use block is open (ToolCallStart preceded).
                if self.open_block != Some(OpenBlock::ToolUse) {
                    // Defensive: open one if the upstream skipped start.
                    let mut out = self.encode(&IrStreamEvent::ToolCallStart {
                        index: 0,
                        id: None,
                        name: None,
                    });
                    let payload = json!({
                        "type": "content_block_delta",
                        "index": self.next_index.saturating_sub(1),
                        "delta": {"type": "input_json_delta", "partial_json": arguments_delta},
                    });
                    out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                    return out;
                }
                let payload = json!({
                    "type": "content_block_delta",
                    "index": self.next_index.saturating_sub(1),
                    "delta": {"type": "input_json_delta", "partial_json": arguments_delta},
                });
                format!("data: {payload}\n\n").into_bytes()
            }
            IrStreamEvent::Finish { finish_reason, .. } => {
                let mut out = self.close_open_block();
                let stop = finish_to_stop(finish_reason.as_ref());
                let payload = json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": stop},
                });
                out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                out
            }
            IrStreamEvent::End => {
                let mut out = self.close_open_block();
                out.extend_from_slice(b"data: {\"type\":\"message_stop\"}\n\n");
                out
            }
            IrStreamEvent::Usage(_) | IrStreamEvent::Error(_) => Vec::new(),
        }
    }

    fn index_of(&self, kind: OpenBlock) -> u32 {
        // The index of the currently open block (assigned when opened).
        // next_index was already incremented past it.
        if self.open_block == Some(kind) {
            self.next_index.saturating_sub(1)
        } else {
            self.next_index
        }
    }

    fn close_open_block(&mut self) -> Vec<u8> {
        if let Some(kind) = self.open_block.take() {
            let index = self.next_index.saturating_sub(1);
            let _ = kind;
            format!(
                "data: {}\n\n",
                json!({"type": "content_block_stop", "index": index})
            )
            .into_bytes()
        } else {
            Vec::new()
        }
    }
}

// ── request helpers ───────────────────────────────────────────────

fn decode_request_message(
    m: &MessageParam,
    mi: usize,
    messages: &mut Vec<IrMessage>,
    loss: &mut sag_core::LossInventory,
) {
    let role = IrRole::from_raw(&m.role);

    // Message-level extras (unmodelled per-message fields) for round-trip.
    let raw = m.extra.clone();

    let content = match &m.content {
        MessageContent::Text(s) => vec![IrContentBlock::Text { text: s.clone() }],
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .enumerate()
            .map(|(bi, b)| decode_request_block(b, &format!("messages[{mi}]"), bi, loss))
            .collect(),
    };

    // tool_use blocks in an assistant message also populate tool_calls
    // so Chat-bridge encode can emit `tool_calls` (defect 2).
    let mut tool_calls = Vec::new();
    for block in &content {
        if let IrContentBlock::ToolUse {
            id,
            name,
            arguments,
        } = block
        {
            tool_calls.push(IrToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
                index: Some(tool_calls.len() as u32),
            });
        }
    }

    messages.push(IrMessage {
        role,
        content,
        name: None,
        tool_call_id: None,
        tool_calls,
        raw,
    });
}

/// Decode one `tools[]` entry into an [`IrTool`].
///
/// Hosted-tool detection (`type` starting with `web_search`), `input_schema`
/// defaulting and extras splitting all work on the tool's JSON object, so the
/// union is normalised to `Value` first.
fn decode_tool(t: &ToolUnion, i: usize, loss: &mut sag_core::LossInventory) -> Option<IrTool> {
    let value = tool_union_to_value(t);
    let tool_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let is_web_search = tool_type.starts_with("web_search");
    let name = value.get("name").and_then(|v| v.as_str()).map(String::from);
    let name = match name {
        Some(n) => n,
        None if is_web_search => "web_search".to_string(),
        None => {
            loss.push_loss(
                LossScope::Request,
                format!("tools[{i}].name"),
                LossReason::UnsupportedByIr,
                value.clone(),
            );
            return None;
        }
    };
    let description = value
        .get("description")
        .and_then(|v| v.as_str())
        .map(String::from);
    let input_schema = if is_web_search {
        Value::Null
    } else {
        value
            .get("input_schema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"}))
    };
    // Tool-level extras (cache_control, type, …) for round-trip.
    let mut tool_raw = Map::new();
    if let Some(obj) = value.as_object() {
        for (k, v) in obj {
            if !matches!(k.as_str(), "name" | "description" | "input_schema") {
                tool_raw.insert(k.clone(), v.clone());
            }
        }
    }
    Some(IrTool {
        name,
        description,
        input_schema,
        raw: tool_raw,
    })
}

fn decode_request_block(
    b: &AnthropicContentBlock,
    path: &str,
    bi: usize,
    loss: &mut sag_core::LossInventory,
) -> IrContentBlock {
    match b {
        AnthropicContentBlock::Text(t) => IrContentBlock::Text {
            text: t.text.clone(),
        },
        AnthropicContentBlock::Image(_) => IrContentBlock::Image {
            data: content_block_to_value(b),
        },
        AnthropicContentBlock::Document(_) => IrContentBlock::Document {
            data: content_block_to_value(b),
        },
        AnthropicContentBlock::ToolUse(t) => IrContentBlock::ToolUse {
            id: t.id.clone(),
            name: t.name.clone(),
            // Same as `decode_response`: IR carries arguments, not their
            // absence, so a missing `input` becomes `{}`.
            arguments: Value::Object(t.input.clone().unwrap_or_default()),
        },
        AnthropicContentBlock::ToolResult(t) => {
            let content = match &t.content {
                None => Vec::new(),
                Some(sag_core::api::anthropic::ToolResultContent::Text(s)) => {
                    vec![IrContentBlock::Text { text: s.clone() }]
                }
                Some(sag_core::api::anthropic::ToolResultContent::Blocks(blocks)) => blocks
                    .iter()
                    .enumerate()
                    .map(|(i, inner)| decode_request_block(inner, path, i, loss))
                    .collect(),
            };
            IrContentBlock::ToolResult {
                id: Some(t.tool_use_id.clone()),
                tool_call_id: t.tool_use_id.clone(),
                content,
                is_error: t.is_error.unwrap_or(false),
            }
        }
        AnthropicContentBlock::Thinking(t) => IrContentBlock::Reasoning {
            text: t.thinking.clone(),
        },
        AnthropicContentBlock::RedactedThinking(t) => IrContentBlock::Reasoning {
            text: t.data.clone(),
        },
        // Legacy tags outside the documented block union keep mapping to
        // Document (parity with the previous `Value` path).
        AnthropicContentBlock::Unknown(v)
            if matches!(v.get("type").and_then(|t| t.as_str()), Some("file" | "pdf")) =>
        {
            IrContentBlock::Document { data: v.clone() }
        }
        other => {
            let value = content_block_to_value(other);
            loss.push_loss(
                LossScope::ContentBlock,
                format!("{path}.content[{bi}]"),
                LossReason::UnsupportedByIr,
                value.clone(),
            );
            IrContentBlock::Raw(value)
        }
    }
}

/// Encode non-system IR messages into Anthropic user/assistant turns.
///
/// - `tool` role messages and `ToolResult` content blocks collapse into
///   the following/adjacent user turn as `tool_result` blocks.
/// - Assistant `ToolUse` blocks / `tool_calls` become `tool_use` blocks.
/// - Non-user/assistant roles default to `user` (Anthropic has only two).
/// - Unsupported blocks (e.g. Audio) record a loss entry in `loss`.
fn encode_messages(msgs: &[&IrMessage], loss: &mut sag_core::LossInventory) -> Vec<MessageParam> {
    // First pass: collect pending tool_results to merge into user turns.
    let mut out: Vec<MessageParam> = Vec::new();
    let mut pending_tool_results: Vec<AnthropicContentBlock> = Vec::new();

    for (mi, msg) in msgs.iter().enumerate() {
        // Tool-role message → tool_result block (merge into next user).
        if msg.role == IrRole::Tool {
            let tool_call_id = msg.tool_call_id.clone().unwrap_or_default();
            let text: String = msg
                .content
                .iter()
                .filter_map(|b| match b {
                    IrContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let is_error = msg
                .raw
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            pending_tool_results.push(tool_result_block(&tool_call_id, &text, is_error));
            continue;
        }

        let mut blocks: Vec<AnthropicContentBlock> = Vec::new();

        // Pending tool_results attach to this user turn first.
        if msg.role == IrRole::User && !pending_tool_results.is_empty() {
            blocks.append(&mut pending_tool_results);
        }

        // Content blocks → Anthropic blocks (skip pure-text fast path
        // only when empty).
        for block in &msg.content {
            if let IrContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
                id: _,
            } = block
            {
                let text: String = content
                    .iter()
                    .filter_map(|b| match b {
                        IrContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                blocks.push(tool_result_block(tool_call_id, &text, *is_error));
            } else {
                blocks.push(encode_content_block(
                    block,
                    &format!("messages[{mi}].content"),
                    loss,
                ));
            }
        }

        // Message-level tool_calls (Chat-origin IR) → tool_use blocks.
        // Dedup: decode mirrors content ToolUse into tool_calls; emitting both
        // would double-write the same tool_use for Anthropic upstream (B1).
        let content_tool_use_ids: std::collections::HashSet<&str> = msg
            .content
            .iter()
            .filter_map(|b| match b {
                IrContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        for tc in &msg.tool_calls {
            if content_tool_use_ids.contains(tc.id.as_str()) {
                continue;
            }
            blocks.push(content_block_from_value(json!({
                "type": "tool_use",
                "id": tc.id,
                "name": tc.name,
                "input": tc.arguments,
            })));
        }

        if blocks.is_empty() {
            // Empty content — Anthropic rejects empty turns; emit empty text.
            blocks.push(content_block_from_value(
                json!({"type": "text", "text": ""}),
            ));
        }

        // Single pure-text block → Anthropic accepts string content.
        let content = match blocks.as_slice() {
            [AnthropicContentBlock::Text(text)] if text.cache_control.is_none() => {
                MessageContent::Text(text.text.clone())
            }
            _ => MessageContent::Blocks(blocks),
        };

        let anthropic_role = if msg.role == IrRole::Assistant {
            "assistant"
        } else {
            "user"
        };

        out.push(MessageParam {
            role: anthropic_role.to_string(),
            content,
            extra: Map::new(),
        });
    }

    // Trailing tool_results with no following user turn → append a user turn.
    if !pending_tool_results.is_empty() {
        out.push(MessageParam {
            role: "user".to_string(),
            content: MessageContent::Blocks(pending_tool_results),
            extra: Map::new(),
        });
    }

    out
}

/// Build an Anthropic `tool_result` content block.
fn tool_result_block(tool_call_id: &str, text: &str, is_error: bool) -> AnthropicContentBlock {
    let mut block = Map::new();
    block.insert("type".into(), json!("tool_result"));
    block.insert("tool_use_id".into(), json!(tool_call_id));
    block.insert("content".into(), json!(text));
    if is_error {
        block.insert("is_error".into(), json!(true));
    }
    content_block_from_value(Value::Object(block))
}

fn encode_content_block(
    block: &IrContentBlock,
    path: &str,
    loss: &mut sag_core::LossInventory,
) -> AnthropicContentBlock {
    content_block_from_value(encode_content_block_value(block, path, loss))
}

fn encode_content_block_value(
    block: &IrContentBlock,
    path: &str,
    loss: &mut sag_core::LossInventory,
) -> Value {
    match block {
        IrContentBlock::Text { text } => json!({"type": "text", "text": text}),
        IrContentBlock::Reasoning { text } => json!({"type": "thinking", "thinking": text}),
        IrContentBlock::Image { data } => {
            // Already Anthropic-shaped → pass through; Chat-shaped image_url
            // → convert to Anthropic image source.
            if data.get("type").and_then(|t| t.as_str()) == Some("image") {
                data.clone()
            } else if let Some(url) = data
                .get("image_url")
                .and_then(|u| u.get("url"))
                .and_then(|u| u.as_str())
            {
                if url.starts_with("data:") {
                    // data URI → base64 source.
                    let (meta, b64) = url.split_once(',').unwrap_or(("data:image/png;base64", ""));
                    let media = meta
                        .trim_start_matches("data:")
                        .trim_end_matches(";base64")
                        .to_string();
                    json!({
                        "type": "image",
                        "source": {"type": "base64", "media_type": media, "data": b64},
                    })
                } else {
                    json!({
                        "type": "image",
                        "source": {"type": "url", "url": url},
                    })
                }
            } else {
                data.clone()
            }
        }
        IrContentBlock::Document { data } => {
            if data.get("type").is_some() {
                data.clone()
            } else {
                json!({"type": "document", "source": data})
            }
        }
        // Anthropic Messages has no first-class audio content block —
        // record an explicit loss and keep the original payload as a
        // Raw-style passthrough so the upstream can reject loudly
        // rather than silently dropping the block.
        IrContentBlock::Audio { data } => {
            loss.push_loss(
                LossScope::ContentBlock,
                path.to_string(),
                LossReason::UnsupportedByTarget,
                data.clone(),
            );
            data.clone()
        }
        IrContentBlock::ToolUse {
            id,
            name,
            arguments,
        } => json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": arguments,
        }),
        IrContentBlock::ToolResult {
            tool_call_id,
            content,
            is_error,
            ..
        } => {
            let text: String = content
                .iter()
                .filter_map(|b| match b {
                    IrContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let mut tr = Map::new();
            tr.insert("type".into(), json!("tool_result"));
            tr.insert("tool_use_id".into(), json!(tool_call_id));
            tr.insert("content".into(), json!(text));
            if *is_error {
                tr.insert("is_error".into(), json!(true));
            }
            Value::Object(tr)
        }
        IrContentBlock::Raw(v) => v.clone(),
    }
}

/// Map `ir.tool_choice` onto Anthropic's native `tool_choice` shapes (C6).
///
/// Anthropic `ToolChoice` is an object union only — never a bare string.
/// Native objects pass through silently: `{"type":"auto"|"any"|"none"}`
/// and `{"type":"tool","name":…}` (each optionally with
/// `disable_parallel_tool_use`).
///
/// Bare strings `"auto"` / `"any"` / `"none"` are pure wire adaptation
/// → `{"type":"<same>"}` with no loss. Non-native shapes record an
/// `UnsupportedByTarget` loss, then best-effort map: `"required"` (bare
/// or `{"type":"required"}`) → `{"type":"any"}`; a tool name
/// (`function.name` or top-level `name`) → `{"type":"tool","name":…}`;
/// any other unmappable value is omitted (returns `None`) so a foreign
/// shape never reaches Anthropic.
fn encode_tool_choice_for_anthropic(
    tool_choice: &Value,
    loss: &mut sag_core::LossInventory,
) -> Option<Value> {
    match tool_choice {
        Value::Null => None,
        Value::String(s) => match s.as_str() {
            "auto" | "any" | "none" => Some(json!({ "type": s.as_str() })),
            _ => {
                loss.push_loss(
                    LossScope::Request,
                    "tool_choice",
                    LossReason::UnsupportedByTarget,
                    tool_choice.clone(),
                );
                if s == "required" {
                    Some(json!({"type": "any"}))
                } else {
                    None
                }
            }
        },
        Value::Object(obj) => {
            let ty = obj.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if matches!(ty, "auto" | "any" | "none") {
                return Some(tool_choice.clone());
            }
            if ty == "tool"
                && let Some(name) = obj.get("name").and_then(|n| n.as_str())
                && !name.is_empty()
            {
                return Some(tool_choice.clone());
            }
            // Non-native object: record loss, then map `required` →
            // `any`, extract a name if present, else omit.
            loss.push_loss(
                LossScope::Request,
                "tool_choice",
                LossReason::UnsupportedByTarget,
                tool_choice.clone(),
            );
            if ty == "required" {
                return Some(json!({"type": "any"}));
            }
            let name = obj
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .or_else(|| obj.get("name").and_then(|n| n.as_str()))
                .filter(|n| !n.is_empty());
            name.map(|n| json!({"type": "tool", "name": n}))
        }
        // Illegal JSON types (number / array / bool) — record the drop
        // and omit rather than forward a foreign shape.
        other => {
            loss.push_loss(
                LossScope::Request,
                "tool_choice",
                LossReason::UnsupportedByTarget,
                other.clone(),
            );
            None
        }
    }
}

/// Rebuild `tools[]` from IR.
///
/// Extras merging, hosted-tool detection and `type` normalisation all operate
/// on the tool's JSON object, so each shaped tool is parsed back into the typed
/// union (`Unknown` keeps anything the DTO cannot express).
fn encode_tools(tools: &[IrTool]) -> Vec<ToolUnion> {
    tools
        .iter()
        .map(|t| {
            // M8 hosted web_search: official Anthropic shape is
            // `{"type":"web_search_20250305","name":"web_search",...}`.
            // Only treat as hosted when `raw.type` **starts with**
            // `web_search` — a same-named function tool (type absent /
            // `"function"`) must not be misclassified. `raw.type` is read
            // through the dual-convention accessor so chat-sourced IR is
            // tolerated too.
            let raw_type = tool_raw::tool_raw_type(t).unwrap_or("");
            if raw_type.starts_with("web_search") {
                let mut tool = Map::new();
                tool.insert("type".into(), json!("web_search_20250305"));
                tool.insert("name".into(), json!(t.name));
                if let Some(d) = &t.description {
                    tool.insert("description".into(), json!(d));
                }
                // Preserve extras (max_uses, cache_control, …) but never
                // the original type (pinned above), input_schema, or the
                // chat-only reserved markers.
                tool_raw::merge_hosted_extras(&mut tool, t, &["type", "input_schema"]);
                return tool_union_from_value(Value::Object(tool));
            }
            let mut tool = Map::new();
            tool.insert("name".into(), json!(t.name));
            if let Some(d) = &t.description {
                tool.insert("description".into(), json!(d));
            }
            tool.insert("input_schema".into(), t.input_schema.clone());
            // Anthropic tool objects are flat: tool-level and function-level
            // extras (incl. `strict`) both return to the root. Chat's reserved
            // `__tool_extra` / `__function_extra` markers are never emitted.
            tool_raw::merge_extras(&mut tool, t);
            // Anthropic's tool-type enum has no `"function"`: an OpenAI
            // `"function"` marker (and the `""` stand-in for "no type") is
            // omitted regardless of which codec sourced the IR — a custom
            // tool is function-semantic here. Any other value (`"custom"`,
            // …) passes through verbatim; a missing `type` stays missing.
            if matches!(
                tool.get("type").and_then(|v| v.as_str()),
                Some("function" | "")
            ) {
                tool.remove("type");
            }
            tool_union_from_value(Value::Object(tool))
        })
        .collect()
}

/// Inject `cache_control` markers into an Anthropic request **Value**
/// (provider layer; not IR). Marks: last system block, last tool, and the
/// last content block of the last user message before the final turn.
pub fn inject_cache_control_value(body: &mut Value) {
    // 1. System: string → single-element blocks with cache_control; blocks
    //    → mark last.
    if let Some(system) = body.get("system").cloned() {
        match system {
            Value::String(s) => {
                if !s.is_empty() {
                    body["system"] = json!([
                        {"type": "text", "text": s, "cache_control": {"type": "ephemeral"}}
                    ]);
                }
            }
            Value::Array(mut blocks) => {
                if let Some(last) = blocks.last_mut()
                    && let Some(obj) = last.as_object_mut()
                {
                    obj.insert("cache_control".into(), json!({"type": "ephemeral"}));
                }
                body["system"] = Value::Array(blocks);
            }
            _ => {}
        }
    }

    // 2. Tools: mark the last tool definition.
    if let Some(tools) = body.get_mut("tools").and_then(|t| t.as_array_mut())
        && let Some(last) = tools.last_mut()
        && let Some(obj) = last.as_object_mut()
    {
        obj.insert("cache_control".into(), json!({"type": "ephemeral"}));
    }

    // 3. Messages: last content block of the last user message that is not
    //    the final message (when ≥ 2 messages).
    if let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut())
        && messages.len() >= 2
    {
        let mut user_idx: Option<usize> = None;
        for (i, msg) in messages.iter().enumerate().rev().skip(1) {
            if msg.get("role").and_then(|r| r.as_str()) == Some("user") {
                user_idx = Some(i);
                break;
            }
        }
        if let Some(idx) = user_idx
            && let Some(content) = messages[idx].get_mut("content")
        {
            match content {
                Value::String(s) => {
                    let t = std::mem::take(s);
                    if !t.is_empty() {
                        *content = json!([
                            {"type": "text", "text": t, "cache_control": {"type": "ephemeral"}}
                        ]);
                    }
                }
                Value::Array(blocks) => {
                    if let Some(last) = blocks.last_mut()
                        && let Some(obj) = last.as_object_mut()
                    {
                        obj.insert("cache_control".into(), json!({"type": "ephemeral"}));
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── typed test helpers ──────────────────────────────────────

    /// Deserialize a `json!` body into the typed DTO, then decode it.
    fn decode_req(body: Value) -> IrRequest {
        let req: AnthropicMessagesRequest =
            serde_json::from_value(body).expect("body must deserialize into the typed request");
        AnthropicMessagesCodec::decode_request(&req)
    }

    /// Encode, then serialize back to JSON for wire-shape assertions.
    fn encode_json(ir: &IrRequest) -> Value {
        serde_json::to_value(AnthropicMessagesCodec::encode_request(ir))
            .expect("encoded request must serialize")
    }

    /// [`AnthropicMessagesCodec::encode_request_with_loss`] + serialization.
    fn encode_json_with_loss(ir: &IrRequest, model: &str) -> (Value, sag_core::LossInventory) {
        let (body, loss) = AnthropicMessagesCodec::encode_request_with_loss(ir, model);
        (
            serde_json::to_value(body).expect("encoded request must serialize"),
            loss,
        )
    }

    /// Deserialize a `json!` body into the typed DTO, then decode it.
    fn decode_resp(body: Value) -> IrResponse {
        let resp: AnthropicMessagesResponse =
            serde_json::from_value(body).expect("body must deserialize into the typed response");
        AnthropicMessagesCodec::decode_response(&resp)
    }

    /// Encode, then serialize back to JSON for wire-shape assertions.
    fn encode_resp_json(ir: &IrResponse) -> Value {
        serde_json::to_value(AnthropicMessagesCodec::encode_response(ir))
            .expect("encoded response must serialize")
    }

    // ── resolve_budget (migrated from anthropic.rs) ─────────────

    #[test]
    fn resolve_budget_high_hits_direct() {
        assert_eq!(resolve_budget("high", 10000), Some(8192));
    }

    #[test]
    fn resolve_budget_xhigh_downgrades_to_high() {
        assert_eq!(resolve_budget("xhigh", 10000), Some(8192));
    }

    #[test]
    fn resolve_budget_high_downgrades_to_low() {
        assert_eq!(resolve_budget("high", 4096), Some(2048));
    }

    #[test]
    fn resolve_budget_medium_downgrades_to_minimal() {
        assert_eq!(resolve_budget("medium", 2048), Some(1024));
    }

    #[test]
    fn resolve_budget_minimal_hits_direct() {
        assert_eq!(resolve_budget("minimal", 2048), Some(1024));
    }

    #[test]
    fn resolve_budget_max_hits_direct() {
        assert_eq!(resolve_budget("max", 40000), Some(32768));
    }

    #[test]
    fn resolve_budget_all_fail_when_max_tokens_too_small() {
        assert_eq!(resolve_budget("high", 1024), None);
    }

    #[test]
    fn resolve_budget_unknown_effort_returns_none() {
        assert_eq!(resolve_budget("ultra", 10000), None);
    }

    #[test]
    fn resolve_budget_boundary_exact_fit() {
        assert_eq!(resolve_budget("low", 3072), Some(2048));
    }

    #[test]
    fn resolve_budget_low_downgrades_to_minimal_at_boundary() {
        assert_eq!(resolve_budget("low", 3071), Some(1024));
    }

    #[test]
    fn resolve_budget_all_fail_when_even_minimal_cannot_fit() {
        assert_eq!(resolve_budget("low", 2047), None);
    }

    // ── request round-trips ─────────────────────────────────────

    #[test]
    fn request_roundtrip_plain_text() {
        let body = json!({
            "model": "claude-sonnet-4",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": "hello"}
            ]
        });
        let ir = decode_req(body);
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert_eq!(ir.model, "claude-sonnet-4");
        assert_eq!(ir.max_tokens, Some(1024));
        assert_eq!(ir.messages.len(), 1);
        let back = encode_json(&ir);
        assert_eq!(back["model"], "claude-sonnet-4");
        assert_eq!(back["max_tokens"], 1024);
        assert_eq!(back["messages"][0]["content"], "hello");
    }

    #[test]
    fn request_system_blocks_roundtrip() {
        let body = json!({
            "model": "m",
            "max_tokens": 100,
            "system": [
                {"type": "text", "text": "be brief"},
                {"type": "text", "text": "no jokes"}
            ],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let ir = decode_req(body);
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert_eq!(ir.messages[0].role, IrRole::System);
        assert_eq!(ir.messages[0].content.len(), 2);
        let back = encode_json(&ir);
        assert_eq!(back["system"].as_array().unwrap().len(), 2);
        assert_eq!(back["system"][0]["text"], "be brief");
    }

    #[test]
    fn request_tool_use_and_tool_result_roundtrip() {
        let body = json!({
            "model": "m",
            "max_tokens": 2048,
            "messages": [
                {"role": "user", "content": "weather in SF?"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "checking"},
                    {"type": "tool_use", "id": "tu_1", "name": "get_weather",
                     "input": {"city": "SF"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_1", "content": "72F"}
                ]}
            ],
            "tools": [{
                "name": "get_weather",
                "description": "Get weather",
                "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}
            }]
        });
        let ir = decode_req(body);
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        // tool_use → ToolUse block + tool_calls
        assert!(matches!(
            ir.messages[1].content[1],
            IrContentBlock::ToolUse { .. }
        ));
        assert_eq!(ir.messages[1].tool_calls.len(), 1);
        assert_eq!(ir.messages[1].tool_calls[0].name, "get_weather");
        // tool_result → ToolResult block
        assert!(matches!(
            ir.messages[2].content[0],
            IrContentBlock::ToolResult { .. }
        ));
        // tools → IrTool with input_schema
        assert_eq!(ir.tools.len(), 1);
        assert_eq!(ir.tools[0].name, "get_weather");
        assert!(ir.tools[0].input_schema.get("properties").is_some());

        let back = encode_json(&ir);
        // tool_use preserved exactly once (no double-write from tool_calls mirror).
        let assistant_content = back["messages"][1]["content"]
            .as_array()
            .expect("assistant content must be array");
        assert_eq!(
            assistant_content.len(),
            2,
            "content must stay [text, tool_use] — tool_calls mirror must not re-append: {assistant_content:?}"
        );
        assert_eq!(assistant_content[0]["type"], "text");
        assert_eq!(assistant_content[1]["type"], "tool_use");
        assert_eq!(assistant_content[1]["id"], "tu_1");
        // tool_result preserved (merged into user turn)
        let m3 = &back["messages"][2];
        let content3 = &m3["content"];
        let blocks3: Vec<&Value> = if content3.is_array() {
            content3.as_array().unwrap().iter().collect()
        } else {
            // string form wouldn't carry tool_result — must be array
            panic!("tool_result turn must be array content: {content3}");
        };
        assert_eq!(blocks3[0]["type"], "tool_result");
        assert_eq!(blocks3[0]["tool_use_id"], "tu_1");
        // tools input_schema preserved
        assert_eq!(back["tools"][0]["name"], "get_weather");
        assert!(back["tools"][0]["input_schema"].get("properties").is_some());
    }

    /// Multi-turn tool_use → tool_result must survive decode → encode → decode
    /// without block growth (B1: no double-write across turns).
    #[test]
    fn request_multiturn_tool_use_tool_result_roundtrip() {
        let body = json!({
            "model": "m",
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
                {"role": "assistant", "content": [
                    {"type": "text", "text": "It is 72F in SF."}
                ]},
                {"role": "user", "content": "and New York?"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_2", "name": "get_weather",
                     "input": {"city": "NYC"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_2", "content": "55F"}
                ]}
            ],
            "tools": [{
                "name": "get_weather",
                "description": "Get weather",
                "input_schema": {"type": "object"}
            }]
        });
        let ir1 = decode_req(body);
        assert!(ir1.loss.is_empty(), "loss: {:?}", ir1.loss);
        assert_eq!(ir1.messages[1].content.len(), 1);
        assert_eq!(ir1.messages[1].tool_calls.len(), 1);

        let enc1 = encode_json(&ir1);
        let msgs1 = enc1["messages"].as_array().expect("messages array");
        assert_eq!(msgs1.len(), 7, "message count must not grow on encode");
        // First tool_use turn: content has exactly one tool_use.
        assert_eq!(msgs1[1]["content"].as_array().unwrap().len(), 1);
        assert_eq!(msgs1[1]["content"][0]["type"], "tool_use");
        // Second tool_use turn likewise.
        assert_eq!(msgs1[5]["content"].as_array().unwrap().len(), 1);
        assert_eq!(msgs1[5]["content"][0]["id"], "tu_2");
        // tool_result turns stay single-block.
        assert_eq!(msgs1[2]["content"].as_array().unwrap().len(), 1);
        assert_eq!(msgs1[6]["content"][0]["tool_use_id"], "tu_2");

        // Second round: encode → decode must be stable (no growth).
        let ir2 = decode_req(enc1.clone());
        assert!(ir2.loss.is_empty(), "loss: {:?}", ir2.loss);
        assert_eq!(ir2.messages.len(), ir1.messages.len());
        let enc2 = encode_json(&ir2);
        assert_eq!(enc1, enc2, "second encode must equal first encode");
    }

    #[test]
    fn request_image_block_roundtrip() {
        let body = json!({
            "model": "m",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "what is this"},
                    {"type": "image", "source": {"type": "url", "url": "https://x/y.png"}}
                ]
            }]
        });
        let ir = decode_req(body);
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert!(matches!(
            ir.messages[0].content[1],
            IrContentBlock::Image { .. }
        ));
        let back = encode_json(&ir);
        assert_eq!(back["messages"][0]["content"][1]["type"], "image");
        assert_eq!(
            back["messages"][0]["content"][1]["source"]["url"],
            "https://x/y.png"
        );
    }

    #[test]
    fn request_thinking_decoded_to_reasoning() {
        let body = json!({
            "model": "m",
            "max_tokens": 10000,
            "thinking": {"type": "enabled", "budget_tokens": 8192},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let ir = decode_req(body);
        assert!(ir.reasoning.budget_tokens.is_some());
        assert_eq!(ir.reasoning.budget_tokens, Some(8192));
        assert_eq!(ir.reasoning.effort.as_deref(), Some("high"));
        assert!(ir.reasoning.thinking.is_some());
        let back = encode_json(&ir);
        assert_eq!(back["thinking"]["type"], "enabled");
        assert_eq!(back["thinking"]["budget_tokens"], 8192);
    }

    #[test]
    fn request_unknown_fields_go_to_raw_and_roundtrip() {
        let body = json!({
            "model": "m",
            "max_tokens": 100,
            "service_tier": "auto",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let ir = decode_req(body);
        assert!(ir.loss.is_empty());
        assert_eq!(ir.raw.get("service_tier"), Some(&json!("auto")));
        let back = encode_json(&ir);
        assert_eq!(back["service_tier"], "auto");
    }

    /// R1: every typed field without an IR home (plus unmodelled keys) must
    /// survive `decode → ir.raw → encode` so native passthrough stays intact.
    #[test]
    fn request_typed_fields_without_ir_home_roundtrip() {
        let body = json!({
            "model": "m",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hi"}],
            "service_tier": "auto",
            "inference_geo": "us",
            "cache_control": {"type": "ephemeral"},
            "container": "container_1",
            "output_config": {"effort": "high"},
            "unknown_top": {"x": 1}
        });
        let ir = decode_req(body);
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        for key in [
            "service_tier",
            "inference_geo",
            "cache_control",
            "container",
            "output_config",
            "unknown_top",
        ] {
            assert!(ir.raw.contains_key(key), "{key} must ride in ir.raw");
        }

        let back = encode_json(&ir);
        assert_eq!(back["service_tier"], "auto");
        assert_eq!(back["inference_geo"], "us");
        assert_eq!(back["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(back["container"], "container_1");
        assert_eq!(back["output_config"], json!({"effort": "high"}));
        assert_eq!(back["unknown_top"], json!({"x": 1}));

        // Second round: the restored typed slots keep the chain lossless.
        let ir2 = decode_req(back);
        assert_eq!(
            ir2.raw.get("output_config"),
            Some(&json!({"effort": "high"}))
        );
        assert_eq!(ir2.raw.get("service_tier"), Some(&json!("auto")));
    }

    #[test]
    fn encode_max_tokens_missing_records_loss_and_uses_default() {
        let ir = IrRequest {
            model: "m".into(),
            messages: vec![IrMessage {
                role: IrRole::User,
                content: vec![IrContentBlock::Text { text: "hi".into() }],
                ..Default::default()
            }],
            max_tokens: None,
            ..Default::default()
        };
        let (body, loss) = encode_json_with_loss(&ir, "m");
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
        assert!(!loss.is_empty(), "missing max_tokens must record a loss");
        assert_eq!(loss.entries[0].field, "max_tokens");
    }

    #[test]
    fn encode_thinking_budget_too_small_records_loss_and_omits() {
        // effort=high, max_tokens=1024 → no budget fits → loss + omit.
        let ir = IrRequest {
            model: "m".into(),
            max_tokens: Some(1024),
            reasoning: Reasoning {
                effort: Some("high".into()),
                budget_tokens: None,
                thinking: None,
            },
            ..Default::default()
        };
        let (body, loss) = encode_json_with_loss(&ir, "m");
        assert!(
            body.get("thinking").is_none(),
            "thinking must be omitted when no budget fits"
        );
        assert!(
            loss.entries.iter().any(|e| e.field == "thinking"),
            "thinking drop must record a loss; got {:?}",
            loss.entries
        );
    }

    #[test]
    fn encode_thinking_budget_ok_emits_enabled() {
        let ir = IrRequest {
            model: "m".into(),
            max_tokens: Some(10000),
            reasoning: Reasoning {
                effort: Some("high".into()),
                budget_tokens: None,
                thinking: None,
            },
            ..Default::default()
        };
        let (body, loss) = encode_json_with_loss(&ir, "m");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 8192);
        assert!(loss.is_empty());
    }

    /// B3: a silent budget downgrade (high → low) must record a loss entry
    /// even though thinking is still emitted at the reduced budget.
    #[test]
    fn encode_thinking_budget_downgrade_records_loss() {
        // high=8192 needs max_tokens ≥ 9216; 4096 only fits low=2048.
        let ir = IrRequest {
            model: "m".into(),
            max_tokens: Some(4096),
            reasoning: Reasoning {
                effort: Some("high".into()),
                budget_tokens: None,
                thinking: None,
            },
            ..Default::default()
        };
        let (body, loss) = encode_json_with_loss(&ir, "m");
        assert_eq!(body["thinking"]["budget_tokens"], 2048);
        let entry = loss
            .entries
            .iter()
            .find(|e| e.field == "thinking.budget_tokens")
            .unwrap_or_else(|| panic!("downgrade must record loss; got {:?}", loss.entries));
        assert_eq!(entry.reason, LossReason::DroppedByTargetFeature);
        assert_eq!(entry.value["nominal"], 8192);
        assert_eq!(entry.value["actual"], 2048);
    }

    // ── encode_tool_choice_for_anthropic (C6) ──────────────────

    #[test]
    fn tool_choice_native_object_shapes_pass_without_loss() {
        for tc in [
            json!({"type": "auto"}),
            json!({"type": "any"}),
            json!({"type": "none"}),
            json!({"type": "auto", "disable_parallel_tool_use": true}),
            json!({"type": "tool", "name": "f"}),
            json!({"type": "tool", "name": "f", "disable_parallel_tool_use": true}),
        ] {
            let mut loss = sag_core::LossInventory::default();
            let out = encode_tool_choice_for_anthropic(&tc, &mut loss);
            assert_eq!(out, Some(tc.clone()), "native {tc} must pass through");
            assert!(loss.is_empty(), "native {tc} must not record loss");
        }
    }

    #[test]
    fn tool_choice_bare_auto_any_none_adapt_to_object_without_loss() {
        for s in ["auto", "any", "none"] {
            let mut loss = sag_core::LossInventory::default();
            let out = encode_tool_choice_for_anthropic(&json!(s), &mut loss);
            assert_eq!(
                out,
                Some(json!({"type": s})),
                "bare {s:?} must be adapted to object form"
            );
            assert!(loss.is_empty(), "wire adaptation of {s:?} is not a loss");
        }
    }

    #[test]
    fn tool_choice_chat_function_shape_maps_to_tool_with_loss() {
        let tc = json!({"type": "function", "function": {"name": "f"}});
        let mut loss = sag_core::LossInventory::default();
        let out = encode_tool_choice_for_anthropic(&tc, &mut loss);
        assert_eq!(out, Some(json!({"type": "tool", "name": "f"})));
        assert_eq!(loss.len(), 1);
        assert_eq!(loss.entries[0].field, "tool_choice");
        assert_eq!(loss.entries[0].reason, LossReason::UnsupportedByTarget);
        assert_eq!(loss.entries[0].value, tc);
    }

    #[test]
    fn tool_choice_responses_function_shape_maps_to_tool_with_loss() {
        let tc = json!({"type": "function", "name": "f"});
        let mut loss = sag_core::LossInventory::default();
        let out = encode_tool_choice_for_anthropic(&tc, &mut loss);
        assert_eq!(out, Some(json!({"type": "tool", "name": "f"})));
        assert_eq!(loss.len(), 1);
        assert_eq!(loss.entries[0].reason, LossReason::UnsupportedByTarget);
    }

    #[test]
    fn tool_choice_required_maps_to_any_object_with_loss() {
        for tc in [json!("required"), json!({"type": "required"})] {
            let mut loss = sag_core::LossInventory::default();
            let out = encode_tool_choice_for_anthropic(&tc, &mut loss);
            assert_eq!(out, Some(json!({"type": "any"})), "{tc} → any object");
            assert_eq!(loss.len(), 1, "{tc} must record loss");
            assert_eq!(loss.entries[0].field, "tool_choice");
            assert_eq!(loss.entries[0].reason, LossReason::UnsupportedByTarget);
            assert_eq!(loss.entries[0].value, tc);
        }
    }

    #[test]
    fn tool_choice_unmappable_object_omitted_with_loss() {
        // Hosted / unknown object without a usable tool name.
        for tc in [
            json!({"type": "web_search"}),
            json!({"type": "custom", "foo": 1}),
            json!({"type": "function"}),
        ] {
            let mut loss = sag_core::LossInventory::default();
            let out = encode_tool_choice_for_anthropic(&tc, &mut loss);
            assert_eq!(out, None, "{tc} must be omitted");
            assert_eq!(loss.len(), 1, "{tc} must record loss");
            assert_eq!(loss.entries[0].field, "tool_choice");
            assert_eq!(loss.entries[0].value, tc);
        }
    }

    #[test]
    fn tool_choice_illegal_types_omitted_with_loss() {
        for tc in [json!(1), json!(true), json!(["auto"]), json!("banana")] {
            let mut loss = sag_core::LossInventory::default();
            let out = encode_tool_choice_for_anthropic(&tc, &mut loss);
            assert_eq!(out, None, "{tc} must be omitted");
            assert_eq!(loss.len(), 1, "{tc} must record loss");
        }
        // Null → no field, no loss.
        let mut loss = sag_core::LossInventory::default();
        assert_eq!(
            encode_tool_choice_for_anthropic(&Value::Null, &mut loss),
            None
        );
        assert!(loss.is_empty());
    }

    #[test]
    fn encode_request_tool_choice_wins_over_raw_roundtrip() {
        // Explicit mapped tool_choice must win; raw must not re-inject
        // a foreign shape when the value was intentionally omitted.
        let ir = IrRequest {
            model: "m".into(),
            max_tokens: Some(100),
            tool_choice: json!({"type": "function", "function": {"name": "f"}}),
            ..Default::default()
        };
        let (body, loss) = encode_json_with_loss(&ir, "m");
        assert_eq!(body["tool_choice"], json!({"type": "tool", "name": "f"}));
        assert!(
            loss.entries.iter().any(|e| e.field == "tool_choice"),
            "mapped non-native shape must record loss"
        );

        let mut ir2 = IrRequest {
            model: "m".into(),
            max_tokens: Some(100),
            tool_choice: json!({"type": "web_search"}),
            ..Default::default()
        };
        ir2.raw
            .insert("tool_choice".into(), json!({"type": "web_search"}));
        let (body2, loss2) = encode_json_with_loss(&ir2, "m");
        assert!(
            body2.get("tool_choice").is_none(),
            "omitted shape must not re-enter via raw; got {:?}",
            body2.get("tool_choice")
        );
        assert!(
            loss2.entries.iter().any(|e| e.field == "tool_choice"),
            "omitted shape must record loss"
        );
    }

    // ── response round-trips ────────────────────────────────────

    #[test]
    fn response_roundtrip_text_tool_use_thinking() {
        let body = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4",
            "content": [
                {"type": "thinking", "thinking": "let me think"},
                {"type": "text", "text": "hello"},
                {"type": "tool_use", "id": "tu_1", "name": "fn",
                 "input": {"a": 1}}
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_read_input_tokens": 3,
                "cache_creation_input_tokens": 1,
                "thinking_tokens": 2
            }
        });
        let ir = decode_resp(body);
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert_eq!(ir.id, "msg_1");
        assert_eq!(ir.model, "claude-sonnet-4");
        assert!(ir.created.is_none(), "must not fabricate created");
        assert_eq!(ir.output.len(), 3);
        assert!(matches!(ir.output[0], IrOutputItem::Reasoning { .. }));
        assert!(matches!(ir.output[1], IrOutputItem::Text { .. }));
        assert!(matches!(ir.output[2], IrOutputItem::ToolCall(_)));
        assert_eq!(ir.finish_reason, Some(IrFinishReason::ToolCalls));
        let usage = ir.usage.clone().expect("usage");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.cached_tokens, Some(3));
        assert_eq!(usage.cache_creation_tokens, Some(1));
        assert_eq!(usage.reasoning_tokens, Some(2));

        let back = encode_resp_json(&ir);
        assert_eq!(back["id"], "msg_1");
        assert_eq!(back["type"], "message");
        assert_eq!(back["stop_reason"], "tool_use");
        assert_eq!(back["content"][0]["type"], "thinking");
        assert_eq!(back["content"][1]["type"], "text");
        assert_eq!(back["content"][2]["type"], "tool_use");
        assert_eq!(back["content"][2]["input"]["a"], 1);
        assert_eq!(back["usage"]["input_tokens"], 10);
        assert_eq!(back["usage"]["cache_read_input_tokens"], 3);
        assert_eq!(back["usage"]["thinking_tokens"], 2);
    }

    #[test]
    fn response_stop_reason_mappings() {
        for (stop, expect) in [
            ("end_turn", IrFinishReason::Stop),
            ("stop_sequence", IrFinishReason::Stop),
            ("max_tokens", IrFinishReason::Length),
            ("tool_use", IrFinishReason::ToolCalls),
            ("refusal", IrFinishReason::Other("refusal".into())),
        ] {
            let body = json!({
                "id": "x", "type": "message", "role": "assistant", "model": "m",
                "content": [{"type": "text", "text": "t"}],
                "stop_reason": stop,
                "usage": {"input_tokens": 1, "output_tokens": 1}
            });
            let ir = decode_resp(body);
            assert_eq!(ir.finish_reason, Some(expect.clone()), "stop={stop}");
        }
    }

    /// R1: typed response fields without an IR home must survive
    /// `decode → ir.raw → encode` (native passthrough).
    #[test]
    fn response_typed_fields_without_ir_home_roundtrip() {
        let body = json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [{"type": "text", "text": "t"}],
            "stop_reason": "stop_sequence",
            "stop_sequence": "END",
            "container": {"id": "c_1"},
            "stop_details": {"type": "refusal", "category": "cyber"},
            "usage": {"input_tokens": 1, "output_tokens": 2}
        });
        let ir = decode_resp(body);
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        for key in ["stop_sequence", "container", "stop_details"] {
            assert!(ir.raw.contains_key(key), "{key} must ride in ir.raw");
        }

        let back = encode_resp_json(&ir);
        assert_eq!(back["stop_sequence"], "END");
        assert_eq!(back["container"], json!({"id": "c_1"}));
        assert_eq!(
            back["stop_details"],
            json!({"type": "refusal", "category": "cyber"})
        );
        assert_eq!(back["usage"]["input_tokens"], 1);
    }

    // ── usage fidelity ──────────────────────────────────────────

    /// M3: one ill-typed `usage` sub-object must not zero the counters that
    /// parsed fine (per-field degradation, old `Value`-based semantics).
    #[test]
    fn usage_degrades_per_field_when_subobject_is_malformed() {
        let usage = anthropic_usage(&json!({
            "input_tokens": 10,
            "output_tokens": 20,
            "cache_read_input_tokens": 4,
            "cache_creation_input_tokens": 6,
            // String where a u64 is documented → typed parse fails.
            "cache_creation": {"ephemeral_5m_input_tokens": "1"},
            "server_tool_use": {"web_search_requests": "1"},
            "output_tokens_details": {"thinking_tokens": 7}
        }));
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 20);
        assert_eq!(usage.total_tokens, 30);
        assert_eq!(usage.cached_tokens, Some(4));
        assert_eq!(usage.cache_creation_tokens, Some(6));
        assert_eq!(usage.reasoning_tokens, Some(7));
    }

    /// M3: the degraded path keeps the old defaults — missing counters are 0,
    /// cache counters are "known zero" once `input_tokens > 0`, and a legacy
    /// root-level `thinking_tokens` is still read.
    #[test]
    fn usage_degraded_path_keeps_old_defaults() {
        let usage = anthropic_usage(&json!({
            "input_tokens": 10,
            "output_tokens": 20,
            "cache_creation": {"ephemeral_1h_input_tokens": false},
            "thinking_tokens": 5
        }));
        assert_eq!(usage.total_tokens, 30);
        assert_eq!(usage.cached_tokens, Some(0));
        assert_eq!(usage.cache_creation_tokens, Some(0));
        assert_eq!(usage.reasoning_tokens, Some(5));

        // No counters at all → all-zero, cache counters unknown.
        let empty = anthropic_usage(&json!({"cache_creation": []}));
        assert_eq!(empty.prompt_tokens, 0);
        assert_eq!(empty.cached_tokens, None);
        assert_eq!(empty.cache_creation_tokens, None);
        assert_eq!(empty.reasoning_tokens, None);
    }

    // ── faithful round-trip guards (R3) ────────────────────────

    /// M5: a missing `input` must survive the ingress egress unchanged, and an
    /// explicit `"input": {}` must survive as an empty object. The DTOs carry
    /// `input` as an `Option`, so absent and explicitly-empty stay distinct
    /// and both keep their wire shape through the typed bridge.
    #[test]
    fn tool_use_block_without_input_keeps_wire_shape() {
        for value in [
            json!({"type": "tool_use", "id": "tu_1", "name": "fn"}),
            json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search"}),
        ] {
            let block = content_block_from_value(value.clone());
            let back = serde_json::to_value(&block).expect("block must serialize");
            assert_eq!(back, value, "wire shape must not grow `input`");
        }

        // Explicit empty `input` → typed variant, empty object preserved.
        for value in [
            json!({"type": "tool_use", "id": "tu_1", "name": "fn", "input": {}}),
            json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                   "input": {}}),
        ] {
            let block = content_block_from_value(value.clone());
            assert!(
                !matches!(block, AnthropicContentBlock::Unknown(_)),
                "explicit empty input must stay typed: {value}"
            );
            let back = serde_json::to_value(&block).expect("block must serialize");
            assert_eq!(back, value, "explicit empty `input` must survive verbatim");
        }

        // `input` present → the typed variant is still taken.
        let typed = content_block_from_value(
            json!({"type": "tool_use", "id": "tu_1", "name": "fn", "input": {"a": 1}}),
        );
        assert!(matches!(typed, AnthropicContentBlock::ToolUse(_)));
    }

    /// M5: the ingress egress (`IrOutputItem::Raw` → `encode_response`) must
    /// not fabricate `input` either.
    #[test]
    fn response_raw_block_without_input_keeps_wire_shape() {
        let ir = IrResponse {
            id: "msg_1".into(),
            created: None,
            model: "m".into(),
            output: vec![IrOutputItem::Raw(
                json!({"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search"}),
            )],
            finish_reason: None,
            usage: None,
            raw: Map::new(),
            loss: sag_core::LossInventory::default(),
        };
        let back = encode_resp_json(&ir);
        let block = &back["content"][0];
        assert_eq!(block["type"], "server_tool_use");
        assert!(
            block.get("input").is_none(),
            "encode must not fabricate `input`: {block}"
        );
    }

    /// M5 follow-up (reviewer §6, plan B): `decode_response`'s Raw arm
    /// serializes the **typed** block via `content_block_to_value`, bypassing
    /// `parse_faithful`. With `input` defaulted but not skipped, the typed
    /// parse fabricated `"input": {}` before that arm ran, so the guard could
    /// not help — the pollution was client-visible on every translated lane.
    /// The full typed round-trip must now keep the upstream wire shape, in both
    /// directions: absent stays absent, explicit `{}` stays `{}`.
    #[test]
    fn decode_response_typed_path_does_not_grow_server_tool_use_input() {
        let ir = decode_resp(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [
                {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search"}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }));
        let out = encode_resp_json(&ir);
        let block = &out["content"][0];
        assert_eq!(block["type"], "server_tool_use");
        assert!(
            block.get("input").is_none(),
            "typed decode path must not fabricate `input`: {block}"
        );

        // Explicit `"input": {}` upstream must come back as `"input": {}` —
        // the Raw arm re-serializes the typed block, so the DTO has to keep the
        // empty object instead of collapsing it into "absent".
        let ir = decode_resp(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [
                {"type": "server_tool_use", "id": "srvtoolu_1", "name": "web_search",
                 "input": {}}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }));
        let out = encode_resp_json(&ir);
        let block = &out["content"][0];
        assert_eq!(block["type"], "server_tool_use");
        assert_eq!(
            block.get("input"),
            Some(&json!({})),
            "explicit empty `input` must survive the typed decode path: {block}"
        );

        // A `tool_use` block *is* IR-native (`IrToolCall`), so the egress
        // synthesizes `input` from `arguments` — an empty map means "no
        // arguments" and is a property of the IR contract, not of the serde
        // default. Pin that distinction so it is not mistaken for the bug.
        let ir = decode_resp(json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [
                {"type": "tool_use", "id": "tu_1", "name": "fn", "input": {"a": 1}},
                {"type": "tool_use", "id": "tu_2", "name": "noargs"}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        }));
        let out = encode_resp_json(&ir);
        assert_eq!(out["content"][0]["input"], json!({"a": 1}));
        assert_eq!(out["content"][1]["input"], json!({}));
    }

    /// M5: the faithful guard does not disturb well-formed unions.
    #[test]
    fn faithful_guard_keeps_well_formed_unions_typed() {
        assert!(matches!(
            tool_union_from_value(json!({"name": "fn", "input_schema": {"type": "object"}})),
            ToolUnion::Custom(_)
        ));
        assert!(matches!(
            tool_union_from_value(json!({"type": "web_search_20250305", "name": "web_search"})),
            ToolUnion::Builtin(_)
        ));
        assert!(matches!(
            tool_choice_from_value(json!({"type": "tool", "name": "fn"})),
            ToolChoice::Tool(_)
        ));
        assert!(matches!(
            thinking_config_from_value(json!({"type": "enabled", "budget_tokens": 2048})),
            ThinkingConfig::Enabled(_)
        ));
    }

    // ── stream decode table ─────────────────────────────────────

    #[test]
    fn stream_message_start_yields_start_and_usage() {
        let frame = br#"data: {"type":"message_start","message":{"id":"msg_1","model":"claude-3","usage":{"input_tokens":10,"output_tokens":1}}}"#;
        let events = AnthropicMessagesCodec::decode_stream_frame(frame);
        assert!(
            matches!(events[0], IrStreamEvent::Start(ref m) if m.id.as_deref() == Some("msg_1"))
        );
        assert!(matches!(events[1], IrStreamEvent::Usage(ref u) if u.prompt_tokens == 10));
    }

    #[test]
    fn stream_content_block_start_tool_use_yields_tool_call_start() {
        let frame = br#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu_1","name":"fn","input":{}}}"#;
        let events = AnthropicMessagesCodec::decode_stream_frame(frame);
        assert!(matches!(
            events[0],
            IrStreamEvent::ToolCallStart { index: 1, ref id, ref name }
                if id.as_deref() == Some("tu_1") && name.as_deref() == Some("fn")
        ));
    }

    #[test]
    fn stream_thinking_delta_yields_reasoning_delta() {
        let frame = br#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#;
        let events = AnthropicMessagesCodec::decode_stream_frame(frame);
        assert!(matches!(
            events[0],
            IrStreamEvent::ReasoningDelta { index: 0, ref text } if text == "hmm"
        ));
    }

    #[test]
    fn stream_text_delta_yields_text_delta() {
        let frame = br#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#;
        let events = AnthropicMessagesCodec::decode_stream_frame(frame);
        assert!(matches!(
            events[0],
            IrStreamEvent::TextDelta { index: 0, ref text } if text == "hi"
        ));
    }

    #[test]
    fn stream_input_json_delta_yields_tool_call_delta() {
        let frame = br#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}"#;
        let events = AnthropicMessagesCodec::decode_stream_frame(frame);
        assert!(matches!(
            events[0],
            IrStreamEvent::ToolCallDelta { index: 1, ref arguments_delta } if arguments_delta == "{\"a\":"
        ));
    }

    #[test]
    fn stream_message_delta_yields_finish_and_usage() {
        let frame = br#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}"#;
        let events = AnthropicMessagesCodec::decode_stream_frame(frame);
        assert!(matches!(
            events[0],
            IrStreamEvent::Finish {
                finish_reason: Some(IrFinishReason::ToolCalls),
                ..
            }
        ));
        assert!(matches!(events[1], IrStreamEvent::Usage(ref u) if u.completion_tokens == 42));
    }

    #[test]
    fn stream_message_stop_yields_end() {
        let frame = br#"data: {"type":"message_stop"}"#;
        let events = AnthropicMessagesCodec::decode_stream_frame(frame);
        assert!(matches!(events[0], IrStreamEvent::End));
    }

    #[test]
    fn stream_ping_yields_no_event() {
        let frame = br#"data: {"type":"ping"}"#;
        let events = AnthropicMessagesCodec::decode_stream_frame(frame);
        assert!(events.is_empty(), "ping must be ignored: {events:?}");
    }

    #[test]
    fn stream_error_yields_error_and_raw() {
        let frame =
            br#"data: {"type":"error","error":{"type":"overloaded_error","message":"boom"}}"#;
        let events = AnthropicMessagesCodec::decode_stream_frame(frame);
        assert!(matches!(events[0], IrStreamEvent::Error(ref m) if m == "boom"));
        assert!(matches!(events[1], IrStreamEvent::Raw(_)));
    }

    #[test]
    fn stream_unknown_event_yields_raw() {
        let frame = br#"data: {"type":"mystery_event","x":1}"#;
        let events = AnthropicMessagesCodec::decode_stream_frame(frame);
        assert!(matches!(events[0], IrStreamEvent::Raw(_)));
    }

    // ── stream encode ───────────────────────────────────────────

    #[test]
    fn stream_encode_suppresses_done() {
        let out = AnthropicMessagesCodec::encode_stream_event(&IrStreamEvent::Raw(
            b"data: [DONE]\n\n".to_vec(),
        ));
        assert!(out.is_empty(), "must not emit [DONE]: {out:?}");
    }

    #[test]
    fn stream_encode_raw_passthrough() {
        let raw = b"data: {\"type\":\"custom\"}\n\n".to_vec();
        let out = AnthropicMessagesCodec::encode_stream_event(&IrStreamEvent::Raw(raw.clone()));
        assert_eq!(out, raw);
    }

    #[test]
    fn stream_encoder_emits_block_lifecycle() {
        let enc = AnthropicMessagesCodec::stream_encoder();
        let mut all = Vec::new();
        all.extend(enc(&IrStreamEvent::Start(IrResponseMeta {
            id: Some("msg_1".into()),
            created: None,
            model: Some("m".into()),
        })));
        all.extend(enc(&IrStreamEvent::TextDelta {
            index: 0,
            text: "hi".into(),
        }));
        all.extend(enc(&IrStreamEvent::Finish {
            index: 0,
            finish_reason: Some(IrFinishReason::Stop),
        }));
        all.extend(enc(&IrStreamEvent::End));
        let s = String::from_utf8(all).unwrap();
        assert!(s.contains("message_start"), "missing message_start: {s}");
        assert!(
            s.contains("content_block_start"),
            "missing content_block_start: {s}"
        );
        assert!(
            s.contains("content_block_stop"),
            "missing content_block_stop: {s}"
        );
        assert!(s.contains("message_delta"), "missing message_delta: {s}");
        assert!(s.contains("message_stop"), "missing message_stop: {s}");
        assert!(!s.contains("[DONE]"), "must not emit [DONE]: {s}");
    }

    // ── cache_control injection ─────────────────────────────────

    #[test]
    fn inject_cache_control_marks_system_tools_and_user() {
        let mut body = json!({
            "system": "you are helpful",
            "tools": [{"name": "a", "input_schema": {}}, {"name": "b", "input_schema": {}}],
            "messages": [
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": "answer"},
                {"role": "user", "content": "second"}
            ]
        });
        inject_cache_control_value(&mut body);
        // system → blocks with cache_control
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        // last tool marked
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
        // last user before final turn (index 0) marked
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );
        assert!(
            body["messages"][2]
                .get("content")
                .and_then(|c| c.get("cache_control"))
                .is_none()
        );
    }

    // ── M8 hosted web_search tool mapping ─────────────────────────

    #[test]
    fn encode_web_search_tool_uses_official_anthropic_shape() {
        let mut ir = IrRequest {
            model: "claude".into(),
            ..Default::default()
        };
        ir.messages.push(IrMessage {
            role: IrRole::User,
            content: vec![IrContentBlock::Text {
                text: "news".into(),
            }],
            ..Default::default()
        });
        ir.tools.push(IrTool {
            name: "web_search".into(),
            description: None,
            input_schema: Value::Null,
            raw: {
                let mut m = Map::new();
                m.insert("type".into(), json!("web_search"));
                m.insert("max_uses".into(), json!(5));
                m
            },
        });
        ir.max_tokens = Some(64);
        let (body, loss) = encode_json_with_loss(&ir, "claude");
        assert_eq!(body["tools"][0]["type"], "web_search_20250305");
        assert_eq!(body["tools"][0]["name"], "web_search");
        assert_eq!(body["tools"][0]["max_uses"], 5);
        assert!(
            body["tools"][0].get("input_schema").is_none(),
            "web_search has no input_schema: {}",
            body["tools"][0]
        );
        // No unexpected tool losses.
        let tool_losses: Vec<_> = loss
            .entries
            .iter()
            .filter(|e| e.field.starts_with("tools"))
            .collect();
        assert!(tool_losses.is_empty(), "tool losses: {tool_losses:?}");
    }

    #[test]
    fn decode_web_search_tool_preserves_raw_type() {
        let body = json!({
            "model": "claude",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 3}]
        });
        let ir = decode_req(body);
        assert_eq!(ir.tools.len(), 1);
        assert_eq!(ir.tools[0].name, "web_search");
        assert_eq!(
            ir.tools[0].raw.get("type").and_then(|v| v.as_str()),
            Some("web_search_20250305")
        );
        assert_eq!(ir.tools[0].raw.get("max_uses"), Some(&json!(3)));
        let back = encode_json(&ir);
        assert_eq!(back["tools"][0]["type"], "web_search_20250305");
        assert_eq!(back["tools"][0]["max_uses"], 3);
    }

    // ── M8 document / audio content ──────────────────────────────

    #[test]
    fn document_content_roundtrip() {
        let body = json!({
            "model": "claude",
            "max_tokens": 64,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "summarize"},
                    {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "AA=="}}
                ]
            }]
        });
        let ir = decode_req(body);
        assert!(
            matches!(ir.messages[0].content[1], IrContentBlock::Document { .. }),
            "document block → Document"
        );
        let back = encode_json(&ir);
        assert_eq!(back["messages"][0]["content"][1]["type"], "document");
        assert_eq!(
            back["messages"][0]["content"][1]["source"]["media_type"],
            "application/pdf"
        );
    }

    #[test]
    fn audio_content_records_loss_and_keeps_payload() {
        // S3: Anthropic has no first-class audio block — encode must
        // `loss.push` (never a silent passthrough that pretends it was
        // recorded) while keeping the original payload for loud upstream
        // rejection.
        let audio = json!({
            "type": "input_audio",
            "input_audio": {"data": "AAAA", "format": "wav"}
        });
        let ir = IrRequest {
            model: "claude".into(),
            max_tokens: Some(64),
            messages: vec![IrMessage {
                role: IrRole::User,
                content: vec![IrContentBlock::Audio {
                    data: audio.clone(),
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let (back, loss) = encode_json_with_loss(&ir, "claude");
        assert!(
            !loss.is_empty(),
            "audio encode must record a loss entry (got empty inventory)"
        );
        assert!(
            loss.entries
                .iter()
                .any(|e| e.field.contains("messages[0].content")
                    && matches!(e.reason, LossReason::UnsupportedByTarget)),
            "loss must name the audio content block with UnsupportedByTarget: {:?}",
            loss.entries
        );
        // Payload kept (Raw-style passthrough) so upstream rejects loudly.
        let encoded = &back["messages"][0]["content"][0];
        assert_eq!(
            encoded.get("type").and_then(|t| t.as_str()),
            Some("input_audio"),
            "audio payload structure must be preserved: {encoded}"
        );
        assert_eq!(
            encoded
                .pointer("/input_audio/format")
                .and_then(|t| t.as_str()),
            Some("wav")
        );
    }

    #[test]
    fn function_tool_named_web_search_is_not_hosted() {
        // Nit: only `raw.type` starting with `web_search` counts as the
        // hosted family — a function tool merely **named** `web_search`
        // must keep the function shape (input_schema etc.).
        let mut ir = IrRequest {
            model: "claude".into(),
            max_tokens: Some(64),
            ..Default::default()
        };
        ir.messages.push(IrMessage {
            role: IrRole::User,
            content: vec![IrContentBlock::Text { text: "hi".into() }],
            ..Default::default()
        });
        ir.tools.push(IrTool {
            name: "web_search".into(),
            description: Some("custom search".into()),
            input_schema: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
            raw: {
                let mut m = Map::new();
                m.insert("type".into(), json!("function"));
                m
            },
        });
        let (body, _) = encode_json_with_loss(&ir, "claude");
        assert!(
            body["tools"][0].get("type").is_none(),
            "`function` type is omitted (adjudication: value-based mapping), and the tool must not be remapped to hosted web_search: {}",
            body["tools"][0]
        );
        assert_eq!(body["tools"][0]["name"], "web_search");
        assert!(
            body["tools"][0].get("input_schema").is_some(),
            "function tool keeps input_schema: {}",
            body["tools"][0]
        );
    }

    /// Cross-codec: chat-sourced dual-layer `raw` expands to the flat
    /// Anthropic tool shape. The `"function"` type marker is omitted (the
    /// Anthropic tool-type enum has no such value) and no reserved marker
    /// reaches the wire.
    #[test]
    fn encode_tools_expands_chat_dual_layer_and_omits_function_type() {
        let mut raw = Map::new();
        raw.insert(tool_raw::TOOL_EXTRA_KEY.into(), json!({"type": "function"}));
        raw.insert(tool_raw::FUNCTION_EXTRA_KEY.into(), json!({"strict": true}));
        let mut ir = IrRequest {
            model: "claude".into(),
            max_tokens: Some(64),
            ..Default::default()
        };
        ir.messages.push(IrMessage {
            role: IrRole::User,
            content: vec![IrContentBlock::Text { text: "hi".into() }],
            ..Default::default()
        });
        ir.tools.push(IrTool {
            name: "fn".into(),
            description: Some("d".into()),
            input_schema: json!({"type": "object"}),
            raw,
        });
        let (body, loss) = encode_json_with_loss(&ir, "claude");
        let tool = &body["tools"][0];
        assert!(
            tool.get("type").is_none(),
            "`function` type marker must be omitted: {tool:?}"
        );
        assert_eq!(tool["name"], json!("fn"));
        assert_eq!(tool["description"], json!("d"));
        assert_eq!(tool["input_schema"], json!({"type": "object"}));
        assert_eq!(tool["strict"], json!(true), "strict lands at the root");
        for k in tool.as_object().expect("tool object").keys() {
            assert!(!tool_raw::is_reserved_key(k), "reserved marker leaked: {k}");
        }
        let tool_losses: Vec<_> = loss
            .entries
            .iter()
            .filter(|e| e.field.starts_with("tools"))
            .collect();
        assert!(tool_losses.is_empty(), "tool losses: {tool_losses:?}");
    }

    /// Chat-sourced non-`function` type (e.g. `"custom"`) passes through
    /// verbatim, and the extras land at the root.
    #[test]
    fn encode_tools_chat_dual_layer_custom_type_passes_through() {
        let mut raw = Map::new();
        raw.insert(
            tool_raw::TOOL_EXTRA_KEY.into(),
            json!({"type": "custom", "cache_control": {"type": "ephemeral"}}),
        );
        raw.insert(tool_raw::FUNCTION_EXTRA_KEY.into(), json!({}));
        let mut ir = IrRequest {
            model: "claude".into(),
            max_tokens: Some(64),
            ..Default::default()
        };
        ir.messages.push(IrMessage {
            role: IrRole::User,
            content: vec![IrContentBlock::Text { text: "hi".into() }],
            ..Default::default()
        });
        ir.tools.push(IrTool {
            name: "fn".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            raw,
        });
        let (body, _) = encode_json_with_loss(&ir, "claude");
        let tool = &body["tools"][0];
        assert_eq!(tool["type"], json!("custom"));
        assert_eq!(tool["cache_control"], json!({"type": "ephemeral"}));
        for k in tool.as_object().expect("tool object").keys() {
            assert!(!tool_raw::is_reserved_key(k), "reserved marker leaked: {k}");
        }
    }

    /// Adjudicated: the Anthropic `type` mapping is **value**-based, not
    /// source-based — a flat (Responses-ingress) `"function"` and the `""`
    /// stand-in for "no type" are both omitted; other values pass through
    /// verbatim and an absent `type` stays absent.
    #[test]
    fn encode_tools_flat_type_maps_by_value() {
        for (source_type, expected) in [
            (Some("function"), None),
            (Some(""), None),
            (Some("custom"), Some("custom")),
            (None, None),
        ] {
            let mut raw = Map::new();
            if let Some(ty) = source_type {
                raw.insert("type".into(), json!(ty));
            }
            let mut ir = IrRequest {
                model: "claude".into(),
                max_tokens: Some(64),
                ..Default::default()
            };
            ir.messages.push(IrMessage {
                role: IrRole::User,
                content: vec![IrContentBlock::Text { text: "hi".into() }],
                ..Default::default()
            });
            ir.tools.push(IrTool {
                name: "fn".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                raw,
            });
            let (body, _) = encode_json_with_loss(&ir, "claude");
            let tool = &body["tools"][0];
            assert_eq!(
                tool.get("type").and_then(|v| v.as_str()),
                expected,
                "source type {source_type:?} → {tool:?}"
            );
            assert_eq!(tool["name"], json!("fn"));
        }
    }
}
