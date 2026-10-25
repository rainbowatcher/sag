//! The privacy rule engine: detect sensitive values in a chat request and
//! apply the configured [`PrivacyMode`].
//!
//! [`PrivacyEngine::compile`] turns a [`PrivacyConfig`] into a ready-to-use
//! engine (built-in regexes, user regexes, an Aho-Corasick automaton for the
//! literal word lists). [`PrivacyEngine::mask_request`] then walks a
//! [`ChatCompletionRequest`] — and [`PrivacyEngine::mask_ir`] the protocol
//! neutral [`IrRequest`] — and either rewrites every detected value into a
//! deterministic placeholder, only reports the hits, or reports them so the
//! caller can reject the request.
//!
//! # Coverage
//!
//! Both entry points scan **every** string that can reach a provider: message
//! content (a plain string or any text leaf inside a content part),
//! `messages[].name`, every string leaf of `messages[].extra` (tool-call
//! arguments, `reasoning_content`, …), and the whole request surface
//! (`stop`, `metadata`, `tools`, `tool_choice`, `response_format`, the
//! catch-all `extra` / `raw` maps, …). Only two things are skipped, both
//! listed in `UNSCANNED_KEYS`: protocol structure (a masked `model` or
//! `role` would break routing and the request shape) and opaque media
//! payloads (base64 blobs and URLs, where masking would corrupt the media
//! without hiding anything readable).
//!
//! Object *keys* are never masked either — only string values — so a JSON
//! schema keeps its property names.
//!
//! The engine is immutable after compilation: it holds no state of its own,
//! so one instance can serve every concurrent request. Placeholders and their
//! reverse map are per-request (`PlaceholderMap`), which keeps a conversation
//! coherent without leaking state across requests.
//!
//! # Span resolution
//!
//! Every rule reports all of its candidates (rules overlap freely: an ID
//! number contains phone-shaped digits). [`PrivacyEngine::scan`] then sorts by
//! `(start asc, end desc)` and greedily takes non-overlapping spans, so the
//! leftmost *and longest* candidate wins and no text is masked twice.

use aho_corasick::AhoCorasick;
use regex::RegexBuilder;
use sag_config::{PrivacyConfig, PrivacyMode};
use sag_core::{ChatCompletionRequest, IrContentBlock, IrRequest, StopSequence};
use serde_json::{Map, Value};

use crate::placeholder::{self, PlaceholderMap};
use crate::rules::{
    API_KEY_PATTERN, Adjacency, BANK_CARD_PATTERN, Checksum, CompiledRule, EMAIL_PATTERN,
    IBAN_PATTERN, ID_CARD_CN_PATTERN, PHONE_PATTERN, PRIVATE_KEY_PATTERN,
};

/// Kind reported for a match of the `custom_words` list.
const CUSTOM_WORD_KIND: &str = "custom_word";

/// Object keys whose values are **not** scanned.
///
/// - Protocol structure: masking `model` would break route identity and the
///   provider call, `role` / `type` are discriminators, and the id keys pair a
///   tool call with its result — none of them carry user prose.
/// - Opaque payloads: base64 blobs and URLs. Nothing there is readable text,
///   and masking it corrupts the media: a long base64 string is statistically
///   bound to contain IBAN-shaped runs (`[A-Z]{2}[0-9]{2}[A-Z0-9]{10,30}`
///   passes mod-97 about once per 25 KB), and a URL can legitimately contain a
///   phone-shaped path segment.
const UNSCANNED_KEYS: &[&str] = &[
    "model",
    "role",
    "type",
    "id",
    "tool_call_id",
    "tool_use_id",
    "call_id",
    "data",
    "file_data",
    "url",
];

/// A compiled privacy filter, ready to scan requests.
#[derive(Debug)]
pub struct PrivacyEngine {
    /// Regex detectors (built-in + user-supplied), in config order.
    rules: Vec<CompiledRule>,
    /// Automaton over `custom_words`, longest patterns first.
    words: AhoCorasick,
    /// Kind reported for `words` matches.
    word_kind: String,
    /// Values that must never be masked (exact match).
    allow: Vec<String>,
    /// Salt mixed into placeholder digests.
    salt: String,
    /// How a detected value is handled.
    mode: PrivacyMode,
    /// Whether placeholders are restored in the upstream response.
    restore: bool,
}

/// What [`PrivacyEngine::mask_request`] did with a request.
pub enum Outcome {
    /// `mask` mode: the request was rewritten in place.
    Masked(MaskReport),
    /// `warn` mode: sensitive values were found but the request is untouched.
    Detected(MaskReport),
    /// `block` mode: sensitive values were found; the caller must reject.
    Blocked(MaskReport),
    /// Nothing matched.
    Clean,
}

/// The hits of one scan plus the placeholder reverse map.
///
/// `hits` is `(kind, occurrences)` in first-appearance order; `map` is empty
/// unless the request was actually rewritten (a `warn` / `block` request has
/// no placeholders to restore).
#[derive(Default)]
pub struct MaskReport {
    /// Detector kind and how many times it matched.
    pub hits: Vec<(String, usize)>,
    /// Placeholder → plaintext, for [`crate::restore::RestoreWriter`].
    pub map: PlaceholderMap,
}

