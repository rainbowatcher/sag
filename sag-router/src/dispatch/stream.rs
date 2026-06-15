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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use futures_util::stream;
    use sag_analytics::CallLogCollector;
    use sag_config::{ConfigHandle, GatewayAuthConfig, GatewayConfig, ServerConfig};
    use sag_core::Usage;
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::RwLock;
    use uuid::Uuid;

    async fn build_state() -> AppState {
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
            providers: Arc::new(RwLock::new(HashMap::new())),
            collector: Arc::new(CallLogCollector::new_memory().await.unwrap()),
            route_counters: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn chunk_stream(chunks: Vec<StreamChunk>) -> Pin<Box<dyn Stream<Item = StreamChunk> + Send>> {
        Box::pin(stream::iter(chunks))
    }

    #[tokio::test]
    async fn data_chunks_are_forwarded_to_body() {
        let state = build_state().await;
        let chunks = vec![
            StreamChunk::Data(b"data: chunk-1\n\n".to_vec()),
            StreamChunk::Data(b"data: chunk-2\n\n".to_vec()),
            StreamChunk::Done,
        ];
        let resp = build_streaming_response(
            &state,
            chunk_stream(chunks),
            "openai",
            "gpt-4o",
            "gpt-4o",
            Uuid::new_v4(),
            Instant::now(),
        );
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("Content-Type").unwrap(),
            "text/event-stream"
        );

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"data: chunk-1\n\ndata: chunk-2\n\n");
    }

    #[tokio::test]
    async fn usage_chunks_are_accumulated_into_call_log() {
        let state = build_state().await;
        let chunks = vec![
            StreamChunk::Data(b"data: hi\n\n".to_vec()),
            StreamChunk::Usage(Usage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
            }),
            StreamChunk::Usage(Usage {
                prompt_tokens: 5,
                completion_tokens: 0,
                total_tokens: 5,
            }),
            StreamChunk::Done,
        ];
        let resp = build_streaming_response(
            &state,
            chunk_stream(chunks),
            "openai",
            "gpt-4o",
            "gpt-4o",
            Uuid::new_v4(),
            Instant::now(),
        );
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        // CallLog should record the accumulated usage: 10+5=15 prompt, 20+0=20 completion, 30+5=35 total
        state.collector.flush().await;
        let rows = sag_analytics::query_stats(state.collector.pool(), None, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, "openai");
        assert_eq!(rows[0].call_count, 1);
        assert_eq!(rows[0].error_count, 0);
        assert_eq!(rows[0].total_prompt_tokens, 15);
        assert_eq!(rows[0].total_completion_tokens, 20);
        assert_eq!(rows[0].total_tokens, 35);
    }

    #[tokio::test]
    async fn mid_stream_error_sets_call_log_status_502() {
        let state = build_state().await;
        let chunks = vec![
            StreamChunk::Data(b"data: partial\n\n".to_vec()),
            StreamChunk::Error("upstream died".into()),
            StreamChunk::Done,
        ];
        let resp = build_streaming_response(
            &state,
            chunk_stream(chunks),
            "openai",
            "gpt-4o",
            "gpt-4o",
            Uuid::new_v4(),
            Instant::now(),
        );
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        state.collector.flush().await;
        let rows = sag_analytics::query_stats(state.collector.pool(), None, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].error_count, 1,
            "mid-stream error should bump error_count"
        );
        assert_eq!(rows[0].call_count, 1);
    }

    #[tokio::test]
    async fn clean_done_terminates_stream_with_status_200() {
        let state = build_state().await;
        let chunks = vec![
            StreamChunk::Data(b"data: ok\n\n".to_vec()),
            StreamChunk::Done,
        ];
        let resp = build_streaming_response(
            &state,
            chunk_stream(chunks),
            "openai",
            "gpt-4o",
            "gpt-4o",
            Uuid::new_v4(),
            Instant::now(),
        );
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        state.collector.flush().await;
        let rows = sag_analytics::query_stats(state.collector.pool(), None, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].call_count, 1);
        assert_eq!(rows[0].error_count, 0);
    }

    #[tokio::test]
    async fn empty_stream_terminates_immediately() {
        let state = build_state().await;
        let resp = build_streaming_response(
            &state,
            chunk_stream(vec![]),
            "openai",
            "gpt-4o",
            "gpt-4o",
            Uuid::new_v4(),
            Instant::now(),
        );
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert!(body.is_empty(), "empty stream should yield empty body");

        // CallLog should still be recorded with default (zero) usage
        state.collector.flush().await;
        let rows = sag_analytics::query_stats(state.collector.pool(), None, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].call_count, 1);
        assert_eq!(rows[0].error_count, 0);
    }

    #[tokio::test]
    async fn none_terminates_stream_just_like_done() {
        // The `unfold` body matches `StreamChunk::Done | None` identically.
        // This test covers the `None` arm: a stream that yields some data
        // then just ends (no explicit `Done`).
        let state = build_state().await;
        let chunks = vec![StreamChunk::Data(b"data: partial\n\n".to_vec())];
        let resp = build_streaming_response(
            &state,
            chunk_stream(chunks),
            "openai",
            "gpt-4o",
            "gpt-4o",
            Uuid::new_v4(),
            Instant::now(),
        );
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"data: partial\n\n");

        state.collector.flush().await;
        let rows = sag_analytics::query_stats(state.collector.pool(), None, None)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].call_count, 1);
        assert_eq!(rows[0].error_count, 0);
    }
}
