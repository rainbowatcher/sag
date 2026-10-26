//! Session registry facts — the display-side companion of [`crate::session`].
//!
//! [`crate::session::SessionKey`] hashes a session's raw identity and never
//! lets it leave unhashed. The log page's session info card needs a *human
//! readable* label for that hash, so this module builds a
//! [`SessionRegistryFact`] from the same inputs the dispatch layer already
//! holds (`SessionKey` + [`IrRequest`]):
//!
//! - `conversation_id` — the raw session value, **only** for the `meta` /
//!   `top` sources (an explicit `conversation_id` field is meant to be
//!   human-readable), truncated to [`CONVERSATION_ID_MAX_CHARS`].
//! - `first_message_preview` — a title-style preview of the first user
//!   message, markup stripped and truncated to [`PREVIEW_MAX_CHARS`].
//!
//! This is the approved PII carve-out described in [`crate::session`]'s
//! module docs: the fact is persisted in the `session_registry` table only,
//! with a retention policy. Raw values still never reach `CallLog`, the
//! affinity map or the admin snapshot.
//!
//! The preview is built from the original (**unmasked**) IR. Privacy masking
//! applies only to the content sent to the upstream model: the registry is a
//! local, admin-visible surface with its own retention policy, so it keeps the
//! raw text — settled decision, not a follow-up.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ir::{IrContentBlock, IrRequest, IrRole};
use crate::session::SessionKey;

/// Max characters kept for a conversation id (`meta` / `top` sources).
pub const CONVERSATION_ID_MAX_CHARS: usize = 128;

/// Max characters kept for the first-message preview (before the `…`).
pub const PREVIEW_MAX_CHARS: usize = 160;

/// Session sources whose raw value is a human-meaningful conversation id.
/// `pc` (prompt cache key) and `fb` (content hash) are opaque.
const DISPLAYABLE_ID_SOURCES: &[&str] = &["meta", "top"];

/// One session-registry row: the display facts for a hashed session key.
///
/// `session_key` is [`SessionKey::map_id`] (32 lowercase hex chars), so this
/// row joins `call_logs.session_key` directly. `first_seen` / `last_seen`
/// are maintained by the analytics upsert (earliest-requester-wins: the
/// opener fields stay as observed by the request with the smallest
/// `first_seen`, regardless of write order).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRegistryFact {
    /// 32-hex map id — joins `call_logs.session_key`.
    pub session_key: String,
    /// Extraction-chain source (`meta` / `top` / `pc` / `fb`).
    pub session_source: String,
    /// Truncated raw conversation id; `None` for `pc` / `fb` sources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    /// Title-style preview of the first user message; `None` when the
    /// request carried no usable text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_message_preview: Option<String>,
    /// First time this session key was observed.
    pub first_seen: DateTime<Utc>,
    /// Most recent time this session key was observed.
    pub last_seen: DateTime<Utc>,
}

/// Build the registry fact for one request.
///
/// `route_model` participates exactly as in [`SessionKey::map_id`] (the
/// caller passes the same model it hashes with), and `now` becomes both
/// `first_seen` and `last_seen` — the store's upsert keeps the smallest
/// `first_seen` and the largest `last_seen`.
/// `ir` is `None` on paths without a request IR (e.g. image generation),
/// which only drops the preview.
pub fn build_session_fact(
    key: &SessionKey,
    route_model: &str,
    ir: Option<&IrRequest>,
    now: DateTime<Utc>,
) -> SessionRegistryFact {
    SessionRegistryFact {
        session_key: key.map_id(route_model),
        session_source: key.source.to_string(),
        conversation_id: conversation_id_of(key),
        first_message_preview: ir.and_then(first_user_preview),
        first_seen: now,
        last_seen: now,
    }
}

/// Truncated raw conversation id, or `None` when the source is opaque
/// (`pc` / `fb`) or the value is blank.
fn conversation_id_of(key: &SessionKey) -> Option<String> {
    if !DISPLAYABLE_ID_SOURCES.contains(&key.source) {
        return None;
    }
    let trimmed = key.value.trim();
    (!trimmed.is_empty()).then(|| truncate_chars(trimmed, CONVERSATION_ID_MAX_CHARS))
}

/// Title-style preview of the first user message's first text block.
///
/// Markup (`<tag …>` / `</tag>`) is stripped, runs of whitespace collapse to
/// a single space, and the result is truncated to [`PREVIEW_MAX_CHARS`] with
/// a trailing `…`. A message whose text is empty once cleaned yields `None`.
///
/// Deliberately *not* the same function as the `fb:` fallback hash input
/// (`sag-router::session::content_hash_fallback`): that one hashes and keeps
/// no text, while this one strips markup and truncates for display. Both
/// read the same block, so keep the block-selection rule in sync.
pub fn first_user_preview(ir: &IrRequest) -> Option<String> {
    let first_user = ir.messages.iter().find(|m| m.role == IrRole::User)?;
    let text = first_user.content.iter().find_map(|block| match block {
        IrContentBlock::Text { text } => Some(text.as_str()),
        _ => None,
    })?;
    let cleaned = collapse_whitespace(&strip_markup(text));
    if cleaned.is_empty() {
        return None;
    }
    Some(truncate_with_ellipsis(&cleaned, PREVIEW_MAX_CHARS))
}

