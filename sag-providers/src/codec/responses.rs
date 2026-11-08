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
use std::collections::{BTreeMap, HashMap, HashSet};

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

    /// Decode a single Responses SSE frame into 0..n IR stream events,
    /// statelessly: a fresh [`StreamDecodeState`] is used per call.
    ///
    /// State machine (plan §5.2):
    /// `response.created → response.in_progress → response.output_item.added
    ///  → response.content_part.added → delta → done → response.completed /
    ///  response.failed`. Known bookkeeping types (`response.in_progress`,
    /// `response.content_part.*`, `response.output_text.done`,
    /// `response.function_call_arguments.done`, `response.reasoning_*`
    /// terminals, ...) decode to **no event at all**: they carry nothing the
    /// IR needs, and passing them through as `Raw` would leak Responses
    /// frames onto a different-protocol egress (e.g. an Anthropic client).
    /// Only frames that cannot be classified (non-JSON payload, SSE
    /// comments, unknown `type` strings) become `Raw`.
    ///
    /// Prefer [`Self::stream_decoder`] for a real stream: the per-call state
    /// cannot tell whether an item's payload already arrived as deltas, so a
    /// `response.output_item.done` seen in its own frame is treated as the
    /// sole supplier (see [`StreamDecodeState`]).
    pub fn decode_stream_frame(frame: &[u8]) -> Vec<IrStreamEvent> {
        StreamDecodeState::default().decode(frame)
    }

    /// Build a **stateful** upstream stream decoder that remembers which
    /// `output_index` values already delivered their payload through deltas.
    ///
    /// Without it, `response.output_item.done` is either blindly dropped (a
    /// `function_call` whose `arguments` never delta-streamed loses them) or
    /// blindly re-emitted (a client accumulating deltas then double-applies
    /// the payload). Returns a `FnMut` suitable for a stream combinator.
    pub fn stream_decoder() -> impl FnMut(&[u8]) -> Vec<IrStreamEvent> + Send {
        let mut state = StreamDecodeState::default();
        move |frame| state.decode(frame)
    }

    /// Decode one frame against `state`; the shared body of the stateless
    /// wrapper and the stateful [`Self::stream_decoder`].
    fn decode_frame(state: &mut StreamDecodeState, frame: &[u8]) -> Vec<IrStreamEvent> {
        let text = String::from_utf8_lossy(frame);
        let mut events = Vec::new();
        let mut saw_data = false;
        // A multi-line `data:` event is one SSE frame, so it may contribute
        // **at most one** verbatim `Raw` copy of those bytes. Without the flag
        // each unparseable payload line pushed the whole frame again, and two
        // malformed lines produced two identical `Raw` events (duplicated
        // foreign bytes on the egress).
        let mut raw_emitted = false;

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
                push_frame_raw(&mut events, &mut raw_emitted, frame);
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
                push_frame_raw(&mut events, &mut raw_emitted, frame);
                continue;
            }

            let Some(event_type) = val.get("type").and_then(|t| t.as_str()) else {
                push_frame_raw(&mut events, &mut raw_emitted, frame);
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
                    // Remember the payload so `output_item.done` can tell
                    // whether the item was already started.
                    state.open_items.insert(index, item.clone());
                    // Only `function_call` carries IR content at item-open.
                    // `message` / `reasoning` just announce the item (content
                    // deltas follow) and any other item type (e.g. hosted tool
                    // calls) is bookkeeping whose payload the IR never consumes
                    // — emitting `Raw` for those would leak a Responses frame
                    // onto a foreign-protocol egress.
                    if let Some("function_call") = item.get("type").and_then(|t| t.as_str()) {
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
                            state.deltas_seen.insert(index);
                        }
                    }
                }
                "response.output_item.done" => {
                    let index = val
                        .get("output_index")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as u32;
                    let item = val.get("item").cloned().unwrap_or(Value::Null);
                    let added = state.open_items.remove(&index);
                    // Round-trip invariant: every item's payload reaches IR
                    // exactly once — `done` is the sole supplier only when
                    // no delta was streamed for this index, and the deltas
                    // are the sole supplier otherwise (`done` is an
                    // authoritative replacement, not an addition, so it
                    // must stay silent once deltas were delivered). A
                    // replayed `done` (SSE reconnect / chunk replay) is the
                    // same replacement, so it must stay silent too.
                    let first_done = state.closed_items.insert(index);
                    if !state.deltas_seen.contains(&index) && first_done {
                        match item.get("type").and_then(|t| t.as_str()) {
                            Some("function_call") => {
                                // `added` is normally the start carrier; when
                                // upstream skipped it, start here instead so
                                // the IR shape is identical either way.
                                let field = |key: &str| {
                                    item.get(key)
                                        .or_else(|| added.as_ref().and_then(|a| a.get(key)))
                                        .and_then(|v| v.as_str())
                                };
                                if added.is_none() {
                                    events.push(IrStreamEvent::ToolCallStart {
                                        index,
                                        id: field("call_id")
                                            .or_else(|| field("id"))
                                            .map(String::from),
                                        name: field("name").map(String::from),
                                    });
                                }
                                if let Some(args) = field("arguments")
                                    && !args.is_empty()
                                {
                                    events.push(IrStreamEvent::ToolCallDelta {
                                        index,
                                        arguments_delta: args.to_string(),
                                    });
                                }
                            }
                            Some("message") => {
                                let text = item_part_text(&item, "content");
                                if !text.is_empty() {
                                    events.push(IrStreamEvent::TextDelta { index, text });
                                }
                            }
                            Some("reasoning") => {
                                let mut text = item_part_text(&item, "summary");
                                if text.is_empty() {
                                    text = item_part_text(&item, "content");
                                }
                                if !text.is_empty() {
                                    events.push(IrStreamEvent::ReasoningDelta { index, text });
                                }
                            }
                            // Unknown / empty item: bookkeeping only.
                            _ => {}
                        }
                    }
                }
                // Part / text terminals: no IR event. Part text is delivered
                // by the delta frames and `output_item.done` carries the
                // complete item, so reconstructing from a terminal frame here
                // would emit the same text twice. Decoding them to nothing (not
                // `Raw`) keeps them off a foreign-protocol egress.
                "response.content_part.added"
                | "response.content_part.done"
                | "response.output_text.done"
                | "response.function_call_arguments.done" => {}
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
                        state.deltas_seen.insert(index);
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
                        state.deltas_seen.insert(index);
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
                        state.deltas_seen.insert(index);
                    }
                }
                // Reasoning terminals (`response.reasoning_summary_text.done`,
                // `response.reasoning_text.done`, part terminals) carry the
                // already-streamed text — the deltas are the sole supplier, so
                // these decode to nothing instead of `Raw`. Annotations likewise
                // have no IR slot (citations stay upstream-only).
                "response.reasoning_summary_text.done"
                | "response.reasoning_text.done"
                | "response.reasoning_summary_part.added"
                | "response.reasoning_summary_part.done"
                | "response.output_text.annotation.added" => {}
                "response.completed" => {
                    // Terminal of a compliant turn: derive the IR `Finish`
                    // from `status` (+ `incomplete_details`) with the same
                    // mapping the non-streaming decode uses. Without it a
                    // translated egress (Chat client) never sees
                    // `finish_reason` and the stream looks truncated — the
                    // `Raw` frame below cannot rescue it, because the
                    // dispatch filter drops `response.*` frames on an
                    // IR-decoded lane. `has_function_call` matters: a turn
                    // that ends on tool calls must report `tool_calls`, not
                    // `stop`, so scan `output[]` for a `function_call` item.
                    let has_function_call = val
                        .pointer("/response/output")
                        .and_then(|o| o.as_array())
                        .is_some_and(|items| {
                            items.iter().any(|item| {
                                item.get("type").and_then(|t| t.as_str()) == Some("function_call")
                            })
                        });
                    let incomplete_reason = val
                        .pointer("/response/incomplete_details/reason")
                        .and_then(|r| r.as_str());
                    // `completed` is the status implied by the event name, so
                    // a payload that omits `/response/status` still yields a
                    // reason: `tool_calls` when the output carries a call,
                    // `stop` otherwise.
                    let finish_reason =
                        match val.pointer("/response/status").and_then(|s| s.as_str()) {
                            Some(status) => {
                                status_to_finish(status, incomplete_reason, has_function_call)
                            }
                            None => Some(if has_function_call {
                                IrFinishReason::FunctionCall
                            } else {
                                IrFinishReason::Stop
                            }),
                        };
                    // `status_to_finish` keeps the Responses-native token
                    // `function_call`; the IR stream terminal for a tool-call
                    // turn is `tool_calls` (what the Chat and Anthropic
                    // encoders render), so fold it here instead of adding a
                    // second status→reason table.
                    let finish_reason = finish_reason.map(|reason| match reason {
                        IrFinishReason::FunctionCall => IrFinishReason::ToolCalls,
                        other => other,
                    });
                    events.push(IrStreamEvent::Finish {
                        index: 0,
                        finish_reason,
                    });
                    if let Some(u) = val.pointer("/response/usage").filter(|u| !u.is_null()) {
                        events.push(IrStreamEvent::Usage(responses_usage(u)));
                    }
                    // Keep forwarding the original frame: on the same-protocol
                    // lane it is the authoritative terminal, and the
                    // IR-decoded lanes drop every Responses-decoder `Raw`
                    // (they carry Responses event names).
                    push_frame_raw(&mut events, &mut raw_emitted, frame);
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
                    push_frame_raw(&mut events, &mut raw_emitted, frame);
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
                    push_frame_raw(&mut events, &mut raw_emitted, frame);
                }
                _ => {
                    push_frame_raw(&mut events, &mut raw_emitted, frame);
                }
            }
        }

        if !saw_data && events.is_empty() && !text.trim().is_empty() {
            push_frame_raw(&mut events, &mut raw_emitted, frame);
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
                // Same id scheme as the stateful encoder: the spec `id` is
                // derived from the output index, the IR call id is `call_id`.
                let item = json!({
                    "type": "function_call",
                    "id": format!("fc_{index}"),
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
                // passes the upstream frame through as Raw). The stateless
                // view carries no item history.
                encode_terminal_event(finish_reason.as_ref(), &[])
            }
            // Usage / End are handled inline by the dispatch loop.
            IrStreamEvent::Usage(_) | IrStreamEvent::End => Vec::new(),
            // Mid-stream upstream error (translated lane) or a gateway-local
            // interruption (passthrough lane): the Responses wire form is the
            // `{"type":"error"}` event, after which the stream terminates —
            // no `response.completed` may follow.
            IrStreamEvent::Error(msg) => encode_error_event(msg),
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
// stream decoder state
// ═══════════════════════════════════════════════════════════════════

/// Push the verbatim copy of `frame` at most once per decoded frame.
///
/// A single SSE event may span several `data:` lines; the decoder parses them
/// one by one, so without this guard every unclassifiable line pushed the whole
/// frame again and the client received the same foreign bytes twice.
fn push_frame_raw(events: &mut Vec<IrStreamEvent>, raw_emitted: &mut bool, frame: &[u8]) {
    if *raw_emitted {
        return;
    }
    *raw_emitted = true;
    events.push(IrStreamEvent::Raw(frame.to_vec()));
}

/// Per-stream decode state: which item indices already delivered their
/// payload through deltas, and the `output_item.added` payloads still open.
///
/// Invariant: **every item's payload reaches IR exactly once** — `done` is
/// the sole supplier only when deltas were absent, and the deltas are the
/// sole supplier otherwise.
#[derive(Default)]
struct StreamDecodeState {
    /// `output_index` values whose content was already emitted as deltas.
    deltas_seen: HashSet<u32>,
    /// `response.output_item.added` payloads, keyed by `output_index`.
    open_items: HashMap<u32, Value>,
    /// `output_index` values whose `response.output_item.done` was already
    /// processed, so a replayed `done` frame cannot re-deliver a payload.
    closed_items: HashSet<u32>,
}

impl StreamDecodeState {
    fn decode(&mut self, frame: &[u8]) -> Vec<IrStreamEvent> {
        ResponsesCodec::decode_frame(self, frame)
    }
}

/// Concatenate the `text` fields of an item's `content` / `summary` parts.
fn item_part_text(item: &Value, field: &str) -> String {
    item.get(field)
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<String>()
        })
        .unwrap_or_default()
}

