use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// Root configuration for the SAG (Smart AI Gateway).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub server: ServerConfig,
    pub analytics: Option<AnalyticsConfig>,
    pub gateway_auth: Option<GatewayAuthConfig>,
    pub providers: std::collections::HashMap<String, ProviderConfig>,
    /// Global default for per-target retries (can be overridden per-route or per-target).
    /// `1` → one attempt (no retries). `2` → one initial + one retry, etc.
    #[serde(default = "default_retries")]
    pub retries: u32,
    /// Global exponential-backoff settings for retry delays.
    #[serde(default)]
    pub backoff: BackoffConfig,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
    /// Logging configuration (optional, default log level: "info").
    /// The `RUST_LOG` environment variable, if set, takes precedence.
    #[serde(default)]
    pub logging: LoggingConfig,
}

impl GatewayConfig {
    /// Validate the configuration, returning all semantic errors found.
    /// Returns `Ok(())` if no errors, `Err(Vec<String>)` with all error messages.
    pub fn validate(&self) -> std::result::Result<(), Vec<String>> {
        let mut errors: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        // ── Server validation ──
        if self.server.host.trim().is_empty() {
            errors.push("server.host must not be empty".into());
        }
        if self.server.port == 0 {
            errors.push("server.port must be greater than 0".into());
        }

        // ── Analytics validation ──
        if let Some(ref analytics) = self.analytics
            && analytics.enabled
            && !analytics.storage_dsn.starts_with("sqlite://")
            && analytics.storage_dsn != ":memory:"
        {
            errors
                .push("analytics.storage_dsn must start with 'sqlite://' or be ':memory:'".into());
        }

        // ── Providers validation ──
        let provider_names: HashSet<&str> = self.providers.keys().map(String::as_str).collect();

        if self.providers.is_empty() {
            errors.push("at least one provider is required".into());
        }
        if self.retries > 10 {
            warnings.push(format!(
                "global retries={} seems unusually high",
                self.retries
            ));
        }

        // ── Backoff validation ──
        if self.backoff.base_ms == 0 {
            errors.push("backoff.base_ms must be greater than 0".into());
        }
        if self.backoff.max_ms < self.backoff.base_ms {
            errors.push(format!(
                "backoff.max_ms ({}) must be >= backoff.base_ms ({})",
                self.backoff.max_ms, self.backoff.base_ms
            ));
        }

        for (name, provider) in &self.providers {
            if provider.api_keys.is_empty() {
                errors.push(format!("provider '{name}': api_keys must not be empty"));
            }
            if !provider.base_url.contains("://") {
                errors.push(format!(
                    "provider '{name}': base_url '{}' is not a valid URL (missing scheme)",
                    provider.base_url
                ));
            }
            if provider.models_endpoint.contains("://")
                || provider.models_endpoint.contains('?')
                || provider.models_endpoint.contains('#')
                || provider
                    .models_endpoint
                    .split('/')
                    .any(|segment| segment.contains(char::is_whitespace))
            {
                errors.push(format!(
                    "provider '{name}': models_endpoint '{}' must be a relative path without scheme, query, fragment, or whitespace",
                    provider.models_endpoint
                ));
            }
            if provider.timeout_secs == 0 {
                errors.push(format!(
                    "provider '{name}': timeout_secs must be greater than 0"
                ));
            }
            if provider.key_strategy == KeyStrategy::RoundRobin && provider.api_keys.len() == 1 {
                warnings.push(format!(
                    "provider '{name}': RoundRobin strategy with a single API key behaves the same as FillFirst"
                ));
            }
        }

        // ── Routes validation ──
        let mut seen_model_names: HashSet<&str> = HashSet::new();
        let mut wildcard_index: Option<usize> = None;

        for (i, route) in self.routes.iter().enumerate() {
            if route.model_name.trim().is_empty() {
                errors.push(format!("routes[{i}]: model_name must not be empty"));
            } else {
                if !seen_model_names.insert(route.model_name.as_str()) {
                    warnings.push(format!(
                        "routes[{i}]: duplicate model_name '{}'",
                        route.model_name
                    ));
                }
                if route.model_name == "*" {
                    wildcard_index = Some(i);
                }
            }

            if !route.api_endpoint.starts_with('/') {
                errors.push(format!(
                    "routes[{i}] (model='{}'): api_endpoint must start with '/'",
                    route.model_name
                ));
            }

            if let Some(retries) = route.retries
                && retries > 10
            {
                warnings.push(format!(
                    "routes[{i}] (model='{}'): retries={retries} seems unusually high",
                    route.model_name
                ));
            }

            if route.targets.is_empty() {
                errors.push(format!(
                    "routes[{i}] (model='{}'): must have at least one target",
                    route.model_name
                ));
            }

            for (j, target) in route.targets.iter().enumerate() {
                if !provider_names.contains(target.provider.as_str()) {
                    errors.push(format!(
                        "routes[{i}] (model='{}'): target[{j}] references unknown provider '{}'",
                        route.model_name, target.provider
                    ));
                }
                if let Some(retries) = target.retries
                    && retries > 10
                {
                    warnings.push(format!(
                        "routes[{i}] (model='{}'): target[{j}] retries={retries} seems unusually high",
                        route.model_name
                    ));
                }
            }

            // ── Composite (provider, real_model) uniqueness per route ──
            // The save path uses `(provider, real_model)` as the
            // composite key for `sync_inline_table_array_in_place`
            // (two targets may legitimately share a `provider` as long
            // as their `real_model`s differ). Forbidding the conflicting
            // case — same `provider` AND same `real_model` — here keeps
            // the on-disk ↔ in-memory mapping unambiguous; without
            // this check a config with two identical targets would
            // silently collapse to one on save.
            let mut seen_targets: HashSet<(&str, Option<&str>)> = HashSet::new();
            for (j, target) in route.targets.iter().enumerate() {
                let key: (&str, Option<&str>) =
                    (target.provider.as_str(), target.real_model.as_deref());
                if !seen_targets.insert(key) {
                    let real_model_desc = match &target.real_model {
                        Some(rm) => format!("real_model='{rm}'"),
                        None => "real_model=omitted".to_string(),
                    };
                    errors.push(format!(
                        "routes[{i}] (model='{}'): duplicates (provider, real_model) \
                         for target[{j}] — another target in this route already has \
                         provider='{}' and {real_model_desc}; the save path uses this \
                         pair as the sync key and would silently overwrite one of the two",
                        route.model_name, target.provider
                    ));
                }
            }
        }

        if let Some(idx) = wildcard_index
            && idx != self.routes.len() - 1
        {
            warnings.push(format!(
                "routes[{idx}] (model='*'): wildcard route should be the last route",
            ));
        }

        // Log warnings
        for w in &warnings {
            tracing::warn!("configuration warning: {w}");
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Log output format.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    /// Human-readable compact format (default).
    #[default]
    Compact,
    /// Structured JSON format for log aggregators.
    Json,
}

/// Logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level filter string (e.g., "info", "debug", "trace", "warn", "error").
    /// Also supports `EnvFilter` directives like `"info,sag_router=debug"`.
    /// The `RUST_LOG` environment variable, if set, takes precedence over this value.
    #[serde(default = "default_log_level")]
    pub level: String,
    /// Output format: `"compact"` (human-readable) or `"json"` (structured logging).
    /// Defaults to `"compact"`.
    #[serde(default)]
    pub format: LogFormat,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: LogFormat::default(),
        }
    }
}

