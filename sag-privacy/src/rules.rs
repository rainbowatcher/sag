//! Compiled detection rules: built-in regexes plus their post-filters.
//!
//! A detector is a plain [`regex::Regex`] plus two hand-rolled guards that
//! Rust's regex engine cannot express:
//!
//! - [`Adjacency`] — the characters *touching* the match are checked by
//!   [`boundary_ok`]. The engine has no lookaround, so "not surrounded by
//!   digits" is enforced here rather than in the pattern. Without it
//!   `1[3-9]\d{9}` would happily match the middle of a longer number.
//! - [`Checksum`] — structural validation of the matched text
//!   ([`Checksum::Luhn`], [`Checksum::IdCardCn`], [`Checksum::Iban`]).
//!
//! Both guards only *reject* candidates; the span resolution (overlap,
//! longest-first) lives in [`crate::engine`].

use regex::Regex;

/// Characters that must not touch a match on either side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adjacency {
    /// No ASCII digit may be adjacent (`\d` runs are whole numbers).
    Digits,
    /// No word-ish character may be adjacent: `[A-Za-z0-9_.+-]`.
    Wordish,
    /// No adjacency requirement (multi-line blocks, user regexes).
    None,
}

impl Adjacency {
    /// Whether `c` is forbidden immediately before or after a match.
    pub fn forbids(self, c: char) -> bool {
        match self {
            Adjacency::Digits => c.is_ascii_digit(),
            Adjacency::Wordish => c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-'),
            Adjacency::None => false,
        }
    }
}

/// Structural validation applied to an already-matched candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Checksum {
    /// Bank card numbers: 13–19 digits passing the Luhn check.
    Luhn,
    /// Mainland-China resident ID: 18 chars with the GB 11643 weighted
    /// check digit, or the 15-char legacy form (boundary check only).
    IdCardCn,
    /// IBAN: ISO 13616 mod-97 check (`mod 97 == 1`).
    Iban,
}

impl Checksum {
    /// Whether `matched` passes this check. Non-digit input fails every
    /// check rather than panicking.
    pub fn verify(self, matched: &str) -> bool {
        match self {
            Checksum::Luhn => luhn_ok(matched),
            Checksum::IdCardCn => id_card_cn_ok(matched),
            Checksum::Iban => iban_ok(matched),
        }
    }
}

/// A single detector: pattern + adjacency guard + optional checksum.
#[derive(Debug, Clone)]
pub struct CompiledRule {
    /// Kind reported for a match (also names the placeholder).
    pub kind: String,
    /// The pattern itself.
    pub re: Regex,
    /// Neighbouring characters that invalidate a match.
    pub adj: Adjacency,
    /// Optional structural validation of the matched text.
    pub check: Option<Checksum>,
}

impl CompiledRule {
    /// Build a rule, mapping a bad pattern to a human-readable message.
    pub fn new(
        kind: &str,
        pattern: &str,
        adj: Adjacency,
        check: Option<Checksum>,
    ) -> Result<Self, String> {
        let re = Regex::new(pattern).map_err(|e| {
            format!("privacy builtin '{kind}': invalid regex pattern '{pattern}': {e}")
        })?;
        Ok(Self {
            kind: kind.to_string(),
            re,
            adj,
            check,
        })
    }

    /// Whether the candidate `[start, end)` is accepted by this rule's
    /// adjacency and checksum guards.
    pub fn accepts(&self, text: &str, start: usize, end: usize) -> bool {
        if !boundary_ok(text, start, end, self.adj) {
            return false;
        }
        match self.check {
            Some(check) => check.verify(&text[start..end]),
            None => true,
        }
    }
}

/// Check that neither character touching `[start, end)` is forbidden by `adj`.
///
/// `start` and `end` must be char boundaries (they come from regex matches).
/// A match at the very beginning/end of `text` has no neighbour on that side
/// and therefore passes.
pub fn boundary_ok(text: &str, start: usize, end: usize, adj: Adjacency) -> bool {
    if adj == Adjacency::None {
        return true;
    }
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();
    !before.is_some_and(|c| adj.forbids(c)) && !after.is_some_and(|c| adj.forbids(c))
}

