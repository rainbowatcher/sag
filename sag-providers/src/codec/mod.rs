//! Protocol codecs: Chat Completions / Anthropic Messages / Responses ↔ IR (M2/M5/M6).
//!
//! Pure mapping functions only.

mod anthropic;
mod chat;
mod responses;

pub use anthropic::{
    AnthropicMessagesCodec, DEFAULT_MAX_TOKENS, inject_cache_control_value, resolve_budget,
};
pub use chat::ChatCompletionsCodec;
pub use responses::ResponsesCodec;
