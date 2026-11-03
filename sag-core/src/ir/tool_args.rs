//! Tool-call argument quality predicates.
//!
//! Single source of truth for the question "does this tool call actually carry
//! usable `arguments`?". Upstream providers do emit tool calls whose arguments
//! are empty or unparseable, and such a call must not be counted as a successful
//! answer. This module is pure: no I/O, no logging, no side effects — callers
//! (`sag-dispatch`) own the emission.

use std::collections::BTreeMap;

use serde_json::Value;

use super::stream::IrStreamEvent;

/// Quality of a tool call's `arguments` payload.
///
/// Variant order is the severity order (`Ok` < `Blank` < `NotJson`), which
/// [`ToolArgWatch::quality`] relies on to pick the worst observed call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ToolArgsQuality {
    /// Arguments are a JSON object. The empty object `{}` is valid, not blank.
    Ok,
    /// Arguments are missing, empty, or whitespace-only.
    Blank,
    /// Arguments are present but not a JSON object (`null`, `"x"`, `[1]`,
    /// `42`, `true`, or malformed text such as `{bad`).
    NotJson,
}

/// Classify a raw argument string, either a streamed delta or an accumulated one.
///
/// The string is trimmed first; an empty result is [`ToolArgsQuality::Blank`].
/// Anything that does not parse as a JSON **object** is
/// [`ToolArgsQuality::NotJson`] — tool arguments are always an object, so
/// `null` / `"x"` / `[1]` / `42` are as unusable as syntactically broken text.
pub fn args_quality(s: &str) -> ToolArgsQuality {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return ToolArgsQuality::Blank;
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Object(_)) => ToolArgsQuality::Ok,
        Ok(_) | Err(_) => ToolArgsQuality::NotJson,
    }
}

/// Classify an already-parsed JSON `arguments` value.
///
/// A [`Value::String`] is re-classified as raw text: the chat codec only falls
/// back to a string when the upstream `arguments` failed to parse (or when the
/// upstream sent a JSON string), so both cases are malformed. An object is
/// [`ToolArgsQuality::Ok`]; every other shape is [`ToolArgsQuality::NotJson`].
pub fn args_value_quality(v: &Value) -> ToolArgsQuality {
    match v {
        Value::String(s) => args_quality(s),
        Value::Object(_) => ToolArgsQuality::Ok,
        Value::Null | Value::Array(_) | Value::Number(_) | Value::Bool(_) => {
            ToolArgsQuality::NotJson
        }
    }
}

/// Accumulates tool-call argument evidence from a stream and reports its quality.
///
/// Each distinct tool-call index is tracked once. A call that is started but
/// never receives an `arguments_delta` has no arguments at all, so its quality
/// is [`ToolArgsQuality::Blank`].
#[derive(Debug, Default)]
pub struct ToolArgWatch {
    calls: BTreeMap<u32, TrackedCall>,
}

/// Per-call accumulation state, keyed by the IR tool-call index.
#[derive(Debug, Default)]
struct TrackedCall {
    /// Concatenated `arguments_delta` text; empty until the first delta.
    args: String,
}

impl ToolArgWatch {
    /// Create an empty watch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one stream event. Events unrelated to tool calls are ignored.
    ///
    /// [`IrStreamEvent::ToolCallStart`] registers the call (so a call with no
    /// deltas is still counted); [`IrStreamEvent::ToolCallDelta`] appends to
    /// that call's accumulator. Both cases delegate to the raw-observation API
    /// ([`Self::observe_tool_call_start`] / [`Self::observe_arguments`]), which
    /// is the single implementation of the accumulation logic.
    pub fn observe(&mut self, ev: &IrStreamEvent) {
        match ev {
            IrStreamEvent::ToolCallStart { index, .. } => {
                self.observe_tool_call_start(*index);
            }
            IrStreamEvent::ToolCallDelta {
                index,
                arguments_delta,
            } => {
                self.observe_arguments(*index, arguments_delta);
            }
            _ => {}
        }
    }