fn default_log_level() -> String {
    "info".into()
}

/// Server listen configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_host() -> String {
    "0.0.0.0".into()
}
fn default_port() -> u16 {
    3000
}

/// Analytics storage configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyticsConfig {
    pub enabled: bool,
    #[serde(default = "default_storage_dsn")]
    pub storage_dsn: String,
}

fn default_storage_dsn() -> String {
    "sqlite://analytics.db".into()
}

/// Gateway client authentication configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayAuthConfig {
    /// List of valid API keys for clients calling this gateway.
    pub api_keys: Vec<String>,
}

/// Supported LLM provider adapter type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderType {
    /// OpenAI-compatible API (also used for any OpenAI-drop-in replacement).
    #[default]
    #[serde(rename = "openai")]
    OpenAI,
    /// Anthropic Messages API.
    Anthropic,
}

impl ProviderType {
    /// Return the canonical lowercase string for this provider type.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderType::OpenAI => "openai",
            ProviderType::Anthropic => "anthropic",
        }
    }
}

/// API key selection strategy for providers with multiple keys.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyStrategy {
    /// Always use the first available key.
    /// On request failure, fall back to the next key in the list.
    #[default]
    FillFirst,
    /// Distribute requests across all keys in round-robin order.
    RoundRobin,
}