/// Email addresses (conventional, not RFC-complete).
pub const EMAIL_PATTERN: &str = r"[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}";
/// Mainland-China mobile numbers.
pub const PHONE_PATTERN: &str = r"1[3-9][0-9]{9}";
/// 18-char ID numbers (last char may be `X`) or the 15-char legacy form.
pub const ID_CARD_CN_PATTERN: &str = r"(?:[0-9]{17}[0-9Xx]|[0-9]{15})";
/// 13–19 digit card numbers (Luhn-checked afterwards).
pub const BANK_CARD_PATTERN: &str = r"[0-9]{13,19}";
/// IBAN: 2 letters, 2 check digits, 10–30 alphanumerics.
pub const IBAN_PATTERN: &str = r"[A-Z]{2}[0-9]{2}[A-Z0-9]{10,30}";
/// Known API-key prefixes followed by at least 16 key characters.
pub const API_KEY_PATTERN: &str =
    r"(?:sk-ant-|sk-proj-|sk-|ghp_|gho_|gsk_|xox[baprs]-|AKIA|AIza)[A-Za-z0-9_\-]{16,}";
/// PEM private-key blocks (PKCS#1/PKCS#8/EC/OpenSSH), body included.
pub const PRIVATE_KEY_PATTERN: &str =
    r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----";

/// Luhn check for 13–19 digit card numbers.
fn luhn_ok(matched: &str) -> bool {
    let bytes = matched.as_bytes();
    if bytes.len() < 13 || bytes.len() > 19 || !bytes.iter().all(u8::is_ascii_digit) {
        return false;
    }
    let mut sum = 0u32;
    let mut double = false;
    for byte in bytes.iter().rev() {
        let mut digit = u32::from(byte - b'0');
        if double {
            digit *= 2;
            if digit > 9 {
                digit -= 9;
            }
        }
        sum += digit;
        double = !double;
    }
    sum.is_multiple_of(10)
}

/// GB 11643 weights for the first 17 digits of an 18-char ID number.
const ID_CARD_CN_WEIGHTS: [u32; 17] = [7, 9, 10, 5, 8, 4, 2, 1, 6, 3, 7, 9, 10, 5, 8, 4, 2];
/// Check character indexed by `weighted_sum % 11`.
const ID_CARD_CN_CHECK_CHARS: &[u8; 11] = b"10X98765432";

/// GB 11643 check: 18-char form verified, 15-char legacy form accepted as-is.
fn id_card_cn_ok(matched: &str) -> bool {
    let bytes = matched.as_bytes();
    match bytes.len() {
        15 => bytes.iter().all(u8::is_ascii_digit),
        18 => {
            if !bytes[..17].iter().all(u8::is_ascii_digit) {
                return false;
            }
            let sum: u32 = bytes[..17]
                .iter()
                .zip(ID_CARD_CN_WEIGHTS)
                .map(|(byte, weight)| u32::from(byte - b'0') * weight)
                .sum();
            let expected = ID_CARD_CN_CHECK_CHARS[(sum % 11) as usize];
            bytes[17].to_ascii_uppercase() == expected
        }
        _ => false,
    }
}

