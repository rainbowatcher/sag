use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use sag_core::{
    ApiKind, AppError, ChatCompletionRequest, ChatCompletionResponse, ImageGenerationRequest,
    ImageGenerationResponse, StreamChunk,
};
use tokio::sync::mpsc;
use tracing::{error, instrument, trace, warn};

use crate::{
    LlmProvider, ProviderBase, drain_sse_frames, emit_idle_timeout, spawn_in_current_span,
};

/// OpenAI-compatible provider adapter.
///
/// Uses the OpenAI Chat Completions API (and any drop-in compatible endpoints).
/// URLs resolve from `ProviderConfig::apis` per protocol key
/// (`chat_completions` / `responses` / `image`).
pub struct OpenAiProvider {
    base: Arc<ProviderBase>,
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
        Ok(Self::from_base(Arc::new(ProviderBase::new_with_proxy(
            name, config, proxy,
        )?)))
    }

    /// Wrap an existing (possibly shared) provider base.
    pub(crate) fn from_base(base: Arc<ProviderBase>) -> Self {
        Self { base }
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
        _api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
        timeout_secs: u64,
    ) -> sag_core::Result<ChatCompletionResponse> {
        let mut upstream_request = request.clone();
        self.base
            .apply_role_mappings(ApiKind::ChatCompletions, &mut upstream_request.messages);
        upstream_request.model = real_model.to_string();

        if !request.extra.is_empty() {
            let keys: Vec<&str> = request.extra.keys().map(|s| s.as_str()).collect();
            trace!(extra = ?keys, "forwarding non-standard extra parameters");
        }

        // URL derives from apis.chat_completions.base_url + fixed protocol path.
        let url = self.base.protocol_url(ApiKind::ChatCompletions)?;
        let timeout = self
            .base
            .effective_timeout(ApiKind::ChatCompletions, timeout_secs);

        let body = serde_json::to_value(&upstream_request)
            .map_err(|e| AppError::Internal(format!("failed to serialize request: {e}")))?;
        let body_text = self
            .base
            .send_json(
                &url,
                ApiKind::ChatCompletions,
                body,
                target_extra,
                Some(timeout),
            )
            .await?;

        // Some upstreams deliver errors with HTTP 200 by embedding a
        // top-level `error` object in the JSON body (observed with Volcano
        // Ark). Treat it as an upstream error rather than relaying a
        // success-shaped body: the same `AppError::Upstream` the non-2xx
        // path produces, so dispatch records a failed attempt and fails
        // over (or surfaces the all-exhausted envelope) instead of silently
        // reporting success. Rate-limit-ish errors map to 429 so the
        // analytics failure reason is `rate_limited`.
        if let Ok(raw) = serde_json::from_str::<serde_json::Value>(&body_text)
            && crate::base::body_has_error_object(&raw)
        {
            let err_obj = raw.get("error").expect("checked error object");
            let body = crate::base::body_error_message(err_obj);
            let status = if crate::base::body_error_is_rate_limited(err_obj) {
                429
            } else {
                // Upstream error delivered with 200 → 502 (bad gateway).
                502
            };
            warn!(status = 200, %body_text, "upstream returned error");
            return Err(AppError::Upstream {
                provider: self.base.name.clone(),
                status,
                body,
            });
        }

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
        _api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
        _timeout_secs: u64,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        let mut upstream_request = request.clone();
        self.base
            .apply_role_mappings(ApiKind::ChatCompletions, &mut upstream_request.messages);
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

        let url = self.base.protocol_url(ApiKind::ChatCompletions)?;

        let body = serde_json::to_value(&upstream_request)
            .map_err(|e| AppError::Internal(format!("failed to serialize request: {e}")))?;

        let response = self
            .base
            .send_json_stream(&url, ApiKind::ChatCompletions, body, target_extra)
            .await?;

        let (tx, rx) = mpsc::unbounded_channel::<StreamChunk>();
        let chunk_timeout = self
            .base
            .chunk_timeout_secs(ApiKind::ChatCompletions)
            .map(std::time::Duration::from_secs);

        // The reader outlives this call: attach the provider/model span so
        // its chunk-read errors stay correlatable with the request.
        spawn_in_current_span(async move {
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
                        if drain_sse_frames(&mut buffer, &tx) {
                            // A 200-delivered upstream error frame:
                            // `process_sse_frame` already forwarded the frame
                            // and emitted an `Error` chunk. Drop any frames
                            // buffered after it — nothing may be forwarded
                            // beyond the error frame.
                            buffer.clear();
                            had_error = true;
                            break;
                        }
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
                        had_error = emit_idle_timeout(&tx, "openai", &chunk_timeout);
                        break;
                    }
                }
            }

            if !buffer.is_empty() && !had_error {
                // A trailing un-delimited frame at EOF may carry a
                // 200-delivered error — surface it like any other.
                if crate::flush_sse_buffer(&mut buffer, &tx) {
                    had_error = true;
                }
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
        api_endpoint: &str,
        target_extra: &HashMap<String, serde_json::Value>,
        timeout_secs: u64,
    ) -> sag_core::Result<ImageGenerationResponse> {
        let mut upstream = request.clone();
        upstream.model = real_model.to_string();

        // Route-declared path appended to apis.image (fallback:
        // apis.chat_completions) base_url.
        let url = self.base.image_url(api_endpoint)?;
        let timeout = self.base.effective_timeout(ApiKind::Image, timeout_secs);

        let body = serde_json::to_value(&upstream)
            .map_err(|e| AppError::Internal(format!("failed to serialize request: {e}")))?;

        let body_text = self
            .base
            .send_json(&url, ApiKind::Image, body, target_extra, Some(timeout))
            .await?;

        serde_json::from_str(&body_text).map_err(|e| {
            AppError::Internal(format!("failed to parse image generation response: {e}"))
        })
    }

    async fn responses_native(
        &self,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
        api_endpoint: &str,
        timeout_secs: u64,
    ) -> sag_core::Result<serde_json::Value> {
        self.responses_native_inner(body, target_extra, api_endpoint, timeout_secs)
            .await
    }

    async fn responses_stream_native(
        &self,
        body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
        api_endpoint: &str,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        self.responses_stream_native_inner(body, target_extra, api_endpoint)
            .await
    }
}

