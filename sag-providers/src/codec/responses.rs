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
//!
//! Request/response bodies are typed ([`ResponsesRequest`] /
//! [`ResponsesResponse`]); only the SSE segment still works on raw
//! `serde_json::Value`. Unknown fields travel through the DTOs' `extra` maps
//! instead of an allow-list filter, and `IrRequest::raw` / `IrResponse::raw`
//! keep carrying them so encode can put them back (R1 / R5).

use std::cell::RefCell;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use sag_core::{
    AppError, IrContentBlock, IrFinishReason, IrMessage, IrOutputItem, IrRequest, IrResponse,
    IrResponseMeta, IrRole, IrStreamEvent, IrTool, IrToolCall, LossReason, LossScope, Reasoning,
    Sampling, Usage,
    api::responses::{
        FunctionCallItem, IncompleteDetails, InputContentPart, InputItem, MessageContent,
        OutputContentPart, OutputItem, OutputMessage, OutputTextContent, ReasoningItem,
        ReasoningParam, ReasoningSummaryPart, ResponseInputTokensDetails,
        ResponseOutputTokensDetails, ResponseUsage, ResponsesInput, ResponsesRequest,
        ResponsesResponse, ResponsesTool, ToolChoiceParam,
    },
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
///
/// The `/v1/responses` handler runs this on the **raw** JSON before typed
/// parsing, so the 422 messages and their ordering are unchanged (R2);
/// [`ResponsesCodec::decode_request_with_mode`] re-checks the same fields on
/// the typed body through the shared [`reject_stateful`] core.
pub fn reject_unsupported_mode(body: &Value, mode: StatefulMode) -> Result<(), AppError> {
    reject_stateful(StatefulProbe::from_value(body), mode)
}

/// Reject stateful / background features with [`StatefulMode::Reject`]
/// (default ingress path for translated-only routes).
pub fn reject_unsupported(body: &Value) -> Result<(), AppError> {
    reject_unsupported_mode(body, StatefulMode::Reject)
}

/// The stateful-relevant slice of a request body.
///
/// Built either from the raw JSON (handler-side pre-check) or from the typed
/// [`ResponsesRequest`] (codec-side re-check).
struct StatefulProbe<'a> {
    /// `previous_response_id` present and non-null.
    previous_response_id: bool,
    /// Raw `store` value, kept untyped on purpose (boolish, see N1).
    store: Option<&'a Value>,
    /// Raw `background` value, kept untyped on purpose (boolish).
    background: Option<&'a Value>,
    /// `input` as a bare string — the stateful response-id form.
    input_text: Option<&'a str>,
}

impl<'a> StatefulProbe<'a> {
    fn from_value(body: &'a Value) -> Self {
        Self {
            previous_response_id: body
                .get("previous_response_id")
                .is_some_and(|v| !v.is_null()),
            store: body.get("store"),
            background: body.get("background"),
            input_text: body.get("input").and_then(|v| v.as_str()),
        }
    }

    fn from_request(req: &'a ResponsesRequest) -> Self {
        Self {
            // Typed: a non-null value always deserialises into `Some`.
            previous_response_id: req.previous_response_id.is_some(),
            store: req.store.as_ref(),
            background: req.background.as_ref(),
            input_text: match &req.input {
                Some(ResponsesInput::Text(text)) => Some(text.as_str()),
                _ => None,
            },
        }
    }
}

/// Shared stateful/background reject core (messages are wire-visible — do not
/// reword).
fn reject_stateful(probe: StatefulProbe<'_>, mode: StatefulMode) -> Result<(), AppError> {
    if mode == StatefulMode::Reject && probe.previous_response_id {
        return Err(reject(
            "previous_response_id is not supported (stateless Responses only)",
        ));
    }
    if mode == StatefulMode::Reject
        && let Some(store) = probe.store
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
    if probe
        .background
        .is_some_and(|v| !v.is_null() && *v != Value::Bool(false))
    {
        return Err(reject(
            "background is not supported (stateless Responses only)",
        ));
    }
    // input as a bare response id string (stateful form) — only when
    // stateful is not allowed for this route.
    if mode == StatefulMode::Reject
        && let Some(s) = probe.input_text
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

// ═══════════════════════════════════════════════════════════════════
// helpers
// ═══════════════════════════════════════════════════════════════════

/// Serialize a typed DTO slice for passthrough snapshots (never panics).
fn to_value_or_null<T: Serialize + ?Sized>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// Copy a typed field into the IR `raw` passthrough map (encode puts it back).
fn put_raw<T: Serialize>(raw: &mut Map<String, Value>, key: &str, value: &Option<T>) {
    if let Some(value) = value
        && let Ok(value) = serde_json::to_value(value)
    {
        raw.insert(key.to_string(), value);
    }
}

/// Move a `raw` passthrough value back into its typed slot.
///
/// Returns `false` when the key has no typed home (or the value does not fit),
/// so the caller keeps it in `extra` instead of dropping it.
fn absorb<T: DeserializeOwned>(slot: &mut Option<T>, value: &Value) -> bool {
    if slot.is_some() {
        return true;
    }
    match serde_json::from_value(value.clone()) {
        Ok(parsed) => {
            *slot = Some(parsed);
            true
        }
        Err(_) => false,
    }
}

/// Responses usage object → IR [`Usage`].
///
/// SSE frames are still raw JSON, so streaming parses the object on demand and
/// both paths share [`usage_from_responses`].
///
/// A single ill-typed sub-object (e.g. `input_tokens_details.cached_tokens`
/// sent as a string) fails the whole typed parse, so the fallback reads every
/// counter independently instead of reporting all-zero usage.
fn responses_usage(value: &Value) -> Usage {
    match serde_json::from_value::<ResponseUsage>(value.clone()) {
        Ok(usage) => usage_from_responses(&usage),
        Err(_) => {
            let input = value
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            let output = value
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32;
            let total = value
                .get("total_tokens")
                .and_then(Value::as_u64)
                .map(|v| v as u32)
                .unwrap_or_else(|| input.saturating_add(output));
            Usage {
                prompt_tokens: input,
                completion_tokens: output,
                total_tokens: total,
                // Same known-miss/unknown split as the typed arm: a present
                // `input_tokens` with no readable details means "no cache
                // hit" (`Some(0)`), not "unknown" (`None`).
                cached_tokens: value
                    .pointer("/input_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32)
                    .or((input > 0).then_some(0)),
                cache_creation_tokens: None,
                reasoning_tokens: value
                    .pointer("/output_tokens_details/reasoning_tokens")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32),
            }
        }
    }
}

/// Typed [`ResponseUsage`] → IR [`Usage`].
fn usage_from_responses(usage: &ResponseUsage) -> Usage {
    let input = usage.input_tokens as u32;
    let output = usage.output_tokens as u32;
    // `total_tokens` is `#[serde(default)]` on the DTO, so a missing value is
    // indistinguishable from an explicit 0 — fall back to the sum in that case.
    let total = if usage.total_tokens > 0 {
        usage.total_tokens as u32
    } else {
        input.saturating_add(output)
    };
    Usage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: total,
        cached_tokens: usage
            .input_tokens_details
            .as_ref()
            .map(|d| d.cached_tokens as u32)
            .or((input > 0).then_some(0)),
        cache_creation_tokens: None,
        reasoning_tokens: usage
            .output_tokens_details
            .as_ref()
            .map(|d| d.reasoning_tokens as u32),
    }
}

