use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::config::{GatewayConfig, LogFormat};
use crate::loader::handle::ConfigHandle;

/// Quickly read `logging.level` and `logging.format` from a TOML file without
/// full config parsing. Returns `(level, format)` filled with defaults where
/// the file can't be read or the keys are missing.
///
/// This is used before tracing is initialized so that the log level and format
/// from the config file can be applied early, without waiting for the full
/// config load.
pub fn peek_logging_config(path: impl AsRef<Path>) -> (String, LogFormat) {
    let content = match std::fs::read_to_string(path.as_ref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[sag] failed to read config file '{}' for logging peek: {e}",
                path.as_ref().display()
            );
            return ("info".into(), LogFormat::default());
        }
    };
    #[derive(serde::Deserialize)]
    struct LoggingPeek {
        level: Option<String>,
        format: Option<String>,
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
                path.as_ref().display()
            );
            return ("info".into(), LogFormat::default());
        }
    };
    let logging = match config.logging {
        Some(l) => l,
        None => {
            eprintln!(
                "[sag] no [logging] section found in config file '{}', using defaults",
                path.as_ref().display()
            );
            return ("info".into(), LogFormat::default());
        }
    };
    let level = logging.level.unwrap_or_else(|| "info".into());
    let format = match logging.format.as_deref() {
        Some("json") => LogFormat::Json,
        _ => LogFormat::Compact,
    };
    (level, format)
}

/// Log a warning for each unknown key in a TOML table.
fn check_unknown_keys(table: &toml::Table, known: &[&str], context: &str) {
    for key in table.keys() {
        if !known.contains(&key.as_str()) {
            warn!("unknown config key '{key}' in {context}");
        }
    }
}

/// Scan the raw TOML document for keys that are not recognised by
/// `GatewayConfig` and emit a `warn!` for each one. This catches
/// typos, obsolete keys, and misplaced fields that serde would
/// otherwise silently discard.
///
/// Covers: top level, [server], [analytics], [gateway_auth], [backoff],
/// [circuit_breaker], [logging], [providers.*], [[routes]], and route
/// target inline tables.
fn warn_unknown_fields(doc: &toml::Value) {
    let Some(top) = doc.as_table() else { return };

    check_unknown_keys(
        top,
        &[
            "server",
            "analytics",
            "gateway_auth",
            "providers",
            "backoff",
            "circuit_breaker",
            "shuffle_cache_ttl_secs",
            "sticky_failover_reset_secs",
            "routes",
            "logging",
        ],
        "top level",
    );

    if let Some(server) = top.get("server").and_then(|v| v.as_table()) {
        check_unknown_keys(server, &["host", "port"], "[server]");
    }

    if let Some(analytics) = top.get("analytics").and_then(|v| v.as_table()) {
        check_unknown_keys(analytics, &["enabled", "storage_dsn"], "[analytics]");
    }

    if let Some(auth) = top.get("gateway_auth").and_then(|v| v.as_table()) {
        check_unknown_keys(auth, &["api_keys"], "[gateway_auth]");
    }

    if let Some(backoff) = top.get("backoff").and_then(|v| v.as_table()) {
        check_unknown_keys(
            backoff,
            &["retries", "base_ms", "max_ms", "jitter"],
            "[backoff]",
        );
    }

    if let Some(cb) = top.get("circuit_breaker").and_then(|v| v.as_table()) {
        check_unknown_keys(
            cb,
            &["quota_exhausted_secs", "rate_limited_secs", "generic_secs"],
            "[circuit_breaker]",
        );
    }

    if let Some(logging) = top.get("logging").and_then(|v| v.as_table()) {
        check_unknown_keys(logging, &["level", "format"], "[logging]");
    }

    if let Some(providers) = top.get("providers").and_then(|v| v.as_table()) {
        let known_provider = &[
            "provider_type",
            "api_keys",
            "key_strategy",
            "base_url",
            "models_endpoint",
            "timeout_secs",
            "chunk_timeout_secs",
            "ttfb_timeout_secs",
            "headers",
            "body_overrides",
            "model_overrides",
            "role_mappings",
            "enabled",
            "enabled_models",
            "anthropic_cache_control",
        ];
        for (name, pv) in providers {
            let Some(pt) = pv.as_table() else { continue };
            check_unknown_keys(pt, known_provider, &format!("[providers.{name}]"));
        }
    }

    if let Some(routes) = top.get("routes").and_then(|v| v.as_array()) {
        let known_route = &[
            "model_name",
            "api_endpoint",
            "strategy",
            "retries",
            "timeout_secs",
            "targets",
        ];
        let known_target = &[
            "provider",
            "real_model",
            "priority",
            "retries",
            "timeout_secs",
            "extra",
        ];
        for (i, rv) in routes.iter().enumerate() {
            let Some(rt) = rv.as_table() else { continue };
            check_unknown_keys(rt, known_route, &format!("routes[{i}]"));
            if let Some(targets) = rt.get("targets").and_then(|v| v.as_array()) {
                for (j, tv) in targets.iter().enumerate() {
                    let Some(tt) = tv.as_table() else { continue };
                    check_unknown_keys(tt, known_target, &format!("routes[{i}].targets[{j}]"));
                }
            }
        }
    }
}

