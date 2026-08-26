//! Shared test helpers for provider adapter unit tests.
//!
//! Only compiled under `#[cfg(test)]`. Provides a one-shot TCP mock server
//! plus small config/request builders used by the `base`, `openai`, and
//! `anthropic` test modules. No external network dependency.

use std::collections::HashMap;

use sag_config::ProviderConfig;
use sag_core::{ChatCompletionRequest, ChatMessage};

/// Spawn a one-shot TCP server. `handler` receives the accepted stream
/// after the request bytes are (best-effort) drained, and drives the
/// response however the test needs. Returns the bound `http://` base URL.
pub(crate) async fn spawn_server<F, Fut>(handler: F) -> String
where
    F: FnOnce(tokio::net::TcpStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    use tokio::io::AsyncReadExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // Drain the request headers so the client's send side completes.
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf).await;
        handler(stream).await;
    });
    format!("http://{addr}")
}

/// Build a minimal `ProviderConfig` pointing at `base_url` with the given
/// timeout settings. `provider_type` is left at OpenAI — construction does
/// not depend on it.
pub(crate) fn test_config(
    base_url: &str,
    timeout_secs: u64,
    ttfb_timeout_secs: Option<u64>,
    chunk_timeout_secs: Option<u64>,
) -> ProviderConfig {
    ProviderConfig {
        api_keys: vec!["sk-test".into()],
        provider_type: sag_config::ProviderType::OpenAI,
        key_strategy: sag_config::KeyStrategy::FillFirst,
        base_url: base_url.into(),
        models_endpoint: "models".into(),
        timeout_secs,
        headers: HashMap::new(),
        body_overrides: HashMap::new(),
        strip_fields: vec![],
        model_strip_fields: HashMap::new(),
        model_overrides: HashMap::new(),
        enabled: true,
        enabled_models: Vec::new(),
        role_mappings: HashMap::new(),
        anthropic_cache_control: true,
        chunk_timeout_secs,
        ttfb_timeout_secs,
    }
}

/// A minimal chat request with a single user message.
pub(crate) fn chat_request() -> ChatCompletionRequest {
    ChatCompletionRequest {
        model: "test-model".into(),
        messages: vec![ChatMessage {
            role: "user".into(),
            content: serde_json::Value::String("hi".into()),
            name: None,
            extra: serde_json::Map::new(),
        }],
        ..Default::default()
    }
}
