//! OpenAI Responses ↔ IR codec (M6 stateless subset + M8 stateful native
//! subset — plan §5.2 / D1 / M8).
//!
//! Pure mapping: Responses request/response/SSE ↔ IR types.
//!
//! Ingress side: `decode_request` / `decode_request_allow_stateful` /
//! `encode_response` / `encode_stream_event`.
//! Upstream side: `encode_request` / `decode_response` / `decode_stream_frame`.
//!
//! Hard rejects (D1, plan §4.4): `previous_response_id` / `store=true` /
//! `background` / unsupported tools return [`AppError::BadRequest`],
//! rendered as HTTP 422 by the `/v1/responses` handler — never
//! capability-precheck drops. Complex `tool_choice` shapes are opaque
//! raw passthrough into `ir.tool_choice` (C6), not hard rejects.
//!
//! M8 stateful subset: `decode_request_allow_stateful` permits
//! `previous_response_id` / `store=true` into `IrRequest.raw` so the
//! **native** Responses lane re-emits them on encode. `background` is
//! still always rejected (M8 does not implement background execution).

use std::cell::RefCell;

use serde_json::{Map, Value, json};

use sag_core::{
    AppError, IrContentBlock, IrFinishReason, IrMessage, IrOutputItem, IrRequest, IrResponse,
    IrResponseMeta, IrRole, IrStreamEvent, IrTool, IrToolCall, LossReason, LossScope, Reasoning,
    Sampling, Usage,
};

use super::tool_raw;

/// Stateless OpenAI Responses codec: decode to IR and encode back.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResponsesCodec;

// ═══════════════════════════════════════════════════════════════════
// D1 capability rejects / M8 stateful mode
// ═══════════════════════════════════════════════════════════════════

fn reject(msg: impl Into<String>) -> AppError {
    AppError::BadRequest(msg.into())
}

/// Whether ingress decode permits stateful Responses fields
/// (`previous_response_id` / `store=true`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatefulMode {
    /// Translated-only route (or unknown): reject stateful fields (422).
    Reject,
    /// Route has at least one explicit `upstream_protocol=Responses`
    /// target: allow stateful fields into `IrRequest.raw` for native
    /// passthrough. `background` is still rejected.
    Allow,
}

/// Reject stateful / background features **before** decode (D1 / M8).
/// `store=false` and absent fields are always allowed.
///
/// `mode == Allow` skips the `previous_response_id` / `store=true` /
/// bare-response-id-input rejects (M8 native passthrough) but still
/// rejects `background`.
pub fn reject_unsupported_mode(body: &Value, mode: StatefulMode) -> Result<(), AppError> {
    if mode == StatefulMode::Reject
        && body
            .get("previous_response_id")
            .is_some_and(|v| !v.is_null())
    {
        return Err(reject(
            "previous_response_id is not supported (stateless Responses only)",
        ));
    }
    if mode == StatefulMode::Reject
        && let Some(store) = body.get("store")
    {
        // Reject bool true and the string form "true" (N1).
        let is_true = store.as_bool() == Some(true)
            || store
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("true"));
        if is_true {
            return Err(reject(
                "store=true is not supported (stateless Responses only)",
            ));
        }
    }
    // background is always rejected (M8 does not execute background work).
    if body
        .get("background")
        .is_some_and(|v| !v.is_null() && *v != Value::Bool(false))
    {
        return Err(reject(
            "background is not supported (stateless Responses only)",
        ));
    }
    // input as a bare response id string (stateful form) — only when
    // stateful is not allowed for this route.
    if mode == StatefulMode::Reject
        && let Some(input) = body.get("input").filter(|v| !v.is_null())
        && let Value::String(s) = input
        && (s.starts_with("resp_") || s.starts_with("response_"))
        && !s.contains(char::is_whitespace)
        && s.len() > 8
    {
        // Heuristic: a bare response-id string input is the stateful form.
        // Real free-form short inputs contain spaces or are ordinary prose;
        // OpenAI response ids are `resp_[a-f0-9…]`. Only reject the clear
        // `resp_`/`response_` prefix without spaces to avoid false positives.
        return Err(reject(
            "input as a previous response id is not supported (stateless Responses only)",
        ));
    }
    Ok(())
}

/// Reject stateful / background features with [`StatefulMode::Reject`]
/// (default ingress path for translated-only routes).
pub fn reject_unsupported(body: &Value) -> Result<(), AppError> {
    reject_unsupported_mode(body, StatefulMode::Reject)
}

// ═══════════════════════════════════════════════════════════════════
// helpers
// ═══════════════════════════════════════════════════════════════════

/// Responses usage object → IR [`Usage`].
fn responses_usage(value: &Value) -> Usage {
    let input = value
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let output = value
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let total = value
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or_else(|| input.saturating_add(output));
    let input_details = value.get("input_tokens_details");
    let output_details = value.get("output_tokens_details");
    Usage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: total,
        cached_tokens: input_details
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
            .or((input > 0).then_some(0)),
        cache_creation_tokens: None,
        reasoning_tokens: output_details
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
    }
}

/// IR [`Usage`] → Responses usage object.
fn usage_to_responses(usage: &Usage) -> Value {
    let mut m = Map::new();
    m.insert("input_tokens".into(), json!(usage.prompt_tokens));
    m.insert("output_tokens".into(), json!(usage.completion_tokens));
    m.insert("total_tokens".into(), json!(usage.total_tokens));
    let mut in_d = Map::new();
    if let Some(c) = usage.cached_tokens {
        in_d.insert("cached_tokens".into(), json!(c));
    }
    if !in_d.is_empty() {
        m.insert("input_tokens_details".into(), Value::Object(in_d));
    }
    let mut out_d = Map::new();
    if let Some(r) = usage.reasoning_tokens {
        out_d.insert("reasoning_tokens".into(), json!(r));
    }
    if !out_d.is_empty() {
        m.insert("output_tokens_details".into(), Value::Object(out_d));
    }
    Value::Object(m)
}

/// Responses `status` / `incomplete_details` → [`IrFinishReason`].
fn status_to_finish(
    status: &str,
    incomplete_reason: Option<&str>,
    has_function_call: bool,
) -> Option<IrFinishReason> {
    match status {
        "completed" => Some(if has_function_call {
            IrFinishReason::FunctionCall
        } else {
            IrFinishReason::Stop
        }),
        "incomplete" => Some(match incomplete_reason {
            Some("max_output_tokens") => IrFinishReason::Length,
            Some("content_filter") => IrFinishReason::ContentFilter,
            Some(other) => IrFinishReason::Other(other.to_string()),
            None => IrFinishReason::Other("incomplete".into()),
        }),
        "failed" => Some(IrFinishReason::Other("failed".into())),
        "cancelled" | "canceled" => Some(IrFinishReason::Cancelled),
        other => Some(IrFinishReason::Other(other.to_string())),
    }
}

/// Opaque-passthrough `tool_choice` validation (C6): any string or object
/// shape is kept byte-identical in `ir.tool_choice`; only illegal JSON
/// types (number / array / bool) are rejected (422).
fn decode_tool_choice(v: &Value) -> Result<Value, AppError> {
    match v {
        Value::Null => Ok(Value::Null),
        Value::String(_) | Value::Object(_) => Ok(v.clone()),
        // numbers / arrays / bools are not legal tool_choice shapes — reject loudly.
        _ => Err(reject(
            "tool_choice must be a string or object (auto/none/required/function/…)",
        )),
    }
}

/// Validate a `tools[]` entry. M8: `type:"function"`, `type:"web_search"`
/// and the `web_search_preview*` family are supported; everything else
/// (`hosted` / `custom` / `file_search` / unknown) is rejected (422).
fn tool_reject(t: &Value) -> Option<AppError> {
    let ty = t.get("type").and_then(|x| x.as_str()).unwrap_or("");
    let is_web_search_family = ty.starts_with("web_search");
    match ty {
        "function" | "" => {}
        _ if is_web_search_family => {}
        other => {
            return Some(reject(format!(
                "tools[].type \"{other}\" is not supported (only \"function\" and \"web_search\" are supported on Responses)"
            )));
        }
    }
    // Shape-less custom entries without name also unsupported
    // (web_search family may omit name — default filled at decode).
    if !is_web_search_family && t.get("name").is_none() && t.get("function").is_none() {
        return Some(reject(
            "unsupported tools[] entry (only type \"function\" with name is supported)",
        ));
    }
    None
}

/// Request-content part → IR content block.
fn decode_content_part(
    part: &Value,
    path: &str,
    loss: &mut sag_core::LossInventory,
) -> IrContentBlock {
    match part.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "text" | "input_text" | "output_text" => IrContentBlock::Text {
            text: part
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
        },
        "refusal" => IrContentBlock::Text {
            text: part
                .get("refusal")
                .or_else(|| part.get("text"))
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
        },
        "input_image" | "image" => IrContentBlock::Image { data: part.clone() },
        "input_file" | "file" => IrContentBlock::Document { data: part.clone() },
        "input_audio" | "audio" => IrContentBlock::Audio { data: part.clone() },
        "summary_text" | "reasoning_text" => IrContentBlock::Reasoning {
            text: part
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string(),
        },
        other => {
            loss.push_loss(
                LossScope::ContentBlock,
                format!("{path}.content[{other}]"),
                LossReason::UnsupportedByIr,
                part.clone(),
            );
            IrContentBlock::Raw(part.clone())
        }
    }
}

impl ResponsesCodec {
    // ═══════════════════════════════════════════════════════════
    // request decode (ingress): Responses body → IrRequest
    // ═══════════════════════════════════════════════════════════

    /// Decode a Responses request body into IR (stateless subset).
    ///
    /// Returns [`AppError::BadRequest`] (rendered as 422 by the handler)
    /// for D1-unsupported features and unsupported tools.
    pub fn decode_request(body: &Value) -> sag_core::Result<IrRequest> {
        Self::decode_request_with_mode(body, StatefulMode::Reject)
    }

    /// M8: decode allowing stateful fields (`previous_response_id` /
    /// `store=true`) into `IrRequest.raw` for native Responses passthrough.
    /// `background` is still rejected.
    pub fn decode_request_allow_stateful(body: &Value) -> sag_core::Result<IrRequest> {
        Self::decode_request_with_mode(body, StatefulMode::Allow)
    }