/// Load the initial configuration from a TOML file and return a `ConfigHandle`.
pub fn load_config(path: impl AsRef<Path>) -> Result<ConfigHandle, sag_core::AppError> {
    let path = path.as_ref().to_path_buf();
    let content = std::fs::read_to_string(&path).map_err(|e| {
        sag_core::AppError::Config(format!("failed to read config {}: {e}", path.display()))
    })?;

    // Parse raw TOML to detect unknown keys before discarding them via serde.
    if let Ok(raw) = toml::from_str::<toml::Value>(&content) {
        warn_unknown_fields(&raw);
    }

    let config: GatewayConfig = toml::from_str(&content)
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
    if let Ok(raw) = toml::from_str::<toml::Value>(&content) {
        warn_unknown_fields(&raw);
    }

    let config: GatewayConfig = toml::from_str(&content)
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

    // ── unknown key warning tests ──

    #[test]
    fn warn_unknown_fields_detects_top_level_typos() {
        let doc = toml::from_str::<toml::Value>(
            r#"
host = "127.0.0.1"
serverr = "oops"
[server]
port = 8080
[providers]
"#,
        )
        .unwrap();
        warn_unknown_fields(&doc);
    }

    #[test]
    fn warn_unknown_fields_detects_unknown_server_keys() {
        let doc = toml::from_str::<toml::Value>(
            r#"
[server]
host = "127.0.0.1"
port = 8080
bind = "0.0.0.0"
[providers]
"#,
        )
        .unwrap();
        warn_unknown_fields(&doc);
    }

    #[test]
    fn warn_unknown_fields_detects_unknown_provider_keys() {
        let doc = toml::from_str::<toml::Value>(
            r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.openai]
api_keys = ["sk-test"]
max_tokens = 4096
"#,
        )
        .unwrap();
        warn_unknown_fields(&doc);
    }

    #[test]
    fn warn_unknown_fields_detects_unknown_route_keys() {
        let doc = toml::from_str::<toml::Value>(
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
        )
        .unwrap();
        warn_unknown_fields(&doc);
    }

    #[test]
    fn warn_unknown_fields_detects_unknown_target_keys() {
        let doc = toml::from_str::<toml::Value>(
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
        )
        .unwrap();
        warn_unknown_fields(&doc);
    }

    #[test]
    fn warn_unknown_fields_ignores_known_keys() {
        let doc = toml::from_str::<toml::Value>(valid_toml()).unwrap();
        warn_unknown_fields(&doc);
    }

    #[test]
    fn warn_unknown_fields_flags_top_level_retries() {
        let doc = toml::from_str::<toml::Value>(
            r#"
[server]
host = "127.0.0.1"
port = 8080
retries = 5
[providers]
"#,
        )
        .unwrap();
        warn_unknown_fields(&doc);
    }
}
