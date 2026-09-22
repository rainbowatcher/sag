use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::config::{GatewayConfig, LogFormat};
use crate::loader::handle::ConfigHandle;

/// Quickly read `logging.level` and `logging.format` from a TOML file without
/// full config parsing. Returns `(level, format)`.
///
/// - Missing `format` key or missing `[logging]` section → defaults
///   (`level = "info"`, `format = LogFormat::default()` = Compact). The
///   default only ever fills in *absent* keys (same semantics as the main
///   loader's `#[serde(default)]` policy) — peek never deserializes into
///   `LoggingConfig` itself.
/// - `format` present but unknown (not `"compact"` / `"json"`, or not a
///   string) → `Err(AppError::Config)`, same hard-reject policy as the main
///   loader for typed enum values: never silently coerce to Compact.
/// - Unreadable file / invalid TOML → defaults with an eprintln so bootstrap
///   can still start tracing; the subsequent `load_config` surfaces the real
///   error.
///
/// This is used before tracing is initialized so that the log level and format
/// from the config file can be applied early, without waiting for the full
/// config load.
pub fn peek_logging_config(
    path: impl AsRef<Path>,
) -> Result<(String, LogFormat), sag_core::AppError> {
    let path = path.as_ref();
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[sag] failed to read config file '{}' for logging peek: {e}",
                path.display()
            );
            return Ok(("info".into(), LogFormat::default()));
        }
    };
    // `format` is read as `toml::Value` (not `String`) so a non-string value
    // reaches the hard-reject below instead of failing the whole ConfigPeek
    // and falling into the "TOML failed → bootstrap default" branch.
    #[derive(serde::Deserialize)]
    struct LoggingPeek {
        level: Option<String>,
        format: Option<toml::Value>,
    }
    #[derive(serde::Deserialize)]
    struct ConfigPeek {
        logging: Option<LoggingPeek>,
    }
    let config: ConfigPeek = match toml::from_str(&content) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[sag] failed to parse config file '{}' as TOML for logging peek: {e}",
                path.display()
            );
            return Ok(("info".into(), LogFormat::default()));
        }
    };
    let logging = match config.logging {
        Some(l) => l,
        None => {
            eprintln!(
                "[sag] no [logging] section found in config file '{}', using defaults",
                path.display()
            );
            return Ok(("info".into(), LogFormat::default()));
        }
    };
    let level = logging.level.unwrap_or_else(|| "info".into());
    let format = match logging.format {
        None => LogFormat::default(),
        Some(toml::Value::String(s)) => match s.as_str() {
            "compact" => LogFormat::Compact,
            "json" => LogFormat::Json,
            other => {
                return Err(sag_core::AppError::Config(format!(
                    "invalid logging.format '{other}': expected \"compact\" or \"json\""
                )));
            }
        },
        Some(other) => {
            return Err(sag_core::AppError::Config(format!(
                "invalid logging.format: expected string, got {other}"
            )));
        }
    };
    Ok((level, format))
}

/// Parse a TOML document into `T`, returning the typed value plus the dotted
/// paths of any fields the type does not recognise (via `serde_ignored`).
///
/// Unknown fields are never rejected — the gateway's contract is to warn and
/// continue. The returned `Vec` holds paths like `providers.openai.max_tokens`
/// or `routes.0.targets.0.model`. Keys of dynamic maps (e.g. `providers.<name>`,
/// `headers.<name>`, `body_overrides.<key>`, `RouteTarget.extra.<key>`) are
/// legitimately consumed by the map types and are therefore **not** reported.
fn deserialize_config_collecting<T: serde::de::DeserializeOwned>(
    content: &str,
) -> Result<(T, Vec<String>), toml::de::Error> {
    let deserializer = toml::Deserializer::parse(content)?;
    let mut unknown = Vec::new();
    let config: T = serde_ignored::deserialize(deserializer, |path| {
        unknown.push(path.to_string());
    })?;
    Ok((config, unknown))
}