impl OpenAiProvider {
    /// Native Responses call (M6): send a Responses-shaped body to the
    /// `responses` API node and return the raw JSON.
    ///
    /// The URL derives from `apis.responses.base_url` + the fixed
    /// `responses` protocol path; `api_endpoint` is retained for
    /// signature compatibility but no longer selects the path.
    ///
    /// Never translates through Chat — the body is the caller's
    /// Responses-shaped payload after the three-tier override chain.
    pub async fn responses_native_inner(
        &self,
        mut body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
        _api_endpoint: &str,
        timeout_secs: u64,
    ) -> sag_core::Result<serde_json::Value> {
        // role_mappings on input messages (best-effort, native shape).
        if self.base.has_role_mappings(ApiKind::Responses)
            && let Some(items) = body.get_mut("input").and_then(|v| v.as_array_mut())
        {
            for item in items.iter_mut() {
                if let Some(role) = item.get("role").and_then(|r| r.as_str()).map(String::from)
                    && let Some(mapped) = self.base.role_mapping_for(ApiKind::Responses, &role)
                {
                    item["role"] = serde_json::json!(mapped);
                }
            }
        }
        let url = self.base.protocol_url(ApiKind::Responses)?;
        let timeout = self
            .base
            .effective_timeout(ApiKind::Responses, timeout_secs);
        let body_text = self
            .base
            .send_json(&url, ApiKind::Responses, body, target_extra, Some(timeout))
            .await?;

        // 200-delivered error object (same convention as chat()).
        if let Ok(raw) = serde_json::from_str::<serde_json::Value>(&body_text)
            && crate::base::body_has_error_object(&raw)
        {
            let err_obj = raw.get("error").expect("checked error object");
            let msg = crate::base::body_error_message(err_obj);
            let status = if crate::base::body_error_is_rate_limited(err_obj) {
                429
            } else {
                502
            };
            warn!(status = 200, %body_text, "upstream returned error");
            return Err(AppError::Upstream {
                provider: self.base.name.clone(),
                status,
                body: msg,
            });
        }

        serde_json::from_str(&body_text)
            .map_err(|e| AppError::Internal(format!("failed to parse responses body: {e}")))
    }

