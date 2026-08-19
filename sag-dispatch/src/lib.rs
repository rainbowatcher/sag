//! Multi-provider failover dispatch engine for SAG.
//!
//! This crate owns the core dispatch logic that was previously part of
//! `sag-router`:
//!
//! - [`order`] — target ordering strategies (FillFirst / RoundRobin).
//! - [`dispatch`] — route resolution, per-target retry loop, chat & image
//!   dispatch fallback, and SSE streaming response builder.
//! - [`state`] — [`DispatchRuntime`] (dispatch-only runtime state).
//!   [`AppState`] has moved to `sag-state`; this module re-exports it
//!   for backwards compatibility.
//! - [`testing`] — test helpers for constructing [`DispatchRuntime`]
//!   (not gated by `#[cfg(test)]` so integration tests can reach them).
//!   The `test_app_state` / `build_provider_registry` helpers have also
//!   moved to `sag-state` and are re-exported here.
//!
//! Circuit-breaker primitives (`RouteCircuitBreaker`, `RouteTargetKey`,
//! `CircuitBreakerEntry`, `CircuitStateKind`) live in `sag-core` and are
//! re-exported there — import them from `sag_core` directly.

pub mod dispatch;
pub mod order;
pub mod state;
pub mod testing;

// ── Re-exports ──────────────────────────────────────────────────────

pub use dispatch::{
    DispatchCtx, ResolvedRoute, ResolvedTarget, dispatch_chat, dispatch_image, resolve_route,
};
pub use order::order_targets;
pub use sag_state::CachedOrder;
pub use state::{AppState, DispatchRuntime, stale_probe_threshold_from_config};

// Re-export test helpers (not #[cfg(test)] so integration tests can use them).
pub use testing::{test_app_state, test_dispatch_runtime};
