//! Filesystem-level integration tests for persisting the `[privacy]`
//! section.
//!
//! These live under `tests/` (not inline in `save.rs`) because every case
//! reads and writes a real config file and reloads it through the public
//! loader. The shared invariant under test: a save only rewrites the privacy
//! fields whose typed value actually changed, keeps unknown keys / comments /
//! hand-authored table forms intact, and never materialises `[privacy]` on an
//! unrelated save.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use sag_config::{
    ApiConfig, ApiKind, GatewayConfig, PrivacyBuiltins, PrivacyConfig, PrivacyCustomRegex,
    PrivacyMode, PrivacyWordList, ProviderConfig, load_config, save_config_to_file,
};

const BASE: &str = r#"[server]
host = "127.0.0.1"
port = 9090

[providers.openai]
api_keys = ["sk-test"]
apis = { chat_completions = { base_url = "https://api.openai.com/v1" } }
"#;

struct TempConfig {
    path: PathBuf,
}

impl TempConfig {
    fn new(name: &str, body: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "sag-privacy-persistence-{}-{name}.toml",
            std::process::id()
        ));
        std::fs::write(&path, body).expect("write test config");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> String {
        std::fs::read_to_string(&self.path).expect("read test config")
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn config_with(privacy: PrivacyConfig) -> GatewayConfig {
    let provider = ProviderConfig {
        api_keys: vec!["sk-test".into()],
        apis: IndexMap::from([(
            ApiKind::ChatCompletions,
            ApiConfig {
                base_url: "https://api.openai.com/v1".into(),
                ..Default::default()
            },
        )]),
        ..Default::default()
    };
    GatewayConfig {
        providers: HashMap::from([("openai".into(), provider)]),
        privacy,
        ..sag_config::gateway_config_base!()
    }
}

async fn reload(path: &Path) -> GatewayConfig {
    let handle = load_config(path).expect("saved config should load");
    handle.read().await.clone()
}

/// The `[privacy]`/`privacy = {…}` block, trimmed at the next top-level
/// section so trailing unrelated writes (e.g. a newly created `[backoff]`)
/// do not pollute the comparison.
fn privacy_suffix(text: &str) -> &str {
    let start = text
        .find("[privacy]")
        .or_else(|| text.find("privacy = {"))
        .expect("privacy section present");
    let rest = &text[start..];
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let is_privacy_header = line.starts_with("[privacy") || line.starts_with("[[privacy");
        if offset > 0 && line.starts_with('[') && !is_privacy_header {
            return rest[..offset].trim_end_matches('\n');
        }
        offset += line.len();
    }
    rest.trim_end_matches('\n')
}

fn regex(kind: &str, pattern: &str, case_insensitive: bool) -> PrivacyCustomRegex {
    PrivacyCustomRegex {
        kind: kind.into(),
        pattern: pattern.into(),
        case_insensitive,
    }
}

fn words(items: &[&str]) -> PrivacyWordList {
    PrivacyWordList {
        items: items.iter().map(|item| item.to_string()).collect(),
    }
}

fn full_privacy() -> PrivacyConfig {
    PrivacyConfig {
        enabled: true,
        mode: PrivacyMode::Block,
        restore: false,
        salt: "round-trip-salt".into(),
        builtins: PrivacyBuiltins {
            email: false,
            phone: false,
            id_card_cn: true,
            bank_card: false,
            iban: false,
            api_key: true,
            private_key: false,
        },
        custom_regexes: vec![
            regex("employee_id", r"EMP-\d{6}", false),
            regex("ticket", r"TCK-\d+", true),
        ],
        custom_words: words(&["alice", "bob"]),
        allowlist: words(&["public@example.com"]),
    }
}

