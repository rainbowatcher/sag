//! Gateway runtime-config handlers for the admin API.
//!
//! - `GET /api/config` — read-only snapshot of the current in-memory
//!   top-level configuration (server / logging / analytics / backoff / the
//!   four circuit-breaker fields including `probe_interval_secs` /
//!   routing-cache TTLs / global proxy / privacy).
//! - `PUT /api/config` — partially update the runtime-tuning subset
//!   (backoff, the four circuit-breaker fields, the two TTLs) plus the
//!   `[privacy]` section, delegating to
//!   [`ConfigMutationCoordinator::update_runtime_config`](sag_state::ConfigMutationCoordinator::update_runtime_config)
//!   so the change goes through the same atomic config + provider-registry +
//!   version commit and persistence path as every other production write.
//!   A single PUT applies every section in one mutation, so a rejected
//!   section rejects the whole request.
//!
//! `server`, `logging` and `analytics` are deliberately **not** writable
//! here: they apply at process startup. `proxy_url` is owned by
//! `PUT /api/settings`, and `providers` / `routes` have their own CRUD
//! endpoints. Unknown request fields are ignored by serde (the gateway
//! forward-compat rule).
//!
//! The privacy salt is write-only: [`PrivacySnapshot`] reports
//! `salt_configured` instead of the value (see [`UpdatePrivacyRequest`] for
//! the write rules), and no handler in this module logs a request body.
//!
//! Both are mounted under `/api` by [`crate::admin::build_admin_routes`], so
//! they are auth-protected by the `gateway_auth` middleware.

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use sag_config::{
    AnalyticsConfig, BackoffConfig, CircuitBreakerConfig, GatewayConfig, LoggingConfig,
    PrivacyBuiltins, PrivacyConfig, PrivacyCustomRegex, PrivacyMode, PrivacyWordList, ServerConfig,
};
use sag_core::AppError;
use tracing::instrument;

use sag_state::{AppState, PrivacyConfigUpdate};

/// Read-only projection of the top-level [`GatewayConfig`] sections exposed
/// by `GET /api/config` and returned by `PUT /api/config`.
///
/// Deliberately excludes `providers`, `routes` and `gateway_auth` (each has
/// its own admin endpoint).
#[derive(Debug, serde::Serialize)]
pub struct ConfigSnapshot {
    pub server: ServerConfig,
    pub logging: LoggingConfig,
    /// `null` when the `[analytics]` section is absent.
    pub analytics: Option<AnalyticsConfig>,
    pub backoff: BackoffConfig,
    pub circuit_breaker: CircuitBreakerView,
    pub shuffle_cache_ttl_secs: u64,
    pub sticky_failover_reset_secs: u64,
    /// Global outbound proxy URL; `null` when unset.
    pub proxy_url: Option<String>,
    /// Effective `[privacy]` section, with the salt reduced to a boolean.
    pub privacy: PrivacySnapshot,
}

/// The four circuit-breaker fields exposed by the admin API: the three
/// cooldown durations plus the `probe_interval_secs` half-open probe cadence.
///
/// A separate view (rather than serializing [`CircuitBreakerConfig`]
/// directly) keeps the public contract explicit; all four fields are part of
/// both the `GET` snapshot and the `PUT` full-object replacement.
#[derive(Debug, serde::Serialize)]
pub struct CircuitBreakerView {
    pub quota_exhausted_secs: u64,
    pub rate_limited_secs: u64,
    pub generic_secs: u64,
    pub probe_interval_secs: u64,
}

impl From<&CircuitBreakerConfig> for CircuitBreakerView {
    fn from(cb: &CircuitBreakerConfig) -> Self {
        Self {
            quota_exhausted_secs: cb.quota_exhausted_secs,
            rate_limited_secs: cb.rate_limited_secs,
            generic_secs: cb.generic_secs,
            probe_interval_secs: cb.probe_interval_secs,
        }
    }
}

