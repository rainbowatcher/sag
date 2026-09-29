use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
pub use sag_core::ApiKind;
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
/// - `backoff`: `BackoffConfig::default()` (retries 2, base 100ms, max 5s, jitter, retry_on_empty_response)
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
            sticky_failover_reset_secs: 7200,
            circuit_breaker: $crate::CircuitBreakerConfig::default(),
            proxy_url: ::std::option::Option::None,
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
    /// Sticky-failover active target 重置超时（秒）。
    /// active target 在该时长后失效，回到正常 fill-first 顺序。
    /// 默认 7200（2 小时）。设为 0 则永不过期。
    #[serde(default = "default_sticky_failover_reset_secs")]
    pub sticky_failover_reset_secs: u64,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
    /// Logging configuration (optional, default log level: "info").
    /// The `RUST_LOG` environment variable, if set, takes precedence.
    #[serde(default)]
    pub logging: LoggingConfig,
    /// Optional global HTTP/HTTPS proxy URL, used as the default upstream
    /// proxy for all providers unless a provider opts out via
    /// `proxy_mode`. `None` (or an empty/whitespace-only value) means no
    /// explicit proxy is configured. SAG **never** reads system proxy
    /// environment variables (`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`),
    /// so a missing `proxy_url` always means direct connections unless a
    /// provider sets `proxy_mode = "enabled"`. Only `http://` and
    /// `https://` URLs are supported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
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

    /// Resolve a request `model` to its route: an exact `model_name` match
    /// first, falling back to the `"*"` wildcard route.
    pub fn resolve_route<'a>(&'a self, model: &str) -> Option<&'a RouteConfig> {
        resolve_model_route(&self.routes, model)
    }

    /// Normalize the gateway-level `proxy_url` into a [`ProxyConfig`].
    ///
    /// Returns `Ok(None)` for `None`, empty, or whitespace-only values
    /// ("no global proxy configured"), and `Err(String)` for malformed
    /// URLs — the same condition [`GatewayConfig::validate`] reports as a
    /// `proxy_url` error. Downstream consumers should use this rather than
    /// re-parsing the raw string, so every caller agrees on what counts as
    /// "a usable global proxy" (including that a blank or invalid value
    /// does **not** count).
    pub fn normalized_global_proxy(&self) -> Result<Option<ProxyConfig>, String> {
        normalize_proxy_url(self.proxy_url.as_deref())
    }

    /// Load-time normalization（`load_config` / `reload_config` 在校验前调用）：
    /// 剥去每个 `apis.<key>.base_url` 的尾部 `/`，避免「base_url + 固定协议
    /// path」拼接出 `//path`（参照 [`normalize_models_endpoint`] 的裁剪语义）。
    pub fn normalize(&mut self) {
        for provider in self.providers.values_mut() {
            for api in provider.apis.values_mut() {
                let trimmed = api.base_url.trim_end_matches('/');
                if trimmed != api.base_url {
                    api.base_url = trimmed.to_string();
                }
            }
        }
    }

    /// Validate the configuration, returning all semantic errors found.
    /// Returns `Ok(())` if no errors, `Err(Vec<String>)` with all error messages.
    pub fn validate(&self) -> std::result::Result<(), Vec<String>> {
        let mut errors: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();

        self.validate_server(&mut errors);
        self.validate_analytics(&mut errors);
        self.validate_gateway_auth(&mut errors);
        let normalized_global_proxy = self.validate_proxy(&mut errors);
        // The empty-map check is emitted here, before circuit-breaker and
        // backoff errors, to preserve the historical error-vector order;
        // the per-provider entry checks run later via `validate_providers`.
        if self.providers.is_empty() {
            errors.push("at least one provider is required".into());
        }
        self.validate_circuit_breaker(&mut errors);
        self.validate_backoff(&mut errors, &mut warnings);
        self.validate_providers(normalized_global_proxy.as_ref(), &mut errors, &mut warnings);
        self.validate_routes(&mut errors, &mut warnings);

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

    /// ── Server validation ──
    fn validate_server(&self, errors: &mut Vec<String>) {
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
    }

    /// ── Analytics validation ──
    fn validate_analytics(&self, errors: &mut Vec<String>) {
        if let Some(ref analytics) = self.analytics
            && analytics.enabled
            && !analytics.storage_dsn.starts_with("sqlite://")
            && analytics.storage_dsn != ":memory:"
        {
            errors
                .push("analytics.storage_dsn must start with 'sqlite://' or be ':memory:'".into());
        }
    }

    /// ── Gateway auth validation ──
    fn validate_gateway_auth(&self, errors: &mut Vec<String>) {
        if let Some(ref auth) = self.gateway_auth
            && auth.api_keys.is_empty()
        {
            errors.push(
                "gateway_auth.api_keys must not be empty when authentication is configured".into(),
            );
        }
    }

    /// ── Proxy validation ──
    ///
    /// Returns the normalized global proxy for later provider checks.
    /// A malformed top-level `proxy_url` is a configuration error; a
    /// whitespace-only value normalizes to `None` and is fine. The
    /// normalized global proxy is also the single source of truth for
    /// whether a usable global proxy exists: `Some("")`,
    /// `Some("   ")` and invalid URLs all resolve to `None` here, so
    /// an `enabled` provider is correctly rejected for all of them —
    /// not just for a literal `None`.
    fn validate_proxy(&self, errors: &mut Vec<String>) -> Option<ProxyConfig> {
        match self.normalized_global_proxy() {
            Ok(proxy) => proxy,
            Err(e) => {
                errors.push(format!("proxy_url: {e}"));
                None
            }
        }
    }

    /// ── Circuit breaker validation ──
    fn validate_circuit_breaker(&self, errors: &mut Vec<String>) {
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
    }

    /// ── Backoff validation ──
    fn validate_backoff(&self, errors: &mut Vec<String>, warnings: &mut Vec<String>) {
        if self.backoff.retries > 10 {
            warnings.push(format!(
                "global retries={} seems unusually high",
                self.backoff.retries
            ));
        }

        if self.backoff.base_ms == 0 {
            errors.push("backoff.base_ms must be greater than 0".into());
        }
        if self.backoff.max_ms < self.backoff.base_ms {
            errors.push(format!(
                "backoff.max_ms ({}) must be >= backoff.base_ms ({})",
                self.backoff.max_ms, self.backoff.base_ms
            ));
        }
    }

    /// ── Providers validation (per-provider entry checks) ──
    fn validate_providers(
        &self,
        normalized_global_proxy: Option<&ProxyConfig>,
        errors: &mut Vec<String>,
        warnings: &mut Vec<String>,
    ) {
        for (name, provider) in &self.providers {
            // ── credential mode: static vs oauth ──
            match provider.auth_mode {
                AuthMode::OAuth => {
                    if provider.oauth.is_none() {
                        errors.push(format!(
                            "provider '{name}': auth_mode='oauth' requires an [providers.{name}.oauth] section"
                        ));
                    }
                    if let Some(oauth) = &provider.oauth {
                        if oauth.token_url.trim().is_empty() {
                            errors.push(format!(
                                "provider '{name}': oauth.token_url must not be empty"
                            ));
                        }
                        if oauth.client_id.trim().is_empty() {
                            errors.push(format!(
                                "provider '{name}': oauth.client_id must not be empty"
                            ));
                        }
                        if oauth.client_secret.trim().is_empty() {
                            errors.push(format!(
                                "provider '{name}': oauth.client_secret must not be empty"
                            ));
                        }
                    }
                    // Phase 1: OAuth tokens are rendered as
                    // `Authorization: Bearer` for chat/responses/image.
                    // The Anthropic Messages wire shape (`x-api-key`) is
                    // not supported for OAuth credentials yet.
                    if provider.apis.contains_key(&ApiKind::AnthropicMessages) {
                        errors.push(format!(
                            "provider '{name}': auth_mode='oauth' does not support anthropic_messages (Phase 1); remove apis.anthropic_messages or use auth_mode='static'"
                        ));
                    }
                    // oauth 模式允许空 api_keys；若同时填写则忽略。
                    if !provider.api_keys.is_empty() {
                        warnings.push(format!(
                            "provider '{name}': auth_mode='oauth' ignores api_keys"
                        ));
                    }
                    // 单 OAuth client：key_strategy 无意义。
                    if provider.key_strategy == KeyStrategy::RoundRobin {
                        warnings.push(format!(
                            "provider '{name}': auth_mode='oauth' uses a single client; key_strategy='round_robin' has no effect"
                        ));
                    }
                }
                AuthMode::Static => {
                    if provider.api_keys.is_empty() {
                        errors.push(format!("provider '{name}': api_keys must not be empty"));
                    }
                    // 字段宽容：static 下写了 oauth 段 → warning（不 error）。
                    if provider.oauth.is_some() {
                        warnings.push(format!(
                            "provider '{name}': oauth section is ignored when auth_mode='static'"
                        ));
                    }
                }
            }
            // ── apis 目录：非空、至少一个协议 key、fallback 规则 ──
            let protocol_keys: Vec<ApiKind> = provider
                .apis
                .keys()
                .copied()
                .filter(|k| k.is_protocol())
                .collect();
            if provider.apis.is_empty() {
                errors.push(format!(
                    "provider '{name}': apis must not be empty — declare at least one protocol API, e.g. [providers.{name}.apis.chat_completions]"
                ));
            } else if protocol_keys.is_empty() {
                errors.push(format!(
                    "provider '{name}': apis must declare at least one protocol key (chat_completions, responses, or anthropic_messages); 'image' alone is not a protocol"
                ));
            } else {
                let key_list: Vec<&str> = protocol_keys.iter().map(|k| k.as_str()).collect();
                match provider.fallback {
                    Some(fb) if !protocol_keys.contains(&fb) => {
                        errors.push(format!(
                            "provider '{name}': fallback='{}' must be one of the configured apis protocol keys [{}]",
                            fb.as_str(),
                            key_list.join(", ")
                        ));
                    }
                    Some(_) => {}
                    None if protocol_keys.len() > 1 => {
                        // 多协议 key 未写 fallback → 报错并给出建议值
                        // （apis 中第一个协议 key，IndexMap 表序）。
                        errors.push(format!(
                            "provider '{name}': fallback is required when apis declares multiple protocol keys [{}]; suggested value: fallback = \"{}\"",
                            key_list.join(", "),
                            protocol_keys[0].as_str()
                        ));
                    }
                    // 单协议 key：降级目标唯一，未写则自动取之 —— 校验通过。
                    None => {}
                }
            }
            // ── 每个 apis 节的 base_url / models_endpoint / 超时 ──
            if !provider.apis.is_empty() {
                for (kind, api) in &provider.apis {
                    match url::Url::parse(&api.base_url) {
                        Ok(u) => {
                            if u.scheme() != "http" && u.scheme() != "https" {
                                errors.push(format!(
                                    "provider '{name}': apis.{}.base_url '{}' has unsupported scheme '{}', expected 'http' or 'https'",
                                    kind.as_str(),
                                    api.base_url,
                                    u.scheme()
                                ));
                            }
                        }
                        Err(e) => {
                            errors.push(format!(
                                "provider '{name}': apis.{}.base_url '{}' is not a valid URL: {e}",
                                kind.as_str(),
                                api.base_url
                            ));
                        }
                    }
                    if let Some(endpoint) = &api.models_endpoint
                        && (endpoint.contains("://")
                            || endpoint.contains('?')
                            || endpoint.contains('#')
                            || endpoint
                                .split('/')
                                .any(|segment| segment.contains(char::is_whitespace)))
                    {
                        errors.push(format!(
                            "provider '{name}': apis.{}.models_endpoint '{}' must be a relative path without scheme, query, fragment, or whitespace",
                            kind.as_str(),
                            endpoint
                        ));
                    }
                    if api.timeout_secs == Some(0) {
                        errors.push(format!(
                            "provider '{name}': apis.{}.timeout_secs must be greater than 0",
                            kind.as_str()
                        ));
                    }
                    if api.chunk_timeout_secs == Some(0) {
                        errors.push(format!(
                            "provider '{name}': apis.{}.chunk_timeout_secs must be greater than 0",
                            kind.as_str()
                        ));
                    }
                    if api.ttfb_timeout_secs == Some(0) {
                        errors.push(format!(
                            "provider '{name}': apis.{}.ttfb_timeout_secs must be greater than 0",
                            kind.as_str()
                        ));
                    }
                }
            }
            // `proxy_mode = "enabled"` forces the top-level `proxy_url`;
            // without one it cannot be satisfied and is a config error.
            // The check runs against the *normalized* global proxy so a
            // blank or invalid `proxy_url` is treated as "no proxy
            // available" rather than silently satisfying the mode.
            if provider.proxy_mode == ProxyMode::Enabled && normalized_global_proxy.is_none() {
                errors.push(format!(
                    "provider '{name}': proxy_mode='enabled' requires a top-level proxy_url to be configured"
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
    }

    /// ── Routes validation (including wildcard-not-last checks) ──
    fn validate_routes(&self, errors: &mut Vec<String>, warnings: &mut Vec<String>) {
        let provider_names: HashSet<&str> = self.providers.keys().map(String::as_str).collect();

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

            // ── api_endpoint × pipeline ──
            // `api_endpoint` 是 image 路线的真实上游路径（D3/D8），
            // chat 族 route 不允许出现该字段。
            match (&route.pipeline, &route.api_endpoint) {
                (Pipeline::Chat, Some(endpoint)) => {
                    errors.push(format!(
                        "routes[{i}] (model='{}'): api_endpoint='{endpoint}' is only valid when pipeline = \"image\"; remove api_endpoint from a chat-pipeline route",
                        route.model_name
                    ));
                }
                (Pipeline::Image, None) => {
                    errors.push(format!(
                        "routes[{i}] (model='{}'): pipeline = \"image\" requires api_endpoint (the real upstream path appended to apis.image.base_url)",
                        route.model_name
                    ));
                }
                (_, Some(endpoint)) if !endpoint.starts_with('/') => {
                    errors.push(format!(
                        "routes[{i}] (model='{}'): api_endpoint must start with '/'",
                        route.model_name
                    ));
                }
                _ => {}
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
                // ── capabilities unknown keys (fixed key set, §7.1.5) ──
                if let Some(caps) = &target.capabilities
                    && let Some(unknown) = caps.unknown_keys_error()
                {
                    errors.push(format!(
                        "routes[{i}] (model='{}'): target[{j}] capabilities: {unknown}",
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

/// Fixed-key capability override for a route target (§7.1.3 / D3).
///
/// Every field is optional: an omitted key leaves the derived capability
/// (keyed by the actually-used protocol) untouched. The
/// key set is fixed — unknown keys are captured during deserialize and
/// rejected by [`GatewayConfig::validate`] (same strictness as typed
/// config enums; `capabilities` is a closed set, not an open map).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CapabilitiesOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,
    /// thinking / reasoning_content support
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documents: Option<bool>,
    /// Responses store / previous_response_id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stateful: Option<bool>,
    /// Responses hosted tool types
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosted_tools: Option<bool>,
    /// Responses background execution
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    /// Unknown keys captured by a flatten so `validate()` can reject them
    /// with a clear error instead of silently dropping them at parse time.
    /// Never written back to disk (empty after a successful validate).
    #[serde(flatten, default, skip_serializing_if = "HashMap::is_empty")]
    pub unknown: HashMap<String, serde_json::Value>,
}

impl CapabilitiesOverride {
    /// Returns `Some(message)` when `unknown` holds at least one key.
    pub fn unknown_keys_error(&self) -> Option<String> {
        if self.unknown.is_empty() {
            return None;
        }
        let mut keys: Vec<&str> = self.unknown.keys().map(String::as_str).collect();
        keys.sort_unstable();
        Some(format!(
            "unknown capability key(s) [{}] — allowed keys: streaming, reasoning, tools, images, documents, stateful, hosted_tools, background",
            keys.join(", ")
        ))
    }

    /// Apply this override on top of a derived capability bitmap.
    pub fn apply_to(&self, base: &mut sag_core::Capabilities) {
        if let Some(v) = self.streaming {
            base.streaming = v;
        }
        if let Some(v) = self.reasoning {
            base.reasoning = v;
        }
        if let Some(v) = self.tools {
            base.tools = v;
        }
        if let Some(v) = self.images {
            base.images = v;
        }
        if let Some(v) = self.documents {
            base.documents = v;
        }
        if let Some(v) = self.stateful {
            base.stateful = v;
        }
        if let Some(v) = self.hosted_tools {
            base.hosted_tools = v;
        }
        if let Some(v) = self.background {
            base.background = v;
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

/// How a provider authenticates to the upstream LLM API.
///
/// Serde values are `static` / `oauth`. Unknown values follow the same
/// strict pattern as the other config enums ([`Strategy`], [`KeyStrategy`],
/// [`ProxyMode`]) — they fail config parsing rather than silently guessing.
///
/// OAuth is a **credential mode**, orthogonal to the `apis` protocol
/// catalog: `auth_mode = "oauth"` with `apis.anthropic_messages` is
/// rejected in validation (Phase 1 does not support rendering OAuth
/// tokens for the Anthropic Messages wire shape).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// Static `api_keys` (existing behavior; default).
    #[default]
    Static,
    /// OAuth 2.0 client-credentials (optional `refresh_token` grant).
    /// Explicit rename: default snake_case would render `o_auth`.
    #[serde(rename = "oauth")]
    OAuth,
}

impl AuthMode {
    /// Stable snake_case identifier matching the serde representation
    /// (`rename_all = "snake_case"`), used by the save path to write the
    /// value to TOML without a serde round-trip.
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthMode::Static => "static",
            AuthMode::OAuth => "oauth",
        }
    }
}

/// OAuth 2.0 client-credentials settings for a provider
/// (`[providers.<name>.oauth]`).
///
/// `client_secret` is stored as TOML plaintext — the same reality as
/// `api_keys`; vault/encryption is out of scope. `OAuthConfig` derives
/// `PartialEq` because `adapter_config_changed` compares `old.oauth !=
/// new.oauth` on hot reload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OAuthConfig {
    /// Token endpoint URL (e.g. Azure AD / OIDC IdP).
    pub token_url: String,
    /// OAuth client id.
    pub client_id: String,
    /// OAuth client secret.
    pub client_secret: String,
    /// Optional OAuth scope(s).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Optional audience/resource parameter some IdPs require.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    /// Optional refresh token. When present, the client first tries
    /// `grant_type=refresh_token`; otherwise `client_credentials`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

/// How a provider resolves the upstream HTTP proxy.
///
/// Serde values are `inherit` / `enabled` / `disabled`. Unknown values
/// follow the same strict pattern as the other config enums
/// ([`Strategy`], [`KeyStrategy`]) — they fail config
/// parsing rather than silently guessing, and there is no lenient
/// fallback for unknown enum values anywhere in the loader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyMode {
    /// Inherit the gateway-level `proxy_url` (default). When no global
    /// proxy is configured this means a direct connection.
    #[default]
    Inherit,
    /// Force using the gateway-level `proxy_url`; a configuration error
    /// when no top-level `proxy_url` is set.
    Enabled,
    /// Force a direct connection, ignoring any gateway-level proxy.
    Disabled,
}

impl ProxyMode {
    /// Stable snake_case identifier matching the serde representation
    /// (`rename_all = "snake_case"`), used by the save path to write the
    /// value to TOML without a serde round-trip.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProxyMode::Inherit => "inherit",
            ProxyMode::Enabled => "enabled",
            ProxyMode::Disabled => "disabled",
        }
    }
}

/// A normalized HTTP/HTTPS proxy endpoint.
///
/// Stores only the connection metadata (scheme, host, port). The original
/// URL string — and any userinfo credentials embedded in it — is
/// deliberately **not** retained, so logging or `Debug`-printing a
/// [`ProxyConfig`] can never leak proxy credentials. Transport-layer code
/// reconstructs a connect URL from these fields via
/// [`ProxyConfig::to_connect_url`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProxyConfig {
    /// `"http"` or `"https"`.
    pub scheme: String,
    /// Host, IPv6 literals stored without brackets (as `url` reports them).
    pub host: String,
    /// Explicit port, or `None` when omitted / the scheme default applies.
    pub port: Option<u16>,
}

impl ProxyConfig {
    /// Reconstruct a connect URL from the stored scheme/host/port (e.g. for
    /// building a transport-layer proxy). IPv6 hosts are re-bracketed.
    /// No userinfo is ever present in the returned URL — credentials are
    /// not stored by [`ProxyConfig`].
    pub fn to_connect_url(&self) -> String {
        // Defensive: strip any brackets a caller may have embedded in an
        // IPv6 host before re-bracketing, so directly-constructed
        // `ProxyConfig`s never produce `[[::1]]`.
        let host = self.host.trim_start_matches('[').trim_end_matches(']');
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        match self.port {
            Some(port) => format!("{}://{}:{}", self.scheme, host, port),
            None => format!("{}://{}", self.scheme, host),
        }
    }
}

impl std::fmt::Display for ProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_connect_url())
    }
}

/// Normalize and validate a raw proxy URL (the top-level `proxy_url` value).
///
/// - `None` → `Ok(None)` (no proxy configured).
/// - A value that is empty / whitespace-only after trimming → `Ok(None)`
///   (treated as "not configured").
/// - Must parse as a URL with the `http` or `https` scheme and a non-empty
///   host. Non-standard ports and IPv6 literals are allowed.
/// - URLs containing userinfo (`user:pass@`) are **rejected**: SAG never
///   records or forwards proxy credentials, so such a URL could never be
///   used and would only risk leaking credentials if echoed into logs.
///
/// Returns `Err(String)` with a human-readable reason for invalid input.
/// The returned [`ProxyConfig`] stores only scheme/host/port — no raw URL
/// text, no credentials — so downstream logging cannot leak them.
pub fn normalize_proxy_url(raw: Option<&str>) -> Result<Option<ProxyConfig>, String> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let url = url::Url::parse(trimmed).map_err(|e| format!("invalid proxy URL: {e}"))?;
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "unsupported scheme '{scheme}': only 'http' and 'https' proxy URLs are supported"
        ));
    }
    let host = match url.host() {
        Some(url::Host::Domain(d)) => d.to_string(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => ip.to_string(),
        None => return Err("proxy URL must have a host".into()),
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Err("proxy URL must not contain userinfo (username/password)".into());
    }

    Ok(Some(ProxyConfig {
        scheme: scheme.to_string(),
        host,
        port: url.port(),
    }))
}