impl PrivacyEngine {
    /// Compile `cfg` into an engine.
    ///
    /// Returns `Err` for an invalid user regex; built-in patterns are
    /// constants and cannot fail in practice. `cfg.enabled` is **not**
    /// consulted — the caller decides whether to build an engine at all.
    pub fn compile(cfg: &PrivacyConfig) -> Result<Self, String> {
        let mut rules = Vec::new();
        let builtins = &cfg.builtins;
        if builtins.email {
            rules.push(CompiledRule::new(
                "email",
                EMAIL_PATTERN,
                Adjacency::Wordish,
                None,
            )?);
        }
        if builtins.phone {
            rules.push(CompiledRule::new(
                "phone",
                PHONE_PATTERN,
                Adjacency::Digits,
                None,
            )?);
        }
        if builtins.id_card_cn {
            rules.push(CompiledRule::new(
                "id_card_cn",
                ID_CARD_CN_PATTERN,
                Adjacency::Digits,
                Some(Checksum::IdCardCn),
            )?);
        }
        if builtins.bank_card {
            rules.push(CompiledRule::new(
                "bank_card",
                BANK_CARD_PATTERN,
                Adjacency::Digits,
                Some(Checksum::Luhn),
            )?);
        }
        if builtins.iban {
            rules.push(CompiledRule::new(
                "iban",
                IBAN_PATTERN,
                Adjacency::Digits,
                Some(Checksum::Iban),
            )?);
        }
        if builtins.api_key {
            rules.push(CompiledRule::new(
                "api_key",
                API_KEY_PATTERN,
                Adjacency::Wordish,
                None,
            )?);
        }
        if builtins.private_key {
            rules.push(CompiledRule::new(
                "private_key",
                PRIVATE_KEY_PATTERN,
                Adjacency::None,
                None,
            )?);
        }

        for (i, custom) in cfg.custom_regexes.iter().enumerate() {
            let re = RegexBuilder::new(&custom.pattern)
                .case_insensitive(custom.case_insensitive)
                .build()
                .map_err(|e| {
                    format!(
                        "privacy.custom_regexes[{i}]: invalid regex pattern '{}': {e}",
                        custom.pattern
                    )
                })?;
            rules.push(CompiledRule {
                kind: custom.kind.clone(),
                re,
                adj: Adjacency::None,
                check: None,
            });
        }

        // Longest word first so the automaton's own preference (and the
        // greedy span pass) agrees with "longest match wins". Empty strings
        // are dropped: an empty pattern matches at every offset.
        let mut words: Vec<&str> = cfg
            .custom_words
            .items
            .iter()
            .map(String::as_str)
            .filter(|word| !word.is_empty())
            .collect();
        words.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        words.dedup();
        let words = AhoCorasick::new(words)
            .map_err(|e| format!("privacy.custom_words: failed to build automaton: {e}"))?;

        Ok(Self {
            rules,
            words,
            word_kind: CUSTOM_WORD_KIND.to_string(),
            allow: cfg
                .allowlist
                .items
                .iter()
                .filter(|word| !word.is_empty())
                .cloned()
                .collect(),
            salt: cfg.salt.clone(),
            mode: cfg.mode,
            restore: cfg.restore,
        })
    }

    /// How detected values are handled (`mask` / `warn` / `block`).
    pub fn mode(&self) -> PrivacyMode {
        self.mode
    }

    /// Whether masked placeholders are restored in the upstream response.
    pub fn restore(&self) -> bool {
        self.restore
    }

