//! Session-key extraction for session-sticky routing (design §4.1).
//!
//! The chain is a built-in constant — the first source that yields a
//! non-empty value wins and the rest are skipped:
//!
//! 1. `metadata.conversation_id` / `metadata.conversationId` → `meta:`
//! 2. top-level `conversation_id` / `conversationId` → `top:`
//! 3. top-level `prompt_cache_key` → `pc:`
//! 4. hash of the first user message's first text block → `fb:`
//!
//! Two hard exclusions keep sticky routing fair (workbuddy2api's design,
//! adopted verbatim):
//!
//! - `user` / `user_id` / `metadata.user_id` are **never** key sources —
//!   a single user sending many conversations must not monopolize a target.
//! - Their mere presence **suppresses the content-hash fallback**: when the
//!   request identifies a user, the conversation is unknowable from the
//!   body and a content hash would over-bind (identical openers across a
//!   user's conversations would collide into one pin).

use sag_core::{ChatCompletionRequest, IrContentBlock, IrRequest, IrRole, SessionKey, sha256_hex};
use serde_json::Value;

/// Double front-end for extraction: chat ingress carries the typed request
/// (fields read directly, unknown fields land in `extra`), while the
/// Anthropic / Responses compat ingresses only have the raw JSON body
/// (dot-path lookups). Never re-serialize a typed request just to extract.
pub enum BodyView<'a> {
    /// Typed OpenAI chat request.
    Chat(&'a ChatCompletionRequest),
    /// Raw JSON body (Anthropic Messages / Responses ingress).
    Raw(&'a Value),
}

/// Which body fields identify a *user* (not a conversation). Presence of
/// any of these suppresses the content-hash fallback; they are never used
/// as key sources.
const USER_IDENTIFIER_KEYS: &[&str] = &["user_id", "user"];

/// Run the extraction chain. `ir` is only consulted for the `fb:` content
/// hash fallback; passing `None` disables the fallback.
pub fn extract_session_key(body: BodyView<'_>, ir: Option<&IrRequest>) -> Option<SessionKey> {
    // 1. metadata.conversation_id / metadata.conversationId → meta:
    if let Some(v) = str_at(metadata_of(&body), &["conversation_id", "conversationId"]) {
        return Some(SessionKey::new("meta", v));
    }
    // 2. top-level conversation_id / conversationId → top:
    if let Some(v) = top_str(&body, &["conversation_id", "conversationId"]) {
        return Some(SessionKey::new("top", v));
    }
    // 3. top-level prompt_cache_key → pc:
    if let Some(v) = prompt_cache_key_of(&body) {
        return Some(SessionKey::new("pc", v));
    }
    // 4. content-hash fallback — suppressed when the request identifies a user.
    if has_user_identifier(&body) {
        return None;
    }
    content_hash_fallback(ir)
}

/// The `fb:` fallback: SHA-256 of the first user message's first text
/// block, normalized (trim + collapse whitespace) so trivial formatting
/// differences don't split a session. Image / document / audio blocks never
/// participate (design decision O3) and a message with no text block yields
/// no key at all.
fn content_hash_fallback(ir: Option<&IrRequest>) -> Option<SessionKey> {
    let ir = ir?;
    let first_user = ir.messages.iter().find(|m| m.role == IrRole::User)?;
    let first_text = first_user.content.iter().find_map(|block| match block {
        IrContentBlock::Text { text } => Some(text.as_str()),
        _ => None,
    })?;
    let normalized: String = first_text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    Some(SessionKey::new("fb", sha256_hex(&normalized)))
}

/// `true` when the body carries a user identifier (top-level or inside
/// `metadata`) — see the module docs for why this gates the fallback.
fn has_user_identifier(body: &BodyView<'_>) -> bool {
    USER_IDENTIFIER_KEYS
        .iter()
        .any(|k| top_has(body, k) || metadata_of(body).is_some_and(|m| lookup(m, k).is_some()))
}

/// `metadata` as a JSON object, whichever front-end we're on.
fn metadata_of<'a>(body: &BodyView<'a>) -> Option<&'a Value> {
    match body {
        BodyView::Chat(req) => req.metadata.as_ref(),
        BodyView::Raw(raw) => raw.get("metadata"),
    }
}

/// Top-level string lookup across both front-ends. The typed request's
/// unknown fields live in `extra`, so that's where `conversation_id` and
/// `prompt_cache_key`'s raw twin land on chat ingress.
fn top_str(body: &BodyView<'_>, keys: &[&str]) -> Option<String> {
    match body {
        BodyView::Chat(req) => keys
            .iter()
            .find_map(|k| req.extra.get(*k).and_then(nonempty_str)),
        BodyView::Raw(raw) => str_at(Some(raw), keys),
    }
}

