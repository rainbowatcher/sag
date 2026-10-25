//! Deterministic privacy placeholders.
//!
//! A placeholder stands in for a piece of sensitive text on its way to an
//! upstream provider. It is *stable* (same kind + text + salt always yields
//! the same placeholder) so that multi-turn conversations keep a coherent
//! view of the same entity, and it is *opaque* (the digest is salted and
//! truncated) so the provider cannot recover the plaintext from it.
//!
//! The wire format is `[[{KIND}_{h12}]]`:
//!
//! - `KIND` — the detector kind uppercased (`EMAIL`, `PHONE`, ...).
//! - `h12` — the first 12 lowercase hex chars of
//!   `sha256("{kind_lowercase}:{text}:{salt}")`.
//!
//! [`PlaceholderMap`] is the reverse index (placeholder → plaintext) used by
//! [`crate::restore`] to put the plaintext back into the response stream.

use std::collections::HashMap;

use sha2::{Digest, Sha256};

/// Number of hex characters of the SHA-256 digest kept in a placeholder.
pub const HASH_HEX_LEN: usize = 12;

/// Build the placeholder for `text` of detector `kind` under `salt`.
///
/// `kind` is case-insensitive: the digest input uses the lowercased kind and
/// the rendered placeholder uses the uppercased one, so `EMAIL` and `email`
/// produce the same placeholder.
pub fn make(kind: &str, text: &str, salt: &str) -> String {
    let seed = format!("{}:{}:{}", kind.to_lowercase(), text, salt);
    let digest = format!("{:x}", Sha256::digest(seed));
    // `digest` is 64 hex chars; the truncation is safe and total.
    let hash = &digest[..HASH_HEX_LEN];
    format!("[[{}_{}]]", kind.to_uppercase(), hash)
}

/// Length in bytes of the placeholder rendered for `kind`.
///
/// `[[` + KIND + `_` + 12 hex + `]]`. The length is measured on the
/// *uppercased* kind, exactly as [`make`] renders it: uppercasing can change
/// the byte length (`ß` becomes `SS`, `ﬁ` becomes `FI`), so `kind.len()`
/// would be off for non-ASCII kinds.
pub fn placeholder_len(kind: &str) -> usize {
    2 + kind.to_uppercase().len() + 1 + HASH_HEX_LEN + 2
}

/// Reverse index from placeholder to the plaintext it stands for.
#[derive(Default, Clone)]
pub struct PlaceholderMap {
    inner: HashMap<String, String>,
}

impl PlaceholderMap {
    /// Record that `placeholder` stands for `plaintext`.
    pub fn insert(&mut self, placeholder: String, plaintext: String) {
        self.inner.insert(placeholder, plaintext);
    }

    /// Merge every entry of `other` into `self`.
    ///
    /// Placeholders are deterministic ([`make`] derives them from kind +
    /// plaintext + salt), so a key collision means both maps recorded the
    /// same plaintext; the entry already present in `self` is kept either
    /// way, which makes the merge order-independent.
    pub fn extend(&mut self, other: &PlaceholderMap) {
        for (placeholder, plaintext) in &other.inner {
            self.inner
                .entry(placeholder.clone())
                .or_insert_with(|| plaintext.clone());
        }
    }

