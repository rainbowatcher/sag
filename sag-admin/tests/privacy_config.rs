//! Integration tests for the `[privacy]` contract on `GET|PUT /api/config`.
//!
//! These drive the full admin request lifecycle (router → handler → coordinator
//! → committed `AppState`) over HTTP, so they live in the integration-test
//! directory per the project's testing conventions (AGENTS.md). The pure DTO /
//! serde mapping is covered by the in-crate `admin_config::privacy_dto_tests`;
//! this file covers what only an end-to-end request can prove:
//!
//! - the snapshot exposes the seven writable fields plus a `salt_configured`
//!   boolean and never the salt itself,
//! - the salt write rules (`omit` / `null` / `""` keep, non-empty replaces the
//!   bytes verbatim), asserted against the committed config through the public
//!   `AppState` handle,
//! - runtime-only updates never touch the privacy section,
//! - forward-compat (unknown fields ignored) and the rejection boundaries
//!   (unknown mode 422, partial privacy 422, invalid rule 400 even when
//!   disabled) all leave the committed config and `routes_version` untouched.
//!
//! Every salt below is a fictional marker; no real credential or config file is
//! read, and assertions never format a stored salt into a panic message.

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use sag_config::{
    ApiConfig, ApiKind, BackoffConfig, ConfigHandle, GatewayConfig, KeyStrategy, PrivacyBuiltins,
    PrivacyConfig, PrivacyCustomRegex, PrivacyMode, PrivacyWordList, ProviderConfig, ProxyMode,
};
use sag_state::AppState;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Salt stored in the seeded config. Unique marker: it must never appear in a
/// response body.
const STORED_SALT: &str = "test-stored-salt-0a1b2c";

/// Replacement salt used by the PUT tests. It carries literal leading/trailing
/// spaces on purpose — the write path must not trim it.
const REPLACEMENT_SALT: &str = "  test replacement salt-3d4e5f  ";

/// Marker salt carrying the characters JSON has to escape (`"` and `\`). A
/// leaking body would hold its *escaped* form, so the no-echo helper has to
/// look for both spellings.
const ESCAPE_SALT: &str = r#"test-"quoted"-\-backslash-salt-9f8e7d"#;

/// Non-default privacy values (everything except the salt) used as both the
/// seeded config and the expected result of a full-object PUT.
fn non_default_privacy() -> PrivacyConfig {
    PrivacyConfig {
        enabled: true,
        mode: PrivacyMode::Warn,
        restore: false,
        salt: String::new(),
        builtins: PrivacyBuiltins {
            email: false,
            ..PrivacyBuiltins::default()
        },
        custom_regexes: vec![PrivacyCustomRegex {
            pattern: r"EMP-\d{4}".into(),
            kind: "employee_id".into(),
            case_insensitive: true,
        }],
        custom_words: PrivacyWordList {
            items: vec!["alpha".into(), "alpha".into()],
        },
        allowlist: PrivacyWordList {
            items: vec!["beta".into()],
        },
    }
}

/// [`non_default_privacy`] with the stored salt seeded.
fn seeded_privacy() -> PrivacyConfig {
    PrivacyConfig {
        salt: STORED_SALT.into(),
        ..non_default_privacy()
    }
}

/// Complete `privacy` request object matching [`non_default_privacy`] (no
/// `salt` key; individual cases add the salt variant they exercise).
fn full_privacy_json() -> Value {
    json!({
        "enabled": true,
        "mode": "warn",
        "restore": false,
        "builtins": {"email": false},
        "custom_regexes": [
            {"kind": "employee_id", "pattern": r"EMP-\d{4}", "case_insensitive": true}
        ],
        "custom_words": {"items": ["alpha", "alpha"]},
        "allowlist": {"items": ["beta"]}
    })
}

/// Complete, valid backoff object, used to prove a runtime-only (or mixed)
/// update does not smuggle a privacy change.
fn full_backoff_json() -> Value {
    json!({
        "retries": 3,
        "base_ms": 200,
        "max_ms": 4000,
        "jitter": false,
        "retry_on_empty_response": false
    })
}

/// Minimal provider: the unified mutation protocol rejects a config without
/// any provider, so every fixture seeds this one (it is never contacted).
fn provider_config() -> ProviderConfig {
    let mut apis = indexmap::IndexMap::new();
    apis.insert(
        ApiKind::ChatCompletions,
        ApiConfig {
            base_url: "https://api.example.com/v1".into(),
            ..ApiConfig::default()
        },
    );
    ProviderConfig {
        api_keys: vec!["sk-test-only".into()],
        key_strategy: KeyStrategy::FillFirst,
        proxy_mode: ProxyMode::default(),
        timeout_secs: 120,
        apis,
        ..ProviderConfig::default()
    }
}