    /// All accepted, non-overlapping matches in `text`, ordered by position.
    ///
    /// Each entry is `(start, end, kind, matched)`. Candidates are collected
    /// from every rule (and from `custom_words`), filtered by the allowlist,
    /// then resolved greedily: leftmost first, longest at the same start, and
    /// — for identical spans — the detector configured first.
    pub fn scan(&self, text: &str) -> Vec<(usize, usize, String, String)> {
        let mut found: Vec<(usize, usize, String, String)> = Vec::new();

        for rule in &self.rules {
            for m in rule.re.find_iter(text) {
                if m.start() == m.end() || !rule.accepts(text, m.start(), m.end()) {
                    continue;
                }
                found.push((
                    m.start(),
                    m.end(),
                    rule.kind.clone(),
                    m.as_str().to_string(),
                ));
            }
        }

        // Overlapping iteration: the greedy pass below decides which of two
        // overlapping words survives, so none may be dropped up front.
        for m in self.words.find_overlapping_iter(text) {
            found.push((
                m.start(),
                m.end(),
                self.word_kind.clone(),
                text[m.start()..m.end()].to_string(),
            ));
        }

        found.retain(|(_, _, _, matched)| {
            !self
                .allow
                .iter()
                .any(|allowed| allowed.as_str() == matched.as_str())
        });

        // Stable sort: identical spans keep collection order, so the more
        // specific detector (an ID number is pushed before the bank-card rule)
        // wins over a generic one that happens to match the same text.
        found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));

        let mut out = Vec::with_capacity(found.len());
        let mut cursor = 0;
        for span in found {
            if span.0 < cursor {
                continue;
            }
            cursor = span.1;
            out.push(span);
        }
        out
    }

    /// Mask every detected value in `text`, recording placeholders in `map`
    /// and per-kind counts in `hits`.
    ///
    /// The same `(kind, text, salt)` always yields the same placeholder, so
    /// re-masking a repeated value re-inserts an identical map entry and the
    /// client sees one stable marker for one entity.
    pub fn mask_text(
        &self,
        text: &str,
        map: &mut PlaceholderMap,
        hits: &mut Vec<(String, usize)>,
    ) -> String {
        let spans = self.scan(text);
        if spans.is_empty() {
            return text.to_string();
        }

        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for (start, end, kind, matched) in spans {
            out.push_str(&text[cursor..start]);
            let placeholder = placeholder::make(&kind, &matched, &self.salt);
            map.insert(placeholder.clone(), matched);
            out.push_str(&placeholder);
            cursor = end;
            bump(hits, kind);
        }
        out.push_str(&text[cursor..]);
        out
    }

    /// Apply the configured mode to every scannable field of `req`.
    ///
    /// Coverage is everything that reaches the provider: message content (a
    /// string, or any text leaf of a content part), `messages[].name`, every
    /// string leaf of `messages[].extra` (`tool_calls[].function.arguments`,
    /// `reasoning_content`, …), the top-level text fields (`stop`,
    /// `metadata`, `tools`, `tool_choice`, `response_format`, …) and the
    /// catch-all `extra` map. See `UNSCANNED_KEYS` for the two exclusions
    /// (protocol structure, opaque media payloads).
    pub fn mask_request(&self, req: &mut ChatCompletionRequest) -> Outcome {
        let rewrite = matches!(self.mode, PrivacyMode::Mask);
        let mut map = PlaceholderMap::default();
        let mut hits: Vec<(String, usize)> = Vec::new();

        for msg in req.messages.iter_mut() {
            self.scrub_json(&mut msg.content, rewrite, &mut map, &mut hits);
            self.scrub_opt_string(&mut msg.name, rewrite, &mut map, &mut hits);
            self.scrub_map(&mut msg.extra, rewrite, &mut map, &mut hits);
        }

        // Top-level fields a client can move PII into (and that providers
        // forward verbatim).
        self.scrub_opt_string(&mut req.reasoning_effort, rewrite, &mut map, &mut hits);
        self.scrub_opt_string(&mut req.prompt_cache_key, rewrite, &mut map, &mut hits);
        self.scrub_opt_string(
            &mut req.prompt_cache_retention,
            rewrite,
            &mut map,
            &mut hits,
        );
        self.scrub_opt_string(&mut req.safety_identifier, rewrite, &mut map, &mut hits);
        self.scrub_opt_string(&mut req.service_tier, rewrite, &mut map, &mut hits);
        self.scrub_opt_string(&mut req.verbosity, rewrite, &mut map, &mut hits);
        match req.stop.as_mut() {
            Some(StopSequence::Single(text)) => {
                self.scrub_string(text, rewrite, &mut map, &mut hits);
            }
            Some(StopSequence::Multiple(items)) => {
                for text in items.iter_mut() {
                    self.scrub_string(text, rewrite, &mut map, &mut hits);
                }
            }
            None => {}
        }
        self.scrub_opt_json(&mut req.logit_bias, rewrite, &mut map, &mut hits);
        self.scrub_opt_json(&mut req.modalities, rewrite, &mut map, &mut hits);
        self.scrub_opt_json(&mut req.audio, rewrite, &mut map, &mut hits);
        self.scrub_opt_json(&mut req.stream_options, rewrite, &mut map, &mut hits);
        self.scrub_opt_json(&mut req.metadata, rewrite, &mut map, &mut hits);
        self.scrub_opt_json(&mut req.prediction, rewrite, &mut map, &mut hits);
        self.scrub_opt_json(&mut req.tools, rewrite, &mut map, &mut hits);
        self.scrub_opt_json(&mut req.tool_choice, rewrite, &mut map, &mut hits);
        self.scrub_opt_json(&mut req.response_format, rewrite, &mut map, &mut hits);
        self.scrub_opt_json(&mut req.moderation, rewrite, &mut map, &mut hits);
        self.scrub_opt_json(&mut req.web_search_options, rewrite, &mut map, &mut hits);
        self.scrub_map(&mut req.extra, rewrite, &mut map, &mut hits);

        self.outcome(hits, map)
    }

    /// Apply the configured mode to every scannable field of the
    /// protocol-neutral `ir`, **in place**.
    ///
    /// This is the translated-lane counterpart of [`Self::mask_request`]: the
    /// IR is the encode source for Anthropic / Responses targets, so it has to
    /// carry the same placeholders. Masking happens in place — no
    /// encode/decode round-trip — so every block keeps its variant
    /// (`Reasoning` is not downgraded to `Text`, `ToolResult` is not split into
    /// `role: "tool"` messages, `Raw` blocks survive) and `ir.loss` is left
    /// untouched. `model` and the tool-call ids are identity, not text, and are
    /// never rewritten.
    pub fn mask_ir(&self, ir: &mut IrRequest) -> Outcome {
        let rewrite = matches!(self.mode, PrivacyMode::Mask);
        let mut map = PlaceholderMap::default();
        let mut hits: Vec<(String, usize)> = Vec::new();

        for msg in ir.messages.iter_mut() {
            self.scrub_blocks(&mut msg.content, rewrite, &mut map, &mut hits);
            self.scrub_opt_string(&mut msg.name, rewrite, &mut map, &mut hits);
            for call in msg.tool_calls.iter_mut() {
                self.scrub_string(&mut call.name, rewrite, &mut map, &mut hits);
                self.scrub_json(&mut call.arguments, rewrite, &mut map, &mut hits);
            }
            self.scrub_map(&mut msg.raw, rewrite, &mut map, &mut hits);
        }

        for tool in ir.tools.iter_mut() {
            self.scrub_string(&mut tool.name, rewrite, &mut map, &mut hits);
            if let Some(description) = tool.description.as_mut() {
                self.scrub_string(description, rewrite, &mut map, &mut hits);
            }
            self.scrub_json(&mut tool.input_schema, rewrite, &mut map, &mut hits);
            self.scrub_map(&mut tool.raw, rewrite, &mut map, &mut hits);
        }

        for text in ir.stop.iter_mut() {
            self.scrub_string(text, rewrite, &mut map, &mut hits);
        }
        self.scrub_json(&mut ir.tool_choice, rewrite, &mut map, &mut hits);
        self.scrub_json(&mut ir.response_format, rewrite, &mut map, &mut hits);
        self.scrub_json(&mut ir.metadata, rewrite, &mut map, &mut hits);
        if let Some(thinking) = ir.reasoning.thinking.as_mut() {
            self.scrub_json(thinking, rewrite, &mut map, &mut hits);
        }
        if let Some(logit_bias) = ir.sampling.logit_bias.as_mut() {
            self.scrub_json(logit_bias, rewrite, &mut map, &mut hits);
        }
        self.scrub_map(&mut ir.raw, rewrite, &mut map, &mut hits);

        self.outcome(hits, map)
    }

    /// Turn the accumulated hits and placeholder map into the mode's
    /// [`Outcome`].
    fn outcome(&self, hits: Vec<(String, usize)>, map: PlaceholderMap) -> Outcome {
        if hits.is_empty() {
            return Outcome::Clean;
        }
        let report = MaskReport { hits, map };
        match self.mode {
            PrivacyMode::Mask => Outcome::Masked(report),
            PrivacyMode::Warn => Outcome::Detected(report),
            PrivacyMode::Block => Outcome::Blocked(report),
        }
    }

    /// Scrub every content block of one IR message, keeping its shape.
    fn scrub_blocks(
        &self,
        blocks: &mut [IrContentBlock],
        rewrite: bool,
        map: &mut PlaceholderMap,
        hits: &mut Vec<(String, usize)>,
    ) {
        for block in blocks.iter_mut() {
            match block {
                IrContentBlock::Text { text } | IrContentBlock::Reasoning { text } => {
                    self.scrub_string(text, rewrite, map, hits);
                }
                IrContentBlock::ToolUse {
                    name, arguments, ..
                } => {
                    self.scrub_string(name, rewrite, map, hits);
                    self.scrub_json(arguments, rewrite, map, hits);
                }
                IrContentBlock::ToolResult { content, .. } => {
                    self.scrub_blocks(content, rewrite, map, hits);
                }
                // Unknown blocks are carried verbatim by design; their payload
                // is the one place an unknown protocol could hide text.
                IrContentBlock::Raw(value) => self.scrub_json(value, rewrite, map, hits),
                // Opaque media payloads — see [`UNSCANNED_KEYS`].
                IrContentBlock::Image { .. }
                | IrContentBlock::Document { .. }
                | IrContentBlock::Audio { .. } => {}
            }
        }
    }

    /// Scrub every string leaf of a JSON value.
    ///
    /// Objects are traversed by value only (keys stay untouched so a schema
    /// keeps its property names), and the keys in [`UNSCANNED_KEYS`] are
    /// skipped entirely.
    fn scrub_json(
        &self,
        value: &mut Value,
        rewrite: bool,
        map: &mut PlaceholderMap,
        hits: &mut Vec<(String, usize)>,
    ) {
        match value {
            Value::String(text) => self.scrub_string(text, rewrite, map, hits),
            Value::Array(items) => {
                for item in items.iter_mut() {
                    self.scrub_json(item, rewrite, map, hits);
                }
            }
            Value::Object(object) => self.scrub_map(object, rewrite, map, hits),
            _ => {}
        }
    }

    /// [`Self::scrub_json`] for a JSON object.
    fn scrub_map(
        &self,
        object: &mut Map<String, Value>,
        rewrite: bool,
        map: &mut PlaceholderMap,
        hits: &mut Vec<(String, usize)>,
    ) {
        for (key, value) in object.iter_mut() {
            if UNSCANNED_KEYS.contains(&key.as_str()) {
                continue;
            }
            self.scrub_json(value, rewrite, map, hits);
        }
    }

    /// [`Self::scrub_string`] for an optional string slot.
    fn scrub_opt_string(
        &self,
        slot: &mut Option<String>,
        rewrite: bool,
        map: &mut PlaceholderMap,
        hits: &mut Vec<(String, usize)>,
    ) {
        if let Some(text) = slot.as_mut() {
            self.scrub_string(text, rewrite, map, hits);
        }
    }

    /// [`Self::scrub_json`] for an optional value slot.
    fn scrub_opt_json(
        &self,
        slot: &mut Option<Value>,
        rewrite: bool,
        map: &mut PlaceholderMap,
        hits: &mut Vec<(String, usize)>,
    ) {
        if let Some(value) = slot.as_mut() {
            self.scrub_json(value, rewrite, map, hits);
        }
    }

    /// Mask (or only scan) one string in place.
    fn scrub_string(
        &self,
        text: &mut String,
        rewrite: bool,
        map: &mut PlaceholderMap,
        hits: &mut Vec<(String, usize)>,
    ) {
        if rewrite {
            *text = self.mask_text(text.as_str(), map, hits);
        } else {
            for (_, _, kind, _) in self.scan(text.as_str()) {
                bump(hits, kind);
            }
        }
    }
}