/// Resolve the effective proxy for a provider from its [`ProxyMode`] and
/// the gateway-level proxy.
///
/// - [`ProxyMode::Inherit`] → the gateway-level proxy if any, otherwise
///   `None` (direct connection).
/// - [`ProxyMode::Enabled`] → the gateway-level proxy; `Err` when none is
///   configured (the same condition [`GatewayConfig::validate`] rejects).
/// - [`ProxyMode::Disabled`] → always `None` (forced direct connection).
///
/// This is the pure resolution step — it does not construct any
/// transport-layer (reqwest) type.
pub fn resolve_effective_proxy(
    mode: ProxyMode,
    global_proxy: Option<&ProxyConfig>,
) -> Result<Option<ProxyConfig>, String> {
    match mode {
        ProxyMode::Inherit => Ok(global_proxy.cloned()),
        ProxyMode::Enabled => global_proxy.cloned().map(Some).ok_or_else(|| {
            "proxy_mode='enabled' requires a top-level proxy_url to be configured".into()
        }),
        ProxyMode::Disabled => Ok(None),
    }
}

/// 单个上游 API 节（`[providers.<name>.apis.<key>]`）：地址 + 协议相关覆盖。
///
/// - `base_url` **必填**：host + API 根前缀，不含协议 path（path 由协议 key
///   固定推导，见 `sag_core::protocol_path`；image 的路径来自 route.`api_endpoint`）。
/// - `models_endpoint` 与全部协议相关字段为 `Option`：`None` → 继承 provider
///   级缺省；`Some` → **整段覆盖** provider 级同名字段（map 不合并、数组不并集）。
/// - 未知字段被容忍（warn），未知 apis key 在反序列化层报错（枚举严格）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ApiConfig {
    /// Upstream API root (host + root prefix, no protocol path).
    pub base_url: String,
    /// Relative path used only by admin fetch-models (URL = this node's
    /// `base_url` + `models_endpoint`). `None` → provider-level default
    /// behavior (`"models"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_endpoint: Option<String>,
    /// 覆盖 provider 级 `headers`（整段替换）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    /// 覆盖 provider 级 `body_overrides`（整段替换）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_overrides: Option<HashMap<String, serde_json::Value>>,
    /// 覆盖 provider 级 `strip_fields`（整段替换，数组不并集）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strip_fields: Option<Vec<String>>,
    /// 覆盖 provider 级 `model_strip_fields`（整段替换）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_strip_fields: Option<HashMap<String, Vec<String>>>,
    /// 覆盖 provider 级 `model_overrides`（整段替换）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_overrides: Option<HashMap<String, HashMap<String, serde_json::Value>>>,
    /// 覆盖 provider 级 `role_mappings`（整段替换）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_mappings: Option<HashMap<String, String>>,
    /// 覆盖 provider 级 `anthropic_cache_control`（整段替换）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_cache_control: Option<bool>,
    /// 覆盖 provider 级 `timeout_secs`（标量直接替换）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// 覆盖 provider 级 `chunk_timeout_secs`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_timeout_secs: Option<u64>,
    /// 覆盖 provider 级 `ttfb_timeout_secs`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttfb_timeout_secs: Option<u64>,
}