/// In-memory `AppState` + admin router, exactly like the gateway's own
/// admin-test wiring (no auth middleware on the nested surface).
struct Harness {
    state: AppState,
    app: Router,
}

impl Harness {
    async fn with_privacy(privacy: PrivacyConfig) -> Self {
        let mut providers = std::collections::HashMap::new();
        providers.insert("openai".to_string(), provider_config());
        let config = GatewayConfig {
            privacy,
            providers,
            ..sag_config::gateway_config_base!()
        };
        let handle = ConfigHandle::from_config(config);
        let registry = sag_state::testing::build_provider_registry(&handle, false).await;
        let state = sag_state::testing::test_app_state(handle, registry).await;
        let app = Router::new()
            .nest("/api", sag_admin::build_admin_routes(state.clone()))
            .with_state(state.clone());
        Self { state, app }
    }

    /// `GET /api/config`, returning the status and the raw body text.
    async fn get(&self) -> (StatusCode, String) {
        let req = Request::builder()
            .method("GET")
            .uri("/api/config")
            .body(Body::empty())
            .unwrap();
        self.send(req).await
    }

    /// `PUT /api/config` with a JSON body, returning the status and the raw
    /// body text (error bodies are not always JSON, so callers parse when they
    /// expect a JSON payload).
    async fn put(&self, payload: &Value) -> (StatusCode, String) {
        let req = Request::builder()
            .method("PUT")
            .uri("/api/config")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(payload).unwrap()))
            .unwrap();
        self.send(req).await
    }

    async fn send(&self, req: Request<Body>) -> (StatusCode, String) {
        let response = self.app.clone().oneshot(req).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 256 * 1024)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Committed privacy section, cloned so the read guard is released.
    async fn stored_privacy(&self) -> PrivacyConfig {
        self.state.config.read().await.privacy.clone()
    }

    async fn stored_backoff(&self) -> BackoffConfig {
        self.state.config.read().await.backoff.clone()
    }

    async fn routes_version(&self) -> u64 {
        self.state.config.routes_version()
    }
}

fn json_body(text: &str) -> Value {
    serde_json::from_str(text).expect("response body must be JSON")
}

/// Whether `text` leaks `secret` as raw bytes or as the JSON-escaped spelling a
/// serialized response body would contain.
fn echoes_secret(text: &str, secret: &str) -> bool {
    let serialized = serde_json::to_string(secret).expect("a salt must serialize");
    let escaped = serialized
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .expect("serializing a string yields a quoted JSON string");
    text.contains(secret) || text.contains(escaped)
}

/// No response — success or error — may echo any stored or submitted salt,
/// including the escaped form of a salt holding `"` or `\`.
fn assert_no_salt_echo(text: &str) {
    for secret in [STORED_SALT, REPLACEMENT_SALT, ESCAPE_SALT] {
        assert!(
            !echoes_secret(text, secret),
            "an admin response body must never echo a salt value, raw or JSON-escaped"
        );
    }
}

/// Asserts the snapshot carries the seven writable fields plus
/// `salt_configured`, mapped from `expected`, and no `salt` key at all.
fn assert_privacy_snapshot(body: &Value, expected: &PrivacyConfig, salt_configured: bool) {
    let privacy = body
        .get("privacy")
        .expect("the config snapshot must carry a privacy object");
    assert!(
        privacy.is_object(),
        "the privacy snapshot must be a JSON object"
    );

    let mut keys: Vec<&str> = privacy
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "allowlist",
            "builtins",
            "custom_regexes",
            "custom_words",
            "enabled",
            "mode",
            "restore",
            "salt_configured",
        ],
        "the snapshot exposes exactly the seven fields plus salt_configured"
    );

    assert_eq!(privacy["enabled"], json!(expected.enabled));
    assert_eq!(privacy["mode"], json!(expected.mode));
    assert_eq!(privacy["restore"], json!(expected.restore));
    assert_eq!(
        privacy["builtins"],
        serde_json::to_value(expected.builtins).unwrap()
    );
    assert_eq!(
        privacy["custom_regexes"],
        serde_json::to_value(&expected.custom_regexes).unwrap()
    );
    assert_eq!(
        privacy["custom_words"],
        serde_json::to_value(&expected.custom_words).unwrap()
    );
    assert_eq!(
        privacy["allowlist"],
        serde_json::to_value(&expected.allowlist).unwrap()
    );
    assert_eq!(
        privacy["salt_configured"],
        json!(salt_configured),
        "salt_configured must report whether a non-empty salt is stored"
    );
}

