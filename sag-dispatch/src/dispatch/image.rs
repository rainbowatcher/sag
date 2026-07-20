//! Image generation dispatch with multi-provider fallback.

use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use sag_core::{ChatCompletionRequest, ImageGenerationRequest, ImageGenerationResponse};

use crate::dispatch::resolve::resolve_provider;
use crate::dispatch::retry::RetryOutcome;
use crate::dispatch::retry::run_with_retries_outcome;
use crate::dispatch::{
    DispatchCtx, apply_circuit_outcome, circuit_allows_target, fallback_exhausted_response,
    log_per_target_failure, merge_model_overrides, release_probe_slot, success_log,
};

/// Image generation fallback loop. Mirrors the chat dispatch shape but
/// always non-streaming.
///
/// `api_endpoint` and `is_stream` are unused here (image generation is
/// always non-streaming and ignores the route's `api_endpoint`), but
/// they're part of [`DispatchCtx`] so the call site can build the same
/// context for both dispatch paths.
pub async fn dispatch_image(ctx: DispatchCtx<'_>) -> Response<Body> {
    let DispatchCtx {
        state,
        runtime,
        request,
        targets,
        api_endpoint: _,
        is_stream: _,
        trace_id,
        start,
        errors,
        strategy,
        user_agent,
        client_ip,
    } = ctx;

    let img_request = build_image_request(request);

    // Snapshot the backoff config once for the whole dispatch — see
    // `dispatch_chat` for the rationale.
    let config = state.config.read().await.clone();
    let backoff = config.backoff.clone();
    let circuit_breaker_cfg = config.circuit_breaker.clone();

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

        let real_model = real_model.clone();
        let target_extra =
            merge_model_overrides(&config.providers, provider_name, &real_model, &target.extra);
        let retries = target.retries;
        let timeout_secs = target.timeout_secs;
        let img_request = img_request.clone();

        let outcome = run_with_retries_outcome::<ImageGenerationResponse, _, _>(
            provider_name,
            retries,
            &backoff,
            errors,
            |_attempt| {
                let provider = provider.clone();
                let real_model = real_model.clone();
                let target_extra = target_extra.clone();
                let img_request = img_request.clone();
                async move {
                    provider
                        .chat_image(&img_request, &real_model, &target_extra, timeout_secs)
                        .await
                }
            },
        )
        .await;

        // Extract the error_category from the outcome before
        // `apply_circuit_outcome` consumes it.
        let category = match &outcome {
            RetryOutcome::Exhausted(e) => e.error_category(),
            RetryOutcome::NonRetryable => "bad_request",
            RetryOutcome::Success(_) => "", // never logged
        };

        let Some(resp) = apply_circuit_outcome(
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
                false,
                start,
                errors,
                category,
                None, // reasoning_effort — not applicable for image generation
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

        state.collector.log(success_log(
            trace_id,
            provider_name,
            &request.model,
            &real_model,
            false,
            start,
            None,
            None, // reasoning_effort — not applicable for image generation
            None, // upstream_duration_ms — image path timing is out of scope for stage 2
            None, // ttft_ms — image generation is single-shot, conceptually non-streaming
            user_agent.clone(),
            client_ip.clone(),
        ));
        return (StatusCode::OK, Json(resp)).into_response();
    }

    fallback_exhausted_response(
        state,
        &request.model,
        false,
        trace_id,
        start,
        errors,
        None, // reasoning_effort — not applicable for image generation
        user_agent.as_deref(),
        client_ip.as_deref(),
    )
}

