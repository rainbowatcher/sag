//! Gateway-level settings handlers for the admin API.
//!
//! - `GET /api/settings` — read-only snapshot of the current in-memory
//!   settings (the gateway-level `proxy_url` and client-auth `api_keys`).
//! - `PUT /api/settings` — update settings, delegating to the
//!   [`ConfigMutationCoordinator`](sag_state::ConfigMutationCoordinator) so
//!   the change goes through the same atomic config + provider-registry +
//!   version commit and persistence path as every other production write.
//!
//! Both are mounted under `/api` by [`crate::admin::build_admin_routes`], so
//! they are auth-protected by the `gateway_auth` middleware.

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use sag_core::AppError;
use serde_json::json;
use tracing::instrument;

use sag_state::AppState;

/// Request body for `PUT /api/settings`.
///
/// - `proxy_url` is an explicit HTTP/HTTPS proxy URL, or `null` / `""` /
///   whitespace-only to clear the global proxy. The handler never stores
///   credentials: the config layer rejects userinfo-bearing URLs outright.
/// - `api_keys`, when present, replaces the gateway client-auth key list.
///   An empty array disables authentication (same as removing
///   `[gateway_auth]`). When the field is omitted the current key list is
///   left unchanged.
#[derive(Debug, serde::Deserialize)]
pub struct UpdateSettingsRequest {
    pub proxy_url: Option<String>,
    #[serde(default)]
    pub api_keys: Option<Vec<String>>,
}

/// GET /api/settings — return the current in-memory settings.
///
/// Reads the live `state.config` value (never the file, never a provider's
/// saved mode). The config layer already rejects userinfo credentials, so
/// echoing the stored proxy cannot leak credentials.
#[instrument(skip(state))]
pub async fn get_settings(State(state): State<AppState>) -> impl IntoResponse {
    let config = state.config.read().await;
    let api_keys: Vec<String> = config
        .gateway_auth
        .as_ref()
        .map(|auth| auth.api_keys.clone())
        .unwrap_or_default();
    (
        StatusCode::OK,
        Json(json!({
            "proxy_url": config.proxy_url,
            "api_keys": api_keys,
        })),
    )
        .into_response()
}

/// PUT /api/settings — update gateway-level settings.
///
/// Delegates to [`ConfigMutationCoordinator::update_settings`](sag_state::ConfigMutationCoordinator::update_settings)
/// which normalizes empty/whitespace `proxy_url` to "no proxy", validates
/// the URL up front, applies the optional `api_keys` replacement, and
/// atomically commits config + provider registry + version (rebuilding
/// adapters whose effective proxy changed) and persists to disk when the
/// config is file-backed. This handler never writes `state.config` or
/// saves files directly.
///
/// Malformed proxy URLs and `proxy_mode = "enabled"` providers left without
/// a global proxy surface as [`AppError::Config`] from the coordinator; the
/// admin API reports those as **400** `BAD_REQUEST` rather than 500.
#[instrument(skip(state), fields(proxy_url, api_keys_supplied))]
pub async fn update_settings(
    State(state): State<AppState>,
    Json(req): Json<UpdateSettingsRequest>,
) -> impl IntoResponse {
    // Never log the URL itself (could be mis-typed with embedded
    // credentials) or the key material; log only whether values were
    // supplied.
    tracing::Span::current().record("proxy_url", req.proxy_url.is_some());
    tracing::Span::current().record("api_keys_supplied", req.api_keys.is_some());

    match state
        .coordinator
        .update_settings(req.proxy_url, req.api_keys)
        .await
    {
        Ok(report) => {
            let config = state.config.read().await;
            let api_keys: Vec<String> = config
                .gateway_auth
                .as_ref()
                .map(|auth| auth.api_keys.clone())
                .unwrap_or_default();
            (
                StatusCode::OK,
                Json(json!({
                    "proxy_url": config.proxy_url,
                    "api_keys": api_keys,
                    "report": {
                        "providers_added": report.providers_added,
                        "providers_rebuilt": report.providers_rebuilt,
                        "providers_removed": report.providers_removed,
                        "routes_changed": report.routes_changed,
                        "no_op": report.no_op,
                    }
                })),
            )
                .into_response()
        }
        Err(e) => match e {
            // Invalid proxy URL / `enabled`-mode conflict / blank API key:
            // a request-level error, not a server fault.
            AppError::Config(msg) => AppError::BadRequest(msg).into_response(),
            other => other.into_response(),
        },
    }
}

