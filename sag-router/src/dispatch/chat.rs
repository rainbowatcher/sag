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
    DispatchCtx, TargetAvailability, TargetPass, apply_circuit_outcome, circuit_check_target,
    fallback_exhausted_response, log_per_target_failure, success_log,
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
        request,
        targets,
        api_endpoint,
        is_stream,
        trace_id,
        start,
        errors,
        strategy,
    } = ctx;

    // Snapshot the backoff config once so the per-target retry loop doesn't
    // hold the config read lock and so all targets within one request share
    // a consistent view (a hot-reload mid-dispatch shouldn't change the
    // schedule for an in-flight request).
    let backoff = state.config.read().await.backoff.clone();
    let api_endpoint = api_endpoint.to_string();

    // FillFirst two-pass dispatch:
    //   Pass 1 — skip penalized targets (previously tripped but circuit
    //            now closed). Only try non-penalized targets.
    //   Pass 2 — clear penalized and retry (allows previously-tripped
    //            targets back in when the current active target fails).
    // RoundRobin — single pass, penalized targets are skipped but
    //             closed targets rejoin rotation naturally.
    let mut pass = TargetPass::new(strategy);

    loop {
        for (provider_name, real_model, target_extra, retries) in targets {
            let avail =
                circuit_check_target(state, &request.model, provider_name, real_model, strategy)
                    .await;
            pass.record_skip(&avail);
            let circuit_key = match avail {
                TargetAvailability::Allowed(k) => k,
                TargetAvailability::CircuitOpen => {
                    errors.push(format!("{provider_name}/{real_model}: circuit open"));
                    continue;
                }
                TargetAvailability::Penalized => {
                    errors.push(format!(
                        "{provider_name}/{real_model}: penalized (will retry when current target exhausts)"
                    ));
                    continue;
                }
            };

            let Some(provider) = resolve_provider(state, provider_name, errors).await else {
                continue;
            };

            // Clone per-target values once so the retry closure can cheaply clone
            // them for each attempt.
            let real_model = real_model.clone();
            let target_extra = target_extra.clone();

            let outcome = run_with_retries_outcome::<ChatAttempt, _, _>(
                provider_name,
                *retries,
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
                                .chat_stream(request, &real_model, &api_endpoint, &target_extra)
                                .await
                                .map(|s| ChatAttempt::Stream(Box::pin(s), upstream_start))
                        } else {
                            provider
                                .chat(request, &real_model, &api_endpoint, &target_extra)
                                .await
                                .map(|r| ChatAttempt::NonStream(r, upstream_start))
                        }
                    }
                },
            )
            .await;

            let Some(attempt) = apply_circuit_outcome(state, circuit_key, outcome).await else {
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
                );
                continue;
            };

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
                    ));
                    return (StatusCode::OK, Json(resp)).into_response();
                }
            }
        }
        if !pass.advance(state, &request.model, errors).await {
            break;
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
    use sag_analytics::StatsFilter;
    use sag_config::{
        BackoffConfig, ConfigHandle, GatewayAuthConfig, GatewayConfig, ServerConfig, Strategy,
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
            backoff: BackoffConfig::default(),
            providers: HashMap::new(),
            logging: sag_config::LoggingConfig::default(),
            routes: vec![],
            shuffle_cache_ttl_secs: 7200,
        };
        let handle = ConfigHandle::from_config(config);
        crate::testing::test_app_state(handle, Arc::new(RwLock::new(registry))).await
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
            strategy: Strategy::FillFirst,
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
    async fn dispatch_chat_remembers_exhausted_target_on_next_request() {
        let m1 = Arc::new(MockProvider::new("first"));
        let m2 = Arc::new(MockProvider::new("second"));
        m1.push_chat(ChatBehavior::Retryable(AppError::Internal("fail".into())));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        let state = build_state(vec![m1.clone(), m2.clone()]).await;
        let req = build_request("gpt-4o", false);
        let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

        let mut first_errors = Vec::new();
        let first = dispatch_chat(make_ctx(&state, &req, &targets, false, &mut first_errors)).await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(m1.chat_calls(), 1);
        assert_eq!(m2.chat_calls(), 1);

        let mut second_errors = Vec::new();
        let second =
            dispatch_chat(make_ctx(&state, &req, &targets, false, &mut second_errors)).await;
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
        let req = build_request("gpt-4o", false);
        let targets = vec![target("first", "gpt-4o", 1), target("second", "gpt-4o", 0)];

        let mut first_errors = Vec::new();
        let first = dispatch_chat(make_ctx(&state, &req, &targets, false, &mut first_errors)).await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(m1.chat_calls(), 2);
        assert_eq!(m2.chat_calls(), 0);

        let mut second_errors = Vec::new();
        let second =
            dispatch_chat(make_ctx(&state, &req, &targets, false, &mut second_errors)).await;
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
        let req = build_request("gpt-4o", false);
        let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

        let mut first_errors = Vec::new();
        let first = dispatch_chat(make_ctx(&state, &req, &targets, false, &mut first_errors)).await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(m1.chat_calls(), 1);
        assert_eq!(m2.chat_calls(), 1);

        let mut second_errors = Vec::new();
        let second =
            dispatch_chat(make_ctx(&state, &req, &targets, false, &mut second_errors)).await;
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

    /// Full penalized lifecycle integration test:
    ///   1. Request 1: target A fails → circuit open + penalized, falls through to B (succeeds)
    ///   2. Request 2: circuit still open on A → skipped, B succeeds
    ///   3. Request 3: circuit expired but A penalized → skipped in pass 1,
    ///      B also fails → pass 2 clears penalized, A retried and succeeds
    #[tokio::test]
    async fn dispatch_chat_penalized_flow_two_pass_retry() {
        let a = Arc::new(MockProvider::new("a"));
        let b = Arc::new(MockProvider::new("b"));
        // Request 1: A fails, B succeeds
        a.push_chat(ChatBehavior::Retryable(AppError::Internal("fail".into())));
        b.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        // Request 2: A skipped (circuit open), B succeeds
        b.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
        // Request 3 pass 1: A penalized, B fails
        b.push_chat(ChatBehavior::Retryable(AppError::Internal("b-fail".into())));
        // Request 3 pass 2: penalized cleared, A retried and succeeds
        a.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));

        let state = build_state(vec![a.clone(), b.clone()]).await;
        let req = build_request("gpt-4o", false);
        let targets = vec![target("a", "gpt-4o", 0), target("b", "gpt-4o", 0)];

        // ── Request 1 ──
        let mut errors = Vec::new();
        let r1 = dispatch_chat(make_ctx(&state, &req, &targets, false, &mut errors)).await;
        assert_eq!(r1.status(), StatusCode::OK);
        assert_eq!(a.chat_calls(), 1, "A attempted once then exhausted");
        assert_eq!(b.chat_calls(), 1, "B succeeded on fallthrough");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("a:") && e.contains("retries exhausted"))
        );

        // ── Request 2: circuit still open on A ──
        let mut errors2 = Vec::new();
        let r2 = dispatch_chat(make_ctx(&state, &req, &targets, false, &mut errors2)).await;
        assert_eq!(r2.status(), StatusCode::OK);
        assert_eq!(a.chat_calls(), 1, "A still blocked by open circuit");
        assert_eq!(b.chat_calls(), 2, "B handled request 2");
        assert!(errors2.iter().any(|e| e.contains("circuit open")));

        // ── Force-close the circuit to simulate expiry ──
        state.route_circuit_breaker.write().await.close(
            &crate::circuit_breaker::RouteTargetKey::new("gpt-4o", "a", "gpt-4o"),
        );

        // ── Request 3: circuit closed but A penalized ──
        let mut errors3 = Vec::new();
        let r3 = dispatch_chat(make_ctx(&state, &req, &targets, false, &mut errors3)).await;
        assert_eq!(r3.status(), StatusCode::OK);
        // A: penalized in pass 1 (skipped) → pass 2 cleared → retried → success
        assert_eq!(
            a.chat_calls(),
            2,
            "A retried in pass 2 after penalized cleared"
        );
        // B: tried in pass 1, failed → retries exhausted
        assert_eq!(b.chat_calls(), 3, "B failed in pass 1");
        assert!(errors3.iter().any(|e| e.contains("penalized")));
        assert!(errors3.iter().any(|e| e.contains("clearing penalized")));
    }
}