    /// Record that a tool call with the given `index` exists in this stream.
    ///
    /// This is the raw-observation entry point for callers that only have raw
    /// SSE frames, not [`IrStreamEvent`]s. Registering the same `index` again is
    /// idempotent and never clears already-accumulated arguments. A registered
    /// call with no arguments classifies as [`ToolArgsQuality::Blank`].
    pub fn observe_tool_call_start(&mut self, index: u32) {
        self.calls.entry(index).or_default();
    }

    /// Append an arguments fragment to the tool call at `index`.
    ///
    /// This is the raw-observation entry point for callers that only have raw
    /// SSE frames. A delta proves the call exists, so an unregistered `index` is
    /// registered implicitly. An empty `delta` is still recorded as evidence
    /// that the call was streamed with no arguments — it leaves the accumulator
    /// unchanged but must not be skipped.
    pub fn observe_arguments(&mut self, index: u32, delta: &str) {
        self.calls.entry(index).or_default().args.push_str(delta);
    }

    /// Worst quality across all observed tool calls; [`ToolArgsQuality::Ok`]
    /// when no tool call was observed.
    pub fn quality(&self) -> ToolArgsQuality {
        self.calls
            .values()
            .map(|call| args_quality(&call.args))
            .max()
            .unwrap_or(ToolArgsQuality::Ok)
    }

    /// Whether any tool call was observed at all.
    pub fn saw_tool_call(&self) -> bool {
        !self.calls.is_empty()
    }