/// Parse a TOML document into `T`, emitting a `warn!` for every field the
/// typed struct does not recognise. Unknown fields are tolerated (warn, never
/// deny), matching the gateway's forward-compatibility contract. Dynamic map
/// keys (provider names, header names, body-override keys, …) are valid and
/// produce no warning.
fn deserialize_config<T: serde::de::DeserializeOwned>(content: &str) -> Result<T, toml::de::Error> {
    let (config, unknown) = deserialize_config_collecting(content)?;
    for path in &unknown {
        warn!("unknown config field '{path}'");
    }
    Ok(config)
}

/// Load the initial configuration from a TOML file and return a `ConfigHandle`.
pub fn load_config(path: impl AsRef<Path>) -> Result<ConfigHandle, sag_core::AppError> {
    let path = path.as_ref().to_path_buf();
    let content = std::fs::read_to_string(&path).map_err(|e| {
        sag_core::AppError::Config(format!("failed to read config {}: {e}", path.display()))
    })?;

    let config: GatewayConfig = deserialize_config(&content)
        .map_err(|e| sag_core::AppError::Config(format!("failed to parse config: {e}")))?;

    config
        .validate()
        .map_err(|errors| sag_core::AppError::Config(errors.join("; ")))?;

    debug!(?config, "configuration loaded");
    Ok(ConfigHandle {
        inner: Arc::new(RwLock::new(config)),
        config_path: Some(Arc::new(path)),
        routes_version: Arc::new(AtomicU64::new(1)),
    })
}