/// Upstream LLM provider configuration: 凭据 + 协议 API 目录。
///
/// 字段归属三层（见计划「字段归属与覆盖语义」）：
/// - 全局（协议无关）：`api_keys`、`key_strategy`、`proxy_mode`、`enabled`、
///   `enabled_models`、`fallback`；
/// - 协议相关：provider 级写**缺省**，`apis.<key>` 同名字段整段覆盖；
/// - per-API 独有：`apis.<key>.base_url`（必填）、`models_endpoint`。
///
/// provider 级**没有** `base_url` —— 地址唯一来源是各 `apis` 节。
/// `apis` 为 `IndexMap`（TOML 表序 = 迭代序），未知 key 反序列化报错。
///
/// Provider-level priority is not part of routing selection. Target ordering is
/// controlled by `RouteTarget.priority` inside each route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// API keys for this provider（所有 API 共享的凭据池）.
    #[serde(default)]
    pub api_keys: Vec<String>,
    /// How to select among multiple `api_keys`.
    #[serde(default, skip_serializing_if = "is_default_key_strategy")]
    pub key_strategy: KeyStrategy,
    /// Credential mode: `static` (default, uses `api_keys`) or `oauth`
    /// (uses the `[providers.<name>.oauth]` section). Unknown values fail
    /// parsing — no silent fallback.
    #[serde(default, skip_serializing_if = "is_default_auth_mode")]
    pub auth_mode: AuthMode,
    /// OAuth client-credentials settings; required when `auth_mode =
    /// "oauth"`. Table — declared before `apis` so TOML serialization
    /// keeps tables after scalars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthConfig>,
    /// How this provider resolves the upstream HTTP proxy.
    ///
    /// - `inherit` (default): use the gateway-level `proxy_url` if one is
    ///   configured, otherwise connect directly.
    /// - `enabled`: force using the gateway-level `proxy_url`; a
    ///   configuration error if no top-level `proxy_url` is set.
    /// - `disabled`: force a direct connection, ignoring any gateway-level
    ///   proxy.
    #[serde(default, skip_serializing_if = "is_default_proxy_mode")]
    pub proxy_mode: ProxyMode,
    /// Whether this provider is enabled. When `false`, the provider is skipped
    /// during routing and shown as disabled in the admin UI.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// List of model IDs enabled for this provider.
    /// When non-empty, only these models may be used with this provider.
    /// When empty (default), all models are allowed (backward compatible).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enabled_models: Vec<String>,
    /// 降级协议（translated 路径的编码目标）。
    /// - `apis` 恰好一个协议 key → 可省略（自动取之）；
    /// - 多个协议 key → 必填（不静默猜测），值必须 ∈ apis 协议 key 集。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<ApiKind>,
    /// Timeout in seconds for the full round-trip of **non-streaming**
    /// requests to this provider (applied as a per-request
    /// `RequestBuilder::timeout`). Streaming requests are **not** bounded
    /// by this total: they use `ttfb_timeout_secs` for header arrival and
    /// `chunk_timeout_secs` for inter-chunk idle time, so a healthy
    /// long-lived stream can outlive `timeout_secs`.
    ///
    /// Provider 级缺省；`apis.<key>.timeout_secs` 整段覆盖。
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
    ///
    /// Provider 级缺省；`apis.<headers>` 声明时整段替换（map 不合并）。
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    /// Provider-wide request body field overrides merged into the upstream
    /// JSON body. Applied to **all** requests sent to this provider, forming
    /// the lowest-priority layer in the three-layer override chain:
    /// `body_overrides` < `model_overrides[real_model]` < `RouteTarget.extra`.
    /// Key-value pairs are inserted at the top level of the body, allowing you
    /// to set or override any field (e.g. max_tokens, temperature, or
    /// provider-specific parameters).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub body_overrides: HashMap<String, serde_json::Value>,
    /// Provider-wide request body fields to **remove** from the upstream JSON
    /// body before sending. Applied last, after `body_overrides`,
    /// `model_overrides[real_model]`, and `RouteTarget.extra`, so a stripped
    /// field is guaranteed absent from the final body even if an earlier layer
    /// inserted it. Use this for fields the upstream rejects with strict schema
    /// validation (e.g. pydantic `extra=forbid`), such as vLLM-specific
    /// `chat_template_kwargs` when forwarding to an OpenAI-compatible provider.
    ///
    /// TOML example:
    /// ```toml
    /// strip_fields = ["chat_template_kwargs"]
    /// ```
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub strip_fields: Vec<String>,
    /// Per-model body field strip list, keyed by `real_model` name (matched against final `body.model`).
    /// Applied alongside provider-level `strip_fields` as a union of field names
    /// to strip after all overrides are applied.
    ///
    /// TOML example:
    /// ```toml
    /// model_strip_fields = { "glm-5.2" = ["chat_template_kwargs"] }
    /// ```
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub model_strip_fields: HashMap<String, Vec<String>>,
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
    /// Whether to inject `cache_control: {"type": "ephemeral"}` breakpoints
    /// into Anthropic Messages requests（仅当实际使用协议为
    /// `anthropic_messages` 时生效；`apis.anthropic_messages` 可覆盖）。
    #[serde(default = "default_true", skip_serializing_if = "is_true_bool")]
    pub anthropic_cache_control: bool,
    /// 协议 API 目录：协议集 = 此表的**协议** key 集（`image` 不计入）。
    ///
    /// 每节携带自己的 `base_url`（地址唯一来源）与可选覆盖字段。
    /// 声明在最后：TOML 表必须排在标量之后，create 路径的
    /// `toml_edit::ser::to_document` 依赖此顺序。
    #[serde(default)]
    pub apis: IndexMap<ApiKind, ApiConfig>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            api_keys: Vec::new(),
            key_strategy: KeyStrategy::default(),
            auth_mode: AuthMode::default(),
            oauth: None,
            proxy_mode: ProxyMode::default(),
            enabled: default_true(),
            enabled_models: Vec::new(),
            fallback: None,
            timeout_secs: default_timeout_secs(),
            chunk_timeout_secs: None,
            ttfb_timeout_secs: None,
            headers: HashMap::new(),
            body_overrides: HashMap::new(),
            strip_fields: Vec::new(),
            model_strip_fields: HashMap::new(),
            model_overrides: HashMap::new(),
            role_mappings: HashMap::new(),
            anthropic_cache_control: default_true(),
            // 无默认 base_url —— apis 为空，须显式配置至少一个协议 API。
            apis: IndexMap::new(),
        }
    }
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
    /// Whether to retry when an upstream returns an empty response (zero
    /// `completion_tokens` with no content in `choices`). Defaults to `true`.
    /// Set to `false` to disable empty-response detection entirely (the raw
    /// upstream response is forwarded as-is).
    #[serde(default = "default_retry_on_empty_response")]
    pub retry_on_empty_response: bool,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            retries: default_retries(),
            base_ms: default_backoff_base_ms(),
            max_ms: default_backoff_max_ms(),
            jitter: default_backoff_jitter(),
            retry_on_empty_response: default_retry_on_empty_response(),
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
pub fn default_retry_on_empty_response() -> bool {
    true
}

