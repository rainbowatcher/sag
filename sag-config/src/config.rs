use std::collections::HashMap;

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
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Provider adapter type (default: `openai`).
    #[serde(default)]
    pub provider_type: ProviderType,
    /// API keys for this provider.
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// How to select among multiple `api_keys`.
    #[serde(default)]
    pub key_strategy: KeyStrategy,
    /// Base URL for the provider's API.
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// Timeout in seconds for requests to this provider.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Custom headers to add to or override in upstream requests.
    /// These are applied after the provider's default headers,
    /// so they can override auth, content-type, etc.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Request body field overrides merged into the upstream JSON body.
    /// Key-value pairs are inserted at the top level of the body,
    /// allowing you to set or override any field (e.g. max_tokens, temperature,
    /// or provider-specific parameters).
    #[serde(default)]
    pub body_overrides: HashMap<String, serde_json::Value>,
    /// List of model IDs enabled for this provider.
    /// When non-empty, only these models may be used with this provider.
    /// When empty (default), all models are allowed (backward compatible).
    #[serde(default)]
    pub enabled_models: Vec<String>,
}

pub fn default_base_url() -> String {
    "https://api.openai.com/v1".into()
}
pub fn default_retries() -> u32 {
    2
}
pub fn default_timeout_secs() -> u64 {
    120
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
    #[serde(default = "default_api_endpoint")]
    pub api_endpoint: String,
    /// Target selection strategy (default: `FillFirst`).
    #[serde(default)]
    pub strategy: Strategy,
    /// Per-target retry count override for this route.
    /// When `None` (default), falls back to the global `GatewayConfig.retries`.
    /// When `Some(n)`, each target in this route retries up to `n` times.
    #[serde(default)]
    pub retries: Option<u32>,
    /// List of targets for this route.
    pub targets: Vec<RouteTarget>,
}

fn default_api_endpoint() -> String {
    "/chat/completions".into()
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
    #[serde(default)]
    pub real_model: Option<String>,
    /// Priority within this route — higher = higher priority. Targets are
    /// sorted in descending order, so a target with priority `20` is tried
    /// before one with priority `0`. Defaults to `0`.
    #[serde(default)]
    pub priority: u32,
    /// Per-target retry override.
    /// When `None`, falls back to the route's `retries`, then to the global default.
    /// When `Some(n)`, this target retries up to `n` times regardless of route/global
    /// settings.
    #[serde(default)]
    pub retries: Option<u32>,
    /// Per-target extra body fields merged into the upstream JSON body.
    /// Applied after provider-level body_overrides, so target-level
    /// keys take precedence.
    #[serde(default)]
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
}
