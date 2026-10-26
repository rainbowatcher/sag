//! Admin API routes mounted under `/api/*`.
//!
//! This file owns the route wiring only. The handlers themselves live in:
//!
//! - [`crate::admin_providers`] — `GET|POST|PUT|DELETE /api/providers` and
//!   `GET /api/providers/{name}/models`
//! - [`crate::admin_routes`] — `GET|POST|PUT|DELETE /api/routes`
//! - [`crate::admin_testing`] — `POST /api/providers/fetch-models`,
//!   `POST /api/providers/probe-protocols` and `POST /api/model/test`
//! - [`crate::admin_settings`] — `GET|PUT /api/settings`.
//! - [`crate::admin_config`] — `GET|PUT /api/config`.
//! - [`crate::admin_circuit_breaker`] — `GET /api/circuit-breaker` and
//!   `POST /api/circuit-breaker/close`
//! - [`crate::admin_active_targets`] — `GET|DELETE /api/active-targets`
//! - [`crate::admin_inflight`] — `GET /api/inflight` (SSE snapshot stream)
//!
//! All admin routes are intended to be wrapped in the `gateway_auth`
//! middleware by the parent router in `routes.rs`.

use axum::{Router, routing::get, routing::post};

use crate::admin_active_targets::{delete_active_target, list_active_targets};
use crate::admin_circuit_breaker::{close_circuit_breaker, list_circuit_breakers};
use crate::admin_config::{get_config, update_config};
use crate::admin_inflight::stream_inflight;
use crate::admin_logs::list_logs;
use crate::admin_providers::{
    add_provider, delete_provider, list_provider_models, list_providers, update_provider,
};
use crate::admin_routes::{create_route, delete_route, list_routes, update_route};
use crate::admin_session_affinity::{delete_session_affinity, list_session_affinity};
use crate::admin_settings::{get_settings, update_settings};
use crate::admin_testing::{fetch_models, model_test, probe_protocols};
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
        .route("/providers/probe-protocols", post(probe_protocols))
        .route("/model/test", post(model_test))
        .route(
            "/routes",
            get(list_routes)
                .post(create_route)
                .put(update_route)
                .delete(delete_route),
        )
        .route("/settings", get(get_settings).put(update_settings))
        .route("/config", get(get_config).put(update_config))
        .route("/logs", get(list_logs))
        .route("/circuit-breaker", get(list_circuit_breakers))
        .route("/circuit-breaker/close", post(close_circuit_breaker))
        .route(
            "/active-targets",
            get(list_active_targets).delete(delete_active_target),
        )
        .route(
            "/session-affinity",
            get(list_session_affinity).delete(delete_session_affinity),
        )
        .route("/inflight", get(stream_inflight))
        .with_state(state)
}
