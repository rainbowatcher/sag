//! Handlers for routes that are not part of the chat/image dispatch:
//! `/health`, `/v1/models`, `/v1/analytics/stats`.

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

use sag_analytics::{StatsFilter, TrendFilter};
use sag_dispatch::AppState;

#[derive(Deserialize)]
pub struct StatsQuery {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub real_model: Option<String>,
}

#[derive(Deserialize)]
pub struct TrendQuery {
    pub provider: Option<String>,
    /// Bucket width in seconds (e.g. 3600 for hourly). Defaults to 3600.
    #[serde(default = "default_bucket_seconds")]
    pub bucket_seconds: u64,
    /// Unix millisecond timestamp for the range start.
    pub from: i64,
    /// Unix millisecond timestamp for the range end (exclusive).
    pub to: Option<i64>,
}

const fn default_bucket_seconds() -> u64 {
    3600
}

/// GET `/health`
pub async fn show_health() -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
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
            route.targets.iter().any(|t| {
                config
                    .providers
                    .get(&t.provider)
                    .map(|pc| pc.enabled)
                    .unwrap_or(true)
            })
        })
        .map(|route| {
            let owned_by = route
                .targets
                .first()
                .map(|t| t.provider.as_str())
                .unwrap_or("sag");
            json!({
                "id": route.model_name,
                "object": "model",
                "created": 0,
                "owned_by": owned_by,
            })
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

/// GET `/v1/analytics/trend?bucket_seconds=3600&from=...&to=...&provider=...`
///
/// Returns time-bucketed call counts and token usage.
pub async fn analytics_trend(
    State(state): State<AppState>,
    Query(params): Query<TrendQuery>,
) -> Response<Body> {
    let Some(from) = chrono::DateTime::from_timestamp_millis(params.from) else {
        return AppError::BadRequest("invalid 'from' timestamp".into()).into_response();
    };
    let to = params.to.and_then(chrono::DateTime::from_timestamp_millis);

    let filter = TrendFilter {
        provider: params.provider,
        bucket_seconds: params.bucket_seconds,
        from,
        to,
    };
    match state.collector.store().query_trend(filter).await {
        Ok(rows) => (StatusCode::OK, Json(json!({ "data": rows }))).into_response(),
        Err(e) => {
            error!(?e, "trend query failed");
            AppError::Internal(e.to_string()).into_response()
        }
    }
}

/// GET `/v1/analytics/distribution?provider=X&model=Y&real_model=Z`
///
/// Returns call distribution (proportions) grouped by provider, model, and real_model.
pub async fn analytics_distribution(
    State(state): State<AppState>,
    Query(params): Query<StatsQuery>,
) -> Response<Body> {
    let filter = StatsFilter {
        provider: params.provider,
        model: params.model,
        real_model: params.real_model,
        ..Default::default()
    };
    match state.collector.store().query_distribution(filter).await {
        Ok(rows) => (StatusCode::OK, Json(json!({ "data": rows }))).into_response(),
        Err(e) => {
            error!(?e, "distribution query failed");
            AppError::Internal(e.to_string()).into_response()
        }
    }
}

/// GET `/v1/analytics/cache?provider=X&model=Y&real_model=Z`
///
/// Returns cache hit-rate statistics grouped by provider, model, and real_model.
pub async fn analytics_cache_stats(
    State(state): State<AppState>,
    Query(params): Query<StatsQuery>,
) -> Response<Body> {
    let filter = StatsFilter {
        provider: params.provider,
        model: params.model,
        real_model: params.real_model,
        ..Default::default()
    };
    match state.collector.store().query_cache_stats(filter).await {
        Ok(rows) => (StatusCode::OK, Json(json!({ "data": rows }))).into_response(),
        Err(e) => {
            error!(?e, "cache stats query failed");
            AppError::Internal(e.to_string()).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sag_config::{ConfigHandle, GatewayConfig, RouteConfig, RouteTarget, Strategy};
    use sag_dispatch::testing::{build_provider_registry, provider_config};
    use std::collections::HashMap;

    #[tokio::test]
    async fn list_models_excludes_routes_with_only_disabled_providers() {
        let mut providers = HashMap::new();
        providers.insert("openai-enabled".into(), provider_config(true));
        providers.insert("openai-disabled".into(), provider_config(false));

        let routes = vec![
            RouteConfig {
                model_name: "gpt-4o".into(),
                api_endpoint: "/chat/completions".into(),
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
                }],
            },
            RouteConfig {
                model_name: "claude-opus".into(),
                api_endpoint: "/chat/completions".into(),
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
                }],
            },
            RouteConfig {
                model_name: "gpt-4o-mini".into(),
                api_endpoint: "/chat/completions".into(),
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
                    },
                    RouteTarget {
                        provider: "openai-enabled".into(),
                        real_model: Some("gpt-4o-mini".into()),
                        priority: 1,
                        retries: None,
                        timeout_secs: None,
                        extra: HashMap::new(),
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
        let state = sag_dispatch::testing::test_app_state(handle, provider_registry).await;
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
}
