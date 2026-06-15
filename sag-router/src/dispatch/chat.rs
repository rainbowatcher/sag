//! Chat-completion dispatch with multi-provider fallback.

use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use sag_core::{AppError, ChatCompletionResponse, StreamChunk};
use tracing::trace;

use crate::dispatch::resolve::resolve_provider;
use crate::dispatch::retry::run_with_retries;
use crate::dispatch::stream::build_streaming_response;
use crate::dispatch::{DispatchCtx, fallback_exhausted_response, success_log};

/// Unified result of a single chat attempt. The stream and non-stream
/// outcomes are different types, so we wrap them in an enum to share
/// the [`run_with_retries`] helper.
enum ChatAttempt {
    Stream(futures_util::stream::BoxStream<'static, StreamChunk>),
    NonStream(ChatCompletionResponse),
}

/// Try each target in order. On a retryable error, fall through to the next
/// target. On a non-retryable error (4xx), bail out immediately.
pub async fn dispatch_chat(ctx: DispatchCtx<'_>) -> Response<Body> {
    let DispatchCtx {
        state,
        request,
        targets,
        api_endpoint,
        is_stream,
        trace_id,
        start,
        errors,
    } = ctx;

    for (provider_name, real_model, target_extra, retries) in targets {
        let Some(provider) = resolve_provider(state, provider_name, errors).await else {
            continue;
        };

        // Hoist the String conversion out of the closure so we don't
        // re-allocate on every retry attempt.
        let real_model = real_model.clone();
        let target_extra = target_extra.clone();
        let api_endpoint = api_endpoint.to_string();

        let outcome =
            run_with_retries::<ChatAttempt, _, _>(provider_name, *retries, errors, |attempt| {
                let provider = provider.clone();
                let real_model = real_model.clone();
                let target_extra = target_extra.clone();
                let api_endpoint = api_endpoint.clone();
                async move {
                    trace!(
                        provider = %provider_name,
                        real_model = %real_model,
                        attempt,
                        "attempting provider target"
                    );
                    if is_stream {
                        provider
                            .chat_stream(request, &real_model, &api_endpoint, &target_extra)
                            .await
                            .map(|s| ChatAttempt::Stream(Box::pin(s)))
                    } else {
                        provider
                            .chat(request, &real_model, &api_endpoint, &target_extra)
                            .await
                            .map(ChatAttempt::NonStream)
                    }
                }
            })
            .await;

        let Some(attempt) = outcome else {
            continue;
        };

        match attempt {
            ChatAttempt::Stream(stream) => {
                return build_streaming_response(
                    state,
                    stream,
                    provider_name,
                    &request.model,
                    &real_model,
                    trace_id,
                    start,
                );
            }
            ChatAttempt::NonStream(resp) => {
                state.collector.log(success_log(
                    trace_id,
                    provider_name,
                    &request.model,
                    &real_model,
                    false,
                    start,
                    resp.usage.as_ref(),
                ));
                return (StatusCode::OK, Json(resp)).into_response();
            }
        }
    }

    fallback_exhausted_response(state, &request.model, is_stream, trace_id, start, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::ResolvedTarget;
    use crate::state::AppState;
    use async_trait::async_trait;
    use axum::body::to_bytes;
    use futures_util::Stream;
    use sag_analytics::CallLogCollector;
    use sag_config::{ConfigHandle, GatewayAuthConfig, GatewayConfig, ServerConfig};
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
        Retryable(AppError),
        NonRetryable(AppError),
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
        ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
            self.stream_calls.fetch_add(1, Ordering::SeqCst);
            match self.stream_queue.lock().unwrap().pop_front() {
                Some(StreamBehavior::Ok(chunks)) => {
                    Ok(Box::pin(futures_util::stream::iter(chunks)))
                }
                Some(StreamBehavior::Retryable(e)) | Some(StreamBehavior::NonRetryable(e)) => {
                    Err(e)
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

    async fn build_state(providers: Vec<Arc<dyn LlmProvider>>) -> AppState {
        let mut registry: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
        for p in providers {
            registry.insert(p.name().to_string(), p);
        }
        let config = GatewayConfig {
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 3000,
            },
            analytics: None,
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["k".into()],
            }),
            retries: 2,
            providers: HashMap::new(),
            routes: vec![],
        };
        AppState {
            config: ConfigHandle::from_config(config),
            providers: Arc::new(RwLock::new(registry)),
            collector: Arc::new(CallLogCollector::new_memory().await.unwrap()),
            route_counters: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn target(provider: &str, real_model: &str, retries: u32) -> ResolvedTarget {
        (provider.into(), real_model.into(), HashMap::new(), retries)
    }

    fn make_ctx<'a>(
        state: &'a AppState,
        request: &'a ChatCompletionRequest,
        targets: &'a [ResolvedTarget],
        is_stream: bool,
        errors: &'a mut Vec<String>,
    ) -> DispatchCtx<'a> {
        DispatchCtx {
            state,
            request,
            targets,
            api_endpoint: "/chat/completions",
            is_stream,
            trace_id: Uuid::new_v4(),
            start: Instant::now(),
            errors,
        }
    }

    // ─── Tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn dispatch_chat_non_streaming_success() {
        let mock = Arc::new(MockProvider::new("openai"));
        mock.push_chat(ChatBehavior::Ok(build_response(
            "gpt-4o",
            Some(Usage {
                prompt_tokens: 5,
                completion_tokens: 7,
                total_tokens: 12,
            }),
        )));
        let state = build_state(vec![mock.clone()]).await;
        let req = build_request("gpt-4o", false);
        let targets = vec![target("openai", "gpt-4o", 0)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &req, &targets, false, &mut errors);
        let resp = dispatch_chat(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(mock.chat_calls(), 1);
        assert!(errors.is_empty());

        // Verify CallLog was recorded with usage
        state.collector.flush().await;
        let rows = sag_analytics::query_stats(state.collector.pool(), None, None)
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
            }),
            StreamChunk::Done,
        ]));
        let state = build_state(vec![mock.clone()]).await;
        let req = build_request("gpt-4o", true);
        let targets = vec![target("openai", "gpt-4o", 0)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &req, &targets, true, &mut errors);
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
        let req = build_request("gpt-4o", false);
        let targets = vec![target("openai", "gpt-4o", 1)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &req, &targets, false, &mut errors);
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
        let req = build_request("gpt-4o", false);
        let targets = vec![target("openai", "gpt-4o", 5)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &req, &targets, false, &mut errors);
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
        let req = build_request("gpt-4o", false);
        let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &req, &targets, false, &mut errors);
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
        let req = build_request("gpt-4o", false);
        let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &req, &targets, false, &mut errors);
        let resp = dispatch_chat(ctx).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(errors.len(), 2);
    }

    #[tokio::test]
    async fn dispatch_chat_skips_missing_provider() {
        let m2 = Arc::new(MockProvider::new("second"));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        let state = build_state(vec![m2.clone()]).await;
        let req = build_request("gpt-4o", false);
        // "first" is not in the registry — should be skipped, not retried
        let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];
        let mut errors = Vec::new();
        let ctx = make_ctx(&state, &req, &targets, false, &mut errors);
        let resp = dispatch_chat(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(m2.chat_calls(), 1);
        assert_eq!(
            errors.len(),
            1,
            "missing provider should push an error and continue"
        );
    }
}