// ═══════════════════════════════════════════════════════════════════
// stream encoder state
// ═══════════════════════════════════════════════════════════════════

/// Per-stream encode state: the single open non-tool item plus the set of
/// concurrently-open tool calls.
///
/// Invariant: **every item's payload reaches `output` exactly once** — each
/// item is closed by exactly one `output_item.done` and inserted into
/// `completed_items` exactly once.
#[derive(Default)]
struct StreamEncodeState {
    started: bool,
    /// Kind of the currently open non-tool output item, if any
    /// (`message` / `reasoning` — they are never parallel, so they keep the
    /// single-slot treatment).
    open_item: Option<OpenItem>,
    /// Index of the currently open non-tool output item.
    open_index: u32,
    next_index: u32,
    completed: bool,
    /// Tool calls that were announced but never closed, keyed by the IR
    /// tool-call index (chat upstreams stream parallel calls with distinct
    /// indices in one `delta`). Each entry owns its own `output_index` and
    /// argument buffer, so interleaved deltas cannot cross-contaminate.
    open_tool_calls: HashMap<u32, OpenToolCall>,
    /// Text accumulated from text / reasoning deltas for the open item.
    open_text: String,
    /// Items already closed, keyed by `output_index`; the terminal frame emits
    /// them in ascending index order.
    completed_items: BTreeMap<u32, Value>,
    /// IR tool-call indices whose item was already committed by a terminal
    /// close. A late `ToolCallDelta` for one of these is dropped instead of
    /// resurrecting the call as a second `function_call` item.
    closed_tool_calls: HashSet<u32>,
    /// Set once an `Error` frame was rendered: the stream terminated at it, so
    /// no later `output_item.done` / `response.completed` / upstream `Raw` may
    /// be emitted.
    errored: bool,
}

