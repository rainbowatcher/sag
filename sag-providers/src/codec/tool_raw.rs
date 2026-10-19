//! Shared `IrTool.raw` extra-field access for the three codecs.
//!
//! `IrTool` (sag-core) carries only `{name, description, input_schema, raw}`;
//! every extra field of a wire tool definition lives in `raw`, and two
//! conventions coexist:
//!
//! * **dual layer** — chat source: `{"__tool_extra": {...}, "__function_extra": {...}}`,
//!   both keys always present (objects may be empty), so chat encode can
//!   restore each key to its original position (tool root vs. inside
//!   `function`).
//! * **flat** — responses / anthropic source: every extra key sits directly on
//!   `raw` (`type`, `strict`, `cache_control`, …).
//!
//! [`tool_extras`] reads either convention; [`merge_extras`] flattens either
//! convention onto an outbound tool root, for targets whose tool shape has no
//! nested `function` object. The reserved `__`-prefixed keys are structural
//! markers only and are never emitted on the wire.
//!
//! The discriminator is the **presence** of the `__tool_extra` key — a
//! structural marker, never a value-level heuristic such as "`type` is
//! missing".

use serde_json::{Map, Value};

use sag_core::IrTool;

/// Reserved key holding chat-source tool-level extras.
pub(crate) const TOOL_EXTRA_KEY: &str = "__tool_extra";
/// Reserved key holding chat-source function-level extras.
pub(crate) const FUNCTION_EXTRA_KEY: &str = "__function_extra";

/// `true` when `raw` uses the dual-layer (chat-source) layout.
pub(crate) fn is_dual_layer(t: &IrTool) -> bool {
    t.raw.contains_key(TOOL_EXTRA_KEY)
}

/// `true` for the reserved structural marker keys, which must never appear in
/// outbound JSON.
pub(crate) fn is_reserved_key(k: &str) -> bool {
    k == TOOL_EXTRA_KEY || k == FUNCTION_EXTRA_KEY
}