/// Read-only projection of the `[privacy]` section exposed by
/// `GET /api/config` and returned by `PUT /api/config`.
///
/// Built by explicit field mapping (never by serializing [`PrivacyConfig`]),
/// so the salt cannot leak: the response only reports whether one is stored
/// via [`salt_configured`](Self::salt_configured) — not even a masked echo,
/// which would still confirm a guess.
#[derive(Debug, serde::Serialize)]
pub struct PrivacySnapshot {
    pub enabled: bool,
    pub mode: PrivacyMode,
    pub restore: bool,
    pub builtins: PrivacyBuiltins,
    pub custom_regexes: Vec<PrivacyCustomRegex>,
    pub custom_words: PrivacyWordList,
    pub allowlist: PrivacyWordList,
    /// `true` when a non-empty salt is stored. The salt itself is never
    /// returned.
    pub salt_configured: bool,
}

impl From<&PrivacyConfig> for PrivacySnapshot {
    fn from(privacy: &PrivacyConfig) -> Self {
        Self {
            enabled: privacy.enabled,
            mode: privacy.mode,
            restore: privacy.restore,
            builtins: privacy.builtins,
            custom_regexes: privacy.custom_regexes.clone(),
            custom_words: privacy.custom_words.clone(),
            allowlist: privacy.allowlist.clone(),
            salt_configured: !privacy.salt.is_empty(),
        }
    }
}

/// The `[privacy]` section accepted by `PUT /api/config`.
///
/// A **full-object replace** of the seven writable fields: they are all
/// required when the `privacy` object is present (a partial object fails
/// deserialization, surfacing as 422). Nested defaults keep their usual
/// config semantics — omitted `builtins` keys stay enabled, omitted `items`
/// stay empty — so a client that sends the complete object is unaffected.
///
/// `salt` is the only optional field and is write-only: omitted / `null` /
/// `""` keep the stored salt (there is no clear control), while a non-empty
/// string replaces it byte for byte (not trimmed). Normalization is left to
/// the coordinator, so the mapping below is purely positional.
///
/// Deliberately not `Debug`: this struct holds the salt in clear text, so it
/// must never reach a log line or a panic message.
#[derive(serde::Deserialize)]
pub struct UpdatePrivacyRequest {
    pub enabled: bool,
    pub mode: PrivacyMode,
    pub restore: bool,
    pub builtins: PrivacyBuiltins,
    pub custom_regexes: Vec<PrivacyCustomRegex>,
    pub custom_words: PrivacyWordList,
    pub allowlist: PrivacyWordList,
    #[serde(default)]
    pub salt: Option<String>,
}

impl UpdatePrivacyRequest {
    /// Map the HTTP DTO onto the domain command applied by the coordinator.
    fn into_update(self) -> PrivacyConfigUpdate {
        PrivacyConfigUpdate {
            enabled: self.enabled,
            mode: self.mode,
            restore: self.restore,
            builtins: self.builtins,
            custom_regexes: self.custom_regexes,
            custom_words: self.custom_words,
            allowlist: self.allowlist,
            salt: self.salt,
        }
    }
}

/// Request body for `PUT /api/config`.
///
/// Every field is optional; an omitted field is left unchanged. `backoff`
/// and `circuit_breaker` are **full-object** replacements (all their fields
/// are required when the object is present), mirroring the `api_keys`
/// replacement semantics of `PUT /api/settings`.
///
/// `server` / `logging` / `analytics` / `proxy_url` / `providers` /
/// `routes` are intentionally not declared: serde silently ignores them, so
/// a client cannot mutate the restart-required or separately-owned sections
/// through this endpoint.
///
/// Deliberately not `Debug` (it contains [`UpdatePrivacyRequest`], which
/// carries the salt): the handler skips this argument in its `instrument`
/// span and nothing else may format it.
#[derive(serde::Deserialize)]
pub struct UpdateConfigRequest {
    #[serde(default)]
    pub backoff: Option<BackoffConfig>,
    #[serde(default)]
    pub circuit_breaker: Option<CircuitBreakerUpdate>,
    #[serde(default)]
    pub shuffle_cache_ttl_secs: Option<u64>,
    #[serde(default)]
    pub sticky_failover_reset_secs: Option<u64>,
    /// `[privacy]` full-object replace; omitted / `null` leaves the section
    /// untouched (so runtime-only updates never rewrite it).
    #[serde(default)]
    pub privacy: Option<UpdatePrivacyRequest>,
}