/// Circuit-breaker cooldown durations by error category.
///
/// Each field specifies how long (in seconds) a route target stays open
/// after its retries are exhausted. The mapping from upstream error to
/// category is performed by [`sag_core::error::CircuitHint`].
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
fn default_sticky_failover_reset_secs() -> u64 {
    7200
}
fn default_true() -> bool {
    true
}
fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}
fn is_default_key_strategy(v: &KeyStrategy) -> bool {
    *v == KeyStrategy::default()
}
fn is_default_auth_mode(v: &AuthMode) -> bool {
    *v == AuthMode::default()
}
fn is_default_proxy_mode(v: &ProxyMode) -> bool {
    *v == ProxyMode::default()
}
fn is_default_strategy(v: &Strategy) -> bool {
    *v == Strategy::default()
}
/// `anthropic_cache_control` 的 skip 判定：默认值 `true` 不写盘，
/// 显式 `false`（opt-out）持久化。与 update 路径的写入规则一致。
fn is_true_bool(v: &bool) -> bool {
    *v
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

impl Strategy {
    /// Stable snake_case identifier matching the serde representation
    /// (`rename_all = "snake_case"`), used by the admin API to annotate
    /// route-target snapshots without a serde round-trip.
    pub fn as_str(&self) -> &'static str {
        match self {
            Strategy::StickyFailover => "sticky_failover",
            Strategy::FillFirst => "fill_first",
            Strategy::RoundRobin => "round_robin",
        }
    }
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteConfig {
    /// The model name as requested by the client.
    /// Use `"*"` as a wildcard to match any model not matched by an exact route.
    pub model_name: String,
    /// 请求分派管线（`chat`（缺省）/ `image`）。image 是不同物种：无协议
    /// 匹配语义、路径随模型变，须显式声明 `pipeline = "image"`。
    #[serde(default, skip_serializing_if = "is_default_pipeline")]
    pub pipeline: Pipeline,
    /// Real upstream path appended to `apis.image.base_url` —
    /// **only legal when `pipeline = "image"`**（chat 族 route 出现即 error）。
    /// Examples: `/v1beta/models/gemini:generateContent`, `/images/generations`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_endpoint: Option<String>,
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

/// Route 分派管线。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pipeline {
    /// Chat 协议族（chat_completions / responses / anthropic_messages）。
    #[default]
    Chat,
    /// 图像生成管线：上游 URL = `apis.image.base_url` + route.`api_endpoint`。
    Image,
}

impl Pipeline {
    /// Stable snake_case identifier matching the serde representation,
    /// used by the save path to write the value without a serde round-trip.
    pub fn as_str(&self) -> &'static str {
        match self {
            Pipeline::Chat => "chat",
            Pipeline::Image => "image",
        }
    }
}

fn is_default_pipeline(v: &Pipeline) -> bool {
    *v == Pipeline::default()
}

