//! Handlers for routes that are not part of the chat/image dispatch:
//! `/health`, `/v1/models`, `/v1/analytics/stats`.

pub mod analytics_v2;

use axum::{
    Json,
    body::Body,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use sag_core::AppError;
use serde::Deserialize;
use serde_json::json;
use tracing::{error, instrument};

use sag_analytics::StatsFilter;
use sag_state::AppState;

#[derive(Deserialize)]
pub struct StatsQuery {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub real_model: Option<String>,
}

/// GET `/health`
pub async fn show_health() -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Whether a route target can serve traffic under the current provider
/// config: the provider exists, is enabled, and (when `enabled_models` is
/// non-empty) admits the target's model. A target pointing at a provider
/// that is **absent** from the config counts as eligible — a preserved
/// legacy quirk of the `/v1/models` aggregation, kept byte-for-byte.
fn target_is_eligible(
    providers: &std::collections::HashMap<String, sag_config::ProviderConfig>,
    route: &sag_config::RouteConfig,
    target: &sag_config::RouteTarget,
) -> bool {
    providers
        .get(&target.provider)
        .map(|pc| {
            if !pc.enabled {
                return false;
            }
            if !pc.enabled_models.is_empty() {
                let model_to_check = target.real_model.as_deref().unwrap_or(&route.model_name);
                if model_to_check != "*" && !pc.enabled_models.iter().any(|m| m == model_to_check) {
                    return false;
                }
            }
            true
        })
        .unwrap_or(true)
}

/// GET `/v1/models`
///
/// Returns the list of available models in OpenAI-compatible format.
/// The model list is derived from the routing table — each route's
/// `model_name` is surfaced as a model `id`. Routes whose every target
/// points at a disabled provider are excluded.
#[instrument(skip(state))]
pub async fn list_models(State(state): State<AppState>) -> Json<serde_json::Value> {
    let config = state.config.read().await;

    let data: Vec<serde_json::Value> = config
        .routes
        .iter()
        .filter(|route| {
            route
                .targets
                .iter()
                .any(|t| target_is_eligible(&config.providers, route, t))
        })
        .map(|route| {
            let owned_by = route
                .targets
                .first()
                .map(|t| t.provider.as_str())
                .unwrap_or("sag");
            let mut model = json!({
                "id": route.model_name,
                "object": "model",
                "created": 0,
                "owned_by": owned_by,
            });
            // Lane metadata (plan §7.5): both keys are always emitted —
            // the explicit-only gate went away with `upstream_protocol`.
            // `ingress_protocols` is the union of the `apis` protocol keys
            // of every target's provider, in canonical protocol order so
            // the payload is stable regardless of target ordering.
            //
            // An **explicit** `pipeline = "systemone"` route is the one
            // exception: it serves `/v1/systemone` only, so it advertises
            // `systemone` (and nothing else) when an eligible target's
            // provider declares `apis.systemone`. Chat-family protocols are
            // never advertised for it — a chat request would resolve to
            // "no such model". Legacy routes keep the union aggregation
            // unchanged, including its quirks.
            let mut ingress_protocols: Vec<&'static str> = Vec::new();
            if route.pipeline == sag_config::Pipeline::SystemOne {
                let serves_systemone = route.targets.iter().any(|t| {
                    target_is_eligible(&config.providers, route, t)
                        && config
                            .providers
                            .get(&t.provider)
                            .is_some_and(|pc| pc.apis.contains_key(&sag_core::ApiKind::SystemOne))
                });
                if serves_systemone {
                    ingress_protocols.push(sag_core::ApiKind::SystemOne.as_str());
                }
            } else {
                for kind in sag_core::ApiKind::protocols() {
                    let declared = route.targets.iter().any(|t| {
                        config
                            .providers
                            .get(&t.provider)
                            .is_some_and(|pc| pc.apis.contains_key(&kind))
                    });
                    if declared {
                        ingress_protocols.push(kind.as_str());
                    }
                }
            }
            model["ingress_protocols"] = json!(ingress_protocols);
            model["lanes"] = json!(["native", "translated"]);
            model
        })
        .collect();

    Json(json!({
        "object": "list",
        "data": data,
    }))
}

/// GET `/v1/analytics/stats?provider=X&model=Y&real_model=Z`
///
/// Returns aggregated call statistics grouped by provider, model, and real_model.
pub async fn analytics_stats(
    State(state): State<AppState>,
    Query(params): Query<StatsQuery>,
) -> Response<Body> {
    let filter = StatsFilter {
        provider: params.provider,
        model: params.model,
        real_model: params.real_model,
        ..Default::default()
    };
    match state.collector.store().query_stats(filter).await {
        Ok(rows) => (StatusCode::OK, Json(json!({ "data": rows }))).into_response(),
        Err(e) => {
            error!(?e, "analytics query failed");
            AppError::Internal(e.to_string()).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sag_config::{ConfigHandle, GatewayConfig, RouteConfig, RouteTarget, Strategy};
    use sag_dispatch::testing::provider_config;
    use sag_state::testing::build_provider_registry;
    use std::collections::HashMap;

    #[tokio::test]
    async fn list_models_excludes_routes_with_only_disabled_providers() {
        let mut providers = HashMap::new();
        providers.insert("openai-enabled".into(), provider_config(true));
        providers.insert("openai-disabled".into(), provider_config(false));

        let routes = vec![
            RouteConfig {
                session_ttl_secs: None,
                session_max_entries: None,
                model_name: "gpt-4o".into(),
                api_endpoint: None,
                pipeline: sag_config::Pipeline::Chat,
                retries: None,
                timeout_secs: None,
                strategy: Strategy::FillFirst,
                targets: vec![RouteTarget {
                    provider: "openai-enabled".into(),
                    real_model: Some("gpt-4o".into()),
                    priority: 1,
                    retries: None,
                    timeout_secs: None,
                    extra: HashMap::new(),
                    capabilities: None,
                }],
            },
            RouteConfig {
                session_ttl_secs: None,
                session_max_entries: None,
                model_name: "claude-opus".into(),
                api_endpoint: None,
                pipeline: sag_config::Pipeline::Chat,
                retries: None,
                timeout_secs: None,
                strategy: Strategy::FillFirst,
                targets: vec![RouteTarget {
                    provider: "openai-disabled".into(),
                    real_model: Some("claude-opus".into()),
                    priority: 1,
                    retries: None,
                    timeout_secs: None,
                    extra: HashMap::new(),
                    capabilities: None,
                }],
            },
            RouteConfig {
                session_ttl_secs: None,
                session_max_entries: None,
                model_name: "gpt-4o-mini".into(),
                api_endpoint: None,
                pipeline: sag_config::Pipeline::Chat,
                retries: None,
                timeout_secs: None,
                strategy: Strategy::FillFirst,
                targets: vec![
                    RouteTarget {
                        provider: "openai-disabled".into(),
                        real_model: Some("gpt-4o-mini".into()),
                        priority: 2,
                        retries: None,
                        timeout_secs: None,
                        extra: HashMap::new(),
                        capabilities: None,
                    },
                    RouteTarget {
                        provider: "openai-enabled".into(),
                        real_model: Some("gpt-4o-mini".into()),
                        priority: 1,
                        retries: None,
                        timeout_secs: None,
                        extra: HashMap::new(),
                        capabilities: None,
                    },
                ],
            },
        ];

        let config = GatewayConfig {
            providers,
            routes,
            ..sag_config::gateway_config_base!()
        };
        let handle = ConfigHandle::from_config(config);
        let provider_registry = build_provider_registry(&handle, true).await;
        let state = sag_state::testing::test_app_state(handle, provider_registry).await;
        let Json(response) = list_models(State(state)).await;
        let data = response["data"].as_array().unwrap();
        let model_ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();

        assert!(
            model_ids.contains(&"gpt-4o"),
            "gpt-4o should be in models list"
        );
        assert!(
            model_ids.contains(&"gpt-4o-mini"),
            "gpt-4o-mini should be in models list (mixed targets)"
        );
        assert!(
            !model_ids.contains(&"claude-opus"),
            "claude-opus should NOT be in models list (all targets disabled)"
        );
    }

    #[tokio::test]
    async fn list_models_excludes_routes_with_models_not_in_enabled_models() {
        let mut providers = HashMap::new();
        let mut pc = provider_config(true);
        pc.enabled_models = vec!["gpt-4o".into()];
        providers.insert("openai".into(), pc);

        let routes = vec![
            RouteConfig {
                session_ttl_secs: None,
                session_max_entries: None,
                model_name: "gpt-4o".into(),
                api_endpoint: None,
                pipeline: sag_config::Pipeline::Chat,
                retries: None,
                timeout_secs: None,
                strategy: Strategy::FillFirst,
                targets: vec![RouteTarget {
                    provider: "openai".into(),
                    real_model: Some("gpt-4o".into()),
                    priority: 1,
                    retries: None,
                    timeout_secs: None,
                    extra: HashMap::new(),
                    capabilities: None,
                }],
            },
            RouteConfig {
                session_ttl_secs: None,
                session_max_entries: None,
                model_name: "deleted-model".into(),
                api_endpoint: None,
                pipeline: sag_config::Pipeline::Chat,
                retries: None,
                timeout_secs: None,
                strategy: Strategy::FillFirst,
                targets: vec![RouteTarget {
                    provider: "openai".into(),
                    real_model: Some("deleted-model".into()),
                    priority: 1,
                    retries: None,
                    timeout_secs: None,
                    extra: HashMap::new(),
                    capabilities: None,
                }],
            },
        ];

        let config = GatewayConfig {
            providers,
            routes,
            ..sag_config::gateway_config_base!()
        };
        let handle = ConfigHandle::from_config(config);
        let provider_registry = build_provider_registry(&handle, true).await;
        let state = sag_state::testing::test_app_state(handle, provider_registry).await;
        let Json(response) = list_models(State(state)).await;
        let data = response["data"].as_array().unwrap();
        let model_ids: Vec<&str> = data.iter().map(|m| m["id"].as_str().unwrap()).collect();

        assert!(model_ids.contains(&"gpt-4o"));
        assert!(!model_ids.contains(&"deleted-model"));
    }

    // ── `pipeline = "systemone"` advertisement ───────────

    fn route(
        model_name: &str,
        pipeline: sag_config::Pipeline,
        provider: &str,
        real_model: Option<&str>,
    ) -> RouteConfig {
        RouteConfig {
            session_ttl_secs: None,
            session_max_entries: None,
            model_name: model_name.into(),
            api_endpoint: None,
            pipeline,
            retries: None,
            timeout_secs: None,
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                provider: provider.into(),
                real_model: real_model.map(str::to_string),
                priority: 1,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
                capabilities: None,
            }],
        }
    }

    async fn models_for(config: GatewayConfig) -> serde_json::Value {
        let handle = ConfigHandle::from_config(config);
        let provider_registry = build_provider_registry(&handle, true).await;
        let state = sag_state::testing::test_app_state(handle, provider_registry).await;
        let Json(response) = list_models(State(state)).await;
        response
    }

    fn protocols_of<'a>(body: &'a serde_json::Value, model: &str) -> Vec<&'a str> {
        body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == model)
            .unwrap_or_else(|| panic!("{model} must be listed"))["ingress_protocols"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| p.as_str())
            .collect()
    }

    fn systemone_provider(enabled: bool) -> sag_config::ProviderConfig {
        let mut pc = provider_config(enabled);
        pc.apis.insert(
            sag_core::ApiKind::SystemOne,
            sag_config::ApiConfig {
                base_url: "https://api.systemone.example/v1".into(),
                ..Default::default()
            },
        );
        pc
    }

    #[tokio::test]
    async fn list_models_advertises_only_systemone_for_an_explicit_systemone_route() {
        // The provider declares BOTH chat and systemone; the legacy route must
        // keep the union (chat + systemone), while the explicit systemone
        // route must advertise `systemone` only.
        let mut providers = HashMap::new();
        providers.insert("dual".into(), systemone_provider(true));
        let config = GatewayConfig {
            providers,
            routes: vec![
                route(
                    "legacy-chat",
                    sag_config::Pipeline::Chat,
                    "dual",
                    Some("gpt-4o"),
                ),
                route(
                    "sys-one-model",
                    sag_config::Pipeline::SystemOne,
                    "dual",
                    Some("sys-one-v1"),
                ),
            ],
            ..sag_config::gateway_config_base!()
        };

        let body = models_for(config).await;
        assert_eq!(
            protocols_of(&body, "legacy-chat"),
            vec!["chat_completions", "systemone"],
            "legacy aggregation is unchanged (including its quirks)"
        );
        assert_eq!(
            protocols_of(&body, "sys-one-model"),
            vec!["systemone"],
            "an explicit systemone route must not advertise chat protocols"
        );
    }

    #[tokio::test]
    async fn list_models_hides_systemone_for_an_explicit_route_without_eligible_provider() {
        // Provider declares only `apis.chat_completions`: the systemone route
        // has no eligible target, so it advertises nothing (not even chat).
        let mut providers = HashMap::new();
        providers.insert("chat-only".into(), provider_config(true));
        let config = GatewayConfig {
            providers,
            routes: vec![route(
                "sys-one-model",
                sag_config::Pipeline::SystemOne,
                "chat-only",
                Some("sys-one-v1"),
            )],
            ..sag_config::gateway_config_base!()
        };

        let body = models_for(config).await;
        assert_eq!(protocols_of(&body, "sys-one-model"), Vec::<&str>::new());
    }

    #[tokio::test]
    async fn list_models_hides_systemone_for_a_disabled_provider() {
        let mut providers = HashMap::new();
        providers.insert("dual".into(), systemone_provider(false));
        let config = GatewayConfig {
            providers,
            routes: vec![route(
                "sys-one-model",
                sag_config::Pipeline::SystemOne,
                "dual",
                Some("sys-one-v1"),
            )],
            ..sag_config::gateway_config_base!()
        };

        let body = models_for(config).await;
        // Every target is ineligible → the route is not listed at all.
        let ids: Vec<&str> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert!(
            !ids.contains(&"sys-one-model"),
            "all-disabled routes stay excluded, got {ids:?}"
        );
    }
}