    /// Number of distinct tool calls tracked.
    pub fn tool_call_count(&self) -> usize {
        self.calls.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn blank_arguments_are_blank_and_empty_object_is_ok() {
        assert_eq!(args_quality(""), ToolArgsQuality::Blank);
        assert_eq!(args_quality("   "), ToolArgsQuality::Blank);
        assert_eq!(args_quality("\n\t"), ToolArgsQuality::Blank);
        assert_eq!(args_quality("{}"), ToolArgsQuality::Ok);
        assert_eq!(args_quality("{\"city\":\"SF\"}"), ToolArgsQuality::Ok);
    }

    #[test]
    fn malformed_arguments_are_not_json() {
        assert_eq!(args_quality("{bad"), ToolArgsQuality::NotJson);
        assert_eq!(args_quality("null"), ToolArgsQuality::NotJson);
        assert_eq!(args_quality("\"x\""), ToolArgsQuality::NotJson);
        assert_eq!(args_quality("[1]"), ToolArgsQuality::NotJson);
        assert_eq!(args_quality("42"), ToolArgsQuality::NotJson);
    }

    #[test]
    fn value_quality_handles_string_and_object() {
        assert_eq!(args_value_quality(&json!("{\"a\":1}")), ToolArgsQuality::Ok);
        assert_eq!(args_value_quality(&json!("")), ToolArgsQuality::Blank);
        assert_eq!(args_value_quality(&json!("{bad")), ToolArgsQuality::NotJson);
        assert_eq!(args_value_quality(&json!({})), ToolArgsQuality::Ok);
        assert_eq!(args_value_quality(&json!(null)), ToolArgsQuality::NotJson);
    }

    #[test]
    fn watch_flags_started_call_with_no_deltas() {
        let mut watch = ToolArgWatch::new();
        watch.observe(&IrStreamEvent::ToolCallStart {
            index: 0,
            id: Some("c1".into()),
            name: Some("get_weather".into()),
        });

        assert!(watch.saw_tool_call());
        assert_eq!(watch.tool_call_count(), 1);
        assert_eq!(watch.quality(), ToolArgsQuality::Blank);
    }

    #[test]
    fn watch_accumulates_deltas() {
        let mut watch = ToolArgWatch::new();
        watch.observe(&IrStreamEvent::ToolCallStart {
            index: 0,
            id: Some("c1".into()),
            name: Some("get_weather".into()),
        });
        watch.observe(&IrStreamEvent::ToolCallDelta {
            index: 0,
            arguments_delta: "{\"city\":".into(),
        });
        watch.observe(&IrStreamEvent::ToolCallDelta {
            index: 0,
            arguments_delta: "\"SF\"}".into(),
        });

        assert_eq!(watch.tool_call_count(), 1);
        assert_eq!(watch.quality(), ToolArgsQuality::Ok);
    }

    #[test]
    fn watch_reports_worst_quality() {
        let mut watch = ToolArgWatch::new();
        watch.observe(&IrStreamEvent::ToolCallStart {
            index: 0,
            id: Some("c1".into()),
            name: Some("fn".into()),
        });
        watch.observe(&IrStreamEvent::ToolCallDelta {
            index: 0,
            arguments_delta: "{}".into(),
        });
        watch.observe(&IrStreamEvent::ToolCallStart {
            index: 1,
            id: Some("c2".into()),
            name: Some("fn".into()),
        });
        watch.observe(&IrStreamEvent::ToolCallDelta {
            index: 1,
            arguments_delta: "{bad".into(),
        });

        assert_eq!(watch.tool_call_count(), 2);
        assert_eq!(watch.quality(), ToolArgsQuality::NotJson);
    }

    #[test]
    fn observe_arguments_without_start_registers_call() {
        let mut watch = ToolArgWatch::new();
        watch.observe_arguments(0, "{\"a\":1}");

        assert!(watch.saw_tool_call());
        assert_eq!(watch.tool_call_count(), 1);
        assert_eq!(watch.quality(), ToolArgsQuality::Ok);
    }

    #[test]
    fn repeated_start_does_not_reset_accumulated_arguments() {
        let mut watch = ToolArgWatch::new();
        watch.observe_tool_call_start(0);
        watch.observe_arguments(0, "{\"a\":");
        watch.observe_tool_call_start(0);
        watch.observe_arguments(0, "1}");

        assert_eq!(watch.quality(), ToolArgsQuality::Ok);
    }

    #[test]
    fn empty_deltas_only_yield_blank() {
        let mut watch = ToolArgWatch::new();
        watch.observe_tool_call_start(0);
        watch.observe_arguments(0, "");
        watch.observe_arguments(0, "");

        assert!(watch.saw_tool_call());
        assert_eq!(watch.tool_call_count(), 1);
        assert_eq!(watch.quality(), ToolArgsQuality::Blank);
    }

    #[test]
    fn raw_api_agrees_with_ir_api() {
        let deltas = [(0u32, "{\"city\":"), (0u32, "\"SF\"}"), (1u32, "{bad")];

        let mut via_ir = ToolArgWatch::new();
        for (index, delta) in deltas {
            via_ir.observe(&IrStreamEvent::ToolCallStart {
                index,
                id: Some(format!("c{index}")),
                name: Some("fn".into()),
            });
            via_ir.observe(&IrStreamEvent::ToolCallDelta {
                index,
                arguments_delta: delta.into(),
            });
        }

        let mut via_raw = ToolArgWatch::new();
        for (index, delta) in deltas {
            via_raw.observe_tool_call_start(index);
            via_raw.observe_arguments(index, delta);
        }

        assert_eq!(via_raw.quality(), via_ir.quality());
        assert_eq!(via_raw.tool_call_count(), via_ir.tool_call_count());
        assert_eq!(via_raw.saw_tool_call(), via_ir.saw_tool_call());
    }

    #[test]
    fn multiple_indices_accumulate_independently() {
        let mut watch = ToolArgWatch::new();
        watch.observe_tool_call_start(0);
        watch.observe_arguments(0, "{\"a\":1}");
        watch.observe_tool_call_start(1);
        watch.observe_arguments(1, "{bad");

        assert_eq!(watch.tool_call_count(), 2);
        assert_eq!(watch.quality(), ToolArgsQuality::NotJson);
    }

    #[test]
    fn watch_with_no_tool_calls_is_ok() {
        let mut watch = ToolArgWatch::new();
        watch.observe(&IrStreamEvent::TextDelta {
            index: 0,
            text: "hello".into(),
        });

        assert!(!watch.saw_tool_call());
        assert_eq!(watch.tool_call_count(), 0);
        assert_eq!(watch.quality(), ToolArgsQuality::Ok);
    }
}
