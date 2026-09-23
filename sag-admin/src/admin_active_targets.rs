//! Active-target admin endpoints: the currently-effective routing state for
//! every configured route target.
//!
//! - `GET /api/active-targets` — read-only snapshot of every target in the
//!   current config routes, annotated with the live routing state:
//!   sticky-failover pins (`idle` / `pinned` / `expired` from the
//!   `active_targets` memory shared with `DispatchRuntime`) and fill-first
//!   cached-order positions (index 0 = the target absorbing all traffic).
//! - `DELETE /api/active-targets` — reset routing for a model: for every
//!   strategy, clear the sticky active target (idempotent) and bump its
//!   generation so the next request re-evaluates the priority-ordered list
//!   and in-flight requests can never re-pin; for a fill-first route
//!   additionally drop the cached shuffled order so the next request
//!   re-shuffles. Clearing the sticky pin for *every* strategy is what makes
//!   DELETE able to clear a stale pin left behind by a route that switched
//!   from StickyFailover to FillFirst. When the request names the wildcard
//!   route itself (`model = "*"`), every concrete pin belonging to that
//!   wildcard route is cleared (its `active_targets` keys are the concrete
//!   request models), never other routes' pins.
//!
//! Both are mounted under `/api` by [`crate::admin::build_admin_routes`], so
//! they are auth-protected by the `gateway_auth` middleware.

use axum::Json;
use axum::extract::State;
use std::time::Instant;

use sag_config::{RouteTarget, Strategy};
use sag_state::{ActiveTargetEntry, ActiveTargetsInner, AppState};
use serde::{Deserialize, Serialize};

/// One sticky-failover entry in the snapshot: a route target plus the live
/// status of the shared active-target memory.
#[derive(Debug, Serialize)]
pub struct ActiveTargetDto {
    /// Route `model_name` (may be `"*"` for a wildcard route).
    model: String,
    /// Actual `active_targets` map key — the **request model** the pin was
    /// recorded under. For an exact route this equals `model`; for a
    /// wildcard route it is the concrete request model (e.g. `"gpt-4o"`),
    /// which is what `DELETE /api/active-targets` must receive to clear the
    /// pin. Empty string when the entry is `idle` (nothing to clear).
    key: String,
    provider: String,
    /// Resolved real model: `target.real_model`, or the route model name
    /// when omitted (passthrough). Empty for wildcard passthrough targets
    /// that have never pinned a concrete model.
    real_model: String,
    /// Target priority within the route.
    priority: u32,
    /// 0-based position of this target in the route's priority-sorted target
    /// order (priority descending, ties keep original array order). The
    /// frontend uses it (instead of object identity) to confirm which config
    /// target a pin landed on: `target_index == 0` means the route's
    /// highest-priority (primary) target.
    target_index: u32,
    /// `idle` (no active entry), `pinned` (active entry, not expired) or
    /// `expired` (active entry past `sticky_failover_reset_secs`).
    status: String,
    /// Seconds since the active entry was recorded. `0` for `idle` entries.
    age_secs: u64,
    /// Route target-selection strategy: `"sticky_failover"`, `"fill_first"`
    /// or `"round_robin"`. The fill-first fields below are only populated
    /// for `"fill_first"` routes.
    strategy: String,
    /// Position of this target in the cached fill-first shuffled order — `0`
    /// means this target currently absorbs all fill-first traffic. `None`
    /// when the route is not fill-first or no order is cached yet (no
    /// request has ever exercised the route).
    fill_first_position: Option<u32>,
    /// Seconds since the fill-first order was last shuffled (cache age).
    /// `None` when no order is cached.
    fill_first_cache_age_secs: Option<u64>,
    /// `shuffle_cache_ttl_secs` config — after this many seconds the order
    /// is re-shuffled. `0` means never expire. `None` when no order is
    /// cached (or the route is not fill-first).
    fill_first_cache_ttl_secs: Option<u64>,
}