    /// Native Responses streaming: Responses body in, raw Responses SSE
    /// frames as `StreamChunk::Data` out; usage side-channeled from
    /// `response.completed`.
    pub async fn responses_stream_native_inner(
        &self,
        mut body: serde_json::Value,
        target_extra: &HashMap<String, serde_json::Value>,
        _api_endpoint: &str,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        body["stream"] = serde_json::json!(true);
        let url = self.base.protocol_url(ApiKind::Responses)?;
        let response = self
            .base
            .send_json_stream(&url, ApiKind::Responses, body, target_extra)
            .await?;

        let (tx, rx) = mpsc::unbounded_channel::<StreamChunk>();
        let chunk_timeout = self
            .base
            .chunk_timeout_secs(ApiKind::Responses)
            .map(std::time::Duration::from_secs);

        // The reader outlives this call: attach the provider/model span so
        // its chunk-read errors stay correlatable with the request.
        spawn_in_current_span(async move {
            use crate::util::find_sse_boundary;
            let mut byte_stream = response.bytes_stream();
            let mut buffer: Vec<u8> = Vec::new();
            let mut had_error = false;

            // Returns true when the frame carried an in-stream error.
            let extract_side_channel = |frame: &[u8]| -> bool {
                let mut saw = false;
                let text = String::from_utf8_lossy(frame);
                for line in text.lines() {
                    let Some(rest) = line.trim().strip_prefix("data:") else {
                        continue;
                    };
                    let Ok(val) = serde_json::from_str::<serde_json::Value>(rest.trim_start())
                    else {
                        continue;
                    };
                    match val.get("type").and_then(|t| t.as_str()) {
                        Some("response.completed") | Some("response.incomplete") => {
                            if let Some(u) = val.pointer("/response/usage").filter(|u| !u.is_null())
                            {
                                emit_responses_usage(u, &tx);
                            }
                        }
                        Some("response.failed") => {
                            saw = true;
                            let msg = val
                                .pointer("/response/error/message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("upstream response failed")
                                .to_string();
                            warn!(status = 200, %msg, "upstream returned error");
                            if !tx.is_closed() {
                                let _ = tx.send(StreamChunk::Error(msg));
                            }
                        }
                        _ => {}
                    }
                    // Top-level error object inside a data frame.
                    if let Some(err) = val.get("error").filter(|e| !e.is_null()) {
                        saw = true;
                        let msg = crate::base::body_error_message(err);
                        warn!(status = 200, %msg, "upstream returned error");
                        if !tx.is_closed() {
                            let _ = tx.send(StreamChunk::Error(msg));
                        }
                    }
                }
                saw
            };

            loop {
                if tx.is_closed() {
                    break;
                }
                let chunk_result: Result<_, tokio::time::error::Elapsed> = match &chunk_timeout {
                    Some(dur) => tokio::time::timeout(*dur, byte_stream.next()).await,
                    None => Ok(byte_stream.next().await),
                };
                match chunk_result {
                    Ok(Some(Ok(chunk))) => {
                        trace!(received_bytes = chunk.len(), "received responses SSE chunk");
                        buffer.extend_from_slice(&chunk);
                        while let Some(end) = find_sse_boundary(&buffer) {
                            let frame: Vec<u8> = buffer.drain(..end).collect();
                            let delim_len = if buffer.starts_with(b"\r\n\r\n") {
                                4
                            } else {
                                2
                            };
                            buffer.drain(..delim_len);
                            if extract_side_channel(&frame) {
                                had_error = true;
                            }
                            // Forward the raw Responses frame (native lane).
                            let data =
                                if frame.starts_with(b"data:") || frame.starts_with(b"event:") {
                                    let mut data = frame;
                                    if !data.ends_with(b"\n\n") {
                                        data.extend_from_slice(b"\n\n");
                                    }
                                    data
                                } else {
                                    let mut data = b"data: ".to_vec();
                                    data.extend_from_slice(&frame);
                                    data.extend_from_slice(b"\n\n");
                                    data
                                };
                            if !tx.is_closed() {
                                let _ = tx.send(StreamChunk::Data(data));
                            }
                            if had_error {
                                buffer.clear();
                                break;
                            }
                        }
                        if had_error {
                            break;
                        }
                    }
                    Ok(Some(Err(e))) => {
                        error!(?e, "error reading responses stream chunk");
                        if !tx.is_closed() {
                            let _ = tx.send(StreamChunk::Error(e.to_string()));
                        }
                        had_error = true;
                        break;
                    }
                    Ok(None) => break,
                    Err(_elapsed) => {
                        had_error = emit_idle_timeout(&tx, "openai-responses", &chunk_timeout);
                        break;
                    }
                }
            }

            if !buffer.is_empty() && !had_error {
                if extract_side_channel(&buffer) {
                    had_error = true;
                }
                let mut data = buffer.clone();
                if !data.ends_with(b"\n\n") {
                    data.extend_from_slice(b"\n\n");
                }
                if !tx.is_closed() {
                    let _ = tx.send(StreamChunk::Data(data));
                }
                buffer.clear();
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
}

/// Emit a Usage chunk from a Responses usage object (native stream path).
fn emit_responses_usage(usage: &serde_json::Value, tx: &mpsc::UnboundedSender<StreamChunk>) {
    let input = usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let output = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    if input == 0 && output == 0 {
        return;
    }
    let total = usage
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or_else(|| input.saturating_add(output));
    let cached = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .or((input > 0).then_some(0));
    let reasoning = usage
        .pointer("/output_tokens_details/reasoning_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let _ = tx.send(StreamChunk::Usage(sag_core::Usage {
        prompt_tokens: input,
        completion_tokens: output,
        total_tokens: total,
        cached_tokens: cached,
        cache_creation_tokens: None,
        reasoning_tokens: reasoning,
    }));
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

    // ── 200-delivered upstream error detection (non-streaming) ──

    /// A 200 JSON body carrying a top-level non-null `error` object must be
    /// surfaced as `AppError::Upstream` (status 502, the 200-delivered-error
    /// convention), not silently parsed as a success-shaped response.
    #[tokio::test]
    async fn chat_200_body_with_error_object_returns_upstream_error() {
        let body = r#"{"error":{"message":"model overloaded","code":500}}"#;
        let base_url = spawn_server(move |mut stream| {
            let body = body.to_string();
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
        let err = provider
            .chat(
                &chat_request(),
                "test-model",
                "/chat/completions",
                &HashMap::new(),
                30,
            )
            .await
            .unwrap_err();
        match err {
            AppError::Upstream { status, body, .. } => {
                assert_eq!(status, 502, "200-delivered error maps to 502");
                assert_eq!(body, "model overloaded", "message must be extracted");
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    /// A rate-limit-ish error object (numeric code 429) inside a 200 body
    /// maps to `Upstream` status 429 so the analytics failure reason becomes
    /// `rate_limited`.
    #[tokio::test]
    async fn chat_200_body_with_rate_limit_error_maps_to_429() {
        let body = r#"{"error":{"message":"burst tokens limit","code":429}}"#;
        let base_url = spawn_server(move |mut stream| {
            let body = body.to_string();
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
        let err = provider
            .chat(
                &chat_request(),
                "test-model",
                "/chat/completions",
                &HashMap::new(),
                30,
            )
            .await
            .unwrap_err();
        match err {
            AppError::Upstream { status, body, .. } => {
                assert_eq!(status, 429, "rate-limit-ish error maps to 429");
                assert_eq!(body, "burst tokens limit");
            }
            other => panic!("expected Upstream 429, got {other:?}"),
        }
    }

    /// An SSE data frame carrying a top-level `error` object (delivered with
    /// HTTP 200) is forwarded to the client and surfaced as an in-stream
    /// Error chunk; frames after it are NOT forwarded and no `Done` is
    /// emitted (the dispatcher records the attempt as a failure).
    #[tokio::test]
    async fn chat_stream_in_stream_error_frame_emits_error_and_stops() {
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
            stream
                .write_all(b"data: {\"error\":{\"message\":\"burst limit\"}}\n\n")
                .await
                .unwrap();
            stream.flush().await.unwrap();
            // A frame after the error must NOT be forwarded.
            stream
                .write_all(b"data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n\n")
                .await
                .unwrap();
            stream.flush().await.unwrap();
            drop(stream);
        })
        .await;

        let provider = openai_provider(&base_url, 30, None);
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

        let mut data: Vec<Vec<u8>> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        let mut saw_done = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                StreamChunk::Data(d) => data.push(d),
                StreamChunk::Error(e) => errors.push(e),
                StreamChunk::Done => saw_done = true,
                StreamChunk::Usage(_) => {}
            }
        }

        // The error frame itself is still forwarded (behavior-compatible);
        // nothing after it is, and no Done is emitted.
        assert_eq!(
            data.len(),
            2,
            "content frame + forwarded error frame only, got {}: {:?}",
            data.len(),
            data
        );
        assert_eq!(
            data[0],
            b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n".to_vec()
        );
        assert_eq!(
            data[1],
            b"data: {\"error\":{\"message\":\"burst limit\"}}\n\n".to_vec()
        );
        assert_eq!(errors.len(), 1, "exactly one in-stream error signal");
        assert_eq!(errors[0], "burst limit");
        assert!(!saw_done, "no Done after an in-stream upstream error");
    }
}