/// Build an `ImageGenerationRequest` from a chat completion request,
/// forwarding the well-known extra fields (`size`, `quality`, etc.).
pub fn build_image_request(request: &ChatCompletionRequest) -> ImageGenerationRequest {
    let prompt = request
        .messages
        .last()
        .and_then(|m| m.text_content().map(String::from))
        .unwrap_or_default();

    let mut extra = serde_json::Map::new();
    for key in ["size", "quality", "response_format", "style"] {
        if let Some(v) = request.extra.get(key) {
            extra.insert(key.to_string(), v.clone());
        }
    }

    ImageGenerationRequest {
        model: request.model.clone(),
        prompt,
        n: request.n,
        size: None,
        quality: None,
        response_format: None,
        extra,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::ResolvedTarget;
    use crate::state::{AppState, DispatchRuntime};
    use crate::testing::target;
    use async_trait::async_trait;
    use futures_util::Stream;

    use sag_config::{CircuitBreakerConfig, ConfigHandle, GatewayAuthConfig, GatewayConfig};
    use sag_core::{AppError, ChatCompletionResponse, ChatMessage, ImageData, StreamChunk};
    use sag_providers::LlmProvider;
    use serde_json::json;
    use std::collections::{HashMap, VecDeque};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;
    use tokio::sync::RwLock;
    use uuid::Uuid;

    enum ImageBehavior {
        Ok(ImageGenerationResponse),
        Retryable(AppError),
    }

    struct MockImageProvider {
        name: String,
        image_queue: Mutex<VecDeque<ImageBehavior>>,
        image_calls: AtomicU32,
    }

    impl MockImageProvider {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                image_queue: Mutex::new(VecDeque::new()),
                image_calls: AtomicU32::new(0),
            }
        }

        fn push_image(&self, behavior: ImageBehavior) {
            self.image_queue.lock().unwrap().push_back(behavior);
        }

        fn image_calls(&self) -> u32 {
            self.image_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl LlmProvider for MockImageProvider {
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
            unreachable!("image dispatch should not call chat")
        }

        async fn chat_stream(
            &self,
            _request: &ChatCompletionRequest,
            _real_model: &str,
            _api_endpoint: &str,
            _target_extra: &HashMap<String, serde_json::Value>,
            _timeout_secs: u64,
        ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
            unreachable!("image dispatch should not call chat_stream")
        }

        async fn chat_image(
            &self,
            _request: &ImageGenerationRequest,
            _real_model: &str,
            _target_extra: &HashMap<String, serde_json::Value>,
            _timeout_secs: u64,
        ) -> sag_core::Result<ImageGenerationResponse> {
            self.image_calls.fetch_add(1, Ordering::SeqCst);
            match self.image_queue.lock().unwrap().pop_front() {
                Some(ImageBehavior::Ok(resp)) => Ok(resp),
                Some(ImageBehavior::Retryable(err)) => Err(err),
                None => Ok(build_image_response()),
            }
        }
    }

    fn build_image_response() -> ImageGenerationResponse {
        ImageGenerationResponse {
            created: 0,
            data: vec![ImageData {
                url: Some("https://example.test/image.png".into()),
                b64_json: None,
                revised_prompt: None,
            }],
        }
    }

    fn build_request(model: &str) -> ChatCompletionRequest {
        serde_json::from_value(json!({
            "model": model,
            "messages": [{"role": "user", "content": "draw a cat"}],
        }))
        .expect("valid image test request")
    }

    fn build_request_with_reasoning(model: &str) -> ChatCompletionRequest {
        serde_json::from_value(json!({
            "model": model,
            "messages": [{"role": "user", "content": "draw a cat"}],
            "reasoning_effort": "high",
        }))
        .expect("valid image test request with reasoning_effort")
    }

    async fn build_state(providers: Vec<Arc<dyn LlmProvider>>) -> AppState {
        let mut registry: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
        for provider in providers {
            registry.insert(provider.name().to_string(), provider);
        }
        let config = GatewayConfig {
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["k".into()],
            }),
            ..sag_config::gateway_config_base!()
        };
        crate::testing::test_app_state(
            ConfigHandle::from_config(config),
            Arc::new(RwLock::new(registry)),
        )
        .await
    }

    /// Like [`build_state`] but with a custom circuit-breaker config, used to
    /// drive cooldowns/intervals to ~0 so half-open behavior is exercisable.
    async fn build_state_with_cb(
        providers: Vec<Arc<dyn LlmProvider>>,
        circuit_breaker: CircuitBreakerConfig,
    ) -> AppState {
        let mut registry: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
        for provider in providers {
            registry.insert(provider.name().to_string(), provider);
        }
        let config = GatewayConfig {
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["k".into()],
            }),
            circuit_breaker,
            ..sag_config::gateway_config_base!()
        };
        crate::testing::test_app_state(
            ConfigHandle::from_config(config),
            Arc::new(RwLock::new(registry)),
        )
        .await
    }

    fn make_ctx<'a>(
        state: &'a AppState,
        runtime: &'a DispatchRuntime,
        request: &'a ChatCompletionRequest,
        targets: &'a [ResolvedTarget],
        errors: &'a mut Vec<String>,
    ) -> DispatchCtx<'a> {
        DispatchCtx {
            state,
            runtime,
            request,
            targets,
            api_endpoint: "/images/generations",
            is_stream: false,
            trace_id: Uuid::new_v4(),
            start: Instant::now(),
            errors,
            strategy: sag_config::Strategy::StickyFailover,
            user_agent: None,
            client_ip: None,
        }
    }

    #[test]
    fn build_image_request_extracts_prompt_from_last_message() {
        let request = ChatCompletionRequest {
            model: "dall-e-3".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: serde_json::json!("a cat in space"),
                name: None,
                extra: serde_json::Map::new(),
            }],
            temperature: None,
            top_p: None,
            n: Some(2),
            stream: None,
            stop: None,
            max_tokens: None,
            presence_penalty: None,
            frequency_penalty: None,
            logit_bias: None,
            max_completion_tokens: None,
            modalities: None,
            audio: None,
            stream_options: None,
            reasoning_effort: None,
            metadata: None,
            parallel_tool_calls: None,
            prediction: None,
            tools: None,
            tool_choice: None,
            response_format: None,
            seed: None,
            logprobs: None,
            top_logprobs: None,
            user: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            safety_identifier: None,
            store: None,
            service_tier: None,
            verbosity: None,
            moderation: None,
            web_search_options: None,
            function_call: None,
            functions: None,
            extra: serde_json::Map::new(),
        };

        let img = build_image_request(&request);
        assert_eq!(img.model, "dall-e-3");
        assert_eq!(img.prompt, "a cat in space");
        assert_eq!(img.n, Some(2));
    }

    #[tokio::test]
    async fn dispatch_image_remembers_exhausted_target_on_next_request() {
        let m1 = Arc::new(MockImageProvider::new("first"));
        let m2 = Arc::new(MockImageProvider::new("second"));
        m1.push_image(ImageBehavior::Retryable(AppError::Internal("fail".into())));
        m2.push_image(ImageBehavior::Ok(build_image_response()));
        m2.push_image(ImageBehavior::Ok(build_image_response()));
        let state = build_state(vec![m1.clone(), m2.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("dall-e-3");
        let targets = vec![
            target("first", "dall-e-3", 0),
            target("second", "dall-e-3", 0),
        ];

        let mut first_errors = Vec::new();
        let first = dispatch_image(make_ctx(&state, &rt, &req, &targets, &mut first_errors)).await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(m1.image_calls(), 1);
        assert_eq!(m2.image_calls(), 1);

        let mut second_errors = Vec::new();
        let second =
            dispatch_image(make_ctx(&state, &rt, &req, &targets, &mut second_errors)).await;
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(m1.image_calls(), 1, "open circuit should skip first target");
        assert_eq!(m2.image_calls(), 2);
        assert!(
            second_errors.iter().any(|e| e.contains("circuit open")),
            "second request should record why the first target was skipped"
        );
    }

    #[tokio::test]
    async fn dispatch_image_half_open_probe_recovers_target() {
        // Mirrors the chat half-open recovery path for image dispatch: `first`
        // fails once (circuit opens), then a probe during cooldown hits the
        // recovered `first` and restores it.
        let m1 = Arc::new(MockImageProvider::new("first"));
        let m2 = Arc::new(MockImageProvider::new("second"));
        m1.push_image(ImageBehavior::Retryable(AppError::Internal("fail".into())));
        m1.push_image(ImageBehavior::Ok(build_image_response()));
        m2.push_image(ImageBehavior::Ok(build_image_response()));
        let cb = CircuitBreakerConfig {
            quota_exhausted_secs: 0,
            rate_limited_secs: 0,
            generic_secs: 0,
            probe_interval_secs: 0,
        };
        let state = build_state_with_cb(vec![m1.clone(), m2.clone()], cb).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("dall-e-3");
        let targets = vec![
            target("first", "dall-e-3", 0),
            target("second", "dall-e-3", 0),
        ];

        let mut first_errors = Vec::new();
        let first = dispatch_image(make_ctx(&state, &rt, &req, &targets, &mut first_errors)).await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(m1.image_calls(), 1);
        assert_eq!(m2.image_calls(), 1);

        let mut second_errors = Vec::new();
        let second =
            dispatch_image(make_ctx(&state, &rt, &req, &targets, &mut second_errors)).await;
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(
            m1.image_calls(),
            2,
            "half-open probe should hit first target and recover it"
        );
        assert_eq!(m2.image_calls(), 1, "recovered target absorbs traffic");
    }

    #[tokio::test]
    async fn dispatch_image_releases_probe_slot_when_provider_unresolved() {
        // Same regression guard as the chat path: a claimed probe slot must be
        // released when `resolve_provider` returns `None` (provider gone).
        let m2 = Arc::new(MockImageProvider::new("second"));
        m2.push_image(ImageBehavior::Ok(build_image_response()));
        let cb = CircuitBreakerConfig {
            quota_exhausted_secs: 0,
            rate_limited_secs: 0,
            generic_secs: 0,
            probe_interval_secs: 0,
        };
        let state = build_state_with_cb(vec![m2.clone()], cb).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request("dall-e-3");
        let targets = vec![
            target("first", "dall-e-3", 0),
            target("second", "dall-e-3", 0),
        ];

        rt.route_circuit_breaker.write().await.open(
            crate::circuit_breaker::RouteTargetKey::new("dall-e-3", "first", "dall-e-3"),
            0,
            0,
        );

        let mut errors = Vec::new();
        let resp = dispatch_image(make_ctx(&state, &rt, &req, &targets, &mut errors)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(m2.image_calls(), 1);

        let mut b = rt.route_circuit_breaker.write().await;
        assert!(
            b.to_half_open_if_due(&crate::circuit_breaker::RouteTargetKey::new(
                "dall-e-3", "first", "dall-e-3"
            )),
            "after release, target should be probeable again"
        );
        assert!(
            b.try_acquire_probe(&crate::circuit_breaker::RouteTargetKey::new(
                "dall-e-3", "first", "dall-e-3"
            )),
            "released probe slot should be claimable again"
        );
    }

    #[test]
    fn build_image_request_forwards_known_extra_fields() {
        let mut extra = serde_json::Map::new();
        extra.insert("size".into(), serde_json::json!("1024x1024"));
        extra.insert("quality".into(), serde_json::json!("hd"));
        extra.insert("response_format".into(), serde_json::json!("b64_json"));
        extra.insert("style".into(), serde_json::json!("vivid"));
        extra.insert("unsupported_field".into(), serde_json::json!("ignored"));

        let request = ChatCompletionRequest {
            model: "dall-e-3".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: serde_json::json!("a dog"),
                name: None,
                extra: serde_json::Map::new(),
            }],
            temperature: None,
            top_p: None,
            n: None,
            stream: None,
            stop: None,
            max_tokens: None,
            presence_penalty: None,
            frequency_penalty: None,
            logit_bias: None,
            max_completion_tokens: None,
            modalities: None,
            audio: None,
            stream_options: None,
            reasoning_effort: None,
            metadata: None,
            parallel_tool_calls: None,
            prediction: None,
            tools: None,
            tool_choice: None,
            response_format: None,
            seed: None,
            logprobs: None,
            top_logprobs: None,
            user: None,
            prompt_cache_key: None,
            prompt_cache_retention: None,
            safety_identifier: None,
            store: None,
            service_tier: None,
            verbosity: None,
            moderation: None,
            web_search_options: None,
            function_call: None,
            functions: None,
            extra,
        };

        let img = build_image_request(&request);
        assert_eq!(img.extra.get("size"), Some(&serde_json::json!("1024x1024")));
        assert_eq!(img.extra.get("quality"), Some(&serde_json::json!("hd")));
        assert_eq!(
            img.extra.get("response_format"),
            Some(&serde_json::json!("b64_json"))
        );
        assert_eq!(img.extra.get("style"), Some(&serde_json::json!("vivid")));
        assert!(!img.extra.contains_key("unsupported_field"));
    }

    // ── reasoning_effort round-trip test ─────────────────

    #[tokio::test]
    async fn image_failure_always_writes_reasoning_effort_none() {
        // Even when the client request includes reasoning_effort, the image
        // dispatch path must always write None because image generation has
        // no reasoning concept.
        let m1 = Arc::new(MockImageProvider::new("first"));
        m1.push_image(ImageBehavior::Retryable(AppError::Internal("fail".into())));
        let state = build_state(vec![m1.clone()]).await;
        let rt = crate::testing::test_dispatch_runtime();
        let req = build_request_with_reasoning("dall-e-3");
        let targets = vec![target("first", "dall-e-3", 0)];
        let mut errors = Vec::new();

        let _ = dispatch_image(make_ctx(&state, &rt, &req, &targets, &mut errors)).await;

        state.collector.flush().await;
        let logs = state
            .collector
            .store()
            .query_logs(sag_analytics::LogFilter {
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        // Two logs: per-target failure + fallback_exhausted_response
        assert_eq!(logs.len(), 2);
        for log in &logs {
            assert_eq!(
                log.reasoning_effort, None,
                "image dispatch path must always write reasoning_effort=None, got {:?} for provider={}",
                log.reasoning_effort, log.provider,
            );
        }
    }
}