#[cfg(test)]
mod tests {
    use crate::test_utils::{build_state_with_proxy, one_provider_config, test_app};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use sag_config::{KeyStrategy, ProviderConfig, ProviderType, ProxyMode};
    use std::collections::HashMap;
    use tower::ServiceExt;

    async fn json_body(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 65_536)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn put_settings(app: &axum::Router, body: &str) -> axum::response::Response {
        let req = Request::builder()
            .method("PUT")
            .uri("/api/settings")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        app.clone().oneshot(req).await.unwrap()
    }

    fn enabled_provider() -> ProviderConfig {
        ProviderConfig {
            provider_type: ProviderType::OpenAI,
            api_keys: vec!["sk-test".into()],
            key_strategy: KeyStrategy::FillFirst,
            base_url: "https://api.openai.com/v1".into(),
            proxy_mode: ProxyMode::Enabled,
            models_endpoint: "models".into(),
            timeout_secs: 120,
            headers: HashMap::new(),
            body_overrides: HashMap::new(),
            strip_fields: vec![],
            model_strip_fields: HashMap::new(),
            model_overrides: HashMap::new(),
            role_mappings: HashMap::new(),
            enabled_models: vec![],
            enabled: true,
            anthropic_cache_control: true,
            chunk_timeout_secs: None,
            ttfb_timeout_secs: None,
            upstream_protocol: None,
        }
    }

    #[tokio::test]
    async fn get_settings_returns_current_proxy_url() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state =
            build_state_with_proxy(providers, routes, Some("http://proxy.example:8080".into()))
                .await;
        let app = test_app(state);

