//! Cross-codec tool regression suite (public-API level).
//!
//! `IrTool.raw` has two conventions — chat's dual layer
//! (`__tool_extra` / `__function_extra`) and the flat layout used by
//! Responses / Anthropic. These tests pin the **outbound wire behaviour**
//! of every hop (and of the two-hop round-trips), never the IR-internal
//! `raw` layout:
//!
//! 1. Responses → Chat: `tools[].type` is preserved (synthesized to
//!    `"function"` only when absent / `""`); a root-level `strict` lands
//!    inside `function` (OpenAI semantics).
//! 2. Anthropic → Chat: a type-less Anthropic tool gains
//!    `"type":"function"`; extras (`cache_control`, …) return to the tool
//!    root, not inside `function`.
//! 3. Hosted guard: an Anthropic `web_search*` tool aimed at Chat is
//!    dropped with a loss entry (`UnsupportedByTarget`) — never squeezed
//!    into a fake function shape.
//! 4. Chat → Responses: no `__`-pseudo fields, `strict` at the root,
//!    `type` from the original value or the Responses default.
//! 5. Chat → Anthropic: no pseudo fields, `type` mapped **by value** —
//!    `"function"` and `""` are omitted (Anthropic's enum has no
//!    `"function"`), any other value (`"custom"`, …) passes through.
//! 6. Chat ↔ chat red line: an inbound tool without `type` must never grow
//!    a `type` on the way out.
//! 7. Round-trips Responses → Chat → Responses and
//!    Anthropic → Chat → Anthropic keep the semantic fields
//!    (name / description / schema / strict / extras) intact.

use serde_json::{Value, json};

use sag_core::{ChatCompletionRequest, IrRequest, LossInventory, LossReason, LossScope};
use sag_providers::{AnthropicMessagesCodec, ChatCompletionsCodec, ResponsesCodec};

const TOOL_EXTRA_KEY: &str = "__tool_extra";
const FUNCTION_EXTRA_KEY: &str = "__function_extra";

const CHAT_MODEL: &str = "gpt-4o";
const ANTHROPIC_MODEL: &str = "claude-3-7-sonnet";

// ── helpers ─────────────────────────────────────────────────────

fn chat_request(body: Value) -> ChatCompletionRequest {
    serde_json::from_value(body).expect("valid chat request")
}

fn responses_ir(body: Value) -> IrRequest {
    ResponsesCodec::decode_request(&body).expect("valid responses request")
}

/// IR → Chat wire JSON + loss.
fn chat_json(ir: &IrRequest) -> (Value, LossInventory) {
    let (req, loss) = ChatCompletionsCodec::encode_request_with_loss(ir);
    let wire = serde_json::to_value(&req).expect("chat request serializes");
    (wire, loss)
}

/// Responses body → IR → Chat wire.
fn responses_to_chat(body: Value) -> (Value, LossInventory) {
    chat_json(&responses_ir(body))
}

/// Anthropic body → IR → Chat wire.
fn anthropic_to_chat(body: Value) -> (Value, LossInventory) {
    chat_json(&AnthropicMessagesCodec::decode_request(&body))
}

/// Chat body → IR → Responses wire.
fn chat_to_responses(body: Value) -> Value {
    let ir = ChatCompletionsCodec::decode_request(&chat_request(body));
    ResponsesCodec::encode_request(&ir)
}

/// Chat body → IR → Anthropic wire.
fn chat_to_anthropic(body: Value) -> (Value, LossInventory) {
    let ir = ChatCompletionsCodec::decode_request(&chat_request(body));
    AnthropicMessagesCodec::encode_request_with_loss(&ir, ANTHROPIC_MODEL)
}

/// Responses → Chat → Responses (two hops through the Chat wire).
fn responses_chat_responses(body: Value) -> Value {
    let (chat_wire, _) = responses_to_chat(body);
    let ir = ChatCompletionsCodec::decode_request(&chat_request(chat_wire));
    ResponsesCodec::encode_request(&ir)
}

/// Anthropic → Chat → Anthropic (two hops through the Chat wire).
fn anthropic_chat_anthropic(body: Value) -> Value {
    let (chat_wire, _) = anthropic_to_chat(body);
    let ir = ChatCompletionsCodec::decode_request(&chat_request(chat_wire));
    AnthropicMessagesCodec::encode_request(&ir)
}

