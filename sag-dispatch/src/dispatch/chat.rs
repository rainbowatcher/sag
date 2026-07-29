//! Chat-completion dispatch with multi-provider fallback.

use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use sag_core::{AppError, ChatCompletionResponse, StreamChunk};

use crate::dispatch::resolve::resolve_provider;
use crate::dispatch::retry::RetryOutcome;
use crate::dispatch::retry::run_with_retries_outcome;
use crate::dispatch::stream::build_streaming_response;
use crate::dispatch::{
    DispatchCtx, apply_circuit_outcome, circuit_allows_target, fallback_exhausted_response,
    log_per_target_failure, merge_model_overrides, release_probe_slot, success_log,
};

/// Unified result of a single chat attempt. The `Instant` records when
/// the upstream provider call started so the success branch can compute
/// `upstream_duration_ms` without including SAG-internal retry/circuit
/// overhead.
enum ChatAttempt {
    Stream(
        futures_util::stream::BoxStream<'static, StreamChunk>,
        std::time::Instant,
    ),
    NonStream(ChatCompletionResponse, std::time::Instant),
}

// ── empty-response detection ─────────────────────────────────────

/// Quick byte search for `b"usage"` in a raw SSE data frame. Used as a
/// cheap pre-filter to avoid JSON parsing on content-only frames.
fn raw_contains_usage(raw: &[u8]) -> bool {
    raw.windows(5).any(|w| w == b"usage")
}

/// Check whether a raw `StreamChunk::Data` payload is an "empty response"
/// frame: a JSON object whose `usage` reports `prompt_tokens` > 0, and
/// whose `choices` carry no actual content (no non-empty `content` /
/// `reasoning_content` / `refusal` / `tool_calls`).
///
/// Unlike the old heuristic, this no longer short-circuits when
/// `completion_tokens > 0`. Even if a provider reports nonzero ct, a frame
/// with empty `choices` is treated as empty — because some providers may
/// report usage inaccurately. The rule is: **choices with real content are
/// the only signal that a response is non-empty.** If `prompt_tokens > 0`
/// but `choices` are all empty, the frame is considered empty regardless
/// of `completion_tokens`.
///
/// `finish_reason` is NOT considered content. A frame that carries
/// `finish_reason: "stop"` but an empty delta is still an empty response --
/// the model produced zero output. This is the key difference from
/// `has_meaningful_choice` in `process_sse_frame`, which counts
/// `finish_reason` as meaningful to avoid dropping the stream-termination
/// signal. Here we *want* to catch that case so the dispatch layer can
/// retry.
fn is_empty_response_data(raw: &[u8]) -> bool {
    if !raw_contains_usage(raw) {
        return false;
    }

    let text = std::str::from_utf8(raw).unwrap_or("");
    let Some(rest) = text
        .strip_prefix("data:")
        .or_else(|| text.strip_prefix("data: "))
    else {
        return false;
    };
    let json_str = rest.trim();

    let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return false;
    };

    let Some(usage) = val.get("usage") else {
        return false;
    };

    let pt = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    if pt == 0 {
        return false;
    }

    // Check if any choice carries real content.  finish_reason alone does
    // NOT count -- a "stop" with an empty delta is still an empty response.
    let has_content = val
        .get("choices")
        .and_then(|c| c.as_array())
        .is_some_and(|arr| {
            arr.iter().any(|choice| {
                choice.get("delta").is_some_and(|d| {
                    d.get("content")
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| !s.is_empty())
                        || d.get("reasoning_content")
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| !s.is_empty())
                        || d.get("refusal")
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| !s.is_empty())
                        || d.get("tool_calls")
                            .and_then(|t| t.as_array())
                            .is_some_and(|a| !a.is_empty())
                })
            })
        });

    !has_content
}

/// Check whether a `ChatMessage` carries actual content, including
/// tool_calls in the `extra` map.
fn choice_has_message_content(msg: &sag_core::types::ChatMessage) -> bool {
    // Check content field
    match &msg.content {
        serde_json::Value::String(s) if !s.is_empty() => return true,
        serde_json::Value::Array(arr) if !arr.is_empty() => return true,
        _ => {}
    }
    // Check for tool_calls in the extra map
    if let Some(tc) = msg.extra.get("tool_calls")
        && tc.as_array().is_some_and(|a| !a.is_empty())
    {
        return true;
    }
    false
}

/// Check whether a non-streaming response is "empty" — i.e. has non-zero
/// `prompt_tokens` but no actual content in any `choice`. Choices with
/// real content (string/array message content, or tool_calls) are
/// considered non-empty even if `completion_tokens` is 0, because some
/// providers report ct inaccurately.
fn is_empty_non_stream_response(r: &ChatCompletionResponse) -> bool {
    let Some(u) = r.usage.as_ref() else {
        return false;
    };
    if u.prompt_tokens == 0 {
        return false;
    }
    let has_content = r
        .choices
        .iter()
        .any(|c| choice_has_message_content(&c.message));
    if has_content {
        return false;
    }
    true
}

/// Peek at the first chunk of a streaming response.  If the upstream
/// returned an empty response (zero `completion_tokens`, no content),
/// return [`AppError::EmptyResponse`] so the retry loop retries the same
/// target with backoff, falling through to the next target only when the
/// retry budget is exhausted.  Otherwise, prepend the consumed chunk and
/// return the stream for normal forwarding.
///
/// This must be called **inside** the retry closure -- before
/// `ChatAttempt::Stream` is constructed and `build_streaming_response`
/// commits the HTTP 200 headers.  Once the response is committed, retry
/// is no longer possible.
async fn validate_stream_first_chunk(
    mut stream: futures_util::stream::BoxStream<'static, StreamChunk>,
    provider: &str,
) -> Result<futures_util::stream::BoxStream<'static, StreamChunk>, AppError> {
    let first = stream.next().await;
    match first {
        // Empty stream or immediate Done -> no output at all.
        None | Some(StreamChunk::Done) => Err(AppError::EmptyResponse {
            provider: provider.into(),
        }),

        // Mid-stream error from the provider -> propagate as retryable.
        Some(StreamChunk::Error(msg)) => Err(AppError::Upstream {
            provider: provider.into(),
            status: 502,
            body: msg,
        }),

        // Pure Usage chunk (Data was intercepted by process_sse_frame).
        // If ct == 0 and pt > 0, the response is empty.
        Some(StreamChunk::Usage(u)) => {
            // NOTE: A `Usage` chunk carries only token counts, not `choices`, so we
            // cannot apply the same choices-based emptiness check as `is_empty_response_data`.
            // We fall back to the conservative ct==0 && pt>0 signal here. A Data frame
            // carrying usage + choices is checked more precisely in the Data branch below.
            if u.completion_tokens == 0 && u.prompt_tokens > 0 {
                Err(AppError::EmptyResponse {
                    provider: provider.into(),
                })
            } else {
                Ok(futures_util::stream::iter(vec![StreamChunk::Usage(u)])
                    .chain(stream)
                    .boxed())
            }
        }

        // Data chunk -- check for the [DONE] marker or an empty usage frame.
        Some(StreamChunk::Data(raw)) => {
            if raw.starts_with(b"data: [DONE]") || is_empty_response_data(&raw) {
                Err(AppError::EmptyResponse {
                    provider: provider.into(),
                })
            } else {
                Ok(futures_util::stream::iter(vec![StreamChunk::Data(raw)])
                    .chain(stream)
                    .boxed())
            }
        }
    }
}