/// Upstream LLM provider configuration.
///
/// Provider-level priority is not part of routing selection. Target ordering is
/// controlled by `RouteTarget.priority` inside each route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Provider adapter type (default: `openai`).
    #[serde(default, skip_serializing_if = "is_default_provider_type")]
    pub provider_type: ProviderType,
    /// API keys for this provider.
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// How to select among multiple `api_keys`.
    #[serde(default, skip_serializing_if = "is_default_key_strategy")]
    pub key_strategy: KeyStrategy,
    /// Base URL for the provider's API.
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// Upstream endpoint path used by admin APIs to fetch available models.
    /// Defaults to `models`, appended to `base_url`.
    #[serde(default = "default_models_endpoint")]
    pub models_endpoint: String,
    /// Timeout in seconds for requests to this provider.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Custom headers to add to or override in upstream requests.
    /// These are applied after the provider's default headers,
    /// so they can override auth, content-type, etc.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    /// Request body field overrides merged into the upstream JSON body.
    /// Key-value pairs are inserted at the top level of the body,
    /// allowing you to set or override any field (e.g. max_tokens, temperature,
    /// or provider-specific parameters).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub body_overrides: HashMap<String, serde_json::Value>,
    /// Whether this provider is enabled. When `false`, the provider is skipped
    /// during routing and shown as disabled in the admin UI.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// List of model IDs enabled for this provider.
    /// When non-empty, only these models may be used with this provider.
    /// When empty (default), all models are allowed (backward compatible).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_models: Vec<String>,
}

pub fn default_base_url() -> String {
    "https://api.openai.com/v1".into()
}
pub fn default_models_endpoint() -> String {
    "models".into()
}
pub fn normalize_models_endpoint(endpoint: &str) -> String {
    let trimmed = endpoint.trim().trim_matches('/');
    if trimmed.is_empty() {
        default_models_endpoint()
    } else {
        trimmed.to_string()
    }
}
pub fn default_retries() -> u32 {
    2
}
pub fn default_timeout_secs() -> u64 {
    120
}

/// Exponential-backoff settings for retry delays between attempts.
///
/// Formula: `min(base_ms * 2^attempt, max_ms)`, with optional full jitter
/// (`delay = random(0, computed)`) to avoid thundering-herd retries across
/// many concurrent clients. `attempt = 0` returns the base delay; `attempt = 1`
/// doubles it; etc.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackoffConfig {
    /// Initial backoff delay in milliseconds (corresponds to attempt 0).
    #[serde(default = "default_backoff_base_ms")]
    pub base_ms: u64,
    /// Maximum backoff delay cap in milliseconds. Prevents unbounded waits
    /// for high `retries` values.
    #[serde(default = "default_backoff_max_ms")]
    pub max_ms: u64,
    /// Whether to apply full jitter to each delay. When `true`, the actual
    /// sleep is `random(0, computed_cap)`; when `false`, the deterministic
    /// cap is used. Defaults to `true` (recommended for production).
    #[serde(default = "default_backoff_jitter")]
    pub jitter: bool,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            base_ms: default_backoff_base_ms(),
            max_ms: default_backoff_max_ms(),
            jitter: default_backoff_jitter(),
        }
    }
}

