//! Chat-completion dispatch with multi-provider fallback.

use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use sag_core::{ChatCompletionResponse, StreamChunk};

use crate::dispatch::resolve::resolve_provider;
use crate::dispatch::retry::run_with_retries_outcome;
use crate::dispatch::stream::build_streaming_response;
use crate::dispatch::{
    DispatchCtx, apply_circuit_outcome, circuit_allows_target, fallback_exhausted_response,
    log_per_target_failure, success_log,
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

/// Try each target in order. On a retryable error, fall through to the next
/// target. On a non-retryable error (4xx), bail out immediately.
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
    let circuit_breaker_cfg = config.circuit_breaker.clone();
    let api_endpoint = api_endpoint.to_string();

    for target in targets {
        let provider_name = &target.provider;
        let real_model = &target.real_model;

        let Some(circuit_key) =
            circuit_allows_target(runtime, &request.model, provider_name, real_model).await
        else {
            errors.push(format!("{provider_name}/{real_model}: circuit open"));
            continue;
        };

        let Some(provider) = resolve_provider(state, provider_name, errors).await else {
            continue;
        };

        // Clone per-target values once so the retry closure can cheaply clone
        // them for each attempt.
        let real_model = real_model.clone();
        let target_extra = target.extra.clone();
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
                async move {
                    // Capture the upstream start time immediately before the
                    // provider call so retry/circuit overhead is excluded.
                    let upstream_start = std::time::Instant::now();
                    if is_stream {
                        provider
                            .chat_stream(
                                request,
                                &real_model,
                                &api_endpoint,
                                &target_extra,
                                timeout_secs,
                            )
                            .await
                            .map(|s| ChatAttempt::Stream(Box::pin(s), upstream_start))
                    } else {
                        provider
                            .chat(
                                request,
                                &real_model,
                                &api_endpoint,
                                &target_extra,
                                timeout_secs,
                            )
                            .await
                            .map(|r| ChatAttempt::NonStream(r, upstream_start))
                    }
                }
            },
        )
        .await;

        let Some(attempt) =
            apply_circuit_outcome(runtime, circuit_key, outcome, &circuit_breaker_cfg).await
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
                user_agent.as_deref(),
                client_ip.as_deref(),
            );
            continue;
        };

        if strategy == sag_config::Strategy::StickyFailover {
            let mut active = runtime.active_targets.write().await;
            active.insert(
                request.model.clone(),
                (provider_name.clone(), real_model.clone()),
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
    use sag_config::{ConfigHandle, GatewayAuthConfig, GatewayConfig, Strategy};
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
        mock.push_chat(ChatBehavior::Ok(build_response(
            "gpt-4o",
            Some(Usage {
                prompt_tokens: 5,
                completion_tokens: 7,
                total_tokens: 12,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
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
                ("second".to_string(), "gpt-4o".to_string()),
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
}
