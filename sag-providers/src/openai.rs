use std::collections::HashMap;
use std::pin::Pin;

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use sag_core::{
    AppError, ChatCompletionRequest, ChatCompletionResponse, ImageGenerationRequest,
    ImageGenerationResponse, StreamChunk,
};
use tokio::sync::mpsc;
use tracing::{error, instrument, trace};

use crate::{LlmProvider, ProviderBase, drain_sse_frames};

/// OpenAI-compatible provider adapter.
///
/// Uses the OpenAI Chat Completions API (and any drop-in compatible endpoints).
pub struct OpenAiProvider {
    base: ProviderBase,
}

impl OpenAiProvider {
    /// Create a new OpenAI provider from configuration.
    ///
    /// Connects **directly**: the system proxy environment variables
    /// (`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`) are explicitly ignored.
    /// Use [`OpenAiProvider::new_with_proxy`] to route upstream traffic
    /// through an explicit proxy.
    pub fn new(name: String, config: &sag_config::ProviderConfig) -> sag_core::Result<Self> {
        Self::new_with_proxy(name, config, None)
    }

    /// Create a new OpenAI provider from configuration with an explicit
    /// effective proxy. See [`ProviderBase::new_with_proxy`] for the proxy
    /// semantics.
    pub fn new_with_proxy(
        name: String,
        config: &sag_config::ProviderConfig,
        proxy: Option<&sag_config::ProxyConfig>,
    ) -> sag_core::Result<Self> {
        Ok(Self {
            base: ProviderBase::new_with_proxy(name, config, proxy)?,
        })
    }