    /// Decode with an explicit [`StatefulMode`].
    pub fn decode_request_with_mode(
        body: &Value,
        mode: StatefulMode,
    ) -> sag_core::Result<IrRequest> {
        reject_unsupported_mode(body, mode)?;

        let mut loss = sag_core::LossInventory::default();
        let mut raw = Map::new();

        // Typed non-IR fields → raw for encode round-trip.
        const KNOWN: &[&str] = &[
            "model",
            "instructions",
            "input",
            "stream",
            "tools",
            "max_output_tokens",
            "temperature",
            "top_p",
            "tool_choice",
            "parallel_tool_calls",
            "text",
            "reasoning",
            // D1 rejects above; store=false still round-trips via raw.
            "store",
            "background",
            "previous_response_id",
        ];
        if let Some(obj) = body.as_object() {
            for (k, v) in obj {
                // Unknown fields and `store` (even when false) round-trip via
                // raw. In Allow mode `previous_response_id` also lands in raw
                // so encode_request re-emits it for the native lane.
                let keep = !KNOWN.contains(&k.as_str())
                    || k == "store"
                    || (mode == StatefulMode::Allow && k == "previous_response_id");
                if keep {
                    raw.insert(k.clone(), v.clone());
                }
            }
        }

        let model = body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        let stream = body
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut messages = Vec::new();

        // instructions → leading system message (multi-segment preserved).
        if let Some(instr) = body.get("instructions").filter(|v| !v.is_null()) {
            let content = match instr {
                Value::String(s) => vec![IrContentBlock::Text { text: s.clone() }],
                Value::Array(parts) => parts
                    .iter()
                    .map(|p| decode_content_part(p, "instructions", &mut loss))
                    .collect(),
                other => {
                    loss.push_loss(
                        LossScope::Request,
                        "instructions",
                        LossReason::UnsupportedByIr,
                        other.clone(),
                    );
                    vec![IrContentBlock::Raw(other.clone())]
                }
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

        // input → messages / tool results / assistant tool_calls.
        match body.get("input") {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) => {
                messages.push(IrMessage {
                    role: IrRole::User,
                    content: vec![IrContentBlock::Text { text: s.clone() }],
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                    raw: Map::new(),
                });
            }
            Some(Value::Array(items)) => {
                for (ii, item) in items.iter().enumerate() {
                    decode_input_item(item, ii, &mut messages, &mut loss);
                }
            }
            Some(other) => {
                loss.push_loss(
                    LossScope::Request,
                    "input",
                    LossReason::UnsupportedByIr,
                    other.clone(),
                );
                messages.push(IrMessage {
                    role: IrRole::User,
                    content: vec![IrContentBlock::Raw(other.clone())],
                    ..Default::default()
                });
            }
        }

        // tools: type:"function" and type:"web_search" (M8).
        let mut tools = Vec::new();
        if let Some(arr) = body
            .get("tools")
            .filter(|v| !v.is_null())
            .and_then(|v| v.as_array())
        {
            for (i, t) in arr.iter().enumerate() {
                if let Some(err) = tool_reject(t) {
                    return Err(err);
                }
                let ty = t.get("type").and_then(|x| x.as_str()).unwrap_or("");
                if ty.starts_with("web_search") {
                    // Hosted web_search / web_search_preview family: preserve
                    // original shape in raw.
                    let name = t
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("web_search")
                        .to_string();
                    let mut tool_raw = Map::new();
                    if let Some(obj) = t.as_object() {
                        for (k, v) in obj {
                            if k != "name" {
                                tool_raw.insert(k.clone(), v.clone());
                            }
                        }
                    }
                    // Ensure type is always present for Anthropic/Responses
                    // re-encode detection (keep the original type — may be
                    // `web_search_preview*`).
                    tool_raw
                        .entry("type".to_string())
                        .or_insert_with(|| json!("web_search"));
                    tools.push(IrTool {
                        name,
                        description: t
                            .get("description")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        input_schema: Value::Null,
                        raw: tool_raw,
                    });
                    continue;
                }
                let name = t
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .or_else(|| {
                        t.pointer("/function/name")
                            .and_then(|v| v.as_str())
                            .map(String::from)
                    });
                let Some(name) = name else {
                    loss.push_loss(
                        LossScope::Request,
                        format!("tools[{i}]"),
                        LossReason::UnsupportedByIr,
                        t.clone(),
                    );
                    continue;
                };
                let description = t
                    .get("description")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let input_schema = t.get("parameters").cloned().unwrap_or(Value::Null);
                let mut tool_raw = Map::new();
                if let Some(obj) = t.as_object() {
                    for (k, v) in obj {
                        if !matches!(k.as_str(), "name" | "description" | "parameters") {
                            tool_raw.insert(k.clone(), v.clone());
                        }
                    }
                }
                tools.push(IrTool {
                    name,
                    description,
                    input_schema,
                    raw: tool_raw,
                });
            }
        }

        // sampling / limits.
        let temperature = body.get("temperature").and_then(|v| v.as_f64());
        let top_p = body.get("top_p").and_then(|v| v.as_f64());
        let max_tokens = body
            .get("max_output_tokens")
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok());

        let tool_choice = match body.get("tool_choice") {
            None | Some(Value::Null) => Value::Null,
            Some(v) => decode_tool_choice(v)?,
        };

        if let Some(v) = body.get("parallel_tool_calls") {
            raw.insert("parallel_tool_calls".into(), v.clone());
        }
        if let Some(v) = body.get("text") {
            raw.insert("text".into(), v.clone());
        }

        // reasoning: best-effort effort mapping + raw passthrough.
        let mut reasoning = Reasoning::default();
        if let Some(r) = body.get("reasoning").filter(|v| !v.is_null()) {
            raw.insert("reasoning".into(), r.clone());
            if let Some(effort) = r.get("effort").and_then(|v| v.as_str()) {
                reasoning.effort = Some(effort.to_string());
            }
            if let Some(b) = r.get("budget_tokens").and_then(|v| v.as_u64())
                && let Ok(b) = u32::try_from(b)
            {
                reasoning.budget_tokens = Some(b);
            }
        }

        Ok(IrRequest {
            model,
            messages,
            sampling: Sampling {
                temperature,
                top_p,
                top_k: None,
                seed: None,
                presence_penalty: None,
                frequency_penalty: None,
                logit_bias: None,
            },
            max_tokens,
            stream,
            tools,
            tool_choice,
            response_format: Value::Null,
            reasoning,
            stop: Vec::new(),
            metadata: Value::Null,
            raw,
            loss,
        })
    }

    // ═══════════════════════════════════════════════════════════
    // request encode (upstream): IrRequest → Responses body
    // ═══════════════════════════════════════════════════════════

    /// Encode IR → Responses body using `ir.model`.
    pub fn encode_request(ir: &IrRequest) -> Value {
        Self::encode_request_with_loss(ir, &ir.model).0
    }

    /// Encode IR → Responses body with an explicit model (real_model override).
    pub fn encode_request_for_model(ir: &IrRequest, model: &str) -> Value {
        Self::encode_request_with_loss(ir, model).0
    }

    /// Encode IR → Responses body, returning encode-side losses (S1):
    /// system non-Text blocks dropped when lifting to `instructions`,
    /// and ToolUse/ToolResult content dropped when re-encoding messages.
    pub fn encode_request_with_loss(
        ir: &IrRequest,
        model: &str,
    ) -> (Value, sag_core::LossInventory) {
        let mut loss = ir.loss.clone();
        let body = Self::encode_request_inner(ir, model, &mut loss);
        (body, loss)
    }

    fn encode_request_inner(
        ir: &IrRequest,
        model: &str,
        loss: &mut sag_core::LossInventory,
    ) -> Value {
        let mut body = Map::new();
        body.insert("model".into(), json!(model));

        // system messages → instructions (joined; multi/long preserved).
        // Non-Text system blocks cannot live on a bare string → loss (S1).
        let (system_msgs, chat_msgs): (Vec<&IrMessage>, Vec<&IrMessage>) =
            ir.messages.iter().partition(|m| m.role == IrRole::System);
        if !system_msgs.is_empty() {
            let mut text = String::new();
            for (si, m) in system_msgs.iter().enumerate() {
                for (bi, b) in m.content.iter().enumerate() {
                    match b {
                        IrContentBlock::Text { text: t } => text.push_str(t),
                        other => {
                            loss.push_loss(
                                LossScope::ContentBlock,
                                format!("system[{si}].content[{bi}]"),
                                LossReason::UnsupportedByIr,
                                serde_json::to_value(other).unwrap_or(Value::Null),
                            );
                        }
                    }
                }
            }
            if !text.is_empty() {
                body.insert("instructions".into(), json!(text));
            }
        }

        let input = encode_input_items(&chat_msgs, loss);
        body.insert("input".into(), Value::Array(input));

        if ir.stream {
            body.insert("stream".into(), json!(true));
        }
        if let Some(t) = ir.sampling.temperature {
            body.insert("temperature".into(), json!(t));
        }
        if let Some(p) = ir.sampling.top_p {
            body.insert("top_p".into(), json!(p));
        }
        if let Some(m) = ir.max_tokens {
            body.insert("max_output_tokens".into(), json!(m));
        }
        if !ir.tool_choice.is_null() {
            body.insert("tool_choice".into(), ir.tool_choice.clone());
        }

        // tools: prefer original raw when present; else rebuild.
        let tools_value = match ir.raw.get("tools").cloned() {
            Some(v) => Some(v),
            None if !ir.tools.is_empty() => Some(encode_tools(&ir.tools)),
            None => None,
        };
        if let Some(v) = tools_value {
            body.insert("tools".into(), v);
        }

        // reasoning effort → Responses reasoning object (best-effort).
        if let Some(effort) = ir.reasoning.effort.as_deref()
            && effort != "none"
            && !ir.raw.contains_key("reasoning")
        {
            body.insert("reasoning".into(), json!({"effort": effort}));
        }

        // Unknown raw fields round-trip (do not overwrite known keys).
        for (k, v) in &ir.raw {
            if k == "tools" {
                continue; // consumed above
            }
            body.entry(k.clone()).or_insert_with(|| v.clone());
        }

        Value::Object(body)
    }

    // ═══════════════════════════════════════════════════════════
    // response decode (upstream): Responses body → IrResponse
    // ═══════════════════════════════════════════════════════════

