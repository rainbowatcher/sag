//! Sticky-failover active-target admin endpoints.
//!
//! - `GET /api/sticky-targets` — read-only snapshot of every target in the
//!   current config routes, annotated with the live status of the
//!   `active_targets` memory shared with `DispatchRuntime` (`idle` /
//!   `pinned` / `expired`).
//! - `DELETE /api/sticky-targets` — clear the sticky active target for a
//!   single model (idempotent) and bump its generation, so the next request
//!   re-evaluates the route's priority-ordered target list instead of
//!   staying pinned to the failover target — and so in-flight requests that
//!   decided their route before the delete can never re-pin afterwards.
//!
//! Both are mounted under `/api` by [`crate::admin::build_admin_routes`],
//! so they are auth-protected by the `gateway_auth` middleware.

use axum::Json;
use axum::extract::State;
use sag_config::RouteTarget;
use sag_state::{ActiveTargetEntry, ActiveTargetsInner, AppState};
use serde::{Deserialize, Serialize};

/// One sticky-failover entry in the snapshot: a route target plus the live
/// status of the shared active-target memory.
#[derive(Debug, Serialize)]
pub struct StickyTargetEntry {
    /// Route `model_name` (may be `"*"` for a wildcard route).
    model: String,
    provider: String,
    /// Resolved real model: `target.real_model`, or the route model name
    /// when omitted (passthrough). Empty for wildcard passthrough targets
    /// that have never pinned a concrete model.
    real_model: String,
    /// Target priority within the route.
    priority: u32,
    /// `idle` (no active entry), `pinned` (active entry, not expired) or
    /// `expired` (active entry past `sticky_failover_reset_secs`).
    status: String,
    /// Seconds since the active entry was recorded. `0` for `idle` entries.
    age_secs: u64,
}

/// `GET /api/sticky-targets`
///
/// Returns `{ "targets": [StickyTargetEntry, ...] }` — one flat entry per
/// target of every configured route (including wildcard routes), regardless
/// of whether the `active_targets` memory holds an entry for it.
pub async fn list_sticky_targets(State(state): State<AppState>) -> Json<serde_json::Value> {
    // Single config read: clone the snapshot inputs and drop the guard, so
    // the snapshot (reset secs + routes) comes from one consistent config
    // and no config read guard spans the `active_targets` await below.
    let (reset_secs, routes) = {
        let cfg = state.config.read().await;
        (cfg.sticky_failover_reset_secs, cfg.routes.clone())
    };

    // Single active read: clone the whole memory (entries + generations)
    // and drop the guard before computing DTOs — the snapshot is consistent
    // and the active lock is never held across the DTO loop.
    let inner = state.active_targets.read().await.clone();

    let mut targets = Vec::new();
    for route in &routes {
        for target in &route.targets {
            let matched = match_active_target(&inner, &routes, &route.model_name, target);

            // Resolved real model: explicit override, else passthrough — for
            // an exact route the client sent `route.model_name`; for a
            // wildcard route the requesting model is unknown, so fall back
            // to the pinned entry's real model (the model a request actually
            // used) or the empty string when nothing is pinned.
            let real_model = match &target.real_model {
                Some(rm) => rm.clone(),
                None if route.model_name != "*" => route.model_name.clone(),
                None => matched
                    .map(|entry| entry.real_model.clone())
                    .unwrap_or_default(),
            };

            let (status, age_secs) = match matched {
                Some(entry) => {
                    let age = entry.recorded_at.elapsed().as_secs();
                    let expired = reset_secs > 0 && age >= reset_secs;
                    if expired {
                        ("expired", age)
                    } else {
                        ("pinned", age)
                    }
                }
                None => ("idle", 0),
            };

            targets.push(StickyTargetEntry {
                model: route.model_name.clone(),
                provider: target.provider.clone(),
                real_model,
                priority: target.priority,
                status: status.to_string(),
                age_secs,
            });
        }
    }
    Json(serde_json::json!({ "targets": targets }))
}

