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
///
/// `upstream_start` is captured immediately before the provider call. It
/// drives two timing fields on the final `CallLog`:
/// - `upstream_duration_ms` — wall-clock from start to stream end.
/// - `ttft_ms` — wall-clock to the first `StreamChunk::Data`, or `None`
///   if the stream emitted only metadata.
#[allow(clippy::too_many_arguments)]
pub fn build_streaming_response<S>(
    state: &AppState,
    chunk_stream: S,
    provider_name: &str,
    model: &str,
    real_model: &str,
    trace_id: uuid::Uuid,
    start: Instant,
    upstream_start: Instant,
    reasoning_effort: Option<String>,
    user_agent: Option<String>,
    client_ip: Option<String>,
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
        upstream_start: Instant,
        first_byte_at: Option<Instant>,
        reasoning_effort: Option<String>,
        user_agent: Option<String>,
        client_ip: Option<String>,
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
        upstream_start,
        first_byte_at: None,
        reasoning_effort,
        user_agent,
        client_ip,
    };

    let body = Body::from_stream(unfold(state, |mut state| async move {
        if state.done {
            return None;
        }
        loop {
            match state.stream.next().await {
                Some(StreamChunk::Data(raw)) => {
                    // Stamp TTFT once on the first user-visible byte.
                    if state.first_byte_at.is_none() {
                        state.first_byte_at = Some(Instant::now());
                    }
                    return Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(raw)), state));
                }
                Some(StreamChunk::Usage(u)) => {
                    // Token-count heuristics & aggregation strategy:
                    // 1. prompt_tokens: Take maximum across frames because providers report cumulative
                    //    prompt_tokens (handling GLM echoes as well as Anthropic tool-use/web-search
                    //    prompt token expansion).
                    // 2. completion_tokens: Take maximum across frames because providers (including Anthropic
                    //    message_start + message_delta) report cumulative completion_tokens in usage events.
                    // 3. total_tokens: Respect upstream's reported total_tokens when specified, while
                    //    ensuring total_tokens is at least prompt_tokens + completion_tokens.
                    if u.prompt_tokens > 0 {
                        state.usage.prompt_tokens = state.usage.prompt_tokens.max(u.prompt_tokens);
                    }
                    state.usage.completion_tokens =
                        state.usage.completion_tokens.max(u.completion_tokens);

                    let derived_total = state
                        .usage
                        .prompt_tokens
                        .saturating_add(state.usage.completion_tokens);

                    state.usage.total_tokens = if u.total_tokens > 0 {
                        state
                            .usage
                            .total_tokens
                            .max(u.total_tokens)
                            .max(derived_total)
                    } else {
                        derived_total
                    };
                    // Cache/reasoning fields take the last non-None value.
                    if u.cached_tokens.is_some() {
                        state.usage.cached_tokens = u.cached_tokens;
                    }
                    if u.cache_creation_tokens.is_some() {
                        state.usage.cache_creation_tokens = u.cache_creation_tokens;
                    }
                    if u.reasoning_tokens.is_some() {
                        state.usage.reasoning_tokens = u.reasoning_tokens;
                    }
                }
                Some(StreamChunk::Error(msg)) => {
                    warn!(provider = %state.provider, error = %msg, "mid-stream error");
                    state.error = Some(msg);
                }
                Some(StreamChunk::Done) | None => {
                    state.done = true;
                    let error = state.error.take();
                    let status_code = if error.is_some() { 502 } else { 200 };
                    // `upstream_duration_ms` is always populated; `ttft_ms`
                    // is only set if at least one `Data` chunk arrived.
                    let upstream_duration_ms = Instant::now()
                        .duration_since(state.upstream_start)
                        .as_millis() as u64;
                    let ttft_ms = state
                        .first_byte_at
                        .map(|t| t.duration_since(state.upstream_start).as_millis() as u64);
                    // Mirror success_log (mod.rs:151-153): the flat
                    // cache/reasoning columns must carry the same values as
                    // `usage`, because the persistence layer binds these
                    // columns from the flat fields, not from `log.usage`.
                    // Without this, query_cache_stats undercounts streaming
                    // responses. Capture before the move into `Some(state.usage)`.
                    let cached_tokens = state.usage.cached_tokens;
                    let cache_creation_tokens = state.usage.cache_creation_tokens;
                    let reasoning_tokens = state.usage.reasoning_tokens;
                    state.collector.log(CallLog {
                        request_id: state.request_id,
                        provider: state.provider.clone(),
                        model: state.model.clone(),
                        real_model: state.real_model.clone(),
                        stream: true,
                        status_code,
                        duration_ms: state.start.elapsed().as_millis() as u64,
                        error,
                        timestamp: chrono::Utc::now(),
                        reasoning_effort: state.reasoning_effort.clone(),
                        upstream_duration_ms: Some(upstream_duration_ms),
                        ttft_ms,
                        cached_tokens,
                        cache_creation_tokens,
                        reasoning_tokens,
                        usage: Some(state.usage),
                        user_agent: state.user_agent.clone(),
                        client_ip: state.client_ip.clone(),
                        error_category: if status_code == 200 {
                            None
                        } else {
                            Some("stream_error".to_string())
                        },
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
    use sag_analytics::{LogFilter, StatsFilter};
    use sag_config::{ConfigHandle, GatewayAuthConfig, GatewayConfig};
    use sag_core::Usage;
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::sync::RwLock;
    use uuid::Uuid;

    async fn build_state() -> AppState {
        let config = GatewayConfig {
            gateway_auth: Some(GatewayAuthConfig {
                api_keys: vec!["k".into()],
            }),
            ..sag_config::gateway_config_base!()
        };
        let handle = ConfigHandle::from_config(config);
        let registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
        crate::testing::test_app_state(handle, Arc::new(RwLock::new(registry))).await
    }

    fn chunk_stream(chunks: Vec<StreamChunk>) -> Pin<Box<dyn Stream<Item = StreamChunk> + Send>> {
        Box::pin(stream::iter(chunks))
    }

    /// Drain the collector and return the most recently inserted CallLog.
    async fn latest_call_log(state: &AppState) -> sag_core::CallLog {
        state.collector.flush().await;
        let mut logs = state
            .collector
            .store()
            .query_logs(LogFilter {
                limit: 1,
                ..Default::default()
            })
            .await
            .unwrap();
        logs.pop().expect("at least one log row")
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
            Instant::now(),
            None,
            None,
            None,
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
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            StreamChunk::Usage(Usage {
                prompt_tokens: 5,
                completion_tokens: 0,
                total_tokens: 5,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
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
            Instant::now(),
            None,
            None,
            None,
        );
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        // CallLog should record the max cumulative usage across chunks: max(10,5)=10 prompt, 20 completion, 30 total
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
        assert_eq!(rows[0].total_prompt_tokens, 10);
        assert_eq!(rows[0].total_completion_tokens, 20);
        assert_eq!(rows[0].total_tokens, 30);
    }

    /// Regression for the streaming cache-passthrough bug: the flat
    /// `cached_tokens` / `cache_creation_tokens` / `reasoning_tokens`
    /// columns on the stream `CallLog` must carry the same values as
    /// `usage`, because `SqliteLogStore::insert_batch` binds these
    /// columns from the flat fields (not from `log.usage`). Without
    /// this, `query_cache_stats` undercounts every streamed response.
    #[tokio::test]
    async fn cache_fields_persisted_from_stream_usage() {
        let state = build_state().await;
        let chunks = vec![
            StreamChunk::Data(b"data: hi\n\n".to_vec()),
            StreamChunk::Usage(Usage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
                cached_tokens: Some(5),
                cache_creation_tokens: Some(3),
                reasoning_tokens: Some(7),
            }),
            StreamChunk::Done,
        ];
        let resp = build_streaming_response(
            &state,
            chunk_stream(chunks),
            "anthropic",
            "claude",
            "claude-sonnet-4",
            Uuid::new_v4(),
            Instant::now(),
            Instant::now(),
            None,
            None,
            None,
        );
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        let log = latest_call_log(&state).await;
        assert_eq!(log.cached_tokens, Some(5));
        assert_eq!(log.cache_creation_tokens, Some(3));
        assert_eq!(log.reasoning_tokens, Some(7));
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
            Instant::now(),
            None,
            None,
            None,
        );
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        state.collector.flush().await;
        let rows = state
            .collector
            .store()
            .query_stats(StatsFilter::default())
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
            Instant::now(),
            None,
            None,
            None,
        );
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        state.collector.flush().await;
        let rows = state
            .collector
            .store()
            .query_stats(StatsFilter::default())
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
            Instant::now(),
            None,
            None,
            None,
        );
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert!(body.is_empty(), "empty stream should yield empty body");

        // CallLog should still be recorded with default (zero) usage
        state.collector.flush().await;
        let rows = state
            .collector
            .store()
            .query_stats(StatsFilter::default())
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
            Instant::now(),
            None,
            None,
            None,
        );
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"data: partial\n\n");

        state.collector.flush().await;
        let rows = state
            .collector
            .store()
            .query_stats(StatsFilter::default())
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].call_count, 1);
        assert_eq!(rows[0].error_count, 0);
    }

    // ─── Plan §2.3 — TTFT and upstream_duration_ms population ──────

    /// Stream with at least one `Data` chunk before `Done`/`None`:
    /// `ttft_ms` must be `Some` (and ≤ upstream_duration_ms),
    /// `upstream_duration_ms` must be `Some`.
    #[tokio::test]
    async fn ttft_and_upstream_duration_populated_when_data_chunk_seen() {
        let state = build_state().await;
        let chunks = vec![
            StreamChunk::Data(b"data: hello\n\n".to_vec()),
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
            // upstream_start recorded significantly before the stream
            // completes — guarantees a positive upstream_duration_ms.
            Instant::now() - std::time::Duration::from_millis(50),
            Some("medium".into()),
            None,
            None,
        );
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        let log = latest_call_log(&state).await;
        let upstream = log
            .upstream_duration_ms
            .expect("upstream_duration_ms must be set");
        let ttft = log
            .ttft_ms
            .expect("ttft_ms must be set when a Data chunk is seen");
        assert!(
            ttft <= upstream,
            "TTFT ({ttft}) must be ≤ upstream_duration_ms ({upstream})"
        );
        assert!(
            upstream >= 50,
            "upstream_duration_ms ({upstream}) should reflect the 50ms upstream_start offset"
        );
        assert_eq!(
            log.reasoning_effort.as_deref(),
            Some("medium"),
            "reasoning_effort must round-trip through the stream CallLog"
        );
    }

    /// Stream with ONLY Usage/Error/Done metadata chunks (no
    /// `StreamChunk::Data`): `ttft_ms` stays `None` (no observable
    /// user-visible first byte), but `upstream_duration_ms` is still
    /// populated (wall-clock end-to-end elapsed).
    #[tokio::test]
    async fn ttft_is_none_when_stream_has_no_data_chunk() {
        let state = build_state().await;
        let chunks = vec![
            StreamChunk::Usage(Usage {
                prompt_tokens: 10,
                completion_tokens: 20,
                total_tokens: 30,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
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
            Instant::now(),
            None,
            None,
            None,
        );
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        let log = latest_call_log(&state).await;
        assert!(
            log.upstream_duration_ms.is_some(),
            "upstream_duration_ms must still be set for metadata-only streams"
        );
        assert!(
            log.ttft_ms.is_none(),
            "ttft_ms must remain None when no Data chunk was emitted"
        );
    }

    #[tokio::test]
    async fn multiple_usage_chunks_max_aggregated_without_double_counting() {
        let state = build_state().await;
        let chunks = vec![
            // Chunk 1: Intermediate usage with prompt_tokens = 100
            StreamChunk::Usage(Usage {
                prompt_tokens: 100,
                completion_tokens: 0,
                total_tokens: 100,
                cached_tokens: Some(30),
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            // Chunk 2: Final usage repeating prompt_tokens = 100 (cumulative) and adding completion_tokens = 50
            StreamChunk::Usage(Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: Some(20),
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
            Instant::now(),
            None,
            None,
            None,
        );
        let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        let log = latest_call_log(&state).await;
        let usage = log.usage.expect("usage must be set in call log");
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.total_tokens, 150);
        assert_eq!(usage.cached_tokens, Some(30));
        assert_eq!(usage.reasoning_tokens, Some(20));
    }

    #[tokio::test]
    async fn anthropic_cumulative_usage_chunks_max_aggregated() {
        let state = build_state().await;
        let chunks = vec![
            // Chunk 1: message_start with prompt_tokens=25, output_tokens=1
            StreamChunk::Usage(Usage {
                prompt_tokens: 25,
                completion_tokens: 1,
                total_tokens: 26,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            // Chunk 2: message_delta with cumulative output_tokens=15
            StreamChunk::Usage(Usage {
                prompt_tokens: 0,
                completion_tokens: 15,
                total_tokens: 15,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            StreamChunk::Done,
        ];
        let resp = build_streaming_response(
            &state,
            chunk_stream(chunks),
            "anthropic",
            "claude-3-5-sonnet",
            "claude-3-5-sonnet",
            Uuid::new_v4(),
            Instant::now(),
            Instant::now(),
            None,
            None,
            None,
        );

        let _body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        let log = latest_call_log(&state).await;
        let usage = log.usage.expect("usage must be set in call log");
        assert_eq!(usage.prompt_tokens, 25);
        assert_eq!(usage.completion_tokens, 15);
        assert_eq!(usage.total_tokens, 40);
    }

    #[tokio::test]
    async fn anthropic_prompt_tokens_expansion_max_aggregated() {
        let state = build_state().await;
        let chunks = vec![
            // Chunk 1: message_start with initial prompt_tokens=25
            StreamChunk::Usage(Usage {
                prompt_tokens: 25,
                completion_tokens: 1,
                total_tokens: 26,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            // Chunk 2: message_delta with web-search / tool-use expanded prompt_tokens=10682
            StreamChunk::Usage(Usage {
                prompt_tokens: 10682,
                completion_tokens: 510,
                total_tokens: 11192,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            StreamChunk::Done,
        ];
        let resp = build_streaming_response(
            &state,
            chunk_stream(chunks),
            "anthropic",
            "claude-3-5-sonnet",
            "claude-3-5-sonnet",
            Uuid::new_v4(),
            Instant::now(),
            Instant::now(),
            None,
            None,
            None,
        );

        let _body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

        let log = latest_call_log(&state).await;
        let usage = log.usage.expect("usage must be set in call log");
        assert_eq!(usage.prompt_tokens, 10682);
        assert_eq!(usage.completion_tokens, 510);
        assert_eq!(usage.total_tokens, 11192);
    }
}
