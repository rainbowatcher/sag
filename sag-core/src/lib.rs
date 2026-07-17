//! SAG Core — Shared data types, error definitions, and common utilities.
//!
//! Also hosts [`circuit_breaker`] — the in-memory per-target circuit breaker
//! state machine. It lives here (rather than in `sag-dispatch`) so that both
//! the dispatch engine and the shared [`AppState`] (in `sag-state`) can hold
//! a handle to the same `Arc<RwLock<RouteCircuitBreaker>>` without `sag-state`
//! depending on `sag-dispatch`. The breaker's `CircuitHint` classification
//! logic is co-located in [`error`] (`AppError::circuit_hint`), so the two
//! halves of the circuit-breaker feature share one crate.

pub mod circuit_breaker;
pub mod error;
pub mod types;

// Re-export key types at crate level for convenience
pub use circuit_breaker::{
    CircuitBreakerEntry, CircuitStateKind, RouteCircuitBreaker, RouteTargetKey,
};
pub use error::AppError;
pub use types::{
    CallLog, ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, ImageData,
    ImageGenerationRequest, ImageGenerationResponse, StopSequence, StreamChunk, Usage,
};

/// Convenient alias for `Result<T, error::AppError>`.
pub type Result<T> = std::result::Result<T, AppError>;