    /// Decode a non-stream Responses body into IR.
    pub fn decode_response(body: &Value) -> IrResponse {
        let mut loss = sag_core::LossInventory::default();
        let mut raw = Map::new();
        let mut output = Vec::new();

        let id = body
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let model = body
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let created = body.get("created_at").and_then(|v| v.as_u64());

        // Unknown top-level fields → raw (passthrough, no loss).
        const KNOWN: &[&str] = &[
            "id",
            "object",
            "created_at",
            "status",
            "error",
            "incomplete_details",
            "instructions",
            "max_output_tokens",
            "model",
            "output",
            "parallel_tool_calls",
            "previous_response_id",
            "store",
            "temperature",
            "text",
            "tool_choice",
            "tools",
            "top_p",
            "truncation",
            "usage",
            "user",
            "metadata",
        ];
        if let Some(obj) = body.as_object() {
            for (k, v) in obj {
                if !KNOWN.contains(&k.as_str()) {
                    raw.insert(k.clone(), v.clone());
                }
            }
        }
        // Preserve status/incomplete for finish_reason encode round-trip.
        if let Some(s) = body.get("status") {
            raw.insert("__status".into(), s.clone());
        }
        if let Some(s) = body.get("incomplete_details") {
            raw.insert("__incomplete_details".into(), s.clone());
        }

        let mut has_function_call = false;
        if let Some(arr) = body.get("output").and_then(|v| v.as_array()) {
            for (i, item) in arr.iter().enumerate() {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("message") => {
                        // content parts → Text (annotations preserved in raw item skip).
                        if let Some(parts) = item.get("content").and_then(|c| c.as_array()) {
                            for (pi, part) in parts.iter().enumerate() {
                                match part.get("type").and_then(|t| t.as_str()) {
                                    Some("output_text") | Some("text") => {
                                        // Non-empty annotations cannot live on
                                        // IrOutputItem::Text — record loss.
                                        if let Some(anns) = part
                                            .get("annotations")
                                            .and_then(|a| a.as_array())
                                            .filter(|a| !a.is_empty())
                                        {
                                            loss.push_loss(
                                                LossScope::Response,
                                                format!("output[{i}].content[{pi}].annotations"),
                                                LossReason::UnsupportedByIr,
                                                Value::Array(anns.clone()),
                                            );
                                        }
                                        output.push(IrOutputItem::Text {
                                            text: part
                                                .get("text")
                                                .and_then(|t| t.as_str())
                                                .unwrap_or("")
                                                .to_string(),
                                        });
                                    }
                                    _ => {
                                        loss.push_loss(
                                            LossScope::Response,
                                            format!("output[{i}].content[{pi}]"),
                                            LossReason::UnsupportedByIr,
                                            part.clone(),
                                        );
                                        output.push(IrOutputItem::Raw(part.clone()));
                                    }
                                }
                            }
                        } else if let Some(t) = item.get("content").and_then(|c| c.as_str()) {
                            output.push(IrOutputItem::Text {
                                text: t.to_string(),
                            });
                        }
                    }
                    Some("function_call") => {
                        has_function_call = true;
                        let arguments = match item.get("arguments") {
                            Some(Value::String(s)) => serde_json::from_str::<Value>(s)
                                .ok()
                                .filter(|v| !v.is_string())
                                .unwrap_or_else(|| Value::String(s.clone())),
                            Some(other) => other.clone(),
                            None => json!({}),
                        };
                        output.push(IrOutputItem::ToolCall(IrToolCall {
                            id: item
                                .get("call_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            name: item
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            arguments,
                            index: Some(output.len() as u32),
                        }));
                    }
                    Some("reasoning") => {
                        // summary / content text → Reasoning.
                        let mut text = String::new();
                        for key in ["summary", "content"] {
                            if let Some(parts) = item.get(key).and_then(|p| p.as_array()) {
                                for p in parts {
                                    if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                                        text.push_str(t);
                                    }
                                }
                            }
                        }
                        output.push(IrOutputItem::Reasoning { text });
                    }
                    _ => {
                        // Unknown (web_search_call, …) → Raw + loss.
                        loss.push_loss(
                            LossScope::Response,
                            format!("output[{i}]"),
                            LossReason::UnsupportedByIr,
                            item.clone(),
                        );
                        output.push(IrOutputItem::Raw(item.clone()));
                    }
                }
            }
        }

        let status = body
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("completed");
        let incomplete_reason = body
            .pointer("/incomplete_details/reason")
            .and_then(|v| v.as_str());
        let finish_reason = status_to_finish(status, incomplete_reason, has_function_call);

        let usage = body
            .get("usage")
            .filter(|u| !u.is_null())
            .map(responses_usage);

