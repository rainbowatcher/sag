pub mod anthropic;
pub mod base;
pub mod openai;
pub mod traits;
pub mod util;

pub use anthropic::*;
pub use base::*;
pub use openai::*;
pub use traits::*;
pub use util::*;

/// Create a provider adapter from its type and configuration.
///
/// Returns a type-erased `Arc<dyn LlmProvider>` ready for the runtime
/// registry.  This keeps the `ProviderType` → adapter dispatch in one
/// place so adding a new provider only touches two files (the new
/// adapter module + this function).
pub fn create_provider(
    name: String,
    config: &sag_config::ProviderConfig,
) -> std::sync::Arc<dyn LlmProvider> {
    match config.provider_type {
        sag_config::ProviderType::OpenAI => std::sync::Arc::new(OpenAiProvider::new(name, config)),
        sag_config::ProviderType::Anthropic => {
            std::sync::Arc::new(AnthropicProvider::new(name, config))
        }
    }
}