    /// Number of recorded placeholders.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether no placeholder is recorded.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Replace every recorded placeholder occurring in `text` with its
    /// plaintext, returning the result and the number of replacements made.
    ///
    /// Scanning is single-pass over `text`, so plaintext inserted by an
    /// earlier replacement is never re-scanned; when one recorded placeholder
    /// is a prefix of another the longer one wins.
    pub fn replace_in(&self, text: &str) -> (String, usize) {
        if self.inner.is_empty() || text.is_empty() {
            return (text.to_string(), 0);
        }
        let mut keys: Vec<&str> = self.inner.keys().map(String::as_str).collect();
        keys.sort_by_key(|key| std::cmp::Reverse(key.len()));

        let mut out = String::with_capacity(text.len());
        let mut count = 0;
        let mut idx = 0;
        while idx < text.len() {
            // `idx` always sits on a char boundary, so the slice is valid.
            let rest = &text[idx..];
            match keys.iter().find(|key| rest.starts_with(**key)) {
                Some(key) => {
                    out.push_str(&self.inner[*key]);
                    idx += key.len();
                    count += 1;
                }
                None => {
                    let ch = rest.chars().next().expect("idx is in bounds");
                    out.push(ch);
                    idx += ch.len_utf8();
                }
            }
        }
        (out, count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_is_deterministic_and_salted() {
        let a = make("email", "alice@example.com", "salt-1");
        let b = make("email", "alice@example.com", "salt-1");
        assert_eq!(a, b);

        let other_salt = make("email", "alice@example.com", "salt-2");
        assert_ne!(a, other_salt);

        let other_text = make("email", "bob@example.com", "salt-1");
        assert_ne!(a, other_text);
    }

    #[test]
    fn make_kind_is_case_insensitive() {
        let lower = make("email", "alice@example.com", "salt-1");
        let upper = make("EMAIL", "alice@example.com", "salt-1");
        let mixed = make("Email", "alice@example.com", "salt-1");
        assert_eq!(lower, upper);
        assert_eq!(lower, mixed);
        assert!(lower.starts_with("[[EMAIL_"));
    }

    #[test]
    fn make_renders_kind_and_twelve_hex_chars() {
        let placeholder = make("phone", "+1 555 0100", "s");
        assert!(placeholder.starts_with("[[PHONE_"));
        assert!(placeholder.ends_with("]]"));
        let hash = placeholder
            .trim_start_matches("[[PHONE_")
            .trim_end_matches("]]");
        assert_eq!(hash.len(), HASH_HEX_LEN);
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        // Placeholder never contains the plaintext.
        assert!(!placeholder.contains("555"));
    }

    #[test]
    fn make_matches_known_digest_prefix() {
        // sha256("email:alice@example.com:salt-1") first 12 hex chars.
        let expected = &format!(
            "{:x}",
            Sha256::digest("email:alice@example.com:salt-1".as_bytes())
        )[..HASH_HEX_LEN];
        assert_eq!(
            make("email", "alice@example.com", "salt-1"),
            format!("[[EMAIL_{expected}]]")
        );
    }

    #[test]
    fn placeholder_len_matches_make() {
        for kind in ["email", "phone", "PERSON", "id"] {
            assert_eq!(make(kind, "value", "salt").len(), placeholder_len(kind));
        }
        assert_eq!(placeholder_len("email"), 2 + 5 + 1 + 12 + 2);
    }

    #[test]
    fn placeholder_len_counts_bytes_of_the_uppercased_kind() {
        // Non-ASCII kinds: `to_uppercase` may change the byte length, so the
        // length must be taken from the rendered placeholder, not `kind`.
        for kind in ["工号", "straße", "ﬁle", "ÉMAIL", "π"] {
            let rendered = make(kind, "value", "salt");
            assert_eq!(rendered.len(), placeholder_len(kind), "kind {kind}");
        }
        // `工号` is 6 bytes: 2 + 6 + 1 + 12 + 2.
        assert_eq!(placeholder_len("工号"), 23);
    }

    #[test]
    fn extend_merges_maps_and_keeps_existing_entries() {
        let alice = make("email", "alice@example.com", "s");
        let bob = make("email", "bob@example.com", "s");

        let mut left = PlaceholderMap::default();
        left.insert(alice.clone(), "alice@example.com".to_string());

        let mut right = PlaceholderMap::default();
        right.insert(bob.clone(), "bob@example.com".to_string());
        right.insert(alice.clone(), "alice@example.com".to_string());

        left.extend(&right);
        assert_eq!(left.len(), 2);
        let (out, hits) = left.replace_in(&format!("{alice} {bob}"));
        assert_eq!(out, "alice@example.com bob@example.com");
        assert_eq!(hits, 2);

        // An empty source changes nothing, and merging into an empty map
        // copies everything over.
        let before = left.len();
        left.extend(&PlaceholderMap::default());
        assert_eq!(left.len(), before);

        let mut empty = PlaceholderMap::default();
        empty.extend(&right);
        assert_eq!(empty.len(), 2);
        let (out, hits) = empty.replace_in(&alice);
        assert_eq!(out, "alice@example.com");
        assert_eq!(hits, 1);
    }

    #[test]
    fn replace_in_handles_multiple_and_adjacent_placeholders() {
        let mut map = PlaceholderMap::default();
        assert!(map.is_empty());
        let alice = make("email", "alice@example.com", "s");
        let bob = make("email", "bob@example.com", "s");
        map.insert(alice.clone(), "alice@example.com".to_string());
        map.insert(bob.clone(), "bob@example.com".to_string());
        assert_eq!(map.len(), 2);
        assert!(!map.is_empty());

        // Multiple occurrences of the same placeholder.
        let text = format!("cc {alice} and {alice} ok");
        let (out, hits) = map.replace_in(&text);
        assert_eq!(out, "cc alice@example.com and alice@example.com ok");
        assert_eq!(hits, 2);

        // Adjacent placeholders, no separator between them.
        let (out, hits) = map.replace_in(&format!("{alice}{bob}"));
        assert_eq!(out, "alice@example.combob@example.com");
        assert_eq!(hits, 2);
    }

    #[test]
    fn replace_in_without_hit_returns_input_unchanged() {
        let mut map = PlaceholderMap::default();
        map.insert(
            make("email", "alice@example.com", "s"),
            "alice@example.com".to_string(),
        );
        let text = "nothing to see here [[EMAIL_ffffffffffff]]";
        let (out, hits) = map.replace_in(text);
        assert_eq!(out, text);
        assert_eq!(hits, 0);

        // Empty map is a no-op too.
        let empty = PlaceholderMap::default();
        let (out, hits) = empty.replace_in(text);
        assert_eq!(out, text);
        assert_eq!(hits, 0);
    }

    #[test]
    fn replace_in_does_not_rescan_plaintext() {
        let mut map = PlaceholderMap::default();
        let inner = make("email", "alice@example.com", "s");
        // Plaintext that itself contains another placeholder.
        map.insert(inner.clone(), inner.clone());
        let outer = make("phone", "+1 555 0100", "s");
        map.insert(outer.clone(), "555".to_string());
        let (out, hits) = map.replace_in(&outer);
        assert_eq!(out, "555");
        assert_eq!(hits, 1);
    }
}
