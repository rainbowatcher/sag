use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use reqwest::RequestBuilder;
use sag_config::{ApiConfig, KeyStrategy, ProviderConfig};
use sag_core::{ApiKind, AppError, ChatMessage, protocol_path};
use tracing::{debug, trace, warn};

use crate::{apply_body_overrides, apply_body_strip, apply_headers, provider_auth_headers};

/// Join an API base URL and a path with exactly one `/` between them,
/// regardless of leading/trailing slashes on either side.
fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    format!("{}/{}", base, path)
}

/// Shared state and helper methods for all provider adapters.
///
/// Each provider struct (e.g. `OpenAiProvider`, `AnthropicProvider`,
/// `CatalogProvider`) composes this as a field and delegates common
/// operations to it.
///
/// Per-protocol addresses live in `apis` (`ApiKind` → [`ApiConfig`]).
/// Protocol-related settings resolve through the `effective_*` helpers:
/// an `apis.<key>` field declared (`Some`) replaces the provider-level
/// default wholesale; `None` inherits it.
pub struct ProviderBase {
    /// Human-readable provider name (e.g. `"openai"`, `"anthropic"`).
    pub name: String,
    api_keys: Vec<String>,
    key_strategy: KeyStrategy,
    key_counter: AtomicUsize,
    /// Per-protocol API nodes (address + optional protocol-level overrides).
    apis: HashMap<ApiKind, ApiConfig>,
    /// Reusable HTTP client with **no** total request deadline. A healthy
    /// streaming request can outlive `config.timeout_secs`, so the client
    /// must not impose one. Non-streaming requests get their deadline via
    /// `RequestBuilder::timeout` in [`ProviderBase::send_json`].
    pub http_client: reqwest::Client,
    /// Provider-level default headers (fallback for `effective_headers`).
    headers: HashMap<String, String>,
    /// Provider-level default body overrides (fallback for
    /// `effective_body_overrides`).
    body_overrides: HashMap<String, serde_json::Value>,
    /// Top-level fields to strip from the upstream body before sending
    /// (provider-level default; `apis.<key>.strip_fields` replaces it).
    /// Applied after all override layers, guaranteeing absence in the
    /// final body.
    strip_fields: Vec<String>,
    /// Per-model strip fields keyed by real_model name (provider-level
    /// default; `apis.<key>.model_strip_fields` replaces it).
    model_strip_fields: HashMap<String, Vec<String>>,
    /// Provider-level default role mappings (fallback for
    /// `effective_role_mappings`).
    role_mappings: HashMap<String, String>,
    /// Provider-level default for Anthropic cache_control injection
    /// (`apis.anthropic_cache_control` overrides it).
    anthropic_cache_control: bool,
    /// Provider-level default: maximum idle time between stream chunks
    /// before the connection is terminated. `None` = wait indefinitely.
    chunk_timeout_secs: Option<u64>,
    /// Provider-level default: maximum time to wait for the upstream
    /// response headers (time-to-first-byte) on streaming requests.
    /// `None` = use the crate default. Only used by the streaming path;
    /// once headers arrive this no longer applies.
    ttfb_timeout_secs: Option<u64>,
    /// Fallback protocol (translated-lane encoding target), when the
    /// provider declares multiple protocol keys.
    fallback: Option<ApiKind>,
}

impl ProviderBase {
    /// Create a new provider base from configuration.
    ///
    /// Connects **directly**: the system proxy environment variables
    /// (`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`) are explicitly ignored.
    /// Use [`ProviderBase::new_with_proxy`] to route upstream traffic
    /// through an explicit proxy.
    pub fn new(name: String, config: &ProviderConfig) -> sag_core::Result<Self> {
        Self::new_with_proxy(name, config, None)
    }

    /// Create a new provider base from configuration with an explicit
    /// effective proxy.
    ///
    /// `proxy: None` → direct connection with the system proxy explicitly
    /// disabled. `Some(proxy)` → all upstream traffic is routed through
    /// that single proxy URL (e.g. resolved via
    /// `sag_config::resolve_effective_proxy` and reconstructed with
    /// `ProxyConfig::to_connect_url`); the system proxy environment
    /// variables are ignored either way.
    ///
    /// No client-level `.timeout(...)` is set: that would impose a total
    /// request deadline that truncates healthy long-lived streams.
    /// `timeout_secs` still applies to non-streaming requests via
    /// `RequestBuilder::timeout` in `send_json`, and streaming keeps its
    /// TTFB / chunk-idle semantics.
    pub fn new_with_proxy(
        name: String,
        config: &ProviderConfig,
        proxy: Option<&sag_config::ProxyConfig>,
    ) -> sag_core::Result<Self> {
        let http_client =
            sag_core::build_http_client(proxy.map(|p| p.to_connect_url()).as_deref(), None)
                .map_err(|e| AppError::Internal(format!("failed to build reqwest client: {e}")))?;

        if config.api_keys.is_empty() {
            return Err(AppError::BadRequest(format!(
                "provider '{}': api_keys is empty",
                name
            )));
        }
        let api_keys = config.api_keys.clone();

        Ok(Self {
            name,
            api_keys,
            key_strategy: config.key_strategy,
            key_counter: AtomicUsize::new(0),
            apis: config.apis.iter().map(|(k, v)| (*k, v.clone())).collect(),
            http_client,
            headers: config.headers.clone(),
            body_overrides: config.body_overrides.clone(),
            strip_fields: config.strip_fields.clone(),
            model_strip_fields: config.model_strip_fields.clone(),
            role_mappings: config.role_mappings.clone(),
            anthropic_cache_control: config.anthropic_cache_control,
            chunk_timeout_secs: config.chunk_timeout_secs,
            ttfb_timeout_secs: config.ttfb_timeout_secs,
            fallback: config.fallback,
        })
    }

    /// Select the current API key based on the configured strategy.
    pub fn current_key(&self) -> &str {
        match self.key_strategy {
            KeyStrategy::FillFirst => &self.api_keys[0],
            KeyStrategy::RoundRobin => {
                let idx = self.key_counter.fetch_add(1, Ordering::Relaxed) % self.api_keys.len();
                &self.api_keys[idx]
            }
        }
    }

    /// The configured fallback protocol (translated-lane encoding target).
    pub fn fallback(&self) -> Option<ApiKind> {
        self.fallback
    }

    /// Whether the provider declares an API node for `kind`.
    pub fn supports(&self, kind: ApiKind) -> bool {
        self.apis.contains_key(&kind)
    }

    /// The raw API node for `kind`, if configured.
    fn api(&self, kind: ApiKind) -> Option<&ApiConfig> {
        self.apis.get(&kind)
    }

    // ── effective (per-API) configuration resolution ─────────────────
    //
    // `apis.<key>.<field> = Some(v)` replaces the provider-level default
    // wholesale; `None` inherits it.

    /// Effective headers for `kind` (per-API map replaces provider map).
    pub fn effective_headers(&self, kind: ApiKind) -> HashMap<String, String> {
        match self.api(kind).and_then(|a| a.headers.as_ref()) {
            Some(h) => h.clone(),
            None => self.headers.clone(),
        }
    }

    /// Effective body overrides for `kind`.
    pub fn effective_body_overrides(&self, kind: ApiKind) -> HashMap<String, serde_json::Value> {
        match self.api(kind).and_then(|a| a.body_overrides.as_ref()) {
            Some(b) => b.clone(),
            None => self.body_overrides.clone(),
        }
    }

    /// Effective non-streaming timeout for `kind`.
    ///
    /// `caller` is the timeout resolved by the caller's chain (target →
    /// route → provider-level `timeout_secs`). A per-API `timeout_secs`
    /// declaration replaces it wholesale.
    pub fn effective_timeout(&self, kind: ApiKind, caller: u64) -> u64 {
        self.api(kind)
            .and_then(|a| a.timeout_secs)
            .unwrap_or(caller)
    }

    /// Effective strip fields for `kind` (provider-level default unless
    /// the API node replaces them).
    pub fn effective_strip_fields(&self, kind: ApiKind) -> &[String] {
        match self.api(kind).and_then(|a| a.strip_fields.as_ref()) {
            Some(s) => s.as_slice(),
            None => &self.strip_fields,
        }
    }