/// Resolve a request `model` against a route table: an exact `model_name`
/// match first, falling back to the `"*"` wildcard route. Returns the
/// matching route, or `None` when the model does not belong to any route.
///
/// Shared by the dispatch resolver, the stale-entry prune, and the admin
/// sticky-target snapshot so all three agree on which route a request model
/// belongs to.
pub fn resolve_model_route<'a>(routes: &'a [RouteConfig], model: &str) -> Option<&'a RouteConfig> {
    routes
        .iter()
        .find(|r| r.model_name == model)
        .or_else(|| routes.iter().find(|r| r.model_name == "*"))
}

/// A routing target: maps a logical model to a specific provider + real model name.
///
/// Multiple targets in a route are grouped by priority and tried based on the
/// route's `strategy`. Targets with the same priority are shuffled once then
/// cached when strategy is `FillFirst`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
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
    /// Applied after provider-level `body_overrides` and
    /// `model_overrides[real_model]`, so target-level keys take
    /// the highest precedence in the three-layer override chain.
    ///
    /// Serialized as a single nested inline table (`extra = { ... }`)
    /// within the target's inline table, e.g.
    /// `{ provider = "openai", extra = { temperature = 0.7 } }`. This
    /// keeps target-level extras clearly grouped, avoids namespace
    /// collisions with future `RouteTarget` fields, and round-trips with
    /// the on-disk key order preserved.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, serde_json::Value>,
    /// Per-target capability overrides (fixed key set; unknown keys fail
    /// `GatewayConfig::validate`). Applied on top of the capability table
    /// derived from the actually-used protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<CapabilitiesOverride>,
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

[providers.openai.apis.chat_completions]
base_url = "https://api.openai.com/v1"

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
        assert_eq!(openai.apis.len(), 1);
        assert_eq!(
            openai.apis[&ApiKind::ChatCompletions].base_url,
            "https://api.openai.com/v1"
        );
        assert_eq!(
            openai.fallback, None,
            "single protocol key auto-fills fallback"
        );
        assert_eq!(openai.timeout_secs, 600);
        assert_eq!(config.backoff.retries, 2);

        assert_eq!(config.routes.len(), 1);
        let route = &config.routes[0];
        assert_eq!(route.model_name, "gpt-4o");
        assert_eq!(route.pipeline, Pipeline::Chat);
        assert_eq!(route.api_endpoint, None, "chat route has no api_endpoint");
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
apis = { chat_completions = { base_url = "https://api.openai.com/v1" } }
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
apis = { chat_completions = { base_url = "https://api.openai.com/v1" } }

[[routes]]
model_name = "gpt-4o"
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
timeout_secs = 60

[providers.anthropic.apis.anthropic_messages]
base_url = "https://api.anthropic.com/v1"
models_endpoint = "v1/models"

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
        let api = &p.apis[&ApiKind::AnthropicMessages];
        assert_eq!(api.base_url, "https://api.anthropic.com/v1");
        assert_eq!(api.models_endpoint.as_deref(), Some("v1/models"));
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

