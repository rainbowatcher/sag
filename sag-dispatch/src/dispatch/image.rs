//! Image generation dispatch with multi-provider fallback.

use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use sag_core::{ChatCompletionRequest, ImageGenerationRequest, ImageGenerationResponse};

use crate::dispatch::resolve::resolve_provider;
use crate::dispatch::retry::run_with_retries_outcome;
use crate::dispatch::{
    DispatchCtx, apply_circuit_outcome, circuit_allows_target, fallback_exhausted_response,
    log_per_target_failure, success_log,
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

        let Some(circuit_key) =
            circuit_allows_target(runtime, &request.model, provider_name, real_model).await
        else {
            errors.push(format!("{provider_name}/{real_model}: circuit open"));
            continue;
        };

        let Some(provider) = resolve_provider(state, provider_name, errors).await else {
            continue;
        };

        let real_model = real_model.clone();
        let target_extra = target.extra.clone();
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

        let Some(resp) =
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
                false,
                start,
                errors,
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
        ));
        return (StatusCode::OK, Json(resp)).into_response();
    }

    fallback_exhausted_response(state, &request.model, false, trace_id, start, errors)
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

    use sag_config::{ConfigHandle, GatewayAuthConfig, GatewayConfig};
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
}
