//! Image generation dispatch with multi-provider fallback.

use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use sag_core::{ChatCompletionRequest, ImageGenerationRequest, ImageGenerationResponse};

use crate::dispatch::resolve::resolve_provider;
use crate::dispatch::retry::run_with_retries;
use crate::dispatch::{DispatchCtx, fallback_exhausted_response, success_log};

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
        request,
        targets,
        api_endpoint: _,
        is_stream: _,
        trace_id,
        start,
        errors,
    } = ctx;

    let img_request = build_image_request(request);

    // Snapshot the backoff config once for the whole dispatch — see
    // `dispatch_chat` for the rationale.
    let backoff = state.config.read().await.backoff.clone();

    for (provider_name, real_model, target_extra, retries) in targets {
        let Some(provider) = resolve_provider(state, provider_name, errors).await else {
            continue;
        };

        let real_model = real_model.clone();
        let target_extra = target_extra.clone();
        let img_request = img_request.clone();

        let outcome = run_with_retries::<ImageGenerationResponse, _, _>(
            provider_name,
            *retries,
            &backoff,
            errors,
            |_attempt| {
                let provider = provider.clone();
                let real_model = real_model.clone();
                let target_extra = target_extra.clone();
                let img_request = img_request.clone();
                async move {
                    provider
                        .chat_image(&img_request, &real_model, &target_extra)
                        .await
                }
            },
        )
        .await;

        let Some(resp) = outcome else {
            continue;
        };

        state.collector.log(success_log(
            trace_id,
            provider_name,
            &request.model,
            &real_model,
            false,
            start,
            None,
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
    use sag_core::ChatMessage;

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