[providers.openai.apis.chat_completions]
base_url = "https://api.openai.com/v1"
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
apis = { chat_completions = { base_url = "https://custom.example.com/v1" } }
timeout_secs = 60
enabled = false
"#,
        )
        .unwrap();
        assert!(config.role_mappings.is_empty());
        assert_eq!(
            config.apis[&ApiKind::ChatCompletions].base_url,
            "https://custom.example.com/v1"
        );
        assert!(!config.enabled);
    }

    mod proxy_tests {
        use super::*;

        // ── default compatibility with legacy configs ──

        #[test]
        fn parse_legacy_config_without_proxy_fields() {
            // Old configs (no `proxy_url` / no `proxy_mode`) must keep
            // working: `proxy_url` defaults to `None`, provider
            // `proxy_mode` defaults to `Inherit`.
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.openai]
api_keys = ["sk-test"]
apis = { chat_completions = { base_url = "https://api.openai.com/v1" } }

[[routes]]
model_name = "gpt-4o"
targets = [{ provider = "openai" }]
"#,
            )
            .unwrap();
            assert_eq!(config.proxy_url, None);
            assert_eq!(
                config.providers.get("openai").unwrap().proxy_mode,
                ProxyMode::Inherit
            );
            assert!(config.validate().is_ok());
        }

        #[test]
        fn parse_proxy_url_and_proxy_mode() {
            let config: GatewayConfig = toml::from_str(
                r#"
proxy_url = "http://proxy.example.com:8080"
[server]
host = "127.0.0.1"
port = 8080

[providers.openai]
api_keys = ["sk-test"]
apis = { chat_completions = { base_url = "https://api.openai.com/v1" } }
proxy_mode = "disabled"
"#,
            )
            .unwrap();
            assert_eq!(
                config.proxy_url.as_deref(),
                Some("http://proxy.example.com:8080")
            );
            assert_eq!(
                config.providers.get("openai").unwrap().proxy_mode,
                ProxyMode::Disabled
            );
            assert!(config.validate().is_ok());
        }

        // ── ProxyMode serde ──

        #[test]
        fn proxy_mode_serde_roundtrip() {
            for (mode, name) in [
                (ProxyMode::Inherit, "inherit"),
                (ProxyMode::Enabled, "enabled"),
                (ProxyMode::Disabled, "disabled"),
            ] {
                assert_eq!(mode.as_str(), name);
                let reparsed: ProviderConfig = toml::from_str(&format!(
                    "api_keys = [\"sk-test\"]\nproxy_mode = \"{name}\""
                ))
                .unwrap();
                assert_eq!(reparsed.proxy_mode, mode);
            }
        }

        #[test]
        fn proxy_mode_defaults_to_inherit() {
            let config: ProviderConfig = toml::from_str(r#"api_keys = ["sk-test"]"#).unwrap();
            assert_eq!(config.proxy_mode, ProxyMode::Inherit);
        }

        #[test]
        fn proxy_mode_unknown_value_fails_parse() {
            // There is no lenient fallback for unknown enum values anywhere
            // in the loader (same as `strategy` / `key_strategy`): an
            // unknown `proxy_mode` value is a hard
            // parse error, never silently coerced to `Inherit`. This is the
            // established strict-value-validation pattern the loader relies
            // on; a forward-compatible *field* is tolerated via
            // `serde_ignored` (unknown keys warn), but an unknown *value*
            // for a known enum field is rejected.
            let result: Result<ProviderConfig, _> = toml::from_str(
                r#"
api_keys = ["sk-test"]
proxy_mode = "auto"
"#,
            );
            assert!(
                result.is_err(),
                "unknown proxy_mode value must fail parsing, got: {result:?}"
            );
        }

        // ── normalize_proxy_url ──

        #[test]
        fn normalize_proxy_url_none_and_blank_are_unset() {
            assert_eq!(normalize_proxy_url(None), Ok(None));
            assert_eq!(normalize_proxy_url(Some("")), Ok(None));
            assert_eq!(normalize_proxy_url(Some("   ")), Ok(None));
            assert_eq!(normalize_proxy_url(Some("\t\n")), Ok(None));
        }

        #[test]
        fn normalize_proxy_url_accepts_http_and_https() {
            let http = normalize_proxy_url(Some("http://proxy.example.com:8080"))
                .unwrap()
                .unwrap();
            assert_eq!(http.scheme, "http");
            assert_eq!(http.host, "proxy.example.com");
            assert_eq!(http.port, Some(8080));
            assert_eq!(http.to_connect_url(), "http://proxy.example.com:8080");

            let https = normalize_proxy_url(Some("https://proxy.example.com"))
                .unwrap()
                .unwrap();
            assert_eq!(https.scheme, "https");
            assert_eq!(https.host, "proxy.example.com");
            assert_eq!(https.port, None);
            assert_eq!(https.to_connect_url(), "https://proxy.example.com");
        }

        #[test]
        fn normalize_proxy_url_allows_nonstandard_port_and_ipv6() {
            let v6 = normalize_proxy_url(Some("http://[::1]:3128"))
                .unwrap()
                .unwrap();
            assert_eq!(v6.host, "::1");
            assert_eq!(v6.port, Some(3128));
            // to_connect_url re-brackets IPv6 hosts so the transport layer
            // gets a usable URL.
            assert_eq!(v6.to_connect_url(), "http://[::1]:3128");

            let v4 = normalize_proxy_url(Some("http://192.168.1.10:8080"))
                .unwrap()
                .unwrap();
            assert_eq!(v4.host, "192.168.1.10");
            assert_eq!(v4.port, Some(8080));
        }

        #[test]
        fn to_connect_url_avoids_double_bracketing_ipv6() {
            // Directly-constructed `ProxyConfig`s may already carry an
            // IPv6 host with brackets; `to_connect_url` must not double
            // them (`[[::1]]`).
            let bracketed = ProxyConfig {
                scheme: "http".into(),
                host: "[::1]".into(),
                port: Some(3128),
            };
            assert_eq!(bracketed.to_connect_url(), "http://[::1]:3128");

            let plain = ProxyConfig {
                scheme: "http".into(),
                host: "::1".into(),
                port: None,
            };
            assert_eq!(plain.to_connect_url(), "http://[::1]");
        }

        #[test]
        fn normalize_proxy_url_rejects_invalid_scheme() {
            let err = normalize_proxy_url(Some("ftp://proxy.example.com:8080")).unwrap_err();
            assert!(err.contains("scheme"), "got: {err}");
            let err = normalize_proxy_url(Some("socks5://proxy.example.com:1080")).unwrap_err();
            assert!(err.contains("scheme"), "got: {err}");
        }

        #[test]
        fn normalize_proxy_url_rejects_missing_host() {
            // `url` rejects empty hosts for http(s) at parse time (e.g.
            // "empty host" / "invalid IPv6 address" errors).
            let err = normalize_proxy_url(Some("http://")).unwrap_err();
            assert!(err.contains("host"), "got: {err}");
            let err = normalize_proxy_url(Some("http://:8080")).unwrap_err();
            assert!(err.contains("host"), "got: {err}");
        }

        #[test]
        fn normalize_proxy_url_rejects_userinfo() {
            let err =
                normalize_proxy_url(Some("http://user:pass@proxy.example.com:8080")).unwrap_err();
            assert!(err.contains("userinfo"), "got: {err}");
        }

        #[test]
        fn normalize_proxy_url_trims_surrounding_whitespace() {
            let p = normalize_proxy_url(Some("  https://proxy.example.com  "))
                .unwrap()
                .unwrap();
            assert_eq!(p.host, "proxy.example.com");
        }

        // ── resolve_effective_proxy ──

        fn global(url: &str) -> Option<ProxyConfig> {
            normalize_proxy_url(Some(url)).unwrap()
        }

        #[test]
        fn effective_proxy_inherit_uses_global_when_present() {
            let g = global("http://proxy.example.com:8080");
            assert_eq!(
                resolve_effective_proxy(ProxyMode::Inherit, g.as_ref()),
                Ok(g.clone())
            );
        }

        #[test]
        fn effective_proxy_inherit_without_global_is_direct() {
            assert_eq!(resolve_effective_proxy(ProxyMode::Inherit, None), Ok(None));
        }

        #[test]
        fn effective_proxy_enabled_requires_global() {
            let g = global("http://proxy.example.com:8080");
            assert_eq!(
                resolve_effective_proxy(ProxyMode::Enabled, g.as_ref()),
                Ok(g.clone())
            );
            let err = resolve_effective_proxy(ProxyMode::Enabled, None).unwrap_err();
            assert!(err.contains("proxy_url"), "got: {err}");
        }

        #[test]
        fn effective_proxy_disabled_always_direct() {
            let g = global("http://proxy.example.com:8080");
            assert_eq!(
                resolve_effective_proxy(ProxyMode::Disabled, g.as_ref()),
                Ok(None)
            );
            assert_eq!(resolve_effective_proxy(ProxyMode::Disabled, None), Ok(None));
        }

        #[test]
        fn provider_config_default_uses_field_defaults() {
            // `Default` must agree with the serde load-time defaults so a
            // `ProviderConfig::default()` is exactly what a minimal TOML
            // block deserializes to.
            let p = ProviderConfig::default();
            assert!(p.api_keys.is_empty());
            assert_eq!(p.key_strategy, KeyStrategy::default());
            assert_eq!(p.proxy_mode, ProxyMode::default());
            assert_eq!(p.timeout_secs, default_timeout_secs());
            assert!(p.chunk_timeout_secs.is_none());
            assert!(p.ttfb_timeout_secs.is_none());
            assert!(p.headers.is_empty());
            assert!(p.body_overrides.is_empty());
            assert!(p.strip_fields.is_empty());
            assert!(p.model_strip_fields.is_empty());
            assert!(p.model_overrides.is_empty());
            assert!(p.role_mappings.is_empty());
            assert!(p.enabled);
            assert!(p.enabled_models.is_empty());
            assert!(p.anthropic_cache_control);
            assert!(p.fallback.is_none());
            assert!(p.apis.is_empty(), "no default base_url — apis starts empty");
        }

        #[test]
        fn normalized_global_proxy_matches_validate_semantics() {
            let mut config: GatewayConfig = toml::from_str(
                r#"
[server]
[providers.openai]
api_keys = ["sk-test"]
"#,
            )
            .unwrap();
            assert_eq!(config.normalized_global_proxy().unwrap(), None);

            config.proxy_url = Some("   ".into());
            assert_eq!(config.normalized_global_proxy().unwrap(), None);

            config.proxy_url = Some("http://proxy.example.com:8080".into());
            let g = config.normalized_global_proxy().unwrap().unwrap();
            assert_eq!(g.host, "proxy.example.com");
            assert_eq!(g.port, Some(8080));

            config.proxy_url = Some("ftp://proxy.example.com".into());
            assert!(config.normalized_global_proxy().is_err());
        }
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
apis = { chat_completions = { base_url = "https://api.openai.com/v1" } }

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
        fn rejects_proxy_mode_enabled_without_global_proxy_url() {
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.openai]
api_keys = ["sk-test"]
proxy_mode = "enabled"

[[routes]]
model_name = "gpt-4o"
targets = [{ provider = "openai" }]
"#,
            )
            .unwrap();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("proxy_mode='enabled'") && e.contains("proxy_url")),
                "expected enabled-without-global-proxy error, got {err:?}"
            );
        }

        #[test]
        fn accepts_proxy_mode_enabled_with_global_proxy_url() {
            let config: GatewayConfig = toml::from_str(
                r#"
proxy_url = "http://proxy.example.com:8080"
[server]
host = "127.0.0.1"
port = 8080

[providers.openai]
api_keys = ["sk-test"]
apis = { chat_completions = { base_url = "https://api.openai.com/v1" } }
proxy_mode = "enabled"
"#,
            )
            .unwrap();
            assert!(config.validate().is_ok());
        }

        #[test]
        fn rejects_invalid_top_level_proxy_url() {
            let config: GatewayConfig = toml::from_str(
                r#"
proxy_url = "ftp://proxy.example.com"
[server]
host = "127.0.0.1"
port = 8080

[providers.openai]
api_keys = ["sk-test"]
"#,
            )
            .unwrap();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("proxy_url") && e.contains("scheme")),
                "expected proxy_url scheme error, got {err:?}"
            );
        }

        #[test]
        fn blank_proxy_url_normalizes_to_none_and_passes() {
            // A whitespace-only `proxy_url` must be treated as "not
            // configured" rather than a validation error.
            let mut config = valid_config();
            config.proxy_url = Some("   ".into());
            assert!(config.validate().is_ok());
        }

        #[test]
        fn rejects_proxy_mode_enabled_with_blank_proxy_url() {
            // `Some("")` / `Some("   ")` normalize to "no global proxy",
            // so `enabled` must be rejected just like a literal `None`.
            for blank in ["", "   ", "\t\n"] {
                let mut config = valid_config();
                config.providers.get_mut("openai").unwrap().proxy_mode = ProxyMode::Enabled;
                config.proxy_url = Some(blank.into());
                let err = config.validate().unwrap_err();
                assert!(
                    err.iter()
                        .any(|e| e.contains("proxy_mode='enabled'") && e.contains("proxy_url")),
                    "blank proxy_url {blank:?} + enabled must be rejected, got {err:?}"
                );
            }
        }

        #[test]
        fn rejects_proxy_mode_enabled_with_invalid_proxy_url() {
            // An invalid `proxy_url` must (a) still be reported as a
            // `proxy_url` error and (b) not count as an available global
            // proxy for `enabled` providers.
            let mut config = valid_config();
            config.providers.get_mut("openai").unwrap().proxy_mode = ProxyMode::Enabled;
            config.proxy_url = Some("ftp://proxy.example.com".into());
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("proxy_url") && e.contains("scheme")),
                "expected proxy_url scheme error, got {err:?}"
            );
            assert!(
                err.iter()
                    .any(|e| e.contains("proxy_mode='enabled'") && e.contains("proxy_url")),
                "enabled provider must not see the invalid URL as a usable proxy, got {err:?}"
            );
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

        // ── auth_mode / oauth ───────────────────────────────────────

        #[test]
        fn rejects_unknown_auth_mode_value() {
            // Unknown typed-enum values fail at parse time (loader strictness).
            let result = toml::from_str::<GatewayConfig>(
                r#"
[server]
[providers.openai]
auth_mode = "jwt"
api_keys = ["sk-test"]
apis = { chat_completions = { base_url = "https://api.openai.com/v1" } }
"#,
            );
            assert!(
                result.is_err(),
                "unknown auth_mode must fail deserialization"
            );
        }

        #[test]
        fn legacy_config_without_auth_mode_parses_as_static() {
            let config = valid_config();
            let p = config.providers.get("openai").unwrap();
            assert_eq!(p.auth_mode, AuthMode::Static);
            assert!(p.oauth.is_none());
            assert!(config.validate().is_ok());
        }

        #[test]
        fn accepts_oauth_provider_with_complete_section() {
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
[providers.azure]
auth_mode = "oauth"
api_keys = []
apis = { chat_completions = { base_url = "https://my-resource.openai.azure.com" } }

[providers.azure.oauth]
token_url = "https://login.microsoftonline.com/t/oauth2/v2.0/token"
client_id = "cid"
client_secret = "sec"
scope = "https://cognitiveservices.azure.com/.default"
"#,
            )
            .unwrap();
            assert!(config.validate().is_ok(), "{:?}", config.validate());
            let p = config.providers.get("azure").unwrap();
            assert_eq!(p.auth_mode, AuthMode::OAuth);
            assert!(p.api_keys.is_empty(), "oauth allows empty api_keys");
            let oauth = p.oauth.as_ref().expect("oauth section");
            assert_eq!(oauth.client_id, "cid");
            assert_eq!(
                oauth.scope.as_deref(),
                Some("https://cognitiveservices.azure.com/.default")
            );
        }

        #[test]
        fn rejects_oauth_missing_section() {
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
[providers.azure]
auth_mode = "oauth"
api_keys = []
apis = { chat_completions = { base_url = "https://x.openai.azure.com" } }
"#,
            )
            .unwrap();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("requires an [providers.azure.oauth]")),
                "expected missing oauth section error, got {err:?}"
            );
        }

        #[test]
        fn rejects_oauth_missing_required_fields() {
            for field in ["token_url", "client_id", "client_secret"] {
                let (url, cid, secret) = match field {
                    "token_url" => ("", "cid", "sec"),
                    "client_id" => ("https://login.example.com/token", "", "sec"),
                    _ => ("https://login.example.com/token", "cid", ""),
                };
                let config: GatewayConfig = toml::from_str(
                    format!(
                        r#"
[server]
[providers.azure]
auth_mode = "oauth"
api_keys = []
apis = {{ chat_completions = {{ base_url = "https://x.openai.azure.com" }} }}

[providers.azure.oauth]
token_url = "{url}"
client_id = "{cid}"
client_secret = "{secret}"
"#
                    )
                    .as_str(),
                )
                .unwrap();
                let err = config.validate().unwrap_err();
                assert!(
                    err.iter()
                        .any(|e| e.contains(&format!("oauth.{field} must not be empty"))),
                    "expected empty {field} error, got {err:?}"
                );
            }
        }

        #[test]
        fn rejects_anthropic_messages_with_oauth() {
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
[providers.anti]
auth_mode = "oauth"
api_keys = []
apis = { anthropic_messages = { base_url = "https://api.anthropic.com" }, chat_completions = { base_url = "https://api.openai.com/v1" } }
fallback = "chat_completions"

[providers.anti.oauth]
token_url = "https://login.example.com/token"
client_id = "cid"
client_secret = "sec"
"#,
            )
            .unwrap();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("anthropic_messages") && e.contains("Phase 1")),
                "expected anthropic+oauth rejection, got {err:?}"
            );
        }

        #[test]
        fn oauth_with_round_robin_still_validates() {
            // warning-only: round_robin + oauth is a single-client no-op.
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
[providers.azure]
auth_mode = "oauth"
key_strategy = "round_robin"
api_keys = []
apis = { chat_completions = { base_url = "https://x.openai.azure.com" } }

[providers.azure.oauth]
token_url = "https://login.example.com/token"
client_id = "cid"
client_secret = "sec"
"#,
            )
            .unwrap();
            assert!(config.validate().is_ok(), "{:?}", config.validate());
        }

        #[test]
        fn static_with_oauth_section_still_validates() {
            // warning-only: oauth section ignored under static.
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
[providers.openai]
api_keys = ["sk-test"]
apis = { chat_completions = { base_url = "https://api.openai.com/v1" } }

[providers.openai.oauth]
token_url = "https://login.example.com/token"
client_id = "cid"
client_secret = "sec"
"#,
            )
            .unwrap();
            assert!(config.validate().is_ok(), "{:?}", config.validate());
            assert_eq!(
                config.providers.get("openai").unwrap().auth_mode,
                AuthMode::Static
            );
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
            config
                .providers
                .get_mut("openai")
                .unwrap()
                .apis
                .get_mut(&ApiKind::ChatCompletions)
                .unwrap()
                .base_url = "not-a-url".into();
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
apis = { chat_completions = { base_url = "https://api.openai.com/v1" }, image = { base_url = "https://api.openai.com/v1" } }

