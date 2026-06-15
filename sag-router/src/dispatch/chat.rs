//! Chat-completion dispatch with multi-provider fallback.

use axum::{
    Json,
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use sag_core::{ChatCompletionResponse, StreamChunk};
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
