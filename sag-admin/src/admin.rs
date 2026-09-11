//! Admin API routes mounted under `/api/*`.
//!
//! This file owns the route wiring only. The handlers themselves live in:
//!
//! - [`admin_providers`] — `GET|POST|PUT|DELETE /api/providers` and
//!   `GET /api/providers/{name}/models`
//! - [`admin_routes`] — `GET|POST|PUT|DELETE /api/routes`
//! - [`admin_testing`] — `POST /api/providers/fetch-models` and
//!   `POST /api/model/test`
//! - [`admin_settings`] — `GET|PUT /api/settings`.
//! - [`admin_circuit_breaker`] — `GET /api/circuit-breaker` and
//!   `POST /api/circuit-breaker/close`
//! - [`admin_active_targets`] — `GET|DELETE /api/active-targets`
//!
//! All admin routes are intended to be wrapped in the `gateway_auth`
//! middleware by the parent router in `routes.rs`.

use axum::{Router, routing::get, routing::post};

use crate::admin_active_targets::{delete_active_target, list_active_targets};
use crate::admin_circuit_breaker::{close_circuit_breaker, list_circuit_breakers};
use crate::admin_logs::list_logs;
use crate::admin_providers::{
    add_provider, delete_provider, list_provider_models, list_providers, update_provider,
};
use crate::admin_routes::{create_route, delete_route, list_routes, update_route};
use crate::admin_settings::{get_settings, update_settings};
use crate::admin_testing::{fetch_models, model_test};
use sag_state::AppState;

/// Build admin routes under `/api/*` (all auth-protected by the parent).
pub fn build_admin_routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/providers",
            get(list_providers)
                .post(add_provider)
                .put(update_provider)
                .delete(delete_provider),
        )
        .route("/providers/{name}/models", get(list_provider_models))
        .route("/providers/fetch-models", post(fetch_models))
        .route("/model/test", post(model_test))
        .route(
            "/routes",
            get(list_routes)
                .post(create_route)
                .put(update_route)
                .delete(delete_route),
        )
        .route("/settings", get(get_settings).put(update_settings))
        .route("/logs", get(list_logs))
        .route("/circuit-breaker", get(list_circuit_breakers))
        .route("/circuit-breaker/close", post(close_circuit_breaker))
        .route(
            "/active-targets",
            get(list_active_targets).delete(delete_active_target),
        )
        .with_state(state)
}