        IrResponse {
            id,
            created,
            model,
            output,
            finish_reason,
            usage,
            raw,
            loss,
        }
    }

    // ═══════════════════════════════════════════════════════════
    // response encode (ingress): IrResponse → Responses body
    // ═══════════════════════════════════════════════════════════

    /// Encode IR → Responses non-stream response (`object:"response"`).
    pub fn encode_response(ir: &IrResponse) -> Value {
        let mut body = Map::new();
        body.insert("id".into(), json!(ir.id));
        body.insert("object".into(), json!("response"));
        if let Some(c) = ir.created {
            body.insert("created_at".into(), json!(c));
        }

        // status: prefer raw (round-trip) else derive from finish_reason.
        let status = match ir.raw.get("__status").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => match ir.finish_reason.as_ref() {
                Some(IrFinishReason::Length | IrFinishReason::ContentFilter) => "incomplete".into(),
                Some(IrFinishReason::Other(s)) if s == "failed" => "failed".into(),
                Some(IrFinishReason::Cancelled) => "cancelled".into(),
                _ => "completed".into(),
            },
        };
        body.insert("status".into(), json!(status));
        if let Some(inc) = ir.raw.get("__incomplete_details") {
            body.insert("incomplete_details".into(), inc.clone());
        } else if status == "incomplete" {
            let reason = match ir.finish_reason.as_ref() {
                Some(IrFinishReason::Length) => "max_output_tokens",
                Some(IrFinishReason::ContentFilter) => "content_filter",
                _ => "max_output_tokens",
            };
            body.insert("incomplete_details".into(), json!({"reason": reason}));
        }

        body.insert("model".into(), json!(ir.model));

        let mut output: Vec<Value> = Vec::new();
        for item in &ir.output {
            match item {
                IrOutputItem::Text { text } => {
                    output.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "id": format!("msg_{}", ir.id),
                        "status": "completed",
                        "content": [{
                            "type": "output_text",
                            "text": text,
                            "annotations": [],
                        }],
                    }));
                }
                IrOutputItem::Reasoning { text } => {
                    if text.is_empty() {
                        continue;
                    }
                    output.push(json!({
                        "type": "reasoning",
                        "id": format!("rs_{}", ir.id),
                        "summary": [{
                            "type": "summary_text",
                            "text": text,
                        }],
                    }));
                }
                IrOutputItem::ToolCall(tc) => {
                    let arguments = match &tc.arguments {
                        Value::String(s) => s.clone(),
                        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".into()),
                    };
                    output.push(json!({
                        "type": "function_call",
                        "id": format!("fc_{}", tc.id),
                        "call_id": tc.id,
                        "name": tc.name,
                        "arguments": arguments,
                        "status": "completed",
                    }));
                }
                IrOutputItem::Raw(v) => output.push(v.clone()),
            }
        }
        body.insert("output".into(), Value::Array(output));

        if let Some(u) = &ir.usage {
            body.insert("usage".into(), usage_to_responses(u));
        }

        // Unknown raw fields round-trip (skip codec-private keys).
        for (k, v) in &ir.raw {
            if k.starts_with("__") {
                continue;
            }
            body.entry(k.clone()).or_insert_with(|| v.clone());
        }

        Value::Object(body)
    }

    // ═══════════════════════════════════════════════════════════
    // stream decode (upstream): Responses SSE → IrStreamEvent(s)
    // ═══════════════════════════════════════════════════════════

    /// Decode a single Responses SSE frame into 0..n IR stream events.
    ///
    /// State machine (plan §5.2):
    /// `response.created → response.in_progress → response.output_item.added
    ///  → response.content_part.added → delta → done → response.completed /
    ///  response.failed`; unknown types → Raw.
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
                // Responses never uses [DONE]; tolerate upstream Chat bridges.
                events.push(IrStreamEvent::End);
                continue;
            }

            let Ok(val) = serde_json::from_str::<Value>(json_str) else {
                events.push(IrStreamEvent::Raw(frame.to_vec()));
                continue;
            };

            // Top-level error object.
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

            let Some(event_type) = val.get("type").and_then(|t| t.as_str()) else {
                events.push(IrStreamEvent::Raw(frame.to_vec()));
                continue;
            };

            match event_type {
                "response.created" | "response.in_progress" => {
                    if event_type == "response.created" {
                        let resp = val.get("response").cloned().unwrap_or(Value::Null);
                        events.push(IrStreamEvent::Start(IrResponseMeta {
                            id: resp.get("id").and_then(|v| v.as_str()).map(String::from),
                            created: resp.get("created_at").and_then(|v| v.as_u64()),
                            model: resp.get("model").and_then(|v| v.as_str()).map(String::from),
                        }));
                    }
                    // in_progress is pure bookkeeping — no IR event.
                }
                "response.output_item.added" => {
                    let item = val.get("item").cloned().unwrap_or(Value::Null);
                    let index = val
                        .get("output_index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    match item.get("type").and_then(|t| t.as_str()) {
                        Some("function_call") => {
                            events.push(IrStreamEvent::ToolCallStart {
                                index,
                                id: item
                                    .get("call_id")
                                    .or_else(|| item.get("id"))
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                                name: item.get("name").and_then(|v| v.as_str()).map(String::from),
                            });
                            // Initial arguments (often "") → ToolCallDelta.
                            if let Some(args) = item.get("arguments").and_then(|v| v.as_str())
                                && !args.is_empty()
                            {
                                events.push(IrStreamEvent::ToolCallDelta {
                                    index,
                                    arguments_delta: args.to_string(),
                                });
                            }
                        }
                        Some("message") | Some("reasoning") => {
                            // Item open — content deltas follow. Nothing to emit.
                        }
                        _ => {
                            events.push(IrStreamEvent::Raw(frame.to_vec()));
                        }
                    }
                }
                "response.output_item.done"
                | "response.content_part.added"
                | "response.content_part.done" => {
                    // Bookkeeping frames — no IR event (block re-opens on next delta).
                }
                "response.output_text.delta" | "response.content_part.delta" => {
                    let index = val
                        .get("output_index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    // content_part.delta nests the part; output_text.delta is flat.
                    let text: String;
                    let is_reasoning: bool;
                    if event_type == "response.output_text.delta" {
                        text = val
                            .get("delta")
                            .and_then(|d| d.as_str())
                            .or_else(|| val.get("text").and_then(|t| t.as_str()))
                            .unwrap_or("")
                            .to_string();
                        is_reasoning = false;
                    } else {
                        let delta = val.get("delta").cloned().unwrap_or(Value::Null);
                        let ty = delta
                            .get("type")
                            .and_then(|t| t.as_str())
                            .unwrap_or("output_text")
                            .to_string();
                        text = delta
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        is_reasoning = matches!(
                            ty.as_str(),
                            "summary_text" | "summary_delta" | "reasoning_text"
                        );
                    }
                    if !text.is_empty() {
                        if is_reasoning {
                            events.push(IrStreamEvent::ReasoningDelta { index, text });
                        } else {
                            events.push(IrStreamEvent::TextDelta { index, text });
                        }
                    }
                }
                "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                    let index = val
                        .get("output_index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    let text = val.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                    if !text.is_empty() {
                        events.push(IrStreamEvent::ReasoningDelta {
                            index,
                            text: text.to_string(),
                        });
                    }
                }
                "response.function_call_arguments.delta" => {
                    let index = val
                        .get("output_index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    let delta = val.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                    if !delta.is_empty() {
                        events.push(IrStreamEvent::ToolCallDelta {
                            index,
                            arguments_delta: delta.to_string(),
                        });
                    }
                }
                "response.completed" => {
                    // Forward the original frame (client-visible, carries
                    // usage) + extract Usage for analytics. Finish/End are
                    // NOT emitted here — native Data→Raw path never decodes,
                    // and the translated path relies on the upstream
                    // finish_reason / Done. Raw first so the client always
                    // receives the authoritative completed frame.
                    events.push(IrStreamEvent::Raw(frame.to_vec()));
                    if let Some(u) = val.pointer("/response/usage").filter(|u| !u.is_null()) {
                        events.push(IrStreamEvent::Usage(responses_usage(u)));
                    }
                }
                "response.failed" => {
                    let msg = val
                        .pointer("/response/error/message")
                        .and_then(|m| m.as_str())
                        .or_else(|| {
                            val.get("error")
                                .and_then(|e| e.get("message"))
                                .and_then(|m| m.as_str())
                        })
                        .unwrap_or("upstream response failed")
                        .to_string();
                    events.push(IrStreamEvent::Error(msg));
                    events.push(IrStreamEvent::Raw(frame.to_vec()));
                }
                "response.incomplete" => {
                    // Incomplete is a normal terminal (token limit / filter),
                    // not an error — map to Finish(Length/Other) so the
                    // stream does not post-commit an Error (S3).
                    let reason = val
                        .pointer("/response/incomplete_details/reason")
                        .and_then(|r| r.as_str());
                    let finish_reason = match reason {
                        Some("max_output_tokens") => IrFinishReason::Length,
                        Some("content_filter") => IrFinishReason::ContentFilter,
                        Some(other) => IrFinishReason::Other(other.to_string()),
                        None => IrFinishReason::Other("incomplete".into()),
                    };
                    events.push(IrStreamEvent::Finish {
                        index: 0,
                        finish_reason: Some(finish_reason),
                    });
                    if let Some(u) = val.pointer("/response/usage").filter(|u| !u.is_null()) {
                        events.push(IrStreamEvent::Usage(responses_usage(u)));
                    }
                    events.push(IrStreamEvent::Raw(frame.to_vec()));
                }
                _ => {
                    events.push(IrStreamEvent::Raw(frame.to_vec()));
                }
            }
        }

        if !saw_data && events.is_empty() && !text.trim().is_empty() {
            events.push(IrStreamEvent::Raw(frame.to_vec()));
        }

        events
    }

    // ═══════════════════════════════════════════════════════════
    // stream encode (ingress): IrStreamEvent → Responses SSE
    // ═══════════════════════════════════════════════════════════

    /// Encode one IR stream event as Responses SSE bytes (stateless view).
    ///
    /// Prefer [`Self::stream_encoder`] for ingress egress — it tracks the
    /// output-item / content-part lifecycle so `response.output_item.added`
    /// / `response.content_part.*` frames are emitted correctly. This fn
    /// handles Raw passthrough and simple one-shot events.
    pub fn encode_stream_event(ev: &IrStreamEvent) -> Vec<u8> {
        match ev {
            IrStreamEvent::Raw(bytes) => {
                // Suppress OpenAI `[DONE]` — Responses ends with response.completed.
                let text = String::from_utf8_lossy(bytes);
                if text.contains("[DONE]") {
                    return Vec::new();
                }
                bytes.clone()
            }
            IrStreamEvent::Start(meta) => {
                let resp = json!({
                    "id": meta.id.clone().unwrap_or_default(),
                    "object": "response",
                    "created_at": meta.created.unwrap_or(0),
                    "status": "in_progress",
                    "model": meta.model.clone().unwrap_or_default(),
                    "output": [],
                });
                let payload = json!({"type": "response.created", "response": resp});
                format!("data: {payload}\n\n").into_bytes()
            }
            IrStreamEvent::TextDelta { text, .. } => {
                let payload = json!({
                    "type": "response.content_part.delta",
                    "output_index": 0,
                    "content_index": 0,
                    "delta": {"type": "output_text", "text": text},
                });
                format!("data: {payload}\n\n").into_bytes()
            }
            IrStreamEvent::ReasoningDelta { text, .. } => {
                let payload = json!({
                    "type": "response.reasoning_summary_text.delta",
                    "output_index": 0,
                    "item_id": "",
                    "delta": text,
                });
                format!("data: {payload}\n\n").into_bytes()
            }
            IrStreamEvent::ToolCallStart { index, id, name } => {
                let item = json!({
                    "type": "function_call",
                    "id": id.clone().unwrap_or_default(),
                    "call_id": id.clone().unwrap_or_default(),
                    "name": name.clone().unwrap_or_default(),
                    "arguments": "",
                    "status": "in_progress",
                });
                let payload = json!({
                    "type": "response.output_item.added",
                    "output_index": index,
                    "item": item,
                });
                format!("data: {payload}\n\n").into_bytes()
            }
            IrStreamEvent::ToolCallDelta {
                index,
                arguments_delta,
            } => {
                let payload = json!({
                    "type": "response.function_call_arguments.delta",
                    "output_index": index,
                    "delta": arguments_delta,
                });
                format!("data: {payload}\n\n").into_bytes()
            }
            IrStreamEvent::Finish { finish_reason, .. } => {
                // Synthesize response.completed (translated path; native
                // passes the upstream frame through as Raw).
                let status = match finish_reason {
                    Some(IrFinishReason::Length | IrFinishReason::ContentFilter) => "incomplete",
                    Some(IrFinishReason::Cancelled) => "cancelled",
                    Some(IrFinishReason::Other(s)) if s == "failed" => "failed",
                    _ => "completed",
                };
                let payload = json!({
                    "type": "response.completed",
                    "response": {"object": "response", "status": status, "output": []},
                });
                format!("data: {payload}\n\n").into_bytes()
            }
            // Usage / Error / End are handled inline by the dispatch loop.
            IrStreamEvent::Usage(_) | IrStreamEvent::Error(_) | IrStreamEvent::End => Vec::new(),
        }
    }

    /// Build a **stateful** ingress stream encoder that tracks the open
    /// output item / content part so Responses' `output_item.added` /
    /// `content_part.*` lifecycle is emitted correctly.
    ///
    /// Returns a `Fn` suitable for `sag_dispatch::build_streaming_response`.
    pub fn stream_encoder() -> impl Fn(&IrStreamEvent) -> Vec<u8> + Send + 'static {
        let state = RefCell::new(StreamEncodeState::default());
        move |ev| {
            let mut st = state.borrow_mut();
            st.encode(ev)
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
// stream encoder state
// ═══════════════════════════════════════════════════════════════════

#[derive(Default)]
struct StreamEncodeState {
    started: bool,
    /// Kind of the currently open output item, if any.
    open_item: Option<OpenItem>,
    /// Index of the currently open output item.
    open_index: u32,
    next_index: u32,
    completed: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum OpenItem {
    Message,
    Reasoning,
    FunctionCall,
}

impl StreamEncodeState {
    fn encode(&mut self, ev: &IrStreamEvent) -> Vec<u8> {
        match ev {
            IrStreamEvent::Raw(bytes) => {
                let text = String::from_utf8_lossy(bytes);
                if text.contains("[DONE]") {
                    return Vec::new();
                }
                // Tight completed detection: match the frame `type` field,
                // not a substring of the whole payload (N2).
                let is_completed = text.lines().any(|line| {
                    line.trim()
                        .strip_prefix("data:")
                        .map(str::trim_start)
                        .and_then(|rest| serde_json::from_str::<Value>(rest).ok())
                        .and_then(|v| {
                            v.get("type")
                                .and_then(|t| t.as_str())
                                .map(|t| t == "response.completed")
                        })
                        .unwrap_or(false)
                });
                if is_completed {
                    self.completed = true;
                }
                bytes.clone()
            }
            IrStreamEvent::Start(meta) => {
                self.started = true;
                self.open_item = None;
                self.next_index = 0;
                self.completed = false;
                let _ = meta;
                ResponsesCodec::encode_stream_event(ev)
            }
            IrStreamEvent::TextDelta { text, .. } => {
                let mut out = self.ensure_started();
                if self.open_item != Some(OpenItem::Message) {
                    out.extend_from_slice(&self.close_open_item());
                    let index = self.next_index;
                    // output_item.added(message)
                    let item = json!({
                        "type": "message",
                        "id": format!("msg_{index}"),
                        "role": "assistant",
                        "status": "in_progress",
                        "content": [],
                    });
                    let payload = json!({
                        "type": "response.output_item.added",
                        "output_index": index,
                        "item": item,
                    });
                    out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                    // content_part.added(output_text)
                    let payload = json!({
                        "type": "response.content_part.added",
                        "output_index": index,
                        "content_index": 0,
                        "part": {"type": "output_text", "text": "", "annotations": []},
                    });
                    out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                    self.open_item = Some(OpenItem::Message);
                    self.open_index = index;
                    self.next_index += 1;
                }
                let payload = json!({
                    "type": "response.output_text.delta",
                    "item_id": format!("msg_{}", self.open_index),
                    "output_index": self.open_index,
                    "content_index": 0,
                    "delta": text,
                });
                out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                out
            }
            IrStreamEvent::ReasoningDelta { text, .. } => {
                let mut out = self.ensure_started();
                if self.open_item != Some(OpenItem::Reasoning) {
                    out.extend_from_slice(&self.close_open_item());
                    let index = self.next_index;
                    let item = json!({
                        "type": "reasoning",
                        "id": format!("rs_{index}"),
                        "summary": [],
                        "status": "in_progress",
                    });
                    let payload = json!({
                        "type": "response.output_item.added",
                        "output_index": index,
                        "item": item,
                    });
                    out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                    self.open_item = Some(OpenItem::Reasoning);
                    self.open_index = index;
                    self.next_index += 1;
                }
                let payload = json!({
                    "type": "response.reasoning_summary_text.delta",
                    "item_id": format!("rs_{}", self.open_index),
                    "output_index": self.open_index,
                    "delta": text,
                });
                out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                out
            }
            IrStreamEvent::ToolCallStart { id, name, .. } => {
                let mut out = self.ensure_started();
                out.extend_from_slice(&self.close_open_item());
                let index = self.next_index;
                let call_id = id.clone().unwrap_or_default();
                let item = json!({
                    "type": "function_call",
                    "id": format!("fc_{index}"),
                    "call_id": call_id,
                    "name": name.clone().unwrap_or_default(),
                    "arguments": "",
                    "status": "in_progress",
                });
                let payload = json!({
                    "type": "response.output_item.added",
                    "output_index": index,
                    "item": item,
                });
                out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                self.open_item = Some(OpenItem::FunctionCall);
                self.open_index = index;
                self.next_index += 1;
                out
            }
            IrStreamEvent::ToolCallDelta {
                arguments_delta, ..
            } => {
                // Defensive: open a function_call item if upstream skipped start.
                if self.open_item != Some(OpenItem::FunctionCall) {
                    let mut out = self.encode(&IrStreamEvent::ToolCallStart {
                        index: 0,
                        id: None,
                        name: None,
                    });
                    let payload = json!({
                        "type": "response.function_call_arguments.delta",
                        "output_index": self.open_index,
                        "delta": arguments_delta,
                    });
                    out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                    return out;
                }
                let payload = json!({
                    "type": "response.function_call_arguments.delta",
                    "output_index": self.open_index,
                    "delta": arguments_delta,
                });
                format!("data: {payload}\n\n").into_bytes()
            }
            IrStreamEvent::Finish { finish_reason, .. } => {
                let mut out = self.close_open_item();
                if !self.completed {
                    out.extend_from_slice(&encode_completed(finish_reason.as_ref()));
                    self.completed = true;
                }
                out
            }
            IrStreamEvent::Usage(_) | IrStreamEvent::Error(_) | IrStreamEvent::End => Vec::new(),
        }
    }

    fn ensure_started(&mut self) -> Vec<u8> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        ResponsesCodec::encode_stream_event(&IrStreamEvent::Start(IrResponseMeta::default()))
    }

    fn close_open_item(&mut self) -> Vec<u8> {
        let Some(kind) = self.open_item.take() else {
            return Vec::new();
        };
        let index = self.open_index;
        let mut out = Vec::new();
        match kind {
            OpenItem::Message => {
                let payload = json!({
                    "type": "response.content_part.done",
                    "output_index": index,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": "", "annotations": []},
                });
                out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                let payload = json!({
                    "type": "response.output_item.done",
                    "output_index": index,
                    "item": {
                        "type": "message",
                        "role": "assistant",
                        "status": "completed",
                        "content": [],
                    },
                });
                out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
            }
            OpenItem::Reasoning | OpenItem::FunctionCall => {
                let ty = if kind == OpenItem::Reasoning {
                    "reasoning"
                } else {
                    "function_call"
                };
                let payload = json!({
                    "type": "response.output_item.done",
                    "output_index": index,
                    "item": {"type": ty, "status": "completed"},
                });
                out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
            }
        }
        out
    }
}

fn encode_completed(finish: Option<&IrFinishReason>) -> Vec<u8> {
    let status = match finish {
        Some(IrFinishReason::Length | IrFinishReason::ContentFilter) => "incomplete",
        Some(IrFinishReason::Cancelled) => "cancelled",
        Some(IrFinishReason::Other(s)) if s == "failed" => "failed",
        _ => "completed",
    };
    let payload = json!({
        "type": "response.completed",
        "response": {"object": "response", "status": status, "output": []},
    });
    format!("data: {payload}\n\n").into_bytes()
}

// ═══════════════════════════════════════════════════════════════════
// request helpers
// ═══════════════════════════════════════════════════════════════════

fn decode_input_item(
    item: &Value,
    ii: usize,
    messages: &mut Vec<IrMessage>,
    loss: &mut sag_core::LossInventory,
) {
    match item.get("type").and_then(|t| t.as_str()) {
        Some("message") | None => {
            // Bare object with role is also a message.
            if item.get("type").is_none() && item.get("role").is_none() {
                loss.push_loss(
                    LossScope::Message,
                    format!("input[{ii}]"),
                    LossReason::UnsupportedByIr,
                    item.clone(),
                );
                return;
            }
            let role =
                IrRole::from_raw(item.get("role").and_then(|v| v.as_str()).unwrap_or("user"));
            let mut raw = Map::new();
            if let Some(obj) = item.as_object() {
                for (k, v) in obj {
                    if !matches!(k.as_str(), "type" | "role" | "content") {
                        raw.insert(k.clone(), v.clone());
                    }
                }
            }
            let content = match item.get("content") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::String(s)) => vec![IrContentBlock::Text { text: s.clone() }],
                Some(Value::Array(parts)) => parts
                    .iter()
                    .enumerate()
                    .map(|(pi, p)| {
                        decode_content_part(p, &format!("input[{ii}].content[{pi}]"), loss)
                    })
                    .collect(),
                Some(other) => {
                    loss.push_loss(
                        LossScope::ContentBlock,
                        format!("input[{ii}].content"),
                        LossReason::UnsupportedByIr,
                        other.clone(),
                    );
                    vec![IrContentBlock::Raw(other.clone())]
                }
            };
            messages.push(IrMessage {
                role,
                content,
                name: None,
                tool_call_id: None,
                tool_calls: Vec::new(),
                raw,
            });
        }
        Some("function_call_output") => {
            let call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let output_text = match item.get("output") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => {
                    let mut text = String::new();
                    for (pi, p) in parts.iter().enumerate() {
                        if let Some(t) = p
                            .get("text")
                            .and_then(|t| t.as_str())
                            .or_else(|| p.as_str())
                        {
                            text.push_str(t);
                        } else {
                            // Non-text part discarded → loss (S2).
                            loss.push_loss(
                                LossScope::ContentBlock,
                                format!("input[{ii}].output[{pi}]"),
                                LossReason::UnsupportedByIr,
                                p.clone(),
                            );
                        }
                    }
                    text
                }
                Some(other) => serde_json::to_string(other).unwrap_or_default(),
                None => String::new(),
            };
            let mut raw = Map::new();
            if let Some(obj) = item.as_object() {
                for (k, v) in obj {
                    if !matches!(k.as_str(), "type" | "call_id" | "output") {
                        raw.insert(k.clone(), v.clone());
                    }
                }
            }
            messages.push(IrMessage {
                role: IrRole::Tool,
                content: vec![IrContentBlock::Text { text: output_text }],
                name: None,
                tool_call_id: Some(call_id),
                tool_calls: Vec::new(),
                raw,
            });
        }
        Some("function_call") => {
            let call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let arguments = match item.get("arguments") {
                Some(Value::String(s)) => serde_json::from_str::<Value>(s)
                    .ok()
                    .filter(|v| !v.is_string())
                    .unwrap_or_else(|| Value::String(s.clone())),
                Some(other) => other.clone(),
                None => json!({}),
            };
            let mut raw = Map::new();
            if let Some(obj) = item.as_object() {
                for (k, v) in obj {
                    if !matches!(k.as_str(), "type" | "call_id" | "name" | "arguments") {
                        raw.insert(k.clone(), v.clone());
                    }
                }
            }
            messages.push(IrMessage {
                role: IrRole::Assistant,
                content: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: vec![IrToolCall {
                    id: call_id,
                    name,
                    arguments,
                    index: Some(0),
                }],
                raw,
            });
        }
        Some("reasoning") => {
            let mut text = String::new();
            for key in ["summary", "content"] {
                if let Some(parts) = item.get(key).and_then(|p| p.as_array()) {
                    for p in parts {
                        if let Some(t) = p.get("text").and_then(|t| t.as_str()) {
                            text.push_str(t);
                        }
                    }
                }
            }
            messages.push(IrMessage {
                role: IrRole::Assistant,
                content: vec![IrContentBlock::Reasoning { text }],
                ..Default::default()
            });
        }
        Some(other) => {
            // Unknown item type → structural drop with loss (raw preserved
            // on the loss entry value snapshot).
            loss.push_loss(
                LossScope::Message,
                format!("input[{ii}]"),
                LossReason::UnsupportedByIr,
                item.clone(),
            );
            let _ = other;
        }
    }
}