/// ISO 13616 mod-97 check: letters expand to 10–35, `mod 97` must be 1.
fn iban_ok(matched: &str) -> bool {
    let bytes = matched.as_bytes();
    if bytes.len() < 4 {
        return false;
    }
    // The first four characters (country + check digits) move to the end.
    let rearranged = bytes[4..].iter().chain(bytes[..4].iter());
    let mut rem = 0u32;
    for byte in rearranged {
        let value = match byte {
            b'0'..=b'9' => u32::from(byte - b'0'),
            b'A'..=b'Z' => u32::from(byte - b'A') + 10,
            b'a'..=b'z' => u32::from(byte - b'a') + 10,
            _ => return false,
        };
        rem = if value >= 10 {
            (rem * 100 + value) % 97
        } else {
            (rem * 10 + value) % 97
        };
    }
    rem == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luhn_accepts_known_card_numbers_only() {
        assert!(luhn_ok("4111111111111111"));
        assert!(luhn_ok("4111111111111111110"));
        // 16 digits failing the Luhn check.
        assert!(!luhn_ok("4111111111111112"));
        // Out of the 13–19 length window.
        assert!(!luhn_ok("411111111111"));
        assert!(!luhn_ok("41111111111111111111"));
        // Non-digits never pass.
        assert!(!luhn_ok("4111-1111-1111-1111"));
    }

    #[test]
    fn id_card_cn_verifies_check_digit() {
        assert!(id_card_cn_ok("110101199003074135"));
        // `X` check digit is case-insensitive.
        assert!(id_card_cn_ok("11010119900307416X"));
        assert!(id_card_cn_ok("11010119900307416x"));
        // Wrong check digit.
        assert!(!id_card_cn_ok("110101199003074136"));
        // 15-char legacy form: boundary check only.
        assert!(id_card_cn_ok("110101900307413"));
        assert!(!id_card_cn_ok("11010190030741"));
    }

    #[test]
    fn iban_checks_mod_97() {
        assert!(iban_ok("GB82WEST12345698765432"));
        assert!(!iban_ok("GB82WEST12345698765433"));
        assert!(!iban_ok("GB8"));
    }

    #[test]
    fn boundary_ok_rejects_touching_characters() {
        let text = "a13812345678b";
        // Digits adjacency: letters around the number are fine.
        assert!(boundary_ok(text, 1, 12, Adjacency::Digits));
        // A digit on either side is not.
        assert!(!boundary_ok("913812345678", 1, 12, Adjacency::Digits));
        assert!(!boundary_ok("138123456789", 0, 11, Adjacency::Digits));
        // Wordish additionally rejects letters, `_`, `.`, `+`, `-`.
        assert!(!boundary_ok("a13812345678b", 1, 12, Adjacency::Wordish));
        assert!(boundary_ok(" 13812345678 ", 1, 12, Adjacency::Wordish));
        // Adjacency::None never rejects.
        assert!(boundary_ok("9138123456789", 1, 12, Adjacency::None));
    }

    #[test]
    fn builtin_patterns_match_expected_shapes() {
        let cases: [(&str, &str, &str); 5] = [
            ("email", EMAIL_PATTERN, "write to alice@example.com"),
            ("phone", PHONE_PATTERN, "call 13812345678"),
            ("id_card_cn", ID_CARD_CN_PATTERN, "id 110101199003074135"),
            ("bank_card", BANK_CARD_PATTERN, "card 4111111111111111"),
            ("iban", IBAN_PATTERN, "iban GB82WEST12345698765432"),
        ];
        for (kind, pattern, text) in cases {
            let rule = CompiledRule::new(kind, pattern, Adjacency::None, None).unwrap();
            let found: Vec<&str> = rule.re.find_iter(text).map(|m| m.as_str()).collect();
            assert_eq!(found.len(), 1, "{kind} should match once in {text}");
        }
    }

    #[test]
    fn accepts_applies_adjacency_then_checksum() {
        let rule = CompiledRule::new(
            "bank_card",
            BANK_CARD_PATTERN,
            Adjacency::Digits,
            Some(Checksum::Luhn),
        )
        .unwrap();
        assert!(rule.accepts("card 4111111111111111 ok", 5, 21));
        // Luhn failure.
        assert!(!rule.accepts("card 4111111111111112 ok", 5, 21));
        // Adjacency failure (embedded in a longer digit run).
        assert!(!rule.accepts("94111111111111111", 1, 17));
    }

    #[test]
    fn new_reports_bad_patterns() {
        let err = CompiledRule::new("broken", "(", Adjacency::None, None).unwrap_err();
        assert!(err.contains("privacy builtin 'broken'"), "{err}");
        assert!(err.contains("invalid regex pattern"), "{err}");
    }
}