    /// Effective per-model strip fields for `kind`.
    pub fn effective_model_strip_fields(&self, kind: ApiKind) -> &HashMap<String, Vec<String>> {
        match self.api(kind).and_then(|a| a.model_strip_fields.as_ref()) {
            Some(m) => m,
            None => &self.model_strip_fields,
        }
    }

    /// Effective role mappings for `kind`.
    pub fn effective_role_mappings(&self, kind: ApiKind) -> &HashMap<String, String> {
        match self.api(kind).and_then(|a| a.role_mappings.as_ref()) {
            Some(m) => m,
            None => &self.role_mappings,
        }
    }

    /// Effective Anthropic cache_control flag for `kind`.
    pub fn effective_anthropic_cache_control(&self, kind: ApiKind) -> bool {
        self.api(kind)
            .and_then(|a| a.anthropic_cache_control)
            .unwrap_or(self.anthropic_cache_control)
    }

    // ── URL resolution ───────────────────────────────────────────────

    /// The base URL configured for `kind` (trailing `/` trimmed at load
    /// time; trimmed again here for defensively constructed configs).
    pub fn api_base_url(&self, kind: ApiKind) -> sag_core::Result<&str> {
        self.api(kind)
            .map(|a| a.base_url.trim_end_matches('/'))
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "provider '{}': apis.{} is not configured",
                    self.name,
                    kind.as_str()
                ))
            })
    }

    /// Full upstream URL for a protocol key: `apis.<kind>.base_url` +
    /// the fixed protocol path (`sag_core::protocol_path`).
    pub fn protocol_url(&self, kind: ApiKind) -> sag_core::Result<String> {
        let path = protocol_path(kind).ok_or_else(|| {
            AppError::BadRequest(format!(
                "provider '{}': api kind '{}' has no fixed protocol path \
                 (use image_url for image requests)",
                self.name,
                kind.as_str()
            ))
        })?;
        Ok(join_url(self.api_base_url(kind)?, path))
    }

    /// Full upstream URL for an image generation request:
    /// `apis.image.base_url` + `api_endpoint` (the route-declared path).
    /// Falls back to `apis.chat_completions.base_url` when no `image`
    /// node is declared (single-base configuration compatibility).
    pub fn image_url(&self, api_endpoint: &str) -> sag_core::Result<String> {
        let base = self
            .api(ApiKind::Image)
            .or_else(|| self.api(ApiKind::ChatCompletions))
            .map(|a| a.base_url.trim_end_matches('/'))
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "provider '{}': image generation requires apis.image \
                     (or apis.chat_completions) base_url",
                    self.name
                ))
            })?;
        Ok(join_url(base, api_endpoint))
    }

    // ── stream timeout accessors (per-API) ──────────────────────────

    /// Return the configured stream-chunk idle timeout for `kind`, if any.
    pub fn chunk_timeout_secs(&self, kind: ApiKind) -> Option<u64> {
        self.api(kind)
            .and_then(|a| a.chunk_timeout_secs)
            .or(self.chunk_timeout_secs)
    }

    /// Return the effective time-to-first-byte timeout for streaming
    /// requests to `kind`, falling back to the crate default when unset.
    pub fn ttfb_timeout_secs(&self, kind: ApiKind) -> u64 {
        self.api(kind)
            .and_then(|a| a.ttfb_timeout_secs)
            .or(self.ttfb_timeout_secs)
            .unwrap_or_else(sag_config::default_ttfb_timeout_secs)
    }

    /// Build a reqwest POST request for `kind` with per-kind auth,
    /// effective headers, and effective body overrides applied.
    ///
    /// `Content-Type: application/json` is set automatically.
    /// Per-kind auth headers are applied first, then
    /// `effective_headers(kind)` (so configured headers can override auth),
    /// then `effective_body_overrides(kind)`, then `target_extra`.
    pub(crate) fn build_request(
        &self,
        url: &str,
        kind: ApiKind,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
    ) -> RequestBuilder {
        let req = self.http_client.post(url);
        let auth = provider_auth_headers(kind, self.current_key());
        let auth_refs: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let headers = self.effective_headers(kind);
        let req = apply_headers(req, &auth_refs, &headers);
        let body_overrides = self.effective_body_overrides(kind);
        let body = apply_body_overrides(body, &body_overrides);
        let body = apply_body_overrides(body, target_extra);

        // Union effective (per-API or provider-level) strip_fields and
        // model-level strip_fields (if real_model matches).
        // Note: By contract, body["model"] has already been substituted to real_model by the caller.
        let provider_strip = self.effective_strip_fields(kind);
        let model_strip = self.effective_model_strip_fields(kind);
        let effective_strip = if let Some(model_name) = body.get("model").and_then(|v| v.as_str()) {
            if let Some(model_strips) = model_strip.get(model_name) {
                let mut combined = provider_strip.to_vec();
                combined.extend(model_strips.clone());
                combined
            } else {
                provider_strip.to_vec()
            }
        } else {
            provider_strip.to_vec()
        };

        let body = apply_body_strip(body, &effective_strip);
        let body_str = serde_json::to_string(&body).unwrap_or_default();
        debug!(url, "sending upstream request");
        trace!(url, body = %body_str, "upstream request body");
        req.json(&body)
    }

    /// Check upstream response status for **non-streaming** requests.
    ///
    /// On success returns the response body text for further parsing.
    /// On error returns `AppError::Upstream`.
    pub(crate) async fn check_upstream_error(
        &self,
        response: reqwest::Response,
    ) -> sag_core::Result<String> {
        let status = response.status().as_u16();
        let body_text = response.text().await.map_err(AppError::from)?;

        if !(200..300).contains(&status) {
            warn!(status, %body_text, "upstream returned error");
            return Err(AppError::Upstream {
                provider: self.name.clone(),
                status,
                body: body_text,
            });
        }

        Ok(body_text)
    }

    /// Send a JSON POST request for `kind` and return the response body
    /// text on success.
    ///
    /// Combines `build_request` + `send` + `check_upstream_error`. Used by
    /// `chat()` and `chat_image()`.
    ///
    /// `timeout_secs` applies a per-request deadline covering the whole
    /// round-trip (connect, headers, and body read) when `Some`. `None`
    /// means no request-level deadline; the client itself carries none, so
    /// the request can run indefinitely.
    pub async fn send_json(
        &self,
        url: &str,
        kind: ApiKind,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
        timeout_secs: Option<u64>,
    ) -> sag_core::Result<String> {
        let mut request = self.build_request(url, kind, body, target_extra);
        if let Some(secs) = timeout_secs {
            request = request.timeout(std::time::Duration::from_secs(secs));
        }
        let response = request.send().await.map_err(AppError::from)?;
        let body_text = self.check_upstream_error(response).await?;
        trace!(response_body = %body_text, "upstream response body");
        Ok(body_text)
    }

    /// Send a JSON POST request for `kind` (streaming) and return the
    /// `Response`.
    ///
    /// Combines `build_request` + `send` + `ensure_success`. Used by
    /// `chat_stream()`.
    ///
    /// Unlike [`Self::send_json`], streaming does **not** apply a total
    /// request deadline (that would truncate a healthy long-lived
    /// stream). The provider's `timeout_secs` is therefore a **no-op**
    /// for a successful streaming request: once the 2xx headers arrive,
    /// the total wall-clock duration of the body is unbounded by it.
    /// Instead, only the time-to-first-byte phase — connecting
    /// through receipt of the response headers — is bounded by
    /// [`ProviderBase::ttfb_timeout_secs`]. `reqwest`'s `send()` future
    /// resolves as soon as the response headers arrive and before the
    /// body is consumed, so wrapping it in `tokio::time::timeout`
    /// precisely bounds TTFB without constraining the subsequent body
    /// stream. Once headers arrive, the body is governed solely by the
    /// per-chunk idle timeout in `chat_stream()`.
    ///
    /// A TTFB timeout maps to a retryable [`AppError::TtfbTimeout`] so the
    /// dispatcher can fall over to the next target.
    pub async fn send_json_stream(
        &self,
        url: &str,
        kind: ApiKind,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<reqwest::Response> {
        let request = self.build_request(url, kind, body, target_extra);
        let ttfb = std::time::Duration::from_secs(self.ttfb_timeout_secs(kind));
        let response = match tokio::time::timeout(ttfb, request.send()).await {
            Ok(result) => result.map_err(AppError::from)?,
            Err(_elapsed) => {
                warn!(
                    ttfb_timeout_secs = ttfb.as_secs(),
                    "upstream time-to-first-byte timeout"
                );
                return Err(AppError::TtfbTimeout {
                    provider: self.name.clone(),
                    secs: ttfb.as_secs(),
                });
            }
        };
        let response = self.ensure_success(response, kind).await?;
        trace!("upstream stream connection established");
        debug!(
            status = response.status().as_u16(),
            "upstream stream established"
        );
        Ok(response)
    }

    /// Check upstream response status for **streaming** requests.
    ///
    /// On success, returns the `Response` so the caller can consume the byte
    /// stream. On error, consumes the response to read the error body and
    /// returns `AppError::Upstream`.
    ///
    /// Reading the error body is bounded by the same time-to-first-byte
    /// budget as header arrival ([`ProviderBase::ttfb_timeout_secs`]): an
    /// upstream that returns a non-2xx status but never finishes its error
    /// body would otherwise block this future indefinitely. On expiry we
    /// still return `AppError::Upstream` (preserving `status`) with a
    /// diagnostic body instead of waiting forever.
    pub(crate) async fn ensure_success(
        &self,
        response: reqwest::Response,
        kind: ApiKind,
    ) -> sag_core::Result<reqwest::Response> {
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let ttfb = std::time::Duration::from_secs(self.ttfb_timeout_secs(kind));
            let body = match tokio::time::timeout(ttfb, response.text()).await {
                Ok(Ok(body)) => body,
                Ok(Err(e)) => return Err(AppError::from(e)),
                Err(_elapsed) => {
                    warn!(
                        status,
                        ttfb_timeout_secs = ttfb.as_secs(),
                        "upstream stream error body not received within ttfb budget; \
                         returning partial diagnostics"
                    );
                    format!(
                        "upstream error (status {status}); error body not received within {}s",
                        ttfb.as_secs()
                    )
                }
            };
            warn!(status, %body, "upstream returned error on stream");
            return Err(AppError::Upstream {
                provider: self.name.clone(),
                status,
                body,
            });
        }
        Ok(response)
    }

    /// Apply role mappings for `kind` to a mutable slice of chat messages.
    ///
    /// Messages whose role matches a key in the effective `role_mappings`
    /// get their role replaced with the corresponding value. Non-matching
    /// messages are unchanged. Early-returns when the effective mappings
    /// are empty.
    pub fn has_role_mappings(&self, kind: ApiKind) -> bool {
        !self.effective_role_mappings(kind).is_empty()
    }

    pub fn apply_role_mappings(&self, kind: ApiKind, messages: &mut [ChatMessage]) {
        let role_mappings = self.effective_role_mappings(kind);
        if role_mappings.is_empty() {
            return;
        }
        for msg in messages.iter_mut() {
            if let Some(new_role) = role_mappings.get(&msg.role) {
                msg.role = new_role.clone();
            }
        }
    }

    /// Look up a single effective role mapping for `kind` (native body
    /// path, B5).
    pub fn role_mapping_for(&self, kind: ApiKind, role: &str) -> Option<&String> {
        self.effective_role_mappings(kind).get(role)
    }

    // ── native SystemOne passthrough ───────────────────────────────

    /// Native SystemOne call: `POST` the caller's SystemOne-shaped body to
    /// `apis.systemone.base_url` + `/systemone` and return the raw JSON
    /// response.
    ///
    /// The body is never translated: `state` / `questions` / `answers` /
    /// `id` and the probability fields round-trip verbatim (same principle
    /// as the native Responses lane). Body synthesis (overrides, strip
    /// fields, target extras), auth headers and the request deadline are
    /// all handled by [`ProviderBase::build_request`] +
    /// [`ProviderBase::send_json`].
    ///
    /// A 200-delivered `error` object is promoted to
    /// [`AppError::Upstream`] (429 when rate-limit-ish, else 502), matching
    /// the chat / responses convention.
    pub async fn systemone_inner(
        &self,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
        timeout_secs: u64,
    ) -> sag_core::Result<serde_json::Value> {
        let url = self.protocol_url(ApiKind::SystemOne)?;
        let timeout = self.effective_timeout(ApiKind::SystemOne, timeout_secs);
        let body_text = self
            .send_json(&url, ApiKind::SystemOne, body, target_extra, Some(timeout))
            .await?;

        let raw: serde_json::Value = serde_json::from_str(&body_text)
            .map_err(|e| AppError::Internal(format!("failed to parse systemone response: {e}")))?;

        if body_has_error_object(&raw) {
            let err_obj = raw.get("error").expect("checked error object");
            let msg = body_error_message(err_obj);
            let status = if body_error_is_rate_limited(err_obj) {
                429
            } else {
                502
            };
            warn!(status = 200, %body_text, "upstream returned error");
            return Err(AppError::Upstream {
                provider: self.name.clone(),
                status,
                body: msg,
            });
        }

        Ok(raw)
    }
}