#[tokio::test]
async fn full_privacy_config_round_trips_and_reloads() {
    let file = TempConfig::new("full", BASE);
    let privacy = full_privacy();

    save_config_to_file(&config_with(privacy.clone()), file.path()).expect("save");
    let saved = file.read();
    assert!(
        saved.contains("[privacy]"),
        "privacy section should be created, got:\n{saved}"
    );
    assert_eq!(
        reload(file.path()).await.privacy,
        privacy,
        "every privacy field must survive save + reload"
    );

    // Re-saving an unchanged config must not rewrite the section at all.
    let before = privacy_suffix(&saved).to_string();
    save_config_to_file(&config_with(privacy), file.path()).expect("second save");
    assert_eq!(
        privacy_suffix(&file.read()),
        before,
        "an unchanged privacy section must not be rewritten"
    );
}

#[tokio::test]
async fn clearing_rules_and_word_lists_removes_old_entries() {
    let file = TempConfig::new("clear", BASE);
    let original = full_privacy();
    save_config_to_file(&config_with(original.clone()), file.path()).expect("seed save");

    let mut cleared = reload(file.path()).await.privacy;
    cleared.custom_regexes.clear();
    cleared.custom_words.items.clear();
    cleared.allowlist.items.clear();

    save_config_to_file(&config_with(cleared.clone()), file.path()).expect("clear save");
    let saved = file.read();
    assert!(
        !saved.contains(r"EMP-\d{6}") && !saved.contains("TCK-"),
        "removed rules must not linger on disk, got:\n{saved}"
    );
    assert!(
        !saved.contains("alice") && !saved.contains("public@example.com"),
        "cleared word lists must not linger on disk, got:\n{saved}"
    );

    let reloaded = reload(file.path()).await.privacy;
    assert_eq!(reloaded, cleared);
    assert!(reloaded.custom_regexes.is_empty());
    assert!(reloaded.custom_words.items.is_empty());
    assert!(reloaded.allowlist.items.is_empty());
}

#[tokio::test]
async fn duplicate_kinds_and_item_order_round_trip() {
    let file = TempConfig::new("duplicates", BASE);
    let mut privacy = PrivacyConfig {
        enabled: true,
        custom_regexes: vec![
            regex("dup", "DUP-1", false),
            regex("dup", "DUP-2", true),
            regex("other", "OTHER-1", false),
        ],
        custom_words: words(&["b", "a", "b"]),
        ..PrivacyConfig::default()
    };
    save_config_to_file(&config_with(privacy.clone()), file.path()).expect("save");
    assert_eq!(reload(file.path()).await.privacy, privacy);

    // Reorder, drop one rule, and edit a word list. Duplicate kinds and item
    // order (including duplicates) must follow the candidate exactly.
    privacy.custom_regexes = vec![
        regex("other", "OTHER-1", false),
        regex("dup", "DUP-2", true),
        regex("dup", "DUP-3", false),
    ];
    privacy.custom_words = words(&["a", "b", "b", "c"]);
    save_config_to_file(&config_with(privacy.clone()), file.path()).expect("update save");

    let reloaded = reload(file.path()).await.privacy;
    assert_eq!(reloaded, privacy);
    assert_eq!(
        reloaded
            .custom_regexes
            .iter()
            .map(|rule| rule.kind.as_str())
            .collect::<Vec<_>>(),
        vec!["other", "dup", "dup"],
        "duplicate kinds must not be merged or re-keyed"
    );
    assert_eq!(reloaded.custom_words.items, vec!["a", "b", "b", "c"]);
}

