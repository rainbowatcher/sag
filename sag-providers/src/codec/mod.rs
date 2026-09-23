//! Protocol codecs: Chat Completions ↔ IR (M2).
//!
//! Pure mapping functions only — not yet wired into dispatch/providers.

mod chat;

pub use chat::ChatCompletionsCodec;
