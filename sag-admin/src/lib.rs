//! Admin REST API CRUD handlers for the SAG gateway.
//!
//! This crate owns the management endpoints that were previously part of
//! `sag-router`:
//!
//! - [`admin`] — route wiring, builds the `/api/*` Axum [`Router`].
//! - [`admin_providers`] — `GET|POST|PUT|DELETE /api/providers`.
//! - [`admin_routes`] — `GET|POST|PUT|DELETE /api/routes`.
//! - [`admin_logs`] — `GET /api/logs`.
//! - [`admin_testing`] — `POST /api/providers/fetch-models` and
//!   `POST /api/model/test`.

pub mod admin;
pub mod admin_logs;
pub mod admin_providers;
pub mod admin_routes;
pub mod admin_testing;

pub use admin::build_admin_routes;