/// Read the extra fields of `t.raw`, tolerating both conventions.
///
/// Returns `(tool level, function level)`:
///
/// * dual layer → the two stored objects verbatim (empty when the key is
///   present but not an object);
/// * flat → tool level is a copy of `raw` (including `type` etc.) with
///   `strict` moved to function level, since OpenAI puts `strict` inside
///   `function`.
pub(crate) fn tool_extras(t: &IrTool) -> (Map<String, Value>, Map<String, Value>) {
    if is_dual_layer(t) {
        let object_at = |key: &str| match t.raw.get(key) {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
        return (object_at(TOOL_EXTRA_KEY), object_at(FUNCTION_EXTRA_KEY));
    }
    let mut tool_level = t.raw.clone();
    // Defensive: a stray marker without `__tool_extra` must not leak.
    tool_level.remove(TOOL_EXTRA_KEY);
    tool_level.remove(FUNCTION_EXTRA_KEY);
    let mut function_level = Map::new();
    if let Some(strict) = tool_level.remove("strict") {
        function_level.insert("strict".into(), strict);
    }
    (tool_level, function_level)
}

/// Tool-level `type` from either convention: dual layer → `__tool_extra.type`,
/// flat → `raw.type`. `None` when the source carried no `type` at all.
pub(crate) fn tool_raw_type(t: &IrTool) -> Option<&str> {
    let holder = if is_dual_layer(t) {
        match t.raw.get(TOOL_EXTRA_KEY) {
            Some(Value::Object(m)) => m,
            _ => return None,
        }
    } else {
        &t.raw
    };
    holder.get("type").and_then(|v| v.as_str())
}

/// Flatten both extra levels onto `root` (`or_insert` semantics: fields the
/// caller already wrote — `name`, `description`, `parameters`/`input_schema`,
/// `type` — win). Used by targets whose tool shape has no nested `function`
/// object, so tool level and function level both land at the root.
pub(crate) fn merge_extras(root: &mut Map<String, Value>, t: &IrTool) {
    let (tool_level, function_level) = tool_extras(t);
    for (k, v) in tool_level.into_iter().chain(function_level) {
        if is_reserved_key(&k) {
            continue;
        }
        root.entry(k).or_insert(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(raw: Value) -> IrTool {
        IrTool {
            name: "fn".into(),
            description: Some("d".into()),
            input_schema: json!({"type": "object"}),
            raw: raw.as_object().expect("raw object").clone(),
        }
    }

    fn dual(tool_extra: Value, function_extra: Value) -> IrTool {
        tool(json!({
            TOOL_EXTRA_KEY: tool_extra,
            FUNCTION_EXTRA_KEY: function_extra,
        }))
    }

    #[test]
    fn dual_layer_extras_are_read_per_level() {
        let t = dual(
            json!({"type": "function", "custom": 1}),
            json!({"strict": true}),
        );
        let (tool_level, function_level) = tool_extras(&t);
        assert_eq!(
            Value::Object(tool_level),
            json!({"type": "function", "custom": 1})
        );
        assert_eq!(Value::Object(function_level), json!({"strict": true}));
    }

    #[test]
    fn flat_extras_move_strict_to_function_level() {
        let t = tool(json!({"type": "custom", "strict": true, "max_uses": 5}));
        let (tool_level, function_level) = tool_extras(&t);
        assert_eq!(
            Value::Object(tool_level),
            json!({"type": "custom", "max_uses": 5})
        );
        assert_eq!(Value::Object(function_level), json!({"strict": true}));
    }

    #[test]
    fn flat_extras_without_strict_leave_function_level_empty() {
        let t = tool(json!({"type": "function"}));
        let (tool_level, function_level) = tool_extras(&t);
        assert_eq!(Value::Object(tool_level), json!({"type": "function"}));
        assert!(function_level.is_empty());
    }

    /// Empty-but-present `__tool_extra` is still the dual-layer layout: the
    /// discriminator is key presence, not content.
    #[test]
    fn empty_tool_extra_is_still_dual_layer() {
        let t = dual(json!({}), json!({}));
        let (tool_level, function_level) = tool_extras(&t);
        assert!(tool_level.is_empty(), "got {tool_level:?}");
        assert!(function_level.is_empty(), "got {function_level:?}");

        let mut t2 = dual(json!({}), json!({}));
        t2.raw.insert("stray".into(), json!(1));
        let (tool_level, _) = tool_extras(&t2);
        assert!(
            tool_level.is_empty(),
            "flat fallback must not kick in: {tool_level:?}"
        );
    }

    #[test]
    fn merge_extras_flattens_dual_layer_to_root() {
        let t = dual(
            json!({"type": "custom", "custom": 1}),
            json!({"strict": true}),
        );
        let mut root = Map::new();
        root.insert("name".into(), json!("fn"));
        merge_extras(&mut root, &t);
        assert_eq!(
            Value::Object(root),
            json!({"name": "fn", "type": "custom", "custom": 1, "strict": true})
        );
    }

    #[test]
    fn merge_extras_flattens_flat_raw_to_root() {
        let t = tool(
            json!({"type": "function", "strict": true, "cache_control": {"type": "ephemeral"}}),
        );
        let mut root = Map::new();
        root.insert("name".into(), json!("fn"));
        merge_extras(&mut root, &t);
        assert_eq!(
            Value::Object(root),
            json!({
                "name": "fn",
                "type": "function",
                "strict": true,
                "cache_control": {"type": "ephemeral"}
            })
        );
    }

    #[test]
    fn merge_extras_keeps_caller_fields() {
        let t = dual(json!({"type": "custom"}), json!({"strict": true}));
        let mut root = Map::new();
        root.insert("type".into(), json!("function"));
        merge_extras(&mut root, &t);
        assert_eq!(root.get("type"), Some(&json!("function")), "or_insert wins");
        assert_eq!(root.get("strict"), Some(&json!(true)));
    }

    /// Reserved markers must never reach outbound JSON, not even when a flat
    /// `raw` carries a stray marker without `__tool_extra`.
    #[test]
    fn merge_extras_never_emits_reserved_keys() {
        for t in [
            dual(json!({"type": "function"}), json!({"strict": true})),
            tool(json!({"type": "function"})),
            tool(json!({"type": "function", FUNCTION_EXTRA_KEY: {"strict": true}})),
        ] {
            let mut root = Map::new();
            merge_extras(&mut root, &t);
            for k in root.keys() {
                assert!(!is_reserved_key(k), "reserved key leaked: {k}");
            }
        }
    }

    #[test]
    fn tool_raw_type_reads_both_conventions() {
        assert_eq!(
            tool_raw_type(&dual(json!({"type": "custom"}), json!({}))),
            Some("custom")
        );
        assert_eq!(
            tool_raw_type(&tool(json!({"type": "web_search_preview"}))),
            Some("web_search_preview")
        );
        assert_eq!(tool_raw_type(&dual(json!({}), json!({}))), None);
        assert_eq!(tool_raw_type(&tool(json!({}))), None);
    }

    #[test]
    fn is_dual_layer_uses_key_presence_only() {
        assert!(is_dual_layer(&dual(json!({}), json!({}))));
        assert!(!is_dual_layer(&tool(json!({"type": "function"}))));
    }
}