/// Reload configuration from disk (async variant used by the hot-reload watcher).
pub async fn reload_config(path: &std::path::Path) -> Result<GatewayConfig, sag_core::AppError> {
    let content = tokio::fs::read_to_string(path).await.map_err(|e| {
        sag_core::AppError::Config(format!("failed to read config for reload: {e}"))
    })?;

    let config: GatewayConfig = deserialize_config(&content)
        .map_err(|e| sag_core::AppError::Config(format!("failed to parse config: {e}")))?;

    config
        .validate()
        .map_err(|errors| sag_core::AppError::Config(errors.join("; ")))?;

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_toml(content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let id = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let file_path = dir.join(format!("sag-load-test-{}-{}.toml", std::process::id(), id));
        let mut file = std::fs::File::create(&file_path).unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file_path
    }

    fn valid_toml() -> &'static str {
        r#"[server]
host = "127.0.0.1"
port = 9090

[gateway_auth]
api_keys = ["test-key"]

[providers.openai]
api_keys = ["sk-test"]
"#
    }

    #[test]
    fn load_config_success() {
        let path = temp_toml(valid_toml());
        let handle = load_config(&path).unwrap();
        assert_eq!(handle.path(), Some(path.as_path()));

        let config = handle.inner.try_read().unwrap();
        assert_eq!(config.server.port, 9090);
        assert_eq!(
            config.gateway_auth.as_ref().unwrap().api_keys,
            vec!["test-key"]
        );
    }

    #[test]
    fn load_config_missing_file() {
        let result = load_config("/tmp/does-not-exist-sag-test.toml");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("failed to read config"));
    }

    #[test]
    fn load_config_invalid_toml() {
        let path = temp_toml("this is not valid toml {{{ ");
        let result = load_config(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("failed to parse config"));
    }

    // ── peek_logging_config: format enum strictness ──
    //
    // Missing key / missing [logging] → default Compact (serde `#[default]`
    // serves absence only). Present-but-unknown format value → hard reject,
    // matching the main loader's typed-enum policy. Unreadable file / invalid
    // TOML still return Ok defaults for bootstrap resilience.

    #[test]
    fn peek_missing_format_defaults_to_compact() {
        let path = temp_toml("[logging]\nlevel = \"debug\"\n");
        let (level, format) = peek_logging_config(&path).unwrap();
        assert_eq!(level, "debug");
        assert_eq!(format, LogFormat::default());
    }

    #[test]
    fn peek_valid_format_values_parse() {
        for (value, expected) in [("compact", LogFormat::Compact), ("json", LogFormat::Json)] {
            let path = temp_toml(&format!("[logging]\nformat = \"{value}\"\n"));
            let (level, format) = peek_logging_config(&path).unwrap();
            assert_eq!(format, expected, "format = \"{value}\"");
            assert_eq!(level, "info");
        }
    }

    #[test]
    fn peek_unknown_format_values_hard_reject() {
        for bad in [
            r#""pretty""#,
            r#""JSON""#,
            r#""""#,
            "42",
            "true",
            // Non-string illegal types: array and table.
            r#"["json"]"#,
            "{ a = 1 }",
        ] {
            let path = temp_toml(&format!("[logging]\nformat = {bad}\n"));
            let err = match peek_logging_config(&path) {
                Err(e) => e,
                Ok(v) => panic!("format = {bad} must hard-reject, got: {v:?}"),
            };
            assert!(
                matches!(err, sag_core::AppError::Config(_)),
                "format = {bad} must be AppError::Config, got: {err:?}"
            );
            if bad == r#""pretty""# {
                assert!(
                    err.to_string().contains("logging.format"),
                    "pretty error must mention logging.format, got: {err}"
                );
            }
        }
    }

    #[test]
    fn peek_no_logging_section_returns_ok_default() {
        let path = temp_toml("[server]\nport = 9090\n");
        let (level, format) = peek_logging_config(&path).unwrap();
        assert_eq!(level, "info");
        assert_eq!(format, LogFormat::default());
    }

    #[test]
    fn peek_unreadable_file_returns_ok_default() {
        let (level, format) = peek_logging_config("/tmp/does-not-exist-sag-peek.toml").unwrap();
        assert_eq!(level, "info");
        assert_eq!(format, LogFormat::default());
    }

    #[test]
    fn peek_invalid_toml_returns_ok_default() {
        let path = temp_toml("this is not valid toml {{{ ");
        let (level, format) = peek_logging_config(&path).unwrap();
        assert_eq!(level, "info");
        assert_eq!(format, LogFormat::default());
    }

    #[test]
    fn load_config_missing_providers() {
        let path = temp_toml("[server]\nhost = \"127.0.0.1\"\nport = 8080\n");
        let result = load_config(&path);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        // When providers are missing, serde fails because the field is required
        assert!(
            err.contains("failed to parse config") || err.contains("missing field"),
            "expected parse error for missing providers, got: {err}"
        );
    }

    // ── unknown-field warning tests (serde_ignored based) ──
    //
    // `deserialize_config_collecting` returns the typed config plus the dotted
    // paths of every unrecognised field. Unknown fields are warned, never
    // rejected; dynamic map keys are consumed by the map types and never
    // reported. Paths follow `serde_ignored`'s format: `server.bind`,
    // `providers.openai.max_tokens`, `routes.0.endpoint`, `routes.0.targets.0.model`.

    /// Parse `content` as `GatewayConfig` and return the collected unknown paths.
    fn collect_unknown(content: &str) -> Vec<String> {
        let (_config, unknown) = deserialize_config_collecting::<GatewayConfig>(content).unwrap();
        unknown
    }

    #[test]
    fn unknown_field_detects_top_level_typos() {
        let unknown = collect_unknown(
            r#"
host = "127.0.0.1"
serverr = "oops"
[server]
port = 8080
[providers]
"#,
        );
        assert!(unknown.contains(&"serverr".to_string()), "got: {unknown:?}");
        assert!(unknown.contains(&"host".to_string()), "got: {unknown:?}");
    }

    #[test]
    fn unknown_field_detects_unknown_server_keys() {
        let unknown = collect_unknown(
            r#"
[server]
host = "127.0.0.1"
port = 8080
bind = "0.0.0.0"
[providers]
"#,
        );
        assert!(
            unknown.contains(&"server.bind".to_string()),
            "got: {unknown:?}"
        );
    }

    #[test]
    fn unknown_field_detects_unknown_provider_keys() {
        let unknown = collect_unknown(
            r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.openai]
api_keys = ["sk-test"]
max_tokens = 4096
"#,
        );
        assert!(
            unknown.contains(&"providers.openai.max_tokens".to_string()),
            "got: {unknown:?}"
        );
    }

    #[test]
    fn unknown_field_detects_unknown_route_keys() {
        let unknown = collect_unknown(
            r#"
[server]
host = "127.0.0.1"
port = 8080

[providers]

[[routes]]
model_name = "gpt-4o"
endpoint = "/v1/chat"
targets = [
    { provider = "openai" },
]
"#,
        );
        assert!(
            unknown.contains(&"routes.0.endpoint".to_string()),
            "got: {unknown:?}"
        );
    }

    #[test]
    fn unknown_field_detects_unknown_target_keys() {
        let unknown = collect_unknown(
            r#"
[server]
host = "127.0.0.1"
port = 8080

[providers]

[[routes]]
model_name = "gpt-4o"
targets = [
    { provider = "openai", model = "gpt-4" },
]
"#,
        );
        assert!(
            unknown.contains(&"routes.0.targets.0.model".to_string()),
            "got: {unknown:?}"
        );
    }

    #[test]
    fn unknown_field_ignores_known_keys() {
        assert_eq!(collect_unknown(valid_toml()), Vec::<String>::new());
    }

    #[test]
    fn unknown_field_ignores_known_backoff_keys() {
        let unknown = collect_unknown(
            r#"
[server]
host = "127.0.0.1"
port = 8080

[providers]

[backoff]
retries = 3
base_ms = 100
max_ms = 5000
jitter = true
retry_on_empty_response = false
"#,
        );
        assert_eq!(unknown, Vec::<String>::new());
    }

    #[test]
    fn unknown_field_flags_top_level_retries() {
        // `retries` was moved under [backoff]; a stray top-level `retries`
        // (before any [table] header) must be flagged.
        let unknown = collect_unknown(
            r#"
retries = 5
[server]
host = "127.0.0.1"
port = 8080
[providers]
"#,
        );
        assert!(unknown.contains(&"retries".to_string()), "got: {unknown:?}");
    }

    /// Dynamic maps must never be flagged: provider names, header names,
    /// body/model override keys, role mappings and per-target `extra` keys
    /// are all legitimately arbitrary.
    #[test]
    fn unknown_field_ignores_dynamic_map_keys() {
        let unknown = collect_unknown(
            r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.my-custom-provider-1]
api_keys = ["sk-1"]

[providers.anthropic]
api_keys = ["sk-ant"]

[providers.anthropic.headers]
X-Arbitrary-Header = "value"
Authorization = "Bearer x"

[providers.anthropic.body_overrides]
max_tokens = 4096
some_custom_field = { nested = true }

[providers.anthropic.model_overrides."gpt-4o"]
temperature = 0.8
any_weird_key = 42

[providers.anthropic.role_mappings]
developer = "system"

[providers.anthropic.model_strip_fields]
"gpt-4o" = ["chat_template_kwargs"]

[[routes]]
model_name = "gpt-4o"
targets = [
    { provider = "anthropic", extra = { temperature = 0.7, any_custom_flag = true } },
]
"#,
        );
        assert_eq!(
            unknown,
            Vec::<String>::new(),
            "dynamic map keys must not be flagged, got: {unknown:?}"
        );
    }

    /// `load_config` must succeed (warn, not deny) when the file contains
    /// unknown fields, and unknown fields must be ignored by serde while
    /// known ones still parse.
    #[test]
    fn load_config_does_not_deny_unknown_fields() {
        let path = temp_toml(
            r#"
[server]
host = "127.0.0.1"
port = 9090
bind = "0.0.0.0"

[gateway_auth]
api_keys = ["test-key"]

[providers.openai]
api_keys = ["sk-test"]
max_tokens = 4096
"#,
        );
        let handle = load_config(&path).unwrap();
        let config = handle.inner.try_read().unwrap();
        assert_eq!(config.server.port, 9090);
        assert_eq!(config.providers.len(), 1);
    }

    // ── warning emission ──

    /// A minimal `tracing::Subscriber` that records the `message` of every
    /// event. Used to assert that unknown fields emit a `warn!` (warn, never
    /// deny). A hand-rolled subscriber is deterministic under the parallel
    /// test harness (the fmt-subscriber writer path is not), and it needs no
    /// extra dev-dependency.
    #[derive(Clone, Default)]
    struct RecordingSubscriber {
        events: Arc<Mutex<Vec<String>>>,
    }

    impl tracing::Subscriber for RecordingSubscriber {
        fn enabled(&self, _meta: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(0xDEAD_BEEF)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Message(String);
            impl tracing::field::Visit for Message {
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    if field.name() == "message" {
                        self.0 = value.to_string();
                    }
                }
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    if field.name() == "message" {
                        self.0 = format!("{value:?}");
                    }
                }
            }
            let mut message = Message(String::new());
            event.record(&mut message);
            self.events.lock().unwrap().push(message.0);
        }
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    /// `deserialize_config` / `load_config` must emit a `warn!` for every
    /// unknown field and still succeed (warn, never deny). The subscriber is
    /// installed with `set_global_default` (once per process) rather than
    /// `with_default`: `with_default` bumps a process-global scoped-dispatch
    /// counter that races across threads in the parallel test harness,
    /// intermittently dropping events; the global fast-path is deterministic.
    #[test]
    fn unknown_fields_emit_warn() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let subscriber = RecordingSubscriber {
            events: events.clone(),
        };
        // Err only if a global default was already installed; that is fine.
        let _ = tracing::subscriber::set_global_default(subscriber);
        events.lock().unwrap().clear();

        // `deserialize_config` path (shared by load_config and reload_config).
        let content = r#"