fn encode_input_items(chat_msgs: &[&IrMessage], loss: &mut sag_core::LossInventory) -> Vec<Value> {
    let mut out = Vec::new();
    for (mi, msg) in chat_msgs.iter().enumerate() {
        match msg.role {
            IrRole::Tool => {
                let call_id = msg.tool_call_id.clone().unwrap_or_default();
                // Non-Text tool-result content cannot join a bare string → loss.
                let mut text = String::new();
                for (bi, b) in msg.content.iter().enumerate() {
                    match b {
                        IrContentBlock::Text { text: t } => text.push_str(t),
                        other => {
                            loss.push_loss(
                                LossScope::ContentBlock,
                                format!("messages[{mi}].content[{bi}]"),
                                LossReason::UnsupportedByIr,
                                serde_json::to_value(other).unwrap_or(Value::Null),
                            );
                        }
                    }
                }
                out.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": text,
                }));
            }
            IrRole::Assistant if !msg.tool_calls.is_empty() => {
                // Optional preceding text as a message item.
                let text: String = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        IrContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                if !text.is_empty() {
                    out.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    }));
                }
                for tc in &msg.tool_calls {
                    let arguments = match &tc.arguments {
                        Value::String(s) => s.clone(),
                        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".into()),
                    };
                    let mut item = Map::new();
                    item.insert("type".into(), json!("function_call"));
                    item.insert("call_id".into(), json!(tc.id));
                    item.insert("name".into(), json!(tc.name));
                    item.insert("arguments".into(), json!(arguments));
                    for (k, v) in &msg.raw {
                        item.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                    out.push(Value::Object(item));
                }
                // Reasoning blocks → reasoning item.
                for b in &msg.content {
                    if let IrContentBlock::Reasoning { text } = b {
                        out.push(json!({
                            "type": "reasoning",
                            "summary": [{"type": "summary_text", "text": text}],
                        }));
                    }
                }
            }
            IrRole::System => {
                // System messages are lifted to `instructions` by the caller;
                // any residual system message becomes a plain message.
                out.push(encode_message_item(msg, mi, loss));
            }
            _ => out.push(encode_message_item(msg, mi, loss)),
        }
    }
    out
}