pub fn default_backoff_base_ms() -> u64 {
    100
}
pub fn default_backoff_max_ms() -> u64 {
    5_000
}
pub fn default_backoff_jitter() -> bool {
    true
}
fn default_true() -> bool {
    true
}
fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}
fn is_default_provider_type(v: &ProviderType) -> bool {
    *v == ProviderType::default()
}
fn is_default_key_strategy(v: &KeyStrategy) -> bool {
    *v == KeyStrategy::default()
}
fn is_default_strategy(v: &Strategy) -> bool {
    *v == Strategy::default()
}
fn is_default_api_endpoint(v: &str) -> bool {
    v == default_api_endpoint()
}

/// Target selection strategy for a route.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// Try targets in priority order; within the same priority, pick randomly.
    /// On failure, fall back to the next priority tier.
    #[default]
    FillFirst,
    /// Distribute requests across all targets in round-robin order.
    /// On failure, try the next target in sequence.
    RoundRobin,
}

/// Per-model routing configuration.
/// Maps a requested model name to one or more provider targets.
///
/// When a request matches this route, targets are selected based on the route's
/// `strategy`:
/// - `FillFirst`: sorted by priority (descending); same-priority targets are
///   tried in random order. Higher-priority targets are tried first.
/// - `RoundRobin`: targets are tried in a rotating order. Each request advances
///   the counter, distributing load evenly.
///
/// ## Retries
///
/// The effective number of retries for each target is resolved via three-tier
/// fallback: **target-level** → **route-level** → **global** (`GatewayConfig.retries`).
/// The first tier that explicitly sets a value wins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteConfig {
    /// The model name as requested by the client.
    /// Use `"*"` as a wildcard to match any model not matched by an exact route.
    pub model_name: String,
    /// Upstream API endpoint path for this model.
    /// Examples: `/chat/completions`, `/images/generations`, `/responses`.
    #[serde(
        default = "default_api_endpoint",
        skip_serializing_if = "is_default_api_endpoint"
    )]
    pub api_endpoint: String,
    /// Target selection strategy (default: `FillFirst`).
    #[serde(default, skip_serializing_if = "is_default_strategy")]
    pub strategy: Strategy,
    /// Per-target retry count override for this route.
    /// When `None` (default), falls back to the global `GatewayConfig.retries`.
    /// When `Some(n)`, each target in this route retries up to `n` times.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
    /// List of targets for this route.
    pub targets: Vec<RouteTarget>,
}

fn default_api_endpoint() -> String {
    "/chat/completions".into()
}

impl RouteConfig {
    /// Returns `true` if this route's `api_endpoint` is the built-in default
    /// (`"/chat/completions"`). Save logic uses this to mirror the
    /// `skip_serializing_if = "is_default_api_endpoint"` rule on
    /// [`RouteConfig::api_endpoint`]: the field is dropped from the file
    /// when the value is the default, and written otherwise. Calling
    /// `default_api_endpoint` directly would leak an implementation detail
    /// of "what counts as a default" into the public API; this method
    /// encapsulates that comparison.
    pub fn uses_default_api_endpoint(&self) -> bool {
        is_default_api_endpoint(&self.api_endpoint)
    }
}

