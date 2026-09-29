pub mod anthropic;
pub mod base;
pub mod catalog;
pub mod codec;
pub mod credential;
pub mod openai;
pub mod traits;
pub mod util;

#[cfg(test)]
mod test_support;

pub use anthropic::*;
pub use base::*;
pub use catalog::*;
pub use codec::*;
pub use credential::*;
pub use openai::*;
pub use traits::*;
pub use util::*;

/// Create a provider adapter from its configuration.
///
/// Returns a type-erased `Arc<dyn LlmProvider>` ready for the runtime
/// registry. Construction is driven purely by the `apis` catalog in
/// [`sag_config::ProviderConfig`] (no `provider_type` dispatch): a
/// [`CatalogProvider`] serves every protocol key the provider declares,
/// within a single instance.
///
/// Connects **directly**: the system proxy environment variables are
/// explicitly ignored, never inherited. Use
/// [`create_provider_with_proxy`] to pass an effective proxy.
pub fn create_provider(
    name: String,
    config: &sag_config::ProviderConfig,
) -> sag_core::Result<std::sync::Arc<dyn LlmProvider>> {
    create_provider_with_proxy(name, config, None)
}

/// Create a provider adapter with an explicit effective proxy.
///
/// `proxy: None` → direct connection with the system proxy explicitly
/// disabled. `Some(proxy)` → all upstream traffic is routed through that
/// single proxy URL; the system proxy environment variables are ignored in
/// both cases.
///
/// Production callers resolve the effective proxy via
/// `sag_config::resolve_effective_proxy(mode, global_proxy)` and pass the
/// resulting `Option<ProxyConfig>` here (it is reconstructed with
/// `ProxyConfig::to_connect_url` — no re-parsing).
pub fn create_provider_with_proxy(
    name: String,
    config: &sag_config::ProviderConfig,
    proxy: Option<&sag_config::ProxyConfig>,
) -> sag_core::Result<std::sync::Arc<dyn LlmProvider>> {
    let provider: std::sync::Arc<dyn LlmProvider> =
        std::sync::Arc::new(CatalogProvider::new_with_proxy(name, config, proxy)?);
    Ok(provider)
}
