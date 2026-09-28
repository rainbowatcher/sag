//! Shared test helpers for provider adapter unit tests.
//!
//! Only compiled under `#[cfg(test)]`. Provides a one-shot TCP mock server
//! plus small config/request builders used by the `base`, `openai`, and
//! `anthropic` test modules. No external network dependency.

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
/// timeout settings. Every API kind (`chat_completions` / `responses` /
/// `anthropic_messages` / `image`) shares the same `base_url` so any
/// protocol path resolves in tests.
pub(crate) fn test_config(
    base_url: &str,
    timeout_secs: u64,
    ttfb_timeout_secs: Option<u64>,
    chunk_timeout_secs: Option<u64>,
) -> ProviderConfig {
    let mut apis = indexmap::IndexMap::new();
    for kind in [
        sag_core::ApiKind::ChatCompletions,
        sag_core::ApiKind::Responses,
        sag_core::ApiKind::AnthropicMessages,
        sag_core::ApiKind::Image,
    ] {
        apis.insert(
            kind,
            sag_config::ApiConfig {
                base_url: base_url.into(),
                ..Default::default()
            },
        );
    }
    ProviderConfig {
        api_keys: vec!["sk-test".into()],
        key_strategy: sag_config::KeyStrategy::FillFirst,
        timeout_secs,
        chunk_timeout_secs,
        ttfb_timeout_secs,
        anthropic_cache_control: true,
        apis,
        ..Default::default()
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