/// Top-level presence check for a single key (any value shape).
fn top_has(body: &BodyView<'_>, key: &str) -> bool {
    match body {
        BodyView::Chat(req) => req.extra.get(key).is_some_and(|v| !v.is_null()),
        BodyView::Raw(raw) => lookup(raw, key).is_some(),
    }
}

/// `prompt_cache_key` is a *typed* field on chat ingress (it never lands in
/// `extra`) and a raw key on the compat ingresses.
fn prompt_cache_key_of(body: &BodyView<'_>) -> Option<String> {
    match body {
        BodyView::Chat(req) => req.prompt_cache_key.as_deref().and_then(trim_nonempty),
        BodyView::Raw(raw) => str_at(Some(raw), &["prompt_cache_key"]),
    }
}

/// Object member lookup by any of `keys` (first hit wins) with the value
/// coerced to a non-empty trimmed string.
fn str_at(obj: Option<&Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(s) = lookup(obj?, key).and_then(nonempty_str) {
            return Some(s);
        }
    }
    None
}

/// Non-empty trimmed string coercion.
fn nonempty_str(v: &Value) -> Option<String> {
    v.as_str().and_then(trim_nonempty)
}

/// Trim + non-empty filter over a raw string slice.
fn trim_nonempty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Single-key object lookup that tolerates non-object parents.
fn lookup<'a>(obj: &'a Value, key: &str) -> Option<&'a Value> {
    obj.get(key).filter(|v| !v.is_null())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sag_core::{IrMessage, Sampling};
    use serde_json::json;

    fn ir_with_user_text(text: &str) -> IrRequest {
        IrRequest {
            model: "gpt-4o".into(),
            messages: vec![IrMessage {
                role: IrRole::User,
                content: vec![IrContentBlock::Text {
                    text: text.to_string(),
                }],
                ..Default::default()
            }],
            sampling: Sampling::default(),
            ..Default::default()
        }
    }

    fn raw(body: Value) -> Value {
        body
    }

    #[test]
    fn chain_meta_wins_over_top_and_pc() {
        let body = raw(json!({
            "metadata": { "conversation_id": "conv-a" },
            "conversation_id": "conv-b",
            "prompt_cache_key": "pc-c",
        }));
        let key = extract_session_key(BodyView::Raw(&body), None).unwrap();
        assert_eq!(key.source, "meta");
        assert_eq!(key.value, "conv-a");
    }

    #[test]
    fn chain_top_wins_over_pc() {
        let body = raw(json!({
            "conversationId": "conv-b",
            "prompt_cache_key": "pc-c",
        }));
        let key = extract_session_key(BodyView::Raw(&body), None).unwrap();
        assert_eq!(key.source, "top");
        assert_eq!(key.value, "conv-b");
    }

    #[test]
    fn chain_pc_used_when_no_conversation_id() {
        let body = raw(json!({ "prompt_cache_key": "pc-c" }));
        let key = extract_session_key(BodyView::Raw(&body), None).unwrap();
        assert_eq!(key.source, "pc");
        assert_eq!(key.value, "pc-c");
    }

    #[test]
    fn empty_and_blank_values_are_skipped() {
        let body = raw(json!({
            "metadata": { "conversation_id": "   " },
            "conversation_id": "",
            "prompt_cache_key": "pc-c",
        }));
        let key = extract_session_key(BodyView::Raw(&body), None).unwrap();
        assert_eq!(key.source, "pc");
    }

    #[test]
    fn user_identifier_suppresses_fallback() {
        let ir = ir_with_user_text("hello world");
        for gate in [
            json!({ "user_id": "u-1" }),
            json!({ "user": "u-1" }),
            json!({ "metadata": { "user_id": "u-1" } }),
        ] {
            assert!(
                extract_session_key(BodyView::Raw(&gate), Some(&ir)).is_none(),
                "user identifier must suppress the hash fallback: {gate}"
            );
        }
        // ...but explicit conversation fields still win before the gate.
        let explicit = raw(json!({ "user_id": "u-1", "conversation_id": "conv" }));
        assert!(extract_session_key(BodyView::Raw(&explicit), Some(&ir)).is_some());
    }

    #[test]
    fn user_identifiers_are_never_key_sources() {
        let body = raw(json!({ "user_id": "u-1", "user": "u-2" }));
        assert!(extract_session_key(BodyView::Raw(&body), None).is_none());
    }

    #[test]
    fn fallback_hash_is_normalized_and_deterministic() {
        let a = ir_with_user_text("  hello   world \n");
        let b = ir_with_user_text("hello world");
        let ka = extract_session_key(BodyView::Raw(&json!({})), Some(&a)).unwrap();
        let kb = extract_session_key(BodyView::Raw(&json!({})), Some(&b)).unwrap();
        assert_eq!(ka.source, "fb");
        assert_eq!(ka, kb, "whitespace normalization must not split a session");

        let c = ir_with_user_text("different opener");
        let kc = extract_session_key(BodyView::Raw(&json!({})), Some(&c)).unwrap();
        assert_ne!(ka.value, kc.value);
    }

    #[test]
    fn fallback_ignores_non_text_blocks() {
        let mut ir = ir_with_user_text("hello");
        // An image block *before* the text block must not become the hash input.
        ir.messages[0].content.insert(
            0,
            IrContentBlock::Image {
                data: json!({ "url": "https://example.com/x.png" }),
            },
        );
        let with_image = extract_session_key(BodyView::Raw(&json!({})), Some(&ir)).unwrap();
        let plain =
            extract_session_key(BodyView::Raw(&json!({})), Some(&ir_with_user_text("hello")))
                .unwrap();
        assert_eq!(with_image, plain);

        // A message with only an image block yields no fallback key.
        let mut image_only = ir_with_user_text("hello");
        image_only.messages[0].content = vec![IrContentBlock::Image {
            data: json!({ "url": "https://example.com/x.png" }),
        }];
        assert!(extract_session_key(BodyView::Raw(&json!({})), Some(&image_only)).is_none());
    }

    #[test]
    fn fallback_needs_ir() {
        assert!(extract_session_key(BodyView::Raw(&json!({})), None).is_none());
    }

    #[test]
    fn first_user_message_wins() {
        let mut ir = ir_with_user_text("system-ish setup");
        ir.messages.insert(
            0,
            IrMessage {
                role: IrRole::System,
                content: vec![IrContentBlock::Text {
                    text: "you are a bot".into(),
                }],
                ..Default::default()
            },
        );
        ir.messages.push(IrMessage {
            role: IrRole::User,
            content: vec![IrContentBlock::Text {
                text: "second user turn".into(),
            }],
            ..Default::default()
        });
        let key = extract_session_key(BodyView::Raw(&json!({})), Some(&ir)).unwrap();
        let expected = extract_session_key(
            BodyView::Raw(&json!({})),
            Some(&ir_with_user_text("system-ish setup")),
        )
        .unwrap();
        assert_eq!(key, expected);
    }

    #[test]
    fn chat_typed_view_matches_raw_view() {
        // Build a typed request carrying the same session fields as a raw body.
        let mut req: ChatCompletionRequest =
            serde_json::from_value(json!({ "model": "gpt-4o", "messages": [] })).unwrap();
        req.metadata = Some(json!({ "conversation_id": "conv-a" }));
        req.prompt_cache_key = Some("pc-c".into());

        let raw_body = raw(json!({
            "metadata": { "conversation_id": "conv-a" },
            "prompt_cache_key": "pc-c",
        }));

        let typed = extract_session_key(BodyView::Chat(&req), None).unwrap();
        let raw_view = extract_session_key(BodyView::Raw(&raw_body), None).unwrap();
        assert_eq!(typed, raw_view);
        assert_eq!(typed.source, "meta");
    }

    #[test]
    fn chat_typed_extra_carries_top_level_conversation() {
        let mut req: ChatCompletionRequest =
            serde_json::from_value(json!({ "model": "gpt-4o", "messages": [] })).unwrap();
        req.extra.insert("conversation_id".into(), json!("conv-x"));
        let key = extract_session_key(BodyView::Chat(&req), None).unwrap();
        assert_eq!(key.source, "top");
        assert_eq!(key.value, "conv-x");
    }

    #[test]
    fn chat_typed_user_gate_reads_extra_and_metadata() {
        let ir = ir_with_user_text("hello");
        let mut req: ChatCompletionRequest =
            serde_json::from_value(json!({ "model": "gpt-4o", "messages": [], "user": "u-1" }))
                .unwrap();
        assert!(extract_session_key(BodyView::Chat(&req), Some(&ir)).is_none());

        req.extra.remove("user");
        req.metadata = Some(json!({ "user_id": "u-1" }));
        assert!(extract_session_key(BodyView::Chat(&req), Some(&ir)).is_none());
    }
}