/// Try each target in order. On a retryable error, retry the same target with
/// exponential backoff. On a non-retryable 4xx error (client-side request
/// problem), do **not** retry the same target and do **not** open the circuit
/// breaker — instead, failover to the next target in the list.
pub async fn dispatch_chat(ctx: DispatchCtx<'_>) -> Response<Body> {
    let DispatchCtx {
        state,
        runtime,
        request,
        targets,
        api_endpoint,
        is_stream,
        trace_id,
        start,
        errors,
        strategy,
        user_agent,
        client_ip,
    } = ctx;

    // Snapshot the backoff config once so the per-target retry loop doesn't
    // hold the config read lock and so all targets within one request share
    // a consistent view (a hot-reload mid-dispatch shouldn't change the
    // schedule for an in-flight request).
    let config = state.config.read().await.clone();
    let backoff = config.backoff.clone();
    let retry_on_empty = backoff.retry_on_empty_response;
    let circuit_breaker_cfg = config.circuit_breaker.clone();
    let api_endpoint = api_endpoint.to_string();

    for target in targets {
        let provider_name = &target.provider;
        let real_model = &target.real_model;

        let Some(verdict) =
            circuit_allows_target(runtime, &request.model, provider_name, real_model).await
        else {
            errors.push(format!("{provider_name}/{real_model}: circuit open"));
            continue;
        };
        let circuit_key = verdict.key;
        let is_probe = verdict.is_probe;

        let Some(provider) = resolve_provider(state, provider_name, errors).await else {
            // Provider is disabled / unregistered. If this request claimed the
            // half-open probe slot we must release it, otherwise the target
            // would be skipped forever (probe_in_flight stuck true).
            if is_probe {
                release_probe_slot(
                    runtime,
                    circuit_key.clone(),
                    circuit_breaker_cfg.probe_interval_secs,
                )
                .await;
            }
            continue;
        };

        // Clone per-target values once so the retry closure can cheaply clone
        // them for each attempt.
        let real_model = real_model.clone();
        let target_extra =
            merge_model_overrides(&config.providers, provider_name, &real_model, &target.extra);
        let retries = target.retries;
        let timeout_secs = target.timeout_secs;

        let outcome = run_with_retries_outcome::<ChatAttempt, _, _>(
            provider_name,
            retries,
            &backoff,
            errors,
            |_attempt| {
                let provider = provider.clone();
                let real_model = real_model.clone();
                let target_extra = target_extra.clone();
                let api_endpoint = api_endpoint.clone();
                let provider_name = provider_name.to_string();
                async move {
                    // Capture the upstream start time immediately before the
                    // provider call so retry/circuit overhead is excluded.
                    let upstream_start = std::time::Instant::now();
                    if is_stream {
                        let s = provider
                            .chat_stream(
                                request,
                                &real_model,
                                &api_endpoint,
                                &target_extra,
                                timeout_secs,
                            )
                            .await?;
                        if retry_on_empty {
                            // Peek at the first chunk before committing the HTTP
                            // 200 response.  If the upstream returned an empty
                            // response (ct=0, no content), surface an
                            // `EmptyResponse` error so the retry loop retries the
                            // same target with backoff.
                            let s =
                                validate_stream_first_chunk(Box::pin(s), &provider_name).await?;
                            Ok(ChatAttempt::Stream(s, upstream_start))
                        } else {
                            Ok(ChatAttempt::Stream(Box::pin(s), upstream_start))
                        }
                    } else {
                        let r = provider
                            .chat(
                                request,
                                &real_model,
                                &api_endpoint,
                                &target_extra,
                                timeout_secs,
                            )
                            .await?;
                        if retry_on_empty && is_empty_non_stream_response(&r) {
                            return Err(AppError::EmptyResponse {
                                provider: provider_name.clone(),
                            });
                        }
                        Ok(ChatAttempt::NonStream(r, upstream_start))
                    }
                }
            },
        )
        .await;

        // Extract the error_category from the outcome before
        // `apply_circuit_outcome` consumes it, so per-target
        // failure logs carry the real failure type instead of a
        // generic "provider_error".
        let category = match &outcome {
            RetryOutcome::Exhausted(e) => e.error_category(),
            RetryOutcome::NonRetryable => "bad_request",
            RetryOutcome::Success(_) => "", // never logged — Success never reaches the else block
        };

        let Some(attempt) = apply_circuit_outcome(
            runtime,
            circuit_key,
            outcome,
            &circuit_breaker_cfg,
            is_probe,
        )
        .await
        else {
            // Per-provider failure: log a failure entry with the actual provider/real_model
            // so the analytics table knows which provider was attempted.
            log_per_target_failure(
                state,
                trace_id,
                provider_name,
                &request.model,
                &real_model,
                is_stream,
                start,
                errors,
                category,
                request.reasoning_effort.as_deref(),
                user_agent.as_deref(),
                client_ip.as_deref(),
            );
            continue;
        };

        if strategy == sag_config::Strategy::StickyFailover {
            let mut active = runtime.active_targets.write().await;
            active.insert(
                request.model.clone(),
                (
                    provider_name.clone(),
                    real_model.clone(),
                    std::time::Instant::now(),
                ),
            );
        }

        match attempt {
            ChatAttempt::Stream(stream, upstream_start) => {
                return build_streaming_response(
                    state,
                    stream,
                    provider_name,
                    &request.model,
                    &real_model,
                    trace_id,
                    start,
                    upstream_start,
                    request.reasoning_effort.clone(),
                    user_agent.clone(),
                    client_ip.clone(),
                );
            }
            ChatAttempt::NonStream(resp, upstream_start) => {
                // Non-streaming responses are single-shot: TTFT equals the
                // total upstream duration.
                let upstream_duration_ms = upstream_start.elapsed().as_millis() as u64;
                let timing = Some(upstream_duration_ms);
                state.collector.log(success_log(
                    trace_id,
                    provider_name,
                    &request.model,
                    &real_model,
                    false,
                    start,
                    resp.usage.as_ref(),
                    request.reasoning_effort.as_deref(),
                    timing,
                    timing,
                    user_agent.clone(),
                    client_ip.clone(),
                ));
                return (StatusCode::OK, Json(resp)).into_response();
            }
        }
    }

    fallback_exhausted_response(
        state,
        &request.model,
        is_stream,
        trace_id,
        start,
        errors,
        request.reasoning_effort.as_deref(),
        user_agent.as_deref(),
        client_ip.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::ResolvedTarget;
    use crate::state::{AppState, DispatchRuntime};
    use crate::testing::target;
    use async_trait::async_trait;
    use axum::body::to_bytes;
    use futures_util::Stream;
    use sag_analytics::StatsFilter;
    use sag_config::{
        CircuitBreakerConfig, ConfigHandle, GatewayAuthConfig, GatewayConfig, Strategy,
    };
    use sag_core::AppError;
    use sag_core::types::{ChatCompletionRequest, Usage};
    use sag_providers::LlmProvider;
    use serde_json::json;
    use std::collections::{HashMap, VecDeque};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;
    use tokio::sync::RwLock;
    use uuid::Uuid;

    // ─── Mock provider with configurable behavior queue ─────────────

    #[allow(clippy::large_enum_variant)]
    enum ChatBehavior {
        Ok(ChatCompletionResponse),
        Retryable(AppError),
        NonRetryable(AppError),
    }

    enum StreamBehavior {
        Ok(Vec<StreamChunk>),
    }

    struct MockProvider {
        name: String,
        chat_queue: Mutex<VecDeque<ChatBehavior>>,
        stream_queue: Mutex<VecDeque<StreamBehavior>>,
        chat_calls: AtomicU32,
        stream_calls: AtomicU32,
    }

    impl MockProvider {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                chat_queue: Mutex::new(VecDeque::new()),
                stream_queue: Mutex::new(VecDeque::new()),
                chat_calls: AtomicU32::new(0),
                stream_calls: AtomicU32::new(0),
            }
        }
        fn push_chat(&self, b: ChatBehavior) {
            self.chat_queue.lock().unwrap().push_back(b);
        }
        fn push_stream(&self, b: StreamBehavior) {
            self.stream_queue.lock().unwrap().push_back(b);
        }
        fn chat_calls(&self) -> u32 {
            self.chat_calls.load(Ordering::SeqCst)
        }
        fn stream_calls(&self) -> u32 {
            self.stream_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        fn name(&self) -> &str {
            &self.name
        }

        async fn chat(
            &self,
            _request: &ChatCompletionRequest,
            _real_model: &str,
            _api_endpoint: &str,
            _target_extra: &HashMap<String, serde_json::Value>,
            _timeout_secs: u64,
        ) -> sag_core::Result<ChatCompletionResponse> {
            self.chat_calls.fetch_add(1, Ordering::SeqCst);
            match self.chat_queue.lock().unwrap().pop_front() {
                Some(ChatBehavior::Ok(r)) => Ok(r),
                Some(ChatBehavior::Retryable(e)) | Some(ChatBehavior::NonRetryable(e)) => Err(e),
                None => Ok(ChatCompletionResponse {
                    id: "default".into(),
                    object: "chat.completion".into(),
                    created: 0,
                    model: "default".into(),
                    choices: vec![],
                    usage: None,
                }),
            }
        }

        async fn chat_stream(
            &self,
            _request: &ChatCompletionRequest,
            _real_model: &str,
            _api_endpoint: &str,
            _target_extra: &HashMap<String, serde_json::Value>,
            _timeout_secs: u64,
        ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
            self.stream_calls.fetch_add(1, Ordering::SeqCst);
            match self.stream_queue.lock().unwrap().pop_front() {
                Some(StreamBehavior::Ok(chunks)) => {
                    Ok(Box::pin(futures_util::stream::iter(chunks)))
                }
                None => Ok(Box::pin(futures_util::stream::empty())),
            }
        }
    }

    // ─── Helpers ──────────────────────────────────────────────────

    fn build_request(model: &str, stream: bool) -> ChatCompletionRequest {
        // Build via JSON to avoid coupling tests to every field in the struct.
        serde_json::from_value(json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": stream,
        }))
        .expect("valid test request")
    }

    fn build_response(model: &str, usage: Option<Usage>) -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: "resp-1".into(),
            object: "chat.completion".into(),
            created: 0,
            model: model.into(),
            choices: vec![],
            usage,
        }
    }

    /// Like `build_response` but with one `ChatChoice` containing the
    /// given text as message content.
    fn build_response_with_content(
        model: &str,
        usage: Option<Usage>,
        text: &str,
    ) -> ChatCompletionResponse {
        ChatCompletionResponse {
            choices: vec![sag_core::types::ChatChoice {
                index: 0,
                message: sag_core::types::ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::String(text.into()),
                    name: None,
                    extra: serde_json::Map::new(),
                },
                finish_reason: Some("stop".into()),
            }],
            ..build_response(model, usage)
        }
    }

    async fn build_state(providers: Vec<Arc<dyn LlmProvider>>) -> AppState {
        let mut registry: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
        for p in providers {
            registry.insert(p.name().to_string(), p);
        }
        let config = GatewayConfig {
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["k".into()],
            }),
            ..sag_config::gateway_config_base!()
        };
        let handle = ConfigHandle::from_config(config);
        crate::testing::test_app_state(handle, Arc::new(RwLock::new(registry))).await
    }

    /// Like [`build_state`] but with a custom circuit-breaker config. Used to
    /// drive cooldowns to ~0 so half-open recovery can be exercised quickly.
    async fn build_state_with_cb(
        providers: Vec<Arc<dyn LlmProvider>>,
        circuit_breaker: CircuitBreakerConfig,
    ) -> AppState {
        let mut registry: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
        for p in providers {
            registry.insert(p.name().to_string(), p);
        }
        let config = GatewayConfig {
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["k".into()],
            }),
            circuit_breaker,
            ..sag_config::gateway_config_base!()
        };
        let handle = ConfigHandle::from_config(config);
        crate::testing::test_app_state(handle, Arc::new(RwLock::new(registry))).await
    }

    fn make_ctx<'a>(
        state: &'a AppState,
        runtime: &'a DispatchRuntime,
        request: &'a ChatCompletionRequest,
        targets: &'a [ResolvedTarget],
        is_stream: bool,
        errors: &'a mut Vec<String>,
    ) -> DispatchCtx<'a> {
        make_ctx_with_strategy(
            state,
            runtime,
            request,
            targets,
            is_stream,
            sag_config::Strategy::StickyFailover,
            errors,
        )
    }

    fn make_ctx_with_strategy<'a>(
        state: &'a AppState,
        runtime: &'a DispatchRuntime,
        request: &'a ChatCompletionRequest,
        targets: &'a [ResolvedTarget],
        is_stream: bool,
        strategy: sag_config::Strategy,
        errors: &'a mut Vec<String>,
    ) -> DispatchCtx<'a> {
        DispatchCtx {
            state,
            runtime,
            request,
            targets,
            api_endpoint: "/chat/completions",
            is_stream,
            trace_id: Uuid::new_v4(),
            start: Instant::now(),
            errors,
            strategy,
            user_agent: None,
            client_ip: None,
        }
    }

    // ─── Tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn dispatch_chat_non_streaming_success() {
        let mock = Arc::new(MockProvider::new("openai"));
        mock.push_chat(ChatBehavior::Ok(build_response_with_content(
            "gpt-4o",
            Some(Usage {
                prompt_tokens: 5,
                completion_tokens: 7,
                total_tokens: 12,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            "hello",
        )));
        let state = build_state(vec![mock.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);
        let targets = vec![target("openai", "gpt-4o", 0)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
        let resp = dispatch_chat(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(mock.chat_calls(), 1);
        assert!(errors.is_empty());

        // Verify CallLog was recorded with usage
        state.collector.flush().await;
        let rows = state
            .collector
            .store()
            .query_stats(StatsFilter::default())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, "openai");
        assert_eq!(rows[0].call_count, 1);
        assert_eq!(rows[0].error_count, 0);
        assert_eq!(rows[0].total_prompt_tokens, 5);
        assert_eq!(rows[0].total_completion_tokens, 7);
        assert_eq!(rows[0].total_tokens, 12);
    }

    #[tokio::test]
    async fn dispatch_chat_streaming_success() {
        let mock = Arc::new(MockProvider::new("openai"));
        mock.push_stream(StreamBehavior::Ok(vec![
            StreamChunk::Data(b"data: hello\n\n".to_vec()),
            StreamChunk::Usage(Usage {
                prompt_tokens: 1,
                completion_tokens: 2,
                total_tokens: 3,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            StreamChunk::Done,
        ]));
        let state = build_state(vec![mock.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", true);
        let targets = vec![target("openai", "gpt-4o", 0)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
        let resp = dispatch_chat(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(mock.stream_calls(), 1);
        assert!(errors.is_empty());

        // Read the body to confirm data was forwarded
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"data: hello\n\n");
    }

    #[tokio::test]
    async fn dispatch_chat_retries_then_succeeds() {
        let mock = Arc::new(MockProvider::new("openai"));
        mock.push_chat(ChatBehavior::Retryable(AppError::Internal("oops".into())));
        mock.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        let state = build_state(vec![mock.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);
        let targets = vec![target("openai", "gpt-4o", 1)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
        let resp = dispatch_chat(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(mock.chat_calls(), 2);
        assert!(errors.is_empty());
    }

    #[tokio::test]
    async fn dispatch_chat_non_retryable_does_not_retry() {
        let mock = Arc::new(MockProvider::new("openai"));
        mock.push_chat(ChatBehavior::NonRetryable(AppError::BadRequest(
            "bad".into(),
        )));
        let state = build_state(vec![mock.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);
        let targets = vec![target("openai", "gpt-4o", 5)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
        let _ = dispatch_chat(ctx).await;
        assert_eq!(
            mock.chat_calls(),
            1,
            "non-retryable should not retry the same provider"
        );
        assert_eq!(errors.len(), 1);
    }

    #[tokio::test]
    async fn dispatch_chat_falls_through_to_next_target() {
        let m1 = Arc::new(MockProvider::new("first"));
        let m2 = Arc::new(MockProvider::new("second"));
        m1.push_chat(ChatBehavior::Retryable(AppError::Internal("fail".into())));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        let state = build_state(vec![m1.clone(), m2.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);
        let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
        let resp = dispatch_chat(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(m1.chat_calls(), 1);
        assert_eq!(m2.chat_calls(), 1);
    }

    #[tokio::test]
    async fn dispatch_chat_all_targets_exhausted() {
        let m1 = Arc::new(MockProvider::new("first"));
        let m2 = Arc::new(MockProvider::new("second"));
        m1.push_chat(ChatBehavior::Retryable(AppError::Internal("fail1".into())));
        m2.push_chat(ChatBehavior::Retryable(AppError::Internal("fail2".into())));
        let state = build_state(vec![m1.clone(), m2.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);
        let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
        let resp = dispatch_chat(ctx).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(errors.len(), 2);
    }

    #[tokio::test]
    async fn dispatch_chat_remembers_exhausted_target_on_next_request() {
        let m1 = Arc::new(MockProvider::new("first"));
        let m2 = Arc::new(MockProvider::new("second"));
        m1.push_chat(ChatBehavior::Retryable(AppError::Internal("fail".into())));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        let state = build_state(vec![m1.clone(), m2.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);
        let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

        let mut first_errors = Vec::new();
        let first = dispatch_chat(make_ctx(
            &state,
            &rt,
            &req,
            &targets,
            false,
            &mut first_errors,
        ))
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(m1.chat_calls(), 1);
        assert_eq!(m2.chat_calls(), 1);

        let mut second_errors = Vec::new();
        let second = dispatch_chat(make_ctx(
            &state,
            &rt,
            &req,
            &targets,
            false,
            &mut second_errors,
        ))
        .await;
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(m1.chat_calls(), 1, "open circuit should skip first target");
        assert_eq!(m2.chat_calls(), 2);
        assert!(
            second_errors.iter().any(|e| e.contains("circuit open")),
            "second request should record why the first target was skipped"
        );
    }

    #[tokio::test]
    async fn dispatch_chat_half_open_probe_recovers_target() {
        // First request: `first` fails (retryable) → circuit opens. With a
        // 0-second generic cooldown the circuit is already half-open by the
        // next request, which probes `first` and succeeds, recovering it.
        let m1 = Arc::new(MockProvider::new("first"));
        let m2 = Arc::new(MockProvider::new("second"));
        m1.push_chat(ChatBehavior::Retryable(AppError::Internal("fail".into())));
        m1.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        let cb = CircuitBreakerConfig {
            quota_exhausted_secs: 0,
            rate_limited_secs: 0,
            generic_secs: 0,
            probe_interval_secs: 0,
        };
        let state = build_state_with_cb(vec![m1.clone(), m2.clone()], cb).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);
        let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

        let mut first_errors = Vec::new();
        let first = dispatch_chat(make_ctx(
            &state,
            &rt,
            &req,
            &targets,
            false,
            &mut first_errors,
        ))
        .await;
        // First request: first fails → fall through to second (success).
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(m1.chat_calls(), 1);
        assert_eq!(m2.chat_calls(), 1);

        let mut second_errors = Vec::new();
        let second = dispatch_chat(make_ctx(
            &state,
            &rt,
            &req,
            &targets,
            false,
            &mut second_errors,
        ))
        .await;
        // Second request: circuit is half-open, probe `first` succeeds →
        // target recovered, no longer skipped.
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            m1.chat_calls(),
            2,
            "half-open probe should hit first target"
        );
        assert_eq!(m2.chat_calls(), 1, "recovered target should absorb traffic");
        assert!(second_errors.is_empty());
    }

    #[tokio::test]
    async fn dispatch_chat_probes_during_cooldown_not_after() {
        // The whole point of periodic probing: a probe is allowed *while the
        // target is still cooling down* (long `generic_secs` cooldown, short
        // `probe_interval_secs`), so a target that recovers mid-cooldown is
        // discovered early instead of staying skipped until the full window
        // elapses.
        let m1 = Arc::new(MockProvider::new("first"));
        let m2 = Arc::new(MockProvider::new("second"));
        // First request: `first` fails (retryable) → circuit opens with a long
        // cooldown. Immediately after, `first` has actually recovered.
        m1.push_chat(ChatBehavior::Retryable(AppError::Internal("fail".into())));
        m1.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        let cb = CircuitBreakerConfig {
            quota_exhausted_secs: 3600,
            rate_limited_secs: 10,
            generic_secs: 3600,     // long cooldown — would stay skipped for 1h
            probe_interval_secs: 0, // but a probe is due immediately
        };
        let state = build_state_with_cb(vec![m1.clone(), m2.clone()], cb).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);
        let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

        let mut first_errors = Vec::new();
        let first = dispatch_chat(make_ctx(
            &state,
            &rt,
            &req,
            &targets,
            false,
            &mut first_errors,
        ))
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(m1.chat_calls(), 1);
        assert_eq!(m2.chat_calls(), 1);

        // Second request arrives while the 1h cooldown is still in effect — but
        // a periodic probe is due, hits the recovered `first`, and restores it.
        let mut second_errors = Vec::new();
        let second = dispatch_chat(make_ctx(
            &state,
            &rt,
            &req,
            &targets,
            false,
            &mut second_errors,
        ))
        .await;
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            m1.chat_calls(),
            2,
            "periodic probe during cooldown should hit recovered first target"
        );
        assert_eq!(m2.chat_calls(), 1, "recovered target absorbs traffic");
        assert!(second_errors.is_empty());
    }

    #[tokio::test]
    async fn dispatch_chat_half_open_probe_failure_reschedules() {
        // `first` keeps failing. Because probes are allowed every interval, each
        // request probes `first` once more; a failed probe reschedules the next
        // probe (it does NOT re-open the whole cooldown), so further requests
        // keep falling through to `second` until `first` recovers.
        let m1 = Arc::new(MockProvider::new("first"));
        let m2 = Arc::new(MockProvider::new("second"));
        m1.push_chat(ChatBehavior::Retryable(AppError::Internal("fail1".into())));
        m1.push_chat(ChatBehavior::Retryable(AppError::Internal("fail2".into())));
        m1.push_chat(ChatBehavior::Retryable(AppError::Internal("fail3".into())));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        let cb = CircuitBreakerConfig {
            quota_exhausted_secs: 0,
            rate_limited_secs: 0,
            generic_secs: 0,
            probe_interval_secs: 0,
        };
        let state = build_state_with_cb(vec![m1.clone(), m2.clone()], cb).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);
        let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

        let mut first_errors = Vec::new();
        let _first = dispatch_chat(make_ctx(
            &state,
            &rt,
            &req,
            &targets,
            false,
            &mut first_errors,
        ))
        .await;
        assert_eq!(m1.chat_calls(), 1);
        assert_eq!(m2.chat_calls(), 1);

        let mut probe_errors = Vec::new();
        let _probe = dispatch_chat(make_ctx(
            &state,
            &rt,
            &req,
            &targets,
            false,
            &mut probe_errors,
        ))
        .await;
        // Probe hits first again (failure) → re-opens; second still serves.
        assert_eq!(m1.chat_calls(), 2, "half-open probe should retry first");
        assert_eq!(m2.chat_calls(), 2);

        let mut third_errors = Vec::new();
        let third = dispatch_chat(make_ctx(
            &state,
            &rt,
            &req,
            &targets,
            false,
            &mut third_errors,
        ))
        .await;
        // Cooldown (0s) elapsed again → another probe; first still failing →
        // falls through again.
        assert_eq!(third.status(), StatusCode::OK);
        assert_eq!(m1.chat_calls(), 3);
        assert_eq!(m2.chat_calls(), 3);
    }

    #[tokio::test]
    async fn dispatch_chat_releases_probe_slot_when_provider_unresolved() {
        // Regression: when `circuit_allows_target` claims the half-open probe
        // slot but `resolve_provider` returns `None` (provider disabled or no
        // longer registered, e.g. after a config hot-reload), the slot must be
        // released. Otherwise `probe_in_flight` stays true and the target's
        // half-open recovery is defeated for the rest of the cooldown window.
        let m2 = Arc::new(MockProvider::new("second"));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        // Only `second` is registered; `first` is referenced by the route but
        // gone from the registry, so `resolve_provider("first")` returns None.
        let cb = CircuitBreakerConfig {
            quota_exhausted_secs: 0,
            rate_limited_secs: 0,
            generic_secs: 0,
            probe_interval_secs: 0,
        };
        let state = build_state_with_cb(vec![m2.clone()], cb).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);
        let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

        // Pre-open `first`'s circuit so the next request finds it probe-due
        // and claims the slot before `resolve_provider` fails.
        rt.route_circuit_breaker.write().await.open(
            crate::circuit_breaker::RouteTargetKey::new("gpt-4o", "first", "gpt-4o"),
            0,
            0,
        );

        let mut errors = Vec::new();
        let resp = dispatch_chat(make_ctx(&state, &rt, &req, &targets, false, &mut errors)).await;
        assert_eq!(resp.status(), StatusCode::OK, "falls through to second");
        assert_eq!(m2.chat_calls(), 1);

        // The slot must have been released: the target is probeable again and
        // the single probe slot can be claimed once more.
        let mut b = rt.route_circuit_breaker.write().await;
        assert!(
            b.to_half_open_if_due(&crate::circuit_breaker::RouteTargetKey::new(
                "gpt-4o", "first", "gpt-4o"
            )),
            "after release, target should be probeable again (not stuck half-open)"
        );
        assert!(
            b.try_acquire_probe(&crate::circuit_breaker::RouteTargetKey::new(
                "gpt-4o", "first", "gpt-4o"
            )),
            "released probe slot should be claimable again"
        );
    }

    #[tokio::test]
    async fn dispatch_chat_half_open_probe_in_flight_skips_others() {
        // When a target is half-open with a probe already in flight, a
        // concurrent request must skip it and fall through to the next target
        // rather than launching a second probe.
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct SlowProbe {
            name: String,
            calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl LlmProvider for SlowProbe {
            fn name(&self) -> &str {
                &self.name
            }
            async fn chat(
                &self,
                _r: &ChatCompletionRequest,
                _m: &str,
                _e: &str,
                _t: &HashMap<String, serde_json::Value>,
                _to: u64,
            ) -> sag_core::Result<ChatCompletionResponse> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                // Simulate a slow upstream so the probe is "in flight" while a
                // second request arrives.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                Ok(build_response("gpt-4o", None))
            }
            async fn chat_stream(
                &self,
                _r: &ChatCompletionRequest,
                _m: &str,
                _e: &str,
                _t: &HashMap<String, serde_json::Value>,
                _to: u64,
            ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
                Ok(Box::pin(futures_util::stream::empty()))
            }
        }

        let m1 = Arc::new(SlowProbe {
            name: "first".into(),
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let m2 = Arc::new(MockProvider::new("second"));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        let cb = CircuitBreakerConfig {
            quota_exhausted_secs: 0,
            rate_limited_secs: 0,
            generic_secs: 0,
            probe_interval_secs: 0,
        };
        let state = build_state_with_cb(vec![m1.clone(), m2.clone()], cb).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);
        let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

        // Pre-open the circuit for `first` so the next concurrent requests
        // find it half-open and race for the single probe slot.
        rt.route_circuit_breaker.write().await.open(
            crate::circuit_breaker::RouteTargetKey::new("gpt-4o", "first", "gpt-4o"),
            0,
            0,
        );

        let mut e1 = Vec::new();
        let mut e2 = Vec::new();
        let r1 = dispatch_chat(make_ctx(&state, &rt, &req, &targets, false, &mut e1));
        let r2 = dispatch_chat(make_ctx(&state, &rt, &req, &targets, false, &mut e2));
        let (a, b) = tokio::join!(r1, r2);
        assert_eq!(a.status(), StatusCode::OK);
        assert_eq!(b.status(), StatusCode::OK);
        // Exactly one probe should have hit `first`; the other must have been
        // skipped (probe in flight) and served by `second`.
        assert_eq!(
            m1.calls.load(Ordering::SeqCst),
            1,
            "only one half-open probe should hit first"
        );
        assert_eq!(
            m2.chat_calls(),
            1,
            "skipped request falls through to second"
        );
    }

    #[tokio::test]
    async fn dispatch_chat_success_after_retry_keeps_target_available() {
        let m1 = Arc::new(MockProvider::new("first"));
        let m2 = Arc::new(MockProvider::new("second"));
        m1.push_chat(ChatBehavior::Retryable(AppError::Internal(
            "transient".into(),
        )));
        m1.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        let state = build_state(vec![m1.clone(), m2.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);
        let targets = vec![target("first", "gpt-4o", 1), target("second", "gpt-4o", 0)];

        let mut first_errors = Vec::new();
        let first = dispatch_chat(make_ctx(
            &state,
            &rt,
            &req,
            &targets,
            false,
            &mut first_errors,
        ))
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(m1.chat_calls(), 2);
        assert_eq!(m2.chat_calls(), 0);

        let mut second_errors = Vec::new();
        let second = dispatch_chat(make_ctx(
            &state,
            &rt,
            &req,
            &targets,
            false,
            &mut second_errors,
        ))
        .await;
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            m1.chat_calls(),
            3,
            "successful target should remain available"
        );
        assert_eq!(m2.chat_calls(), 0);
        assert!(second_errors.is_empty());
    }

    #[tokio::test]
    async fn dispatch_chat_non_retryable_failure_does_not_open_circuit() {
        let m1 = Arc::new(MockProvider::new("first"));
        let m2 = Arc::new(MockProvider::new("second"));
        m1.push_chat(ChatBehavior::NonRetryable(AppError::BadRequest(
            "bad".into(),
        )));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        let state = build_state(vec![m1.clone(), m2.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);
        let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

        let mut first_errors = Vec::new();
        let first = dispatch_chat(make_ctx(
            &state,
            &rt,
            &req,
            &targets,
            false,
            &mut first_errors,
        ))
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(m1.chat_calls(), 1);
        assert_eq!(m2.chat_calls(), 1);

        let mut second_errors = Vec::new();
        let second = dispatch_chat(make_ctx(
            &state,
            &rt,
            &req,
            &targets,
            false,
            &mut second_errors,
        ))
        .await;
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            m1.chat_calls(),
            2,
            "non-retryable failure should not open circuit"
        );
        assert_eq!(m2.chat_calls(), 1);
        assert!(second_errors.is_empty());
    }

    #[tokio::test]
    async fn dispatch_chat_upstream_4xx_does_not_retry_or_open_circuit() {
        // Regression test: AppError::Upstream { status: 400 } is a
        // client-side error and must be treated as non-retryable:
        //   (1) no retries on the failing provider,
        //   (2) circuit breaker stays closed (no cooldown),
        //   (3) immediately failover to the next target.
        let m1 = Arc::new(MockProvider::new("first"));
        let m2 = Arc::new(MockProvider::new("second"));
        m1.push_chat(ChatBehavior::NonRetryable(AppError::Upstream {
            provider: "first".into(),
            status: 400,
            body: "bad request from upstream".into(),
        }));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        let state = build_state(vec![m1.clone(), m2.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);
        let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

        // First request: first fails with 4xx, should not retry or open circuit.
        let mut first_errors = Vec::new();
        let first = dispatch_chat(make_ctx(
            &state,
            &rt,
            &req,
            &targets,
            false,
            &mut first_errors,
        ))
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(
            m1.chat_calls(),
            1,
            "4xx upstream should not retry the same provider"
        );
        assert_eq!(
            m2.chat_calls(),
            1,
            "should failover to the next target after non-retryable"
        );

        // Circuit must still be closed — a 4xx on a healthy target should
        // not open the breaker (mod.rs:167 design intent).
        let key = crate::circuit_breaker::RouteTargetKey::new("gpt-4o", "first", "gpt-4o");
        assert!(
            rt.route_circuit_breaker.read().await.is_closed(&key),
            "non-retryable 4xx upstream error must not open the circuit"
        );

        // Second request: first should still be callable (not circuit-open).
        let mut second_errors = Vec::new();
        m1.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        let second = dispatch_chat(make_ctx(
            &state,
            &rt,
            &req,
            &targets,
            false,
            &mut second_errors,
        ))
        .await;
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            m1.chat_calls(),
            2,
            "first should be callable again — circuit was not opened"
        );
        assert_eq!(m2.chat_calls(), 1);
        assert!(second_errors.is_empty());
    }

    #[tokio::test]
    async fn dispatch_chat_skips_missing_provider() {
        let m2 = Arc::new(MockProvider::new("second"));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        let state = build_state(vec![m2.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);
        // "first" is not in the registry — should be skipped, not retried
        let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
        let resp = dispatch_chat(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(m2.chat_calls(), 1);
        assert_eq!(
            errors.len(),
            1,
            "missing provider should push an error and continue"
        );
    }

    async fn build_state_with_strategy(
        providers: Vec<Arc<dyn LlmProvider>>,
        strategy: Strategy,
    ) -> AppState {
        let mut registry: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
        for p in providers {
            registry.insert(p.name().to_string(), p);
        }
        let config = GatewayConfig {
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["k".into()],
            }),
            routes: vec![sag_config::RouteConfig {
                model_name: "gpt-4o".into(),
                api_endpoint: "/chat/completions".into(),
                strategy,
                targets: vec![
                    sag_config::RouteTarget {
                        provider: "first".into(),
                        real_model: None,
                        priority: 10,
                        retries: Some(0),
                        timeout_secs: None,
                        extra: HashMap::new(),
                    },
                    sag_config::RouteTarget {
                        provider: "second".into(),
                        real_model: None,
                        priority: 5,
                        retries: Some(0),
                        timeout_secs: None,
                        extra: HashMap::new(),
                    },
                ],
                retries: None,
                timeout_secs: None,
            }],
            ..sag_config::gateway_config_base!()
        };
        let handle = ConfigHandle::from_config(config);
        crate::testing::test_app_state(handle, Arc::new(RwLock::new(registry))).await
    }

    #[tokio::test]
    async fn sticky_failover_keeps_active_target_even_if_others_have_higher_priority() {
        let m1 = Arc::new(MockProvider::new("first"));
        let m2 = Arc::new(MockProvider::new("second"));
        m1.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));

        let state =
            build_state_with_strategy(vec![m1.clone(), m2.clone()], Strategy::StickyFailover).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);

        // Pre-record "second" as the active target (which has priority 5, lower than "first"'s 10)
        {
            let mut active = rt.active_targets.write().await;
            active.insert(
                "gpt-4o".to_string(),
                (
                    "second".to_string(),
                    "gpt-4o".to_string(),
                    std::time::Instant::now(),
                ),
            );
        }

        let trace_id = Uuid::new_v4();
        let start = Instant::now();

        // Resolve route (should place "second" first due to sticky active target)
        let (resolved_targets, api_endpoint, strategy) =
            crate::dispatch::resolve_route(&state, &rt, &req, trace_id, start, false, None, None)
                .await
                .unwrap();

        assert_eq!(resolved_targets[0].provider, "second");

        // Dispatch
        let mut errors = Vec::new();
        let ctx = DispatchCtx {
            state: &state,
            runtime: &rt,
            request: &req,
            targets: &resolved_targets,
            api_endpoint: &api_endpoint,
            is_stream: false,
            trace_id,
            start,
            errors: &mut errors,
            strategy,
            user_agent: None,
            client_ip: None,
        };

        let resp = dispatch_chat(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(m2.chat_calls(), 1);
        assert_eq!(
            m1.chat_calls(),
            0,
            "first should not be called since second was sticky active"
        );
    }

    #[tokio::test]
    async fn sticky_failover_updates_active_target_on_success_after_failure() {
        let m1 = Arc::new(MockProvider::new("first"));
        let m2 = Arc::new(MockProvider::new("second"));
        // "first" fails, "second" succeeds
        m1.push_chat(ChatBehavior::Retryable(AppError::Internal("fail".into())));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));

        let state =
            build_state_with_strategy(vec![m1.clone(), m2.clone()], Strategy::StickyFailover).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);

        let trace_id = Uuid::new_v4();
        let start = Instant::now();

        // Resolve route
        let (resolved_targets, api_endpoint, strategy) =
            crate::dispatch::resolve_route(&state, &rt, &req, trace_id, start, false, None, None)
                .await
                .unwrap();

        // Dispatch
        let mut errors = Vec::new();
        let ctx = DispatchCtx {
            state: &state,
            runtime: &rt,
            request: &req,
            targets: &resolved_targets,
            api_endpoint: &api_endpoint,
            is_stream: false,
            trace_id,
            start,
            errors: &mut errors,
            strategy,
            user_agent: None,
            client_ip: None,
        };

        let resp = dispatch_chat(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(m1.chat_calls(), 1);
        assert_eq!(m2.chat_calls(), 1);

        // Now "second" must have been recorded as the active target
        {
            let active = rt.active_targets.read().await;
            let active_target = active.get("gpt-4o").unwrap();
            assert_eq!(active_target.0, "second");
        }
    }

    // ── empty-response detection tests ─────────────────────────────

    #[test]
    fn is_empty_response_data_detects_ct0_with_finish_reason() {
        // Production pattern: finish_reason=stop + usage{pt>0,ct absent}
        // + empty delta.  finish_reason does NOT count as content.
        let raw = b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"total_tokens\":100}}\n\n";
        assert!(
            is_empty_response_data(raw),
            "frame with ct=0 and finish_reason but no content is empty"
        );
    }

    #[test]
    fn is_empty_response_data_detects_ct0_without_choices() {
        let raw = b"data: {\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":0,\"total_tokens\":100}}\n\n";
        assert!(is_empty_response_data(raw));
    }

    #[test]
    fn is_empty_response_data_rejects_content_frame() {
        let raw = b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n";
        assert!(!is_empty_response_data(raw), "content frame is not empty");
    }

    #[test]
    fn is_empty_response_data_rejects_ct_positive_with_content() {
        // ct>0 with content in choices → not empty
        let raw = b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"total_tokens\":150}}\n\n";
        assert!(
            !is_empty_response_data(raw),
            "ct>0 with content frame is not empty"
        );
    }

    #[test]
    fn is_empty_response_data_detects_ct_positive_no_content() {
        // ct>0 but choices empty → now detected as empty (choices trump ct)
        let raw = b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"total_tokens\":150}}\n\n";
        assert!(
            is_empty_response_data(raw),
            "ct>0 with empty choices is now empty"
        );
    }

    #[test]
    fn is_empty_response_data_rejects_pt_zero() {
        let raw = b"data: {\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":0}}\n\n";
        assert!(
            !is_empty_response_data(raw),
            "pt=0 is not an anomalous empty response"
        );
    }

    #[test]
    fn is_empty_response_data_rejects_non_json() {
        let raw = b"data: [DONE]\n\n";
        assert!(!is_empty_response_data(raw), "[DONE] is handled separately");
    }

    // ── is_empty_non_stream_response tests ──────────────────────────

    #[test]
    fn non_stream_empty_detects_ct0_no_content() {
        let r = ChatCompletionResponse {
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 0,
                total_tokens: 100,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            ..build_response("gpt-4o", None)
        };
        assert!(is_empty_non_stream_response(&r));
    }

    #[test]
    fn non_stream_empty_detects_ct_positive_no_content() {
        let r = ChatCompletionResponse {
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            ..build_response("gpt-4o", None)
        };
        assert!(
            is_empty_non_stream_response(&r),
            "ct>0 with empty choices should be detected as empty"
        );
    }

    #[test]
    fn non_stream_empty_rejects_string_content() {
        let r = ChatCompletionResponse {
            choices: vec![sag_core::types::ChatChoice {
                index: 0,
                message: sag_core::types::ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::String("hello".into()),
                    name: None,
                    extra: serde_json::Map::new(),
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 0,
                total_tokens: 100,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            ..build_response("gpt-4o", None)
        };
        assert!(
            !is_empty_non_stream_response(&r),
            "string content is not empty"
        );
    }

    #[test]
    fn non_stream_empty_rejects_array_content() {
        let r = ChatCompletionResponse {
            choices: vec![sag_core::types::ChatChoice {
                index: 0,
                message: sag_core::types::ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::Array(vec![serde_json::json!({
                        "type": "text",
                        "text": "hello"
                    })]),
                    name: None,
                    extra: serde_json::Map::new(),
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 0,
                total_tokens: 100,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            ..build_response("gpt-4o", None)
        };
        assert!(
            !is_empty_non_stream_response(&r),
            "array content is not empty"
        );
    }

    #[test]
    fn non_stream_empty_rejects_tool_calls() {
        let r = ChatCompletionResponse {
            choices: vec![sag_core::types::ChatChoice {
                index: 0,
                message: sag_core::types::ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::Null,
                    name: None,
                    extra: serde_json::Map::from_iter([(
                        "tool_calls".into(),
                        serde_json::json!([{"id": "call_1", "type": "function"}]),
                    )]),
                },
                finish_reason: Some("tool_calls".into()),
            }],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 0,
                total_tokens: 100,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            ..build_response("gpt-4o", None)
        };
        assert!(!is_empty_non_stream_response(&r), "tool_calls is not empty");
    }

    #[test]
    fn non_stream_empty_rejects_no_usage() {
        let r = ChatCompletionResponse {
            choices: vec![],
            usage: None,
            ..build_response("gpt-4o", None)
        };
        assert!(!is_empty_non_stream_response(&r));
    }

    #[test]
    fn non_stream_empty_rejects_pt_zero() {
        let r = ChatCompletionResponse {
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            ..build_response("gpt-4o", None)
        };
        assert!(!is_empty_non_stream_response(&r));
    }

    #[test]
    fn non_stream_empty_rejects_empty_string_content() {
        let r = ChatCompletionResponse {
            choices: vec![sag_core::types::ChatChoice {
                index: 0,
                message: sag_core::types::ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::String("".into()),
                    name: None,
                    extra: serde_json::Map::new(),
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 0,
                total_tokens: 100,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            ..build_response("gpt-4o", None)
        };
        assert!(
            is_empty_non_stream_response(&r),
            "empty string is not content"
        );
    }

    #[test]
    fn non_stream_empty_rejects_null_content() {
        let r = ChatCompletionResponse {
            choices: vec![sag_core::types::ChatChoice {
                index: 0,
                message: sag_core::types::ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::Null,
                    name: None,
                    extra: serde_json::Map::new(),
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 0,
                total_tokens: 100,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            ..build_response("gpt-4o", None)
        };
        assert!(is_empty_non_stream_response(&r), "null is not content");
    }

    #[tokio::test]
    async fn validate_stream_empty_data_with_usage_ct0_returns_error() {
        let stream = futures_util::stream::iter(vec![StreamChunk::Data(
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"total_tokens\":100}}\n\n".to_vec(),
        )]);
        let result = validate_stream_first_chunk(stream.boxed(), "cpa").await;
        assert!(matches!(result, Err(AppError::EmptyResponse { .. })));
    }

    #[tokio::test]
    async fn validate_stream_content_data_passes_through() {
        let stream = futures_util::stream::iter(vec![
            StreamChunk::Data(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n".to_vec(),
            ),
            StreamChunk::Done,
        ]);
        let mut s = validate_stream_first_chunk(stream.boxed(), "cpa")
            .await
            .expect("content frame should pass through");
        // First chunk should be the original Data
        let first = s.next().await.unwrap();
        assert!(matches!(first, StreamChunk::Data(_)));
        // Second chunk should be Done
        let second = s.next().await.unwrap();
        assert!(matches!(second, StreamChunk::Done));
    }

    #[tokio::test]
    async fn validate_stream_done_marker_returns_error() {
        let stream =
            futures_util::stream::iter(vec![StreamChunk::Data(b"data: [DONE]\n\n".to_vec())]);
        let result = validate_stream_first_chunk(stream.boxed(), "cpa").await;
        assert!(matches!(result, Err(AppError::EmptyResponse { .. })));
    }

    #[tokio::test]
    async fn validate_stream_usage_ct0_returns_error() {
        let stream = futures_util::stream::iter(vec![StreamChunk::Usage(sag_core::types::Usage {
            prompt_tokens: 100,
            completion_tokens: 0,
            total_tokens: 100,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        })]);
        let result = validate_stream_first_chunk(stream.boxed(), "cpa").await;
        assert!(matches!(result, Err(AppError::EmptyResponse { .. })));
    }

    #[tokio::test]
    async fn validate_stream_usage_ct_positive_passes_through() {
        let stream = futures_util::stream::iter(vec![StreamChunk::Usage(sag_core::types::Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        })]);
        let mut s = validate_stream_first_chunk(stream.boxed(), "cpa")
            .await
            .expect("ct>0 should pass through");
        assert!(matches!(s.next().await, Some(StreamChunk::Usage(_))));
    }

    #[tokio::test]
    async fn validate_stream_empty_stream_returns_error() {
        let stream = futures_util::stream::empty::<StreamChunk>();
        let result = validate_stream_first_chunk(stream.boxed(), "cpa").await;
        assert!(matches!(result, Err(AppError::EmptyResponse { .. })));
    }

    #[tokio::test]
    async fn dispatch_streaming_empty_response_retries_then_succeeds() {
        // First attempt: empty response (Data with ct=0 + finish_reason=stop)
        // Second attempt: normal content response
        let mock = Arc::new(MockProvider::new("openai"));
        mock.push_stream(StreamBehavior::Ok(vec![StreamChunk::Data(
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"total_tokens\":100}}\n\n".to_vec(),
        )]));
        mock.push_stream(StreamBehavior::Ok(vec![
            StreamChunk::Data(b"data: hello\n\n".to_vec()),
            StreamChunk::Usage(sag_core::types::Usage {
                prompt_tokens: 1,
                completion_tokens: 2,
                total_tokens: 3,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            StreamChunk::Done,
        ]));
        let state = build_state(vec![mock.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", true);
        let targets = vec![target("openai", "gpt-4o", 1)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
        let resp = dispatch_chat(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(mock.stream_calls(), 2, "should retry after empty response");

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"data: hello\n\n");
    }

    #[tokio::test]
    async fn dispatch_streaming_empty_response_falls_through_to_next_target() {
        let m1 = Arc::new(MockProvider::new("first"));
        let m2 = Arc::new(MockProvider::new("second"));
        // First provider returns empty response
        m1.push_stream(StreamBehavior::Ok(vec![StreamChunk::Data(
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"total_tokens\":100}}\n\n".to_vec(),
        )]));
        // Second provider returns normal content
        m2.push_stream(StreamBehavior::Ok(vec![
            StreamChunk::Data(b"data: hello\n\n".to_vec()),
            StreamChunk::Done,
        ]));
        let state = build_state(vec![m1.clone(), m2.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", true);
        let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
        let resp = dispatch_chat(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(m1.stream_calls(), 1, "first provider tried once");
        assert_eq!(m2.stream_calls(), 1, "should fall through to second");

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"data: hello\n\n");
    }

    #[tokio::test]
    async fn dispatch_non_streaming_empty_response_retries_then_succeeds() {
        let mock = Arc::new(MockProvider::new("openai"));
        // First attempt: ct=0, no content in choices → empty
        mock.push_chat(ChatBehavior::Ok(build_response(
            "gpt-4o",
            Some(sag_core::types::Usage {
                prompt_tokens: 100,
                completion_tokens: 0,
                total_tokens: 100,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
        )));
        // Second attempt: ct>0 with content → success
        mock.push_chat(ChatBehavior::Ok(build_response_with_content(
            "gpt-4o",
            Some(sag_core::types::Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            "hello",
        )));
        let state = build_state(vec![mock.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);
        let targets = vec![target("openai", "gpt-4o", 1)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
        let resp = dispatch_chat(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(mock.chat_calls(), 2, "should retry after ct=0 response");
    }

    #[tokio::test]
    async fn dispatch_streaming_empty_resp_disabled_passes_through() {
        // retry_on_empty_response = false → ct=0 stream passes through
        let mock = Arc::new(MockProvider::new("openai"));
        mock.push_stream(StreamBehavior::Ok(vec![
            StreamChunk::Data(
                b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"total_tokens\":100}}\n\n"
                    .to_vec(),
            ),
            StreamChunk::Done,
        ]));
        let state = build_state(vec![mock.clone()]).await;
        // Disable empty-response detection
        state.config.write().await.backoff.retry_on_empty_response = false;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", true);
        let targets = vec![target("openai", "gpt-4o", 0)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
        let resp = dispatch_chat(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(mock.stream_calls(), 1, "single attempt, no retry");
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert!(
            !body.is_empty(),
            "raw empty frame forwarded as-is when retry_on_empty_response=false"
        );
    }

    #[tokio::test]
    async fn dispatch_non_streaming_empty_resp_disabled_passes_through() {
        // retry_on_empty_response = false → ct=0 non-stream response passes through
        let mock = Arc::new(MockProvider::new("openai"));
        mock.push_chat(ChatBehavior::Ok(build_response(
            "gpt-4o",
            Some(sag_core::types::Usage {
                prompt_tokens: 100,
                completion_tokens: 0,
                total_tokens: 100,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
        )));
        let state = build_state(vec![mock.clone()]).await;
        // Disable empty-response detection
        state.config.write().await.backoff.retry_on_empty_response = false;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("gpt-4o", false);
        let targets = vec![target("openai", "gpt-4o", 0)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
        let resp = dispatch_chat(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(mock.chat_calls(), 1, "single attempt, no retry");
    }
}