/// One concurrently-open `function_call` output item.
struct OpenToolCall {
    /// Responses `output_index` allocated when the item was opened.
    output_index: u32,
    /// IR call id, repeated as `call_id` by `added` / `done`.
    call_id: String,
    name: String,
    /// Arguments accumulated from `ToolCallDelta` for this call only.
    arguments: String,
}

#[derive(Clone, Copy, PartialEq)]
enum OpenItem {
    Message,
    Reasoning,
}

impl StreamEncodeState {
    fn encode(&mut self, ev: &IrStreamEvent) -> Vec<u8> {
        match ev {
            IrStreamEvent::Raw(bytes) => {
                // Defense in depth: a translated stream terminated at its
                // error frame, so no later raw frame may reach this egress.
                if self.errored {
                    return Vec::new();
                }
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
                self.errored = false;
                self.open_tool_calls.clear();
                self.closed_tool_calls.clear();
                self.open_text.clear();
                self.completed_items.clear();
                let _ = meta;
                ResponsesCodec::encode_stream_event(ev)
            }
            IrStreamEvent::TextDelta { text, .. } => {
                let mut out = self.ensure_started();
                // An open tool call stays open: it is committed once at the
                // stream terminal, so a text delta cannot split it.
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
                self.open_text.push_str(text);
                out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                out
            }
            IrStreamEvent::ReasoningDelta { text, .. } => {
                let mut out = self.ensure_started();
                // An open tool call stays open: it is committed once at the
                // stream terminal, so a reasoning delta cannot split it.
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
                self.open_text.push_str(text);
                out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                out
            }
            IrStreamEvent::ToolCallStart { index, id, name } => {
                let mut out = self.ensure_started();
                // Idempotent re-open: the item already announced itself, so it
                // keeps its `output_index` and no second `added` is emitted.
                if self.open_tool_calls.contains_key(index) {
                    return out;
                }
                // An open message / reasoning item stays open: it is
                // committed at the stream terminal (or when a text delta
                // switches reasoning → message), never by a tool call.
                // An explicit restart also clears the closed marker so the
                // deltas that follow are not mistaken for a stale replay.
                self.closed_tool_calls.remove(index);
                let output_index = self.next_index;
                let call_id = id.clone().unwrap_or_default();
                let item = json!({
                    "type": "function_call",
                    "id": format!("fc_{output_index}"),
                    "call_id": call_id,
                    "name": name.clone().unwrap_or_default(),
                    "arguments": "",
                    "status": "in_progress",
                });
                let payload = json!({
                    "type": "response.output_item.added",
                    "output_index": output_index,
                    "item": item,
                });
                out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                self.next_index += 1;
                self.open_tool_calls.insert(
                    *index,
                    OpenToolCall {
                        output_index,
                        call_id,
                        name: name.clone().unwrap_or_default(),
                        arguments: String::new(),
                    },
                );
                out
            }
            IrStreamEvent::ToolCallDelta {
                index,
                arguments_delta,
            } => {
                let mut out = self.ensure_started();
                // A fragment for a call already committed by the terminal
                // close is dropped: minting a fresh `output_index` here would
                // resurrect the item as a duplicate `function_call` in
                // `response.completed.output`.
                if self.closed_tool_calls.contains(index) {
                    return out;
                }
                // Defensive: open a function_call item if upstream skipped the
                // start frame for this index (never panic, never drop delta).
                if !self.open_tool_calls.contains_key(index) {
                    out.extend_from_slice(&self.encode(&IrStreamEvent::ToolCallStart {
                        index: *index,
                        id: None,
                        name: None,
                    }));
                }
                let Some(call) = self.open_tool_calls.get_mut(index) else {
                    return out;
                };
                let output_index = call.output_index;
                call.arguments.push_str(arguments_delta);
                let payload = json!({
                    "type": "response.function_call_arguments.delta",
                    "output_index": output_index,
                    "delta": arguments_delta,
                });
                out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                out
            }
            IrStreamEvent::Finish { finish_reason, .. } => {
                // The error frame already terminated the stream: no
                // `output_item.done` and no `response.completed` may follow it.
                if self.errored {
                    return Vec::new();
                }
                let mut out = self.close_open_tool_calls();
                out.extend_from_slice(&self.close_open_item());
                if !self.completed {
                    out.extend_from_slice(&encode_terminal_event(
                        finish_reason.as_ref(),
                        &self.completed_output(),
                    ));
                    self.completed = true;
                }
                self.completed_items.clear();
                out
            }
            IrStreamEvent::End => {
                // The error frame already terminated the stream: the open item
                // is deliberately left unclosed (exactly like a native
                // Responses error stream, whose last frame is the error).
                if self.errored {
                    return Vec::new();
                }
                // A stream closing with an item still open must flush it so
                // the terminal payload is complete.
                if self.open_item.is_none() && self.open_tool_calls.is_empty() {
                    return Vec::new();
                }
                let mut out = self.close_open_tool_calls();
                out.extend_from_slice(&self.close_open_item());
                if !self.completed {
                    out.extend_from_slice(&encode_terminal_event(None, &self.completed_output()));
                    self.completed = true;
                }
                self.completed_items.clear();
                out
            }
            IrStreamEvent::Usage(_) => Vec::new(),
            // A mid-stream upstream error (translated lane) or a
            // gateway-local interruption (passthrough lane): one
            // `{"type":"error"}` event, after which nothing else may be
            // emitted. The open item is deliberately left unclosed.
            IrStreamEvent::Error(msg) => {
                self.errored = true;
                encode_error_event(msg)
            }
        }
    }

    /// Closed items in ascending `output_index` order (BTreeMap iteration).
    fn completed_output(&self) -> Vec<Value> {
        self.completed_items.values().cloned().collect()
    }