// ── 200-delivered upstream error detection ───────────────────────
//
// Some upstreams (observed with Volcano Ark) deliver errors with HTTP 200,
// either as an SSE data frame whose JSON carries a top-level `error` object,
// or as an error object inside a 200 JSON body. These helpers centralize the
// "does this 200 payload contain an error" check for reuse across every
// provider impl and the SSE frame path.

/// Return true when a 200 response body JSON carries a top-level `error`
/// **object** — the OpenAI-style error payload some upstreams deliver with
/// HTTP 200 instead of a non-2xx status. Only an actual JSON object counts:
/// `"error": null`, an absent `error` key, or a scalar `error` value
/// (string / bool / array / number) is treated as no error, so a 200
/// payload with a non-object `error` field is never misclassified.
pub fn body_has_error_object(value: &serde_json::Value) -> bool {
    value.get("error").is_some_and(|e| e.is_object())
}

/// Extract a human-readable message from an upstream error object.
/// Prefers `message`, then `msg`; falls back to the serialized object so
/// the value stays searchable in the UI even for unusual error shapes.
pub fn body_error_message(error: &serde_json::Value) -> String {
    error
        .get("message")
        .and_then(|m| m.as_str())
        .or_else(|| error.get("msg").and_then(|m| m.as_str()))
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string())
}