/// Record one occurrence of `kind` in `hits`.
fn bump(hits: &mut Vec<(String, usize)>, kind: String) {
    match hits.iter().position(|(seen, _)| *seen == kind) {
        Some(idx) => hits[idx].1 += 1,
        None => hits.push((kind, 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placeholder::make;
    use sag_config::{PrivacyBuiltins, PrivacyCustomRegex};
    use sag_core::{IrMessage, IrRole, IrTool, IrToolCall};
    use serde_json::json;

    /// A config with every built-in disabled.
    fn bare() -> PrivacyConfig {
        PrivacyConfig {
            enabled: true,
            builtins: PrivacyBuiltins {
                email: false,
                phone: false,
                id_card_cn: false,
                bank_card: false,
                iban: false,
                api_key: false,
                private_key: false,
            },
            ..PrivacyConfig::default()
        }
    }

    fn engine_with(mutate: impl FnOnce(&mut PrivacyConfig)) -> PrivacyEngine {
        let mut cfg = PrivacyConfig {
            enabled: true,
            ..PrivacyConfig::default()
        };
        mutate(&mut cfg);
        PrivacyEngine::compile(&cfg).expect("engine compiles")
    }

    /// An engine with exactly one built-in detector enabled.
    fn only(kind: &str) -> PrivacyEngine {
        let mut cfg = bare();
        match kind {
            "email" => cfg.builtins.email = true,
            "phone" => cfg.builtins.phone = true,
            "id_card_cn" => cfg.builtins.id_card_cn = true,
            "bank_card" => cfg.builtins.bank_card = true,
            "iban" => cfg.builtins.iban = true,
            "api_key" => cfg.builtins.api_key = true,
            "private_key" => cfg.builtins.private_key = true,
            other => panic!("unknown builtin {other}"),
        }
        PrivacyEngine::compile(&cfg).expect("engine compiles")
    }

    fn kinds(spans: &[(usize, usize, String, String)]) -> Vec<&str> {
        spans.iter().map(|(_, _, kind, _)| kind.as_str()).collect()
    }

    fn request_with_content(content: Value) -> ChatCompletionRequest {
        serde_json::from_value(json!({
            "model": "m",
            "messages": [{"role": "user", "content": content}],
        }))
        .expect("request parses")
    }

    #[test]
    fn email_matches_and_respects_wordish_adjacency() {
        let engine = only("email");
        let spans = engine.scan("write to alice@example.com now");
        assert_eq!(kinds(&spans), vec!["email"]);
        assert_eq!(spans[0].3, "alice@example.com");
        // A word-ish character right after the TLD invalidates the match.
        assert!(engine.scan("alice@example.com_x").is_empty());
    }

    #[test]
    fn phone_requires_digit_boundaries() {
        let engine = only("phone");
        assert_eq!(kinds(&engine.scan("call 13812345678")), vec!["phone"]);
        // Embedded in a longer digit run: not a phone number.
        assert!(engine.scan("1381234567890").is_empty());
        assert!(engine.scan("9913812345678").is_empty());
    }

    #[test]
    fn id_card_cn_requires_a_valid_check_digit() {
        let engine = only("id_card_cn");
        assert_eq!(
            kinds(&engine.scan("id 110101199003074135")),
            vec!["id_card_cn"]
        );
        // `X` check digit is accepted in either case.
        assert_eq!(
            kinds(&engine.scan("id 11010119900307416X")),
            vec!["id_card_cn"]
        );
        // Wrong check digit.
        assert!(engine.scan("id 110101199003074136").is_empty());
        // 15-char legacy form: boundary check only.
        assert_eq!(
            kinds(&engine.scan("id 110101900307413")),
            vec!["id_card_cn"]
        );
        // 19 digits is not an ID number.
        assert!(engine.scan("id 1101011990030741355").is_empty());
    }

    #[test]
    fn bank_card_requires_luhn() {
        let engine = only("bank_card");
        assert_eq!(
            kinds(&engine.scan("card 4111111111111111")),
            vec!["bank_card"]
        );
        // 16 digits, Luhn fails.
        assert!(engine.scan("card 4111111111111112").is_empty());
    }

    #[test]
    fn iban_requires_mod_97() {
        let engine = only("iban");
        assert_eq!(
            kinds(&engine.scan("pay to GB82WEST12345698765432")),
            vec!["iban"]
        );
        assert!(engine.scan("pay to GB82WEST12345698765433").is_empty());
    }

    #[test]
    fn api_key_needs_a_known_prefix_and_long_enough_body() {
        let engine = only("api_key");
        assert_eq!(
            kinds(&engine.scan("token sk-abcdefghijklmnopqrstuvwx")),
            vec!["api_key"]
        );
        assert_eq!(
            kinds(&engine.scan("token ghp_0123456789abcdef0123456789abcdef")),
            vec!["api_key"]
        );
        // Too short after the prefix.
        assert!(engine.scan("token sk-short").is_empty());
        // Glued to a preceding word-ish character.
        assert!(engine.scan("xsk-abcdefghijklmnopqrstuvwx").is_empty());
    }

    #[test]
    fn private_key_matches_the_whole_pem_block() {
        let engine = only("private_key");
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----";
        let spans = engine.scan(&format!("key: {pem} done"));
        assert_eq!(kinds(&spans), vec!["private_key"]);
        assert_eq!(spans[0].3, pem);
    }

    #[test]
    fn disabled_builtins_detect_nothing() {
        let engine = engine_with(|cfg| *cfg = bare());
        assert!(
            engine
                .scan("alice@example.com 13812345678 4111111111111111")
                .is_empty()
        );
    }

    #[test]
    fn overlapping_spans_resolve_to_the_longest_match() {
        // The ID number embeds a phone-shaped run; only the ID is reported.
        let engine = engine_with(|_| {});
        let spans = engine.scan("id 110101199003074135 ok");
        assert_eq!(kinds(&spans), vec!["id_card_cn"]);

        // Two custom rules matching at the same start: the longer one wins.
        let engine = engine_with(|cfg| {
            cfg.custom_regexes = vec![
                PrivacyCustomRegex {
                    pattern: r"tok-[0-9]+".into(),
                    kind: "short".into(),
                    case_insensitive: false,
                },
                PrivacyCustomRegex {
                    pattern: r"tok-[0-9]+-[a-z]+".into(),
                    kind: "long".into(),
                    case_insensitive: false,
                },
            ];
        });
        assert_eq!(kinds(&engine.scan("tok-123-abc")), vec!["long"]);

        // Longest custom word wins over its own prefix.
        let engine =
            engine_with(|cfg| cfg.custom_words.items = vec!["beta".into(), "alpha beta".into()]);
        let spans = engine.scan("alpha beta");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].3, "alpha beta");
    }

    #[test]
    fn identical_spans_prefer_the_specific_detector() {
        // This ID number is also a Luhn-valid 18-digit number; the ID rule is
        // configured first, so it wins instead of the generic card rule.
        let engine = engine_with(|_| {});
        let spans = engine.scan("id 110101199003074098");
        assert_eq!(kinds(&spans), vec!["id_card_cn"]);
    }

    #[test]
    fn allowlist_suppresses_exact_matches() {
        let engine = engine_with(|cfg| {
            cfg.allowlist.items = vec!["alice@example.com".into(), "Secret".into()];
            cfg.custom_words.items = vec!["Secret".into(), "Project".into()];
        });
        let spans = engine.scan("alice@example.com Secret Project");
        assert_eq!(kinds(&spans), vec!["custom_word"]);
        assert_eq!(spans[0].3, "Project");
    }

    #[test]
    fn custom_words_are_literal_and_case_sensitive() {
        let engine = engine_with(|cfg| cfg.custom_words.items = vec!["Falcon".into()]);
        assert_eq!(
            kinds(&engine.scan("Falcon falcon FALCON")),
            vec!["custom_word"]
        );

        // Regex metacharacters in a word are literal.
        let engine = engine_with(|cfg| cfg.custom_words.items = vec!["a.c".into()]);
        assert!(engine.scan("abc").is_empty());
        assert_eq!(engine.scan("a.c").len(), 1);
    }

    #[test]
    fn custom_regex_honours_case_insensitive() {
        let build = |case_insensitive: bool| {
            engine_with(|cfg| {
                cfg.custom_regexes = vec![PrivacyCustomRegex {
                    pattern: r"EMP-[0-9]{4}".into(),
                    kind: "employee_id".into(),
                    case_insensitive,
                }]
            })
        };
        assert_eq!(kinds(&build(false).scan("EMP-1234")), vec!["employee_id"]);
        assert!(build(false).scan("emp-1234").is_empty());
        assert_eq!(kinds(&build(true).scan("emp-1234")), vec!["employee_id"]);
    }

    #[test]
    fn compile_rejects_invalid_custom_regex() {
        let cfg = PrivacyConfig {
            enabled: true,
            custom_regexes: vec![PrivacyCustomRegex {
                pattern: "(".into(),
                kind: "broken".into(),
                case_insensitive: false,
            }],
            ..PrivacyConfig::default()
        };
        let err = PrivacyEngine::compile(&cfg).unwrap_err();
        assert!(err.contains("privacy.custom_regexes[0]"), "{err}");
    }

    #[test]
    fn placeholders_are_deterministic_and_reused() {
        let engine = engine_with(|cfg| cfg.salt = "s1".into());
        let mut map = PlaceholderMap::default();
        let mut hits = Vec::new();
        let masked = engine.mask_text("a@x.com and a@x.com", &mut map, &mut hits);
        let placeholder = make("email", "a@x.com", "s1");
        assert_eq!(masked, format!("{placeholder} and {placeholder}"));
        assert_eq!(map.len(), 1);
        assert_eq!(hits, vec![("email".to_string(), 2)]);

        // A fresh map yields the same placeholder for the same value.
        let mut fresh = PlaceholderMap::default();
        let mut fresh_hits = Vec::new();
        assert_eq!(
            engine.mask_text("a@x.com", &mut fresh, &mut fresh_hits),
            placeholder
        );
        assert_eq!(fresh_hits, vec![("email".to_string(), 1)]);
    }

    #[test]
    fn mask_text_without_hits_returns_input() {
        let engine = engine_with(|_| {});
        let mut map = PlaceholderMap::default();
        let mut hits = Vec::new();
        assert_eq!(
            engine.mask_text("nothing here", &mut map, &mut hits),
            "nothing here"
        );
        assert!(map.is_empty());
        assert!(hits.is_empty());
    }

    #[test]
    fn mask_request_covers_string_parts_and_tool_arguments() {
        let mut req: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "mail alice@example.com"},
                {"role": "user", "content": [
                    {"type": "text", "text": "phone 13812345678"},
                    {"type": "image_url", "image_url": {"url": "https://example.com/13812345678.png"}}
                ]},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "c1", "type": "function",
                     "function": {"name": "f", "arguments": "{\"email\":\"bob@example.com\"}"}}
                ]}
            ]
        }))
        .expect("request parses");

        let engine = engine_with(|cfg| cfg.salt = "s".into());
        let Outcome::Masked(report) = engine.mask_request(&mut req) else {
            panic!("mask mode must rewrite the request");
        };

        // Plain string content.
        assert_eq!(
            req.messages[0].content,
            json!(format!("mail {}", make("email", "alice@example.com", "s")))
        );

        // Content parts: only the text part is touched.
        let parts = req.messages[1].content.as_array().expect("parts array");
        assert_eq!(
            parts[0]["text"],
            json!(format!("phone {}", make("phone", "13812345678", "s")))
        );
        assert_eq!(
            parts[1]["image_url"]["url"],
            json!("https://example.com/13812345678.png")
        );

        // tool_calls[].function.arguments is a JSON string and is scrubbed.
        let arguments = req.messages[2].extra["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("arguments string");
        assert!(arguments.contains(&make("email", "bob@example.com", "s")));
        assert!(!arguments.contains("bob@example.com"));

        assert_eq!(
            report.hits,
            vec![("email".to_string(), 2), ("phone".to_string(), 1)]
        );
        assert_eq!(report.map.len(), 3);
    }

    #[test]
    fn mask_request_covers_name_extra_and_top_level_fields() {
        let engine = engine_with(|cfg| cfg.salt = "s".into());
        let ph = |text: &str| make("email", text, "s");
        let mut req: ChatCompletionRequest = serde_json::from_value(json!({
            "model": "gpt-4o",
            "messages": [{
                "role": "user",
                "content": "hello",
                "name": "alice@example.com",
                "reasoning_content": "bob@example.com",
            }],
            "metadata": {"notes": "carol@example.com"},
            "stop": ["dave@example.com"],
            "tools": [{"type": "function", "function": {
                "name": "lookup", "description": "erin@example.com"
            }}],
            "notes": "frank@example.com",
        }))
        .expect("request parses");

        let Outcome::Masked(_) = engine.mask_request(&mut req) else {
            panic!("mask mode must rewrite the request");
        };

        // Route identity is never rewritten.
        assert_eq!(req.model, "gpt-4o");
        // Message name and the flattened message extras.
        assert_eq!(
            req.messages[0].name.as_deref(),
            Some(ph("alice@example.com").as_str())
        );
        assert_eq!(
            req.messages[0].extra["reasoning_content"],
            json!(ph("bob@example.com"))
        );
        // Typed top-level fields and the `extra` catch-all.
        assert_eq!(
            req.metadata.as_ref().expect("metadata")["notes"],
            json!(ph("carol@example.com"))
        );
        match req.stop.as_ref() {
            Some(StopSequence::Multiple(items)) => {
                assert_eq!(items, &vec![ph("dave@example.com")]);
            }
            other => panic!("stop must stay a sequence list: {other:?}"),
        }
        let function = &req.tools.as_ref().expect("tools")[0]["function"];
        assert_eq!(function["description"], json!(ph("erin@example.com")));
        assert_eq!(function["name"], json!("lookup"));
        assert_eq!(req.extra["notes"], json!(ph("frank@example.com")));

        // No plaintext survived anywhere in the forwarded request.
        let sent = serde_json::to_string(&req).expect("request serializes");
        assert!(!sent.contains("@example.com"), "upstream would see: {sent}");
    }

    #[test]
    fn mask_ir_covers_every_field_and_keeps_block_variants() {
        let engine = engine_with(|cfg| cfg.salt = "s".into());
        let ph = |text: &str| make("email", text, "s");

        let mut message_raw = Map::new();
        message_raw.insert("notes".into(), json!("nick@example.com"));
        let mut request_raw = Map::new();
        request_raw.insert("notes".into(), json!("olive@example.com"));

        let mut ir = IrRequest {
            model: "gpt-4o".into(),
            messages: vec![IrMessage {
                role: IrRole::Assistant,
                content: vec![
                    IrContentBlock::Text {
                        text: "mail alice@example.com".into(),
                    },
                    IrContentBlock::Reasoning {
                        text: "think bob@example.com".into(),
                    },
                    IrContentBlock::ToolUse {
                        id: "call_1".into(),
                        name: "lookup".into(),
                        arguments: json!({"email": "carol@example.com"}),
                    },
                    IrContentBlock::ToolResult {
                        id: None,
                        tool_call_id: "call_1".into(),
                        content: vec![IrContentBlock::Text {
                            text: "dave@example.com".into(),
                        }],
                        is_error: false,
                    },
                    IrContentBlock::Image {
                        data: json!({"url": "https://example.com/13812345678.png"}),
                    },
                    IrContentBlock::Raw(json!({"type": "mystery", "body": "erin@example.com"})),
                ],
                name: Some("frank@example.com".into()),
                tool_call_id: Some("call_1".into()),
                tool_calls: vec![IrToolCall {
                    id: "call_1".into(),
                    name: "lookup".into(),
                    arguments: json!({"email": "grace@example.com"}),
                    index: Some(0),
                }],
                raw: message_raw,
            }],
            tools: vec![IrTool {
                name: "lookup".into(),
                description: Some("find heidi@example.com".into()),
                input_schema: json!({
                    "type": "object",
                    "properties": {"q": {"description": "ivan@example.com"}}
                }),
                raw: {
                    let mut raw = Map::new();
                    raw.insert("x_notes".into(), json!("judy@example.com"));
                    raw
                },
            }],
            tool_choice: json!({"type": "function", "function": {"name": "lookup"}}),
            response_format: json!({
                "type": "json_schema",
                "json_schema": {"description": "karl@example.com"}
            }),
            metadata: json!({"notes": "lena@example.com"}),
            stop: vec!["mallory@example.com".into()],
            raw: request_raw,
            ..Default::default()
        };

        let Outcome::Masked(report) = engine.mask_ir(&mut ir) else {
            panic!("mask mode must rewrite the IR");
        };

        // Every block keeps its variant — `Reasoning` must not become `Text`
        // (M1 regression anchor), `ToolResult` must not be split, `Raw` and the
        // media payload must survive untouched.
        let blocks = &ir.messages[0].content;
        assert_eq!(
            blocks[0],
            IrContentBlock::Text {
                text: format!("mail {}", ph("alice@example.com"))
            }
        );
        assert_eq!(
            blocks[1],
            IrContentBlock::Reasoning {
                text: format!("think {}", ph("bob@example.com"))
            }
        );
        assert_eq!(
            blocks[2],
            IrContentBlock::ToolUse {
                id: "call_1".into(),
                name: "lookup".into(),
                arguments: json!({"email": ph("carol@example.com")}),
            }
        );
        assert_eq!(
            blocks[3],
            IrContentBlock::ToolResult {
                id: None,
                tool_call_id: "call_1".into(),
                content: vec![IrContentBlock::Text {
                    text: ph("dave@example.com")
                }],
                is_error: false,
            }
        );
        assert_eq!(
            blocks[4],
            IrContentBlock::Image {
                data: json!({"url": "https://example.com/13812345678.png"})
            }
        );
        assert_eq!(
            blocks[5],
            IrContentBlock::Raw(json!({"type": "mystery", "body": ph("erin@example.com")}))
        );

        // Message-level text: `name`, tool-call arguments and `raw`.
        let message = &ir.messages[0];
        assert_eq!(
            message.name.as_deref(),
            Some(ph("frank@example.com").as_str())
        );
        assert_eq!(message.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(
            message.tool_calls[0].arguments,
            json!({"email": ph("grace@example.com")})
        );
        assert_eq!(message.raw["notes"], json!(ph("nick@example.com")));

        // Tools, tool_choice, response_format, metadata, stop and top-level raw.
        let tool = &ir.tools[0];
        assert_eq!(tool.name, "lookup");
        assert_eq!(
            tool.description.as_deref(),
            Some(format!("find {}", ph("heidi@example.com")).as_str())
        );
        assert_eq!(
            tool.input_schema["properties"]["q"]["description"],
            json!(ph("ivan@example.com"))
        );
        assert_eq!(tool.raw["x_notes"], json!(ph("judy@example.com")));
        assert_eq!(
            ir.response_format["json_schema"]["description"],
            json!(ph("karl@example.com"))
        );
        assert_eq!(ir.metadata["notes"], json!(ph("lena@example.com")));
        assert_eq!(ir.stop, vec![ph("mallory@example.com")]);
        assert_eq!(ir.raw["notes"], json!(ph("olive@example.com")));
        assert_eq!(ir.model, "gpt-4o");

        // One placeholder per distinct value, shared across every field.
        assert_eq!(report.hits, vec![("email".to_string(), 15)]);
        assert_eq!(report.map.len(), 15);
    }

    #[test]
    fn mask_request_applies_the_configured_mode() {
        let text = "mail alice@example.com";

        let engine = engine_with(|cfg| cfg.mode = PrivacyMode::Mask);
        let mut req = request_with_content(json!(text));
        assert!(matches!(engine.mask_request(&mut req), Outcome::Masked(_)));
        assert_ne!(req.messages[0].content, json!(text));

        let engine = engine_with(|cfg| cfg.mode = PrivacyMode::Warn);
        let mut req = request_with_content(json!(text));
        match engine.mask_request(&mut req) {
            Outcome::Detected(report) => {
                assert_eq!(report.hits, vec![("email".to_string(), 1)]);
                // Nothing was rewritten, so there is nothing to restore.
                assert!(report.map.is_empty());
            }
            _ => panic!("warn mode must report Detected"),
        }
        assert_eq!(req.messages[0].content, json!(text));

        let engine = engine_with(|cfg| cfg.mode = PrivacyMode::Block);
        let mut req = request_with_content(json!(text));
        assert!(matches!(engine.mask_request(&mut req), Outcome::Blocked(_)));
        assert_eq!(req.messages[0].content, json!(text));

        let engine = engine_with(|_| {});
        let mut req = request_with_content(json!("nothing sensitive here"));
        assert!(matches!(engine.mask_request(&mut req), Outcome::Clean));
    }

    #[test]
    fn engine_exposes_mode_and_restore_flag() {
        let engine = engine_with(|cfg| {
            cfg.mode = PrivacyMode::Block;
            cfg.restore = false;
        });
        assert_eq!(engine.mode(), PrivacyMode::Block);
        assert!(!engine.restore());
    }
}