    fn ensure_started(&mut self) -> Vec<u8> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        ResponsesCodec::encode_stream_event(&IrStreamEvent::Start(IrResponseMeta::default()))
    }

    /// Close the open non-tool item (`message` / `reasoning`), if any.
    ///
    /// Tool calls are not single-slot: they are flushed by
    /// [`Self::close_open_tool_calls`].
    fn close_open_item(&mut self) -> Vec<u8> {
        let Some(kind) = self.open_item.take() else {
            return Vec::new();
        };
        let index = self.open_index;
        let mut out = Vec::new();
        // `done` repeats the full item the `added` frame announced — OpenAI's
        // real stream does the same, so a client that accumulates deltas
        // treats `done` as an authoritative replacement (never an addition).
        let item = match kind {
            OpenItem::Message => {
                let text = std::mem::take(&mut self.open_text);
                let payload = json!({
                    "type": "response.content_part.done",
                    "output_index": index,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": text, "annotations": []},
                });
                out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
                json!({
                    "type": "message",
                    "id": format!("msg_{index}"),
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": text, "annotations": []}],
                })
            }
            OpenItem::Reasoning => {
                let text = std::mem::take(&mut self.open_text);
                let summary = if text.is_empty() {
                    json!([])
                } else {
                    json!([{"type": "summary_text", "text": text}])
                };
                json!({
                    "type": "reasoning",
                    "id": format!("rs_{index}"),
                    "summary": summary,
                    "status": "completed",
                })
            }
        };
        self.completed_items.insert(index, item.clone());
        let payload = json!({
            "type": "response.output_item.done",
            "output_index": index,
            "item": item,
        });
        out.extend_from_slice(format!("data: {payload}\n\n").as_bytes());
        out
    }

    /// Close every open tool call, in ascending `output_index` order.
    ///
    /// The map is drained so a closed call can never leak into a later item,
    /// each item reaches `completed_items` exactly once, and each IR index is
    /// recorded in `closed_tool_calls` so later fragments cannot resurrect it.
    fn close_open_tool_calls(&mut self) -> Vec<u8> {
        if self.open_tool_calls.is_empty() {
            return Vec::new();
        }
        let mut open: Vec<(u32, OpenToolCall)> = self.open_tool_calls.drain().collect();
        open.sort_by_key(|(_, call)| call.output_index);
        let mut out = Vec::new();
        for (ir_index, call) in open {
            self.closed_tool_calls.insert(ir_index);
            out.extend_from_slice(&self.close_tool_call(call));
        }
        out
    }

    /// Commit one tool call as an `output_item.done` carrying its full item.
    fn close_tool_call(&mut self, call: OpenToolCall) -> Vec<u8> {
        let index = call.output_index;
        // `done` repeats the full item the `added` frame announced — OpenAI's
        // real stream does the same, so a client that accumulates deltas
        // treats `done` as an authoritative replacement (never an addition).
        let item = json!({
            "type": "function_call",
            "id": format!("fc_{index}"),
            "call_id": call.call_id,
            "name": call.name,
            "arguments": call.arguments,
            "status": "completed",
        });
        self.completed_items.insert(index, item.clone());
        let payload = json!({
            "type": "response.output_item.done",
            "output_index": index,
            "item": item,
        });
        format!("data: {payload}\n\n").into_bytes()
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

/// Encode a mid-stream error as the Responses SSE error event.
///
/// This is the **only** client-visible frame an errored Responses egress
/// produces: the dispatch loop drops every later event and the encoder
/// suppresses its own terminal, so no `response.completed` follows it.
///
/// Shape: the protocol event (`type` / `code` / `message` / `param`) plus the
/// gateway's generic `{"error":{"code","message"}}` envelope inside the same
/// payload, so a client reading either convention finds the upstream message
/// and the stable `STREAM_ERROR` marker.
fn encode_error_event(message: &str) -> Vec<u8> {
    let payload = json!({
        "type": "error",
        "code": "STREAM_ERROR",
        "message": message,
        "param": Value::Null,
        "error": {"code": "STREAM_ERROR", "message": message},
    });
    format!("data: {payload}\n\n").into_bytes()
}

/// Encode the synthesized terminal SSE frame for a finish reason.
///
/// `completed` is the closed items in ascending `output_index` order, so the
/// terminal payload mirrors OpenAI's real `response.completed.output`.
fn encode_terminal_event(finish: Option<&IrFinishReason>, completed: &[Value]) -> Vec<u8> {
    let (event_type, status, incomplete_reason) = terminal_event_fields(finish);
    let mut response = json!({"object": "response", "status": status, "output": completed});
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
        // `Finish` now precedes `Usage` / `Raw` (the translated lanes need
        // the terminal reason; see the `response.completed` arm).
        assert!(matches!(events[0], IrStreamEvent::Finish { .. }));
        assert!(matches!(events[1], IrStreamEvent::Usage(ref u)
            if u.prompt_tokens == 7 && u.completion_tokens == 3));
        let raw = match &events[2] {
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

    // ── error frame (Responses egress) ───────────────────────────

    /// Every `data:` payload of an encoded SSE buffer, parsed as JSON.
    fn data_payloads(bytes: &[u8]) -> Vec<Value> {
        String::from_utf8_lossy(bytes)
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|payload| serde_json::from_str::<Value>(payload).ok())
            .collect()
    }

    /// A mid-stream `Error` must render exactly one Responses error event: the
    /// stateless view and the stateful encoder agree, the payload carries the
    /// upstream message plus a stable `code`, and nothing — no
    /// `response.completed`, no open item's `output_item.done` — may follow it.
    #[test]
    fn stream_encoder_error_renders_one_error_event_and_suppresses_terminal() {
        let ev = IrStreamEvent::Error("upstream exploded".into());
        let stateless = ResponsesCodec::encode_stream_event(&ev);
        let enc = ResponsesCodec::stream_encoder();
        // An open item must not be closed by the error frame.
        let mut out = enc(&IrStreamEvent::Start(IrResponseMeta::default()));
        out.extend(enc(&IrStreamEvent::TextDelta {
            index: 0,
            text: "partial".into(),
        }));
        let error = enc(&ev);
        assert_eq!(
            error, stateless,
            "both entry points must render the same error frame"
        );
        out.extend(error);
        // Everything after the error frame must be suppressed.
        out.extend(enc(&IrStreamEvent::Finish {
            index: 0,
            finish_reason: Some(IrFinishReason::Stop),
        }));
        out.extend(enc(&IrStreamEvent::Raw(
            b"data: {\"type\":\"response.completed\"}\n\n".to_vec(),
        )));
        out.extend(enc(&IrStreamEvent::End));

        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text.matches("\"type\":\"error\"").count(),
            1,
            "exactly one error frame: {text}"
        );
        assert!(
            !text.contains("response.completed") && !text.contains("response.output_item.done"),
            "nothing may follow the error frame: {text}"
        );
        let payload = data_payloads(text.as_bytes())
            .into_iter()
            .find(|v| v.get("type").and_then(Value::as_str) == Some("error"))
            .expect("an error payload");
        assert_eq!(payload["message"], "upstream exploded");
        assert_eq!(payload["code"], "STREAM_ERROR");
        assert_eq!(payload["error"]["code"], "STREAM_ERROR");
        assert_eq!(payload["error"]["message"], "upstream exploded");
    }

    /// A multi-line malformed SSE event is **one** frame: the verbatim copy may
    /// not be duplicated per unparseable `data:` line.
    #[test]
    fn stream_multi_line_malformed_frame_yields_single_raw() {
        let frame = b"data: not-json\ndata: also-not-json\n\n".to_vec();
        let events = ResponsesCodec::decode_stream_frame(&frame);
        assert_eq!(
            events,
            vec![IrStreamEvent::Raw(frame.clone())],
            "one verbatim copy per frame"
        );
    }

    // ── P0-2 encoder completeness / P0-3a stateful decoder ─────────
    /// Split encoded SSE bytes into the decoded `data:` payloads.
    fn sse_frames(bytes: &[u8]) -> Vec<Value> {
        String::from_utf8_lossy(bytes)
            .split("\n\n")
            .filter_map(|chunk| {
                chunk
                    .trim()
                    .strip_prefix("data:")
                    .and_then(|rest| serde_json::from_str::<Value>(rest.trim()).ok())
            })
            .collect()
    }

    fn find_frame<'a>(frames: &'a [Value], ty: &str) -> &'a Value {
        frames
            .iter()
            .find(|f| f["type"] == json!(ty))
            .unwrap_or_else(|| panic!("no {ty} frame in {frames:?}"))
    }

    /// `output_item.done` must repeat the full item — `arguments`, `call_id`
    /// and `name` — not a hollow `{type, status}` stub.
    #[test]
    fn output_item_done_carries_function_call_payload() {
        let enc = ResponsesCodec::stream_encoder();
        let mut all = Vec::new();
        all.extend(enc(&IrStreamEvent::ToolCallStart {
            index: 0,
            id: Some("call_1".into()),
            name: Some("get_weather".into()),
        }));
        all.extend(enc(&IrStreamEvent::ToolCallDelta {
            index: 0,
            arguments_delta: "{\"city\":\"SF\"}".into(),
        }));
        all.extend(enc(&IrStreamEvent::Finish {
            index: 0,
            finish_reason: Some(IrFinishReason::ToolCalls),
        }));

        let frames = sse_frames(&all);
        let done = find_frame(&frames, "response.output_item.done");
        let item = &done["item"];
        assert_eq!(item["type"], json!("function_call"));
        assert_eq!(item["arguments"], json!("{\"city\":\"SF\"}"));
        assert!(
            item["call_id"].as_str().is_some_and(|s| !s.is_empty()),
            "call_id missing: {item}"
        );
        assert!(
            item["name"].as_str().is_some_and(|s| !s.is_empty()),
            "name missing: {item}"
        );
        // The `added` frame announced the item with the same id.
        let added = find_frame(&frames, "response.output_item.added");
        assert_eq!(added["item"]["id"], item["id"]);
    }

    /// The terminal frame must carry every closed item, not `output: []`.
    #[test]
    fn response_completed_output_lists_all_items_in_index_order() {
        let enc = ResponsesCodec::stream_encoder();
        let mut all = Vec::new();
        all.extend(enc(&IrStreamEvent::ToolCallStart {
            index: 0,
            id: Some("call_1".into()),
            name: Some("fn".into()),
        }));
        all.extend(enc(&IrStreamEvent::ToolCallDelta {
            index: 0,
            arguments_delta: "{}".into(),
        }));
        all.extend(enc(&IrStreamEvent::TextDelta {
            index: 1,
            text: "hi".into(),
        }));
        all.extend(enc(&IrStreamEvent::Finish {
            index: 0,
            finish_reason: Some(IrFinishReason::Stop),
        }));

        let frames = sse_frames(&all);
        let completed = find_frame(&frames, "response.completed");
        let output = completed["response"]["output"]
            .as_array()
            .expect("output array");
        assert_eq!(output.len(), 2, "{completed}");
        // Ascending `output_index`: item ids are minted from the index, and
        // spec items carry no `output_index` field, so the suffixes pin the
        // order (function_call is item 0, message is item 1).
        assert_eq!(output[0]["id"], json!("fc_0"));
        assert_eq!(output[1]["id"], json!("msg_1"));
    }

    /// Without deltas, `output_item.done` is the sole carrier of `arguments`.
    #[test]
    fn output_item_done_supplies_arguments_when_no_deltas() {
        let mut dec = ResponsesCodec::stream_decoder();
        let added = frame(json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "type": "function_call", "id": "fc_0", "call_id": "call_1",
                "name": "get_weather", "arguments": "", "status": "in_progress"
            }
        }));
        let done = frame(json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "function_call", "id": "fc_0", "call_id": "call_1",
                "name": "get_weather", "arguments": "{\"city\":\"SF\"}",
                "status": "completed"
            }
        }));

        let mut events = dec(&added);
        events.extend(dec(&done));
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                IrStreamEvent::ToolCallDelta {
                    arguments_delta, ..
                } => Some(arguments_delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["{\"city\":\"SF\"}"], "{events:?}");
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, IrStreamEvent::ToolCallStart { .. }))
                .count(),
            1,
            "start emitted once, at `added`: {events:?}"
        );
    }

    /// Once the deltas delivered the payload, `done` must stay silent.
    #[test]
    fn output_item_done_does_not_duplicate_streamed_deltas() {
        let mut dec = ResponsesCodec::stream_decoder();
        let mut events = dec(&frame(json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "type": "function_call", "id": "fc_0", "call_id": "call_1",
                "name": "get_weather", "arguments": "", "status": "in_progress"
            }
        })));
        for delta in ["{\"city\":", "\"SF\"}"] {
            events.extend(dec(&frame(json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": delta
            }))));
        }
        events.extend(dec(&frame(json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "function_call", "id": "fc_0", "call_id": "call_1",
                "name": "get_weather", "arguments": "{\"city\":\"SF\"}",
                "status": "completed"
            }
        }))));

        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                IrStreamEvent::ToolCallDelta {
                    arguments_delta, ..
                } => Some(arguments_delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            deltas,
            vec!["{\"city\":", "\"SF\"}"],
            "arguments must not be re-delivered by `done`: {events:?}"
        );
    }

    /// P0-2/P0-3 pairing: encode a tool call, decode the bytes, and the
    /// arguments arrive exactly once and intact.
    #[test]
    fn stream_roundtrip_encode_then_decode_delivers_arguments_once() {
        let enc = ResponsesCodec::stream_encoder();
        let mut bytes = Vec::new();
        bytes.extend(enc(&IrStreamEvent::Start(IrResponseMeta {
            id: Some("resp_1".into()),
            created: Some(1),
            model: Some("m".into()),
        })));
        bytes.extend(enc(&IrStreamEvent::ToolCallStart {
            index: 0,
            id: Some("call_1".into()),
            name: Some("get_weather".into()),
        }));
        bytes.extend(enc(&IrStreamEvent::ToolCallDelta {
            index: 0,
            arguments_delta: "{\"city\":\"SF\"}".into(),
        }));
        bytes.extend(enc(&IrStreamEvent::Finish {
            index: 0,
            finish_reason: Some(IrFinishReason::ToolCalls),
        }));

        let text = String::from_utf8(bytes).expect("utf-8 SSE");
        let mut dec = ResponsesCodec::stream_decoder();
        let mut events = Vec::new();
        for chunk in text.split_inclusive("\n\n") {
            events.extend(dec(chunk.as_bytes()));
        }

        let mut arguments = String::new();
        let mut delta_count = 0;
        let mut start_count = 0;
        for ev in &events {
            match ev {
                IrStreamEvent::ToolCallStart { name, .. } => {
                    start_count += 1;
                    assert_eq!(name.as_deref(), Some("get_weather"));
                }
                IrStreamEvent::ToolCallDelta {
                    arguments_delta, ..
                } => {
                    delta_count += 1;
                    arguments.push_str(arguments_delta);
                }
                _ => {}
            }
        }
        assert_eq!(start_count, 1, "{events:?}");
        assert_eq!(delta_count, 1, "arguments delivered once: {events:?}");
        assert_eq!(arguments, "{\"city\":\"SF\"}");
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

    // ── P0-3b: bookkeeping frames must not leak as `Raw` ────────

    /// Every known Responses bookkeeping event decodes to **no** IR event:
    /// neither content nor `Raw`. One sequential decoder is used so that the
    /// shared state cannot make a frame look bookkeeping by accident; each
    /// frame is fed as its own upstream frame, in lifecycle order.
    #[test]
    fn known_bookkeeping_events_yield_no_raw() {
        let mut dec = ResponsesCodec::stream_decoder();
        for ty in [
            "response.in_progress",
            "response.output_item.added",
            "response.content_part.added",
            "response.content_part.done",
            "response.output_text.done",
            "response.function_call_arguments.done",
            "response.reasoning_summary_text.done",
            "response.reasoning_text.done",
            "response.output_text.annotation.added",
        ] {
            let events = dec(&frame(json!({
                "type": ty,
                "output_index": 0,
                "content_index": 0,
                "item": {"type": "message", "role": "assistant", "content": []},
                "part": {"type": "output_text", "text": ""},
                "text": "",
                "arguments": "",
                "delta": ""
            })));
            assert!(events.is_empty(), "{ty}: {events:?}");
        }
        // `response.created` is the one bookkeeping-adjacent frame that legitimately
        // carries IR content (`Start`) — it must stay `Start`-only, never `Raw`.
        let created = dec(&frame(json!({
            "type": "response.created",
            "response": {"id": "resp_1", "created_at": 5, "model": "gpt-4o"}
        })));
        assert!(
            matches!(created.as_slice(), [IrStreamEvent::Start(_)]),
            "{created:?}"
        );
        // No hosted-tool item is `Raw` either: its payload never reaches the IR.
        let added = dec(&frame(json!({
            "type": "response.output_item.added",
            "output_index": 3,
            "item": {"type": "web_search_call", "id": "ws_3", "status": "in_progress"}
        })));
        assert!(added.is_empty(), "{added:?}");
    }

    /// The deliberate non-catch-all: a genuinely unknown `type` stays visible
    /// as `Raw` so a same-protocol passthrough lane still forwards it.
    #[test]
    fn unknown_event_type_still_surfaces_as_raw() {
        let mut dec = ResponsesCodec::stream_decoder();
        let events = dec(&frame(json!({
            "type": "response.something_brand_new", "x": 1
        })));
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(events[0], IrStreamEvent::Raw(_)), "{events:?}");
    }

    // ── parallel tool calls (interleaved IR indices) ──────────────

    /// Encode a whole IR event sequence through one stateful encoder and
    /// return the decoded `data:` payloads.
    fn encode_all(events: &[IrStreamEvent]) -> Vec<Value> {
        let enc = ResponsesCodec::stream_encoder();
        let mut bytes = Vec::new();
        for ev in events {
            bytes.extend(enc(ev));
        }
        sse_frames(&bytes)
    }

    /// Two tool calls opened back to back, then their deltas interleaved.
    fn parallel_tool_call_events() -> Vec<IrStreamEvent> {
        vec![
            IrStreamEvent::ToolCallStart {
                index: 0,
                id: Some("c0".into()),
                name: Some("f0".into()),
            },
            IrStreamEvent::ToolCallStart {
                index: 1,
                id: Some("c1".into()),
                name: Some("f1".into()),
            },
            IrStreamEvent::ToolCallDelta {
                index: 0,
                arguments_delta: "{\"a\":1}".into(),
            },
            IrStreamEvent::ToolCallDelta {
                index: 1,
                arguments_delta: "{\"b\":2}".into(),
            },
            IrStreamEvent::Finish {
                index: 0,
                finish_reason: Some(IrFinishReason::ToolCalls),
            },
        ]
    }

    /// `output_index` → `call_id` for every `function_call` item announced.
    fn function_call_owners(frames: &[Value]) -> HashMap<u32, String> {
        frames
            .iter()
            .filter(|f| {
                f["type"] == json!("response.output_item.added")
                    && f["item"]["type"] == json!("function_call")
            })
            .map(|f| {
                let index = f["output_index"].as_u64().expect("output_index") as u32;
                let call_id = f["item"]["call_id"].as_str().expect("call_id").to_string();
                (index, call_id)
            })
            .collect()
    }

    /// The production incident: two calls streamed concurrently must keep
    /// their own arguments instead of one losing them and the other
    /// concatenating both.
    #[test]
    fn parallel_tool_calls_preserve_independent_arguments() {
        let frames = encode_all(&parallel_tool_call_events());
        let completed = find_frame(&frames, "response.completed");
        let output = completed["response"]["output"]
            .as_array()
            .expect("output array");
        assert_eq!(output.len(), 2, "{completed}");

        let arguments_for = |call_id: &str| {
            output
                .iter()
                .find(|item| item["call_id"] == json!(call_id))
                .unwrap_or_else(|| panic!("no item with call_id {call_id}: {completed}"))["arguments"]
                .clone()
        };
        assert_eq!(arguments_for("c0"), json!("{\"a\":1}"), "{completed}");
        assert_eq!(arguments_for("c1"), json!("{\"b\":2}"), "{completed}");
    }

    /// Every `function_call_arguments.delta` must name the `output_index` of
    /// the call whose arguments it carries.
    #[test]
    fn parallel_tool_call_deltas_carry_correct_output_index() {
        let frames = encode_all(&parallel_tool_call_events());
        let owners = function_call_owners(&frames);
        assert_eq!(owners.len(), 2, "{frames:?}");

        let mut routed: Vec<(String, Value)> = Vec::new();
        for frame in &frames {
            if frame["type"] != json!("response.function_call_arguments.delta") {
                continue;
            }
            let index = frame["output_index"].as_u64().expect("output_index") as u32;
            let call_id = owners
                .get(&index)
                .unwrap_or_else(|| panic!("delta for unknown output_index {index}: {frames:?}"))
                .clone();
            routed.push((call_id, frame["delta"].clone()));
        }
        assert_eq!(
            routed,
            vec![
                ("c0".to_string(), json!("{\"a\":1}")),
                ("c1".to_string(), json!("{\"b\":2}")),
            ],
            "{frames:?}"
        );
    }

    /// A second concurrently-open call must not commit the first one.
    #[test]
    fn parallel_tool_calls_in_one_chunk_do_not_close_each_other() {
        let frames = encode_all(&[
            IrStreamEvent::ToolCallStart {
                index: 0,
                id: Some("c0".into()),
                name: Some("f0".into()),
            },
            IrStreamEvent::ToolCallStart {
                index: 1,
                id: Some("c1".into()),
                name: Some("f1".into()),
            },
        ]);
        let count = |ty: &str| frames.iter().filter(|f| f["type"] == json!(ty)).count();
        assert_eq!(count("response.output_item.added"), 2, "{frames:?}");
        assert_eq!(count("response.output_item.done"), 0, "{frames:?}");
    }

    /// A replayed `output_item.done` (SSE reconnect / chunk replay) must not
    /// re-deliver the arguments the deltas already carried.
    #[test]
    fn duplicate_output_item_done_does_not_duplicate_arguments() {
        let mut dec = ResponsesCodec::stream_decoder();
        let mut events = dec(&frame(json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "type": "function_call", "id": "fc_0", "call_id": "call_1",
                "name": "get_weather", "arguments": "", "status": "in_progress"
            }
        })));
        for delta in ["{\"city\":", "\"SF\"}"] {
            events.extend(dec(&frame(json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": delta
            }))));
        }
        let done = frame(json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "function_call", "id": "fc_0", "call_id": "call_1",
                "name": "get_weather", "arguments": "{\"city\":\"SF\"}",
                "status": "completed"
            }
        }));
        events.extend(dec(&done));
        events.extend(dec(&done));

        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                IrStreamEvent::ToolCallDelta {
                    arguments_delta, ..
                } => Some(arguments_delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            deltas,
            vec!["{\"city\":", "\"SF\"}"],
            "arguments delivered once: {events:?}"
        );
    }

    /// The stateless preview path must mint the same id shape as the stateful
    /// encoder: `fc_{output_index}` for `id`, the IR call id for `call_id`.
    #[test]
    fn stateless_encode_stream_event_uses_index_derived_id() {
        let out = ResponsesCodec::encode_stream_event(&IrStreamEvent::ToolCallStart {
            index: 2,
            id: Some("call_x".into()),
            name: Some("fn".into()),
        });
        let frames = sse_frames(&out);
        let added = find_frame(&frames, "response.output_item.added");
        assert_eq!(added["output_index"], json!(2), "{added}");
        assert_eq!(added["item"]["id"], json!("fc_2"), "{added}");
        assert_eq!(added["item"]["call_id"], json!("call_x"), "{added}");
    }

    /// Interleaved calls survive an encode→decode round trip without mixing.
    #[test]
    fn parallel_tool_calls_roundtrip_through_decoder() {
        let enc = ResponsesCodec::stream_encoder();
        let mut bytes = enc(&IrStreamEvent::Start(IrResponseMeta {
            id: Some("resp_1".into()),
            created: Some(1),
            model: Some("m".into()),
        }));
        for ev in parallel_tool_call_events() {
            bytes.extend(enc(&ev));
        }

        let text = String::from_utf8(bytes).expect("utf-8 SSE");
        let mut dec = ResponsesCodec::stream_decoder();
        let mut events = Vec::new();
        for chunk in text.split_inclusive("\n\n") {
            events.extend(dec(chunk.as_bytes()));
        }

        // `output_index` → call id from the start frames, then route deltas.
        let mut owners: HashMap<u32, String> = HashMap::new();
        let mut arguments: HashMap<u32, String> = HashMap::new();
        for ev in &events {
            match ev {
                IrStreamEvent::ToolCallStart {
                    index,
                    id: Some(id),
                    ..
                } => {
                    owners.insert(*index, id.clone());
                }
                IrStreamEvent::ToolCallDelta {
                    index,
                    arguments_delta,
                } => {
                    arguments
                        .entry(*index)
                        .or_default()
                        .push_str(arguments_delta);
                }
                _ => {}
            }
        }
        assert_eq!(owners.len(), 2, "{events:?}");
        let mut by_call_id: Vec<(String, String)> = arguments
            .iter()
            .map(|(index, args)| {
                let call_id = owners
                    .get(index)
                    .unwrap_or_else(|| panic!("delta for unknown index {index}: {events:?}"));
                (call_id.clone(), args.clone())
            })
            .collect();
        by_call_id.sort();
        assert_eq!(
            by_call_id,
            vec![
                ("c0".to_string(), "{\"a\":1}".to_string()),
                ("c1".to_string(), "{\"b\":2}".to_string()),
            ],
            "{events:?}"
        );
    }

    // ── interleaving text / reasoning with tool calls ────────────

    /// Count `response.output_item.added` frames whose `item.type` is `ty`.
    fn count_added(frames: &[Value], ty: &str) -> usize {
        frames
            .iter()
            .filter(|f| {
                f["type"] == json!("response.output_item.added") && f["item"]["type"] == json!(ty)
            })
            .count()
    }

    /// Reviewer reproducer: a text delta in the middle of a tool call's
    /// argument fragments must not split the tool call into two items, nor
    /// leave a phantom `call_id: ""` item in the terminal output.
    #[test]
    fn interleaved_text_does_not_split_tool_call() {
        let frames = encode_all(&[
            IrStreamEvent::ToolCallStart {
                index: 0,
                id: Some("c0".into()),
                name: Some("f0".into()),
            },
            IrStreamEvent::ToolCallDelta {
                index: 0,
                arguments_delta: "{\"a\":".into(),
            },
            IrStreamEvent::TextDelta {
                index: 0,
                text: "Wait".into(),
            },
            IrStreamEvent::ToolCallDelta {
                index: 0,
                arguments_delta: "1}".into(),
            },
            IrStreamEvent::Finish {
                index: 0,
                finish_reason: Some(IrFinishReason::Stop),
            },
        ]);

        assert_eq!(count_added(&frames, "function_call"), 1, "{frames:?}");
        assert_eq!(count_added(&frames, "message"), 1, "{frames:?}");

        let completed = find_frame(&frames, "response.completed");
        let output = completed["response"]["output"]
            .as_array()
            .expect("output array");
        assert_eq!(output.len(), 2, "{completed}");

        let calls: Vec<&Value> = output
            .iter()
            .filter(|item| item["type"] == json!("function_call"))
            .collect();
        assert_eq!(calls.len(), 1, "{completed}");
        assert_eq!(calls[0]["call_id"], json!("c0"), "{completed}");
        assert_eq!(calls[0]["arguments"], json!("{\"a\":1}"), "{completed}");

        let messages: Vec<&Value> = output
            .iter()
            .filter(|item| item["type"] == json!("message"))
            .collect();
        assert_eq!(messages.len(), 1, "{completed}");
        assert_eq!(
            messages[0]["content"][0]["text"],
            json!("Wait"),
            "{completed}"
        );
    }

    /// Mirror direction: a tool call opening in the middle of a message must
    /// not split the message into two items.
    #[test]
    fn interleaved_tool_call_does_not_split_message() {
        let frames = encode_all(&[
            IrStreamEvent::TextDelta {
                index: 0,
                text: "Hello ".into(),
            },
            IrStreamEvent::ToolCallStart {
                index: 1,
                id: Some("c1".into()),
                name: Some("f1".into()),
            },
            IrStreamEvent::TextDelta {
                index: 0,
                text: "world".into(),
            },
            IrStreamEvent::Finish {
                index: 0,
                finish_reason: Some(IrFinishReason::Stop),
            },
        ]);

        assert_eq!(count_added(&frames, "message"), 1, "{frames:?}");
        assert_eq!(count_added(&frames, "function_call"), 1, "{frames:?}");

        let completed = find_frame(&frames, "response.completed");
        let output = completed["response"]["output"]
            .as_array()
            .expect("output array");
        assert_eq!(output.len(), 2, "{completed}");

        let messages: Vec<&Value> = output
            .iter()
            .filter(|item| item["type"] == json!("message"))
            .collect();
        assert_eq!(messages.len(), 1, "{completed}");
        assert_eq!(
            messages[0]["content"][0]["text"],
            json!("Hello world"),
            "{completed}"
        );
        assert_eq!(
            output
                .iter()
                .filter(|item| item["type"] == json!("function_call"))
                .count(),
            1,
            "{completed}"
        );
    }

    /// A fragment arriving after the item was committed must be dropped, not
    /// resurrected as a fresh `function_call` with a new `output_index`.
    #[test]
    fn delta_after_close_does_not_resurrect_item() {
        let enc = ResponsesCodec::stream_encoder();
        let mut bytes = Vec::new();
        bytes.extend(enc(&IrStreamEvent::ToolCallStart {
            index: 0,
            id: Some("c0".into()),
            name: Some("f0".into()),
        }));
        bytes.extend(enc(&IrStreamEvent::ToolCallDelta {
            index: 0,
            arguments_delta: "{\"a\":1}".into(),
        }));
        bytes.extend(enc(&IrStreamEvent::Finish {
            index: 0,
            finish_reason: Some(IrFinishReason::Stop),
        }));
        // Late replay of the same call's fragment.
        bytes.extend(enc(&IrStreamEvent::ToolCallDelta {
            index: 0,
            arguments_delta: "}".into(),
        }));

        let frames = sse_frames(&bytes);
        assert_eq!(count_added(&frames, "function_call"), 1, "{frames:?}");
        let completed = find_frame(&frames, "response.completed");
        assert_eq!(
            completed["response"]["output"][0]["arguments"],
            json!("{\"a\":1}"),
            "{completed}"
        );
        assert!(
            !frames.iter().any(|f| {
                f["type"] == json!("response.output_item.added") && f["output_index"] == json!(1)
            }),
            "{frames:?}"
        );
    }

    /// Reasoning → message keeps one item of each: the reasoning item is
    /// committed when the message opens, and every text delta accumulates
    /// into a single message item.
    #[test]
    fn reasoning_then_text_reuses_single_message_item() {
        let frames = encode_all(&[
            IrStreamEvent::ReasoningDelta {
                index: 0,
                text: "think ".into(),
            },
            IrStreamEvent::TextDelta {
                index: 0,
                text: "Hello ".into(),
            },
            IrStreamEvent::TextDelta {
                index: 0,
                text: "world".into(),
            },
            IrStreamEvent::Finish {
                index: 0,
                finish_reason: Some(IrFinishReason::Stop),
            },
        ]);

        assert_eq!(count_added(&frames, "reasoning"), 1, "{frames:?}");
        assert_eq!(count_added(&frames, "message"), 1, "{frames:?}");

        let completed = find_frame(&frames, "response.completed");
        let output = completed["response"]["output"]
            .as_array()
            .expect("output array");
        assert_eq!(output.len(), 2, "{completed}");
        assert_eq!(
            output
                .iter()
                .filter(|item| item["type"] == json!("reasoning"))
                .count(),
            1,
            "{completed}"
        );
        let messages: Vec<&Value> = output
            .iter()
            .filter(|item| item["type"] == json!("message"))
            .collect();
        assert_eq!(messages.len(), 1, "{completed}");
        assert_eq!(
            messages[0]["content"][0]["text"],
            json!("Hello world"),
            "{completed}"
        );
    }

    // ── stream terminal: `response.completed` emits `Finish` ─────────

    /// A Chat-ingress client on a Responses-upstream lane must receive a
    /// `finish_reason`: `response.completed` is the upstream's terminal, so
    /// the decoder emits the structured `Finish` (before `Usage`/`Raw`).
    #[test]
    fn response_completed_emits_stop_finish_reason() {
        let events = ResponsesCodec::decode_stream_frame(&frame(json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "status": "completed",
                "output": [{
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "hi", "annotations": []}]
                }],
                "usage": {"input_tokens": 7, "output_tokens": 3, "total_tokens": 10}
            }
        })));
        assert!(
            matches!(
                events[0],
                IrStreamEvent::Finish {
                    index: 0,
                    finish_reason: Some(IrFinishReason::Stop),
                }
            ),
            "a text-only completed turn must decode to Finish(Stop): {events:?}"
        );
        assert!(
            matches!(events[1], IrStreamEvent::Usage(ref u) if u.completion_tokens == 3),
            "Usage must still follow the Finish: {events:?}"
        );
    }

    /// A turn that ends on tool calls must report `tool_calls`, not `stop` —
    /// the `output[]` scan is the only evidence the payload carries.
    #[test]
    fn response_completed_with_function_call_emits_tool_calls() {
        let events = ResponsesCodec::decode_stream_frame(&frame(json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "status": "completed",
                "output": [{
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": "get_weather",
                    "arguments": "{}",
                    "status": "completed"
                }]
            }
        })));
        assert!(
            matches!(
                events[0],
                IrStreamEvent::Finish {
                    index: 0,
                    finish_reason: Some(IrFinishReason::ToolCalls),
                }
            ),
            "a function_call turn must decode to Finish(ToolCalls): {events:?}"
        );
    }

    /// No `/response/status`: the event name implies `completed`, so fall back
    /// to the tool-call scan and otherwise `stop` — never panic, never omit.
    #[test]
    fn response_completed_without_status_falls_back_to_stop() {
        let events = ResponsesCodec::decode_stream_frame(&frame(json!({
            "type": "response.completed",
            "response": {
                "output": [{
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "hi", "annotations": []}]
                }]
            }
        })));
        assert!(
            matches!(
                events[0],
                IrStreamEvent::Finish {
                    index: 0,
                    finish_reason: Some(IrFinishReason::Stop),
                }
            ),
            "a payload without `status` falls back to Finish(Stop): {events:?}"
        );
    }
}