/// Find the active entry (if any) that corresponds to `target` on `route`.
///
/// Matching is route-aware and mirrors dispatch + prune semantics:
/// - only entries whose [`ActiveTargetEntry::route_model`] equals
///   `route.model_name` are considered — an entry pinned by a wildcard route
///   is never attributed to an exact route (or vice versa), and two exact
///   routes sharing a provider never cross-attribute;
/// - for a wildcard route the candidate entry's request-model key must still
///   resolve to the wildcard route under the current config (no exact route
///   shadows it) — the same check prune applies;
/// - the target identity is provider + `real_model` spec: an explicit
///   `Some(rm)` target matches only entries pinned through an explicit
///   target with the same `rm`, and a passthrough (`None`) target matches
///   only entries pinned through a passthrough target — a passthrough pin
///   is never misreported against an explicit target on the same provider,
///   and vice versa.
///
/// When several entries match (possible on wildcard routes pinning
/// different request models), the newest `recorded_at` wins.
fn match_active_target<'a>(
    guard: &'a ActiveTargetsInner,
    cfg_routes: &[sag_config::RouteConfig],
    route_model: &str,
    target: &RouteTarget,
) -> Option<&'a ActiveTargetEntry> {
    guard
        .iter()
        .filter(|(key, entry)| {
            // Route affiliation: the entry must have been pinned by this route.
            if entry.route_model != route_model {
                return false;
            }
            // Wildcard routes: the key must still belong to the wildcard
            // route. Prune keeps this invariant, but re-verify defensively
            // so a snapshot taken before the next prune never misattributes.
            if route_model == "*" {
                return sag_config::resolve_model_route(cfg_routes, key)
                    .map(|r| r.model_name.as_str())
                    == Some("*");
            }
            true
        })
        .filter(|(_, entry)| {
            entry.provider == target.provider && entry.target_real_model == target.real_model
        })
        .max_by_key(|(_, entry)| entry.recorded_at)
        .map(|(_, entry)| entry)
}

/// Request body for `DELETE /api/sticky-targets`.
#[derive(Debug, Deserialize)]
pub struct DeleteStickyTargetRequest {
    pub model: String,
}