/// Equality without formatting the salt into a panic message.
fn assert_privacy_equals(actual: &PrivacyConfig, expected: &PrivacyConfig, context: &str) {
    assert!(
        actual == expected,
        "{context}: committed privacy config differs from the expected one"
    );
}

/// Salt comparison without formatting the stored bytes.
fn assert_salt_equals(actual: &PrivacyConfig, expected: &str, context: &str) {
    assert!(
        actual.salt == expected,
        "{context}: stored salt bytes differ from the expected value"
    );
}

#[tokio::test]
async fn get_default_privacy_never_auto_enables() {
    let harness = Harness::with_privacy(PrivacyConfig::default()).await;

    let (status, text) = harness.get().await;
    assert_eq!(status, StatusCode::OK);

    let body = json_body(&text);
    assert_privacy_snapshot(&body, &PrivacyConfig::default(), false);
    // Spot-check the documented defaults: mask / restore on / all detectors on.
    assert_eq!(body["privacy"]["enabled"], json!(false));
    assert_eq!(body["privacy"]["mode"], json!("mask"));
    assert_eq!(body["privacy"]["restore"], json!(true));
    assert_eq!(body["privacy"]["builtins"]["private_key"], json!(true));
    assert_eq!(body["privacy"]["custom_regexes"], json!([]));

    // A read must never materialize filtering: the committed config stays off.
    let stored = harness.stored_privacy().await;
    assert!(
        !stored.enabled,
        "a GET must not auto-enable privacy filtering"
    );
    assert_salt_equals(&stored, "", "default config");
    assert_no_salt_echo(&text);
}

#[tokio::test]
async fn get_non_default_privacy_reports_every_field_without_the_salt() {
    let harness = Harness::with_privacy(seeded_privacy()).await;

    let (status, text) = harness.get().await;
    assert_eq!(status, StatusCode::OK);

    let body = json_body(&text);
    // `seeded_privacy` is `non_default_privacy` plus the salt, which only
    // surfaces as `salt_configured: true`.
    assert_privacy_snapshot(&body, &non_default_privacy(), true);
    assert_no_salt_echo(&text);
}

#[tokio::test]
async fn put_full_privacy_returns_latest_snapshot_without_old_or_new_salt() {
    let harness = Harness::with_privacy(seeded_privacy()).await;
    let version_before = harness.routes_version().await;

    let mut privacy = full_privacy_json();
    privacy["mode"] = json!("block");
    privacy["restore"] = json!(true);
    privacy["salt"] = json!(REPLACEMENT_SALT);

    let (status, text) = harness.put(&json!({"privacy": privacy})).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a valid full privacy replace must succeed"
    );
    assert_no_salt_echo(&text);

    let expected = PrivacyConfig {
        mode: PrivacyMode::Block,
        restore: true,
        salt: REPLACEMENT_SALT.into(),
        ..non_default_privacy()
    };
    assert_privacy_snapshot(&json_body(&text), &expected, true);

    // The committed config carries the new values and the salt replaced byte
    // for byte (leading/trailing spaces included).
    let stored = harness.stored_privacy().await;
    assert_privacy_equals(&stored, &expected, "full replace");
    assert_salt_equals(&stored, REPLACEMENT_SALT, "full replace");
    assert_eq!(
        harness.routes_version().await,
        version_before + 1,
        "a committed mutation bumps routes_version"
    );

    // A following GET reports the same latest snapshot, still without a salt.
    let (status, text) = harness.get().await;
    assert_eq!(status, StatusCode::OK);
    assert_privacy_snapshot(&json_body(&text), &expected, true);
    assert_no_salt_echo(&text);
}

#[derive(Debug, Clone, Copy)]
enum SaltVariant {
    Omitted,
    Null,
    EmptyString,
}

#[tokio::test]
async fn salt_omitted_null_or_empty_keeps_the_stored_value() {
    for variant in [
        SaltVariant::Omitted,
        SaltVariant::Null,
        SaltVariant::EmptyString,
    ] {
        let harness = Harness::with_privacy(seeded_privacy()).await;

        let mut privacy = full_privacy_json();
        match variant {
            SaltVariant::Omitted => {}
            SaltVariant::Null => privacy["salt"] = Value::Null,
            SaltVariant::EmptyString => privacy["salt"] = json!(""),
        }

        let (status, text) = harness.put(&json!({"privacy": privacy})).await;
        assert_eq!(status, StatusCode::OK, "{variant:?} salt must be accepted");
        assert_no_salt_echo(&text);
        assert_privacy_snapshot(&json_body(&text), &non_default_privacy(), true);

        let stored = harness.stored_privacy().await;
        assert_privacy_equals(&stored, &seeded_privacy(), &format!("{variant:?} salt"));
        assert_salt_equals(&stored, STORED_SALT, &format!("{variant:?} salt"));
    }
}