/// Reserved `__`-prefixed markers are IR-internal: they must never reach
/// any outbound JSON, from any source convention.
fn assert_no_reserved_markers(wire: &Value) {
    let text = wire.to_string();
    assert!(
        !text.contains(TOOL_EXTRA_KEY),
        "{TOOL_EXTRA_KEY} leaked to the wire: {text}"
    );
    assert!(
        !text.contains(FUNCTION_EXTRA_KEY),
        "{FUNCTION_EXTRA_KEY} leaked to the wire: {text}"
    );
}

fn chat_body(tools: Value) -> Value {
    json!({
        "model": CHAT_MODEL,
        "messages": [{"role": "user", "content": "hi"}],
        "tools": tools,
    })
}

// ── 1. Responses → Chat ─────────────────────────────────────────

/// Responses tool `type` survives the hop and the root-level `strict`
/// moves inside `function` (OpenAI puts `strict` there).
#[test]
fn responses_to_chat_keeps_type_and_strict() {
    let (wire, loss) = responses_to_chat(json!({
        "model": CHAT_MODEL,
        "input": "hi",
        "tools": [{
            "type": "function",
            "name": "get_weather",
            "description": "Get weather",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
            "strict": true
        }]
    }));
    assert!(loss.is_empty(), "loss: {:?}", loss.entries);

    let tool = &wire["tools"][0];
    assert_eq!(tool["type"], json!("function"), "type survives: {tool}");
    assert_eq!(tool["function"]["name"], json!("get_weather"));
    assert_eq!(tool["function"]["description"], json!("Get weather"));
    assert_eq!(
        tool["function"]["parameters"]["properties"]["city"],
        json!({"type": "string"})
    );
    assert_eq!(
        tool["function"]["strict"],
        json!(true),
        "strict belongs inside function: {tool}"
    );
    assert!(
        tool.get("strict").is_none(),
        "no root-level strict on the chat wire: {tool}"
    );
    assert_no_reserved_markers(&wire);
}

/// A Responses tool without `type` — or with the `""` stand-in for
/// "absent" — gets the OpenAI default `"function"` on the chat wire.
#[test]
fn responses_to_chat_absent_type_gets_function() {
    for tool_in in [
        json!({"name": "fn", "parameters": {"type": "object"}}),
        json!({"type": "", "name": "fn", "parameters": {"type": "object"}}),
    ] {
        let (wire, loss) = responses_to_chat(json!({
            "model": CHAT_MODEL,
            "input": "hi",
            "tools": [tool_in],
        }));
        assert!(loss.is_empty(), "loss: {:?}", loss.entries);
        assert_eq!(
            wire["tools"][0]["type"],
            json!("function"),
            "cross-protocol translation synthesizes the OpenAI default: {:?}",
            wire["tools"][0]
        );
        assert_eq!(wire["tools"][0]["function"]["name"], json!("fn"));
        assert_no_reserved_markers(&wire);
    }
}

// ── 2./3. Anthropic → Chat ──────────────────────────────────────

/// Anthropic tools carry no `type` for plain functions: the chat wire
/// needs `"type":"function"`, while `cache_control` and friends return to
/// the tool root (not inside `function`).
#[test]
fn anthropic_to_chat_gets_function_type() {
    let (wire, loss) = anthropic_to_chat(json!({
        "model": ANTHROPIC_MODEL,
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [
            {
                "name": "get_weather",
                "description": "Get weather",
                "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}},
                "cache_control": {"type": "ephemeral"}
            },
            {
                "type": "custom",
                "name": "custom_tool",
                "input_schema": {"type": "object"}
            }
        ]
    }));
    assert!(loss.is_empty(), "loss: {:?}", loss.entries);

    let tool = &wire["tools"][0];
    assert_eq!(
        tool["type"],
        json!("function"),
        "type-less Anthropic tool gains the chat default: {tool}"
    );
    assert_eq!(tool["function"]["name"], json!("get_weather"));
    assert_eq!(
        tool["function"]["parameters"]["properties"]["city"],
        json!({"type": "string"})
    );
    assert_eq!(
        tool["cache_control"],
        json!({"type": "ephemeral"}),
        "tool-level extras return to the tool root: {tool}"
    );
    assert!(
        tool["function"].get("cache_control").is_none(),
        "extras must not sink into function: {tool}"
    );
    assert!(
        tool["function"].get("type").is_none(),
        "type is tool-level only: {tool}"
    );

    // An explicit non-function type passes through verbatim.
    assert_eq!(
        wire["tools"][1]["type"],
        json!("custom"),
        "original type value is preserved: {:?}",
        wire["tools"][1]
    );
    assert_no_reserved_markers(&wire);
}

