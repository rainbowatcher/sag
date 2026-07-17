use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// Macro: produce a minimal `GatewayConfig` with sensible defaults for
/// tests. Use struct-update syntax to override fields:
///
/// ```ignore
/// let config = GatewayConfig {
///     providers: my_providers,
///     ..gateway_config_base!()
/// };
/// ```
///
/// Defaults:
/// - `server`: 127.0.0.1:3000
/// - `gateway_auth`: `Some(GatewayAuthConfig { api_keys: ["test-key"] })`
/// - `backoff`: `BackoffConfig::default()` (retries 2, base 100ms, max 5s, jitter)
/// - `circuit_breaker`: `CircuitBreakerConfig::default()` (quota 1h, rate 10s, generic 60s)
/// - `logging`: `LoggingConfig::default()` (info, compact)
/// - `shuffle_cache_ttl_secs`: 7200
/// - `analytics`, `providers`, `routes`: empty
#[macro_export]
macro_rules! gateway_config_base {
    () => {
        $crate::GatewayConfig {
            server: $crate::ServerConfig {
                host: "127.0.0.1".into(),
                port: 3000,
            },
            analytics: ::std::option::Option::None,
            gateway_auth: ::std::option::Option::Some($crate::GatewayAuthConfig {
                api_keys: ::std::vec!["test-key".into()],
            }),
            providers: ::std::collections::HashMap::new(),
            backoff: $crate::BackoffConfig::default(),
            logging: $crate::LoggingConfig::default(),
            routes: ::std::vec::Vec::new(),
            shuffle_cache_ttl_secs: 7200,
            circuit_breaker: $crate::CircuitBreakerConfig::default(),
        }
    };
}

/// Root configuration for the SAG (Smart AI Gateway).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub server: ServerConfig,
    pub analytics: Option<AnalyticsConfig>,
    pub gateway_auth: Option<GatewayAuthConfig>,
    pub providers: std::collections::HashMap<String, ProviderConfig>,
    /// Global exponential-backoff settings for retry delays (including global retries).
    #[serde(default)]
    pub backoff: BackoffConfig,
    /// Circuit-breaker cooldown durations by error category.
    #[serde(default)]
    pub circuit_breaker: CircuitBreakerConfig,
    /// Fill-first shuffle cache TTL in seconds. The shuffled target order
    /// is re-randomised after this many seconds since the last shuffle.
    /// Defaults to 7200 (2 hours). Set to 0 to disable automatic re-shuffle
    /// (the cached order persists until the route config changes).
    #[serde(default = "default_shuffle_cache_ttl_secs")]
    pub shuffle_cache_ttl_secs: u64,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
    /// Logging configuration (optional, default log level: "info").
    /// The `RUST_LOG` environment variable, if set, takes precedence.
    #[serde(default)]
    pub logging: LoggingConfig,
}

/// Result of removing a provider from the configuration, describing which
/// routes and targets were affected by the cascade.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CascadeResult {
    /// Names of routes that had at least one target referencing the deleted provider.
    /// This includes routes that were entirely removed (listed in `deleted_routes`).
    pub cascaded_routes: Vec<String>,
    /// Names of routes that were entirely removed because all their targets
    /// referenced the deleted provider (a subset of `cascaded_routes`).
    pub deleted_routes: Vec<String>,
    /// Total number of targets removed across all routes.
    pub removed_targets: usize,
}