#[tokio::test]
async fn inline_privacy_form_is_updated_in_place() {
    let body = format!(
        r#"privacy = {{ enabled = true, mode = "mask", restore = true, salt = "", builtins = {{ email = false, phone = true, id_card_cn = true, bank_card = true, iban = true, api_key = true, private_key = true }}, custom_regexes = [{{ kind = "k1", pattern = "p1" }}], custom_words = {{ items = ["w1"], future_list = "keep" }}, allowlist = {{ items = [] }}, future_root = "keep" }}

{BASE}"#
    );
    let file = TempConfig::new("inline", &body);
    let mut privacy = reload(file.path()).await.privacy;
    privacy.mode = PrivacyMode::Warn;
    privacy.builtins.email = true;
    privacy.custom_regexes.push(regex("k2", "p2", true));
    privacy.custom_words.items.push("w2".into());
    privacy.allowlist.items.push("ok".into());

    save_config_to_file(&config_with(privacy.clone()), file.path()).expect("save");
    let saved = file.read();
    assert!(
        saved.contains("privacy = {"),
        "inline privacy form must be preserved, got:\n{saved}"
    );
    assert!(
        saved.contains("custom_regexes = ["),
        "inline rule array form must be preserved, got:\n{saved}"
    );
    assert!(
        !saved.contains("[[privacy.custom_regexes]]"),
        "inline config must not be converted to array-of-tables, got:\n{saved}"
    );
    assert!(saved.contains(r#"future_root = "keep""#));
    assert!(saved.contains(r#"future_list = "keep""#));
    assert_eq!(reload(file.path()).await.privacy, privacy);

    // Clearing from the inline form removes the old rows while keeping the
    // inline array shape.
    privacy.custom_regexes.clear();
    privacy.custom_words.items.clear();
    save_config_to_file(&config_with(privacy.clone()), file.path()).expect("clear save");
    let saved = file.read();
    assert!(
        saved.contains("custom_regexes = []"),
        "cleared inline rules should be an explicit empty array, got:\n{saved}"
    );
    assert!(!saved.contains(r#""p1""#) && !saved.contains(r#""p2""#));
    assert_eq!(reload(file.path()).await.privacy, privacy);
}

#[tokio::test]
async fn table_form_preserves_comments_and_unknown_fields() {
    let body = format!(
        r#"{BASE}
[privacy]
# privacy comment
enabled = true
mode = "mask"
future_root = "keep"

[privacy.builtins]
email = true
future_detector = true

[privacy.custom_words]
# words comment
items = ["alpha", "alpha"]

[privacy.allowlist]
items = []

[[privacy.custom_regexes]]
# rule one comment
kind = "empid"
pattern = "EMP-1"
note = "keep-row-unknown"

[[privacy.custom_regexes]]
kind = "ticket"
pattern = "TCK-1"
case_insensitive = false
"#
    );
    let file = TempConfig::new("tables", &body);
    let mut privacy = reload(file.path()).await.privacy;
    privacy.mode = PrivacyMode::Warn;
    privacy.builtins.email = false;
    privacy.custom_words.items.push("beta".into());
    privacy
        .custom_regexes
        .push(regex("new_kind", "NEW-1", true));

    save_config_to_file(&config_with(privacy.clone()), file.path()).expect("save");
    let saved = file.read();
    for expected in [
        "# privacy comment",
        "# words comment",
        "# rule one comment",
        r#"future_root = "keep""#,
        "future_detector = true",
        r#"note = "keep-row-unknown""#,
        "[privacy.builtins]",
        "[[privacy.custom_regexes]]",
    ] {
        assert!(
            saved.contains(expected),
            "expected {expected:?} to be preserved, got:\n{saved}"
        );
    }
    assert_eq!(reload(file.path()).await.privacy, privacy);
}

#[tokio::test]
async fn unparsable_privacy_section_fails_closed_without_touching_file() {
    // Legal TOML with an invalid value type: the on-disk privacy projection
    // cannot be read, so the save must abort before the atomic write instead
    // of silently rewriting the section from defaults.
    let body = format!("{BASE}\n[privacy]\nenabled = \"oops\"\n");
    let file = TempConfig::new("fail-closed", &body);
    let before = file.read();
    let mut config = config_with(PrivacyConfig::default());
    config
        .providers
        .get_mut("openai")
        .expect("provider present")
        .api_keys = vec!["sk-new".into()];

    // Assert `is_err` only: the error category (Internal vs Config) belongs
    // to the loader's own contract, not to this fail-closed invariant.
    let result = save_config_to_file(&config, file.path());
    assert!(result.is_err(), "type-invalid privacy must fail the save");
    assert_eq!(
        file.read(),
        before,
        "file bytes must be untouched when the privacy section cannot be parsed"
    );
}

#[tokio::test]
async fn dotted_key_forms_survive_partial_updates() {
    // Root-level dotted keys (before any table header) create `privacy` as a
    // dotted table. Updating a scalar/sub-table field must keep that shape
    // instead of materialising a `[privacy]` section.
    let body = format!(
        r#"privacy.enabled = true
privacy.mode = "mask"
privacy.salt = "dotted-salt"
privacy.builtins.email = false
privacy.builtins.phone = true
privacy.future_root = "keep"

{BASE}"#
    );
    let file = TempConfig::new("dotted-root", &body);
    let mut privacy = reload(file.path()).await.privacy;
    assert!(!privacy.builtins.email && privacy.builtins.phone);
    privacy.mode = PrivacyMode::Warn;
    privacy.salt = "dotted-salt-2".into();
    privacy.builtins.phone = false;
    privacy.custom_regexes.push(regex("dotted", "DOT-1", true));

    save_config_to_file(&config_with(privacy.clone()), file.path()).expect("save");
    let saved = file.read();
    assert!(
        !saved.contains("[privacy]"),
        "root dotted keys must not be converted into a [privacy] table, got:\n{saved}"
    );
    for expected in [
        "privacy.builtins.email = false",
        "privacy.builtins.phone = false",
        r#"privacy.salt = "dotted-salt-2""#,
        r#"privacy.future_root = "keep""#,
    ] {
        assert!(
            saved.contains(expected),
            "expected {expected:?} to be preserved, got:\n{saved}"
        );
    }
    assert_eq!(reload(file.path()).await.privacy, privacy);

    // Dotted keys inside an explicit `[privacy]` section are the same shape
    // one level down and must also be updated in place.
    let body = format!(
        r#"{BASE}
[privacy]
enabled = true
builtins.email = false
"#
    );
    let file = TempConfig::new("dotted-section", &body);
    let mut privacy = reload(file.path()).await.privacy;
    privacy.builtins.phone = false;

    save_config_to_file(&config_with(privacy.clone()), file.path()).expect("save");
    let saved = file.read();
    assert!(
        !saved.contains("[privacy.builtins]"),
        "dotted sub-table must not be expanded into a header table, got:\n{saved}"
    );
    for expected in ["builtins.email = false", "builtins.phone = false"] {
        assert!(
            saved.contains(expected),
            "expected {expected:?} to be preserved, got:\n{saved}"
        );
    }
    assert_eq!(reload(file.path()).await.privacy, privacy);
}

#[tokio::test]
async fn unrelated_save_does_not_materialise_privacy() {
    let file = TempConfig::new("no-materialise", BASE);
    save_config_to_file(&config_with(PrivacyConfig::default()), file.path()).expect("save");
    let saved = file.read();
    assert!(
        !saved.contains("privacy"),
        "an unrelated save must not create a default [privacy] section, got:\n{saved}"
    );
}

#[tokio::test]
async fn unrelated_save_leaves_existing_privacy_untouched() {
    let body = format!(
        r#"{BASE}
[privacy]
# hand-written section
enabled = true
mode = "warn"
future_flag = true

[privacy.custom_words]
items = ["keep-me"]
"#
    );
    let file = TempConfig::new("untouched", &body);
    let mut config = reload(file.path()).await;
    let privacy_before = config.privacy.clone();
    let section_before = privacy_suffix(&file.read()).to_string();

    // Only a provider field changes; privacy is semantically unchanged.
    config
        .providers
        .get_mut("openai")
        .expect("provider present")
        .api_keys = vec!["sk-rotated".into()];
    save_config_to_file(&config, file.path()).expect("save");

    let saved = file.read();
    assert!(saved.contains("sk-rotated"));
    assert_eq!(
        privacy_suffix(&saved),
        section_before,
        "an unrelated save must not rewrite the existing privacy section"
    );
    assert_eq!(reload(file.path()).await.privacy, privacy_before);
}