/// Conservative rate-limit detection on an upstream error object.
///
/// Returns true when the numeric `code` == 429, or a string `code` /
/// `message` contains a rate-limit marker ("rate limit", "burst", "flow",
/// "quota", case-insensitive). Anything else → false. Conservative by
/// design: it only inspects the `code` / `message` fields (never arbitrary
/// object keys), so a generic upstream failure is never misclassified.
pub fn body_error_is_rate_limited(error: &serde_json::Value) -> bool {
    // "ratelimit" (no space) covers camelCase codes like `RateLimitExceeded`;
    // the spaced form covers human-readable messages.
    const MARKERS: &[&str] = &["rate limit", "ratelimit", "burst", "flow", "quota"];

    if let Some(429) = error.get("code").and_then(|c| c.as_u64()) {
        return true;
    }
    let lower_code = error
        .get("code")
        .and_then(|c| c.as_str())
        .map(|s| s.to_lowercase());
    let lower_msg = error
        .get("message")
        .and_then(|m| m.as_str())
        .map(|s| s.to_lowercase());
    lower_code
        .iter()
        .chain(lower_msg.iter())
        .any(|h| MARKERS.iter().any(|m| h.contains(m)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::spawn_server;

    fn make_base(role_mappings: HashMap<String, String>) -> ProviderBase {
        let mut config = crate::test_support::test_config("https://example.com", 30, None, None);
        config.role_mappings = role_mappings;
        ProviderBase::new("test".into(), &config).unwrap()
    }

    fn msg(role: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: serde_json::Value::String("content".into()),
            name: None,
            extra: serde_json::Map::new(),
        }
    }

    fn make_base_with(base_url: String, ttfb_timeout_secs: Option<u64>) -> ProviderBase {
        make_base_with_timeouts(base_url, 30, ttfb_timeout_secs, None)
    }

    fn make_base_with_timeouts(
        base_url: String,
        timeout_secs: u64,
        ttfb_timeout_secs: Option<u64>,
        chunk_timeout_secs: Option<u64>,
    ) -> ProviderBase {
        ProviderBase::new(
            "test".into(),
            &crate::test_support::test_config(
                &base_url,
                timeout_secs,
                ttfb_timeout_secs,
                chunk_timeout_secs,
            ),
        )
        .unwrap()
    }

    fn make_base_with_body_overrides(
        body_overrides: HashMap<String, serde_json::Value>,
    ) -> ProviderBase {
        let mut config = crate::test_support::test_config("https://example.com", 30, None, None);
        config.body_overrides = body_overrides;
        ProviderBase::new("test".into(), &config).unwrap()
    }

    fn extract_json_body(req: reqwest::Request) -> serde_json::Value {
        let body_bytes = req.body().unwrap().as_bytes().unwrap();
        serde_json::from_slice(body_bytes).unwrap()
    }

    #[test]
    fn ttfb_timeout_secs_falls_back_to_default() {
        let base = make_base_with("https://example.com".into(), None);
        assert_eq!(base.ttfb_timeout_secs(ApiKind::ChatCompletions), 60);
        let base = make_base_with("https://example.com".into(), Some(5));
        assert_eq!(base.ttfb_timeout_secs(ApiKind::ChatCompletions), 5);
    }

    #[tokio::test]
    async fn send_json_stream_times_out_before_headers() {
        // Server accepts the connection but never sends response headers.
        let base_url = spawn_server(|stream| async move {
            // Hold the connection open well past the TTFB budget.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            drop(stream);
        })
        .await;

        let base = make_base_with(base_url.clone(), Some(1));
        let url = base.protocol_url(ApiKind::ChatCompletions).unwrap();
        let start = std::time::Instant::now();
        let result = base
            .send_json_stream(
                &url,
                ApiKind::ChatCompletions,
                serde_json::json!({}),
                &HashMap::new(),
            )
            .await;
        let elapsed = start.elapsed();

        match result {
            Err(sag_core::AppError::TtfbTimeout { secs, .. }) => assert_eq!(secs, 1),
            other => panic!("expected TtfbTimeout, got: {other:?}"),
        }
        // Fired at ~1s, well before any total-request deadline.
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "TTFB timeout fired too late: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn send_json_stream_does_not_time_out_on_slow_body() {
        use tokio::io::AsyncWriteExt;
        // Headers arrive immediately; the body then dribbles out over a
        // span longer than the TTFB budget. TTFB must not truncate it.
        let base_url = spawn_server(|mut stream| async move {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            stream.flush().await.unwrap();
            for _ in 0..3 {
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                stream.write_all(b"data: {}\n\n").await.unwrap();
                stream.flush().await.unwrap();
            }
            drop(stream);
        })
        .await;

        // TTFB budget (1s) is shorter than the total body duration (~1.2s).
        let base = make_base_with(base_url.clone(), Some(1));
        let url = base.protocol_url(ApiKind::ChatCompletions).unwrap();
        let response = base
            .send_json_stream(
                &url,
                ApiKind::ChatCompletions,
                serde_json::json!({}),
                &HashMap::new(),
            )
            .await
            .expect("headers arrived within TTFB budget");

        // Fully drain the slow body; the absence of any total deadline means
        // this completes without a timeout error.
        use futures_util::StreamExt;
        let mut stream = response.bytes_stream();
        let mut total = 0usize;
        while let Some(chunk) = stream.next().await {
            total += chunk.expect("body chunk read without timeout").len();
        }
        assert!(total > 0, "expected to receive streamed body bytes");
    }

    #[tokio::test]
    async fn send_json_stream_success_body_can_exceed_timeout_secs() {
        use tokio::io::AsyncWriteExt;
        // Provider `timeout_secs` = 1s, but the healthy stream takes ~1.6s
        // total. The old client-level total deadline would truncate it; the
        // new implementation must consume the body fully.
        let base_url = spawn_server(|mut stream| async move {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            stream.flush().await.unwrap();
            for _ in 0..4 {
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                stream.write_all(b"data: {}\n\n").await.unwrap();
                stream.flush().await.unwrap();
            }
            drop(stream);
        })
        .await;

        let base = make_base_with_timeouts(base_url.clone(), 1, Some(1), None);
        let url = base.protocol_url(ApiKind::ChatCompletions).unwrap();
        let start = std::time::Instant::now();
        let response = base
            .send_json_stream(
                &url,
                ApiKind::ChatCompletions,
                serde_json::json!({}),
                &HashMap::new(),
            )
            .await
            .expect("headers should arrive within the TTFB budget");

        use futures_util::StreamExt;
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            body.extend_from_slice(
                &chunk.expect("body chunk read without a total request deadline"),
            );
        }
        let elapsed = start.elapsed();

        // The full 4-frame payload must be delivered verbatim — not truncated
        // by the (removed) 1s total deadline.
        let expected = b"data: {}\n\ndata: {}\n\ndata: {}\n\ndata: {}\n\n".to_vec();
        assert_eq!(
            body, expected,
            "the full 4-frame SSE body must be delivered despite outliving timeout_secs"
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(1200),
            "stream must actually run past the 1s deadline before completing: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn send_json_with_deadline_times_out_on_slow_body() {
        use tokio::io::AsyncWriteExt;
        // Headers arrive immediately, but the body never arrives. The
        // request-level deadline (Some(1)) must fire as a reqwest timeout
        // even though the shared client carries no total timeout.
        let base_url = spawn_server(|mut stream| async move {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            stream.flush().await.unwrap();
            // Hold the connection open well past the deadline without body bytes.
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            drop(stream);
        })
        .await;

        let base = make_base_with_timeouts(base_url.clone(), 30, None, None);
        let url = base.protocol_url(ApiKind::ChatCompletions).unwrap();
        let start = std::time::Instant::now();
        let result = base
            .send_json(
                &url,
                ApiKind::ChatCompletions,
                serde_json::json!({}),
                &HashMap::new(),
                Some(1),
            )
            .await;
        let elapsed = start.elapsed();

        match result {
            Err(sag_core::AppError::Network(e)) => {
                assert!(e.is_timeout(), "expected reqwest timeout, got: {e}");
            }
            other => panic!("expected Network timeout, got: {other:?}"),
        }
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "deadline should fire at ~1s: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn send_json_stream_error_body_read_is_bounded_by_ttfb() {
        use tokio::io::AsyncWriteExt;
        // Non-2xx headers arrive immediately, then the error body never
        // completes and the connection is held open. `ensure_success` must
        // bound the error-body read with the TTFB budget instead of waiting
        // forever, and still return Upstream with the preserved status.
        let base_url = spawn_server(|mut stream| async move {
            stream
                .write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"error\":",
                )
                .await
                .unwrap();
            stream.flush().await.unwrap();
            // Keep the connection open: the error body is never finished.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            drop(stream);
        })
        .await;

        let base = make_base_with_timeouts(base_url.clone(), 30, Some(1), None);
        let url = base.protocol_url(ApiKind::ChatCompletions).unwrap();
        let start = std::time::Instant::now();
        let result = base
            .send_json_stream(
                &url,
                ApiKind::ChatCompletions,
                serde_json::json!({}),
                &HashMap::new(),
            )
            .await;
        let elapsed = start.elapsed();

        match result {
            Err(sag_core::AppError::Upstream { status, body, .. }) => {
                assert_eq!(status, 503, "upstream status must be preserved");
                assert!(
                    body.contains("503"),
                    "diagnostic body should mention the status: {body}"
                );
            }
            other => panic!("expected Upstream 503, got: {other:?}"),
        }
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "error-body read must be bounded by the TTFB budget: {elapsed:?}"
        );
    }

    #[test]
    fn target_extra_overrides_body_override_for_shared_key() {
        // body_overrides sets temperature=0.5;
        // target_extra (already merged model_override + target.extra) sets temperature=0.7.
        // The target_extra value must win.
        let mut body_overrides = HashMap::new();
        body_overrides.insert("temperature".into(), serde_json::json!(0.5));

        let mut target_extra = HashMap::new();
        target_extra.insert("temperature".into(), serde_json::json!(0.7));

        let base = make_base_with_body_overrides(body_overrides);
        let req = base
            .build_request(
                "https://example.com/v1/chat/completions",
                ApiKind::ChatCompletions,
                serde_json::json!({ "model": "gpt-4o", "temperature": 1.0 }),
                &target_extra,
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        assert_eq!(
            body.get("temperature").and_then(|v| v.as_f64()),
            Some(0.7),
            "target_extra should override body_overrides for shared key"
        );
        assert_eq!(
            body.get("model").unwrap(),
            "gpt-4o",
            "client key should be preserved"
        );
    }

    #[test]
    fn body_override_alone_overrides_client_key() {
        // Only body_overrides is set; it should override the client-provided value.
        let mut body_overrides = HashMap::new();
        body_overrides.insert("temperature".into(), serde_json::json!(0.5));

        let base = make_base_with_body_overrides(body_overrides);
        let req = base
            .build_request(
                "https://example.com/v1/chat/completions",
                ApiKind::ChatCompletions,
                serde_json::json!({ "model": "gpt-4o", "temperature": 1.0 }),
                &HashMap::new(),
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        assert_eq!(
            body.get("temperature").and_then(|v| v.as_f64()),
            Some(0.5),
            "body_overrides alone should override client key"
        );
        assert_eq!(body.get("model").unwrap(), "gpt-4o");
    }

    #[test]
    fn target_extra_alone_overrides_client_key() {
        // Only target_extra is set (simulating merged model_overrides + target.extra);
        // it should override the client-provided value.
        let mut target_extra = HashMap::new();
        target_extra.insert("temperature".into(), serde_json::json!(0.7));

        let base = make_base_with_body_overrides(HashMap::new());
        let req = base
            .build_request(
                "https://example.com/v1/chat/completions",
                ApiKind::ChatCompletions,
                serde_json::json!({ "model": "gpt-4o", "temperature": 1.0 }),
                &target_extra,
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        assert_eq!(
            body.get("temperature").and_then(|v| v.as_f64()),
            Some(0.7),
            "target_extra alone should override client key"
        );
        assert_eq!(body.get("model").unwrap(), "gpt-4o");
    }

    #[test]
    fn three_tier_same_key_body_override_then_model_override_then_target_extra() {
        // Simulates the full three-tier chain end-to-end:
        //   client body: { temperature: 1.0 }
        //   → body_overrides: { temperature: 0.5 }         (tier 1)
        //   → merged target_extra:
        //       model_override: { temperature: 0.6 }       (tier 2)
        //       target.extra:   { temperature: 0.7 }       (tier 3)
        //       merged result:  { temperature: 0.7 }
        //   → final body: { temperature: 0.7 }
        let mut body_overrides = HashMap::new();
        body_overrides.insert("temperature".into(), serde_json::json!(0.5));

        let mut target_extra = HashMap::new();
        target_extra.insert("temperature".into(), serde_json::json!(0.7));

        let base = make_base_with_body_overrides(body_overrides);
        let req = base
            .build_request(
                "https://example.com/v1/chat/completions",
                ApiKind::ChatCompletions,
                serde_json::json!({ "model": "gpt-4o", "temperature": 1.0 }),
                &target_extra,
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        assert_eq!(
            body.get("temperature").and_then(|v| v.as_f64()),
            Some(0.7),
            "in three-tier chain, target.extra (via merged target_extra) should win"
        );
    }

    #[test]
    fn distinct_keys_across_tiers_accumulate() {
        // body_overrides has max_tokens; target_extra (merged) has temperature and top_p.
        // All keys should be present in the final body.
        let mut body_overrides = HashMap::new();
        body_overrides.insert("max_tokens".into(), serde_json::json!(4096));

        let mut target_extra = HashMap::new();
        target_extra.insert("temperature".into(), serde_json::json!(0.7));
        target_extra.insert("top_p".into(), serde_json::json!(0.9));

        let base = make_base_with_body_overrides(body_overrides);
        let req = base
            .build_request(
                "https://example.com/v1/chat/completions",
                ApiKind::ChatCompletions,
                serde_json::json!({ "model": "gpt-4o" }),
                &target_extra,
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        assert_eq!(
            body.get("max_tokens").and_then(|v| v.as_i64()),
            Some(4096),
            "body_overrides-only key should be present"
        );
        assert_eq!(
            body.get("temperature").and_then(|v| v.as_f64()),
            Some(0.7),
            "target_extra-only key should be present"
        );
        assert_eq!(
            body.get("top_p").and_then(|v| v.as_f64()),
            Some(0.9),
            "target_extra-only key should be present"
        );
        assert_eq!(
            body.get("model").unwrap(),
            "gpt-4o",
            "client key should be preserved"
        );
    }

    #[test]
    fn nested_object_replaced_wholly_by_target_extra() {
        // body_overrides sets thinking = { type: "enabled" };
        // target_extra sets thinking = { budget_tokens: 1600 }.
        // The target_extra nested object replaces the body_overrides one entirely.
        let mut body_overrides = HashMap::new();
        body_overrides.insert("thinking".into(), serde_json::json!({ "type": "enabled" }));

        let mut target_extra = HashMap::new();
        target_extra.insert(
            "thinking".into(),
            serde_json::json!({ "budget_tokens": 1600 }),
        );

        let base = make_base_with_body_overrides(body_overrides);
        let req = base
            .build_request(
                "https://example.com/v1/chat/completions",
                ApiKind::ChatCompletions,
                serde_json::json!({ "model": "gpt-4o" }),
                &target_extra,
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        let thinking = body.get("thinking").unwrap();
        assert_eq!(
            thinking.get("budget_tokens").and_then(|v| v.as_i64()),
            Some(1600),
            "target-extra nested object should win"
        );
        assert!(
            thinking.get("type").is_none(),
            "nested object should be replaced wholly, not deep-merged"
        );
    }

    #[test]
    fn empty_overrides_short_circuits() {
        // Both body_overrides and target_extra are empty.
        // The original body should pass through unchanged.
        let base = make_base_with_body_overrides(HashMap::new());
        let original = serde_json::json!({ "model": "gpt-4o", "temperature": 1.0 });

        let req = base
            .build_request(
                "https://example.com/v1/chat/completions",
                ApiKind::ChatCompletions,
                original.clone(),
                &HashMap::new(),
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        assert_eq!(body, original, "empty overrides should not mutate the body");
    }

    #[test]
    fn strip_fields_removes_field_after_all_overrides() {
        // strip_fields removes chat_template_kwargs even after
        // body_overrides and target_extra insert it.
        let mut body_overrides: HashMap<String, serde_json::Value> = HashMap::new();
        body_overrides.insert("temperature".into(), serde_json::json!(0.5));

        let mut target_extra = HashMap::new();
        target_extra.insert(
            "chat_template_kwargs".into(),
            serde_json::json!({ "enable_thinking": false }),
        );

        let mut config = crate::test_support::test_config("https://example.com", 30, None, None);
        config.body_overrides = body_overrides;
        config.strip_fields = vec!["chat_template_kwargs".to_string()];
        let base = ProviderBase::new("test".into(), &config).unwrap();
        let req = base
            .build_request(
                "https://example.com/v1/chat/completions",
                ApiKind::ChatCompletions,
                serde_json::json!({ "model": "gpt-4o" }),
                &target_extra,
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        // chat_template_kwargs must be absent despite target_extra inserting it
        assert!(
            body.get("chat_template_kwargs").is_none(),
            "strip_fields must remove chat_template_kwargs even if target_extra inserted it"
        );
        // Other overrides still apply
        assert_eq!(
            body.get("temperature").and_then(|v| v.as_f64()),
            Some(0.5),
            "body_overrides should still be applied"
        );
        assert_eq!(body.get("model").unwrap(), "gpt-4o");
    }

    #[test]
    fn model_strip_fields_combines_with_provider_strip_fields() {
        let mut model_strip = HashMap::new();
        model_strip.insert(
            "glm-5.2".to_string(),
            vec!["chat_template_kwargs".to_string()],
        );

        let mut config = crate::test_support::test_config("https://example.com", 30, None, None);
        config.strip_fields = vec!["global_bad_field".to_string()];
        config.model_strip_fields = model_strip;
        let base = ProviderBase::new("test".into(), &config).unwrap();

        // Request for glm-5.2 -> strips both global_bad_field and chat_template_kwargs
        let req = base
            .build_request(
                "https://example.com/v1/chat/completions",
                ApiKind::ChatCompletions,
                serde_json::json!({
                    "model": "glm-5.2",
                    "global_bad_field": 123,
                    "chat_template_kwargs": { "foo": "bar" },
                    "keep_me": "yes"
                }),
                &HashMap::new(),
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        assert!(body.get("global_bad_field").is_none());
        assert!(body.get("chat_template_kwargs").is_none());
        assert_eq!(body.get("keep_me").unwrap(), "yes");

        // Request for gpt-4o -> only strips global_bad_field
        let req2 = base
            .build_request(
                "https://example.com/v1/chat/completions",
                ApiKind::ChatCompletions,
                serde_json::json!({
                    "model": "gpt-4o",
                    "global_bad_field": 123,
                    "chat_template_kwargs": { "foo": "bar" },
                }),
                &HashMap::new(),
            )
            .build()
            .unwrap();

        let body2 = extract_json_body(req2);
        assert!(body2.get("global_bad_field").is_none());
        assert!(body2.get("chat_template_kwargs").is_some());
    }

    #[test]
    fn empty_mappings_does_nothing() {
        let base = make_base(HashMap::new());
        let mut msgs = vec![msg("developer")];
        base.apply_role_mappings(ApiKind::ChatCompletions, &mut msgs);
        assert_eq!(msgs[0].role, "developer");
    }

    #[test]
    fn replaces_matching_role() {
        let base = make_base([("developer".into(), "system".into())].into());
        let mut msgs = vec![msg("developer")];
        base.apply_role_mappings(ApiKind::ChatCompletions, &mut msgs);
        assert_eq!(msgs[0].role, "system");
    }

    #[test]
    fn skips_non_matching_roles() {
        let base = make_base([("developer".into(), "system".into())].into());
        let mut msgs = vec![msg("user"), msg("assistant")];
        base.apply_role_mappings(ApiKind::ChatCompletions, &mut msgs);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
    }

    #[test]
    fn partial_map_leaves_others_unchanged() {
        let base = make_base([("developer".into(), "system".into())].into());
        let mut msgs = vec![msg("developer"), msg("user")];
        base.apply_role_mappings(ApiKind::ChatCompletions, &mut msgs);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[1].role, "user");
    }

    // ── proxy / system-env behavior ────────────────────────────────

    /// Serializes tests that mutate the process-global proxy env vars.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// All spellings of the system proxy env vars reqwest would honor.
    const PROXY_ENV_VARS: &[&str] = &[
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ];

    /// Set every system proxy env var to `value`, returning the previous
    /// values so [`restore_proxy_env`] can put them back.
    ///
    /// # Safety
    ///
    /// `std::env::set_var` is `unsafe` in edition 2024 (concurrent access
    /// to process env is UB). All callers hold [`ENV_LOCK`] for the whole
    /// mutation + use window, and every client built by this crate ignores
    /// the proxy env, so no other code observes the mutation.
    fn set_proxy_env(value: &str) -> Vec<(&'static str, Option<String>)> {
        PROXY_ENV_VARS
            .iter()
            .map(|var| {
                let old = std::env::var(var).ok();
                // SAFETY: caller holds ENV_LOCK; see doc comment.
                unsafe { std::env::set_var(var, value) };
                (*var, old)
            })
            .collect()
    }

    fn restore_proxy_env(saved: Vec<(&'static str, Option<String>)>) {
        for (var, old) in saved {
            match old {
                Some(v) => {
                    // SAFETY: caller holds ENV_LOCK; see [`set_proxy_env`].
                    unsafe { std::env::set_var(var, v) };
                }
                None => {
                    // SAFETY: caller holds ENV_LOCK; see [`set_proxy_env`].
                    unsafe { std::env::remove_var(var) };
                }
            }
        }
    }

    fn proxy_config(scheme: &str, host: &str, port: u16) -> sag_config::ProxyConfig {
        sag_config::ProxyConfig {
            scheme: scheme.into(),
            host: host.into(),
            port: Some(port),
        }
    }

    /// Spawn a listener that counts every accepted connection and answers
    /// each with `200 OK` and `body`. Returns `(base_url, connection_count)`.
    async fn spawn_probe_server(
        body: &'static str,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_task = count.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                count_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{addr}"), count)
    }

    #[test]
    fn provider_base_accepts_direct_and_explicit_proxy() {
        // Direct (None) — the old-wrapper code path.
        let direct = ProviderBase::new(
            "test".into(),
            &crate::test_support::test_config("https://example.com", 30, None, None),
        )
        .expect("direct construction succeeds");
        assert_eq!(
            direct.protocol_url(ApiKind::ChatCompletions).unwrap(),
            "https://example.com/chat/completions"
        );

        // Explicit HTTP proxy.
        let http_proxy = ProviderBase::new_with_proxy(
            "test".into(),
            &crate::test_support::test_config("https://example.com", 30, None, None),
            Some(&proxy_config("http", "127.0.0.1", 8080)),
        )
        .expect("http proxy construction succeeds");

        // Explicit HTTPS proxy.
        let https_proxy = ProviderBase::new_with_proxy(
            "test".into(),
            &crate::test_support::test_config("https://example.com", 30, None, None),
            Some(&proxy_config("https", "proxy.example.com", 8443)),
        )
        .expect("https proxy construction succeeds");

        let _ = (direct, http_proxy, https_proxy);
    }

    // ── per-API URL resolution ───────────────────────────────────────

    #[test]
    fn protocol_url_derives_from_apis_base_and_fixed_path() {
        let base = make_base_with("https://example.com/v1".into(), None);
        assert_eq!(
            base.protocol_url(ApiKind::ChatCompletions).unwrap(),
            "https://example.com/v1/chat/completions"
        );
        assert_eq!(
            base.protocol_url(ApiKind::Responses).unwrap(),
            "https://example.com/v1/responses"
        );
        assert_eq!(
            base.protocol_url(ApiKind::AnthropicMessages).unwrap(),
            "https://example.com/v1/messages"
        );
        // Image has no fixed protocol path.
        assert!(base.protocol_url(ApiKind::Image).is_err());
    }

    #[test]
    fn protocol_url_missing_api_node_is_bad_request() {
        let mut config = crate::test_support::test_config("https://example.com", 30, None, None);
        config.apis.shift_remove(&ApiKind::AnthropicMessages);
        let base = ProviderBase::new("test".into(), &config).unwrap();
        let err = base.protocol_url(ApiKind::AnthropicMessages).unwrap_err();
        assert!(
            matches!(&err, AppError::BadRequest(m) if m.contains("apis.anthropic_messages")),
            "expected missing-node BadRequest, got {err:?}"
        );
    }

    #[test]
    fn image_url_prefers_image_node_then_chat_fallback() {
        let mut config =
            crate::test_support::test_config("https://chat.example.com", 30, None, None);
        config.apis.insert(
            ApiKind::Image,
            sag_config::ApiConfig {
                base_url: "https://images.example.com".into(),
                ..Default::default()
            },
        );
        let base = ProviderBase::new("test".into(), &config).unwrap();
        assert_eq!(
            base.image_url("/images/generations").unwrap(),
            "https://images.example.com/images/generations"
        );

        // No image node → chat_completions base.
        let chat_only =
            crate::test_support::test_config("https://chat.example.com", 30, None, None);
        let base = ProviderBase::new("test".into(), &chat_only).unwrap();
        assert_eq!(
            base.image_url("/images/generations").unwrap(),
            "https://chat.example.com/images/generations"
        );
    }

    // ── effective (per-API override) resolution ──────────────────────

    #[test]
    fn effective_headers_api_node_replaces_provider_level() {
        let mut config = crate::test_support::test_config("https://example.com", 30, None, None);
        config.headers.insert("X-Provider".into(), "p".into());
        config.headers.insert("X-Shared".into(), "provider".into());
        config.apis.get_mut(&ApiKind::Responses).unwrap().headers =
            Some(HashMap::from([("X-Shared".into(), "api".into())]));
        let base = ProviderBase::new("test".into(), &config).unwrap();

        // Chat inherits the provider map untouched.
        let chat_headers = base.effective_headers(ApiKind::ChatCompletions);
        assert_eq!(chat_headers.get("X-Shared").unwrap(), "provider");
        assert_eq!(chat_headers.get("X-Provider").unwrap(), "p");

        // Responses replaces the whole map (no merge).
        let resp_headers = base.effective_headers(ApiKind::Responses);
        assert_eq!(resp_headers.get("X-Shared").unwrap(), "api");
        assert!(!resp_headers.contains_key("X-Provider"));
    }

    #[test]
    fn effective_timeout_api_override_replaces_caller_value() {
        let mut config = crate::test_support::test_config("https://example.com", 30, None, None);
        config
            .apis
            .get_mut(&ApiKind::ChatCompletions)
            .unwrap()
            .timeout_secs = Some(7);
        let base = ProviderBase::new("test".into(), &config).unwrap();
        // Declared per-API value wins over the caller-resolved chain.
        assert_eq!(base.effective_timeout(ApiKind::ChatCompletions, 600), 7);
        // Undeclared kind keeps the caller-resolved value.
        assert_eq!(base.effective_timeout(ApiKind::Responses, 600), 600);
    }

    // ── native SystemOne passthrough ───────────────────────────────

    /// Base with an `apis.systemone` node at `base_url`; the shared test
    /// config deliberately omits that node (presence is the capability).
    fn make_systemone_base(
        base_url: &str,
        timeout_secs: Option<u64>,
        body_overrides: Option<HashMap<String, serde_json::Value>>,
    ) -> ProviderBase {
        let mut config = crate::test_support::test_config(base_url, 30, None, None);
        config.apis.insert(
            ApiKind::SystemOne,
            sag_config::ApiConfig {
                base_url: base_url.into(),
                timeout_secs,
                body_overrides,
                ..Default::default()
            },
        );
        ProviderBase::new("test".into(), &config).unwrap()
    }

    #[test]
    fn systemone_protocol_url_appends_fixed_path() {
        let base = make_systemone_base("https://example.com/v1/", None, None);
        assert_eq!(
            base.protocol_url(ApiKind::SystemOne).unwrap(),
            "https://example.com/v1/systemone"
        );
    }

    #[tokio::test]
    async fn systemone_inner_missing_api_node_is_bad_request() {
        // `test_config` declares no `apis.systemone`: the capability is
        // absent, so the call must fail before any network round-trip.
        let base = make_base_with("https://example.com".into(), None);
        let err = base
            .systemone_inner(
                serde_json::json!({ "model": "m", "state": "s", "questions": {} }),
                &HashMap::new(),
                30,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, AppError::BadRequest(m) if m.contains("apis.systemone")),
            "expected missing-node BadRequest, got {err:?}"
        );
    }

    #[test]
    fn systemone_effective_timeout_prefers_api_node() {
        let base = make_systemone_base("https://example.com", Some(7), None);
        assert_eq!(base.effective_timeout(ApiKind::SystemOne, 600), 7);
        // No per-API declaration → the caller-resolved value wins.
        let base = make_systemone_base("https://example.com", None, None);
        assert_eq!(base.effective_timeout(ApiKind::SystemOne, 600), 600);
    }

    #[tokio::test]
    async fn systemone_inner_returns_raw_upstream_json() {
        let body = r#"{"id":"ans-1","model":"m","answers":{"q1":{"type":"choice","choice":"a","confidence":0.9,"probabilities":{"a":0.9,"b":0.1}}},"usage":{"input_tokens":7,"output_tokens":3}}"#;
        let base_url = spawn_server(move |mut stream| {
            use tokio::io::AsyncWriteExt;
            let body = body.to_string();
            async move {
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(resp.as_bytes()).await.unwrap();
                stream.flush().await.unwrap();
            }
        })
        .await;

        let base = make_systemone_base(&base_url, None, None);
        let value = base
            .systemone_inner(
                serde_json::json!({ "model": "m", "state": "s", "questions": {} }),
                &HashMap::new(),
                30,
            )
            .await
            .expect("200 body parses as JSON");

        // Passthrough: id / answers / probability fields survive verbatim.
        assert_eq!(value["id"], "ans-1");
        assert_eq!(value["answers"]["q1"]["choice"], "a");
        assert_eq!(
            value["answers"]["q1"]["probabilities"]["b"].as_f64(),
            Some(0.1)
        );
    }

    #[tokio::test]
    async fn systemone_inner_200_error_object_maps_to_502() {
        let body = r#"{"error":{"message":"model overloaded","code":500}}"#;
        let base_url = spawn_server(move |mut stream| {
            use tokio::io::AsyncWriteExt;
            let body = body.to_string();
            async move {
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(resp.as_bytes()).await.unwrap();
                stream.flush().await.unwrap();
            }
        })
        .await;

        let base = make_systemone_base(&base_url, None, None);
        let err = base
            .systemone_inner(serde_json::json!({}), &HashMap::new(), 30)
            .await
            .unwrap_err();
        match err {
            AppError::Upstream { status, body, .. } => {
                assert_eq!(status, 502, "200-delivered error maps to 502");
                assert_eq!(body, "model overloaded");
            }
            other => panic!("expected Upstream 502, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn systemone_inner_200_rate_limit_error_maps_to_429() {
        let body = r#"{"error":{"message":"burst tokens limit","code":429}}"#;
        let base_url = spawn_server(move |mut stream| {
            use tokio::io::AsyncWriteExt;
            let body = body.to_string();
            async move {
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(resp.as_bytes()).await.unwrap();
                stream.flush().await.unwrap();
            }
        })
        .await;

        let base = make_systemone_base(&base_url, None, None);
        let err = base
            .systemone_inner(serde_json::json!({}), &HashMap::new(), 30)
            .await
            .unwrap_err();
        match err {
            AppError::Upstream { status, body, .. } => {
                assert_eq!(status, 429, "rate-limit-ish error maps to 429");
                assert_eq!(body, "burst tokens limit");
            }
            other => panic!("expected Upstream 429, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn systemone_inner_applies_api_node_timeout_over_caller_value() {
        use tokio::io::AsyncWriteExt;
        // Headers arrive immediately, the body never does. The node declares
        // timeout_secs = 1 while the caller passes 30, so the request must
        // abort at ~1s — proving `apis.systemone.timeout_secs` wins.
        let base_url = spawn_server(|mut stream| async move {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            drop(stream);
        })
        .await;

        let base = make_systemone_base(&base_url, Some(1), None);
        let start = std::time::Instant::now();
        let err = base
            .systemone_inner(serde_json::json!({}), &HashMap::new(), 30)
            .await
            .unwrap_err();
        let elapsed = start.elapsed();

        match err {
            AppError::Network(e) => assert!(e.is_timeout(), "expected reqwest timeout, got: {e}"),
            other => panic!("expected Network timeout, got {other:?}"),
        }
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "api-node deadline should fire at ~1s, not the caller's 30s: {elapsed:?}"
        );
    }

    #[test]
    fn systemone_body_composition_api_overrides_then_target_extra() {
        // provider-level body_overrides { temperature: 0.5 }
        // → apis.systemone.body_overrides { temperature: 0.4 } (wholesale replace)
        // → target_extra { temperature: 0.7 } (wins)
        let mut provider_overrides = HashMap::new();
        provider_overrides.insert("temperature".into(), serde_json::json!(0.5));
        let mut api_overrides = HashMap::new();
        api_overrides.insert("temperature".into(), serde_json::json!(0.4));

        let mut config = crate::test_support::test_config("https://example.com", 30, None, None);
        config.body_overrides = provider_overrides;
        config.apis.insert(
            ApiKind::SystemOne,
            sag_config::ApiConfig {
                base_url: "https://example.com".into(),
                body_overrides: Some(api_overrides),
                ..Default::default()
            },
        );
        let base = ProviderBase::new("test".into(), &config).unwrap();

        let mut target_extra = HashMap::new();
        target_extra.insert("temperature".into(), serde_json::json!(0.7));
        let req = base
            .build_request(
                "https://example.com/systemone",
                ApiKind::SystemOne,
                serde_json::json!({
                    "model": "m",
                    "state": { "turn": 2 },
                    "questions": { "q1": { "type": "noul" } },
                    "temperature": 1.0
                }),
                &target_extra,
            )
            .build()
            .unwrap();

        let body = extract_json_body(req);
        assert_eq!(
            body.get("temperature").and_then(|v| v.as_f64()),
            Some(0.7),
            "target_extra must win the composition chain"
        );
        // Passthrough fields survive the override layers untouched.
        assert_eq!(body["model"], "m");
        assert_eq!(body["state"]["turn"].as_i64(), Some(2));
        assert_eq!(body["questions"]["q1"]["type"], "noul");
    }

    #[test]
    fn build_request_auth_is_per_kind_and_headers_override_auth() {
        let mut config = crate::test_support::test_config("https://example.com", 30, None, None);
        // Configured Authorization header must win over the constructed Bearer.
        config
            .headers
            .insert("authorization".into(), "override".into());
        let base = ProviderBase::new("test".into(), &config).unwrap();

        // Chat → Bearer auth (overridden by the configured header).
        let req = base
            .build_request(
                "https://example.com/chat/completions",
                ApiKind::ChatCompletions,
                serde_json::json!({"model": "m"}),
                &HashMap::new(),
            )
            .build()
            .unwrap();
        assert_eq!(
            req.headers().get("authorization").unwrap(),
            "override",
            "effective_headers must be applied after auth"
        );

        // Anthropic → x-api-key + anthropic-version, no Authorization override here.
        let mut config2 = crate::test_support::test_config("https://example.com", 30, None, None);
        config2.headers.clear();
        let base2 = ProviderBase::new("test".into(), &config2).unwrap();
        let req = base2
            .build_request(
                "https://example.com/messages",
                ApiKind::AnthropicMessages,
                serde_json::json!({"model": "m"}),
                &HashMap::new(),
            )
            .build()
            .unwrap();
        assert_eq!(req.headers().get("x-api-key").unwrap(), "sk-test");
        assert_eq!(
            req.headers().get("anthropic-version").unwrap(),
            "2023-06-01"
        );
        assert!(req.headers().get("authorization").is_none());
    }

    #[test]
    fn create_provider_accepts_proxy() {
        let config = crate::test_support::test_config("https://example.com", 30, None, None);
        let _ = crate::create_provider("t".into(), &config).expect("direct create_provider works");
        let _ = crate::create_provider_with_proxy(
            "t".into(),
            &config,
            Some(&proxy_config("http", "127.0.0.1", 8080)),
        )
        .expect("create_provider_with_proxy works");
    }

    #[tokio::test]
    async fn old_wrapper_client_ignores_system_proxy_env() {
        // Spawn the servers first — they do not depend on env vars.
        // A "detector" stands in for whatever HTTP_PROXY might point at. If
        // the client ever read the system proxy env, it would connect here.
        // It answers with a body distinct from the target's so a wrongly
        // proxied request is recognizable from the response alone.
        let (detector_url, detector_hits) = spawn_probe_server("det").await;
        let target = crate::test_support::spawn_server(|mut stream| async move {
            use tokio::io::AsyncWriteExt;
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await;
        })
        .await;

        // Build through the old 2-arg wrapper while the env is poisoned: any
        // implicit system-proxy discovery would pick up `detector_url`.
        // reqwest reads the proxy env at client-build time, and the build is
        // synchronous, so the lock scope covers mutation → construction and
        // is dropped before any await.
        let base = {
            let _guard = ENV_LOCK.lock().unwrap();
            let saved = set_proxy_env(&detector_url);
            let base = ProviderBase::new(
                "test".into(),
                &crate::test_support::test_config(&target, 30, None, None),
            )
            .expect("provider builds with poisoned proxy env");
            restore_proxy_env(saved);
            base
        };

        // A real direct request still succeeds against the local target.
        // reqwest connections are lazy: even if the client had wrongly
        // captured the system proxy at build time, the detector would only
        // be contacted here. Asserting the body and the hit count only after
        // the request completes ensures a (wrong) proxied connection cannot
        // slip past either check.
        let url = base.protocol_url(ApiKind::ChatCompletions).unwrap();
        let body = base
            .send_json(
                &url,
                ApiKind::ChatCompletions,
                serde_json::json!({}),
                &HashMap::new(),
                None,
            )
            .await
            .expect("direct request succeeds");
        assert_eq!(
            body, "ok",
            "direct request must hit the target, not the proxy"
        );

        // The detector must never have been contacted.
        assert_eq!(
            detector_hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "old wrapper must not read the system proxy env"
        );
    }

    #[tokio::test]
    async fn explicit_proxy_routes_upstream_through_proxy() {
        // The probe server doubles as the explicit proxy: it answers every
        // request itself, so if the client correctly proxies through it, the
        // real target is never contacted.
        let (proxy_url, proxy_hits) = spawn_probe_server("ok").await;
        let proxy_port: u16 = proxy_url
            .rsplit(':')
            .next()
            .and_then(|s| s.parse().ok())
            .expect("probe server url has a port");

        // A real, reachable target that must NOT receive the request when
        // the client honors the explicit proxy.
        let (target_url, target_hits) = spawn_probe_server("ok").await;

        let base = ProviderBase::new_with_proxy(
            "test".into(),
            &crate::test_support::test_config(&target_url, 30, None, None),
            Some(&proxy_config("http", "127.0.0.1", proxy_port)),
        )
        .expect("explicit http proxy construction succeeds");

        let url = base.protocol_url(ApiKind::ChatCompletions).unwrap();
        let body = base
            .send_json(
                &url,
                ApiKind::ChatCompletions,
                serde_json::json!({}),
                &HashMap::new(),
                None,
            )
            .await
            .expect("request routed through the explicit proxy succeeds");

        assert_eq!(body, "ok");
        assert_eq!(
            proxy_hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the request must go through the explicit proxy"
        );
        assert_eq!(
            target_hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the real target must not be contacted when proxied"
        );
    }

    // ── 200-delivered upstream error detection ───────────────────

    #[test]
    fn body_has_error_object_detects_real_error_object() {
        // A real JSON object `error` → error.
        assert!(body_has_error_object(&serde_json::json!({
            "error": { "message": "burst" }
        })));
        // `error` absent → no error.
        assert!(!body_has_error_object(&serde_json::json!({
            "choices": []
        })));
        // `error: null` is treated as absent.
        assert!(!body_has_error_object(&serde_json::json!({
            "error": null,
            "choices": []
        })));
        // Non-object error values (string / bool / array / number) must be
        // treated as absent — only an actual JSON object signals a real
        // upstream error object.
        for non_object in [
            serde_json::json!("oops"),
            serde_json::json!(true),
            serde_json::json!([1, 2]),
            serde_json::json!(0),
        ] {
            assert!(
                !body_has_error_object(&serde_json::json!({ "error": non_object })),
                "non-object error value {non_object} must be treated as absent"
            );
        }
    }

    #[test]
    fn body_error_message_extracts_human_readable_message() {
        let err = serde_json::json!({ "message": "upstream overloaded", "code": "500" });
        assert_eq!(body_error_message(&err), "upstream overloaded");
        // `msg` alias.
        assert_eq!(
            body_error_message(&serde_json::json!({ "msg": "boom" })),
            "boom"
        );
        // Falls back to the serialized object when no message field exists.
        let obj = serde_json::json!({ "code": "EMPTY" });
        assert_eq!(body_error_message(&obj), obj.to_string());
    }

    #[test]
    fn body_error_is_rate_limited_matches_expected_markers() {
        // Numeric code 429.
        assert!(body_error_is_rate_limited(&serde_json::json!({
            "code": 429,
            "message": "too many requests"
        })));
        // String code with a marker.
        assert!(body_error_is_rate_limited(&serde_json::json!({
            "code": "RateLimitExceeded",
            "message": "n/a"
        })));
        // Message-only markers (case-insensitive).
        for msg in [
            "rate limit reached",
            "BURST tokens exceeded",
            "flow control",
            "quota exceeded",
        ] {
            assert!(
                body_error_is_rate_limited(&serde_json::json!({ "message": msg })),
                "message {msg:?} should classify as rate-limited"
            );
        }
    }

    #[test]
    fn body_error_is_rate_limited_is_conservative() {
        // Generic failures must not be misclassified.
        assert!(!body_error_is_rate_limited(&serde_json::json!({
            "code": 500,
            "message": "internal server error"
        })));
        assert!(!body_error_is_rate_limited(&serde_json::json!({
            "code": "ModelNotFound",
            "message": "unknown model"
        })));
        // Numeric code != 429 is never rate-limited, even with a generic body.
        assert!(!body_error_is_rate_limited(&serde_json::json!({
            "code": 503,
            "message": "service unavailable"
        })));
    }
}