/// Hosted `web_search*` tools have no Chat equivalent: drop them and
/// record a `UnsupportedByTarget` loss instead of faking a function.
#[test]
fn anthropic_web_search_to_chat_drops_with_loss() {
    let (wire, loss) = anthropic_to_chat(json!({
        "model": ANTHROPIC_MODEL,
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [
            {"name": "get_weather", "input_schema": {"type": "object"}},
            {"type": "web_search_20250305", "name": "web_search", "max_uses": 5}
        ]
    }));

    let tools = wire["tools"]
        .as_array()
        .expect("tools stays an array even when everything is dropped");
    assert_eq!(
        tools.len(),
        1,
        "hosted web_search must be dropped, not squeezed into a function: {wire}"
    );
    assert_eq!(tools[0]["function"]["name"], json!("get_weather"));

    assert_eq!(loss.len(), 1, "exactly one loss: {:?}", loss.entries);
    let entry = &loss.entries[0];
    assert_eq!(entry.scope, LossScope::Request);
    assert_eq!(entry.field, "tools[1]");
    assert_eq!(entry.reason, LossReason::UnsupportedByTarget);
    // The loss snapshot is the IR tool (name is IR-level, not raw layout).
    assert_eq!(entry.value["name"], json!("web_search"));
    assert_no_reserved_markers(&wire);
}

// ── 4./5. Chat → Responses / Anthropic ──────────────────────────

/// Chat-sourced IR expands to the flat Responses tool shape: `strict` at
/// the root, `type` from the original value, no `__`-pseudo fields.
#[test]
fn chat_to_responses_no_pseudo_fields() {
    let wire = chat_to_responses(chat_body(json!([{
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get weather",
            "parameters": {"type": "object"},
            "strict": true
        }
    }])));

    let tool = &wire["tools"][0];
    assert_eq!(tool["type"], json!("function"));
    assert_eq!(tool["name"], json!("get_weather"));
    assert_eq!(tool["description"], json!("Get weather"));
    assert_eq!(tool["parameters"], json!({"type": "object"}));
    assert_eq!(
        tool["strict"],
        json!(true),
        "function-level extras land at the tool root: {tool}"
    );
    assert!(
        tool.get("function").is_none(),
        "Responses tools are flat: {tool}"
    );
    assert_no_reserved_markers(&wire);

    // No original `type` → the Responses default applies, no marker leaks.
    let wire = chat_to_responses(chat_body(json!([{"function": {"name": "fn"}}])));
    let tool = &wire["tools"][0];
    assert_eq!(
        tool["type"],
        json!("function"),
        "absent type takes the Responses default: {tool}"
    );
    assert!(tool.get("parameters").is_none(), "{tool}");
    assert!(tool.get("function").is_none(), "{tool}");
    assert_no_reserved_markers(&wire);
}

/// Chat → Anthropic: no pseudo fields and `type` mapped **by value** —
/// `"function"` / `""` are omitted, anything else passes through.
#[test]
fn chat_to_anthropic_no_pseudo_fields() {
    // `max_completion_tokens` keeps the Anthropic encode from recording its
    // unrelated "max_tokens defaulted" loss, so `loss` here is tool-only.
    let (wire, loss) = chat_to_anthropic(json!({
        "model": CHAT_MODEL,
        "messages": [{"role": "user", "content": "hi"}],
        "max_completion_tokens": 1024,
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "fn",
                    "parameters": {"type": "object"},
                    "strict": true
                }
            },
            {"function": {"name": "no_type"}},
            {"type": "", "function": {"name": "empty_type"}},
            {
                "type": "custom",
                "function": {"name": "custom_tool", "parameters": {"type": "object"}}
            }
        ]
    }));
    assert!(loss.is_empty(), "loss: {:?}", loss.entries);

    let tools = wire["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 4);

    let fn_tool = &tools[0];
    assert!(
        fn_tool.get("type").is_none(),
        "Anthropic's tool-type enum has no \"function\": {fn_tool}"
    );
    assert_eq!(fn_tool["name"], json!("fn"));
    assert_eq!(fn_tool["input_schema"], json!({"type": "object"}));
    assert_eq!(
        fn_tool["strict"],
        json!(true),
        "function-level extras land at the tool root: {fn_tool}"
    );
    assert!(fn_tool.get("function").is_none(), "{fn_tool}");

    assert!(
        tools[1].get("type").is_none(),
        "a missing type stays missing: {:?}",
        tools[1]
    );
    assert!(
        tools[2].get("type").is_none(),
        "the empty-string stand-in is omitted too: {:?}",
        tools[2]
    );
    assert_eq!(
        tools[3]["type"],
        json!("custom"),
        "non-function types pass through verbatim: {:?}",
        tools[3]
    );
    assert_no_reserved_markers(&wire);
}