    fn auth_header(&self) -> (&str, String) {
        (
            "Authorization",
            format!("Bearer {}", self.base.current_key()),
        )
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn name(&self) -> &str {
        &self.base.name
    }

    #[instrument(skip_all, fields(provider = %self.base.name, model = %real_model))]
    async fn chat(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
        timeout_secs: u64,
    ) -> sag_core::Result<ChatCompletionResponse> {
        let mut upstream_request = request.clone();
        self.base
            .apply_role_mappings(&mut upstream_request.messages);
        upstream_request.model = real_model.to_string();

        if !request.extra.is_empty() {
            let keys: Vec<&str> = request.extra.keys().map(|s| s.as_str()).collect();
            trace!(extra = ?keys, "forwarding non-standard extra parameters");
        }

        let url = self.base.build_url(api_endpoint);

        let body = serde_json::to_value(&upstream_request)
            .map_err(|e| AppError::Internal(format!("failed to serialize request: {e}")))?;
        let (auth_name, auth_value) = self.auth_header();
        let body_text = self
            .base
            .send_json(
                &url,
                body,
                target_extra,
                &[(auth_name, &auth_value)],
                Some(timeout_secs),
            )
            .await?;

        let mut completion: ChatCompletionResponse = serde_json::from_str(&body_text)
            .map_err(|e| AppError::Internal(format!("failed to parse upstream response: {e}")))?;

        // Patch nested cache/reasoning token fields from the raw usage
        // object. The derived Deserialize for Usage sees a flat struct,
        // so `prompt_tokens_details.cached_tokens` and
        // `completion_tokens_details.reasoning_tokens` are silently ignored.
        // This post-deserialization patch lifts them into the top-level
        // Usage fields without requiring a custom deserializer.
        if let Some(ref mut usage) = completion.usage
            && let Ok(raw) = serde_json::from_str::<serde_json::Value>(&body_text)
            && let Some(raw_usage) = raw.get("usage")
        {
            let pt_details = raw_usage.get("prompt_tokens_details");
            usage.cached_tokens = pt_details
                .and_then(|v| v.get("cached_tokens"))
                .or_else(|| raw_usage.get("prompt_cache_hit_tokens"))
                .or_else(|| raw_usage.get("cached_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
                .or(pt_details.is_some().then_some(0));
            usage.cache_creation_tokens = pt_details
                .and_then(|v| v.get("cache_creation_input_tokens"))
                .or_else(|| raw_usage.get("cache_creation_input_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            usage.reasoning_tokens = raw_usage
                .get("completion_tokens_details")
                .and_then(|v| v.get("reasoning_tokens"))
                .or_else(|| raw_usage.get("reasoning_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
        }

        Ok(completion)
    }

    #[instrument(skip_all, fields(provider = %self.base.name, model = %real_model))]
    async fn chat_stream(
        &self,
        request: &ChatCompletionRequest,
        real_model: &str,
        api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
        _timeout_secs: u64,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        let mut upstream_request = request.clone();
        self.base
            .apply_role_mappings(&mut upstream_request.messages);
        upstream_request.model = real_model.to_string();
        upstream_request.stream = Some(true);
        // Force include_usage: SAG needs usage for CallLog stats; a client's
        // stream_options (especially include_usage: false) must not override
        // this internal requirement. OpenAI-compatible backends interpret
        // stream_options.include_usage as documented.
        upstream_request.stream_options = Some(serde_json::json!({"include_usage": true}));

        if !request.extra.is_empty() {
            let keys: Vec<&str> = request.extra.keys().map(|s| s.as_str()).collect();
            trace!(extra = ?keys, "forwarding non-standard extra parameters");
        }

        let url = self.base.build_url(api_endpoint);

        let body = serde_json::to_value(&upstream_request)
            .map_err(|e| AppError::Internal(format!("failed to serialize request: {e}")))?;

        let (auth_name, auth_value) = self.auth_header();
        let response = self
            .base
            .send_json_stream(&url, body, target_extra, &[(auth_name, &auth_value)])
            .await?;

        let (tx, rx) = mpsc::unbounded_channel::<StreamChunk>();
        let chunk_timeout = self
            .base
            .chunk_timeout_secs()
            .map(std::time::Duration::from_secs);

        tokio::spawn(async move {
            let mut byte_stream = response.bytes_stream();
            let mut buffer: Vec<u8> = Vec::new();
            let mut had_error = false;

            loop {
                // Stop reading if the client has disconnected
                if tx.is_closed() {
                    break;
                }

                let chunk_result: Result<_, tokio::time::error::Elapsed> = match &chunk_timeout {
                    Some(dur) => tokio::time::timeout(*dur, byte_stream.next()).await,
                    None => Ok(byte_stream.next().await),
                };

                match chunk_result {
                    Ok(Some(Ok(chunk))) => {
                        trace!(received_bytes = chunk.len(), "received upstream SSE chunk");
                        buffer.extend_from_slice(&chunk);
                        drain_sse_frames(&mut buffer, &tx);
                    }
                    Ok(Some(Err(e))) => {
                        error!(?e, "error reading upstream stream chunk");
                        if !tx.is_closed() {
                            let _ = tx.send(StreamChunk::Error(e.to_string()));
                        }
                        had_error = true;
                        break;
                    }
                    Ok(None) => break,
                    Err(_elapsed) => {
                        error!(chunk_timeout_secs = ?chunk_timeout, "stream chunk idle timeout");
                        if !tx.is_closed() {
                            let _ = tx.send(StreamChunk::Error(format!(
                                "stream chunk idle timeout after {}s",
                                chunk_timeout.unwrap().as_secs()
                            )));
                        }
                        had_error = true;
                        break;
                    }
                }
            }

            if !buffer.is_empty() {
                crate::flush_sse_buffer(&mut buffer, &tx);
            }

            if !had_error {
                let _ = tx.send(StreamChunk::Done);
            }
        });

        let stream = futures_util::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|chunk| (chunk, rx))
        });

        Ok(Box::pin(stream))
    }

    #[instrument(skip_all, fields(provider = %self.base.name, model = %real_model))]
    async fn chat_image(
        &self,
        request: &ImageGenerationRequest,
        real_model: &str,
        target_extra: &HashMap<String, serde_json::Value>,
        timeout_secs: u64,
    ) -> sag_core::Result<ImageGenerationResponse> {
        let mut upstream = request.clone();
        upstream.model = real_model.to_string();

        let url = self.base.build_url("/images/generations");

        let body = serde_json::to_value(&upstream)
            .map_err(|e| AppError::Internal(format!("failed to serialize request: {e}")))?;

        let (auth_name, auth_value) = self.auth_header();
        let body_text = self
            .base
            .send_json(
                &url,
                body,
                target_extra,
                &[(auth_name, &auth_value)],
                Some(timeout_secs),
            )
            .await?;

        serde_json::from_str(&body_text).map_err(|e| {
            AppError::Internal(format!("failed to parse image generation response: {e}"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{chat_request, spawn_server, test_config};
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    fn openai_provider(
        base_url: &str,
        timeout_secs: u64,
        chunk_timeout_secs: Option<u64>,
    ) -> OpenAiProvider {
        OpenAiProvider::new(
            "test".into(),
            &test_config(base_url, timeout_secs, Some(60), chunk_timeout_secs),
        )
        .unwrap()
    }

    /// Server streams 4 chunks at 400ms intervals (~1.6s total), far longer
    /// than `timeout_secs` = 1s, followed by an explicit upstream `[DONE]`
    /// frame and connection close. Continuous chunks must never trip the chunk
    /// idle timeout and the removed client-level total deadline must not
    /// truncate the stream.
    #[tokio::test]
    async fn chat_stream_continuous_chunks_survive_past_timeout_secs() {
        let base_url = spawn_server(|mut stream| async move {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            stream.flush().await.unwrap();
            for i in 0..4 {
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
                let frame = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"chunk {i}\"}}}}]}}\n\n"
                );
                stream.write_all(frame.as_bytes()).await.unwrap();
                stream.flush().await.unwrap();
            }
            // Explicit upstream [DONE] frame (OpenAI wire protocol), then a
            // graceful FIN so the client reliably reads the [DONE] bytes
            // before observing EOF.
            stream.write_all(b"data: [DONE]\n\n").await.unwrap();
            stream.flush().await.unwrap();
            let _ = stream.shutdown().await;
            drop(stream);
        })
        .await;

        let provider = openai_provider(&base_url, 1, Some(2));
        let mut stream = provider
            .chat_stream(
                &chat_request(),
                "test-model",
                "/chat/completions",
                &HashMap::new(),
                1,
            )
            .await
            .expect("stream should be established");

        let mut data_frames: Vec<Vec<u8>> = Vec::new();
        let mut saw_done = false;
        let mut saw_error = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                StreamChunk::Data(d) => data_frames.push(d),
                StreamChunk::Error(_) => saw_error = true,
                StreamChunk::Done => saw_done = true,
                StreamChunk::Usage(_) => {}
            }
        }

        assert!(
            !saw_error,
            "continuous chunks must not trip the chunk idle timeout"
        );
        assert!(
            saw_done,
            "stream must end with Done — emitted by EOF after the [DONE] frame"
        );
        assert_eq!(
            data_frames.len(),
            5,
            "expected 4 content frames + 1 explicit [DONE] frame, got {}",
            data_frames.len()
        );
        for (i, frame) in data_frames[..4].iter().enumerate() {
            let text = String::from_utf8(frame.clone()).unwrap();
            let json: serde_json::Value = text
                .strip_prefix("data: ")
                .and_then(|s| s.strip_suffix("\n\n"))
                .map(|s| serde_json::from_str(s).unwrap())
                .expect("content frame must be a `data: <json>` SSE frame");
            assert_eq!(
                json["choices"][0]["delta"]["content"],
                format!("chunk {i}"),
                "content frame {i} must carry the upstream delta verbatim"
            );
        }
        assert_eq!(
            data_frames[4],
            b"data: [DONE]\n\n".to_vec(),
            "the explicit upstream [DONE] frame must be forwarded as its own Data chunk"
        );
    }

    /// A single chunk followed by a 4s silence — well beyond
    /// `chunk_timeout_secs` (1s). The idle timeout must emit an Error chunk
    /// and never a Done.
    #[tokio::test]
    async fn chat_stream_chunk_idle_timeout_emits_error_not_done() {
        let base_url = spawn_server(|mut stream| async move {
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
            stream.flush().await.unwrap();
            stream
                .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n")
                .await
                .unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            stream
                .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n\n")
                .await
                .unwrap();
            stream.flush().await.unwrap();
            drop(stream);
        })
        .await;

        let provider = openai_provider(&base_url, 30, Some(1));
        let start = std::time::Instant::now();
        let mut stream = provider
            .chat_stream(
                &chat_request(),
                "test-model",
                "/chat/completions",
                &HashMap::new(),
                30,
            )
            .await
            .expect("stream should be established");

        let mut saw_error = false;
        let mut saw_done = false;
        let mut saw_data = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                StreamChunk::Data(_) => saw_data = true,
                StreamChunk::Error(_) => saw_error = true,
                StreamChunk::Done => saw_done = true,
                StreamChunk::Usage(_) => {}
            }
        }
        let elapsed = start.elapsed();

        assert!(saw_data, "expected the first chunk before the silence");
        assert!(saw_error, "chunk idle timeout must emit an Error chunk");
        assert!(!saw_done, "a timed-out stream must not emit Done");
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "idle timeout should fire at ~1s, not wait for the connection close: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn chat_sync_extracts_cache_fields_and_aliases() {
        let test_cases = vec![
            (
                r#"{"id":"1","object":"chat.completion","created":1700000000,"model":"test","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150,"prompt_tokens_details":{"cached_tokens":30,"cache_creation_input_tokens":70},"completion_tokens_details":{"reasoning_tokens":15}}}"#,
                Some(30),
                Some(70),
                Some(15),
            ),
            (
                r#"{"id":"2","object":"chat.completion","created":1700000000,"model":"test","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150,"prompt_cache_hit_tokens":40,"cache_creation_input_tokens":60,"reasoning_tokens":20}}"#,
                Some(40),
                Some(60),
                Some(20),
            ),
            (
                r#"{"id":"3","object":"chat.completion","created":1700000000,"model":"test","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150,"cached_tokens":50}}"#,
                Some(50),
                None,
                None,
            ),
            (
                r#"{"id":"4","object":"chat.completion","created":1700000000,"model":"test","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150,"prompt_tokens_details":{}}}"#,
                Some(0),
                None,
                None,
            ),
            (
                r#"{"id":"5","object":"chat.completion","created":1700000000,"model":"test","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150}}"#,
                None,
                None,
                None,
            ),
        ];

        for (body, exp_cached, exp_creation, exp_reasoning) in test_cases {
            let body_owned = body.to_string();
            let base_url = spawn_server(move |mut stream| {
                let body = body_owned.clone();
                async move {
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    stream.write_all(resp.as_bytes()).await.unwrap();
                    stream.flush().await.unwrap();
                }
            })
            .await;

            let provider = openai_provider(&base_url, 30, None);
            let resp = provider
                .chat(
                    &chat_request(),
                    "test-model",
                    "/chat/completions",
                    &HashMap::new(),
                    30,
                )
                .await
                .unwrap();
            let usage = resp.usage.unwrap();
            assert_eq!(usage.cached_tokens, exp_cached);
            assert_eq!(usage.cache_creation_tokens, exp_creation);
            assert_eq!(usage.reasoning_tokens, exp_reasoning);
        }
    }
}