/// IR [`Usage`] → Responses usage object.
fn usage_to_responses(usage: &Usage) -> ResponseUsage {
    ResponseUsage {
        input_tokens: u64::from(usage.prompt_tokens),
        output_tokens: u64::from(usage.completion_tokens),
        total_tokens: u64::from(usage.total_tokens),
        input_tokens_details: usage
            .cached_tokens
            .map(|cached| ResponseInputTokensDetails {
                cached_tokens: u64::from(cached),
                ..Default::default()
            }),
        output_tokens_details: usage.reasoning_tokens.map(|reasoning| {
            ResponseOutputTokensDetails {
                reasoning_tokens: u64::from(reasoning),
                ..Default::default()
            }
        }),
        ..Default::default()
    }
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
/// shape is kept in `ir.tool_choice`; only illegal JSON types (number /
/// array / bool) are rejected (422).
fn decode_tool_choice(choice: &ToolChoiceParam) -> Result<Value, AppError> {
    let value = match choice {
        // `Unknown` holds shapes no modelled variant reproduces exactly: a
        // string/object is legal opaque passthrough, anything else (number /
        // array / bool) is not a legal `tool_choice` at all.
        ToolChoiceParam::Unknown(Value::String(_) | Value::Object(_)) => to_value_or_null(choice),
        ToolChoiceParam::Unknown(_) => {
            return Err(reject(
                "tool_choice must be a string or object (auto/none/required/function/…)",
            ));
        }
        other => to_value_or_null(other),
    };
    Ok(normalize_tool_choice(value))
}

/// Drop the `allowed_tools: []` that `ToolChoiceAllowed` grows as the union's
/// object catch-all (`#[serde(default)]` without `skip_serializing_if`), so a
/// `{"type": "function", "name": …}` selector is re-emitted unchanged (C6).
fn normalize_tool_choice(mut value: Value) -> Value {
    if let Some(obj) = value.as_object_mut()
        && obj.get("type").and_then(Value::as_str) != Some("allowed_tools")
        && obj
            .get("allowed_tools")
            .and_then(Value::as_array)
            .is_some_and(|allowed| allowed.is_empty())
    {
        obj.remove("allowed_tools");
    }
    value
}

/// Parse a raw value into a typed DTO, keeping it verbatim in the caller's
/// `Unknown` variant unless the typed form round-trips unchanged.
///
/// Several DTO variants carry `#[serde(default)]` fields without
/// `skip_serializing_if` (e.g. `tool_choice.allowed_tools`,
/// `file_search.vector_store_ids`), so a plain parse would grow fields the
/// caller never sent. The round-trip check keeps the wire shape intact (R3).
fn parse_faithful<T>(value: &Value) -> Option<T>
where
    T: DeserializeOwned + Serialize,
{
    let parsed = serde_json::from_value::<T>(value.clone()).ok()?;
    (to_value_or_null(&parsed) == *value).then_some(parsed)
}

/// Parse a typed `tool_choice` back out of a raw IR value.
fn tool_choice_from_value(value: Value) -> ToolChoiceParam {
    parse_faithful(&value).unwrap_or(ToolChoiceParam::Unknown(value))
}

/// Parse a raw `tools[]` entry into the typed union (`Unknown` keeps the
/// original object verbatim).
fn responses_tool_from_value(value: Value) -> ResponsesTool {
    parse_faithful(&value).unwrap_or(ResponsesTool::Unknown(value))
}

/// Parse a raw `input[]` entry into the typed union.
fn input_item_from_value(value: Value) -> InputItem {
    parse_faithful(&value).unwrap_or(InputItem::Unknown(value))
}

/// Parse a raw `output[]` entry into the typed union.
fn output_item_from_value(value: Value) -> OutputItem {
    parse_faithful(&value).unwrap_or(OutputItem::Unknown(value))
}

/// Parse `incomplete_details` from a raw IR passthrough value.
fn incomplete_details_from_value(value: Value) -> IncompleteDetails {
    serde_json::from_value(value.clone()).unwrap_or_else(|_| IncompleteDetails {
        extra: value.as_object().cloned().unwrap_or_default(),
        ..Default::default()
    })
}

/// `arguments` is a JSON-encoded string on the wire; parse it back unless it
/// is plain text (then keep the string verbatim).
fn parse_arguments(arguments: &str) -> Value {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .filter(|value| !value.is_string())
        .unwrap_or_else(|| Value::String(arguments.to_string()))
}

/// Concatenated reasoning text (`summary` plus the unmodelled `content`).
fn reasoning_text(item: &ReasoningItem) -> String {
    let mut text = String::new();
    for part in &item.summary {
        text.push_str(&part.text);
    }
    if let Some(parts) = item.extra.get("content").and_then(Value::as_array) {
        for part in parts {
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                text.push_str(t);
            }
        }
    }
    text
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

/// Typed input content part → IR content block.
fn decode_input_part(
    part: &InputContentPart,
    path: &str,
    loss: &mut sag_core::LossInventory,
) -> IrContentBlock {
    match part {
        InputContentPart::Text(text) => IrContentBlock::Text {
            text: text.text.clone(),
        },
        // Media parts are forwarded verbatim as IR payloads; the shape-based
        // mapper keeps working on their serialized form.
        InputContentPart::Image(_) | InputContentPart::File(_) | InputContentPart::Unknown(_) => {
            decode_content_part(&to_value_or_null(part), path, loss)
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
    pub fn decode_request(req: &ResponsesRequest) -> sag_core::Result<IrRequest> {
        Self::decode_request_with_mode(req, StatefulMode::Reject)
    }

    /// M8: decode allowing stateful fields (`previous_response_id` /
    /// `store=true`) into `IrRequest.raw` for native Responses passthrough.
    /// `background` is still rejected.
    pub fn decode_request_allow_stateful(req: &ResponsesRequest) -> sag_core::Result<IrRequest> {
        Self::decode_request_with_mode(req, StatefulMode::Allow)
    }

    /// Decode with an explicit [`StatefulMode`].
    pub fn decode_request_with_mode(
        req: &ResponsesRequest,
        mode: StatefulMode,
    ) -> sag_core::Result<IrRequest> {
        reject_stateful(StatefulProbe::from_request(req), mode)?;

        let mut loss = sag_core::LossInventory::default();
        // Unknown fields ride the typed `extra` map — the old allow-list
        // filter is gone (R1).
        let mut raw = req.extra.clone();

        // Typed fields without an IR home round-trip via `raw`; encode moves
        // them back into their typed slot. `store` always round-trips and
        // `previous_response_id` is re-emitted for the native lane only (R5).
        if let Some(store) = &req.store {
            raw.insert("store".into(), store.clone());
        }
        if mode == StatefulMode::Allow
            && let Some(previous) = &req.previous_response_id
        {
            raw.insert("previous_response_id".into(), json!(previous));
        }
        put_raw(&mut raw, "parallel_tool_calls", &req.parallel_tool_calls);
        put_raw(&mut raw, "text", &req.text);
        put_raw(&mut raw, "reasoning", &req.reasoning);
        put_raw(&mut raw, "metadata", &req.metadata);
        put_raw(&mut raw, "service_tier", &req.service_tier);
        put_raw(&mut raw, "include", &req.include);
        put_raw(&mut raw, "top_logprobs", &req.top_logprobs);
        put_raw(&mut raw, "max_tool_calls", &req.max_tool_calls);
        put_raw(&mut raw, "stream_options", &req.stream_options);
        put_raw(&mut raw, "prompt", &req.prompt);
        put_raw(&mut raw, "conversation", &req.conversation);
        put_raw(&mut raw, "prompt_cache_key", &req.prompt_cache_key);
        put_raw(&mut raw, "prompt_cache_options", &req.prompt_cache_options);
        put_raw(&mut raw, "safety_identifier", &req.safety_identifier);
        put_raw(&mut raw, "context_management", &req.context_management);
        put_raw(&mut raw, "moderation", &req.moderation);
        put_raw(&mut raw, "access_programs", &req.access_programs);

        let model = req.model.clone().unwrap_or_default();
        let stream = req.stream.unwrap_or(false);
        let mut messages = Vec::new();

        // instructions → leading system message (multi-segment preserved).
        if let Some(instructions) = &req.instructions {
            let content = match instructions {
                MessageContent::Text(text) => vec![IrContentBlock::Text { text: text.clone() }],
                MessageContent::Parts(parts) => parts
                    .iter()
                    .map(|part| decode_input_part(part, "instructions", &mut loss))
                    .collect(),
                MessageContent::Unknown(other) => {
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
        match &req.input {
            None => {}
            Some(ResponsesInput::Text(text)) => {
                messages.push(IrMessage {
                    role: IrRole::User,
                    content: vec![IrContentBlock::Text { text: text.clone() }],
                    name: None,
                    tool_call_id: None,
                    tool_calls: Vec::new(),
                    raw: Map::new(),
                });
            }
            Some(ResponsesInput::Items(items)) => {
                for (ii, item) in items.iter().enumerate() {
                    decode_input_item(item, ii, &mut messages, &mut loss);
                }
            }
            Some(ResponsesInput::Unknown(other)) => {
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

        // tools: type:"function" and type:"web_search" (M8). The shape checks
        // stay on the wire form because `IrTool.raw` keeps the original object.
        let mut tools = Vec::new();
        if let Some(list) = &req.tools {
            for (i, tool) in list.iter().enumerate() {
                let t = to_value_or_null(tool);
                if let Some(err) = tool_reject(&t) {
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
        let temperature = req.temperature;
        let top_p = req.top_p;
        let max_tokens = req.max_output_tokens.and_then(|n| u32::try_from(n).ok());

        let tool_choice = match &req.tool_choice {
            None => Value::Null,
            Some(choice) => decode_tool_choice(choice)?,
        };

        // reasoning: best-effort effort mapping + raw passthrough.
        let mut reasoning = Reasoning::default();
        if let Some(param) = &req.reasoning {
            if let Some(effort) = &param.effort {
                reasoning.effort = Some(effort.clone());
            }
            // `budget_tokens` is not modelled by the DTO → read it from `extra`.
            if let Some(b) = param.extra.get("budget_tokens").and_then(Value::as_u64)
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
    pub fn encode_request(ir: &IrRequest) -> ResponsesRequest {
        Self::encode_request_with_loss(ir, &ir.model).0
    }

    /// Encode IR → Responses body with an explicit model (real_model override).
    pub fn encode_request_for_model(ir: &IrRequest, model: &str) -> ResponsesRequest {
        Self::encode_request_with_loss(ir, model).0
    }

    /// Encode IR → Responses body, returning encode-side losses (S1):
    /// system non-Text blocks dropped when lifting to `instructions`,
    /// and ToolUse/ToolResult content dropped when re-encoding messages.
    pub fn encode_request_with_loss(
        ir: &IrRequest,
        model: &str,
    ) -> (ResponsesRequest, sag_core::LossInventory) {
        let mut loss = ir.loss.clone();
        let body = Self::encode_request_inner(ir, model, &mut loss);
        (body, loss)
    }

    fn encode_request_inner(
        ir: &IrRequest,
        model: &str,
        loss: &mut sag_core::LossInventory,
    ) -> ResponsesRequest {
        let mut body = ResponsesRequest {
            model: Some(model.to_string()),
            ..Default::default()
        };

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
                                to_value_or_null(other),
                            );
                        }
                    }
                }
            }
            if !text.is_empty() {
                body.instructions = Some(MessageContent::Text(text));
            }
        }

        body.input = Some(ResponsesInput::Items(encode_input_items(&chat_msgs, loss)));

        if ir.stream {
            body.stream = Some(true);
        }
        body.temperature = ir.sampling.temperature;
        body.top_p = ir.sampling.top_p;
        body.max_output_tokens = ir.max_tokens.map(u64::from);
        if !ir.tool_choice.is_null() {
            body.tool_choice = Some(tool_choice_from_value(ir.tool_choice.clone()));
        }

        // tools: prefer original raw when present; else rebuild.
        match ir.raw.get("tools") {
            Some(value) => {
                if let Some(entries) = value.as_array() {
                    body.tools = Some(
                        entries
                            .iter()
                            .cloned()
                            .map(responses_tool_from_value)
                            .collect(),
                    );
                }
            }
            None if !ir.tools.is_empty() => {
                if let Some(entries) = encode_tools(&ir.tools).as_array() {
                    body.tools = Some(
                        entries
                            .iter()
                            .cloned()
                            .map(responses_tool_from_value)
                            .collect(),
                    );
                }
            }
            None => {}
        }

        // reasoning effort → Responses reasoning object (best-effort).
        if let Some(effort) = ir.reasoning.effort.as_deref()
            && effort != "none"
            && !ir.raw.contains_key("reasoning")
        {
            body.reasoning = Some(ReasoningParam {
                effort: Some(effort.to_string()),
                ..Default::default()
            });
        }

        // Raw fields round-trip: back into their typed slot when one exists
        // (never overwriting a value synthesized above), else into `extra`.
        for (k, v) in &ir.raw {
            if k == "tools" {
                continue; // consumed above
            }
            if !absorb_request(&mut body, k, v) {
                body.extra.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }

        body
    }

    // ═══════════════════════════════════════════════════════════
    // response decode (upstream): Responses body → IrResponse
    // ═══════════════════════════════════════════════════════════

    /// Decode a non-stream Responses body into IR.
    pub fn decode_response(body: &ResponsesResponse) -> IrResponse {
        let mut loss = sag_core::LossInventory::default();
        // Unknown top-level fields → raw (passthrough, no loss).
        let mut raw = body.extra.clone();

        // Preserve status/incomplete for finish_reason encode round-trip.
        if let Some(status) = &body.status {
            raw.insert("__status".into(), json!(status));
        }
        if let Some(details) = &body.incomplete_details {
            raw.insert("__incomplete_details".into(), to_value_or_null(details));
        }
        // Typed echo fields that `encode_response` does not synthesize stay on
        // the raw passthrough lane (same set the old allow-list skipped).
        put_raw(&mut raw, "completed_at", &body.completed_at);
        put_raw(&mut raw, "output_text", &body.output_text);
        put_raw(&mut raw, "max_tool_calls", &body.max_tool_calls);
        put_raw(&mut raw, "reasoning", &body.reasoning);
        put_raw(&mut raw, "top_logprobs", &body.top_logprobs);
        put_raw(&mut raw, "service_tier", &body.service_tier);
        put_raw(&mut raw, "conversation", &body.conversation);
        put_raw(&mut raw, "prompt", &body.prompt);
        put_raw(&mut raw, "prompt_cache_key", &body.prompt_cache_key);
        put_raw(&mut raw, "prompt_cache_options", &body.prompt_cache_options);
        put_raw(
            &mut raw,
            "prompt_cache_diagnostics",
            &body.prompt_cache_diagnostics,
        );
        put_raw(&mut raw, "safety_identifier", &body.safety_identifier);
        put_raw(&mut raw, "access_programs", &body.access_programs);
        put_raw(&mut raw, "background", &body.background);

        let mut has_function_call = false;
        let mut output = Vec::new();
        for (i, item) in body.output.iter().enumerate() {
            match item {
                OutputItem::Message(message) => {
                    for (pi, part) in message.content.iter().enumerate() {
                        match part {
                            OutputContentPart::OutputText(text) => {
                                // Non-empty annotations cannot live on
                                // IrOutputItem::Text — record loss.
                                if !text.annotations.is_empty() {
                                    loss.push_loss(
                                        LossScope::Response,
                                        format!("output[{i}].content[{pi}].annotations"),
                                        LossReason::UnsupportedByIr,
                                        to_value_or_null(&text.annotations),
                                    );
                                }
                                output.push(IrOutputItem::Text {
                                    text: text.text.clone(),
                                });
                            }
                            other => {
                                let value = to_value_or_null(other);
                                loss.push_loss(
                                    LossScope::Response,
                                    format!("output[{i}].content[{pi}]"),
                                    LossReason::UnsupportedByIr,
                                    value.clone(),
                                );
                                output.push(IrOutputItem::Raw(value));
                            }
                        }
                    }
                }
                OutputItem::FunctionCall(call) => {
                    has_function_call = true;
                    output.push(IrOutputItem::ToolCall(IrToolCall {
                        id: call.call_id.clone(),
                        name: call.name.clone(),
                        arguments: parse_arguments(&call.arguments),
                        index: Some(output.len() as u32),
                    }));
                }
                OutputItem::Reasoning(reasoning) => {
                    output.push(IrOutputItem::Reasoning {
                        text: reasoning_text(reasoning),
                    });
                }
                other => {
                    // Unknown (web_search_call, …) → Raw + loss.
                    let value = to_value_or_null(other);
                    loss.push_loss(
                        LossScope::Response,
                        format!("output[{i}]"),
                        LossReason::UnsupportedByIr,
                        value.clone(),
                    );
                    output.push(IrOutputItem::Raw(value));
                }
            }
        }

        let status = body.status.as_deref().unwrap_or("completed");
        let incomplete_reason = body
            .incomplete_details
            .as_ref()
            .and_then(|details| details.reason.as_deref());
        let finish_reason = status_to_finish(status, incomplete_reason, has_function_call);
        let usage = body.usage.as_ref().map(usage_from_responses);

        IrResponse {
            id: body.id.clone(),
            created: body.created_at,
            model: body.model.clone().unwrap_or_default(),
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
    pub fn encode_response(ir: &IrResponse) -> ResponsesResponse {
        let mut body = ResponsesResponse {
            id: ir.id.clone(),
            object: Some("response".to_string()),
            created_at: ir.created,
            model: Some(ir.model.clone()),
            ..Default::default()
        };

        // status: prefer raw (round-trip) else derive from finish_reason.
        let status = match ir.raw.get("__status").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => match ir.finish_reason.as_ref() {
                Some(IrFinishReason::Length | IrFinishReason::ContentFilter) => "incomplete".into(),
                Some(IrFinishReason::Other(s)) if s == "failed" => "failed".into(),
                Some(IrFinishReason::Cancelled) => "cancelled".into(),
                _ => "completed".into(),
            },
        };
        if let Some(details) = ir.raw.get("__incomplete_details") {
            body.incomplete_details = Some(incomplete_details_from_value(details.clone()));
        } else if status == "incomplete" {
            let reason = match ir.finish_reason.as_ref() {
                Some(IrFinishReason::Length) => "max_output_tokens",
                Some(IrFinishReason::ContentFilter) => "content_filter",
                _ => "max_output_tokens",
            };
            body.incomplete_details = Some(IncompleteDetails {
                reason: Some(reason.to_string()),
                ..Default::default()
            });
        }
        body.status = Some(status);

        body.output = ir
            .output
            .iter()
            .filter_map(|item| encode_output_item(item, &ir.id))
            .collect();

        body.usage = ir.usage.as_ref().map(usage_to_responses);

        // Raw fields round-trip (codec-private `__` keys excepted), back into
        // their typed slot when one exists.
        for (k, v) in &ir.raw {
            if k.starts_with("__") {
                continue;
            }
            if !absorb_response(&mut body, k, v) {
                body.extra.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }

        body
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
                // Synthesize the terminal event (translated path; native
                // passes the upstream frame through as Raw).
                encode_terminal_event(finish_reason.as_ref())
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
                    out.extend_from_slice(&encode_terminal_event(finish_reason.as_ref()));
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

/// Map an IR finish reason to the Responses terminal event triple:
/// `(event_type, status, incomplete_reason)`.
///
/// Only truncation / content filtering terminate as `response.incomplete` with
/// an `incomplete_details.reason`; every other reason keeps the historical
/// `response.completed` event with its derived status (native upstream already
/// terminated via `Raw`, so `failed` / `cancelled` stay on `completed` here).
fn terminal_event_fields(
    finish: Option<&IrFinishReason>,
) -> (&'static str, &'static str, Option<&'static str>) {
    match finish {
        Some(IrFinishReason::Length) => (
            "response.incomplete",
            "incomplete",
            Some("max_output_tokens"),
        ),
        Some(IrFinishReason::ContentFilter) => {
            ("response.incomplete", "incomplete", Some("content_filter"))
        }
        Some(IrFinishReason::Cancelled) => ("response.completed", "cancelled", None),
        Some(IrFinishReason::Other(s)) if s == "failed" => ("response.completed", "failed", None),
        _ => ("response.completed", "completed", None),
    }
}

/// Encode the synthesized terminal SSE frame for a finish reason.
fn encode_terminal_event(finish: Option<&IrFinishReason>) -> Vec<u8> {
    let (event_type, status, incomplete_reason) = terminal_event_fields(finish);
    let mut response = json!({"object": "response", "status": status, "output": []});
    if let Some(reason) = incomplete_reason {
        response["incomplete_details"] = json!({"reason": reason});
    }
    let payload = json!({"type": event_type, "response": response});
    format!("data: {payload}\n\n").into_bytes()
}

// ═══════════════════════════════════════════════════════════════════
// request helpers
// ═══════════════════════════════════════════════════════════════════

fn decode_input_item(
    item: &InputItem,
    ii: usize,
    messages: &mut Vec<IrMessage>,
    loss: &mut sag_core::LossInventory,
) {
    match item {
        InputItem::Message(message) => {
            let role = IrRole::from_raw(message.role.as_str());
            // Typed fields without an IR home round-trip via the message raw.
            let mut raw = message.extra.clone();
            put_raw(&mut raw, "phase", &message.phase);
            put_raw(&mut raw, "status", &message.status);
            put_raw(&mut raw, "id", &message.id);
            let content = match &message.content {
                None => Vec::new(),
                Some(MessageContent::Text(text)) => {
                    vec![IrContentBlock::Text { text: text.clone() }]
                }
                Some(MessageContent::Parts(parts)) => parts
                    .iter()
                    .enumerate()
                    .map(|(pi, part)| {
                        decode_input_part(part, &format!("input[{ii}].content[{pi}]"), loss)
                    })
                    .collect(),
                Some(MessageContent::Unknown(other)) => {
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
        InputItem::FunctionCallOutput(item) => {
            let output_text = match &item.output {
                None => String::new(),
                Some(MessageContent::Text(text)) => text.clone(),
                Some(MessageContent::Parts(parts)) => {
                    let mut text = String::new();
                    for (pi, part) in parts.iter().enumerate() {
                        let piece = match part {
                            InputContentPart::Text(part) => Some(part.text.as_str()),
                            // Unmodelled parts still expose `text` (or are a
                            // bare string) — keep those, drop the rest (S2).
                            InputContentPart::Unknown(value) => value
                                .get("text")
                                .and_then(Value::as_str)
                                .or_else(|| value.as_str()),
                            InputContentPart::Image(_) | InputContentPart::File(_) => None,
                        };
                        match piece {
                            Some(piece) => text.push_str(piece),
                            None => loss.push_loss(
                                LossScope::ContentBlock,
                                format!("input[{ii}].output[{pi}]"),
                                LossReason::UnsupportedByIr,
                                to_value_or_null(part),
                            ),
                        }
                    }
                    text
                }
                Some(MessageContent::Unknown(other)) => {
                    serde_json::to_string(other).unwrap_or_default()
                }
            };
            let mut raw = item.extra.clone();
            put_raw(&mut raw, "status", &item.status);
            put_raw(&mut raw, "id", &item.id);
            messages.push(IrMessage {
                role: IrRole::Tool,
                content: vec![IrContentBlock::Text { text: output_text }],
                name: None,
                tool_call_id: Some(item.call_id.clone()),
                tool_calls: Vec::new(),
                raw,
            });
        }
        InputItem::FunctionCall(item) => {
            let mut raw = item.extra.clone();
            put_raw(&mut raw, "namespace", &item.namespace);
            put_raw(&mut raw, "id", &item.id);
            put_raw(&mut raw, "status", &item.status);
            messages.push(IrMessage {
                role: IrRole::Assistant,
                content: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: vec![IrToolCall {
                    id: item.call_id.clone(),
                    name: item.name.clone(),
                    arguments: parse_arguments(&item.arguments),
                    index: Some(0),
                }],
                raw,
            });
        }
        InputItem::Reasoning(item) => {
            messages.push(IrMessage {
                role: IrRole::Assistant,
                content: vec![IrContentBlock::Reasoning {
                    text: reasoning_text(item),
                }],
                ..Default::default()
            });
        }
        // `role` is required by the typed `MessageItem` but was optional for
        // the old value-based decoder, which defaulted it to `user`. Keep
        // tolerating the non-spec `{"type": "message"}` shape — and a
        // non-string `role` — instead of dropping user content.
        InputItem::Unknown(Value::Object(obj))
            if obj.get("type").and_then(Value::as_str) == Some("message")
                && !obj.get("role").is_some_and(Value::is_string) =>
        {
            let mut patched = obj.clone();
            patched.insert("role".into(), json!("user"));
            decode_input_item(
                &input_item_from_value(Value::Object(patched)),
                ii,
                messages,
                loss,
            );
        }
        other => {
            // Unknown item type → structural drop with loss (raw preserved
            // on the loss entry value snapshot).
            loss.push_loss(
                LossScope::Message,
                format!("input[{ii}]"),
                LossReason::UnsupportedByIr,
                to_value_or_null(other),
            );
        }
    }
}

fn encode_input_items(
    chat_msgs: &[&IrMessage],
    loss: &mut sag_core::LossInventory,
) -> Vec<InputItem> {
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
    out.into_iter().map(input_item_from_value).collect()
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

// ═══════════════════════════════════════════════════════════════════
// response helpers
// ═══════════════════════════════════════════════════════════════════

/// IR output item → typed Responses output item (`None` drops empty
/// reasoning items, matching the previous behaviour).
fn encode_output_item(item: &IrOutputItem, response_id: &str) -> Option<OutputItem> {
    match item {
        IrOutputItem::Text { text } => Some(OutputItem::Message(OutputMessage {
            kind: "message".to_string(),
            id: Some(format!("msg_{response_id}")),
            role: Some("assistant".to_string()),
            status: Some("completed".to_string()),
            content: vec![OutputContentPart::OutputText(OutputTextContent {
                kind: "output_text".to_string(),
                text: text.clone(),
                annotations: Vec::new(),
                logprobs: None,
                extra: Map::new(),
            })],
            ..Default::default()
        })),
        IrOutputItem::Reasoning { text } => {
            if text.is_empty() {
                return None;
            }
            Some(OutputItem::Reasoning(ReasoningItem {
                kind: "reasoning".to_string(),
                id: Some(format!("rs_{response_id}")),
                summary: vec![ReasoningSummaryPart {
                    kind: "summary_text".to_string(),
                    text: text.clone(),
                    extra: Map::new(),
                }],
                ..Default::default()
            }))
        }
        IrOutputItem::ToolCall(call) => Some(OutputItem::FunctionCall(FunctionCallItem {
            kind: "function_call".to_string(),
            id: Some(format!("fc_{}", call.id)),
            call_id: call.id.clone(),
            name: call.name.clone(),
            arguments: match &call.arguments {
                Value::String(s) => s.clone(),
                other => serde_json::to_string(other).unwrap_or_else(|_| "{}".into()),
            },
            status: Some("completed".to_string()),
            ..Default::default()
        })),
        IrOutputItem::Raw(value) => Some(output_item_from_value(value.clone())),
    }
}

/// Move a raw passthrough value back into its typed request slot.
///
/// Returns `false` when the key has no typed home (or the value does not fit),
/// so the caller keeps it in `extra` instead of dropping it.
fn absorb_request(body: &mut ResponsesRequest, key: &str, value: &Value) -> bool {
    match key {
        "model" => absorb(&mut body.model, value),
        "input" => absorb(&mut body.input, value),
        "instructions" => absorb(&mut body.instructions, value),
        "tools" => absorb(&mut body.tools, value),
        "tool_choice" => absorb(&mut body.tool_choice, value),
        "text" => absorb(&mut body.text, value),
        "reasoning" => absorb(&mut body.reasoning, value),
        "temperature" => absorb(&mut body.temperature, value),
        "top_p" => absorb(&mut body.top_p, value),
        "top_logprobs" => absorb(&mut body.top_logprobs, value),
        "max_output_tokens" => absorb(&mut body.max_output_tokens, value),
        "max_tool_calls" => absorb(&mut body.max_tool_calls, value),
        "stream" => absorb(&mut body.stream, value),
        "stream_options" => absorb(&mut body.stream_options, value),
        "store" => absorb(&mut body.store, value),
        "metadata" => absorb(&mut body.metadata, value),
        "previous_response_id" => absorb(&mut body.previous_response_id, value),
        "conversation" => absorb(&mut body.conversation, value),
        "parallel_tool_calls" => absorb(&mut body.parallel_tool_calls, value),
        "service_tier" => absorb(&mut body.service_tier, value),
        "prompt" => absorb(&mut body.prompt, value),
        "prompt_cache_key" => absorb(&mut body.prompt_cache_key, value),
        "prompt_cache_options" => absorb(&mut body.prompt_cache_options, value),
        "safety_identifier" => absorb(&mut body.safety_identifier, value),
        "context_management" => absorb(&mut body.context_management, value),
        "include" => absorb(&mut body.include, value),
        "moderation" => absorb(&mut body.moderation, value),
        "access_programs" => absorb(&mut body.access_programs, value),
        "background" => absorb(&mut body.background, value),
        _ => false,
    }
}

/// Move a raw passthrough value back into its typed response slot.
fn absorb_response(body: &mut ResponsesResponse, key: &str, value: &Value) -> bool {
    match key {
        // Always synthesized by `encode_response`; never duplicated.
        "id" | "object" | "output" => true,
        "model" => absorb(&mut body.model, value),
        "created_at" => absorb(&mut body.created_at, value),
        "status" => absorb(&mut body.status, value),
        "incomplete_details" => absorb(&mut body.incomplete_details, value),
        "usage" => absorb(&mut body.usage, value),
        "completed_at" => absorb(&mut body.completed_at, value),
        "output_text" => absorb(&mut body.output_text, value),
        "error" => absorb(&mut body.error, value),
        "instructions" => absorb(&mut body.instructions, value),
        "max_output_tokens" => absorb(&mut body.max_output_tokens, value),
        "max_tool_calls" => absorb(&mut body.max_tool_calls, value),
        "parallel_tool_calls" => absorb(&mut body.parallel_tool_calls, value),
        "previous_response_id" => absorb(&mut body.previous_response_id, value),
        "reasoning" => absorb(&mut body.reasoning, value),
        "temperature" => absorb(&mut body.temperature, value),
        "top_p" => absorb(&mut body.top_p, value),
        "top_logprobs" => absorb(&mut body.top_logprobs, value),
        "text" => absorb(&mut body.text, value),
        "tools" => absorb(&mut body.tools, value),
        "tool_choice" => absorb(&mut body.tool_choice, value),
        "service_tier" => absorb(&mut body.service_tier, value),
        "conversation" => absorb(&mut body.conversation, value),
        "metadata" => absorb(&mut body.metadata, value),
        "prompt" => absorb(&mut body.prompt, value),
        "prompt_cache_key" => absorb(&mut body.prompt_cache_key, value),
        "prompt_cache_options" => absorb(&mut body.prompt_cache_options, value),
        "prompt_cache_diagnostics" => absorb(&mut body.prompt_cache_diagnostics, value),
        "safety_identifier" => absorb(&mut body.safety_identifier, value),
        "access_programs" => absorb(&mut body.access_programs, value),
        "background" => absorb(&mut body.background, value),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Typed request body from a JSON fixture (the handler parses before the
    /// codec runs, so tests mirror that order).
    fn request(body: Value) -> ResponsesRequest {
        serde_json::from_value(body).expect("typed responses request")
    }

    fn decode(body: Value) -> IrRequest {
        ResponsesCodec::decode_request(&request(body)).expect("valid responses request")
    }

    /// Decode expected to fail with a 422-class error.
    fn try_decode(body: Value) -> sag_core::Result<IrRequest> {
        ResponsesCodec::decode_request(&request(body))
    }

    /// M8 native-lane decode (stateful fields allowed).
    fn allow_stateful(body: Value) -> sag_core::Result<IrRequest> {
        ResponsesCodec::decode_request_allow_stateful(&request(body))
    }

    /// Serialize the typed encode output back to JSON for assertions.
    fn encode_request(ir: &IrRequest) -> Value {
        to_value_or_null(&ResponsesCodec::encode_request(ir))
    }

    fn decode_response(body: Value) -> IrResponse {
        let typed: ResponsesResponse =
            serde_json::from_value(body).expect("typed responses response");
        ResponsesCodec::decode_response(&typed)
    }

    fn encode_response(ir: &IrResponse) -> Value {
        to_value_or_null(&ResponsesCodec::encode_response(ir))
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

        let back = encode_request(&ir);
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

        let back = encode_request(&ir);
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
        let back = encode_request(&ir);
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
        let back = encode_request(&ir);
        assert_eq!(back["parallel_tool_calls"], true);
        // effort re-encoded when not in raw... reasoning is in raw, restored.
        assert_eq!(back["reasoning"]["effort"], "high");
    }

    // ── D1 / 422 rejects ────────────────────────────────────────

    #[test]
    fn reject_previous_response_id() {
        let err = try_decode(json!({
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
        let err = try_decode(json!({
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
            try_decode(body).expect("store=false/absent must pass");
        }
    }

    #[test]
    fn reject_background() {
        let err = try_decode(json!({
            "model": "m",
            "background": true,
            "input": "hi"
        }))
        .unwrap_err();
        assert!(err.to_string().contains("background is not supported"));
        // background: false is tolerated (explicit non-background).
        try_decode(json!({
            "model": "m",
            "background": false,
            "input": "hi"
        }))
        .expect("background=false ok");
    }

    #[test]
    fn reject_hosted_tool_types() {
        for ty in ["hosted", "custom", "file_search"] {
            let err = try_decode(json!({
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
        let back = encode_request(&ir);
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
            let back = encode_request(&ir);
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
        let back = encode_request(&ir);
        assert_eq!(back["tool_choice"], complex, "re-emitted unchanged");

        // flat Responses-style function shape also passes through.
        let flat = json!({"type": "function", "name": "f"});
        let ir = decode(json!({
            "model": "m",
            "input": [],
            "tool_choice": flat
        }));
        assert_eq!(ir.tool_choice, flat, "flat shape kept opaque");
        let back = encode_request(&ir);
        assert_eq!(back["tool_choice"], flat, "flat re-emitted unchanged");

        // strings and simple objects still pass.
        for tc in [json!("auto"), json!("required"), json!({"type": "none"})] {
            try_decode(json!({
                "model": "m", "input": [], "tool_choice": tc
            }))
            .unwrap_or_else(|e| panic!("tool_choice {tc} must pass: {e}"));
        }

        // illegal JSON types still reject.
        for tc in [json!(1), json!(true), json!(["auto"])] {
            let err = try_decode(json!({
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
        let err = try_decode(json!({
            "model": "m",
            "input": "resp_1234567890abcdef"
        }))
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("previous response id is not supported")
        );
        // Ordinary prose input must pass.
        try_decode(json!({"model": "m", "input": "hello world"})).expect("prose input ok");
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

    /// A non-spec `{"type": "message"}` without `role` used to default to a
    /// user message; the typed DTO requires `role`, so the codec restores the
    /// default instead of dropping the content.
    #[test]
    fn message_item_without_role_defaults_to_user() {
        let ir = decode(json!({
            "model": "m",
            "input": [{"type": "message", "content": "hi"}]
        }));
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert_eq!(ir.messages.len(), 1);
        assert_eq!(ir.messages[0].role, IrRole::User);
        assert!(
            matches!(&ir.messages[0].content[0], IrContentBlock::Text { text } if text == "hi")
        );
    }

    #[test]
    fn message_item_with_non_string_role_defaults_to_user() {
        let ir = decode(json!({
            "model": "m",
            "input": [{"type": "message", "role": 123, "content": "hi"}]
        }));
        assert!(ir.loss.is_empty(), "loss: {:?}", ir.loss);
        assert_eq!(ir.messages.len(), 1);
        assert_eq!(ir.messages[0].role, IrRole::User);
        assert!(
            matches!(&ir.messages[0].content[0], IrContentBlock::Text { text } if text == "hi")
        );
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
        let ir = decode_response(body);
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

        let back = encode_response(&ir);
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
            let ir = decode_response(body);
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
        let ir = decode_response(body);
        assert!(!ir.loss.is_empty());
        assert!(matches!(ir.output[0], IrOutputItem::Raw(_)));
        // Raw round-trips on encode.
        let back = encode_response(&ir);
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
    fn usage_degrades_per_field_when_details_are_ill_typed() {
        // `cached_tokens` as a string fails the typed `ResponseUsage` parse;
        // the counters must still be read individually (not all-zero).
        let usage = responses_usage(&json!({
            "input_tokens": 10,
            "output_tokens": 20,
            "input_tokens_details": {"cached_tokens": "64"}
        }));
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 20);
        assert_eq!(usage.total_tokens, 30);
        assert_eq!(usage.cached_tokens, Some(0));
        assert_eq!(usage.reasoning_tokens, None);
    }

    #[test]
    fn usage_keeps_cached_tokens_unknown_when_input_is_zero() {
        // The `Some(0)` fallback only applies when `input_tokens` is known and
        // non-zero; with no input there is nothing to classify, so the value
        // stays `None` (unknown) for the analytics rollup.
        let usage = responses_usage(&json!({
            "input_tokens": 0,
            "output_tokens": 0,
            "input_tokens_details": {"cached_tokens": "64"}
        }));
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.cached_tokens, None);
    }

    #[test]
    fn stream_completed_keeps_usage_when_details_are_ill_typed() {
        let events = ResponsesCodec::decode_stream_frame(&frame(json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1", "status": "completed",
                "usage": {
                    "input_tokens": 7, "output_tokens": 3,
                    "output_tokens_details": {"reasoning_tokens": "3"}
                }
            }
        })));
        assert!(matches!(events[1], IrStreamEvent::Usage(ref u)
            if u.prompt_tokens == 7
                && u.completion_tokens == 3
                && u.total_tokens == 10
                && u.reasoning_tokens.is_none()));
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
    fn stream_finish_length_emits_incomplete_with_max_output_tokens() {
        let enc = ResponsesCodec::stream_encoder();
        let s = String::from_utf8(enc(&IrStreamEvent::Finish {
            index: 0,
            finish_reason: Some(IrFinishReason::Length),
        }))
        .unwrap();
        assert!(s.contains("\"type\":\"response.incomplete\""), "{s}");
        assert!(s.contains("\"status\":\"incomplete\""), "{s}");
        assert!(
            s.contains("\"incomplete_details\":{\"reason\":\"max_output_tokens\"}"),
            "{s}"
        );
        assert!(!s.contains("response.completed"), "{s}");
    }

    #[test]
    fn stream_finish_content_filter_emits_incomplete_with_content_filter() {
        let enc = ResponsesCodec::stream_encoder();
        let s = String::from_utf8(enc(&IrStreamEvent::Finish {
            index: 0,
            finish_reason: Some(IrFinishReason::ContentFilter),
        }))
        .unwrap();
        assert!(s.contains("\"type\":\"response.incomplete\""), "{s}");
        assert!(s.contains("\"status\":\"incomplete\""), "{s}");
        assert!(
            s.contains("\"incomplete_details\":{\"reason\":\"content_filter\"}"),
            "{s}"
        );
        assert!(!s.contains("response.completed"), "{s}");
    }

    #[test]
    fn stream_finish_stop_emits_completed_without_incomplete_details() {
        let enc = ResponsesCodec::stream_encoder();
        let s = String::from_utf8(enc(&IrStreamEvent::Finish {
            index: 0,
            finish_reason: Some(IrFinishReason::Stop),
        }))
        .unwrap();
        assert!(s.contains("\"type\":\"response.completed\""), "{s}");
        assert!(s.contains("\"status\":\"completed\""), "{s}");
        assert!(!s.contains("incomplete_details"), "{s}");
    }

    /// The stateless `encode_stream_event` view and the stateful
    /// `stream_encoder` closure must emit byte-identical terminal frames.
    #[test]
    fn stream_finish_terminal_frame_is_shared_by_both_entry_points() {
        let cases = [
            Some(IrFinishReason::Length),
            Some(IrFinishReason::ContentFilter),
            Some(IrFinishReason::Stop),
            Some(IrFinishReason::ToolCalls),
            Some(IrFinishReason::Cancelled),
            Some(IrFinishReason::Other("failed".into())),
            Some(IrFinishReason::Other("weird".into())),
            None,
        ];
        for finish_reason in cases {
            let ev = IrStreamEvent::Finish {
                index: 0,
                finish_reason: finish_reason.clone(),
            };
            let stateless = ResponsesCodec::encode_stream_event(&ev);
            let stateful = ResponsesCodec::stream_encoder()(&ev);
            assert_eq!(
                stateless, stateful,
                "terminal frames diverged for {finish_reason:?}"
            );
        }
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
        let ir = ResponsesCodec::decode_request_allow_stateful(&request(body))
            .expect("Allow mode must accept previous_response_id/store=true");
        assert_eq!(ir.raw.get("previous_response_id"), Some(&json!("resp_abc")));
        assert_eq!(ir.raw.get("store"), Some(&json!(true)));
        let back = encode_request(&ir);
        assert_eq!(back["previous_response_id"], "resp_abc");
        assert_eq!(back["store"], true);
    }

    #[test]
    fn allow_stateful_still_rejects_background() {
        let err = allow_stateful(json!({
            "model": "m",
            "background": true,
            "input": "hi"
        }))
        .unwrap_err();
        assert!(err.to_string().contains("background is not supported"));
    }

    #[test]
    fn reject_mode_still_rejects_stateful() {
        let err = try_decode(json!({
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
        let back = encode_request(&ir);
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
        let back = encode_request(&ir);
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
        let back = encode_request(&ir);
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
        let back = encode_request(&ir);
        let tool = &back["tools"][0];
        assert_eq!(tool["type"], json!("function"));
        assert!(tool.get("parameters").is_none());
        for k in tool.as_object().expect("tool object").keys() {
            assert!(!tool_raw::is_reserved_key(k), "reserved marker leaked: {k}");
        }
    }

    // ── typed DTO boundary (R2 / R4 / R5) ───────────────────────

    /// The handler runs the semantic pre-check on the **raw** JSON before
    /// typed parsing, so these messages and their ordering must not change.
    #[test]
    fn reject_unsupported_keeps_value_layer_messages() {
        for (body, message) in [
            (
                json!({"previous_response_id": "resp_abc"}),
                "previous_response_id is not supported",
            ),
            (json!({"store": true}), "store=true is not supported"),
            (json!({"store": "true"}), "store=true is not supported"),
            (json!({"store": "TRUE"}), "store=true is not supported"),
            (json!({"background": true}), "background is not supported"),
            (json!({"background": "true"}), "background is not supported"),
            (
                json!({"input": "resp_1234567890abcdef"}),
                "previous response id is not supported",
            ),
        ] {
            let err = reject_unsupported(&body).unwrap_err();
            assert!(matches!(err, AppError::BadRequest(_)), "{body}");
            assert!(err.to_string().contains(message), "{body}: {err}");
            // The typed codec path re-checks the same fields.
            let err = ResponsesCodec::decode_request(&request(body.clone())).unwrap_err();
            assert!(err.to_string().contains(message), "typed {body}: {err}");
        }

        // Allow mode accepts the stateful pair but still rejects `background`.
        reject_unsupported_mode(
            &json!({"store": true, "previous_response_id": "resp_1"}),
            StatefulMode::Allow,
        )
        .expect("allow mode accepts stateful fields");
        assert!(
            reject_unsupported_mode(&json!({"background": true}), StatefulMode::Allow).is_err()
        );
    }

    /// Typed fields without an IR home round-trip through `raw` instead of
    /// being dropped by the old allow-list filter.
    #[test]
    fn request_typed_echo_fields_roundtrip_through_raw() {
        let typed = request(json!({
            "model": "m",
            "input": "hi",
            "metadata": {"k": "v"},
            "service_tier": "flex",
            "include": ["reasoning.encrypted_content"],
            "parallel_tool_calls": true,
            "prompt_cache_key": "pk-1"
        }));
        assert!(
            typed.extra.is_empty(),
            "all fields are typed: {:?}",
            typed.extra
        );
        let ir = ResponsesCodec::decode_request(&typed).expect("decode");
        assert_eq!(ir.raw.get("service_tier"), Some(&json!("flex")));
        let back = encode_request(&ir);
        assert_eq!(back["metadata"], json!({"k": "v"}));
        assert_eq!(back["service_tier"], "flex");
        assert_eq!(back["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(back["parallel_tool_calls"], true);
        assert_eq!(back["prompt_cache_key"], "pk-1");
    }

    /// `phase` / `status` / `id` are typed on `MessageItem` but have no IR
    /// home — they ride `IrMessage.raw` so encode re-emits them.
    #[test]
    fn input_message_typed_extras_roundtrip() {
        let ir = decode(json!({
            "model": "m",
            "input": [{
                "type": "message", "role": "user", "content": "hi",
                "phase": "final_answer", "status": "completed", "id": "msg_1"
            }]
        }));
        assert_eq!(
            ir.messages[0].raw.get("phase"),
            Some(&json!("final_answer"))
        );
        assert_eq!(ir.messages[0].raw.get("id"), Some(&json!("msg_1")));
        let back = encode_request(&ir);
        assert_eq!(back["input"][0]["phase"], "final_answer");
        assert_eq!(back["input"][0]["id"], "msg_1");
        assert_eq!(back["input"][0]["status"], "completed");
    }

    /// `ToolChoiceAllowed` is the union's object catch-all and would grow an
    /// `allowed_tools: []` field; both directions must keep the shape intact.
    #[test]
    fn tool_choice_allowed_tools_shape_preserved() {
        let allowed = json!({
            "type": "allowed_tools",
            "mode": "auto",
            "allowed_tools": [{"type": "function", "name": "f"}],
        });
        let ir = decode(json!({"model": "m", "input": [], "tool_choice": allowed}));
        assert_eq!(ir.tool_choice, allowed);
        let back = encode_request(&ir);
        assert_eq!(back["tool_choice"], allowed);
    }

    /// Response echoes the IR does not synthesize survive via `raw`, including
    /// the deprecated `truncation` that now rides `extra`.
    #[test]
    fn response_echo_fields_and_incomplete_details_roundtrip() {
        let ir = decode_response(json!({
            "id": "resp_1", "object": "response", "created_at": 7, "completed_at": 9,
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "model": "m",
            "output": [{"type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": "t", "annotations": []}]}],
            "output_text": "t",
            "service_tier": "flex",
            "truncation": "auto",
            "usage": {"input_tokens": 2, "output_tokens": 1}
        }));
        assert_eq!(ir.finish_reason, Some(IrFinishReason::Length));
        assert_eq!(ir.usage.as_ref().map(|u| u.total_tokens), Some(3));
        let back = encode_response(&ir);
        assert_eq!(back["status"], "incomplete");
        assert_eq!(
            back["incomplete_details"],
            json!({"reason": "max_output_tokens"})
        );
        assert_eq!(back["created_at"], 7);
        assert_eq!(back["completed_at"], 9);
        assert_eq!(back["output_text"], "t");
        assert_eq!(back["service_tier"], "flex");
        assert_eq!(back["truncation"], "auto");
        assert_eq!(back["usage"]["total_tokens"], 3);
    }

    /// R4: typed parsing turns what used to be a silent IR loss into a 422 at
    /// the handler boundary, while unknown fields still pass through `extra`.
    #[test]
    fn malformed_values_are_parse_errors_but_unknown_fields_pass() {
        assert!(
            serde_json::from_value::<ResponsesRequest>(json!({
                "model": "m", "input": "hi", "temperature": "0.7"
            }))
            .is_err(),
            "temperature must be a number"
        );
        // Unions stay lossless: a variant whose fields do not parse falls back
        // to `Unknown` instead of failing the whole body (R1) …
        let typed: ResponsesResponse = serde_json::from_value(json!({
            "id": "r", "output": [{"type": "function_call", "call_id": "c",
                                  "name": "f", "arguments": {"a": 1}}]
        }))
        .expect("union falls back to Unknown");
        assert!(matches!(typed.output[0], OutputItem::Unknown(_)));
        // … and the codec records a loss + raw item instead of accepting the
        // object as `arguments` verbatim (R4: typed narrowing).
        let ir = ResponsesCodec::decode_response(&typed);
        assert!(matches!(ir.output[0], IrOutputItem::Raw(_)));
        assert!(!ir.loss.is_empty());
        assert!(
            serde_json::from_value::<ResponsesRequest>(json!({
                "model": "m", "input": "hi", "brand_new_field": {"x": 1}
            }))
            .is_ok(),
            "unknown fields are tolerated"
        );

        // A message whose `content` is a scalar (non-spec upstream shape) no
        // longer decodes to `IrOutputItem::Text`: it is carried verbatim as a
        // raw item plus a loss, so the client-visible bytes are unchanged.
        let ir = decode_response(json!({
            "id": "r", "status": "completed", "model": "m",
            "output": [{"type": "message", "role": "assistant", "content": "t"}]
        }));
        assert!(matches!(ir.output[0], IrOutputItem::Raw(_)));
        assert!(!ir.loss.is_empty());
        assert_eq!(encode_response(&ir)["output"][0]["content"], "t");
    }
}