/// The circuit-breaker fields accepted by `PUT /api/config`.
///
/// All four fields (including `probe_interval_secs`) are required when the
/// object is present (full-object replace).
#[derive(Debug, serde::Deserialize)]
pub struct CircuitBreakerUpdate {
    pub quota_exhausted_secs: u64,
    pub rate_limited_secs: u64,
    pub generic_secs: u64,
    pub probe_interval_secs: u64,
}

/// Build a [`ConfigSnapshot`] from a live config, cloning the sections so
/// the response never borrows a held `config.read()` guard.
fn snapshot(config: &GatewayConfig) -> ConfigSnapshot {
    ConfigSnapshot {
        server: config.server.clone(),
        logging: config.logging.clone(),
        analytics: config.analytics.clone(),
        backoff: config.backoff.clone(),
        circuit_breaker: CircuitBreakerView::from(&config.circuit_breaker),
        shuffle_cache_ttl_secs: config.shuffle_cache_ttl_secs,
        sticky_failover_reset_secs: config.sticky_failover_reset_secs,
        proxy_url: config.proxy_url.clone(),
        privacy: PrivacySnapshot::from(&config.privacy),
    }
}

/// `GET /api/config`
///
/// Returns a read-only [`ConfigSnapshot`] of the effective in-memory config.
/// The response is built from cloned sections, so it never borrows a held
/// `config.read()` guard.
#[instrument(skip(state))]
pub async fn get_config(State(state): State<AppState>) -> impl IntoResponse {
    let snapshot = {
        let config = state.config.read().await;
        snapshot(&config)
    };
    (StatusCode::OK, Json(snapshot)).into_response()
}