#[tokio::test]
async fn non_empty_salt_replaces_the_stored_value_verbatim() {
    // The escape-bearing salt runs through the very same PUT → commit → GET
    // path, so the no-echo checks below really do see its JSON-escaping spelling.
    for replacement in [REPLACEMENT_SALT, ESCAPE_SALT] {
        let harness = Harness::with_privacy(seeded_privacy()).await;

        let mut privacy = full_privacy_json();
        privacy["salt"] = json!(replacement);

        let (status, text) = harness.put(&json!({"privacy": privacy})).await;
        assert_eq!(status, StatusCode::OK, "a non-empty salt must be accepted");
        assert_no_salt_echo(&text);
        assert_privacy_snapshot(&json_body(&text), &non_default_privacy(), true);

        // Not trimmed: surrounding spaces (or escapes) are part of the salt.
        let stored = harness.stored_privacy().await;
        assert_salt_equals(&stored, replacement, "byte-for-byte replace");

        // The follow-up GET reports `salt_configured` without echoing the salt.
        let (status, text) = harness.get().await;
        assert_eq!(status, StatusCode::OK);
        assert_privacy_snapshot(&json_body(&text), &non_default_privacy(), true);
        assert_no_salt_echo(&text);
    }
}

/// The escape marker is only meaningful if it really holds JSON specials, and
/// the helper must catch a leak the raw-substring check alone would miss.
#[test]
fn no_salt_echo_helper_covers_json_escaped_salt_forms() {
    assert!(
        ESCAPE_SALT.contains('"') && ESCAPE_SALT.contains('\\'),
        "the escape marker must carry both `\"` and `\\`"
    );

    let leaking_body = json!({"privacy": {"salt": ESCAPE_SALT}}).to_string();
    assert!(
        !leaking_body.contains(ESCAPE_SALT),
        "a JSON body holds the escaped spelling, not the raw marker"
    );
    assert!(
        echoes_secret(&leaking_body, ESCAPE_SALT),
        "the no-echo check must catch an escaped salt leak"
    );
    assert!(
        !echoes_secret(
            &json!({"privacy": {"salt_configured": true}}).to_string(),
            ESCAPE_SALT
        ),
        "a snapshot without a salt must not look like a leak"
    );
}

#[tokio::test]
async fn runtime_only_put_leaves_privacy_and_salt_untouched() {
    let harness = Harness::with_privacy(seeded_privacy()).await;
    let privacy_before = harness.stored_privacy().await;
    let version_before = harness.routes_version().await;

    let (status, text) = harness
        .put(&json!({
            "backoff": full_backoff_json(),
            "shuffle_cache_ttl_secs": 123,
            "sticky_failover_reset_secs": 456
        }))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_no_salt_echo(&text);
    assert_privacy_snapshot(&json_body(&text), &non_default_privacy(), true);

    let stored = harness.stored_privacy().await;
    assert_privacy_equals(&stored, &privacy_before, "runtime-only update");
    assert_salt_equals(&stored, STORED_SALT, "runtime-only update");
    assert_eq!(harness.stored_backoff().await.retries, 3);
    assert_eq!(harness.routes_version().await, version_before + 1);
}

#[tokio::test]
async fn unknown_fields_are_tolerated_at_both_levels() {
    let harness = Harness::with_privacy(PrivacyConfig::default()).await;

    let mut privacy = full_privacy_json();
    privacy["future_flag"] = json!({"nested": true});
    privacy["builtins"]["face_id"] = json!(true);
    privacy["custom_words"]["future_items"] = json!(["ignored"]);
    let payload = json!({
        "unknown_top_level": true,
        "server": {"port": 9},
        "privacy": privacy
    });

    let (status, text) = harness.put(&payload).await;
    assert_eq!(status, StatusCode::OK, "unknown fields must be ignored");
    assert_no_salt_echo(&text);

    // Known fields applied; unknown ones neither rejected nor mapped onto a
    // known field (the builtins defaults stay untouched except `email`).
    let expected = non_default_privacy();
    assert_privacy_snapshot(&json_body(&text), &expected, false);
    assert_privacy_equals(
        &harness.stored_privacy().await,
        &expected,
        "unknown-field PUT",
    );
}

