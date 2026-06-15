//! SSE response builder for streaming chat completions.

use std::sync::Arc;
use std::time::Instant;

use axum::{body::Body, http::StatusCode, response::Response};
use futures_util::{Stream, StreamExt, stream::unfold};
use sag_analytics::CallLogCollector;
use sag_core::{CallLog, StreamChunk, Usage};
use tracing::warn;

use crate::state::AppState;

/// Wrap a `StreamChunk` stream in an SSE `Response`, forwarding data chunks
/// to the client verbatim and accumulating `Usage` / `Error` chunks into
/// a final `CallLog` when the stream ends.
pub fn build_streaming_response<S>(
    state: &AppState,
    chunk_stream: S,
    provider_name: &str,
    model: &str,
    real_model: &str,
    trace_id: uuid::Uuid,
    start: Instant,
) -> Response<Body>
where
    // `+ Unpin` is required by `StreamExt::next().await` inside the
    // unfold body. All upstream provider adapters already return
    // `Pin<Box<dyn Stream + Send>>` (boxed at the provider boundary),
    // which satisfies this bound — non-`Unpin` streams would need to
    // be boxed by the caller first.
    S: Stream<Item = StreamChunk> + Send + Unpin + 'static,
{
    struct StreamState<S> {
        stream: S,
        usage: Usage,
        error: Option<String>,
        done: bool,
        collector: Arc<CallLogCollector>,
        request_id: uuid::Uuid,
        provider: String,
        model: String,
        real_model: String,
        start: Instant,
    }

    let collector = state.collector.clone();
    let state = StreamState {
        stream: chunk_stream,
        usage: Usage::default(),
        error: None,
        done: false,
        collector,
        request_id: trace_id,
        provider: provider_name.to_string(),
        model: model.to_string(),
        real_model: real_model.to_string(),
        start,
    };

    let body = Body::from_stream(unfold(state, |mut state| async move {
        if state.done {
            return None;
        }
        loop {
            match state.stream.next().await {
                Some(StreamChunk::Data(raw)) => {
                    return Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(raw)), state));
                }
                Some(StreamChunk::Usage(u)) => {
                    state.usage.prompt_tokens += u.prompt_tokens;
                    state.usage.completion_tokens += u.completion_tokens;
                    state.usage.total_tokens += u.total_tokens;
                }
                Some(StreamChunk::Error(msg)) => {
                    warn!(provider = %state.provider, error = %msg, "mid-stream error");
                    state.error = Some(msg);
                }
                Some(StreamChunk::Done) | None => {
                    state.done = true;
                    let error = state.error.take();
                    let status_code = if error.is_some() { 502 } else { 200 };
                    state.collector.log(CallLog {
                        request_id: state.request_id,
                        provider: state.provider.clone(),
                        model: state.model.clone(),
                        real_model: state.real_model.clone(),
                        stream: true,
                        status_code,
                        duration_ms: state.start.elapsed().as_millis() as u64,
                        usage: Some(state.usage),
                        error,
                        timestamp: chrono::Utc::now(),
                    });
                    return None;
                }
            }
        }
    }));

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("Connection", "keep-alive")
        .body(body)
        .unwrap()
}