impl GatewayConfig {
    /// Remove a provider from the configuration, cascading the deletion to all
    /// routes that reference it.
    ///
    /// - Removes the named provider from `self.providers`.
    /// - Walks every route and drops any `RouteTarget` referencing the deleted
    ///   provider. Routes that end up with zero targets are entirely removed.
    ///
    /// Returns `None` when no provider with the given name exists; otherwise
    /// returns a [`CascadeResult`] describing which routes were touched.
    pub fn remove_provider(&mut self, name: &str) -> Option<CascadeResult> {
        if !self.providers.contains_key(name) {
            return None;
        }

        let mut cascaded_routes: Vec<String> = Vec::new();
        let mut deleted_routes: Vec<String> = Vec::new();
        let mut removed_targets: usize = 0;

        for route in self.routes.iter_mut() {
            let before = route.targets.len();
            route.targets.retain(|t| t.provider != name);
            let dropped = before - route.targets.len();
            if dropped > 0 {
                removed_targets += dropped;
                cascaded_routes.push(route.model_name.clone());
                if route.targets.is_empty() {
                    deleted_routes.push(route.model_name.clone());
                }
            }
        }

        // Drop routes that ended up with no targets at all.
        self.routes.retain(|r| !r.targets.is_empty());
        self.providers.remove(name);

        Some(CascadeResult {
            cascaded_routes,
            deleted_routes,
            removed_targets,
        })
    }

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
        let server_addr = format!("{}:{}", self.server.host, self.server.port);
        if server_addr.parse::<std::net::SocketAddr>().is_err() {
            errors.push(format!(
                "server address '{server_addr}' is not a valid socket address (must be IP:port)"
            ));
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

        // ── Gateway auth validation ──
        if let Some(ref auth) = self.gateway_auth
            && auth.api_keys.is_empty()
        {
            errors.push(
                "gateway_auth.api_keys must not be empty when authentication is configured".into(),
            );
        }

        // ── Providers validation ──
        let provider_names: HashSet<&str> = self.providers.keys().map(String::as_str).collect();

        if self.providers.is_empty() {
            errors.push("at least one provider is required".into());
        }

        // ── Circuit breaker validation ──
        if self.circuit_breaker.quota_exhausted_secs == 0 {
            errors.push("circuit_breaker.quota_exhausted_secs must be greater than 0".into());
        }
        if self.circuit_breaker.rate_limited_secs == 0 {
            errors.push("circuit_breaker.rate_limited_secs must be greater than 0".into());
        }
        if self.circuit_breaker.generic_secs == 0 {
            errors.push("circuit_breaker.generic_secs must be greater than 0".into());
        }
        if self.circuit_breaker.probe_interval_secs == 0 {
            errors.push("circuit_breaker.probe_interval_secs must be greater than 0".into());
        }

        if self.backoff.retries > 10 {
            warnings.push(format!(
                "global retries={} seems unusually high",
                self.backoff.retries
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
            match url::Url::parse(&provider.base_url) {
                Ok(url) => {
                    if url.scheme() != "http" && url.scheme() != "https" {
                        errors.push(format!(
                            "provider '{name}': base_url '{}' has unsupported scheme '{}', expected 'http' or 'https'",
                            provider.base_url, url.scheme()
                        ));
                    }
                }
                Err(e) => {
                    errors.push(format!(
                        "provider '{name}': base_url '{}' is not a valid URL: {e}",
                        provider.base_url
                    ));
                }
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
            if let Some(cts) = provider.chunk_timeout_secs
                && cts == 0
            {
                errors.push(format!(
                    "provider '{name}': chunk_timeout_secs must be greater than 0"
                ));
            }
            if let Some(tts) = provider.ttfb_timeout_secs
                && tts == 0
            {
                errors.push(format!(
                    "provider '{name}': ttfb_timeout_secs must be greater than 0"
                ));
            }
            if provider.key_strategy == KeyStrategy::RoundRobin && provider.api_keys.len() == 1 {
                warnings.push(format!(
                    "provider '{name}': RoundRobin strategy with a single API key behaves the same as FillFirst"
                ));
            }
        }

        // ── Routes validation ──
        if self.routes.is_empty() {
            warnings.push(
                "no routes configured; the gateway will not handle any /v1/chat/completions requests"
                    .into(),
            );
        }

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

            if let Some(ts) = route.timeout_secs
                && ts == 0
            {
                errors.push(format!(
                    "routes[{i}] (model='{}'): timeout_secs must be greater than 0",
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
                if let Some(ts) = target.timeout_secs
                    && ts == 0
                {
                    errors.push(format!(
                        "routes[{i}] (model='{}'): target[{j}] timeout_secs must be greater than 0",
                        route.model_name
                    ));
                }
            }

            // ── Route target availability ──
            let has_enabled_target = route.targets.iter().any(|t| {
                self.providers
                    .get(&t.provider)
                    .map(|p| p.enabled)
                    .unwrap_or(false)
            });
            if !has_enabled_target && !route.targets.is_empty() {
                warnings.push(format!(
                    "routes[{i}] (model='{}'): no enabled targets — all referenced providers are disabled",
                    route.model_name
                ));
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
            for after_idx in (idx + 1)..self.routes.len() {
                warnings.push(format!(
                    "routes[{after_idx}] (model='{}'): unreachable because wildcard route at index {idx} precedes it",
                    self.routes[after_idx].model_name
                ));
            }
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
    ///
    /// Deliberately has **no** `skip_serializing_if` — this field is always
    /// written to disk even when equal to the default. The create path
    /// (`to_document`) and the update path (`update_existing_provider`)
    /// both write it unconditionally. Do not add `skip_serializing_if`
    /// without also updating the update path to mirror the skip rule.
    #[serde(default = "default_models_endpoint")]
    pub models_endpoint: String,
    /// Timeout in seconds for requests to this provider.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Maximum idle time in seconds between stream chunks before the
    /// connection is terminated. When `None` (default), the stream waits
    /// indefinitely for the next chunk. Only applies to streaming
    /// requests; non-streaming requests use `timeout_secs` for the
    /// entire HTTP round-trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_timeout_secs: Option<u64>,
    /// Maximum time in seconds to wait for the upstream response headers
    /// (time-to-first-byte) on **streaming** requests. The clock starts
    /// when the request begins connecting and stops as soon as the
    /// response headers arrive; it does **not** constrain the subsequent
    /// body stream (that is governed by `chunk_timeout_secs`). A TTFB
    /// timeout is treated as a retryable failure so the dispatcher can
    /// fall over to the next target. When `None`, the default
    /// [`default_ttfb_timeout_secs`] applies. Non-streaming requests
    /// ignore this and use `timeout_secs` for the entire round-trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttfb_timeout_secs: Option<u64>,
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
    /// Per-model body field overrides, keyed by `real_model` name.
    /// Applied after provider-level `body_overrides` but before target-level
    /// `extra`, so a model-level override can refine or replace a
    /// provider-level default. Only the override map for the dispatched
    /// `real_model` is applied — other models' maps are ignored.
    ///
    /// TOML example:
    /// ```toml
    /// model_overrides = { "gpt-4o" = { max_tokens = 4096, temperature = 0.8 } }
    /// ```
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub model_overrides: HashMap<String, HashMap<String, serde_json::Value>>,
    /// Role mapping table: maps request roles to upstream-supported roles.
    /// For example, `{ "developer" = "system" }` converts all developer
    /// messages to system messages. Keys and values are case-sensitive.
    /// Empty by default — no mapping applied.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub role_mappings: HashMap<String, String>,
    /// Whether this provider is enabled. When `false`, the provider is skipped
    /// during routing and shown as disabled in the admin UI.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// List of model IDs enabled for this provider.
    /// When non-empty, only these models may be used with this provider.
    /// When empty (default), all models are allowed (backward compatible).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_models: Vec<String>,
    /// Whether to inject `cache_control: {"type": "ephemeral"}` breakpoints
    /// into Anthropic requests. Only meaningful for `provider_type = "anthropic"`.
    /// When enabled, the gateway marks the system prompt, tools, and conversation
    /// history for caching, which can significantly reduce costs for requests
    /// with stable prefixes.
    #[serde(default = "default_true")]
    pub anthropic_cache_control: bool,
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
    600
}

/// Default time-to-first-byte timeout for streaming requests (seconds).
///
/// Covers connection establishment plus the wait for the upstream
/// response headers. Once headers arrive, this budget no longer applies
/// and the body stream is governed by `chunk_timeout_secs`.
pub fn default_ttfb_timeout_secs() -> u64 {
    60
}

/// Exponential-backoff settings for retry delays between attempts.
///
/// Exponential-backoff settings for retry delays between attempts.
///
/// Each retry sleeps for `min(base_ms * 2^attempt, max_ms)` with
/// optional full jitter (`random(0, cap)`). So attempt 0 waits
/// `base_ms`, attempt 1 waits `2*base_ms` (capped at `max_ms`), and so on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackoffConfig {
    /// Global default for per-target retries (can be overridden per-route or per-target).
    /// `1` → one attempt (no retries). `2` → one initial + one retry, etc.
    #[serde(default = "default_retries")]
    pub retries: u32,
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
            retries: default_retries(),
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

/// Circuit-breaker cooldown durations by error category.
///
/// Each field specifies how long (in seconds) a route target stays open
/// after its retries are exhausted. The mapping from upstream error to
/// category is performed by [`sag_core::CircuitHint`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    /// Cooldown after quota/billing exhaustion (HTTP 429 with quota keywords).
    /// Default: 3600 (1 hour).
    #[serde(default = "default_quota_exhausted_secs")]
    pub quota_exhausted_secs: u64,
    /// Cooldown after a plain rate-limit 429 (non-quota).
    /// Default: 10.
    #[serde(default = "default_rate_limited_secs")]
    pub rate_limited_secs: u64,
    /// Cooldown for generic retryable failures (5xx / timeout / network).
    /// Default: 60.
    #[serde(default = "default_generic_secs")]
    pub generic_secs: u64,
    /// How often a half-open probe is allowed while a target is still cooling
    /// down. Lets a target recover as soon as the upstream is healthy again
    /// (e.g. quota topped up mid-cooldown) instead of staying skipped for the
    /// whole cooldown. Default: 60.
    #[serde(default = "default_probe_interval_secs")]
    pub probe_interval_secs: u64,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            quota_exhausted_secs: default_quota_exhausted_secs(),
            rate_limited_secs: default_rate_limited_secs(),
            generic_secs: default_generic_secs(),
            probe_interval_secs: default_probe_interval_secs(),
        }
    }
}

fn default_quota_exhausted_secs() -> u64 {
    3600
}
fn default_rate_limited_secs() -> u64 {
    10
}
fn default_generic_secs() -> u64 {
    60
}
fn default_probe_interval_secs() -> u64 {
    60
}
fn default_shuffle_cache_ttl_secs() -> u64 {
    7200
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
    /// Try targets in priority order; within the same priority, shuffle once
    /// on first access and reuse the same order for all subsequent requests.
    /// Once switched to a backup target, keep using it until it fails.
    #[default]
    StickyFailover,
    /// Try targets in priority order; within the same priority, shuffle once
    /// on first access and reuse the same order for all subsequent requests.
    /// The first target absorbs all traffic until it becomes unavailable.
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
/// - `StickyFailover`: similar to `FillFirst` but once switched to a backup target,
///   keeps using it until it fails (maximizing cache hit rate).
/// - `FillFirst`: sorted by priority (descending); same-priority targets are
///   shuffled once on first access and the order is cached. The first target
///   absorbs all traffic until it becomes unavailable.
/// - `RoundRobin`: targets are tried in a rotating order. Each request advances
///   the counter, distributing load evenly.
///
/// ## Retries
///
/// The effective number of retries for each target is resolved via three-tier
/// fallback: **target-level** → **route-level** → **global** (`GatewayConfig.backoff.retries`).
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
    /// Target selection strategy (default: `StickyFailover`).
    #[serde(default, skip_serializing_if = "is_default_strategy")]
    pub strategy: Strategy,
    /// Per-target retry count override for this route.
    /// When `None` (default), falls back to the global `GatewayConfig.backoff.retries`.
    /// When `Some(n)`, each target in this route retries up to `n` times.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
    /// Per-target timeout override for this route (seconds).
    /// When `None` (default), falls back to the target-level, then to the
    /// provider's `timeout_secs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
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
/// route's `strategy`. Targets with the same priority are shuffled once then
/// cached when strategy is `FillFirst`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Per-target timeout override (seconds).
    /// When `None`, falls back to the route's `timeout_secs`, then to the
    /// provider's `timeout_secs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
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
        assert_eq!(openai.timeout_secs, 600);
        assert_eq!(config.backoff.retries, 2);

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
        assert_eq!(config.backoff.retries, 2);
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
[server]

[providers]

[backoff]
retries = 5

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
        assert_eq!(config.backoff.retries, 5);
        assert_eq!(config.routes[0].retries, None);
        assert_eq!(config.routes[0].targets[0].retries, None);
        assert_eq!(config.routes[1].retries, Some(3));
        assert_eq!(config.routes[1].targets[0].retries, Some(1));
    }

    #[test]
    fn parse_legacy_top_level_retries_ignored() {
        // `retries` moved under [backoff]; serde silently ignores the
        // top-level key and backoff.retries falls back to its default (2).
        // The loader's unknown-key detection will warn about top-level
        // `retries` as an unrecognised key.
        let toml_str = r#"
[server]
[providers.openai]
api_keys = ["sk-test"]
base_url = "https://api.openai.com/v1"

retries = 5
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.backoff.retries, 2);
    }

    #[test]
    fn validate_rejects_zero_ttfb_timeout_secs() {
        let toml_str = r#"
[server]
[providers.openai]
api_keys = ["sk-test"]
ttfb_timeout_secs = 0

[[routes]]
model_name = "gpt-4o"
targets = [
    { provider = "openai", real_model = "gpt-4o" },
]
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        let errors = config.validate().unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("ttfb_timeout_secs must be greater than 0")),
            "expected ttfb_timeout_secs error, got: {errors:?}"
        );
    }

