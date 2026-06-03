use serde::{Deserialize, Serialize};

/// Root configuration for the SAG (Smart AI Gateway).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub server: ServerConfig,
    pub analytics: Option<AnalyticsConfig>,
    pub gateway_auth: Option<GatewayAuthConfig>,
    pub providers: std::collections::HashMap<String, ProviderConfig>,
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
    /// API key for this provider (injected into requests).
    pub api_key: String,
    /// Base URL for the provider's API.
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// Provider-level priority — lower = higher priority (used as fallback).
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

fn default_base_url() -> String {
    "https://api.openai.com/v1".into()
}
fn default_priority() -> u32 {
    10
}
fn default_max_retries() -> u32 {
    2
}
fn default_timeout_secs() -> u64 {
    120
}

/// Per-model routing configuration.
/// Maps a requested model name to one or more provider targets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteConfig {
    /// The model name as requested by the client.
    pub model_name: String,
    /// Ordered list of targets — first matching/reachable target wins.
    pub targets: Vec<RouteTarget>,
}

/// A routing target: maps a logical model to a specific provider + real model name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteTarget {
    /// Provider name (must match a key in `providers`).
    pub provider: String,
    /// The real model name to send to this provider.
    pub real_model: String,
    /// Model-level priority within this route — lower = higher priority.
    #[serde(default = "default_priority")]
    pub priority: u32,
}