/// `GET /api/active-targets`
///
/// Returns `{ "targets": [ActiveTargetDto, ...] }` — one flat entry per
/// target of every configured route (including wildcard routes), regardless
/// of whether the `active_targets` memory holds an entry for it. For
/// fill-first routes the entries additionally carry the cached shuffled
/// order position (index 0 = the target currently absorbing all traffic)
/// and the cache's age / TTL.
pub async fn list_active_targets(State(state): State<AppState>) -> Json<serde_json::Value> {
    // Single config read: clone the snapshot inputs and drop the guard, so
    // the snapshot (reset secs + routes) comes from one consistent config
    // and no config read guard spans the `active_targets` await below.
    let (reset_secs, shuffle_cache_ttl_secs, routes) = {
        let cfg = state.config.read().await;
        (
            cfg.sticky_failover_reset_secs,
            cfg.shuffle_cache_ttl_secs,
            cfg.routes.clone(),
        )
    };

    // Single active read: clone the whole memory (entries + generations)
    // and drop the guard before computing DTOs — the snapshot is consistent
    // and the active lock is never held across the DTO loop.
    let inner = state.active_targets.read().await.clone();
    // Single fill-first read: clone the cached orders (the `Arc` inside
    // `CachedOrder` makes this cheap) so the DTO loop never holds the lock.
    let fill_first_orders = state.fill_first_orders.read().await.clone();
    let now = Instant::now();

    let mut targets = Vec::new();
    for route in &routes {
        // `target_index` is the target's rank in the route's priority-sorted
        // order (stable, priority descending, ties keep original array
        // order) — index 0 = highest-priority target. The response `targets`
        // array itself stays in original config order.
        let ranks = priority_ranks(&route.targets);
        for (config_index, target) in route.targets.iter().enumerate() {
            // Sticky pins are only meaningful for StickyFailover routes. A
            // route that switched to FillFirst / RoundRobin writes no pin
            // anymore, but a stale `active_targets` entry from its previous
            // StickyFailover config may survive in memory until the next
            // prune — it must not be reported as `pinned`/`expired` here.
            let matched = if route.strategy == Strategy::StickyFailover {
                match_active_target(&inner, &routes, &route.model_name, target)
            } else {
                None
            };

            // Resolved real model: explicit override, else passthrough — for
            // an exact route the client sent `route.model_name`; for a
            // wildcard route the requesting model is unknown, so fall back
            // to the pinned entry's real model (the model a request actually
            // used) or the empty string when nothing is pinned.
            let real_model = match &target.real_model {
                Some(rm) => rm.clone(),
                None if route.model_name != "*" => route.model_name.clone(),
                None => matched
                    .map(|(_, entry)| entry.real_model.clone())
                    .unwrap_or_default(),
            };

            let (status, age_secs) = match matched {
                Some((_, entry)) => {
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

            // Fill-first observability: report the cached shuffled order for
            // fill-first routes only. The sticky-failover strategy also uses
            // `order_fill_first` internally, but its observable state is the
            // active pin above; exposing the base order there would muddy
            // the two concepts.
            let (fill_first_position, fill_first_cache_age_secs, fill_first_cache_ttl_secs) =
                if route.strategy == Strategy::FillFirst {
                    match fill_first_orders.get(&route.model_name) {
                        Some(cached) => (
                            cached
                                .targets
                                .iter()
                                .position(|t| {
                                    t.provider == target.provider
                                        && t.real_model == target.real_model
                                })
                                .map(|i| i as u32),
                            Some(now.duration_since(cached.created_at).as_secs()),
                            Some(shuffle_cache_ttl_secs),
                        ),
                        None => (None, None, None),
                    }
                } else {
                    (None, None, None)
                };

            targets.push(ActiveTargetDto {
                model: route.model_name.clone(),
                key: matched.map(|(k, _)| k.to_string()).unwrap_or_default(),
                provider: target.provider.clone(),
                real_model,
                priority: target.priority,
                target_index: ranks[config_index],
                status: status.to_string(),
                age_secs,
                strategy: route.strategy.as_str().to_string(),
                fill_first_position,
                fill_first_cache_age_secs,
                fill_first_cache_ttl_secs,
            });
        }
    }
    Json(serde_json::json!({ "targets": targets }))
}

/// Rank of each target in a route's priority-sorted order: a stable sort by
/// `priority` descending (ties keep original array order), so rank 0 is the
/// route's highest-priority (primary) target. Returns a `Vec` indexed by the
/// target's position in `route.targets` — `result[0]` is the rank of the
/// first configured target, `result[1]` the second, etc. Mirrors the
/// dispatch layer's priority ordering (`sag-dispatch` sorts targets by
/// `Reverse(priority)`).
fn priority_ranks(targets: &[RouteTarget]) -> Vec<u32> {
    let mut ranked: Vec<(u32, usize)> = targets
        .iter()
        .enumerate()
        .map(|(i, t)| (t.priority, i))
        .collect();
    // `sort_by_key` is a stable sort: equal priorities keep array order.
    ranked.sort_by_key(|&(priority, _)| std::cmp::Reverse(priority));
    let mut ranks = vec![0u32; targets.len()];
    for (rank, &(_, original_index)) in ranked.iter().enumerate() {
        ranks[original_index] = rank as u32;
    }
    ranks
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
) -> Option<(&'a str, &'a ActiveTargetEntry)> {
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
}

/// Request body for `DELETE /api/active-targets`.
#[derive(Debug, Deserialize)]
pub struct DeleteActiveTargetRequest {
    pub model: String,
}

/// `DELETE /api/active-targets`
///
/// Reset routing for `req.model` so the next request re-evaluates instead of
/// staying pinned to a previously decided state:
///
/// - **Every strategy**: clear the sticky active target for the model and
///   bump its generation / epoch, so the next request re-evaluates the
///   route's priority-ordered target list instead of staying pinned to the
///   failover target — and so in-flight requests that decided their route
///   before the delete can never re-pin afterwards. This also clears a
///   *stale* pin left behind by a route that switched from StickyFailover
///   to FillFirst: the pin is no longer written by the current strategy but
///   survives in memory until pruned, and DELETE must be able to remove it.
/// - **FillFirst**: additionally drop the cached shuffled order for the
///   route, so the next request re-shuffles the same-priority targets — a
///   manual TTL expiry, the fill-first analogue of un-pinning. Note this
///   does **not** guarantee a different target ends up first (with two
///   same-priority targets it is a 50/50 re-draw); the fill-first order
///   cache is never consulted by the sticky-failover writeback.
/// - **Wildcard route** (`req.model == "*"` with a `"*"` route present):
///   clear every concrete pin belonging to that wildcard route — its
///   `active_targets` keys are the concrete request models (e.g. `"gpt-4o"`)
///   the frontend cannot name individually — while leaving other routes'
///   pins (and wildcard pins now shadowed by an exact route) untouched. For
///   a fill-first wildcard route the same precise rule drops the cached
///   orders of exactly the keys that still belong to it.
///
/// **Idempotent**: deleting a model with no active target / no cached order
/// is a no-op and still returns `200 OK`.
pub async fn delete_active_target(
    State(state): State<AppState>,
    Json(req): Json<DeleteActiveTargetRequest>,
) -> Json<serde_json::Value> {
    // Snapshot the config once: resolve the request model to decide the
    // strategy (whether the fill-first order cache must also be dropped) and
    // detect the wildcard-route case. The model is the route's `model_name`
    // ("*" for a wildcard route) — the same key the fill-first order cache is
    // stored under. Unmatched models keep the legacy sticky-failover clear.
    //
    // The sticky `active_targets` clear always runs, for every strategy: it
    // is what removes a stale pin after a StickyFailover → FillFirst switch
    // (and, for currently-sticky routes, the live pin).
    //
    // Note: the strategy decision is based on the config snapshot taken here.
    // After releasing the config read lock, a config hot-reload could flip
    // the route's strategy before we take the write lock on the corresponding
    // memory (fill_first_orders vs active_targets). If that happens the
    // DELETE applies to the old strategy's memory — the other memory is left
    // untouched. This is best-effort consistency: the "wrongly" cleared entry
    // is naturally re-created by the next request / TTL / generation
    // mechanism, and there is no data corruption risk.
    let (resolved, routes) = {
        let cfg = state.config.read().await;
        (
            sag_config::resolve_model_route(&cfg.routes, &req.model).cloned(),
            cfg.routes.clone(),
        )
    };
    let is_fill_first = resolved
        .as_ref()
        .is_some_and(|r| r.strategy == Strategy::FillFirst);
    // The wildcard case: the request names the wildcard route itself
    // (`model == "*"`) and that route exists. The frontend cannot know every
    // concrete request model the wildcard route pinned (its `active_targets`
    // keys are the concrete request models, e.g. "gpt-4o"), so "clear this
    // route" clears every concrete pin belonging to it — precisely, never
    // other routes' pins — instead of the literal "*" key (which is never a
    // real pin).
    let is_wildcard_clear =
        req.model == "*" && resolved.as_ref().is_some_and(|r| r.model_name == "*");

    if is_wildcard_clear {
        state
            .active_targets
            .write()
            .await
            .clear_wildcard_route(&routes);
        if is_fill_first {
            // The wildcard fill-first order cache lives under concrete request
            // models too; drop exactly the keys that still belong to the
            // wildcard route (an exact route sharing a key keeps its cache).
            let mut orders = state.fill_first_orders.write().await;
            orders.retain(|k, _| {
                !matches!(
                    sag_config::resolve_model_route(&routes, k),
                    Some(r) if r.model_name == "*"
                )
            });
        }
    } else {
        state.active_targets.write().await.clear(&req.model);
        if is_fill_first {
            state.fill_first_orders.write().await.remove(&req.model);
        }
    }
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

    /// Helper: send GET /api/active-targets and return (status, parsed JSON).
    async fn fetch_targets(app: axum::Router) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/active-targets")
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

    /// Helper: send DELETE /api/active-targets with a body and return
    /// (status, parsed JSON).
    async fn delete_target(
        app: axum::Router,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/active-targets")
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
            upstream_protocol: None,
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

    fn route_with_strategy(
        model: &str,
        targets: Vec<RouteTarget>,
        strategy: sag_config::Strategy,
    ) -> RouteConfig {
        RouteConfig {
            model_name: model.into(),
            api_endpoint: "/chat/completions".into(),
            strategy,
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
            assert_eq!(entry["key"], "", "idle entries have no pin key");
        }
        assert_eq!(targets[0]["model"], "gpt-4o");
        assert_eq!(targets[0]["provider"], "openai");
        assert_eq!(targets[0]["real_model"], "gpt-4o");
        assert_eq!(targets[0]["priority"], 10);
        assert_eq!(targets[1]["model"], "gpt-4o");
        assert_eq!(targets[1]["provider"], "anthropic");
        assert_eq!(targets[2]["model"], "dall-e");
        assert_eq!(targets[2]["priority"], 0);
        // These priorities are already descending ([10, 5] and [0]), so the
        // priority-sorted rank equals the array position.
        assert_eq!(
            targets[0]["target_index"], 0,
            "highest-priority target of the route"
        );
        assert_eq!(
            targets[1]["target_index"], 1,
            "second-highest-priority target of the route"
        );
        assert_eq!(
            targets[2]["target_index"], 0,
            "single-target route starts at 0"
        );
    }

    #[tokio::test]
    async fn target_index_follows_priority_order() {
        // `target_index` is the target's rank in the route's priority-sorted
        // order (priority descending, ties keep array order) — NOT its
        // position in the original `route.targets` array. Array order below
        // is [priority 10, priority 20, priority 0], so the priority
        // descending ranks are: array index 1 → 0 (highest priority), array
        // index 0 → 1, array index 2 → 2. The response `targets` array keeps
        // original config order but reports those ranks as the indexes.
        let state = state_with_routes(vec![route(
            "gpt-4o",
            vec![
                target("openai", Some("gpt-4o"), 10),
                target("anthropic", Some("claude-3"), 20),
                target("openai", Some("gpt-4o-mini"), 0),
            ],
        )])
        .await;
        let app = test_app(state.clone());
        let (_, json) = fetch_targets(app).await;
        let targets = json["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 3);
        // Response entries stay in original config array order...
        assert_eq!(targets[0]["provider"], "openai");
        assert_eq!(targets[1]["provider"], "anthropic");
        assert_eq!(targets[2]["provider"], "openai");
        // ...but `target_index` is the priority-descending rank.
        assert_eq!(targets[0]["target_index"], 1);
        assert_eq!(
            targets[1]["target_index"], 0,
            "the highest-priority target ranks 0"
        );
        assert_eq!(targets[2]["target_index"], 2);

        // The pinned target keeps its priority rank: pin the
        // highest-priority target (array index 1, priority 20) and the DTO
        // must report rank 0 on it.
        state.active_targets.write().await.insert(
            "gpt-4o".into(),
            ActiveTargetEntry::new(
                "anthropic",
                "claude-3",
                "gpt-4o",
                Some("claude-3".into()),
                Instant::now(),
            ),
        );
        let app2 = test_app(state);
        let (_, json2) = fetch_targets(app2).await;
        let targets2 = json2["targets"].as_array().unwrap();
        let pinned = targets2.iter().find(|t| t["status"] == "pinned").unwrap();
        assert_eq!(pinned["provider"], "anthropic");
        assert_eq!(
            pinned["target_index"], 0,
            "index is the priority-sorted position"
        );
    }

    #[tokio::test]
    async fn target_index_stable_for_equal_priorities() {
        // All targets share the default priority 0: the stable sort keeps
        // their original array order, so `target_index` equals the array
        // position (ties break in favor of the earlier-configured target).
        let state = state_with_routes(vec![route(
            "gpt-4o",
            vec![
                target("anthropic", Some("claude-3"), 0),
                target("openai", Some("gpt-4o"), 0),
                target("openai", Some("gpt-4o-mini"), 0),
            ],
        )])
        .await;
        let app = test_app(state);
        let (_, json) = fetch_targets(app).await;
        let targets = json["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0]["target_index"], 0);
        assert_eq!(targets[1]["target_index"], 1);
        assert_eq!(targets[2]["target_index"], 2);
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
        assert_eq!(
            pinned["key"], "gpt-4o",
            "exact-route pin key equals the route model name"
        );

        let expired = targets.iter().find(|t| t["model"] == "stale").unwrap();
        assert_eq!(expired["status"], "expired");
        assert_eq!(expired["key"], "stale");
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
        assert_eq!(
            openai["key"], "gpt-4.1",
            "wildcard pin key is the request model, not the '*' route name"
        );

        let anthropic = targets
            .iter()
            .find(|t| t["provider"] == "anthropic")
            .unwrap();
        assert_eq!(anthropic["status"], "pinned");
        assert_eq!(anthropic["real_model"], "claude-3-5-sonnet");
        assert_eq!(anthropic["key"], "anything");
    }

    #[tokio::test]
    async fn wildcard_passthrough_idle_reports_empty_real_model() {
        let state = state_with_routes(vec![route("*", vec![target("openai", None, 10)])]).await;
        let app = test_app(state);
        let (_, json) = fetch_targets(app).await;
        let targets = json["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0]["status"], "idle");
        assert_eq!(targets[0]["key"], "", "idle wildcard pin has no key");
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

    // ── fill-first observability + manual reset ────────────────────────

    #[tokio::test]
    async fn fill_first_route_ignores_stale_active_targets_entry() {
        // Regression for the sticky→fill_first switch: a route that used to
        // be StickyFailover may still carry a pin in `active_targets` until
        // the next prune. GET must NOT report it as `pinned`/`expired` for a
        // route whose current strategy is FillFirst — the entry reports
        // `idle` with no sticky key, and only the fill-first fields carry
        // state.
        let state = state_with_routes(vec![route_with_strategy(
            "gpt-4o",
            vec![target("openai", Some("gpt-4o"), 10)],
            sag_config::Strategy::FillFirst,
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
        // Same for a stale *expired* pin (recorded long ago).
        state.active_targets.write().await.insert(
            "stale".into(),
            ActiveTargetEntry::new(
                "openai",
                "stale",
                "gpt-4o",
                Some("stale".into()),
                Instant::now() - Duration::from_secs(8000),
            ),
        );

        let app = test_app(state);
        let (_, json) = fetch_targets(app).await;
        let targets = json["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0]["strategy"], "fill_first");
        assert_eq!(
            targets[0]["status"], "idle",
            "a stale sticky pin must not be reported on a fill-first route"
        );
        assert_eq!(
            targets[0]["key"], "",
            "no sticky key for a fill-first route"
        );
        assert_eq!(targets[0]["age_secs"], 0);
    }

    #[tokio::test]
    async fn fill_first_route_reports_strategy_and_null_cache_fields_when_absent() {
        // A fill-first route with no cached order yet: every target reports
        // `strategy: "fill_first"` but no position / cache metadata, and
        // `status` stays `idle` (fill-first never writes `active_targets`).
        let state = state_with_routes(vec![route_with_strategy(
            "gpt-4o",
            vec![
                target("openai", Some("gpt-4o"), 10),
                target("anthropic", Some("claude-3"), 5),
            ],
            sag_config::Strategy::FillFirst,
        )])
        .await;
        let app = test_app(state);
        let (_, json) = fetch_targets(app).await;
        let targets = json["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 2);
        for entry in targets {
            assert_eq!(entry["strategy"], "fill_first");
            assert_eq!(entry["status"], "idle");
            assert_eq!(entry["key"], "");
            assert_eq!(entry["fill_first_position"], serde_json::Value::Null);
            assert_eq!(entry["fill_first_cache_age_secs"], serde_json::Value::Null);
            assert_eq!(entry["fill_first_cache_ttl_secs"], serde_json::Value::Null);
        }
    }

    #[tokio::test]
    async fn fill_first_route_reports_cached_order_position_and_age() {
        // Inject a deterministic cached order (openai at index 0, anthropic
        // at index 1) — the snapshot must report the position of each target
        // in that order plus the cache age and the configured TTL.
        let state = state_with_routes(vec![route_with_strategy(
            "gpt-4o",
            vec![
                target("openai", Some("gpt-4o"), 10),
                target("anthropic", Some("claude-3"), 10),
            ],
            sag_config::Strategy::FillFirst,
        )])
        .await;
        {
            let mut guard = state.fill_first_orders.write().await;
            guard.insert(
                "gpt-4o".into(),
                sag_state::CachedOrder {
                    targets: std::sync::Arc::from(vec![
                        target("openai", Some("gpt-4o"), 10),
                        target("anthropic", Some("claude-3"), 10),
                    ]),
                    created_at: Instant::now() - Duration::from_secs(123),
                    routes_version: Some(1),
                },
            );
        }

        let app = test_app(state);
        let (_, json) = fetch_targets(app).await;
        let targets = json["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 2);

        let openai = targets.iter().find(|t| t["provider"] == "openai").unwrap();
        assert_eq!(
            openai["fill_first_position"], 0,
            "openai is the fill-first target (index 0 of the cached order)"
        );
        assert_eq!(openai["strategy"], "fill_first");
        assert_eq!(openai["status"], "idle", "fill-first never pins");

        let anthropic = targets
            .iter()
            .find(|t| t["provider"] == "anthropic")
            .unwrap();
        assert_eq!(anthropic["fill_first_position"], 1);

        // Cache age: recorded 123s before the snapshot; `elapsed()` may
        // straddle a second boundary, so allow 123 or 124.
        let age = openai["fill_first_cache_age_secs"].as_u64().unwrap();
        assert!((123..=124).contains(&age));
        assert_eq!(
            openai["fill_first_cache_ttl_secs"], 7200,
            "default shuffle_cache_ttl_secs"
        );
    }

    #[tokio::test]
    async fn fill_first_cache_reports_zero_ttl_as_never_expire() {
        // `shuffle_cache_ttl_secs = 0` means the order never expires — the
        // snapshot must report TTL 0 (not `null`).
        let config = sag_config::GatewayConfig {
            shuffle_cache_ttl_secs: 0,
            routes: vec![route_with_strategy(
                "gpt-4o",
                vec![target("openai", Some("gpt-4o"), 10)],
                sag_config::Strategy::FillFirst,
            )],
            ..sag_config::gateway_config_base!()
        };
        let handle = sag_config::ConfigHandle::from_config(config);
        let registry = build_provider_registry(&handle, true).await;
        let state = test_app_state(handle, registry).await;
        state.fill_first_orders.write().await.insert(
            "gpt-4o".into(),
            sag_state::CachedOrder {
                targets: std::sync::Arc::from(vec![target("openai", Some("gpt-4o"), 10)]),
                created_at: Instant::now(),
                routes_version: Some(1),
            },
        );

        let app = test_app(state);
        let (_, json) = fetch_targets(app).await;
        let targets = json["targets"].as_array().unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(
            targets[0]["fill_first_cache_ttl_secs"], 0,
            "ttl 0 must be reported as 0 (never expire), not null"
        );
    }

    #[tokio::test]
    async fn delete_fill_first_resets_order_cache_and_clears_stale_pin() {
        // A fill-first DELETE drops the cached shuffled order (next request
        // re-shuffles) AND clears a stale sticky-failover pin left behind by
        // a previous StickyFailover config — the user-visible way to get rid
        // of the residual after switching strategies.
        let state = state_with_routes(vec![route_with_strategy(
            "gpt-4o",
            vec![target("openai", Some("gpt-4o"), 10)],
            sag_config::Strategy::FillFirst,
        )])
        .await;
        state.fill_first_orders.write().await.insert(
            "gpt-4o".into(),
            sag_state::CachedOrder {
                targets: std::sync::Arc::from(vec![target("openai", Some("gpt-4o"), 10)]),
                created_at: Instant::now(),
                routes_version: Some(1),
            },
        );
        // A pre-existing pin on the same model (stale, from the route's
        // earlier StickyFailover config): must be cleared by the fill-first
        // delete — that is the residual-state fix.
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

        // First delete: drops the cached order and clears the stale pin.
        let app = test_app(state.clone());
        let (status, json) = delete_target(app, serde_json::json!({ "model": "gpt-4o" })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["ok"], true);
        assert!(
            state.fill_first_orders.read().await.get("gpt-4o").is_none(),
            "fill-first DELETE must drop the cached order"
        );
        assert!(
            state.active_targets.read().await.get("gpt-4o").is_none(),
            "fill-first DELETE must clear the stale sticky active pin"
        );

        // Second delete (order and pin already gone) is a 200 no-op —
        // idempotent.
        let app2 = test_app(state.clone());
        let (status2, json2) = delete_target(app2, serde_json::json!({ "model": "gpt-4o" })).await;
        assert_eq!(status2, StatusCode::OK);
        assert_eq!(json2["ok"], true);
        assert!(state.fill_first_orders.read().await.is_empty());
        assert!(state.active_targets.read().await.get("gpt-4o").is_none());
    }

    #[tokio::test]
    async fn delete_active_target_does_not_touch_fill_first_cache() {
        // A sticky-failover DELETE clears the pin only — the fill-first
        // order cache (which StickyFailover also uses internally as its base
        // order) must be left alone.
        let state = state_with_routes(vec![route(
            "gpt-4o",
            vec![target("openai", Some("gpt-4o"), 10)],
        )])
        .await;
        state.fill_first_orders.write().await.insert(
            "gpt-4o".into(),
            sag_state::CachedOrder {
                targets: std::sync::Arc::from(vec![target("openai", Some("gpt-4o"), 10)]),
                created_at: Instant::now(),
                routes_version: Some(1),
            },
        );
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

        let app = test_app(state.clone());
        let (status, _) = delete_target(app, serde_json::json!({ "model": "gpt-4o" })).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            state.active_targets.read().await.get("gpt-4o").is_none(),
            "sticky DELETE clears the pin"
        );
        assert!(
            state.fill_first_orders.read().await.contains_key("gpt-4o"),
            "sticky DELETE must not drop the fill-first order cache"
        );
    }

    #[tokio::test]
    async fn delete_fill_first_on_wildcard_route_resets_by_route_name() {
        // A wildcard fill-first route stores its one order under "*";
        // DELETE with the route name "*" must drop it (the fill-first
        // analogue of the sticky wildcard case).
        let state = state_with_routes(vec![route_with_strategy(
            "*",
            vec![target("openai", None, 10)],
            sag_config::Strategy::FillFirst,
        )])
        .await;
        state.fill_first_orders.write().await.insert(
            "*".into(),
            sag_state::CachedOrder {
                targets: std::sync::Arc::from(vec![target("openai", None, 10)]),
                created_at: Instant::now(),
                routes_version: Some(1),
            },
        );

        let app = test_app(state.clone());
        let (status, json) = delete_target(app, serde_json::json!({ "model": "*" })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["ok"], true);
        assert!(
            state.fill_first_orders.read().await.get("*").is_none(),
            "wildcard fill-first DELETE must drop the '*' order cache entry"
        );
    }

    #[tokio::test]
    async fn delete_wildcard_clears_all_concrete_pins_not_other_routes() {
        // The user-facing fix: on a wildcard route the active pins are keyed
        // by concrete request models (e.g. "gpt-4.1"), and the frontend only
        // knows the route name "*". DELETE with model="*" must clear every
        // concrete pin belonging to the wildcard route — and nothing else:
        // an exact route's pin on the same provider must survive, and a
        // wildcard pin shadowed by an exact route must survive too.
        let state = state_with_routes(vec![
            route("gpt-4o", vec![target("openai", Some("gpt-4o"), 10)]),
            route("*", vec![target("openai", None, 5)]),
        ])
        .await;
        {
            let mut guard = state.active_targets.write().await;
            // Concrete wildcard pins (the ones the UI cannot name).
            guard.insert(
                "gpt-4.1".into(),
                ActiveTargetEntry::new("openai", "gpt-4.1", "*", None, Instant::now()),
            );
            guard.insert(
                "claude-3-opus".into(),
                ActiveTargetEntry::new("openai", "claude-3-opus", "*", None, Instant::now()),
            );
            // Exact-route pin on the same provider — must survive.
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
        }

        let app = test_app(state.clone());
        let (status, json) = delete_target(app, serde_json::json!({ "model": "*" })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["ok"], true);

        let inner = state.active_targets.read().await;
        assert!(
            !inner.contains_key("gpt-4.1"),
            "concrete wildcard pin must be cleared"
        );
        assert!(
            !inner.contains_key("claude-3-opus"),
            "second concrete wildcard pin must be cleared"
        );
        assert!(
            inner.contains_key("gpt-4o"),
            "exact-route pin must NOT be cleared by the wildcard DELETE"
        );
        assert_eq!(inner.get("gpt-4o").unwrap().route_model, "gpt-4o");
        assert_eq!(
            inner.generation("gpt-4.1"),
            1,
            "cleared concrete key's generation bumps so stale writebacks fail"
        );
        assert_eq!(
            inner.epoch(),
            1,
            "wildcard DELETE bumps the global epoch (never-pinned concrete keys)"
        );
    }

    #[tokio::test]
    async fn delete_wildcard_does_not_clear_shadowed_pin() {
        // A wildcard pin whose request key is now shadowed by an exact route
        // no longer belongs to the wildcard route: the wildcard DELETE must
        // leave it for the exact route's own DELETE.
        let state = state_with_routes(vec![
            route("gpt-4o", vec![target("openai", None, 10)]),
            route("*", vec![target("openai", None, 5)]),
        ])
        .await;
        state.active_targets.write().await.insert(
            "gpt-4o".into(),
            ActiveTargetEntry::new("openai", "gpt-4o", "*", None, Instant::now()),
        );
        state.active_targets.write().await.insert(
            "gpt-4.1".into(),
            ActiveTargetEntry::new("openai", "gpt-4.1", "*", None, Instant::now()),
        );

        let app = test_app(state.clone());
        let (status, _) = delete_target(app, serde_json::json!({ "model": "*" })).await;
        assert_eq!(status, StatusCode::OK);

        let inner = state.active_targets.read().await;
        assert!(
            inner.contains_key("gpt-4o"),
            "shadowed wildcard pin survives the wildcard DELETE"
        );
        assert!(
            !inner.contains_key("gpt-4.1"),
            "still-wildcard pin is cleared"
        );
    }

    #[tokio::test]
    async fn delete_exact_route_does_not_clear_wildcard_pins() {
        // A plain route DELETE keeps its single-key semantics: DELETE
        // "gpt-4o" clears only that key — a wildcard concrete pin on another
        // request model must survive (no wildcard path is taken).
        let state = state_with_routes(vec![
            route("gpt-4o", vec![target("openai", Some("gpt-4o"), 10)]),
            route("*", vec![target("openai", None, 5)]),
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
        state.active_targets.write().await.insert(
            "gpt-4.1".into(),
            ActiveTargetEntry::new("openai", "gpt-4.1", "*", None, Instant::now()),
        );

        let app = test_app(state.clone());
        let (status, _) = delete_target(app, serde_json::json!({ "model": "gpt-4o" })).await;
        assert_eq!(status, StatusCode::OK);

        let inner = state.active_targets.read().await;
        assert!(
            !inner.contains_key("gpt-4o"),
            "exact DELETE clears its own pin"
        );
        assert!(
            inner.contains_key("gpt-4.1"),
            "wildcard concrete pin must NOT be cleared by an exact-route DELETE"
        );
    }

    #[tokio::test]
    async fn delete_wildcard_fill_first_clears_concrete_orders_only() {
        // A wildcard fill-first route caches its shuffled orders under the
        // concrete request models. DELETE with model="*" must drop exactly
        // those keys that still belong to the wildcard route — an exact
        // route sharing a key (here "gpt-4o") keeps its own cached order.
        let state = state_with_routes(vec![
            route_with_strategy(
                "gpt-4o",
                vec![target("openai", Some("gpt-4o"), 10)],
                sag_config::Strategy::FillFirst,
            ),
            route_with_strategy(
                "*",
                vec![target("openai", None, 5)],
                sag_config::Strategy::FillFirst,
            ),
        ])
        .await;
        {
            let mut guard = state.fill_first_orders.write().await;
            for key in ["gpt-4.1", "claude-3-opus"] {
                guard.insert(
                    key.into(),
                    sag_state::CachedOrder {
                        targets: std::sync::Arc::from(vec![target("openai", None, 5)]),
                        created_at: Instant::now(),
                        routes_version: Some(1),
                    },
                );
            }
            // The exact route's own cache under the same-looking concrete key
            // "gpt-4o" — must survive (it resolves to the exact route).
            guard.insert(
                "gpt-4o".into(),
                sag_state::CachedOrder {
                    targets: std::sync::Arc::from(vec![target("openai", Some("gpt-4o"), 10)]),
                    created_at: Instant::now(),
                    routes_version: Some(1),
                },
            );
        }

        let app = test_app(state.clone());
        let (status, _) = delete_target(app, serde_json::json!({ "model": "*" })).await;
        assert_eq!(status, StatusCode::OK);

        let orders = state.fill_first_orders.read().await;
        assert!(!orders.contains_key("gpt-4.1"));
        assert!(!orders.contains_key("claude-3-opus"));
        assert!(
            orders.contains_key("gpt-4o"),
            "an exact route sharing the key keeps its cached order"
        );
    }
}