[[routes]]
model_name = "dall-e-3"
pipeline = "image"
api_endpoint = "images/generations"
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
apis = { chat_completions = { base_url = "https://api.openai.com/v1" } }
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
        fn pipeline_and_api_endpoint_serde_rules() {
            // `pipeline` defaults to `chat` and is skipped when default;
            // `api_endpoint` is `None` unless explicitly declared (image).
            let route: RouteConfig = toml::from_str(
                r#"model_name = "gpt-4o"
targets = []"#,
            )
            .unwrap();
            assert_eq!(route.pipeline, Pipeline::Chat);
            assert!(route.api_endpoint.is_none());
            assert!(is_default_pipeline(&route.pipeline));

            let image: RouteConfig = toml::from_str(
                r#"model_name = "img"
pipeline = "image"
api_endpoint = "/images/generations"
targets = []"#,
            )
            .unwrap();
            assert_eq!(image.pipeline, Pipeline::Image);
            assert_eq!(image.api_endpoint.as_deref(), Some("/images/generations"));
            assert!(!is_default_pipeline(&image.pipeline));

            // 未知 pipeline 枚举值 → 反序列化报错（枚举严格）。
            let bad: Result<RouteConfig, _> = toml::from_str(
                r#"model_name = "x"
pipeline = "video"
targets = []"#,
            );
            assert!(bad.is_err(), "unknown pipeline value must fail parse");
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
                    capabilities: None,
                },
                RouteTarget {
                    provider: "openai".into(),
                    real_model: Some("gpt-4".into()),
                    priority: 5,
                    retries: None,
                    timeout_secs: None,
                    extra: HashMap::new(),
                    capabilities: None,
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
                    capabilities: None,
                },
                RouteTarget {
                    provider: "openai".into(),
                    real_model: Some("gpt-3.5-turbo".into()),
                    priority: 5,
                    retries: None,
                    timeout_secs: None,
                    extra: HashMap::new(),
                    capabilities: None,
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
            config
                .providers
                .get_mut("openai")
                .unwrap()
                .apis
                .get_mut(&ApiKind::ChatCompletions)
                .unwrap()
                .base_url = "ftp://api.openai.com/v1".into();
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
apis = { chat_completions = { base_url = "https://api.openai.com/v1" } }

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

        // ── apis / fallback / pipeline 校验 ──

        #[test]
        fn rejects_empty_apis() {
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.openai]
api_keys = ["sk-test"]

[[routes]]
model_name = "gpt-4o"
targets = [{ provider = "openai" }]
"#,
            )
            .unwrap();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter().any(|e| e.contains("apis must not be empty")),
                "expected empty-apis error, got {err:?}"
            );
        }

        #[test]
        fn rejects_image_only_apis() {
            // image 不是协议：仅有 image 的 apis 同样报错。
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.openai]
api_keys = ["sk-test"]
apis = { image = { base_url = "https://api.openai.com/v1" } }

[[routes]]
model_name = "gpt-4o"
targets = [{ provider = "openai" }]
"#,
            )
            .unwrap();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter().any(|e| e.contains("at least one protocol key")),
                "image-only apis must be rejected, got {err:?}"
            );
        }

        #[test]
        fn single_protocol_key_without_fallback_validates() {
            // 单协议 key：未写 fallback → 自动取之，校验通过。
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.openai]
api_keys = ["sk-test"]
apis = { chat_completions = { base_url = "https://api.openai.com/v1" } }

[[routes]]
model_name = "gpt-4o"
targets = [{ provider = "openai" }]
"#,
            )
            .unwrap();
            assert!(
                config.validate().is_ok(),
                "single protocol key without fallback must validate"
            );
            assert_eq!(
                config.providers["openai"].fallback, None,
                "fallback stays None in memory (auto-fill happens at consumption)"
            );
        }

        #[test]
        fn rejects_multiple_protocol_keys_without_fallback() {
            // 多协议 key 未写 fallback → 报错，建议值 = 第一个协议 key（表序）。
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.openai]
api_keys = ["sk-test"]
apis = { responses = { base_url = "https://api.openai.com/v1" }, chat_completions = { base_url = "https://api.openai.com/v1" } }

[[routes]]
model_name = "gpt-4o"
targets = [{ provider = "openai" }]
"#,
            )
            .unwrap();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter().any(|e| e.contains("fallback is required")
                    && e.contains("suggested value: fallback = \"responses\"")),
                "expected multi-key-missing-fallback error with first-key suggestion, got {err:?}"
            );
        }

        #[test]
        fn rejects_fallback_not_in_apis_protocol_keys() {
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.openai]
api_keys = ["sk-test"]
fallback = "responses"
apis = { chat_completions = { base_url = "https://api.openai.com/v1" } }

