//! Shared test helpers for the admin crate.
//!
//! Previously `build_state` / `test_app` / `one_provider_config` were
//! copy-pasted across `admin_providers.rs`, `admin_routes.rs`, and
//! `admin_logs.rs`. This module holds a single canonical copy.
#![cfg(test)]

use std::collections::HashMap;

use axum::Router;
use sag_config::{
    ConfigHandle, GatewayConfig, KeyStrategy, ProviderConfig, ProviderType, ProxyMode, RouteConfig,
    RouteTarget, Strategy,
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

/// Build a test Axum app mounting admin routes without auth middleware.
pub(crate) fn test_app(state: AppState) -> Router {
    Router::new()
        .nest("/api", crate::admin::build_admin_routes(state.clone()))
        .with_state(state)
}

/// Convenience: create a single-provider config with optional routes.
/// Returns `(providers, routes)` ready for [`build_state`].
pub(crate) fn one_provider_config(
    name: &str,
    routes: Vec<(&str, &str, u32)>,
) -> (HashMap<String, ProviderConfig>, Vec<RouteConfig>) {
    let mut providers = HashMap::new();
    providers.insert(
        name.to_string(),
        ProviderConfig {
            provider_type: ProviderType::OpenAI,
            api_keys: vec!["sk-test".into()],
            key_strategy: KeyStrategy::FillFirst,
            base_url: "https://api.openai.com/v1".into(),
            proxy_mode: ProxyMode::default(),
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
        },
    );
    let routes = routes
        .into_iter()
        .map(|(model, real_model, prio)| RouteConfig {
            model_name: model.into(),
            api_endpoint: "/chat/completions".into(),
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
            }],
        })
        .collect();
    (providers, routes)
}