/// `PUT /api/config`
///
/// Applies a partial update of the runtime-tuning subset and returns the
/// resulting [`ConfigSnapshot`] (same shape as `GET /api/config`).
///
/// Delegates to [`ConfigMutationCoordinator::update_runtime_config`](sag_state::ConfigMutationCoordinator::update_runtime_config),
/// so the write goes through the unified mutation protocol: the mutation
/// mutex, full-config validation, atomic config commit (+ `routes_version`
/// bump), `toml_edit` persistence with external-edit conflict detection, and
/// the provider-registry sync. This handler never writes `state.config` or
/// saves files directly.
///
/// `circuit_breaker` is a full-object replace of the four public fields
/// (the three cooldown durations plus `probe_interval_secs`); every field is
/// required when the object is present. Validation failures
/// (non-positive cooldowns, `backoff.max_ms < backoff.base_ms`, …) surface as
/// [`AppError::Config`] from the coordinator and are reported as **400**
/// `BAD_REQUEST` rather than 500.
///
/// `privacy` is likewise a full-object replace of the seven writable fields;
/// its `salt` is write-only (`[`UpdatePrivacyRequest`]`). Regex patterns are
/// compiled by the coordinator's validation, so an uncompilable pattern is a
/// **400** naming `privacy.custom_regexes[i]`; an unknown `mode` never gets
/// that far and is rejected while deserializing the body (422).
///
/// A successful commit bumps `routes_version`; the gateway's stale-probe
/// threshold watcher observes that counter and refreshes the circuit breaker's
/// threshold from the new config — the same path hot-reload and every other
/// admin mutation use, so no direct breaker write happens here.
///
/// The body is skipped in the span (it may contain the salt) and the mapped
/// command is never logged.
#[instrument(skip(state, req))]
pub async fn update_config(
    State(state): State<AppState>,
    Json(req): Json<UpdateConfigRequest>,
) -> impl IntoResponse {
    let circuit_breaker = req.circuit_breaker.map(|cb| CircuitBreakerConfig {
        quota_exhausted_secs: cb.quota_exhausted_secs,
        rate_limited_secs: cb.rate_limited_secs,
        generic_secs: cb.generic_secs,
        probe_interval_secs: cb.probe_interval_secs,
    });
    let privacy = req.privacy.map(UpdatePrivacyRequest::into_update);

    // One mutation for the whole body: privacy and the runtime-tuning fields
    // are validated and committed together, so a failure in either leaves the
    // committed config untouched.
    match state
        .coordinator
        .update_runtime_config(
            req.backoff,
            circuit_breaker,
            req.shuffle_cache_ttl_secs,
            req.sticky_failover_reset_secs,
            privacy,
        )
        .await
    {
        Ok(_report) => {
            let config = state.config.read().await;
            (StatusCode::OK, Json(snapshot(&config))).into_response()
        }
        Err(e) => match e {
            // Invalid backoff / circuit-breaker values are a request-level
            // error, not a server fault.
            AppError::Config(msg) => AppError::BadRequest(msg).into_response(),
            other => other.into_response(),
        },
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    use crate::test_utils::{build_state_with_proxy, one_provider_config, test_app};

    async fn json_body(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 65_536)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn get_config(app: &axum::Router) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn put_config(app: &axum::Router, body: &str) -> axum::response::Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/config")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn get_config_returns_readonly_snapshot() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state =
            build_state_with_proxy(providers, routes, Some("http://proxy.example:8080".into()))
                .await;
        let app = test_app(state);

        let response = get_config(&app).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;

        // Read-only, restart-required / separately-owned sections.
        assert_eq!(body["server"]["host"], "127.0.0.1");
        assert_eq!(body["server"]["port"], 3000);
        assert_eq!(body["logging"]["level"], "info");
        assert_eq!(body["analytics"], serde_json::Value::Null);
        assert_eq!(body["proxy_url"], "http://proxy.example:8080");

        // Runtime-tuning sections.
        assert_eq!(body["backoff"]["base_ms"], 100);
        assert_eq!(body["backoff"]["max_ms"], 5000);
        assert_eq!(body["shuffle_cache_ttl_secs"], 7200);
        assert_eq!(body["sticky_failover_reset_secs"], 7200);

        // The circuit-breaker view exposes all four fields, including the
        // `probe_interval_secs` probe cadence.
        assert_eq!(body["circuit_breaker"]["quota_exhausted_secs"], 3600);
        assert_eq!(body["circuit_breaker"]["rate_limited_secs"], 10);
        assert_eq!(body["circuit_breaker"]["generic_secs"], 60);
        assert_eq!(body["circuit_breaker"]["probe_interval_secs"], 60);
    }

    #[tokio::test]
    async fn put_config_applies_all_runtime_sections() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state_with_proxy(providers, routes, None).await;
        // Seed a non-default probe cadence so the full-object replacement
        // assertion is meaningful (the PUT value must win).
        {
            let mut cfg = state.config.write().await;
            cfg.circuit_breaker.probe_interval_secs = 45;
        }
        let app = test_app(state.clone());

        let response = put_config(
            &app,
            r#"{
                "backoff": {"retries": 5, "base_ms": 200, "max_ms": 4000, "jitter": false, "retry_on_empty_response": false},
                "circuit_breaker": {"quota_exhausted_secs": 120, "rate_limited_secs": 20, "generic_secs": 30, "probe_interval_secs": 999},
                "shuffle_cache_ttl_secs": 111,
                "sticky_failover_reset_secs": 222
            }"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;

        // Response mirrors the committed config.
        assert_eq!(body["backoff"]["retries"], 5);
        assert_eq!(body["circuit_breaker"]["quota_exhausted_secs"], 120);
        assert_eq!(body["circuit_breaker"]["probe_interval_secs"], 999);
        assert_eq!(body["shuffle_cache_ttl_secs"], 111);
        assert_eq!(body["sticky_failover_reset_secs"], 222);

        let cfg = state.config.read().await;
        assert_eq!(cfg.backoff.retries, 5);
        assert_eq!(cfg.backoff.base_ms, 200);
        assert_eq!(cfg.backoff.max_ms, 4000);
        assert!(!cfg.backoff.jitter);
        assert!(!cfg.backoff.retry_on_empty_response);
        assert_eq!(cfg.circuit_breaker.quota_exhausted_secs, 120);
        assert_eq!(cfg.circuit_breaker.rate_limited_secs, 20);
        assert_eq!(cfg.circuit_breaker.generic_secs, 30);
        assert_eq!(
            cfg.circuit_breaker.probe_interval_secs, 999,
            "full-object breaker replace must apply the request probe cadence"
        );
        assert_eq!(cfg.shuffle_cache_ttl_secs, 111);
        assert_eq!(cfg.sticky_failover_reset_secs, 222);
    }

    #[tokio::test]
    async fn put_config_cannot_write_readonly_or_separately_owned_sections() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state =
            build_state_with_proxy(providers, routes, Some("http://proxy.example:8080".into()))
                .await;
        let app = test_app(state.clone());

        let response = put_config(
            &app,
            r#"{
                "server": {"host": "0.0.0.0", "port": 9999},
                "logging": {"level": "trace", "format": "json"},
                "analytics": {"enabled": true, "storage_dsn": "sqlite://x.db"},
                "proxy_url": "http://evil.example:9999",
                "backoff": {"retries": 3, "base_ms": 100, "max_ms": 5000, "jitter": true, "retry_on_empty_response": true}
            }"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        assert_eq!(cfg.server.host, "127.0.0.1");
        assert_eq!(cfg.server.port, 3000);
        assert_eq!(cfg.logging.level, "info");
        assert!(cfg.analytics.is_none());
        assert_eq!(
            cfg.proxy_url.as_deref(),
            Some("http://proxy.example:8080"),
            "proxy_url is owned by PUT /api/settings and must be ignored here"
        );
        assert_eq!(cfg.backoff.retries, 3);
    }

    #[tokio::test]
    async fn put_config_invalid_backoff_is_400_and_unchanged() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state_with_proxy(providers, routes, None).await;
        let app = test_app(state.clone());

        let response = put_config(
            &app,
            r#"{"backoff": {"retries": 2, "base_ms": 0, "max_ms": 5000, "jitter": true, "retry_on_empty_response": true}}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = json_body(response).await;
        assert_eq!(body["error"]["code"], "BAD_REQUEST");

        let cfg = state.config.read().await;
        assert_eq!(
            cfg.backoff.base_ms, 100,
            "rejected mutation must not commit"
        );
    }

    #[tokio::test]
    async fn put_config_invalid_circuit_breaker_is_400_and_unchanged() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state_with_proxy(providers, routes, None).await;
        let app = test_app(state.clone());

        let response = put_config(
            &app,
            r#"{"circuit_breaker": {"quota_exhausted_secs": 0, "rate_limited_secs": 10, "generic_secs": 60, "probe_interval_secs": 60}}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = json_body(response).await;
        assert_eq!(body["error"]["code"], "BAD_REQUEST");

        let cfg = state.config.read().await;
        assert_eq!(
            cfg.circuit_breaker.quota_exhausted_secs, 3600,
            "rejected mutation must not commit"
        );
    }

    #[tokio::test]
    async fn put_config_partial_circuit_breaker_object_is_rejected() {
        // Full-object replace: every field (including `probe_interval_secs`)
        // is required when the object is present, so a partial object fails
        // deserialization.
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state_with_proxy(providers, routes, None).await;
        let app = test_app(state.clone());

        let response = put_config(
            &app,
            r#"{"circuit_breaker": {"quota_exhausted_secs": 120, "rate_limited_secs": 20, "generic_secs": 30}}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let cfg = state.config.read().await;
        assert_eq!(cfg.circuit_breaker.generic_secs, 60);
    }

    #[tokio::test]
    async fn put_config_full_circuit_breaker_round_trips_probe_cadence() {
        // All four breaker fields present: the full-object replace is applied
        // and `GET /api/config` round-trips the new cadence.
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state_with_proxy(providers, routes, None).await;
        let app = test_app(state.clone());

        let response = put_config(
            &app,
            r#"{"circuit_breaker": {"quota_exhausted_secs": 120, "rate_limited_secs": 20, "generic_secs": 30, "probe_interval_secs": 15}}"#,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let cfg = state.config.read().await;
        assert_eq!(cfg.circuit_breaker.quota_exhausted_secs, 120);
        assert_eq!(cfg.circuit_breaker.rate_limited_secs, 20);
        assert_eq!(cfg.circuit_breaker.generic_secs, 30);
        assert_eq!(cfg.circuit_breaker.probe_interval_secs, 15);
        drop(cfg);

        let response = get_config(&app).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["circuit_breaker"]["probe_interval_secs"], 15);
        assert_eq!(body["circuit_breaker"]["quota_exhausted_secs"], 120);
    }
}

