use serde::{Deserialize, Serialize};

/// Root configuration for the SAG (Smart AI Gateway).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub server: ServerConfig,
    pub analytics: Option<AnalyticsConfig>,
    pub gateway_auth: Option<GatewayAuthConfig>,
    pub providers: std::collections::HashMap<String, ProviderConfig>,
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

/// Upstream LLM provider configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Provider adapter type: "openai" (default) or "anthropic".
    #[serde(default = "default_provider_type")]
    pub provider_type: String,
    /// API key for this provider (injected into requests).
    pub api_key: String,
    /// Base URL for the provider's API.
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// Provider-level priority — higher = higher priority, used for fallback
    /// ordering across providers. When a route target references this provider,
    /// targets are sorted in descending priority order so that higher-priority
    /// providers are tried first. Defaults to `0`.
    #[serde(default = "default_priority")]
    pub priority: u32,
    /// Maximum number of retries before giving up on this provider.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Timeout in seconds for requests to this provider.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Set of parameter names this provider supports.
    /// Requests containing parameters not in this set will be rejected with 400.
    #[serde(default)]
    pub supported_params: Vec<String>,
}

fn default_provider_type() -> String {
    "openai".into()
}

fn default_base_url() -> String {
    "https://api.openai.com/v1".into()
}
/// Default priority value (`0`).
///
/// When no priority is specified, targets default to `0`.
fn default_priority() -> u32 {
    0
}
fn default_max_retries() -> u32 {
    2
}
fn default_timeout_secs() -> u64 {
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
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteConfig {
    /// The model name as requested by the client (supports glob-like patterns e.g. "gpt-image-*").
    pub model_name: String,
    /// Upstream API endpoint path for this model.
    /// Examples: `/v1/chat/completions`, `/v1/images/generations`, `/v1/responses`.
    #[serde(default = "default_api_endpoint")]
    pub api_endpoint: String,
    /// Target selection strategy (default: `FillFirst`).
    #[serde(default)]
    pub strategy: Strategy,
    /// List of targets for this route.
    pub targets: Vec<RouteTarget>,
}

fn default_api_endpoint() -> String {
    "/v1/chat/completions".into()
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
    #[serde(default = "default_priority")]
    pub priority: u32,
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
api_key = "sk-test"

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

        // Server
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 8080);

        // Auth
        let auth = config.gateway_auth.unwrap();
        assert_eq!(auth.api_keys, vec!["key1", "key2"]);

        // Analytics
        let analytics = config.analytics.unwrap();
        assert!(analytics.enabled);
        assert_eq!(analytics.storage_dsn, "sqlite://analytics.db"); // default

        // Providers
        let openai = config.providers.get("openai").unwrap();
        assert_eq!(openai.api_key, "sk-test");
        assert_eq!(openai.base_url, "https://api.openai.com/v1"); // default
        assert_eq!(openai.priority, 0); // default
        assert_eq!(openai.max_retries, 2); // default
        assert_eq!(openai.timeout_secs, 120); // default
        assert!(openai.supported_params.is_empty()); // default

        // Routes
        assert_eq!(config.routes.len(), 1);
        let route = &config.routes[0];
        assert_eq!(route.model_name, "gpt-4o");
        assert_eq!(route.targets.len(), 1);
        assert_eq!(route.targets[0].provider, "openai");
        assert_eq!(route.targets[0].real_model.as_deref(), Some("gpt-4o"));
        assert_eq!(route.targets[0].priority, 0); // default
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
    fn parse_provider_with_custom_fields() {
        let toml_str = r#"
[server]

[providers.anthropic]
api_key = "sk-ant"
base_url = "https://api.anthropic.com"
priority = 1
max_retries = 5
timeout_secs = 60
supported_params = ["model", "messages", "stream"]
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        let p = config.providers.get("anthropic").unwrap();
        assert_eq!(p.api_key, "sk-ant");
        assert_eq!(p.base_url, "https://api.anthropic.com");
        assert_eq!(p.priority, 1);
        assert_eq!(p.max_retries, 5);
        assert_eq!(p.timeout_secs, 60);
        assert_eq!(p.supported_params, vec!["model", "messages", "stream"]);
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

        // First route
        assert_eq!(config.routes[0].model_name, "gpt-4o");
        assert_eq!(config.routes[0].targets.len(), 2);
        assert_eq!(config.routes[0].targets[0].priority, 10);
        assert_eq!(config.routes[0].targets[1].priority, 1);

        // Second route
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