/// `DELETE /api/sticky-targets`
///
/// Clear the sticky-failover active target for `req.model`. The next request
/// for that model falls back to the route's priority-ordered target list
/// instead of staying pinned to the failover target. This mirrors the
/// behaviour of `sticky_failover_reset_secs` expiring — a manual delete is
/// an explicit "routing can re-evaluate" signal.
///
/// **Idempotent**: deleting a model with no active target is a no-op and
/// still returns `200 OK`. A model that had per-model state (an entry or a
/// generation tombstone) gets its generation bumped; a model with none gets
/// the global epoch bumped instead — either way an in-flight request that
/// decided its route before the delete cannot re-pin afterwards, and
/// arbitrary deletes never grow the `generations` map.
pub async fn delete_sticky_target(
    State(state): State<AppState>,
    Json(req): Json<DeleteStickyTargetRequest>,
) -> Json<serde_json::Value> {
    state.active_targets.write().await.clear(&req.model);
    Json(serde_json::json!({ "ok": true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use sag_config::{RouteConfig, RouteTarget};
    use sag_state::testing::{build_provider_registry, test_app_state};
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    use tower::ServiceExt;

    /// Build a test app with admin routes mounted (no auth middleware).
    fn test_app(state: AppState) -> axum::Router {
        axum::Router::new()
            .nest("/api", crate::admin::build_admin_routes(state.clone()))
            .with_state(state)
    }

    /// Helper: send GET /api/sticky-targets and return (status, parsed JSON).
    async fn fetch_targets(app: axum::Router) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/sticky-targets")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        (status, json)
    }

    /// Helper: send DELETE /api/sticky-targets with a body and return
    /// (status, parsed JSON).
    async fn delete_target(
        app: axum::Router,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/sticky-targets")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        (status, json)
    }

    /// Build an AppState with the given routes (and optionally a
    /// `sticky_failover_reset_secs` override).
    async fn state_with_routes(routes: Vec<RouteConfig>) -> AppState {
        let config = sag_config::GatewayConfig {
            routes,
            ..sag_config::gateway_config_base!()
        };
        let handle = sag_config::ConfigHandle::from_config(config);
        let registry = build_provider_registry(&handle, true).await;
        test_app_state(handle, registry).await
    }

    fn target(provider: &str, real_model: Option<&str>, priority: u32) -> RouteTarget {
        RouteTarget {
            provider: provider.into(),
            real_model: real_model.map(|m| m.to_string()),
            priority,
            retries: None,
            timeout_secs: None,
            extra: HashMap::new(),
        }
    }

    fn route(model: &str, targets: Vec<RouteTarget>) -> RouteConfig {
        RouteConfig {
            model_name: model.into(),
            api_endpoint: "/chat/completions".into(),
            strategy: sag_config::Strategy::StickyFailover,
            retries: None,
            timeout_secs: None,
            targets,
        }
    }

    #[tokio::test]
    async fn no_routes_returns_empty_array() {
        let state = state_with_routes(vec![]).await;
        let app = test_app(state);
        let (status, json) = fetch_targets(app).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["targets"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn all_route_targets_are_idle_when_active_map_empty() {
        // Empty active map but two routes × two targets: every route target
        // must appear, all idle.
        let state = state_with_routes(vec![
            route(
                "gpt-4o",
                vec![
                    target("openai", Some("gpt-4o"), 10),
                    target("anthropic", Some("claude-3"), 5),
                ],
            ),
            route("dall-e", vec![target("openai", Some("dall-e-3"), 0)]),
        ])
        .await;
        let app = test_app(state);
        let (status, json) = fetch_targets(app).await;
        assert_eq!(status, StatusCode::OK);
        let targets = json["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 3);
        for entry in targets {
            assert_eq!(entry["status"], "idle");
            assert_eq!(entry["age_secs"], 0);
        }
        assert_eq!(targets[0]["model"], "gpt-4o");
        assert_eq!(targets[0]["provider"], "openai");
        assert_eq!(targets[0]["real_model"], "gpt-4o");
        assert_eq!(targets[0]["priority"], 10);
        assert_eq!(targets[1]["model"], "gpt-4o");
        assert_eq!(targets[1]["provider"], "anthropic");
        assert_eq!(targets[2]["model"], "dall-e");
        assert_eq!(targets[2]["priority"], 0);
    }

    #[tokio::test]
    async fn pinned_and_expired_statuses() {
        let state = state_with_routes(vec![
            route("gpt-4o", vec![target("openai", Some("gpt-4o"), 10)]),
            route("stale", vec![target("openai", Some("stale"), 10)]),
        ])
        .await;
        {
            let mut guard = state.active_targets.write().await;
            // Pinned: fresh entry.
            guard.insert(
                "gpt-4o".into(),
                ActiveTargetEntry::new(
                    "openai",
                    "gpt-4o",
                    "gpt-4o",
                    Some("gpt-4o".into()),
                    Instant::now(),
                ),
            );
            // Expired: recorded 8000s ago, exceeds default reset (7200s).
            guard.insert(
                "stale".into(),
                ActiveTargetEntry::new(
                    "openai",
                    "stale",
                    "stale",
                    Some("stale".into()),
                    Instant::now() - Duration::from_secs(8000),
                ),
            );
        }
        let app = test_app(state);
        let (_, json) = fetch_targets(app).await;
        let targets = json["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 2);

        let pinned = targets.iter().find(|t| t["model"] == "gpt-4o").unwrap();
        assert_eq!(pinned["status"], "pinned");
        assert_eq!(pinned["age_secs"], 0);
        assert_eq!(pinned["real_model"], "gpt-4o");

        let expired = targets.iter().find(|t| t["model"] == "stale").unwrap();
        assert_eq!(expired["status"], "expired");
        // `elapsed()` may straddle a second boundary, so allow 8000 or 8001.
        let age = expired["age_secs"].as_u64().unwrap();
        assert!((8000..=8001).contains(&age));
    }

    #[tokio::test]
    async fn orphan_active_entries_without_route_target_are_not_listed() {
        // An active entry whose model has no matching route target is not a
        // config route target — the response is keyed to the config routes,
        // so the orphan must be dropped.
        let state = state_with_routes(vec![route(
            "gpt-4o",
            vec![target("openai", Some("gpt-4o"), 10)],
        )])
        .await;
        state.active_targets.write().await.insert(
            "orphan".into(),
            ActiveTargetEntry::new(
                "openai",
                "orphan",
                "orphan",
                Some("orphan".into()),
                Instant::now(),
            ),
        );
        let app = test_app(state);
        let (_, json) = fetch_targets(app).await;
        let targets = json["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0]["model"], "gpt-4o");
        assert_eq!(targets[0]["status"], "idle");
    }

    #[tokio::test]
    async fn real_model_none_passthrough_resolves_to_route_model() {
        let state =
            state_with_routes(vec![route("gpt-4o", vec![target("openai", None, 10)])]).await;
        let app = test_app(state.clone());
        let (_, json) = fetch_targets(app).await;
        let targets = json["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0]["status"], "idle");
        assert_eq!(
            targets[0]["real_model"], "gpt-4o",
            "passthrough on an exact route resolves to the route model name"
        );

        // Once pinned, the entry keyed by the route model matches the target
        // — the pin was recorded through the passthrough target, so the
        // entry carries `target_real_model: None`.
        state.active_targets.write().await.insert(
            "gpt-4o".into(),
            ActiveTargetEntry::new("openai", "gpt-4o", "gpt-4o", None, Instant::now()),
        );
        let app2 = test_app(state);
        let (_, json2) = fetch_targets(app2).await;
        let targets2 = json2["targets"].as_array().unwrap();
        assert_eq!(targets2[0]["status"], "pinned");
    }

    #[tokio::test]
    async fn wildcard_route_matches_by_provider_and_takes_newest() {
        let state = state_with_routes(vec![route(
            "*",
            vec![
                target("openai", None, 10),
                target("anthropic", Some("claude-3-5-sonnet"), 5),
            ],
        )])
        .await;
        {
            let mut guard = state.active_targets.write().await;
            // Two request models pinned the same openai passthrough target;
            // the newest recorded_at must win.
            guard.insert(
                "claude-3-opus".into(),
                ActiveTargetEntry::new(
                    "openai",
                    "claude-3-opus",
                    "*",
                    None,
                    Instant::now() - Duration::from_secs(100),
                ),
            );
            guard.insert(
                "gpt-4.1".into(),
                ActiveTargetEntry::new("openai", "gpt-4.1", "*", None, Instant::now()),
            );
            // A concrete real_model target: matches by provider + explicit spec.
            guard.insert(
                "anything".into(),
                ActiveTargetEntry::new(
                    "anthropic",
                    "claude-3-5-sonnet",
                    "*",
                    Some("claude-3-5-sonnet".into()),
                    Instant::now(),
                ),
            );
        }
        let app = test_app(state);
        let (_, json) = fetch_targets(app).await;
        let targets = json["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 2);

        let openai = targets.iter().find(|t| t["provider"] == "openai").unwrap();
        assert_eq!(openai["status"], "pinned");
        assert_eq!(openai["real_model"], "gpt-4.1", "newest entry wins");
        assert_eq!(openai["model"], "*");

        let anthropic = targets
            .iter()
            .find(|t| t["provider"] == "anthropic")
            .unwrap();
        assert_eq!(anthropic["status"], "pinned");
        assert_eq!(anthropic["real_model"], "claude-3-5-sonnet");
    }

    #[tokio::test]
    async fn wildcard_passthrough_idle_reports_empty_real_model() {
        let state = state_with_routes(vec![route("*", vec![target("openai", None, 10)])]).await;
        let app = test_app(state);
        let (_, json) = fetch_targets(app).await;
        let targets = json["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0]["status"], "idle");
        assert_eq!(
            targets[0]["real_model"], "",
            "wildcard passthrough with no pin has no concrete model to report"
        );
    }

    #[tokio::test]
    async fn zero_reset_secs_never_flags_expired() {
        // sticky_failover_reset_secs = 0 disables expiry entirely: even a
        // very old active target must stay `pinned`.
        let config = sag_config::GatewayConfig {
            sticky_failover_reset_secs: 0,
            routes: vec![route("gpt-4o", vec![target("openai", Some("gpt-4o"), 10)])],
            ..sag_config::gateway_config_base!()
        };
        let handle = sag_config::ConfigHandle::from_config(config);
        let registry = build_provider_registry(&handle, true).await;
        let state = test_app_state(handle, registry).await;
        state.active_targets.write().await.insert(
            "gpt-4o".into(),
            ActiveTargetEntry::new(
                "openai",
                "gpt-4o",
                "gpt-4o",
                Some("gpt-4o".into()),
                Instant::now() - Duration::from_secs(99999),
            ),
        );
        let app = test_app(state);
        let (_, json) = fetch_targets(app).await;
        let targets = json["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 1);
        // `elapsed()` may straddle a second boundary, so allow 99999 or 100000.
        let age = targets[0]["age_secs"].as_u64().unwrap();
        assert!((99999..=100000).contains(&age));
        assert_eq!(targets[0]["status"], "pinned");
    }

    #[tokio::test]
    async fn delete_removes_target_and_bumps_generation_idempotently() {
        let state = state_with_routes(vec![route(
            "gpt-4o",
            vec![target("openai", Some("gpt-4o"), 10)],
        )])
        .await;
        state.active_targets.write().await.insert(
            "gpt-4o".into(),
            ActiveTargetEntry::new(
                "openai",
                "gpt-4o",
                "gpt-4o",
                Some("gpt-4o".into()),
                Instant::now(),
            ),
        );

        // First delete removes the target and bumps the generation.
        let app = test_app(state.clone());
        let (status, json) = delete_target(app, serde_json::json!({ "model": "gpt-4o" })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["ok"], true);
        assert!(
            state.active_targets.read().await.get("gpt-4o").is_none(),
            "DELETE must clear the sticky active target for the model"
        );
        assert_eq!(
            state.active_targets.read().await.generation("gpt-4o"),
            1,
            "DELETE must bump the generation"
        );

        // Second delete (target already gone) is a 200 no-op — idempotent —
        // and still bumps.
        let app2 = test_app(state.clone());
        let (status2, json2) = delete_target(app2, serde_json::json!({ "model": "gpt-4o" })).await;
        assert_eq!(status2, StatusCode::OK);
        assert_eq!(json2["ok"], true);
        assert_eq!(state.active_targets.read().await.generation("gpt-4o"), 2);

        // DELETE without any prior entry: bumps the global epoch instead of
        // creating a per-model tombstone (bounded memory), while still
        // blocking stale writebacks via the epoch check.
        let app3 = test_app(state.clone());
        let (_, _) = delete_target(app3, serde_json::json!({ "model": "never-pinned" })).await;
        assert_eq!(
            state.active_targets.read().await.epoch(),
            1,
            "DELETE with no entry bumps the global epoch"
        );
        assert_eq!(
            state.active_targets.read().await.generation("never-pinned"),
            0,
            "no per-model tombstone is created for arbitrary models"
        );

        // A stale writeback with the pre-delete epoch must not repin — the
        // "never-pinned" delete moved the epoch, so even a gpt-4o decision
        // captured before it (with the then-current generation 2) fails.
        let version = state.config.routes_version();
        let mut guard = state.active_targets.write().await;
        assert!(
            !guard.insert_if_generation(
                "gpt-4o".into(),
                ActiveTargetEntry::new(
                    "openai",
                    "gpt-4o",
                    "gpt-4o",
                    Some("gpt-4o".into()),
                    Instant::now(),
                ),
                &sag_state::StickyToken::new(0, 2, version),
                version,
            ),
            "pre-delete epoch must not repin after the delete"
        );
        assert!(
            guard.insert_if_generation(
                "gpt-4o".into(),
                ActiveTargetEntry::new(
                    "openai",
                    "gpt-4o",
                    "gpt-4o",
                    Some("gpt-4o".into()),
                    Instant::now(),
                ),
                &sag_state::StickyToken::new(1, 2, version),
                version,
            ),
            "post-delete epoch still records"
        );

        // GET reflects the route target as pinned again.
        drop(guard);
        let app4 = test_app(state);
        let (_, json4) = fetch_targets(app4).await;
        let targets4 = json4["targets"].as_array().unwrap();
        assert_eq!(targets4.len(), 1);
        assert_eq!(targets4[0]["status"], "pinned");
    }

    #[tokio::test]
    async fn exact_and_wildcard_same_provider_real_model_do_not_cross_attribute() {
        // The same provider + explicit real_model appears on an exact route
        // AND on a wildcard route. An entry pinned by the exact route (key
        // "gpt-4o" → route_model "gpt-4o") must be attributed to the exact
        // route only — the wildcard target must stay idle, not "pinned" via
        // a provider+real_model scan.
        let state = state_with_routes(vec![
            route("gpt-4o", vec![target("openai", Some("gpt-4o"), 10)]),
            route("*", vec![target("openai", Some("gpt-4o"), 5)]),
        ])
        .await;
        state.active_targets.write().await.insert(
            "gpt-4o".into(),
            ActiveTargetEntry::new(
                "openai",
                "gpt-4o",
                "gpt-4o",
                Some("gpt-4o".into()),
                Instant::now(),
            ),
        );

        let app = test_app(state);
        let (_, json) = fetch_targets(app).await;
        let targets = json["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 2);

        let exact = targets.iter().find(|t| t["model"] == "gpt-4o").unwrap();
        assert_eq!(exact["status"], "pinned", "exact route claims its own pin");

        let wildcard = targets.iter().find(|t| t["model"] == "*").unwrap();
        assert_eq!(
            wildcard["status"], "idle",
            "the exact-route pin must not be attributed to the wildcard route"
        );
    }

    #[tokio::test]
    async fn wildcard_none_vs_explicit_real_model_do_not_cross_attribute() {
        // A wildcard route with a passthrough target and an explicit target
        // on the SAME provider: a passthrough pin must be reported against
        // the passthrough target only, and an explicit pin against the
        // explicit target only — no provider-only fallback.
        let state = state_with_routes(vec![route(
            "*",
            vec![
                target("openai", None, 10),
                target("openai", Some("gpt-4o"), 5),
            ],
        )])
        .await;
        {
            let mut guard = state.active_targets.write().await;
            // Passthrough pin: resolved model == request model.
            guard.insert(
                "claude-3-opus".into(),
                ActiveTargetEntry::new("openai", "claude-3-opus", "*", None, Instant::now()),
            );
            // Explicit pin: request model "anything" pinned the explicit
            // "gpt-4o" target.
            guard.insert(
                "anything".into(),
                ActiveTargetEntry::new(
                    "openai",
                    "gpt-4o",
                    "*",
                    Some("gpt-4o".into()),
                    Instant::now(),
                ),
            );
        }

        let app = test_app(state);
        let (_, json) = fetch_targets(app).await;
        let targets = json["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 2);

        // The passthrough target reports the pinned request model; the
        // explicit target reports its configured real_model. Each pin must
        // land on exactly its own target.
        let passthrough = targets
            .iter()
            .find(|t| t["real_model"] == "claude-3-opus")
            .expect("passthrough target present with its pinned model")
            .clone();
        let explicit = targets
            .iter()
            .find(|t| t["real_model"] == "gpt-4o")
            .expect("explicit target present")
            .clone();

        assert_eq!(
            passthrough["status"], "pinned",
            "the passthrough pin must be reported against the passthrough target"
        );
        assert_eq!(
            explicit["status"], "pinned",
            "the explicit pin must be reported against the explicit target"
        );
    }

    #[tokio::test]
    async fn wildcard_entry_shadowed_by_exact_route_is_not_attributed() {
        // A wildcard pin whose request key is now shadowed by an exact route
        // no longer belongs to the wildcard route: the wildcard target must
        // report idle (the key resolves to the exact route, so it cannot be
        // pinned through the wildcard anymore).
        let state = state_with_routes(vec![
            route("gpt-4o", vec![target("openai", None, 10)]),
            route("*", vec![target("openai", None, 5)]),
        ])
        .await;
        state.active_targets.write().await.insert(
            "gpt-4o".into(),
            ActiveTargetEntry::new("openai", "gpt-4o", "*", None, Instant::now()),
        );

        let app = test_app(state);
        let (_, json) = fetch_targets(app).await;
        let targets = json["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 2);

        let wildcard = targets.iter().find(|t| t["model"] == "*").unwrap();
        assert_eq!(
            wildcard["status"], "idle",
            "a wildcard pin shadowed by an exact route must not be attributed to the wildcard route"
        );
        let exact = targets.iter().find(|t| t["model"] == "gpt-4o").unwrap();
        assert_eq!(
            exact["status"], "idle",
            "the wildcard-route pin must not be attributed to the exact route either"
        );
    }
}
