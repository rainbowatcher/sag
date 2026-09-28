//! Shared test helpers for the admin crate.
//!
//! Previously `build_state` / `test_app` / `one_provider_config` were
//! copy-pasted across `admin_providers.rs`, `admin_routes.rs`, and
//! `admin_logs.rs`. This module holds a single canonical copy.
#![cfg(test)]

use std::collections::HashMap;

use axum::Router;
use sag_config::{
    ApiConfig, ApiKind, ConfigHandle, GatewayConfig, KeyStrategy, Pipeline, ProviderConfig,
    ProxyMode, RouteConfig, RouteTarget, Strategy,
};
use sag_state::AppState;

/// Build an in-memory `AppState` with the given providers and routes.
/// The runtime provider registry is pre-populated with `OpenAiProvider`
/// adapters for every provider in the config.
pub(crate) async fn build_state(
    providers: HashMap<String, ProviderConfig>,
    routes: Vec<RouteConfig>,
) -> AppState {
    build_state_with_proxy(providers, routes, None).await
}

/// Build an in-memory `AppState` with the given providers, routes, and an
/// optional gateway-level `proxy_url`. Same as [`build_state`] but lets
/// tests exercise proxy resolution paths.
pub(crate) async fn build_state_with_proxy(
    providers: HashMap<String, ProviderConfig>,
    routes: Vec<RouteConfig>,
    proxy_url: Option<String>,
) -> AppState {
    let config = GatewayConfig {
        providers,
        routes,
        proxy_url,
        ..sag_config::gateway_config_base!()
    };
    let handle = ConfigHandle::from_config(config);
    let provider_registry = sag_state::testing::build_provider_registry(&handle, false).await;
    sag_state::testing::test_app_state(handle, provider_registry).await
}

/// Build an `AppState` whose config handle is **file-backed** at `path`
/// (so `mutate` takes the persist/conflict path). Seeds a minimal but
/// fully-valid TOML document (`[server]` + providers + routes) that
/// `load_config` accepts — `save_config_to_file` alone cannot create
/// `[server]` on a blank path.
pub(crate) async fn build_file_backed_state(
    path: &std::path::Path,
    providers: HashMap<String, ProviderConfig>,
    routes: Vec<RouteConfig>,
) -> AppState {
    let config = GatewayConfig {
        providers,
        routes,
        ..sag_config::gateway_config_base!()
    };
    // `toml` re-serializes the whole struct (fine for a seed file; the
    // in-place `toml_edit` save path is not exercised here).
    let seed = toml::to_string(&config).expect("config must serialize to TOML");
    std::fs::write(path, seed).expect("seed config must write");
    let handle = sag_config::load_config(path).expect("config must load from path");
    let provider_registry = sag_state::testing::build_provider_registry(&handle, false).await;
    sag_state::testing::test_app_state(handle, provider_registry).await
}

/// Build a test Axum app mounting admin routes without auth middleware.
pub(crate) fn test_app(state: AppState) -> Router {
    Router::new()
        .nest("/api", crate::admin::build_admin_routes(state.clone()))
        .with_state(state)
}

/// Build a provider declaring exactly one API section `kind` at `base_url`.
///
/// Everything else is `ProviderConfig::default()` plus the shared test
/// defaults (FillFirst key strategy, default proxy mode, 120s timeout).
/// Tests that need a second section or an override patch the returned
/// `apis` map themselves.
pub(crate) fn test_provider(
    kind: ApiKind,
    base_url: &str,
    api_keys: Vec<String>,
    enabled_models: Vec<String>,
) -> ProviderConfig {
    let mut pc = ProviderConfig {
        api_keys,
        key_strategy: KeyStrategy::FillFirst,
        proxy_mode: ProxyMode::default(),
        timeout_secs: 120,
        enabled_models,
        ..ProviderConfig::default()
    };
    pc.apis.insert(
        kind,
        ApiConfig {
            base_url: base_url.into(),
            ..ApiConfig::default()
        },
    );
    pc
}

/// Convenience: create a single-provider config with optional routes.
/// Returns `(providers, routes)` ready for [`build_state`].
///
/// The provider declares a `chat_completions` section; the routes are
/// chat-pipeline routes (no `api_endpoint` — that field is image-only in
/// the new config model).
pub(crate) fn one_provider_config(
    name: &str,
    routes: Vec<(&str, &str, u32)>,
) -> (HashMap<String, ProviderConfig>, Vec<RouteConfig>) {
    let mut providers = HashMap::new();
    providers.insert(
        name.to_string(),
        test_provider(
            ApiKind::ChatCompletions,
            "https://api.openai.com/v1",
            vec!["sk-test".into()],
            vec![],
        ),
    );
    let routes = routes
        .into_iter()
        .map(|(model, real_model, prio)| RouteConfig {
            model_name: model.into(),
            pipeline: Pipeline::Chat,
            api_endpoint: None,
            retries: None,
            timeout_secs: None,
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                provider: name.into(),
                real_model: Some(real_model.into()),
                retries: None,
                timeout_secs: None,
                priority: prio,
                extra: HashMap::new(),
                capabilities: None,
            }],
        })
        .collect();
    (providers, routes)
}
