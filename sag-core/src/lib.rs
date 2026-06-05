//! SAG Core — Shared data types, error definitions, and common utilities.

pub mod error;
pub mod types;

// Re-export key types at crate level for convenience
pub use error::AppError;
pub use types::{CallLog, ChatChoice, ChatCompletionRequest, ChatCompletionResponse, ChatMessage, ImageData, ImageGenerationRequest, ImageGenerationResponse, Usage};

/// Convenient alias for `Result<T, error::AppError>`.
pub type Result<T> = std::result::Result<T, AppError>;