    #[test]
    fn ttfb_timeout_secs_parses_and_defaults() {
        let toml_str = r#"
[server]
[providers.openai]
api_keys = ["sk-test"]
ttfb_timeout_secs = 45

[[routes]]
model_name = "gpt-4o"
targets = [{ provider = "openai", real_model = "gpt-4o" }]
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.providers.get("openai").unwrap().ttfb_timeout_secs,
            Some(45)
        );
        // Omitted at the provider layer resolves to this crate default.
        assert_eq!(default_ttfb_timeout_secs(), 60);
    }

    #[test]
    fn validate_rejects_zero_timeout_secs() {
        let toml_str = r#"
[server]
[providers.openai]
api_keys = ["sk-test"]
base_url = "https://api.openai.com/v1"

[[routes]]
model_name = "gpt-4o"
api_endpoint = "/chat/completions"
timeout_secs = 0
targets = [
    { provider = "openai", real_model = "gpt-4o", priority = 10, timeout_secs = 0 },
]
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        let errors = config.validate().unwrap_err();
        let timeout_errors: Vec<_> = errors
            .iter()
            .filter(|e| e.contains("timeout_secs must be greater than 0"))
            .collect();
        assert_eq!(
            timeout_errors.len(),
            2,
            "expected route + target timeout_secs errors, got: {errors:?}"
        );
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

    #[test]
    fn role_mappings_defaults_to_empty() {
        let config: ProviderConfig = toml::from_str(r#"api_keys = ["sk-test"]"#).unwrap();
        assert!(config.role_mappings.is_empty());
    }

    #[test]
    fn role_mappings_parses_valid_mapping() {
        let config: ProviderConfig = toml::from_str(
            r#"
api_keys = ["sk-test"]
role_mappings = { "developer" = "system", "user" = "assistant" }
"#,
        )
        .unwrap();
        assert_eq!(config.role_mappings["developer"], "system");
        assert_eq!(config.role_mappings["user"], "assistant");
        assert_eq!(config.role_mappings.len(), 2);
    }

    #[test]
    fn role_mappings_empty_with_other_fields_is_backward_compatible() {
        let config: ProviderConfig = toml::from_str(
            r#"
api_keys = ["sk-test"]
base_url = "https://custom.example.com/v1"
timeout_secs = 60
enabled = false
"#,
        )
        .unwrap();
        assert!(config.role_mappings.is_empty());
        assert_eq!(config.base_url, "https://custom.example.com/v1");
        assert!(!config.enabled);
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
                timeout_secs: None,
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
                    timeout_secs: None,
                    extra: HashMap::new(),
                },
                RouteTarget {
                    provider: "openai".into(),
                    real_model: Some("gpt-4".into()),
                    priority: 5,
                    retries: None,
                    timeout_secs: None,
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
                    timeout_secs: None,
                    extra: HashMap::new(),
                },
                RouteTarget {
                    provider: "openai".into(),
                    real_model: Some("gpt-3.5-turbo".into()),
                    priority: 5,
                    retries: None,
                    timeout_secs: None,
                    extra: HashMap::new(),
                },
            ];
            assert!(
                config.validate().is_ok(),
                "distinct real_models with the same provider must validate"
            );
        }

        #[test]
        fn rejects_invalid_server_address() {
            let mut config = valid_config();
            config.server.host = "not-an-ip".into();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter().any(|e| e.contains("not a valid socket address")),
                "expected socket address error, got {err:?}"
            );
        }

        #[test]
        fn rejects_empty_gateway_auth_keys() {
            let mut config = valid_config();
            config.gateway_auth = Some(GatewayAuthConfig { api_keys: vec![] });
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("gateway_auth.api_keys must not be empty")),
                "expected gateway_auth error, got {err:?}"
            );
        }

        #[test]
        fn accepts_no_gateway_auth() {
            let mut config = valid_config();
            config.gateway_auth = None;
            assert!(config.validate().is_ok());
        }

        #[test]
        fn accepts_empty_routes_as_warning() {
            let mut config = valid_config();
            config.routes.clear();
            assert!(config.validate().is_ok());
        }

        #[test]
        fn accepts_disabled_targets_as_warning() {
            let mut config = valid_config();
            config.providers.get_mut("openai").unwrap().enabled = false;
            assert!(config.validate().is_ok());
        }

        #[test]
        fn rejects_invalid_base_url_scheme() {
            let mut config = valid_config();
            config.providers.get_mut("openai").unwrap().base_url = "ftp://api.openai.com/v1".into();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter().any(|e| e.contains("unsupported scheme")),
                "expected unsupported scheme error, got {err:?}"
            );
        }

        #[test]
        fn rejects_zero_circuit_breaker_quota() {
            let mut config = valid_config();
            config.circuit_breaker.quota_exhausted_secs = 0;
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("quota_exhausted_secs must be greater than 0")),
                "expected quota_exhausted_secs error, got {err:?}"
            );
        }

        #[test]
        fn rejects_zero_circuit_breaker_rate() {
            let mut config = valid_config();
            config.circuit_breaker.rate_limited_secs = 0;
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("rate_limited_secs must be greater than 0")),
                "expected rate_limited_secs error, got {err:?}"
            );
        }

        #[test]
        fn rejects_zero_circuit_breaker_generic() {
            let mut config = valid_config();
            config.circuit_breaker.generic_secs = 0;
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("generic_secs must be greater than 0")),
                "expected generic_secs error, got {err:?}"
            );
        }

        #[test]
        fn rejects_zero_circuit_breaker_probe_interval() {
            let mut config = valid_config();
            config.circuit_breaker.probe_interval_secs = 0;
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("probe_interval_secs must be greater than 0")),
                "expected probe_interval_secs error, got {err:?}"
            );
        }

        #[test]
        fn accepts_wildcard_not_last_as_warning() {
            let config: GatewayConfig = toml::from_str(
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

[[routes]]
model_name = "*"
targets = [
    { provider = "openai" },
]

[[routes]]
model_name = "claude"
targets = [
    { provider = "openai" },
]
"#,
            )
            .unwrap();
            assert!(config.validate().is_ok());
        }
    }

    // ── remove_provider unit tests ──────────────────────────────────
    mod remove_provider {
        use super::*;

        /// Helper: build a minimal `GatewayConfig` with given providers and routes.
        fn config_with(
            providers: HashMap<String, ProviderConfig>,
            routes: Vec<RouteConfig>,
        ) -> GatewayConfig {
            GatewayConfig {
                providers,
                routes,
                ..gateway_config_base!()
            }
        }

        /// Helper: a bare-bones `ProviderConfig` for testing.
        fn test_provider(name: &str) -> (String, ProviderConfig) {
            (
                name.into(),
                ProviderConfig {
                    provider_type: ProviderType::OpenAI,
                    api_keys: vec![format!("sk-{name}")],
                    key_strategy: KeyStrategy::FillFirst,
                    base_url: "https://example.com".into(),
                    models_endpoint: "models".into(),
                    timeout_secs: 120,
                    chunk_timeout_secs: None,
                    ttfb_timeout_secs: None,
                    headers: HashMap::new(),
                    body_overrides: HashMap::new(),
                    model_overrides: HashMap::new(),
                    role_mappings: HashMap::new(),
                    enabled_models: vec![],
                    enabled: true,
                    anthropic_cache_control: false,
                },
            )
        }

        /// Helper: a `RouteTarget` referencing a provider by name.
        fn target(provider: &str) -> RouteTarget {
            RouteTarget {
                provider: provider.into(),
                real_model: Some(format!("model-{provider}")),
                priority: 0,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
            }
        }

        /// Helper: a `RouteConfig` with a model name and targets.
        fn route(model: &str, targets: Vec<RouteTarget>) -> RouteConfig {
            RouteConfig {
                model_name: model.into(),
                api_endpoint: "/chat/completions".into(),
                strategy: Strategy::FillFirst,
                retries: None,
                timeout_secs: None,
                targets,
            }
        }

        #[test]
        fn returns_none_when_provider_not_found() {
            let mut config = config_with(HashMap::new(), vec![]);
            let result = config.remove_provider("nonexistent");
            assert!(result.is_none());
        }

        #[test]
        fn removes_provider_no_route_refs() {
            let providers = HashMap::from([test_provider("openai")]);
            let mut config = config_with(providers, vec![]);
            let result = config.remove_provider("openai").unwrap();
            assert!(config.providers.is_empty());
            assert!(result.cascaded_routes.is_empty());
            assert!(result.deleted_routes.is_empty());
            assert_eq!(result.removed_targets, 0);
        }

        #[test]
        fn cascades_partial_route_targets() {
            let providers = HashMap::from([test_provider("openai"), test_provider("anthropic")]);
            let routes = vec![route("gpt-4o", vec![target("openai"), target("anthropic")])];
            let mut config = config_with(providers, routes);

            let result = config.remove_provider("openai").unwrap();
            assert!(!config.providers.contains_key("openai"));
            assert!(config.providers.contains_key("anthropic"));
            assert_eq!(result.cascaded_routes, vec!["gpt-4o"]);
            assert!(result.deleted_routes.is_empty());
            assert_eq!(result.removed_targets, 1);

            // The remaining route still has one target.
            assert_eq!(config.routes.len(), 1);
            assert_eq!(config.routes[0].targets.len(), 1);
            assert_eq!(config.routes[0].targets[0].provider, "anthropic");
        }

        #[test]
        fn deletes_route_with_only_deleted_provider() {
            let providers = HashMap::from([test_provider("openai")]);
            let routes = vec![
                route("gpt-4o", vec![target("openai")]),
                // This route references only "openai" and should be deleted.
                route("gpt-3.5-turbo", vec![target("openai")]),
            ];
            let mut config = config_with(providers, routes);

            let result = config.remove_provider("openai").unwrap();
            assert!(config.providers.is_empty());
            assert!(config.routes.is_empty());
            assert_eq!(result.cascaded_routes, vec!["gpt-4o", "gpt-3.5-turbo"]);
            assert_eq!(result.deleted_routes, vec!["gpt-4o", "gpt-3.5-turbo"]);
            assert_eq!(result.removed_targets, 2);
        }

        #[test]
        fn mixed_cascade_partial_and_full_deletion() {
            let providers = HashMap::from([test_provider("openai"), test_provider("anthropic")]);
            let routes = vec![
                // Partial: one target removed, route survives.
                route("gpt-4o", vec![target("openai"), target("anthropic")]),
                // Full: only target is "openai", route deleted.
                route("gpt-3.5-turbo", vec![target("openai")]),
                // Unaffected: no reference to "openai".
                route("claude", vec![target("anthropic")]),
            ];
            let mut config = config_with(providers, routes);

            let result = config.remove_provider("openai").unwrap();
            assert_eq!(result.cascaded_routes, vec!["gpt-4o", "gpt-3.5-turbo"]);
            assert_eq!(result.deleted_routes, vec!["gpt-3.5-turbo"]);
            assert_eq!(result.removed_targets, 2);

            // Two routes survive: gpt-4o (with only anthropic) and claude.
            assert_eq!(config.routes.len(), 2);
            let gpt = config
                .routes
                .iter()
                .find(|r| r.model_name == "gpt-4o")
                .unwrap();
            assert_eq!(gpt.targets.len(), 1);
            let claude = config
                .routes
                .iter()
                .find(|r| r.model_name == "claude")
                .unwrap();
            assert_eq!(claude.targets.len(), 1);
        }
    }
}
