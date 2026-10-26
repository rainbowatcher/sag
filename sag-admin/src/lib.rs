//! Admin REST API CRUD handlers for the SAG gateway.
//!
//! This crate owns the management endpoints that were previously part of
//! `sag-router`:
//!
//! - [`admin`] — route wiring, builds the `/api/*` Axum [`axum::Router`].
//! - [`admin_providers`] — `GET|POST|PUT|DELETE /api/providers`.
//! - [`admin_routes`] — `GET|POST|PUT|DELETE /api/routes`.
//! - [`admin_logs`] — `GET /api/logs`.
//! - [`admin_settings`] — `GET|PUT /api/settings`.
//! - [`admin_config`] — `GET|PUT /api/config`.
//! - [`admin_testing`] — `POST /api/providers/fetch-models`,
//!   `POST /api/providers/probe-protocols` and `POST /api/model/test`.
//! - [`admin_circuit_breaker`] — `GET /api/circuit-breaker` and
//!   `POST /api/circuit-breaker/close`.
//! - [`admin_active_targets`] — `GET|DELETE /api/active-targets`.
//! - [`admin_session_affinity`] — `GET|DELETE /api/session-affinity`.
//! - [`admin_inflight`] — `GET /api/inflight` (SSE per-route in-flight counters).

pub mod admin;
pub mod admin_active_targets;
pub mod admin_circuit_breaker;
pub mod admin_config;
pub mod admin_inflight;
pub mod admin_logs;
pub mod admin_providers;
pub mod admin_routes;
pub mod admin_session_affinity;
pub mod admin_settings;
pub mod admin_testing;

pub(crate) mod probe_provider;
pub(crate) mod probe_request;

pub use admin::build_admin_routes;

#[cfg(test)]
mod test_utils;
