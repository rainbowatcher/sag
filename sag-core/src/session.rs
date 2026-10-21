//! Session identity for session-sticky routing.
//!
//! A [`SessionKey`] is extracted from a request body by the ingress layer
//! (see `sag-router::session`) and carried through dispatch to the
//! session-affinity state. Two invariants live here:
//!
//! - **No PII ever leaves this type unhashed.** The affinity map key is the
//!   truncated digest from [`SessionKey::map_id`] — raw values (conversation
//!   ids, prompt cache keys, content-derived hashes) never enter shared
//!   state or the admin snapshot.
//! - **The digest is content-addressed and stable** across restarts and
//!   instances, so a sticky pin survives a process restart conceptually
//!   (the LRU itself is in-memory, see the design doc's follow-ups).

use sha2::{Digest, Sha256};

/// Session identity extracted from a request body.
///
/// `source` is the extraction-chain prefix — which body field the value
/// came from: `meta` (`metadata.conversation_id`), `top` (top-level
/// `conversation_id`), `pc` (`prompt_cache_key`) or `fb` (content-hash
/// fallback). The prefix participates in the map id so two different
/// sources never collide even when their raw values match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionKey {
    /// Extraction-chain source prefix (`meta` / `top` / `pc` / `fb`).
    pub source: &'static str,
    /// The raw value as found in the request. Never logged, never stored —
    /// only fed into [`SessionKey::map_id`].
    pub value: String,
}

impl SessionKey {
    /// Build a key from a source prefix and a raw value.
    pub fn new(source: &'static str, value: impl Into<String>) -> Self {
        Self {
            source,
            value: value.into(),
        }
    }

    /// Stable 128-bit identity (32 lowercase hex chars) used as the
    /// affinity-map key and the `call_logs.session_key` column:
    /// `sha256(source | route_model | value)` truncated to 16 bytes.
    ///
    /// `route_model` participates so a session asked as two different
    /// models (the same `conversation_id`) gets independent pins per
    /// route — cross-model affinity is explicitly out of scope (design
    /// decision O1).
    pub fn map_id(&self, route_model: &str) -> String {
        let digest = Sha256::digest(format!("{}|{}|{}", self.source, route_model, self.value));
        // 16 bytes = 128 bits = 32 hex chars.
        format!("{digest:x}")[..32].to_string()
    }
}

/// SHA-256 hex digest of a string — used by the content-hash extraction
/// fallback (`fb:` source) and available for admin tooling.
pub fn sha256_hex(input: &str) -> String {
    format!("{:x}", Sha256::digest(input))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_id_is_stable_and_source_scoped() {
        let a = SessionKey::new("meta", "conv-1");
        let b = SessionKey::new("top", "conv-1");
        // Same raw value, different sources -> different ids.
        assert_ne!(a.map_id("gpt-4o"), b.map_id("gpt-4o"));
        // Stable across calls.
        assert_eq!(a.map_id("gpt-4o"), a.map_id("gpt-4o"));
        // Route-scoped: same session asked as two models gets two pins.
        assert_ne!(a.map_id("gpt-4o"), a.map_id("claude-sonnet"));
        // 32 hex chars.
        assert_eq!(a.map_id("gpt-4o").len(), 32);
        assert!(a.map_id("gpt-4o").chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn map_id_contains_no_raw_value() {
        let key = SessionKey::new("meta", "super-secret-conversation-id");
        let id = key.map_id("gpt-4o");
        assert!(!id.contains("secret"));
        assert_ne!(id, key.value);
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        // sha256("abc") — NIST vector.
        assert_eq!(
            sha256_hex("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