#[tokio::test]
async fn unknown_mode_is_422_and_commits_nothing() {
    let harness = Harness::with_privacy(seeded_privacy()).await;
    let privacy_before = harness.stored_privacy().await;
    let version_before = harness.routes_version().await;

    let mut privacy = full_privacy_json();
    privacy["mode"] = json!("enabled");

    let (status, text) = harness.put(&json!({"privacy": privacy})).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an unknown mode must fail deserialization"
    );
    assert_no_salt_echo(&text);
    assert_privacy_equals(
        &harness.stored_privacy().await,
        &privacy_before,
        "unknown mode",
    );
    assert_eq!(harness.routes_version().await, version_before);
}

#[tokio::test]
async fn partial_privacy_is_422_and_commits_nothing() {
    let harness = Harness::with_privacy(seeded_privacy()).await;
    let privacy_before = harness.stored_privacy().await;
    let version_before = harness.routes_version().await;

    // A partial object must not silently default the seven writable fields.
    let (status, text) = harness.put(&json!({"privacy": {"enabled": false}})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_no_salt_echo(&text);
    assert_privacy_equals(
        &harness.stored_privacy().await,
        &privacy_before,
        "partial privacy",
    );
    assert_eq!(harness.routes_version().await, version_before);
}

#[derive(Debug, Clone, Copy)]
enum InvalidRule {
    UnclosedPattern,
    EmptyKind,
}

#[tokio::test]
async fn invalid_custom_rule_is_400_even_when_disabled() {
    for enabled in [true, false] {
        for rule in [InvalidRule::UnclosedPattern, InvalidRule::EmptyKind] {
            let harness = Harness::with_privacy(seeded_privacy()).await;
            let privacy_before = harness.stored_privacy().await;
            let version_before = harness.routes_version().await;

            let mut privacy = full_privacy_json();
            privacy["enabled"] = json!(enabled);
            privacy["salt"] = json!(REPLACEMENT_SALT);
            privacy["custom_regexes"] = match rule {
                InvalidRule::UnclosedPattern => json!([
                    {"kind": "employee_id", "pattern": "(unclosed", "case_insensitive": false}
                ]),
                InvalidRule::EmptyKind => json!([
                    {"kind": "   ", "pattern": r"EMP-\d{4}", "case_insensitive": false}
                ]),
            };

            let context = format!("enabled={enabled} rule={rule:?}");
            let (status, text) = harness.put(&json!({"privacy": privacy})).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{context}: invalid rules are validated regardless of `enabled`"
            );
            assert_no_salt_echo(&text);

            let body = json_body(&text);
            assert_eq!(body["error"]["code"], json!("BAD_REQUEST"));
            assert!(
                body["error"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("privacy.custom_regexes[0]"),
                "{context}: the error must name the offending rule"
            );

            assert_privacy_equals(&harness.stored_privacy().await, &privacy_before, &context);
            assert_eq!(harness.routes_version().await, version_before);
        }
    }
}

#[tokio::test]
async fn mixed_backoff_and_invalid_privacy_commit_nothing() {
    let harness = Harness::with_privacy(seeded_privacy()).await;
    let backoff_before = harness.stored_backoff().await;
    let privacy_before = harness.stored_privacy().await;
    let version_before = harness.routes_version().await;

    // One PUT, one mutation: the rejected privacy section must roll back the
    // otherwise-valid backoff change too (no partial commit).
    let mut privacy = full_privacy_json();
    privacy["custom_regexes"] = json!([
        {"kind": "employee_id", "pattern": "(unclosed", "case_insensitive": false}
    ]);
    let payload = json!({
        "backoff": full_backoff_json(),
        "shuffle_cache_ttl_secs": 321,
        "privacy": privacy
    });

    let (status, text) = harness.put(&payload).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "the invalid privacy section rejects the whole request"
    );
    assert_no_salt_echo(&text);

    assert!(
        harness.stored_backoff().await == backoff_before,
        "the valid backoff change must not commit alongside a rejected privacy section"
    );
    assert_privacy_equals(
        &harness.stored_privacy().await,
        &privacy_before,
        "mixed backoff + invalid privacy",
    );
    assert_eq!(
        harness.state.config.read().await.shuffle_cache_ttl_secs,
        7200,
        "the valid TTL change must not commit either"
    );
    assert_eq!(harness.routes_version().await, version_before);
}