/// Remove XML/HTML-style tags (`<tag>`, `</tag>`, `<tag attr="v">`,
/// `<tag/>`), keeping everything else verbatim. A `<` that does not start a
/// tag is kept as literal text.
pub fn strip_markup(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<'
            && let Some(end) = tag_end(bytes, i)
        {
            i = end;
            continue;
        }
        // Copy one whole char so multi-byte text is never split.
        let ch = input[i..]
            .chars()
            .next()
            .expect("index is on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// End offset (just past `>`) of a tag starting at `start`, or `None` when
/// the `<` is not a tag opener.
fn tag_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut j = start + 1;
    if bytes.get(j) == Some(&b'/') {
        j += 1;
    }
    // A tag name starts with an ASCII letter.
    if !bytes.get(j).is_some_and(u8::is_ascii_alphabetic) {
        return None;
    }
    j += 1;
    while bytes
        .get(j)
        .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':'))
    {
        j += 1;
    }
    while let Some(&b) = bytes.get(j) {
        match b {
            b'>' => return Some(j + 1),
            // A nested `<` means this was not a tag after all.
            b'<' => return None,
            _ => j += 1,
        }
    }
    None
}

/// Collapse every run of whitespace into a single space and trim the ends.
fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate to at most `max` characters on a char boundary (no ellipsis).
pub fn truncate_chars(input: &str, max: usize) -> String {
    match input.char_indices().nth(max) {
        Some((byte_idx, _)) => input[..byte_idx].to_string(),
        None => input.to_string(),
    }
}