// ── 6. Chat ↔ chat red line ─────────────────────────────────────

/// Red line: a chat-sourced tool without `type` must never grow one —
/// the `"function"` synthesis is for foreign (Responses / Anthropic) IR
/// only.
#[test]
fn chat_to_chat_absent_type_stays_absent_cross() {
    let ir = ChatCompletionsCodec::decode_request(&chat_request(chat_body(json!([
        {"function": {"name": "fn"}}
    ]))));
    let (wire, loss) = chat_json(&ir);
    assert!(loss.is_empty(), "loss: {:?}", loss.entries);

    let tool = &wire["tools"][0];
    assert!(
        tool.get("type").is_none(),
        "chat-sourced absent type must stay absent: {tool}"
    );
    assert_eq!(tool["function"]["name"], json!("fn"));
    assert_no_reserved_markers(&wire);
}

// ── 7. two-hop round-trips ──────────────────────────────────────

/// Responses → Chat → Responses keeps name / description / schema /
/// `strict` / extras and never leaks markers.
#[test]
fn responses_chat_responses_roundtrip_tools() {
    let body = json!({
        "model": CHAT_MODEL,
        "input": "hi",
        "tools": [{
            "type": "function",
            "name": "get_weather",
            "description": "Get weather",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
            "strict": true,
            "custom_extra": {"k": 1}
        }]
    });

    // First hop: the chat wire carries the OpenAI shape.
    let (chat_wire, loss) = responses_to_chat(body.clone());
    assert!(loss.is_empty(), "loss: {:?}", loss.entries);
    assert_eq!(chat_wire["tools"][0]["type"], json!("function"));
    assert_eq!(chat_wire["tools"][0]["function"]["strict"], json!(true));
    assert_eq!(
        chat_wire["tools"][0]["custom_extra"],
        json!({"k": 1}),
        "tool-level extras stay at the tool root: {:?}",
        chat_wire["tools"][0]
    );

    // Second hop: back on the Responses wire, everything semantic survives.
    let back = responses_chat_responses(body.clone());
    let tool = &back["tools"][0];
    assert_eq!(tool["type"], json!("function"));
    assert_eq!(tool["name"], json!("get_weather"));
    assert_eq!(tool["description"], json!("Get weather"));
    assert_eq!(tool["parameters"], body["tools"][0]["parameters"]);
    assert_eq!(
        tool["strict"],
        json!(true),
        "strict survives the chat hop and returns to the root: {tool}"
    );
    assert_eq!(tool["custom_extra"], json!({"k": 1}));
    assert!(tool.get("function").is_none(), "{tool}");
    assert_no_reserved_markers(&back);
}

/// Anthropic → Chat → Anthropic keeps name / description / schema /
/// extras; the `"function"` type the chat hop synthesizes is omitted
/// again on the way back, and an explicit `"custom"` type round-trips.
#[test]
fn anthropic_chat_anthropic_roundtrip_tools() {
    let body = json!({
        "model": ANTHROPIC_MODEL,
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [
            {
                "name": "get_weather",
                "description": "Get weather",
                "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}},
                "cache_control": {"type": "ephemeral"}
            },
            {
                "type": "custom",
                "name": "custom_tool",
                "input_schema": {"type": "object"}
            }
        ]
    });

    // First hop: chat needs a concrete type for the type-less tool.
    let (chat_wire, loss) = anthropic_to_chat(body.clone());
    assert!(loss.is_empty(), "loss: {:?}", loss.entries);
    assert_eq!(chat_wire["tools"][0]["type"], json!("function"));
    assert_eq!(
        chat_wire["tools"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );

    // Second hop: back on the Anthropic wire.
    let back = anthropic_chat_anthropic(body.clone());
    let tools = back["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 2);
    assert!(
        tools[0].get("type").is_none(),
        "the chat-hop \"function\" type is omitted on Anthropic: {:?}",
        tools[0]
    );
    assert_eq!(tools[0]["name"], json!("get_weather"));
    assert_eq!(tools[0]["description"], json!("Get weather"));
    assert_eq!(tools[0]["input_schema"], body["tools"][0]["input_schema"]);
    assert_eq!(tools[0]["cache_control"], json!({"type": "ephemeral"}));
    assert_eq!(
        tools[1]["type"],
        json!("custom"),
        "explicit custom type round-trips: {:?}",
        tools[1]
    );
    assert_eq!(tools[1]["name"], json!("custom_tool"));
    assert_eq!(tools[1]["input_schema"], json!({"type": "object"}));
    assert_no_reserved_markers(&back);
}