/// Pure serde / snapshot tests for the privacy DTOs.
///
/// Kept apart from the router tests above: these touch no `AppState`, no file
/// system and no HTTP surface — the request-lifecycle coverage (GET/PUT,
/// rollback, salt preservation) lives in `tests/privacy_config.rs`.
#[cfg(test)]
mod privacy_dto_tests {
    use sag_config::{
        PrivacyBuiltins, PrivacyConfig, PrivacyCustomRegex, PrivacyMode, PrivacyWordList,
    };
    use serde_json::json;

    use super::{PrivacySnapshot, UpdateConfigRequest, UpdatePrivacyRequest};

    /// Unique marker: the serialized snapshot must never contain it.
    const SECRET_SALT: &str = "s3cret-salt-marker";

    fn populated_privacy() -> PrivacyConfig {
        PrivacyConfig {
            enabled: true,
            mode: PrivacyMode::Block,
            restore: false,
            salt: SECRET_SALT.into(),
            builtins: PrivacyBuiltins {
                phone: false,
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

    /// A complete `privacy` object with every non-salt top-level field.
    fn complete_privacy_json() -> serde_json::Value {
        json!({
            "enabled": false,
            "mode": "mask",
            "restore": true,
            "builtins": {},
            "custom_regexes": [],
            "custom_words": {},
            "allowlist": {}
        })
    }

    #[test]
    fn snapshot_maps_every_field_and_never_serializes_the_salt() {
        let value = serde_json::to_value(PrivacySnapshot::from(&populated_privacy())).unwrap();

        assert_eq!(value["enabled"], json!(true));
        assert_eq!(value["mode"], json!("block"));
        assert_eq!(value["restore"], json!(false));
        // Nested config shapes are the wire shapes: all seven detectors, the
        // `{kind, pattern, case_insensitive}` rule and the wrapped `items`.
        assert_eq!(value["builtins"]["email"], json!(true));
        assert_eq!(value["builtins"]["phone"], json!(false));
        assert_eq!(value["custom_regexes"][0]["kind"], json!("employee_id"));
        assert_eq!(value["custom_regexes"][0]["pattern"], json!(r"EMP-\d{4}"));
        assert_eq!(value["custom_regexes"][0]["case_insensitive"], json!(true));
        assert_eq!(value["custom_words"]["items"], json!(["alpha", "alpha"]));
        assert_eq!(value["allowlist"]["items"], json!(["beta"]));
        assert_eq!(value["salt_configured"], json!(true));

        // No `salt` key, no salt value, not even masked.
        assert!(
            value.get("salt").is_none(),
            "snapshot must not expose `salt`"
        );
        let encoded = value.to_string();
        assert!(
            !encoded.contains(SECRET_SALT),
            "snapshot must not echo the salt in any form: {encoded}"
        );
        assert_eq!(
            encoded.matches("salt").count(),
            1,
            "only `salt_configured` may mention salt: {encoded}"
        );
    }

    #[test]
    fn snapshot_salt_configured_only_reports_a_non_empty_salt() {
        assert!(
            !PrivacySnapshot::from(&PrivacyConfig::default()).salt_configured,
            "the default section has no salt"
        );

        let privacy = PrivacyConfig {
            salt: "   ".into(),
            ..PrivacyConfig::default()
        };
        assert!(
            PrivacySnapshot::from(&privacy).salt_configured,
            "whitespace is a literal non-empty salt, not an absent one"
        );
    }

    #[test]
    fn update_request_requires_the_seven_writable_fields() {
        assert!(serde_json::from_value::<UpdatePrivacyRequest>(complete_privacy_json()).is_ok());

        for field in [
            "enabled",
            "mode",
            "restore",
            "builtins",
            "custom_regexes",
            "custom_words",
            "allowlist",
        ] {
            let mut partial = complete_privacy_json();
            partial.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<UpdatePrivacyRequest>(partial).is_err(),
                "`{field}` must be required by the full-object replace"
            );
        }
    }

    #[test]
    fn update_request_keeps_the_salt_verbatim_and_optional() {
        let omitted: UpdatePrivacyRequest =
            serde_json::from_value(complete_privacy_json()).unwrap();
        assert_eq!(omitted.salt, None, "omitted salt keeps the stored one");

        let mut null_salt = complete_privacy_json();
        null_salt["salt"] = json!(null);
        let null_salt: UpdatePrivacyRequest = serde_json::from_value(null_salt).unwrap();
        assert_eq!(null_salt.salt, None, "null salt keeps the stored one");

        let mut empty_salt = complete_privacy_json();
        empty_salt["salt"] = json!("");
        let empty_salt: UpdatePrivacyRequest = serde_json::from_value(empty_salt).unwrap();
        assert_eq!(empty_salt.salt.as_deref(), Some(""));
        // `""` is forwarded as-is; the coordinator decides that it keeps the
        // stored value, so the DTO never normalizes the clear/skip cases.

        let mut spaced_salt = complete_privacy_json();
        spaced_salt["salt"] = json!(" new-salt ");
        let spaced_salt: UpdatePrivacyRequest = serde_json::from_value(spaced_salt).unwrap();
        assert_eq!(
            spaced_salt.salt.as_deref(),
            Some(" new-salt "),
            "salt must be forwarded byte for byte, without trimming"
        );
    }

    #[test]
    fn update_request_tolerates_unknown_fields_but_rejects_unknown_mode() {
        let mut with_unknown = complete_privacy_json();
        with_unknown["future_flag"] = json!({"nested": true});
        with_unknown["builtins"]["face_id"] = json!(false);
        with_unknown["custom_words"]["future_items"] = json!(["x"]);
        let parsed: UpdatePrivacyRequest =
            serde_json::from_value(with_unknown).expect("unknown fields are tolerated");
        assert_eq!(
            parsed.builtins,
            PrivacyBuiltins::default(),
            "an unknown builtin toggle must not flip a known one"
        );

        let mut bad_mode = complete_privacy_json();
        bad_mode["mode"] = json!("enabled");
        assert!(
            serde_json::from_value::<UpdatePrivacyRequest>(bad_mode).is_err(),
            "unknown privacy modes are rejected at deserialization (422)"
        );
    }

    #[test]
    fn update_config_request_leaves_privacy_alone_when_absent_or_null() {
        let omitted: UpdateConfigRequest = serde_json::from_str(r#"{}"#).unwrap();
        assert!(omitted.privacy.is_none());

        let explicit_null: UpdateConfigRequest =
            serde_json::from_str(r#"{"privacy": null}"#).unwrap();
        assert!(explicit_null.privacy.is_none());

        let runtime_only: UpdateConfigRequest =
            serde_json::from_str(r#"{"shuffle_cache_ttl_secs": 111}"#).unwrap();
        assert!(runtime_only.privacy.is_none());

        let with_unknown: UpdateConfigRequest =
            serde_json::from_str(r#"{"server": {"port": 9}, "privacy": {"enabled": false, "mode": "warn", "restore": true, "builtins": {}, "custom_regexes": [], "custom_words": {}, "allowlist": {}}}"#)
                .unwrap();
        assert!(
            with_unknown.privacy.is_some(),
            "unknown sections are ignored"
        );
    }

    #[test]
    fn update_request_maps_field_by_field_onto_the_state_command() {
        let req: UpdateConfigRequest = serde_json::from_str(
            r#"{
                "privacy": {
                    "enabled": true,
                    "mode": "warn",
                    "restore": false,
                    "builtins": {"email": false, "phone": true, "id_card_cn": true, "bank_card": true, "iban": true, "api_key": true, "private_key": true},
                    "custom_regexes": [{"kind": "employee_id", "pattern": "EMP-\\d{4}", "case_insensitive": true}],
                    "custom_words": {"items": ["alpha", "alpha"]},
                    "allowlist": {"items": ["beta"]},
                    "salt": " new-salt "
                }
            }"#,
        )
        .unwrap();

        // `PrivacyConfigUpdate` is intentionally not `Debug`, so the mapping
        // is asserted field by field.
        let update = req.privacy.unwrap().into_update();
        assert!(update.enabled);
        assert_eq!(update.mode, PrivacyMode::Warn);
        assert!(!update.restore);
        assert_eq!(
            update.builtins,
            PrivacyBuiltins {
                email: false,
                ..PrivacyBuiltins::default()
            }
        );
        assert_eq!(
            update.custom_regexes,
            vec![PrivacyCustomRegex {
                pattern: r"EMP-\d{4}".into(),
                kind: "employee_id".into(),
                case_insensitive: true,
            }]
        );
        assert_eq!(update.custom_words.items, vec!["alpha", "alpha"]);
        assert_eq!(update.allowlist.items, vec!["beta"]);
        assert_eq!(
            update.salt.as_deref(),
            Some(" new-salt "),
            "the salt reaches the command unchanged"
        );
    }
}