[server]
host = "127.0.0.1"
port = 8080
bind = "0.0.0.0"

[providers.openai]
api_keys = ["sk-test"]
max_tokens = 4096
"#;
        deserialize_config::<GatewayConfig>(content).expect("parse should succeed");

        // `load_config` (initial-load) path.
        let path = temp_toml(
            r#"
[server]
host = "127.0.0.1"
port = 9090
bind = "0.0.0.0"

[providers.openai]
api_keys = ["sk-test"]
max_tokens = 4096
"#,
        );
        load_config(&path).expect("unknown fields must not deny loading");

        let recorded = events.lock().unwrap().clone();
        assert!(
            recorded
                .iter()
                .any(|e| e.contains("unknown config field 'server.bind'")),
            "expected server.bind warning, got: {recorded:?}"
        );
        assert!(
            recorded
                .iter()
                .any(|e| e.contains("unknown config field 'providers.openai.max_tokens'")),
            "expected provider warning, got: {recorded:?}"
        );
    }

    /// `reload_config` (hot-reload path) shares `deserialize_config`, so it
    /// must also tolerate unknown fields (warn, never deny — no panic, load
    /// succeeds), keeping initial-load and reload behavior consistent.
    #[tokio::test]
    async fn reload_config_handles_unknown_fields() {
        let path = temp_toml(
            r#"
[server]
host = "127.0.0.1"
port = 9090
bind = "0.0.0.0"

[providers.openai]
api_keys = ["sk-test"]
max_tokens = 4096
"#,
        );
        let config = reload_config(&path).await.expect("reload should succeed");
        assert_eq!(config.server.port, 9090);
        assert_eq!(config.providers.len(), 1);
    }
}