/// A routing target: maps a logical model to a specific provider + real model name.
///
/// Multiple targets in a route are grouped by priority and tried based on the
/// route's `strategy`. Targets with the same priority are tried in random order
/// when strategy is `FillFirst`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteTarget {
    /// Provider name (must match a key in `providers`).
    pub provider: String,
    /// The real model name to send to this provider.
    /// If `None` (omitted), the original client-requested model name is used as-is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub real_model: Option<String>,
    /// Priority within this route — higher = higher priority. Targets are
    /// sorted in descending order, so a target with priority `20` is tried
    /// before one with priority `0`. Defaults to `0`.
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub priority: u32,
    /// Per-target retry override.
    /// When `None`, falls back to the route's `retries`, then to the global default.
    /// When `Some(n)`, this target retries up to `n` times regardless of route/global
    /// settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
    /// Per-target extra body fields merged into the upstream JSON body.
    /// Applied after provider-level body_overrides, so target-level
    /// keys take precedence.
    ///
    /// Serialized as a single nested inline table (`extra = { ... }`)
    /// within the target's inline table, e.g.
    /// `{ provider = "openai", extra = { temperature = 0.7 } }`. This
    /// keeps target-level extras clearly grouped, avoids namespace
    /// collisions with future `RouteTarget` fields, and round-trips with
    /// the on-disk key order preserved.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal valid TOML that exercises all config sections.
    fn minimal_toml() -> &'static str {
        r#"
[server]
host = "127.0.0.1"
port = 8080

[gateway_auth]
api_keys = ["key1", "key2"]

[analytics]
enabled = true

[providers.openai]
api_keys = ["sk-test"]