[[routes]]
model_name = "gpt-4o"
targets = [{ provider = "openai" }]
"#,
            )
            .unwrap();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter().any(|e| e.contains("fallback='responses'")
                    && e.contains("must be one of the configured apis protocol keys")),
                "fallback outside the apis protocol key set must be rejected, got {err:?}"
            );
        }

        #[test]
        fn rejects_fallback_image() {
            // image 不是协议 key：fallback = "image" 必须报错。
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.openai]
api_keys = ["sk-test"]
fallback = "image"
apis = { chat_completions = { base_url = "https://api.openai.com/v1" }, image = { base_url = "https://api.openai.com/v1" } }

[[routes]]
model_name = "gpt-4o"
targets = [{ provider = "openai" }]
"#,
            )
            .unwrap();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter().any(|e| e.contains("fallback='image'")),
                "image fallback must be rejected, got {err:?}"
            );
        }

        #[test]
        fn rejects_unknown_apis_key() {
            // 未知 apis key 在反序列化层报错（枚举严格），非 validate 阶段。
            let result = toml::from_str::<GatewayConfig>(
                r#"
[server]
[providers.openai]
api_keys = ["sk-test"]
apis = { completions = { base_url = "https://api.openai.com/v1" } }
"#,
            );
            assert!(result.is_err(), "unknown apis key must fail parsing");
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("completions") || err.contains("unknown"),
                "got: {err}"
            );
        }

        #[test]
        fn rejects_unknown_fallback_value() {
            let result = toml::from_str::<GatewayConfig>(
                r#"
[server]
[providers.openai]
api_keys = ["sk-test"]
fallback = "nope"
apis = { chat_completions = { base_url = "https://api.openai.com/v1" } }
"#,
            );
            assert!(result.is_err(), "unknown fallback value must fail parsing");
        }

        #[test]
        fn rejects_chat_route_with_api_endpoint() {
            // chat 族 route 出现 api_endpoint 即 error。
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
[providers.openai]
api_keys = ["sk-test"]
apis = { chat_completions = { base_url = "https://api.openai.com/v1" } }

[[routes]]
model_name = "gpt-4o"
api_endpoint = "/chat/completions"
targets = [{ provider = "openai" }]
"#,
            )
            .unwrap();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("only valid when pipeline = \"image\"")),
                "chat route with api_endpoint must error, got {err:?}"
            );
        }

        #[test]
        fn image_route_requires_api_endpoint() {
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
[providers.openai]
api_keys = ["sk-test"]
apis = { chat_completions = { base_url = "https://api.openai.com/v1" }, image = { base_url = "https://api.openai.com/v1" } }

[[routes]]
model_name = "dall-e-3"
pipeline = "image"
targets = [{ provider = "openai" }]
"#,
            )
            .unwrap();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("pipeline = \"image\" requires api_endpoint")),
                "image route without api_endpoint must error, got {err:?}"
            );
        }

        #[test]
        fn accepts_image_route_with_pipeline_and_endpoint() {
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
[providers.openai]
api_keys = ["sk-test"]
apis = { chat_completions = { base_url = "https://api.openai.com/v1" }, image = { base_url = "https://api.openai.com/v1" } }

[[routes]]
model_name = "dall-e-3"
pipeline = "image"
api_endpoint = "/images/generations"
targets = [{ provider = "openai" }]
"#,
            )
            .unwrap();
            assert!(
                config.validate().is_ok(),
                "image route with endpoint must validate"
            );
            assert_eq!(config.routes[0].pipeline, Pipeline::Image);
            assert_eq!(
                config.routes[0].api_endpoint.as_deref(),
                Some("/images/generations")
            );
        }

        #[test]
        fn rejects_unknown_capabilities_key() {
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.openai]
api_keys = ["sk-test"]
apis = { chat_completions = { base_url = "https://api.openai.com/v1" } }

[[routes]]
model_name = "gpt-4o"
targets = [
    { provider = "openai", capabilities = { streaming = true, teleport = true } },
]
"#,
            )
            .unwrap();
            let err = config.validate().unwrap_err();
            assert!(
                err.iter()
                    .any(|e| e.contains("capabilities") && e.contains("teleport")),
                "expected unknown capability key error, got {err:?}"
            );
        }

        #[test]
        fn accepts_known_capabilities_keys() {
            let config: GatewayConfig = toml::from_str(
                r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.openai]
api_keys = ["sk-test"]
apis = { chat_completions = { base_url = "https://api.openai.com/v1" } }

[[routes]]
model_name = "gpt-4o"
targets = [
    { provider = "openai", capabilities = { streaming = true, tools = false, reasoning = true } },
]
"#,
            )
            .unwrap();
            assert!(config.validate().is_ok());
            let caps = config.routes[0].targets[0].capabilities.as_ref().unwrap();
            assert_eq!(caps.streaming, Some(true));
            assert_eq!(caps.tools, Some(false));
            assert_eq!(caps.reasoning, Some(true));
            assert!(caps.unknown.is_empty());
        }

        #[test]
        fn capabilities_roundtrip_through_toml() {
            let toml_str = r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.openai]
api_keys = ["sk-test"]
apis = { chat_completions = { base_url = "https://api.openai.com/v1" }, responses = { base_url = "https://api.openai.com/v1" } }
fallback = "responses"

[[routes]]
model_name = "gpt-4o"
targets = [
    { provider = "openai", capabilities = { streaming = true, stateful = false } },
]
"#;
            let config: GatewayConfig = toml::from_str(toml_str).unwrap();
            assert!(config.validate().is_ok());
            assert_eq!(
                config.providers["openai"].fallback,
                Some(ApiKind::Responses)
            );
            let caps = config.routes[0].targets[0].capabilities.as_ref().unwrap();
            assert_eq!(caps.streaming, Some(true));
            assert_eq!(caps.stateful, Some(false));
        }
    }

    #[test]
    fn parse_fallback_on_provider() {
        let toml_str = r#"
[server]
host = "127.0.0.1"
port = 8080

[providers.openai]
api_keys = ["sk-test"]
fallback = "responses"
apis = { chat_completions = { base_url = "https://api.openai.com/v1" }, responses = { base_url = "https://api.openai.com/v1" } }

[[routes]]
model_name = "gpt-4o"
targets = [
    { provider = "openai", real_model = "gpt-4o" },
]
"#;
        let config: GatewayConfig = toml::from_str(toml_str).unwrap();
        let p = config.providers.get("openai").unwrap();
        assert_eq!(p.fallback, Some(ApiKind::Responses));
        // apis 保持 TOML 表序（IndexMap）。
        let keys: Vec<ApiKind> = p.apis.keys().copied().collect();
        assert_eq!(keys, vec![ApiKind::ChatCompletions, ApiKind::Responses]);
    }

    #[test]
    fn parse_fallback_rejects_unknown_value() {
        let toml_str = r#"
[server]
[providers.openai]
api_keys = ["sk-test"]
fallback = "nope"
apis = { chat_completions = { base_url = "https://api.openai.com/v1" } }
"#;
        let result = toml::from_str::<GatewayConfig>(toml_str);
        assert!(result.is_err(), "unknown fallback value must fail");
    }

    #[test]
    fn fallback_defaults_to_none_when_omitted() {
        let config: GatewayConfig = toml::from_str(minimal_toml()).unwrap();
        let p = config.providers.get("openai").unwrap();
        assert_eq!(p.fallback, None);
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
            let mut apis = IndexMap::new();
            apis.insert(
                ApiKind::ChatCompletions,
                ApiConfig {
                    base_url: "https://example.com".into(),
                    ..Default::default()
                },
            );
            (
                name.into(),
                ProviderConfig {
                    api_keys: vec![format!("sk-{name}")],
                    timeout_secs: 120,
                    anthropic_cache_control: false,
                    apis,
                    ..Default::default()
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
                capabilities: None,
            }
        }

        /// Helper: a `RouteConfig` with a model name and targets.
        fn route(model: &str, targets: Vec<RouteTarget>) -> RouteConfig {
            RouteConfig {
                model_name: model.into(),
                pipeline: Pipeline::Chat,
                api_endpoint: None,
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

        #[test]
        fn strategy_as_str_matches_serde_rename() {
            // Every Strategy variant's as_str() must match its serde
            // snake_case representation. If a new variant is added to
            // Strategy without updating as_str(), this test will fail.
            let cases = [
                (Strategy::StickyFailover, "sticky_failover"),
                (Strategy::FillFirst, "fill_first"),
                (Strategy::RoundRobin, "round_robin"),
            ];
            for (variant, expected) in &cases {
                assert_eq!(variant.as_str(), *expected, "literal mismatch");
                // Also verify against the serde rename to catch drift.
                let serde_name = serde_json::to_value(variant)
                    .unwrap()
                    .as_str()
                    .unwrap_or("INVALID_SERDE_OUTPUT")
                    .to_string();
                assert_eq!(
                    variant.as_str(),
                    serde_name,
                    "as_str() mismatches serde rename for {}",
                    serde_name
                );
            }
        }
    }
}
