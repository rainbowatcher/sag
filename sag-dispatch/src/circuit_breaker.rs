//! Re-export shim — the circuit breaker implementation has migrated to
//! [`sag_core::circuit_breaker`].
//!
//! This module re-exports the public types so that existing `crate::circuit_breaker::`
//! references within `sag-dispatch` (in `state.rs`, `dispatch/mod.rs`,
//! `dispatch/chat.rs`, `dispatch/image.rs`) continue to work without changes.
//! External consumers should prefer importing from `sag_core` directly.

pub use sag_core::{CircuitBreakerEntry, CircuitStateKind, RouteCircuitBreaker, RouteTargetKey};