[[routes]]
model_name = "gpt-4o"
targets = [
    { provider = "openai", real_model = "gpt-4o" },
]
"#
    }

    #[test]
    fn parse_full_config() {
        let config: GatewayConfig = toml::from_str(minimal_toml()).unwrap();

        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 8080);

        let auth = config.gateway_auth.unwrap();
        assert_eq!(auth.api_keys, vec!["key1", "key2"]);

        let analytics = config.analytics.unwrap();
        assert!(analytics.enabled);
        assert_eq!(analytics.storage_dsn, "sqlite://analytics.db");

        let openai = config.providers.get("openai").unwrap();
        assert_eq!(openai.api_keys, vec!["sk-test"]);
        assert_eq!(openai.base_url, "https://api.openai.com/v1");
        assert_eq!(openai.models_endpoint, "models");
        assert_eq!(openai.timeout_secs, 120);
        assert_eq!(config.retries, 2);

        assert_eq!(config.routes.len(), 1);
        let route = &config.routes[0];
        assert_eq!(route.model_name, "gpt-4o");
        assert_eq!(route.targets.len(), 1);
        assert_eq!(route.targets[0].provider, "openai");
        assert_eq!(route.targets[0].real_model.as_deref(), Some("gpt-4o"));
        assert_eq!(route.targets[0].priority, 0);
    }

    #[test]
    fn parse_server_defaults() {
        let toml_str = r#"
[server]
[providers]
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.server.host, "0.0.0.0");
        assert_eq!(config.server.port, 3000);
    }

    #[test]
    fn parse_no_auth_no_analytics() {
        let toml_str = r#"
[server]
[providers]
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        assert!(config.gateway_auth.is_none());
        assert!(config.analytics.is_none());
    }

    #[test]
    fn parse_retries_defaults() {
        let toml_str = r#"
[server]
[providers]
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.retries, 2);
    }

    #[test]
    fn parse_backoff_defaults() {
        let toml_str = r#"
[server]
[providers]
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        // BackoffConfig::default() — matches the legacy constants so
        // existing deployments behave identically after upgrading.
        assert_eq!(config.backoff.base_ms, 100);
        assert_eq!(config.backoff.max_ms, 5_000);
        assert!(config.backoff.jitter);
    }

    #[test]
    fn parse_backoff_overrides() {
        let toml_str = r#"
[server]
[providers]

[backoff]
base_ms = 250
max_ms = 30_000
jitter = false
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.backoff.base_ms, 250);
        assert_eq!(config.backoff.max_ms, 30_000);
        assert!(!config.backoff.jitter);
    }

    #[test]
    fn parse_backoff_partial_uses_defaults_for_missing_fields() {
        let toml_str = r#"
[server]
[providers]

[backoff]
max_ms = 10_000
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.backoff.base_ms, 100, "default base_ms");
        assert_eq!(config.backoff.max_ms, 10_000, "overridden max_ms");
        assert!(config.backoff.jitter, "default jitter");
    }

    #[test]
    fn parse_retries_tiered_override() {
        let toml_str = r#"
retries = 5

[server]

[providers]

[[routes]]
model_name = "gpt-4o"
targets = [
    { provider = "openai" },
]

[[routes]]
model_name = "claude"
retries = 3
targets = [
    { provider = "anthropic", retries = 1 },
]
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.retries, 5);
        assert_eq!(config.routes[0].retries, None);
        assert_eq!(config.routes[0].targets[0].retries, None);
        assert_eq!(config.routes[1].retries, Some(3));
        assert_eq!(config.routes[1].targets[0].retries, Some(1));
    }

    #[test]
    fn parse_provider_with_custom_fields() {
        let toml_str = r#"
[server]

[providers.anthropic]
api_keys = ["sk-ant"]
base_url = "https://api.anthropic.com"
models_endpoint = "v1/models"
timeout_secs = 60

[providers.anthropic.headers]
Authorization = "Bearer custom-token"
X-Debug = "true"

[providers.anthropic.body_overrides]
max_tokens = 4096
temperature = 0.8
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        let p = config.providers.get("anthropic").unwrap();
        assert_eq!(p.api_keys, vec!["sk-ant"]);
        assert_eq!(p.base_url, "https://api.anthropic.com");
        assert_eq!(p.models_endpoint, "v1/models");
        assert_eq!(p.timeout_secs, 60);
        assert_eq!(
            p.headers.get("Authorization").unwrap(),
            "Bearer custom-token"
        );
        assert_eq!(p.headers.get("X-Debug").unwrap(), "true");
        assert_eq!(
            p.body_overrides.get("max_tokens").unwrap(),
            &serde_json::json!(4096)
        );
        assert_eq!(
            p.body_overrides.get("temperature").unwrap(),
            &serde_json::json!(0.8)
        );
    }

    #[test]
    fn normalize_models_endpoint_variants() {
        assert_eq!(normalize_models_endpoint("models"), "models");
        assert_eq!(normalize_models_endpoint("/models"), "models");
        assert_eq!(normalize_models_endpoint("v1/models/"), "v1/models");
        assert_eq!(normalize_models_endpoint("  /v1/models/  "), "v1/models");
        assert_eq!(normalize_models_endpoint(""), "models");
    }

    #[test]
    fn rejects_invalid_models_endpoint() {
        let toml_str = r#"
[server]

[providers.openai]
api_keys = ["sk-test"]
models_endpoint = "https://example.com/models"

[[routes]]
model_name = "gpt-4o"
targets = [
    { provider = "openai" },
]
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        let err = config.validate().unwrap_err();
        assert!(err.iter().any(|e| e.contains("models_endpoint")));
    }

    #[test]
    fn parse_multiple_routes_with_priorities() {
        let toml_str = r#"
[server]
[providers]

[[routes]]
model_name = "gpt-4o"
targets = [
    { provider = "openai", real_model = "gpt-4o", priority = 10 },
    { provider = "anthropic", real_model = "claude-opus", priority = 1 },
]

[[routes]]
model_name = "claude-sonnet"
targets = [
    { provider = "anthropic", real_model = "claude-sonnet", priority = 10 },
]
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.routes.len(), 2);
        assert_eq!(config.routes[0].model_name, "gpt-4o");
        assert_eq!(config.routes[0].targets.len(), 2);
        assert_eq!(config.routes[0].targets[0].priority, 10);
        assert_eq!(config.routes[0].targets[1].priority, 1);
        assert_eq!(config.routes[1].model_name, "claude-sonnet");
        assert_eq!(config.routes[1].targets.len(), 1);
    }

    #[test]
    fn parse_missing_providers_fails() {
        let toml_str = r#"
[server]
"#;
        let result: Result<GatewayConfig, _> = toml::from_str(toml_str);
        assert!(result.is_err());
    }

    #[test]
    fn parse_route_target_missing_real_model_defaults_to_passthrough() {
        let toml_str = r#"
[server]
[providers]

[[routes]]
model_name = "gpt-4"
targets = [
    { provider = "openai" },
]
"#;
        let result: Result<GatewayConfig, _> = toml::from_str(toml_str);
        assert!(result.is_ok(), "real_model should be optional");
        let config = result.unwrap();
        assert!(config.routes[0].targets[0].real_model.is_none());
    }

    mod validate_tests {
        use super::*;

        fn valid_config() -> GatewayConfig {
            toml::from_str(
                r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.openai]
api_keys = ["sk-test"]

[[routes]]
model_name = "gpt-4o"
targets = [
    { provider = "openai" },
]
"#,
            )
            .unwrap()
        }

        #[test]
        fn valid_config_passes() {
            let config = valid_config();
            assert!(config.validate().is_ok());
        }

        #[test]
        fn rejects_backoff_base_ms_zero() {
            let mut config = valid_config();
            config.backoff.base_ms = 0;
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("backoff.base_ms must be greater than 0")),
                "expected base_ms error, got {err:?}"
            );
        }

        #[test]
        fn rejects_backoff_max_ms_less_than_base_ms() {
            let mut config = valid_config();
            config.backoff.base_ms = 500;
            config.backoff.max_ms = 100;
            let err = config.validate().unwrap_err();
            assert!(
                err.iter().any(|e| e.contains("backoff.max_ms")),
                "expected max_ms error, got {err:?}"
            );
        }

        #[test]
        fn backoff_max_ms_equal_to_base_ms_is_allowed() {
            // Edge case: fixed delay (no exponential growth, no cap).
            let mut config = valid_config();
            config.backoff.base_ms = 200;
            config.backoff.max_ms = 200;
            assert!(config.validate().is_ok());
        }

        #[test]
        fn rejects_empty_api_keys() {
            let mut config = valid_config();
            config.providers.get_mut("openai").unwrap().api_keys.clear();
            let err = config.validate().unwrap_err();
            assert!(err.iter().any(|e| e.contains("api_keys must not be empty")));
        }

        #[test]
        fn rejects_unknown_provider_ref() {
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
[providers.openai]
api_keys = ["sk-test"]

[[routes]]
model_name = "gpt-4o"
targets = [
    { provider = "nonexistent" },
]
"#,
            )
            .unwrap();
            let err = config.validate().unwrap_err();
            assert!(err.iter().any(|e| e.contains("unknown provider")));
        }

        #[test]
        fn rejects_empty_host() {
            let mut config = valid_config();
            config.server.host = "".into();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("server.host must not be empty"))
            );
        }

        #[test]
        fn rejects_port_zero() {
            let mut config = valid_config();
            config.server.port = 0;
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("port must be greater than 0"))
            );
        }

        #[test]
        fn rejects_invalid_base_url() {
            let mut config = valid_config();
            config.providers.get_mut("openai").unwrap().base_url = "not-a-url".into();
            let err = config.validate().unwrap_err();
            assert!(err.iter().any(|e| e.contains("not a valid URL")));
        }

        #[test]
        fn rejects_timeout_zero() {
            let mut config = valid_config();
            config.providers.get_mut("openai").unwrap().timeout_secs = 0;
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("timeout_secs must be greater than 0"))
            );
        }

        #[test]
        fn rejects_empty_model_name() {
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
[providers.openai]
api_keys = ["sk-test"]

[[routes]]
model_name = ""
targets = [
    { provider = "openai" },
]
"#,
            )
            .unwrap();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("model_name must not be empty"))
            );
        }

        #[test]
        fn rejects_api_endpoint_no_slash() {
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
[providers.openai]
api_keys = ["sk-test"]

[[routes]]
model_name = "gpt-4o"
api_endpoint = "chat/completions"
targets = [
    { provider = "openai" },
]
"#,
            )
            .unwrap();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("api_endpoint must start with '/'"))
            );
        }

        #[test]
        fn rejects_analytics_invalid_dsn() {
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
[analytics]
enabled = true
storage_dsn = "postgres://localhost/db"
[providers.openai]
api_keys = ["sk-test"]
"#,
            )
            .unwrap();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("storage_dsn must start with 'sqlite://'"))
            );
        }

        #[test]
        fn rejects_empty_providers() {
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
[providers]
"#,
            )
            .unwrap();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("at least one provider is required"))
            );
        }

        #[test]
        fn rejects_no_targets() {
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
[providers.openai]
api_keys = ["sk-test"]

[[routes]]
model_name = "gpt-4o"
targets = []
"#,
            )
            .unwrap();
            let err = config.validate().unwrap_err();
            assert!(err.iter().any(|e| e.contains("at least one target")));
        }

        #[test]
        fn valid_config_with_memory_dsn() {
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
[analytics]
enabled = true
storage_dsn = ":memory:"
[providers.openai]
api_keys = ["sk-test"]
"#,
            )
            .unwrap();
            assert!(config.validate().is_ok());
        }

        #[test]
        fn rejects_all_errors_at_once() {
            let mut config = valid_config();
            config.server.host = "".into();
            config.server.port = 0;
            config.providers.get_mut("openai").unwrap().api_keys.clear();
            let err = config.validate().unwrap_err();
            // Should contain multiple errors
            assert!(err.len() >= 3, "expected at least 3 errors, got {err:?}");
        }

        #[test]
        fn uses_default_api_endpoint_matches_serde_skip_rule() {
            // The new predicate must agree with the same comparison that
            // serde uses (via `is_default_api_endpoint`) — otherwise the save
            // path could drop a non-default field or keep a default one.
            let mut route = RouteConfig {
                model_name: "gpt-4o".into(),
                api_endpoint: default_api_endpoint(),
                strategy: Strategy::FillFirst,
                retries: None,
                targets: vec![],
            };
            assert!(route.uses_default_api_endpoint());
            assert!(is_default_api_endpoint(&route.api_endpoint));

            route.api_endpoint = "/images/generations".into();
            assert!(!route.uses_default_api_endpoint());
            assert!(!is_default_api_endpoint(&route.api_endpoint));
        }

        #[test]
        fn rejects_duplicate_provider_and_real_model_within_route() {
            // Two targets in the same route sharing BOTH `provider` AND
            // `real_model` would silently collapse on save (the save
            // path's `(provider, real_model)` key map has only one slot
            // for the pair). `validate()` is the only consumer-visible
            // line of defense — reject it explicitly so the user sees a
            // clear error instead of a quiet data loss.
            let mut config = valid_config();
            config.routes[0].targets = vec![
                RouteTarget {
                    provider: "openai".into(),
                    real_model: Some("gpt-4".into()),
                    priority: 10,
                    retries: None,
                    extra: HashMap::new(),
                },
                RouteTarget {
                    provider: "openai".into(),
                    real_model: Some("gpt-4".into()),
                    priority: 5,
                    retries: None,
                    extra: HashMap::new(),
                },
            ];
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("duplicates (provider, real_model)")),
                "expected duplicate-key error, got {err:?}"
            );
        }

        #[test]
        fn accepts_same_provider_distinct_real_model_within_route() {
            // Sanity check: the why behind the composite-key refactor.
            // Two targets HAVE THE SAME provider but DIFFERENT
            // real_models must be legal (this is how multi-model
            // fallback through one provider works). The validator must
            // permit this and the save path must keep them distinct.
            let mut config = valid_config();
            config.routes[0].targets = vec![
                RouteTarget {
                    provider: "openai".into(),
                    real_model: Some("gpt-4".into()),
                    priority: 10,
                    retries: None,
                    extra: HashMap::new(),
                },
                RouteTarget {
                    provider: "openai".into(),
                    real_model: Some("gpt-3.5-turbo".into()),
                    priority: 5,
                    retries: None,
                    extra: HashMap::new(),
                },
            ];
            assert!(
                config.validate().is_ok(),
                "distinct real_models with the same provider must validate"
            );
        }
    }
}