fn encode_message_item(
    msg: &IrMessage,
    msg_idx: usize,
    loss: &mut sag_core::LossInventory,
) -> Value {
    let role = match msg.role {
        IrRole::Assistant => "assistant",
        IrRole::System => "system",
        IrRole::Developer => "developer",
        _ => "user",
    };
    let mut parts: Vec<Value> = Vec::new();
    // Text blocks are accumulated instead of pushed: a pure-text message
    // encodes as a scalar `content` string, and a mixed one re-emits the
    // accumulated text as the leading `input_text` part.
    let mut text_only = String::new();
    let mut has_non_text = false;
    for (bi, b) in msg.content.iter().enumerate() {
        match b {
            IrContentBlock::Text { text } => {
                text_only.push_str(text);
            }
            IrContentBlock::Reasoning { text } => {
                has_non_text = true;
                parts.push(json!({"type": "output_text", "text": text}));
            }
            IrContentBlock::Image { data } => {
                has_non_text = true;
                // Responses input_image shape pass-through when already typed;
                // Chat-shaped image_url → input_image.
                if data.get("type").and_then(|t| t.as_str()) == Some("input_image") {
                    parts.push(data.clone());
                } else if let Some(url) = data
                    .pointer("/image_url/url")
                    .and_then(|u| u.as_str())
                    .or_else(|| data.get("url").and_then(|u| u.as_str()))
                {
                    parts.push(json!({"type": "input_image", "image_url": url}));
                } else {
                    parts.push(data.clone());
                }
            }
            IrContentBlock::Document { data } => {
                has_non_text = true;
                if data.get("type").is_some() {
                    parts.push(data.clone());
                } else {
                    parts.push(json!({"type": "input_file", "file": data}));
                }
            }
            IrContentBlock::Audio { data } => {
                has_non_text = true;
                parts.push(data.clone());
            }
            IrContentBlock::Raw(v) => {
                has_non_text = true;
                parts.push(v.clone());
            }
            // ToolUse / ToolResult travel as function_call /
            // function_call_output on their own message roles — a bare
            // content copy here is dropped → loss (S1).
            IrContentBlock::ToolUse { .. } | IrContentBlock::ToolResult { .. } => {
                loss.push_loss(
                    LossScope::ContentBlock,
                    format!("messages[{msg_idx}].content[{bi}]"),
                    LossReason::UnsupportedByIr,
                    serde_json::to_value(b).unwrap_or(Value::Null),
                );
            }
        }
    }
    let mut item = Map::new();
    item.insert("type".into(), json!("message"));
    item.insert("role".into(), json!(role));
    if !has_non_text {
        // Scalar string (empty string is legal for EasyInputMessage;
        // InputMessageContentList is "one or many" so `[]` is not).
        item.insert("content".into(), json!(text_only));
    } else {
        // Mixed media: InputMessageContentList. Always include an input_text
        // part so the list carries a `text` field.
        parts.insert(0, json!({"type": "input_text", "text": text_only}));
        item.insert("content".into(), Value::Array(parts));
    }
    for (k, v) in &msg.raw {
        if !matches!(k.as_str(), "type" | "role" | "content") {
            item.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    Value::Object(item)
}

fn encode_tools(tools: &[IrTool]) -> Value {
    let arr: Vec<Value> = tools
        .iter()
        .map(|t| {
            // M8 hosted web_search: emit Responses-hosted shape from raw
            // (`{"type":"web_search", ...}` or `web_search_preview*`)
            // rather than function shape. `raw.type` is read through the
            // dual-convention accessor so chat-sourced IR is tolerated too.
            let raw_ty = tool_raw::tool_raw_type(t).unwrap_or("");
            if raw_ty.starts_with("web_search") {
                let mut tool = Map::new();
                tool.insert("type".into(), json!(raw_ty));
                tool.insert("name".into(), json!(t.name));
                if let Some(d) = &t.description {
                    tool.insert("description".into(), json!(d));
                }
                // Remaining raw keys pass through verbatim (`name` and the
                // chat-only reserved markers excepted).
                tool_raw::merge_hosted_extras(&mut tool, t, &["name"]);
                return Value::Object(tool);
            }
            let mut tool = Map::new();
            tool.insert("name".into(), json!(t.name));
            if let Some(d) = &t.description {
                tool.insert("description".into(), json!(d));
            }
            if !t.input_schema.is_null() {
                tool.insert("parameters".into(), t.input_schema.clone());
            }
            // Responses function tools are flat: tool-level and function-level
            // extras (incl. `strict`) both return to the root. Chat's reserved
            // `__tool_extra` / `__function_extra` markers are never emitted.
            tool_raw::merge_extras(&mut tool, t);
            // Original `type` wins; otherwise keep the Responses default.
            tool.entry("type").or_insert_with(|| json!("function"));
            Value::Object(tool)
        })
        .collect();
    Value::Array(arr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(body: Value) -> IrRequest {
        ResponsesCodec::decode_request(&body).expect("valid responses request")
    }

    // ── request round-trips ─────────────────────────────────────

    #[test]
    fn request_roundtrip_instructions_input_tools() {
        let body = json!({
            "model": "gpt-4o",
            "instructions": "Be brief.",
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "weather in SF?"}
                ]}
            ],
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "description": "Get weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }],
            "temperature": 0.2,
            "max_output_tokens": 256,
            "stream": false
        });
        let ir = decode(body.clone());
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert_eq!(ir.model, "gpt-4o");
        assert_eq!(ir.messages[0].role, IrRole::System);
        assert_eq!(ir.messages[1].role, IrRole::User);
        assert_eq!(ir.max_tokens, Some(256));
        assert_eq!(ir.sampling.temperature, Some(0.2));
        assert_eq!(ir.tools.len(), 1);
        assert_eq!(ir.tools[0].name, "get_weather");

        let back = ResponsesCodec::encode_request(&ir);
        assert_eq!(back["model"], "gpt-4o");
        assert_eq!(back["instructions"], "Be brief.");
        assert_eq!(back["max_output_tokens"], 256);
        assert_eq!(back["input"][0]["type"], "message");
        assert_eq!(back["input"][0]["role"], "user");
        assert_eq!(back["tools"][0]["type"], "function");
        assert_eq!(back["tools"][0]["name"], "get_weather");
        assert_eq!(back["tools"][0]["parameters"]["type"], "object");
    }

    #[test]
    fn request_function_call_and_output_roundtrip() {
        let body = json!({
            "model": "gpt-4o",
            "input": [
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "weather?"}]},
                {"type": "function_call", "call_id": "call_1", "name": "get_weather",
                 "arguments": "{\"city\":\"SF\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "72F"}
            ],
            "tools": [{"type": "function", "name": "get_weather", "parameters": {"type": "object"}}]
        });
        let ir = decode(body);
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert_eq!(ir.messages.len(), 3);
        // function_call → assistant tool_calls (id = call_id)
        assert_eq!(ir.messages[1].role, IrRole::Assistant);
        assert_eq!(ir.messages[1].tool_calls[0].id, "call_1");
        assert_eq!(ir.messages[1].tool_calls[0].name, "get_weather");
        // function_call_output → tool result message
        assert_eq!(ir.messages[2].role, IrRole::Tool);
        assert_eq!(ir.messages[2].tool_call_id.as_deref(), Some("call_1"));

        let back = ResponsesCodec::encode_request(&ir);
        let input = back["input"].as_array().expect("input array");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[1]["arguments"], "{\"city\":\"SF\"}");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["output"], "72F");
    }

    #[test]
    fn request_string_input_becomes_user_message() {
        let ir = decode(json!({"model": "m", "input": "hello"}));
        assert_eq!(ir.messages.len(), 1);
        assert_eq!(ir.messages[0].role, IrRole::User);
        assert!(
            matches!(&ir.messages[0].content[0], IrContentBlock::Text { text } if text == "hello")
        );
    }

    #[test]
    fn request_unknown_fields_go_to_raw() {
        let ir = decode(json!({
            "model": "m",
            "input": [],
            "prompt_cache_key": "pk-1",
            "store": false
        }));
        assert_eq!(ir.raw.get("prompt_cache_key"), Some(&json!("pk-1")));
        assert_eq!(ir.raw.get("store"), Some(&json!(false)));
        let back = ResponsesCodec::encode_request(&ir);
        assert_eq!(back["prompt_cache_key"], "pk-1");
        assert_eq!(back["store"], false);
    }

    #[test]
    fn request_parallel_tool_calls_and_reasoning_raw() {
        let ir = decode(json!({
            "model": "m",
            "input": [],
            "parallel_tool_calls": true,
            "reasoning": {"effort": "high"}
        }));
        assert_eq!(ir.raw.get("parallel_tool_calls"), Some(&json!(true)));
        assert_eq!(ir.reasoning.effort.as_deref(), Some("high"));
        let back = ResponsesCodec::encode_request(&ir);
        assert_eq!(back["parallel_tool_calls"], true);
        // effort re-encoded when not in raw... reasoning is in raw, restored.
        assert_eq!(back["reasoning"]["effort"], "high");
    }

    // ── D1 / 422 rejects ────────────────────────────────────────

    #[test]
    fn reject_previous_response_id() {
        let err = ResponsesCodec::decode_request(&json!({
            "model": "m",
            "previous_response_id": "resp_abc",
            "input": "hi"
        }))
        .unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
        assert!(
            err.to_string()
                .contains("previous_response_id is not supported")
        );
    }

    #[test]
    fn reject_store_true() {
        let err = ResponsesCodec::decode_request(&json!({
            "model": "m",
            "store": true,
            "input": "hi"
        }))
        .unwrap_err();
        assert!(err.to_string().contains("store=true is not supported"));
    }

    #[test]
    fn allow_store_false_and_absent() {
        for body in [
            json!({"model": "m", "store": false, "input": "hi"}),
            json!({"model": "m", "input": "hi"}),
        ] {
            ResponsesCodec::decode_request(&body).expect("store=false/absent must pass");
        }
    }

    #[test]
    fn reject_background() {
        let err = ResponsesCodec::decode_request(&json!({
            "model": "m",
            "background": true,
            "input": "hi"
        }))
        .unwrap_err();
        assert!(err.to_string().contains("background is not supported"));
        // background: false is tolerated (explicit non-background).
        ResponsesCodec::decode_request(&json!({
            "model": "m",
            "background": false,
            "input": "hi"
        }))
        .expect("background=false ok");
    }

    #[test]
    fn reject_hosted_tool_types() {
        for ty in ["hosted", "custom", "file_search"] {
            let err = ResponsesCodec::decode_request(&json!({
                "model": "m",
                "input": [],
                "tools": [{"type": ty, "name": "x"}]
            }))
            .unwrap_err();
            assert!(
                err.to_string().contains("is not supported"),
                "type={ty}: {err}"
            );
            assert!(
                err.to_string().contains(ty) || ty == "custom",
                "message should name the type: {err}"
            );
        }
    }

    #[test]
    fn allow_web_search_tool_type() {
        let ir = decode(json!({
            "model": "m",
            "input": [],
            "tools": [{"type": "web_search"}]
        }));
        assert_eq!(ir.tools.len(), 1);
        assert_eq!(ir.tools[0].name, "web_search");
        assert_eq!(
            ir.tools[0].raw.get("type").and_then(|v| v.as_str()),
            Some("web_search")
        );
        let back = ResponsesCodec::encode_request(&ir);
        assert_eq!(back["tools"][0]["type"], "web_search");
        assert_eq!(back["tools"][0]["name"], "web_search");
    }

    #[test]
    fn allow_web_search_preview_family() {
        // Nit: `web_search_preview*` is part of the web_search family.
        for ty in ["web_search_preview", "web_search_preview_20250305"] {
            let ir = decode(json!({
                "model": "m",
                "input": [],
                "tools": [{"type": ty, "name": "web_search"}]
            }));
            assert_eq!(ir.tools.len(), 1, "type {ty} must pass tool_reject");
            assert_eq!(
                ir.tools[0].raw.get("type").and_then(|v| v.as_str()),
                Some(ty),
                "raw type must round-trip: {ty}"
            );
            let back = ResponsesCodec::encode_request(&ir);
            assert_eq!(back["tools"][0]["type"], ty);
            assert_eq!(back["tools"][0]["name"], "web_search");
        }
    }

    #[test]
    fn accepts_complex_tool_choice_passthrough() {
        let complex = json!({"type": "function", "function": {"name": "f"}});
        let ir = decode(json!({
            "model": "m",
            "input": [],
            "tool_choice": complex
        }));
        assert_eq!(ir.tool_choice, complex, "complex shape kept opaque");
        let back = ResponsesCodec::encode_request(&ir);
        assert_eq!(back["tool_choice"], complex, "re-emitted unchanged");

        // flat Responses-style function shape also passes through.
        let flat = json!({"type": "function", "name": "f"});
        let ir = decode(json!({
            "model": "m",
            "input": [],
            "tool_choice": flat
        }));
        assert_eq!(ir.tool_choice, flat, "flat shape kept opaque");
        let back = ResponsesCodec::encode_request(&ir);
        assert_eq!(back["tool_choice"], flat, "flat re-emitted unchanged");

        // strings and simple objects still pass.
        for tc in [json!("auto"), json!("required"), json!({"type": "none"})] {
            ResponsesCodec::decode_request(&json!({
                "model": "m", "input": [], "tool_choice": tc
            }))
            .unwrap_or_else(|e| panic!("tool_choice {tc} must pass: {e}"));
        }

        // illegal JSON types still reject.
        for tc in [json!(1), json!(true), json!(["auto"])] {
            let err = ResponsesCodec::decode_request(&json!({
                "model": "m", "input": [], "tool_choice": tc
            }))
            .unwrap_err();
            assert!(
                err.to_string().contains("tool_choice"),
                "type {tc} must reject: {err}"
            );
        }
    }

    #[test]
    fn reject_input_as_response_id() {
        let err = ResponsesCodec::decode_request(&json!({
            "model": "m",
            "input": "resp_1234567890abcdef"
        }))
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("previous response id is not supported")
        );
        // Ordinary prose input must pass.
        ResponsesCodec::decode_request(&json!({"model": "m", "input": "hello world"}))
            .expect("prose input ok");
    }

    // ── unknown item → loss ─────────────────────────────────────

    #[test]
    fn unknown_input_item_records_loss() {
        let ir = decode(json!({
            "model": "m",
            "input": [
                {"type": "mystery_item", "foo": 1},
                {"type": "message", "role": "user", "content": "hi"}
            ]
        }));
        assert!(!ir.loss.is_empty());
        assert_eq!(ir.loss.entries[0].field, "input[0]");
        assert_eq!(ir.messages.len(), 1, "known item still decoded");
    }

    // ── response round-trips ────────────────────────────────────

    #[test]
    fn response_roundtrip_text_function_call_reasoning() {
        let body = json!({
            "id": "resp_1",
            "object": "response",
            "created_at": 1700000000,
            "status": "completed",
            "model": "gpt-4o",
            "output": [
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "think"}]},
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "Hello", "annotations": []}
                ]},
                {"type": "function_call", "call_id": "call_1", "name": "fn",
                 "arguments": "{\"a\":1}"}
            ],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "total_tokens": 15,
                "input_tokens_details": {"cached_tokens": 3},
                "output_tokens_details": {"reasoning_tokens": 2}
            }
        });
        let ir = ResponsesCodec::decode_response(&body);
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert_eq!(ir.id, "resp_1");
        assert_eq!(ir.created, Some(1700000000));
        assert_eq!(ir.output.len(), 3);
        assert!(matches!(ir.output[0], IrOutputItem::Reasoning { .. }));
        assert!(matches!(ir.output[1], IrOutputItem::Text { .. }));
        assert!(matches!(ir.output[2], IrOutputItem::ToolCall(_)));
        // function_call present → FunctionCall finish.
        assert_eq!(ir.finish_reason, Some(IrFinishReason::FunctionCall));
        let u = ir.usage.clone().expect("usage");
        assert_eq!(u.prompt_tokens, 10);
        assert_eq!(u.completion_tokens, 5);
        assert_eq!(u.total_tokens, 15);
        assert_eq!(u.cached_tokens, Some(3));
        assert_eq!(u.reasoning_tokens, Some(2));

        let back = ResponsesCodec::encode_response(&ir);
        assert_eq!(back["id"], "resp_1");
        assert_eq!(back["object"], "response");
        assert_eq!(back["status"], "completed");
        assert_eq!(back["output"][1]["content"][0]["type"], "output_text");
        assert_eq!(back["output"][1]["content"][0]["text"], "Hello");
        assert_eq!(back["output"][2]["type"], "function_call");
        assert_eq!(back["output"][2]["call_id"], "call_1");
        assert_eq!(back["usage"]["input_tokens"], 10);
        assert_eq!(back["usage"]["input_tokens_details"]["cached_tokens"], 3);
        assert_eq!(
            back["usage"]["output_tokens_details"]["reasoning_tokens"],
            2
        );
    }

    #[test]
    fn response_status_maps_to_finish_reason() {
        for (status, incomplete, expect) in [
            ("completed", None, IrFinishReason::Stop),
            (
                "incomplete",
                Some("max_output_tokens"),
                IrFinishReason::Length,
            ),
            (
                "incomplete",
                Some("content_filter"),
                IrFinishReason::ContentFilter,
            ),
            ("failed", None, IrFinishReason::Other("failed".into())),
            ("cancelled", None, IrFinishReason::Cancelled),
        ] {
            let body = json!({
                "id": "r", "status": status,
                "incomplete_details": incomplete.map(|r| json!({"reason": r})),
                "model": "m",
                "output": [{"type": "message", "role": "assistant",
                            "content": [{"type": "output_text", "text": "t"}]}],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            });
            let ir = ResponsesCodec::decode_response(&body);
            assert_eq!(ir.finish_reason, Some(expect.clone()), "status={status}");
        }
    }

    #[test]
    fn response_unknown_output_item_records_loss() {
        let body = json!({
            "id": "r", "status": "completed", "model": "m",
            "output": [{"type": "web_search_call", "id": "ws_1"}],
            "usage": null
        });
        let ir = ResponsesCodec::decode_response(&body);
        assert!(!ir.loss.is_empty());
        assert!(matches!(ir.output[0], IrOutputItem::Raw(_)));
        // Raw round-trips on encode.
        let back = ResponsesCodec::encode_response(&ir);
        assert_eq!(back["output"][0]["type"], "web_search_call");
    }

    // ── stream decode state machine (table-driven) ──────────────

    fn frame(json: Value) -> Vec<u8> {
        format!("data: {json}\n\n").into_bytes()
    }

    #[test]
    fn stream_created_yields_start() {
        let events = ResponsesCodec::decode_stream_frame(&frame(json!({
            "type": "response.created",
            "response": {"id": "resp_1", "object": "response", "created_at": 5, "model": "gpt-4o"}
        })));
        assert!(matches!(events[0], IrStreamEvent::Start(ref m)
                if m.id.as_deref() == Some("resp_1")
                    && m.created == Some(5)
                    && m.model.as_deref() == Some("gpt-4o")));
    }

    #[test]
    fn stream_in_progress_yields_no_event() {
        let events = ResponsesCodec::decode_stream_frame(&frame(json!({
            "type": "response.in_progress"
        })));
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn stream_output_item_added_function_call_yields_tool_call_start() {
        let events = ResponsesCodec::decode_stream_frame(&frame(json!({
            "type": "response.output_item.added",
            "output_index": 1,
            "item": {"type": "function_call", "call_id": "call_1", "name": "fn", "arguments": ""}
        })));
        assert!(matches!(
            events[0],
            IrStreamEvent::ToolCallStart { index: 1, ref id, ref name }
                if id.as_deref() == Some("call_1") && name.as_deref() == Some("fn")
        ));
    }

    #[test]
    fn stream_output_text_delta_yields_text_delta() {
        let events = ResponsesCodec::decode_stream_frame(&frame(json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "delta": "Hello"
        })));
        assert!(matches!(
            events[0],
            IrStreamEvent::TextDelta { index: 0, ref text } if text == "Hello"
        ));
    }

    #[test]
    fn stream_content_part_delta_output_text_yields_text_delta() {
        let events = ResponsesCodec::decode_stream_frame(&frame(json!({
            "type": "response.content_part.delta",
            "output_index": 0,
            "content_index": 0,
            "delta": {"type": "output_text", "text": "Hi"}
        })));
        assert!(matches!(
            events[0],
            IrStreamEvent::TextDelta { ref text, .. } if text == "Hi"
        ));
    }

    #[test]
    fn stream_content_part_delta_summary_yields_reasoning_delta() {
        let events = ResponsesCodec::decode_stream_frame(&frame(json!({
            "type": "response.content_part.delta",
            "output_index": 0,
            "delta": {"type": "summary_text", "text": "hmm"}
        })));
        assert!(matches!(
            events[0],
            IrStreamEvent::ReasoningDelta { ref text, .. } if text == "hmm"
        ));
    }

    #[test]
    fn stream_function_call_arguments_delta_yields_tool_call_delta() {
        let events = ResponsesCodec::decode_stream_frame(&frame(json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 1,
            "delta": "{\"city\":"
        })));
        assert!(matches!(
            events[0],
            IrStreamEvent::ToolCallDelta { index: 1, ref arguments_delta }
                if arguments_delta == "{\"city\":"
        ));
    }

    #[test]
    fn stream_completed_yields_raw_and_usage() {
        let events = ResponsesCodec::decode_stream_frame(&frame(json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1", "status": "completed",
                "usage": {"input_tokens": 7, "output_tokens": 3, "total_tokens": 10}
            }
        })));
        assert!(matches!(events[0], IrStreamEvent::Raw(_)));
        assert!(matches!(events[1], IrStreamEvent::Usage(ref u)
            if u.prompt_tokens == 7 && u.completion_tokens == 3));
        let raw = match &events[0] {
            IrStreamEvent::Raw(b) => String::from_utf8_lossy(b).to_string(),
            _ => unreachable!(),
        };
        assert!(raw.contains("response.completed"), "{raw}");
    }

    #[test]
    fn stream_failed_yields_error_and_raw() {
        let events = ResponsesCodec::decode_stream_frame(&frame(json!({
            "type": "response.failed",
            "response": {"error": {"message": "boom"}}
        })));
        assert!(matches!(events[0], IrStreamEvent::Error(ref m) if m == "boom"));
        assert!(matches!(events[1], IrStreamEvent::Raw(_)));
    }

    #[test]
    fn stream_unknown_event_yields_raw() {
        let events = ResponsesCodec::decode_stream_frame(&frame(json!({
            "type": "response.mystery", "x": 1
        })));
        assert!(matches!(events[0], IrStreamEvent::Raw(_)));
    }

    #[test]
    fn stream_bookkeeping_frames_yield_no_event() {
        for ty in [
            "response.output_item.done",
            "response.content_part.added",
            "response.content_part.done",
        ] {
            let events = ResponsesCodec::decode_stream_frame(&frame(json!({"type": ty})));
            assert!(events.is_empty(), "{ty}: {events:?}");
        }
    }

    #[test]
    fn stream_error_object_yields_error_and_raw() {
        let events =
            ResponsesCodec::decode_stream_frame(br#"data: {"error":{"message":"rate limited"}}"#);
        assert!(matches!(events[0], IrStreamEvent::Error(ref m) if m == "rate limited"));
        assert!(matches!(events[1], IrStreamEvent::Raw(_)));
    }

    // ── stream encode ───────────────────────────────────────────

    #[test]
    fn stream_encode_suppresses_done() {
        let out =
            ResponsesCodec::encode_stream_event(&IrStreamEvent::Raw(b"data: [DONE]\n\n".to_vec()));
        assert!(out.is_empty());
    }

    #[test]
    fn stream_encode_raw_passthrough() {
        let raw = b"data: {\"type\":\"response.custom\"}\n\n".to_vec();
        let out = ResponsesCodec::encode_stream_event(&IrStreamEvent::Raw(raw.clone()));
        assert_eq!(out, raw);
    }

    #[test]
    fn stream_encode_start_emits_created() {
        let out = ResponsesCodec::encode_stream_event(&IrStreamEvent::Start(IrResponseMeta {
            id: Some("resp_1".into()),
            created: Some(9),
            model: Some("gpt-4o".into()),
        }));
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("response.created"), "{s}");
        assert!(s.contains("resp_1"));
    }

    #[test]
    fn stream_encoder_emits_full_lifecycle() {
        let enc = ResponsesCodec::stream_encoder();
        let mut all = Vec::new();
        all.extend(enc(&IrStreamEvent::Start(IrResponseMeta {
            id: Some("resp_1".into()),
            created: Some(1),
            model: Some("m".into()),
        })));
        all.extend(enc(&IrStreamEvent::TextDelta {
            index: 0,
            text: "Hello".into(),
        }));
        all.extend(enc(&IrStreamEvent::ToolCallStart {
            index: 1,
            id: Some("call_1".into()),
            name: Some("fn".into()),
        }));
        all.extend(enc(&IrStreamEvent::ToolCallDelta {
            index: 1,
            arguments_delta: "{\"a\":1}".into(),
        }));
        all.extend(enc(&IrStreamEvent::Finish {
            index: 0,
            finish_reason: Some(IrFinishReason::FunctionCall),
        }));
        let s = String::from_utf8(all).unwrap();
        assert!(s.contains("response.created"), "missing created: {s}");
        assert!(
            s.contains("response.output_item.added"),
            "missing item.added: {s}"
        );
        assert!(
            s.contains("response.output_text.delta"),
            "missing text delta: {s}"
        );
        assert!(
            s.contains("response.function_call_arguments.delta"),
            "missing args delta: {s}"
        );
        assert!(
            s.contains("response.output_item.done"),
            "missing item.done: {s}"
        );
        assert!(s.contains("response.completed"), "missing completed: {s}");
        assert!(!s.contains("[DONE]"), "must not emit [DONE]: {s}");
    }

    #[test]
    fn stream_encoder_finish_emits_completed_once() {
        let enc = ResponsesCodec::stream_encoder();
        let mut all = Vec::new();
        all.extend(enc(&IrStreamEvent::Start(IrResponseMeta::default())));
        all.extend(enc(&IrStreamEvent::Finish {
            index: 0,
            finish_reason: Some(IrFinishReason::Stop),
        }));
        // End never reaches encode in production; if it does, no-op.
        all.extend(enc(&IrStreamEvent::End));
        let s = String::from_utf8(all).unwrap();
        assert_eq!(
            s.matches("response.completed").count(),
            1,
            "exactly one response.completed: {s}"
        );
    }

    #[test]
    fn stream_encoder_native_completed_raw_marks_terminal() {
        let enc = ResponsesCodec::stream_encoder();
        let mut all = Vec::new();
        all.extend(enc(&IrStreamEvent::Start(IrResponseMeta::default())));
        let completed = b"data: {\"type\":\"response.completed\",\"response\":{}}\n\n".to_vec();
        all.extend(enc(&IrStreamEvent::Raw(completed.clone())));
        // A later Finish must not re-emit response.completed (Raw already did).
        all.extend(enc(&IrStreamEvent::Finish {
            index: 0,
            finish_reason: Some(IrFinishReason::Stop),
        }));
        let s = String::from_utf8_lossy(&all);
        assert_eq!(
            s.matches("response.completed").count(),
            1,
            "native Raw completed must suppress synthesized completed: {s}"
        );
    }

    // ── M8 stateful (native allow) ────────────────────────────────

    #[test]
    fn allow_stateful_previous_response_id_and_store() {
        let body = json!({
            "model": "m",
            "previous_response_id": "resp_abc",
            "store": true,
            "input": "hi"
        });
        let ir = ResponsesCodec::decode_request_allow_stateful(&body)
            .expect("Allow mode must accept previous_response_id/store=true");
        assert_eq!(ir.raw.get("previous_response_id"), Some(&json!("resp_abc")));
        assert_eq!(ir.raw.get("store"), Some(&json!(true)));
        let back = ResponsesCodec::encode_request(&ir);
        assert_eq!(back["previous_response_id"], "resp_abc");
        assert_eq!(back["store"], true);
    }

    #[test]
    fn allow_stateful_still_rejects_background() {
        let err = ResponsesCodec::decode_request_allow_stateful(&json!({
            "model": "m",
            "background": true,
            "input": "hi"
        }))
        .unwrap_err();
        assert!(err.to_string().contains("background is not supported"));
    }

    #[test]
    fn reject_mode_still_rejects_stateful() {
        let err = ResponsesCodec::decode_request(&json!({
            "model": "m",
            "previous_response_id": "resp_abc",
            "input": "hi"
        }))
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("previous_response_id is not supported")
        );
    }

    // ── M8 audio / file round-trips ──────────────────────────────

    #[test]
    fn audio_and_document_content_roundtrip() {
        let body = json!({
            "model": "m",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "see this"},
                    {"type": "input_file", "file_id": "file-1"},
                    {"type": "input_audio", "input_audio": {"data": "AAAA", "format": "wav"}}
                ]
            }]
        });
        let ir = decode(body);
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert_eq!(ir.messages.len(), 1, "single user message");
        assert_eq!(ir.messages[0].content.len(), 3);
        assert!(
            matches!(ir.messages[0].content[1], IrContentBlock::Document { .. }),
            "file → Document"
        );
        assert!(
            matches!(ir.messages[0].content[2], IrContentBlock::Audio { .. }),
            "input_audio → Audio"
        );
        let back = ResponsesCodec::encode_request(&ir);
        let parts = back["input"][0]["content"].as_array().expect("parts");
        assert_eq!(parts[1]["type"], "input_file");
        assert_eq!(parts[2]["type"], "input_audio");
    }

    #[test]
    fn web_search_tool_maps_and_encodes_hosted_shape() {
        let ir = decode(json!({
            "model": "m",
            "input": [],
            "tools": [{"type": "web_search", "max_uses": 5}]
        }));
        assert_eq!(ir.tools.len(), 1);
        assert_eq!(
            ir.tools[0].raw.get("type").and_then(|v| v.as_str()),
            Some("web_search")
        );
        let back = ResponsesCodec::encode_request(&ir);
        assert_eq!(back["tools"][0]["type"], "web_search");
        assert_eq!(back["tools"][0]["max_uses"], 5);
    }

    /// Cross-codec: chat-sourced dual-layer `raw` must expand to the flat
    /// Responses tool shape, and the reserved markers must never reach the
    /// wire.
    #[test]
    fn encode_tools_expands_chat_dual_layer_without_reserved_markers() {
        let mut raw = Map::new();
        raw.insert(
            tool_raw::TOOL_EXTRA_KEY.into(),
            json!({"type": "function", "custom": 1}),
        );
        raw.insert(tool_raw::FUNCTION_EXTRA_KEY.into(), json!({"strict": true}));
        let ir = IrRequest {
            model: "gpt-4o".into(),
            messages: vec![IrMessage {
                role: IrRole::User,
                content: vec![IrContentBlock::Text { text: "hi".into() }],
                ..Default::default()
            }],
            tools: vec![IrTool {
                name: "fn".into(),
                description: Some("d".into()),
                input_schema: json!({"type": "object"}),
                raw,
            }],
            ..Default::default()
        };
        let back = ResponsesCodec::encode_request(&ir);
        let tool = &back["tools"][0];
        assert_eq!(tool["type"], json!("function"));
        assert_eq!(tool["name"], json!("fn"));
        assert_eq!(tool["description"], json!("d"));
        assert_eq!(tool["parameters"], json!({"type": "object"}));
        assert_eq!(
            tool["strict"],
            json!(true),
            "function-level extras land at the root: {tool:?}"
        );
        assert_eq!(tool["custom"], json!(1));
        for k in tool.as_object().expect("tool object").keys() {
            assert!(!tool_raw::is_reserved_key(k), "reserved marker leaked: {k}");
        }
    }

    /// Chat-sourced tool with no original `type`: the Responses default
    /// applies, and no marker leaks.
    #[test]
    fn encode_tools_chat_dual_layer_without_type_uses_function_default() {
        let mut raw = Map::new();
        raw.insert(tool_raw::TOOL_EXTRA_KEY.into(), json!({}));
        raw.insert(tool_raw::FUNCTION_EXTRA_KEY.into(), json!({}));
        let ir = IrRequest {
            model: "gpt-4o".into(),
            messages: vec![IrMessage {
                role: IrRole::User,
                content: vec![IrContentBlock::Text { text: "hi".into() }],
                ..Default::default()
            }],
            tools: vec![IrTool {
                name: "fn".into(),
                description: None,
                input_schema: Value::Null,
                raw,
            }],
            ..Default::default()
        };
        let back = ResponsesCodec::encode_request(&ir);
        let tool = &back["tools"][0];
        assert_eq!(tool["type"], json!("function"));
        assert!(tool.get("parameters").is_none());
        for k in tool.as_object().expect("tool object").keys() {
            assert!(!tool_raw::is_reserved_key(k), "reserved marker leaked: {k}");
        }
    }
}
