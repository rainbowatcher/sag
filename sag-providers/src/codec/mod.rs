//! Protocol codecs: Chat Completions / Anthropic Messages ↔ IR (M2/M5).
//!
//! Pure mapping functions only.

mod anthropic;
mod chat;

pub use anthropic::{
    AnthropicMessagesCodec, DEFAULT_MAX_TOKENS, inject_cache_control_value, resolve_budget,
};
pub use chat::ChatCompletionsCodec;