/// Truncate to at most `max` characters, appending `…` when text was cut.
fn truncate_with_ellipsis(input: &str, max: usize) -> String {
    match input.char_indices().nth(max) {
        Some((byte_idx, _)) => {
            let mut out = String::with_capacity(byte_idx + 3);
            out.push_str(&input[..byte_idx]);
            out.push('…');
            out
        }
        None => input.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{IrMessage, Sampling};

    fn text_block(text: &str) -> IrContentBlock {
        IrContentBlock::Text {
            text: text.to_string(),
        }
    }

    fn ir_with(messages: Vec<IrMessage>) -> IrRequest {
        IrRequest {
            model: "gpt-4o".into(),
            messages,
            sampling: Sampling::default(),
            ..Default::default()
        }
    }

    fn user_msg(blocks: Vec<IrContentBlock>) -> IrMessage {
        IrMessage {
            role: IrRole::User,
            content: blocks,
            ..Default::default()
        }
    }

    #[test]
    fn preview_takes_first_user_message_only() {
        let ir = ir_with(vec![
            IrMessage {
                role: IrRole::System,
                content: vec![text_block("you are a bot")],
                ..Default::default()
            },
            user_msg(vec![text_block("first opener")]),
            user_msg(vec![text_block("second turn")]),
        ]);
        assert_eq!(first_user_preview(&ir).as_deref(), Some("first opener"));
    }

    #[test]
    fn preview_takes_first_text_block_of_the_message() {
        let ir = ir_with(vec![user_msg(vec![
            IrContentBlock::Image {
                data: serde_json::json!({ "url": "https://example.com/x.png" }),
            },
            text_block("described text"),
            text_block("later block"),
        ])]);
        assert_eq!(first_user_preview(&ir).as_deref(), Some("described text"));
    }

    #[test]
    fn preview_strips_markup() {
        assert_eq!(strip_markup("<b>bold</b>"), "bold");
        assert_eq!(
            strip_markup(r#"<tag attr="v">inner</tag>"#),
            "inner",
            "attributes are part of the tag"
        );
        assert_eq!(strip_markup("line<br/>break"), "linebreak");
        assert_eq!(strip_markup("<svg:path d='x'/>ok"), "ok");
        // A stray '<' is literal text, not a tag opener.
        assert_eq!(strip_markup("a < b"), "a < b");
        assert_eq!(strip_markup("<3 you"), "<3 you");
    }

    #[test]
    fn preview_collapses_whitespace_and_trims() {
        let ir = ir_with(vec![user_msg(vec![text_block(
            "  hello\n\n\t world   again  ",
        )])]);
        assert_eq!(
            first_user_preview(&ir).as_deref(),
            Some("hello world again")
        );
    }

    #[test]
    fn preview_truncates_to_160_chars_with_ellipsis() {
        let long = "a".repeat(500);
        let ir = ir_with(vec![user_msg(vec![text_block(&long)])]);
        let preview = first_user_preview(&ir).unwrap();
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS + 1);
        assert!(preview.ends_with('…'));
        assert_eq!(&preview[..PREVIEW_MAX_CHARS], &long[..PREVIEW_MAX_CHARS]);

        // Exactly at the limit: no ellipsis.
        let exact = "b".repeat(PREVIEW_MAX_CHARS);
        let ir = ir_with(vec![user_msg(vec![text_block(&exact)])]);
        assert_eq!(first_user_preview(&ir).as_deref(), Some(exact.as_str()));
    }

    #[test]
    fn preview_truncation_is_utf8_safe() {
        // 200 CJK chars: each is 3 bytes, so a byte-wise cut would panic.
        let cjk = "你好世界".repeat(50);
        let preview = first_user_preview(&ir_with(vec![user_msg(vec![text_block(&cjk)])])).unwrap();
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS + 1);
        assert!(preview.ends_with('…'));

        // Emoji are 4-byte chars — same guarantee.
        let emoji = "🚀".repeat(200);
        let preview =
            first_user_preview(&ir_with(vec![user_msg(vec![text_block(&emoji)])])).unwrap();
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS + 1);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn preview_is_none_without_usable_text() {
        // No messages at all.
        assert!(first_user_preview(&ir_with(vec![])).is_none());
        // Only a system message.
        assert!(
            first_user_preview(&ir_with(vec![IrMessage {
                role: IrRole::System,
                content: vec![text_block("system only")],
                ..Default::default()
            }]))
            .is_none()
        );
        // User message with no text block (pure image).
        assert!(
            first_user_preview(&ir_with(vec![user_msg(vec![IrContentBlock::Image {
                data: serde_json::json!({ "url": "https://example.com/x.png" }),
            }])]))
            .is_none()
        );
        // Empty and whitespace-only text.
        assert!(first_user_preview(&ir_with(vec![user_msg(vec![text_block("")])])).is_none());
        assert!(
            first_user_preview(&ir_with(vec![user_msg(vec![text_block("  \n\t ")])])).is_none()
        );
        // Text that is nothing but tags.
        assert!(
            first_user_preview(&ir_with(vec![user_msg(vec![text_block(
                "<context></context><br/>"
            )])]))
            .is_none()
        );
    }

    #[test]
    fn conversation_id_only_for_displayable_sources() {
        let now = Utc::now();
        for source in ["meta", "top"] {
            let fact = build_session_fact(&SessionKey::new(source, "conv-1"), "gpt-4o", None, now);
            assert_eq!(fact.conversation_id.as_deref(), Some("conv-1"), "{source}");
            assert_eq!(fact.session_source, source);
        }
        for source in ["pc", "fb"] {
            let fact = build_session_fact(
                &SessionKey::new(source, "0123456789abcdef"),
                "gpt-4o",
                None,
                now,
            );
            assert_eq!(fact.conversation_id, None, "{source} is opaque");
        }
    }

    #[test]
    fn conversation_id_truncated_to_128_chars() {
        let long = "c".repeat(300);
        let key = SessionKey::new("meta", long.clone());
        let fact = build_session_fact(&key, "gpt-4o", None, Utc::now());
        let id = fact.conversation_id.unwrap();
        assert_eq!(id.chars().count(), CONVERSATION_ID_MAX_CHARS);
        assert_eq!(id, long[..CONVERSATION_ID_MAX_CHARS]);

        // Multi-byte id: cut on a char boundary, no panic.
        let cjk = "会话标识".repeat(50);
        let fact = build_session_fact(&SessionKey::new("top", cjk), "gpt-4o", None, Utc::now());
        assert_eq!(
            fact.conversation_id.unwrap().chars().count(),
            CONVERSATION_ID_MAX_CHARS
        );
    }

    #[test]
    fn conversation_id_blank_value_is_none() {
        let fact = build_session_fact(&SessionKey::new("meta", "   "), "gpt-4o", None, Utc::now());
        assert_eq!(fact.conversation_id, None);
    }

    #[test]
    fn fact_key_matches_session_map_id_and_stamps_now() {
        let now = Utc::now();
        let key = SessionKey::new("meta", "conv-1");
        let ir = ir_with(vec![user_msg(vec![text_block("<p>hi  there</p>")])]);
        let fact = build_session_fact(&key, "claude-sonnet", Some(&ir), now);
        assert_eq!(fact.session_key, key.map_id("claude-sonnet"));
        assert_eq!(fact.session_key.len(), 32);
        assert_eq!(fact.first_seen, now);
        assert_eq!(fact.last_seen, now);
        assert_eq!(fact.first_message_preview.as_deref(), Some("hi there"));
    }

    #[test]
    fn fact_without_ir_has_no_preview() {
        let fact = build_session_fact(
            &SessionKey::new("top", "conv-2"),
            "gpt-4o",
            None,
            Utc::now(),
        );
        assert_eq!(fact.first_message_preview, None);
    }
}