        let req = Request::builder()
            .method("GET")
            .uri("/api/settings")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["proxy_url"], "http://proxy.example:8080");
    }

    #[tokio::test]
    async fn get_settings_returns_null_when_no_proxy() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state_with_proxy(providers, routes, None).await;
        let app = test_app(state);

        let req = Request::builder()
            .method("GET")
            .uri("/api/settings")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["proxy_url"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn put_settings_sets_proxy_url() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state_with_proxy(providers, routes, None).await;
        let app = test_app(state.clone());

        let response = put_settings(&app, r#"{"proxy_url":"http://proxy.example:8080"}"#).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["proxy_url"], "http://proxy.example:8080");

        let cfg = state.config.read().await;
        assert_eq!(cfg.proxy_url.as_deref(), Some("http://proxy.example:8080"));
    }

    #[tokio::test]
    async fn put_settings_empty_string_clears_proxy() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state =
            build_state_with_proxy(providers, routes, Some("http://proxy.example:8080".into()))
                .await;
        let app = test_app(state.clone());

        let response = put_settings(&app, r#"{"proxy_url":""}"#).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["proxy_url"], serde_json::Value::Null);

        let cfg = state.config.read().await;
        assert_eq!(cfg.proxy_url, None);
    }

    #[tokio::test]
    async fn put_settings_null_clears_proxy() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state =
            build_state_with_proxy(providers, routes, Some("http://proxy.example:8080".into()))
                .await;
        let app = test_app(state.clone());

        let response = put_settings(&app, r#"{"proxy_url":null}"#).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["proxy_url"], serde_json::Value::Null);

        let cfg = state.config.read().await;
        assert_eq!(cfg.proxy_url, None);
    }

    #[tokio::test]
    async fn put_settings_invalid_url_is_400() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state_with_proxy(providers, routes, None).await;
        let app = test_app(state.clone());

        let response = put_settings(&app, r#"{"proxy_url":"not a url"}"#).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = json_body(response).await;
        assert_eq!(body["error"]["code"], "BAD_REQUEST");

        // The invalid value must not be committed.
        let cfg = state.config.read().await;
        assert_eq!(cfg.proxy_url, None);
    }

    #[tokio::test]
    async fn put_settings_unsupported_scheme_is_400() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state_with_proxy(providers, routes, None).await;
        let app = test_app(state.clone());

        let response = put_settings(&app, r#"{"proxy_url":"ftp://proxy.example"}"#).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = json_body(response).await;
        assert_eq!(body["error"]["code"], "BAD_REQUEST");

        let cfg = state.config.read().await;
        assert_eq!(cfg.proxy_url, None);
    }

    #[tokio::test]
    async fn put_settings_enabled_conflict_is_400_and_config_unchanged() {
        // A provider with proxy_mode="enabled" requires the global proxy;
        // clearing it must fail with 400 (not 500) and leave config intact.
        let mut providers = HashMap::new();
        providers.insert("openai".into(), enabled_provider());
        let state =
            build_state_with_proxy(providers, vec![], Some("http://proxy.example:8080".into()))
                .await;
        let app = test_app(state.clone());

        let response = put_settings(&app, r#"{"proxy_url":null}"#).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = json_body(response).await;
        assert_eq!(body["error"]["code"], "BAD_REQUEST");

        let cfg = state.config.read().await;
        assert_eq!(
            cfg.proxy_url.as_deref(),
            Some("http://proxy.example:8080"),
            "failed mutation must not commit"
        );
        assert_eq!(cfg.providers["openai"].proxy_mode, ProxyMode::Enabled);
    }

    #[tokio::test]
    async fn put_settings_rebuilds_affected_provider_adapters() {
        // Changing the global proxy rebuilds inherit-mode adapters through
        // the coordinator (surfaced in the report) — proving the handler
        // goes through the coordinator's registry diff rather than writing
        // config directly.
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state_with_proxy(providers, routes, None).await;
        let app = test_app(state.clone());

        let response = put_settings(&app, r#"{"proxy_url":"http://proxy.example:8080"}"#).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(
            body["report"]["providers_rebuilt"],
            serde_json::json!(["openai"])
        );
        assert_eq!(body["report"]["no_op"], false);

        // The runtime registry still holds the rebuilt adapter.
        assert!(state.providers.read().await.contains_key("openai"));
    }

    #[tokio::test]
    async fn get_settings_returns_empty_api_keys_when_auth_absent() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state_with_proxy(providers, routes, None).await;
        // The shared fixture ships with a default key; clear it so this test
        // exercises the "no auth configured" path.
        {
            let mut cfg = state.config.write().await;
            cfg.gateway_auth = None;
        }
        let app = test_app(state);

        let req = Request::builder()
            .method("GET")
            .uri("/api/settings")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["api_keys"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn put_settings_manages_api_keys() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state_with_proxy(providers, routes, None).await;
        let app = test_app(state.clone());

        // Install keys.
        let response = put_settings(&app, r#"{"proxy_url":null,"api_keys":["sk-a","sk-b"]}"#).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["api_keys"], serde_json::json!(["sk-a", "sk-b"]));

        // Omitting api_keys leaves them unchanged (proxy-only update).
        let response = put_settings(&app, r#"{"proxy_url":"http://proxy.example:8080"}"#).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["api_keys"], serde_json::json!(["sk-a", "sk-b"]));
        assert_eq!(body["proxy_url"], "http://proxy.example:8080");

        // Empty list disables auth.
        let response = put_settings(&app, r#"{"proxy_url":null,"api_keys":[]}"#).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert_eq!(body["api_keys"], serde_json::json!([]));

        let cfg = state.config.read().await;
        assert!(cfg.gateway_auth.is_none());
    }

    #[tokio::test]
    async fn put_settings_blank_api_key_is_400() {
        let (providers, routes) = one_provider_config("openai", vec![]);
        let state = build_state_with_proxy(providers, routes, None).await;
        // Start from "no auth" so a rejected mutation cannot leave a fixture
        // default key looking like a successful commit.
        {
            let mut cfg = state.config.write().await;
            cfg.gateway_auth = None;
        }
        let app = test_app(state.clone());

        let response = put_settings(&app, r#"{"proxy_url":null,"api_keys":["sk-ok",""]}"#).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = json_body(response).await;
        assert_eq!(body["error"]["code"], "BAD_REQUEST");

        let cfg = state.config.read().await;
        assert!(
            cfg.gateway_auth.is_none(),
            "rejected mutation must not commit keys"
        );
    }
}
