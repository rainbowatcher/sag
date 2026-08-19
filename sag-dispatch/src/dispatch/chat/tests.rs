use super::*;
use crate::dispatch::{ResolvedRoute, ResolvedTarget};
use crate::state::{AppState, DispatchRuntime};
use crate::testing::target;
use async_trait::async_trait;
use axum::body::to_bytes;
use futures_util::Stream;
use sag_analytics::StatsFilter;
use sag_config::{CircuitBreakerConfig, ConfigHandle, GatewayAuthConfig, GatewayConfig, Strategy};
use sag_core::AppError;
use sag_core::types::{ChatCompletionRequest, ChatCompletionResponse, StreamChunk, Usage};
use sag_providers::LlmProvider;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
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
    /// `chat_stream` fails immediately with this error (e.g. an
    /// upstream client-caused 4xx on a streaming request).
    Err(AppError),
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
            Some(StreamBehavior::Ok(chunks)) => Ok(Box::pin(futures_util::stream::iter(chunks))),
            Some(StreamBehavior::Err(e)) => Err(e),
            None => Ok(Box::pin(futures_util::stream::empty())),
        }
    }
}

/// Provider whose `chat` / `chat_stream` never resolve. Used to hold a
/// dispatch in an in-flight provider call so the test can cancel the
/// handler future mid-call (client-disconnect simulation) and assert
/// the `client_cancelled` terminal.
struct BlockingProvider {
    name: String,
    chat_started: Arc<tokio::sync::Notify>,
}

impl BlockingProvider {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            chat_started: Arc::new(tokio::sync::Notify::new()),
        }
    }
}

#[async_trait]
impl LlmProvider for BlockingProvider {
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
        self.chat_started.notify_one();
        std::future::pending().await
    }

    async fn chat_stream(
        &self,
        _request: &ChatCompletionRequest,
        _real_model: &str,
        _api_endpoint: &str,
        _target_extra: &HashMap<String, serde_json::Value>,
        _timeout_secs: u64,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        self.chat_started.notify_one();
        std::future::pending().await
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

/// Like `build_response` but with one `ChatChoice` containing the
/// given text as message content.
fn build_response_with_content(
    model: &str,
    usage: Option<Usage>,
    text: &str,
) -> ChatCompletionResponse {
    ChatCompletionResponse {
        choices: vec![sag_core::types::ChatChoice {
            index: 0,
            message: sag_core::types::ChatMessage {
                role: "assistant".into(),
                content: serde_json::Value::String(text.into()),
                name: None,
                extra: serde_json::Map::new(),
            },
            finish_reason: Some("stop".into()),
        }],
        ..build_response(model, usage)
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

/// Like [`build_state`] but with a custom circuit-breaker config. Used to
/// drive cooldowns to ~0 so half-open recovery can be exercised quickly.
async fn build_state_with_cb(
    providers: Vec<Arc<dyn LlmProvider>>,
    circuit_breaker: CircuitBreakerConfig,
) -> AppState {
    let mut registry: HashMap<String, Arc<dyn LlmProvider>> = HashMap::new();
    for p in providers {
        registry.insert(p.name().to_string(), p);
    }
    let config = GatewayConfig {
        gateway_auth: Some(GatewayAuthConfig {
            api_keys: vec!["k".into()],
        }),
        circuit_breaker,
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
        route_name: "test-route",
        started_at: chrono::Utc::now(),
        errors,
        strategy,
        sticky_token: sag_state::StickyToken::default(),
        user_agent: None,
        client_ip: None,
    }
}

/// Like [`make_ctx`] (StickyFailover) but with an explicit sticky token.
/// [`StickyToken::default`] carries `routes_version = 0`, while a fresh
/// `ConfigHandle` starts at `routes_version = 1`, so tests that assert a
/// sticky writeback must pass a token captured from the live config.
fn make_ctx_with_token<'a>(
    state: &'a AppState,
    runtime: &'a DispatchRuntime,
    request: &'a ChatCompletionRequest,
    targets: &'a [ResolvedTarget],
    errors: &'a mut Vec<String>,
    sticky_token: sag_state::StickyToken,
) -> DispatchCtx<'a> {
    DispatchCtx {
        state,
        runtime,
        request,
        targets,
        api_endpoint: "/chat/completions",
        is_stream: false,
        trace_id: Uuid::new_v4(),
        start: Instant::now(),
        route_name: "test-route",
        started_at: chrono::Utc::now(),
        errors,
        strategy: sag_config::Strategy::StickyFailover,
        sticky_token,
        user_agent: None,
        client_ip: None,
    }
}

// Shared `AppState` builder whose formal event collector writes into a
// pool the test keeps a handle on (see `dispatch::tests`).
use super::super::tests::{build_state_with_event_pool, build_state_with_event_pool_and_backoff};

// ─── Tests ───────────────────────────────────────────────────

#[tokio::test]
#[allow(clippy::type_complexity)]
async fn dispatch_chat_non_streaming_success_emits_success_analytics_event() {
    let mock = Arc::new(MockProvider::new("openai"));
    mock.push_chat(ChatBehavior::Ok(build_response_with_content(
        "gpt-4o",
        Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 7,
            total_tokens: 12,
            cached_tokens: Some(3),
            cache_creation_tokens: None,
            reasoning_tokens: None,
        }),
        "hello",
    )));
    let (state, pool) = build_state_with_event_pool(vec![mock.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("openai", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
    let trace_id = ctx.trace_id;
    let started_at = ctx.started_at;
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Drain the event queue so the writer commits before we read.
    state.event_collector.shutdown().await;

    let (
        final_status,
        failure_reason,
        route_id,
        actual_model,
        prompt_tokens,
        cached_tokens,
        started_ms,
        completed_ms,
        stream,
    ): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        i64,
        i64,
        i32,
    ) = sqlx::query_as(
        "SELECT final_status, failure_reason, route_id, actual_model, prompt_tokens, \
         cached_tokens, started_at, completed_at, stream \
         FROM request_results WHERE request_id = ?",
    )
    .bind(trace_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("success fact must be persisted");

    assert_eq!(final_status, "success");
    assert!(
        failure_reason.is_none(),
        "success must not carry a failure reason"
    );
    let route_id = route_id.expect("success must resolve a route_id");
    assert_eq!(actual_model.as_deref(), Some("gpt-4o"));
    assert_eq!(prompt_tokens, Some(5), "usage must be persisted on success");
    assert_eq!(cached_tokens, Some(3), "cached tokens must be persisted");
    assert_eq!(
        started_ms,
        started_at.timestamp_millis(),
        "started_at must come from the caller-supplied route start"
    );
    assert!(
        completed_ms >= started_ms,
        "completed_at (UTC completion time) must not precede started_at"
    );
    assert_eq!(stream, 0, "non-stream request must record stream=0");

    // The resolved identity must be the route the ctx carried.
    let resolved_name: String =
        sqlx::query_scalar("SELECT route_name FROM analytics_routes WHERE route_id = ?")
            .bind(&route_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(resolved_name, "test-route");

    // Exactly one target attempt, success, with the response usage/cache.
    let (status, t_actual_model, t_pt, t_ct, t_cached, t_stream): (
        String,
        String,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        i32,
    ) = sqlx::query_as(
        "SELECT status, actual_model, prompt_tokens, completion_tokens, cached_tokens, stream \
         FROM target_attempts WHERE request_id = ?",
    )
    .bind(trace_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("exactly one success attempt must be persisted");

    assert_eq!(status, "success");
    assert_eq!(t_actual_model, "gpt-4o");
    assert_eq!(t_pt, Some(5));
    assert_eq!(t_ct, Some(7));
    assert_eq!(t_cached, Some(3));
    assert_eq!(t_stream, 0);
}

#[tokio::test]
async fn dispatch_chat_retry_success_emits_single_success_attempt() {
    // A retry sequence (fail → retry → success) must collapse into
    // exactly one target row with the final success — no retry counts,
    // no intermediate failure rows.
    let mock = Arc::new(MockProvider::new("openai"));
    mock.push_chat(ChatBehavior::Retryable(AppError::Internal("oops".into())));
    mock.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    let (state, pool) = build_state_with_event_pool(vec![mock.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("openai", "gpt-4o", 1)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
    let trace_id = ctx.trace_id;
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(mock.chat_calls(), 2, "provider must have been retried");

    state.event_collector.shutdown().await;

    let final_status: String =
        sqlx::query_scalar("SELECT final_status FROM request_results WHERE request_id = ?")
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("success fact must be persisted");
    assert_eq!(final_status, "success");

    let (count, status): (i64, String) =
        sqlx::query_as("SELECT COUNT(*), MIN(status) FROM target_attempts WHERE request_id = ?")
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        count, 1,
        "retry sequence must produce exactly one target attempt row"
    );
    assert_eq!(
        status, "success",
        "the single row must record the final success"
    );
}

#[tokio::test]
async fn dispatch_chat_fallback_emits_failure_and_success_attempts() {
    // A fails → fallback to B (success): route-level success, one
    // failure attempt for A and one success attempt for B.
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_chat(ChatBehavior::Retryable(AppError::Upstream {
        provider: "first".into(),
        status: 500,
        body: "boom".into(),
    }));
    m2.push_chat(ChatBehavior::Ok(build_response_with_content(
        "gpt-4o", None, "ok",
    )));
    let (state, pool) = build_state_with_event_pool(vec![m1.clone(), m2.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
    let trace_id = ctx.trace_id;
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(m1.chat_calls(), 1);
    assert_eq!(m2.chat_calls(), 1);

    state.event_collector.shutdown().await;

    let final_status: String =
        sqlx::query_scalar("SELECT final_status FROM request_results WHERE request_id = ?")
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("success fact must be persisted");
    assert_eq!(final_status, "success", "fallback success is route success");

    // A failure first, then B success — attempt order follows the
    // dispatch order (provider attribution via the target registry).
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT rt.provider, t.status, t.failure_reason \
         FROM target_attempts t \
         JOIN analytics_route_targets rt ON rt.target_id = t.target_id \
         WHERE t.request_id = ? ORDER BY t.started_at",
    )
    .bind(trace_id.to_string())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2, "two actual attempts → two fact rows");
    assert_eq!(rows[0].0, "first");
    assert_eq!(rows[0].1, "failure");
    assert!(
        rows[0].2.is_some(),
        "failed attempt carries a failure reason"
    );
    assert_eq!(rows[1].0, "second");
    assert_eq!(rows[1].1, "success");
    assert!(
        rows[1].2.is_none(),
        "successful attempt carries no failure reason"
    );
}

#[tokio::test]
async fn dispatch_chat_all_exhausted_emits_model_failure_with_all_attempts() {
    // Every target fails → route-level model_failure with
    // failure_reason all_exhausted plus one failure fact per
    // actually-issued target; failure tokens stay NULL.
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_chat(ChatBehavior::Retryable(AppError::Internal("fail1".into())));
    m2.push_chat(ChatBehavior::Retryable(AppError::Internal("fail2".into())));
    let (state, pool) = build_state_with_event_pool(vec![m1.clone(), m2.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
    let trace_id = ctx.trace_id;
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    state.event_collector.shutdown().await;

    let (final_status, failure_reason, prompt_tokens): (String, Option<String>, Option<i64>) =
        sqlx::query_as(
            "SELECT final_status, failure_reason, prompt_tokens \
             FROM request_results WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("model_failure fact must be persisted");
    assert_eq!(final_status, "model_failure");
    assert_eq!(failure_reason.as_deref(), Some("all_exhausted"));
    assert!(
        prompt_tokens.is_none(),
        "failed request tokens must stay NULL (unknown), never 0"
    );

    let rows: Vec<(String, String, Option<i64>)> = sqlx::query_as(
        "SELECT rt.provider, t.status, t.prompt_tokens \
         FROM target_attempts t \
         JOIN analytics_route_targets rt ON rt.target_id = t.target_id \
         WHERE t.request_id = ? ORDER BY t.started_at",
    )
    .bind(trace_id.to_string())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows.len(),
        2,
        "both actually-issued targets get a failure row"
    );
    assert_eq!(rows[0].0, "first");
    assert_eq!(rows[0].1, "failure");
    assert_eq!(rows[1].0, "second");
    assert_eq!(rows[1].1, "failure");
    assert!(
        rows.iter().all(|(_, _, pt)| pt.is_none()),
        "failed attempts must keep token fields NULL"
    );
}

#[tokio::test]
async fn dispatch_chat_streaming_success_emits_success_analytics_event() {
    // Streaming success now emits the formal analytics event: one
    // `success` request result (stream=1, final usage / cache) plus one
    // success target attempt.
    let mock = Arc::new(MockProvider::new("openai"));
    mock.push_stream(StreamBehavior::Ok(vec![
        StreamChunk::Data(b"data: hello\n\n".to_vec()),
        StreamChunk::Usage(Usage {
            prompt_tokens: 5,
            completion_tokens: 7,
            total_tokens: 12,
            cached_tokens: Some(3),
            cache_creation_tokens: None,
            reasoning_tokens: None,
        }),
        StreamChunk::Done,
    ]));
    let (state, pool) = build_state_with_event_pool(vec![mock.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", true);
    let targets = vec![target("openai", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
    let trace_id = ctx.trace_id;
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(mock.stream_calls(), 1);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&body[..], b"data: hello\n\n");

    state.event_collector.shutdown().await;

    let (final_status, stream, pt, cached, actual_model): (
        String,
        i32,
        Option<i64>,
        Option<i64>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT final_status, stream, prompt_tokens, cached_tokens, actual_model \
         FROM request_results WHERE request_id = ?",
    )
    .bind(trace_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("stream success fact must be persisted");
    assert_eq!(final_status, "success");
    assert_eq!(stream, 1, "streaming request must record stream=1");
    assert_eq!(pt, Some(5), "final usage must be persisted on success");
    assert_eq!(cached, Some(3), "cached tokens must be persisted");
    assert_eq!(actual_model.as_deref(), Some("gpt-4o"));

    let (count, status, t_pt): (i64, String, Option<i64>) = sqlx::query_as(
        "SELECT COUNT(*), MIN(status), MAX(prompt_tokens) \
         FROM target_attempts WHERE request_id = ?",
    )
    .bind(trace_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "exactly one success attempt");
    assert_eq!(status, "success");
    assert_eq!(t_pt, Some(5));
}

#[tokio::test]
async fn dispatch_chat_streaming_fallback_emits_route_success_with_both_attempts() {
    // A fails pre-commit (first chunk is an upstream Error → retryable,
    // exhausted), B streams success: the route succeeds and the single
    // terminal event carries both attempts — A's failure fact (built by
    // the dispatch loop) and B's stream success fact (built at stream
    // terminal inside `build_streaming_response`).
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_stream(StreamBehavior::Ok(vec![StreamChunk::Error(
        "first died".into(),
    )]));
    m2.push_stream(StreamBehavior::Ok(vec![
        StreamChunk::Data(b"data: hello\n\n".to_vec()),
        StreamChunk::Done,
    ]));
    let (state, pool) = build_state_with_event_pool(vec![m1.clone(), m2.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", true);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
    let trace_id = ctx.trace_id;
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(m1.stream_calls(), 1);
    assert_eq!(m2.stream_calls(), 1);
    let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();

    state.event_collector.shutdown().await;

    let (final_status, failure_reason, stream): (String, Option<String>, i32) = sqlx::query_as(
        "SELECT final_status, failure_reason, stream \
             FROM request_results WHERE request_id = ?",
    )
    .bind(trace_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("route success fact must be persisted");
    assert_eq!(final_status, "success");
    assert!(failure_reason.is_none());
    assert_eq!(stream, 1);

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT rt.provider, t.status \
         FROM target_attempts t \
         JOIN analytics_route_targets rt ON rt.target_id = t.target_id \
         WHERE t.request_id = ? ORDER BY t.attempt_id",
    )
    .bind(trace_id.to_string())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows.len(),
        2,
        "pre-commit fallback failure + stream success = two attempts"
    );
    assert_eq!(rows[0].0, "first");
    assert_eq!(rows[0].1, "failure");
    assert_eq!(rows[1].0, "second");
    assert_eq!(rows[1].1, "success");
}

#[tokio::test]
async fn dispatch_chat_streaming_error_after_commit_does_not_fallback() {
    // The stream commits its first byte, then the upstream errors: no
    // fallback to the next target, and the terminal event is
    // model_failure / stream_error_after_commit with a failure attempt.
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_stream(StreamBehavior::Ok(vec![
        StreamChunk::Data(b"data: partial\n\n".to_vec()),
        StreamChunk::Error("upstream died".into()),
        StreamChunk::Done,
    ]));
    let (state, pool) = build_state_with_event_pool(vec![m1.clone(), m2.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", true);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
    let trace_id = ctx.trace_id;
    let resp = dispatch_chat(ctx).await;
    // Headers are already committed once the stream starts — the HTTP
    // status stays 200 (legacy truncated-SSE protocol behavior).
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(m1.stream_calls(), 1);
    assert_eq!(
        m2.stream_calls(),
        0,
        "error after commit must not fall back to the next target"
    );

    state.event_collector.shutdown().await;

    let (final_status, failure_reason): (String, Option<String>) = sqlx::query_as(
        "SELECT final_status, failure_reason FROM request_results WHERE request_id = ?",
    )
    .bind(trace_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("model_failure fact must be persisted");
    assert_eq!(final_status, "model_failure");
    assert_eq!(failure_reason.as_deref(), Some("stream_error_after_commit"));

    let (status, t_fr): (String, Option<String>) =
        sqlx::query_as("SELECT status, failure_reason FROM target_attempts WHERE request_id = ?")
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "failure");
    assert_eq!(t_fr.as_deref(), Some("stream_error_after_commit"));
}

#[tokio::test]
async fn dispatch_chat_streaming_all_exhausted_emits_model_failure_with_attempts() {
    // Every stream target fails pre-commit: 502 response plus a
    // `model_failure` / `all_exhausted` event (stream=1) with one
    // failure fact per actually-issued target.
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_stream(StreamBehavior::Ok(vec![StreamChunk::Error("fail1".into())]));
    m2.push_stream(StreamBehavior::Ok(vec![StreamChunk::Error("fail2".into())]));
    let (state, pool) = build_state_with_event_pool(vec![m1.clone(), m2.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", true);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
    let trace_id = ctx.trace_id;
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    state.event_collector.shutdown().await;

    let (final_status, failure_reason, stream, pt): (String, Option<String>, i32, Option<i64>) =
        sqlx::query_as(
            "SELECT final_status, failure_reason, stream, prompt_tokens \
         FROM request_results WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("model_failure fact must be persisted");
    assert_eq!(final_status, "model_failure");
    assert_eq!(failure_reason.as_deref(), Some("all_exhausted"));
    assert_eq!(stream, 1);
    assert!(pt.is_none(), "failed request tokens stay NULL");

    // `attempt_id` order = insertion order (deterministic; `started_at`
    // has millisecond ties).
    let rows: Vec<(String, String, Option<i64>)> = sqlx::query_as(
        "SELECT rt.provider, t.status, t.prompt_tokens \
         FROM target_attempts t \
         JOIN analytics_route_targets rt ON rt.target_id = t.target_id \
         WHERE t.request_id = ? ORDER BY t.attempt_id",
    )
    .bind(trace_id.to_string())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows.len(),
        2,
        "both actually-issued targets get a failure row"
    );
    assert_eq!(rows[0].0, "first");
    assert_eq!(rows[0].1, "failure");
    assert_eq!(rows[1].0, "second");
    assert_eq!(rows[1].1, "failure");
    assert!(
        rows.iter().all(|(_, _, pt)| pt.is_none()),
        "failed attempts keep token fields NULL"
    );
}

/// Blocker regression: a streaming request whose first provider call
/// returns a client-caused 4xx must still emit a `client_error` fact.
/// Previously the emit was guarded by `if !is_stream`, so the request
/// produced **zero** analytics rows (violating "一请求恰一条") — the
/// stream never committed, so `build_streaming_response`'s terminal
/// could not fire either.
#[tokio::test]
async fn dispatch_chat_streaming_client_error_emits_client_error_fact() {
    let mock = Arc::new(MockProvider::new("openai"));
    mock.push_stream(StreamBehavior::Err(AppError::Upstream {
        provider: "openai".into(),
        status: 400,
        body: "bad request".into(),
    }));
    let (state, pool) = build_state_with_event_pool(vec![mock.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", true);
    let targets = vec![target("openai", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
    let trace_id = ctx.trace_id;
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    state.event_collector.shutdown().await;

    let (final_status, failure_reason, stream): (String, Option<String>, i32) = sqlx::query_as(
        "SELECT final_status, failure_reason, stream \
             FROM request_results WHERE request_id = ?",
    )
    .bind(trace_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("streaming client_error fact must be persisted");
    assert_eq!(final_status, "client_error");
    assert_eq!(failure_reason.as_deref(), Some("invalid_request"));
    assert_eq!(stream, 1, "streaming client_error must record stream=1");

    let attempts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM target_attempts WHERE request_id = ?")
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attempts, 0, "client_error carries no target attempts");
}

/// Major-1 e2e: a non-streaming request cancelled (handler future
/// dropped) while a provider call is in flight emits one
/// `client_cancelled` fact with the in-flight attempt recorded as
/// `cancelled` (spec scenario 11).
#[tokio::test]
async fn dispatch_chat_cancelled_mid_call_emits_client_cancelled_with_cancelled_attempt() {
    let mock = Arc::new(BlockingProvider::new("openai"));
    let (state, pool) = build_state_with_event_pool(vec![mock.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("openai", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
    let trace_id = ctx.trace_id;

    // The provider call never resolves; the timeout drops the handler
    // future mid-call — the axum/hyper client-disconnect signal.
    let result = tokio::time::timeout(Duration::from_millis(100), dispatch_chat(ctx)).await;
    assert!(result.is_err(), "dispatch must be cancelled by the timeout");

    state.event_collector.shutdown().await;

    let (final_status, stream): (String, i32) =
        sqlx::query_as("SELECT final_status, stream FROM request_results WHERE request_id = ?")
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("client_cancelled fact must be persisted");
    assert_eq!(final_status, "client_cancelled");
    assert_eq!(stream, 0, "non-streaming cancel records stream=0");

    let (status, actual_model, pt): (String, String, Option<i64>) = sqlx::query_as(
        "SELECT t.status, t.actual_model, t.prompt_tokens \
         FROM target_attempts t WHERE t.request_id = ?",
    )
    .bind(trace_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("in-flight attempt must be recorded as cancelled");
    assert_eq!(status, "cancelled");
    assert_eq!(actual_model, "gpt-4o");
    assert!(pt.is_none(), "cancelled attempt tokens stay NULL");
}

/// Major-1 e2e: a streaming request cancelled **before** commit (the
/// dispatch loop, not the response body) emits `client_cancelled` with
/// `stream=1` and the in-flight attempt recorded as `cancelled`.
#[tokio::test]
async fn dispatch_chat_streaming_cancelled_before_commit_emits_client_cancelled() {
    let mock = Arc::new(BlockingProvider::new("openai"));
    let (state, pool) = build_state_with_event_pool(vec![mock.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", true);
    let targets = vec![target("openai", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
    let trace_id = ctx.trace_id;

    let result = tokio::time::timeout(Duration::from_millis(100), dispatch_chat(ctx)).await;
    assert!(result.is_err(), "dispatch must be cancelled by the timeout");

    state.event_collector.shutdown().await;

    let (final_status, stream): (String, i32) =
        sqlx::query_as("SELECT final_status, stream FROM request_results WHERE request_id = ?")
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("pre-commit streaming cancel must persist a client_cancelled fact");
    assert_eq!(final_status, "client_cancelled");
    assert_eq!(stream, 1, "streaming cancel records stream=1");

    let (status, actual_model): (String, String) = sqlx::query_as(
        "SELECT t.status, t.actual_model FROM target_attempts t WHERE t.request_id = ?",
    )
    .bind(trace_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("in-flight stream attempt must be recorded as cancelled");
    assert_eq!(status, "cancelled");
    assert_eq!(actual_model, "gpt-4o");
}

/// Major-1 e2e: a cancel during the retry/backoff gap (no in-flight
/// provider call) emits `client_cancelled` with **zero** target
/// attempts (spec scenario 11: 退避/fallback 间隙 → 0 条).
#[tokio::test]
async fn dispatch_chat_cancelled_during_backoff_emits_client_cancelled_without_attempts() {
    let mock = Arc::new(MockProvider::new("openai"));
    // Retryable error: attempt 0 fails fast, then the loop sleeps the
    // (jitter-free, 10 s) backoff — the timeout lands inside the sleep.
    mock.push_chat(ChatBehavior::Retryable(AppError::Internal("boom".into())));
    let backoff = sag_config::BackoffConfig {
        retries: 1,
        base_ms: 10_000,
        max_ms: 10_000,
        jitter: false,
        retry_on_empty_response: false,
    };
    let (state, pool) = build_state_with_event_pool_and_backoff(vec![mock.clone()], backoff).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("openai", "gpt-4o", 1)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
    let trace_id = ctx.trace_id;

    let result = tokio::time::timeout(Duration::from_millis(100), dispatch_chat(ctx)).await;
    assert!(result.is_err(), "dispatch must be cancelled by the timeout");

    state.event_collector.shutdown().await;

    let (final_status, stream): (String, i32) =
        sqlx::query_as("SELECT final_status, stream FROM request_results WHERE request_id = ?")
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .expect("backoff-gap cancel must persist a client_cancelled fact");
    assert_eq!(final_status, "client_cancelled");
    assert_eq!(stream, 0);

    let attempts: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM target_attempts WHERE request_id = ?")
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        attempts, 0,
        "cancel during a backoff gap must not fabricate an attempt"
    );
}

#[tokio::test]
async fn dispatch_chat_non_streaming_success() {
    let mock = Arc::new(MockProvider::new("openai"));
    mock.push_chat(ChatBehavior::Ok(build_response_with_content(
        "gpt-4o",
        Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 7,
            total_tokens: 12,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        }),
        "hello",
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
async fn dispatch_chat_half_open_probe_recovers_target() {
    // First request: `first` fails (retryable) → circuit opens. With a
    // 0-second generic cooldown the circuit is already half-open by the
    // next request, which probes `first` and succeeds, recovering it.
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_chat(ChatBehavior::Retryable(AppError::Internal("fail".into())));
    m1.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    let cb = CircuitBreakerConfig {
        quota_exhausted_secs: 0,
        rate_limited_secs: 0,
        generic_secs: 0,
        probe_interval_secs: 0,
    };
    let state = build_state_with_cb(vec![m1.clone(), m2.clone()], cb).await;
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
    // First request: first fails → fall through to second (success).
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
    // Second request: circuit is half-open, probe `first` succeeds →
    // target recovered, no longer skipped.
    assert_eq!(second.status(), StatusCode::OK);
    assert_eq!(
        m1.chat_calls(),
        2,
        "half-open probe should hit first target"
    );
    assert_eq!(m2.chat_calls(), 1, "recovered target should absorb traffic");
    assert!(second_errors.is_empty());
}

#[tokio::test]
async fn dispatch_chat_probes_during_cooldown_not_after() {
    // The whole point of periodic probing: a probe is allowed *while the
    // target is still cooling down* (long `generic_secs` cooldown, short
    // `probe_interval_secs`), so a target that recovers mid-cooldown is
    // discovered early instead of staying skipped until the full window
    // elapses.
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    // First request: `first` fails (retryable) → circuit opens with a long
    // cooldown. Immediately after, `first` has actually recovered.
    m1.push_chat(ChatBehavior::Retryable(AppError::Internal("fail".into())));
    m1.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    let cb = CircuitBreakerConfig {
        quota_exhausted_secs: 3600,
        rate_limited_secs: 10,
        generic_secs: 3600,     // long cooldown — would stay skipped for 1h
        probe_interval_secs: 0, // but a probe is due immediately
    };
    let state = build_state_with_cb(vec![m1.clone(), m2.clone()], cb).await;
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

    // Second request arrives while the 1h cooldown is still in effect — but
    // a periodic probe is due, hits the recovered `first`, and restores it.
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
        "periodic probe during cooldown should hit recovered first target"
    );
    assert_eq!(m2.chat_calls(), 1, "recovered target absorbs traffic");
    assert!(second_errors.is_empty());
}

#[tokio::test]
async fn dispatch_chat_half_open_probe_failure_reschedules() {
    // `first` keeps failing. Because probes are allowed every interval, each
    // request probes `first` once more; a failed probe reschedules the next
    // probe (it does NOT re-open the whole cooldown), so further requests
    // keep falling through to `second` until `first` recovers.
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_chat(ChatBehavior::Retryable(AppError::Internal("fail1".into())));
    m1.push_chat(ChatBehavior::Retryable(AppError::Internal("fail2".into())));
    m1.push_chat(ChatBehavior::Retryable(AppError::Internal("fail3".into())));
    m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    let cb = CircuitBreakerConfig {
        quota_exhausted_secs: 0,
        rate_limited_secs: 0,
        generic_secs: 0,
        probe_interval_secs: 0,
    };
    let state = build_state_with_cb(vec![m1.clone(), m2.clone()], cb).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

    let mut first_errors = Vec::new();
    let _first = dispatch_chat(make_ctx(
        &state,
        &rt,
        &req,
        &targets,
        false,
        &mut first_errors,
    ))
    .await;
    assert_eq!(m1.chat_calls(), 1);
    assert_eq!(m2.chat_calls(), 1);

    let mut probe_errors = Vec::new();
    let _probe = dispatch_chat(make_ctx(
        &state,
        &rt,
        &req,
        &targets,
        false,
        &mut probe_errors,
    ))
    .await;
    // Probe hits first again (failure) → re-opens; second still serves.
    assert_eq!(m1.chat_calls(), 2, "half-open probe should retry first");
    assert_eq!(m2.chat_calls(), 2);

    let mut third_errors = Vec::new();
    let third = dispatch_chat(make_ctx(
        &state,
        &rt,
        &req,
        &targets,
        false,
        &mut third_errors,
    ))
    .await;
    // Cooldown (0s) elapsed again → another probe; first still failing →
    // falls through again.
    assert_eq!(third.status(), StatusCode::OK);
    assert_eq!(m1.chat_calls(), 3);
    assert_eq!(m2.chat_calls(), 3);
}

#[tokio::test]
async fn dispatch_chat_releases_probe_slot_when_provider_unresolved() {
    // Regression: when `circuit_allows_target` claims the half-open probe
    // slot but `resolve_provider` returns `None` (provider disabled or no
    // longer registered, e.g. after a config hot-reload), the slot must be
    // released. Otherwise `probe_in_flight` stays true and the target's
    // half-open recovery is defeated for the rest of the cooldown window.
    let m2 = Arc::new(MockProvider::new("second"));
    m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    // Only `second` is registered; `first` is referenced by the route but
    // gone from the registry, so `resolve_provider("first")` returns None.
    let cb = CircuitBreakerConfig {
        quota_exhausted_secs: 0,
        rate_limited_secs: 0,
        generic_secs: 0,
        probe_interval_secs: 0,
    };
    let state = build_state_with_cb(vec![m2.clone()], cb).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

    // Pre-open `first`'s circuit so the next request finds it probe-due
    // and claims the slot before `resolve_provider` fails.
    rt.circuit_breaker.write().await.open(
        sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o"),
        0,
        0,
    );

    let mut errors = Vec::new();
    let resp = dispatch_chat(make_ctx(&state, &rt, &req, &targets, false, &mut errors)).await;
    assert_eq!(resp.status(), StatusCode::OK, "falls through to second");
    assert_eq!(m2.chat_calls(), 1);

    // The slot must have been released: the target is probeable again and
    // the single probe slot can be claimed once more.
    let mut b = rt.circuit_breaker.write().await;
    assert!(
        b.to_half_open_if_due(&sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o")),
        "after release, target should be probeable again (not stuck half-open)"
    );
    assert!(
        b.try_acquire_probe(&sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o")),
        "released probe slot should be claimable again"
    );
}

#[tokio::test]
async fn dispatch_chat_half_open_probe_in_flight_skips_others() {
    // When a target is half-open with a probe already in flight, a
    // concurrent request must skip it and fall through to the next target
    // rather than launching a second probe.
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct SlowProbe {
        name: String,
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl LlmProvider for SlowProbe {
        fn name(&self) -> &str {
            &self.name
        }
        async fn chat(
            &self,
            _r: &ChatCompletionRequest,
            _m: &str,
            _e: &str,
            _t: &HashMap<String, serde_json::Value>,
            _to: u64,
        ) -> sag_core::Result<ChatCompletionResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Simulate a slow upstream so the probe is "in flight" while a
            // second request arrives.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            Ok(build_response("gpt-4o", None))
        }
        async fn chat_stream(
            &self,
            _r: &ChatCompletionRequest,
            _m: &str,
            _e: &str,
            _t: &HashMap<String, serde_json::Value>,
            _to: u64,
        ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
            Ok(Box::pin(futures_util::stream::empty()))
        }
    }

    let m1 = Arc::new(SlowProbe {
        name: "first".into(),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let m2 = Arc::new(MockProvider::new("second"));
    m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    let cb = CircuitBreakerConfig {
        quota_exhausted_secs: 0,
        rate_limited_secs: 0,
        generic_secs: 0,
        probe_interval_secs: 0,
    };
    let state = build_state_with_cb(vec![m1.clone(), m2.clone()], cb).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

    // Pre-open the circuit for `first` so the next concurrent requests
    // find it half-open and race for the single probe slot.
    rt.circuit_breaker.write().await.open(
        sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o"),
        0,
        0,
    );

    let mut e1 = Vec::new();
    let mut e2 = Vec::new();
    let r1 = dispatch_chat(make_ctx(&state, &rt, &req, &targets, false, &mut e1));
    let r2 = dispatch_chat(make_ctx(&state, &rt, &req, &targets, false, &mut e2));
    let (a, b) = tokio::join!(r1, r2);
    assert_eq!(a.status(), StatusCode::OK);
    assert_eq!(b.status(), StatusCode::OK);
    // Exactly one probe should have hit `first`; the other must have been
    // skipped (probe in flight) and served by `second`.
    assert_eq!(
        m1.calls.load(Ordering::SeqCst),
        1,
        "only one half-open probe should hit first"
    );
    assert_eq!(
        m2.chat_calls(),
        1,
        "skipped request falls through to second"
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
    // A provider-persistent non-retryable error (non-quota 403) must
    // fail over without opening the circuit. BadRequest is deliberately
    // NOT used here: since it is classified client-caused, it returns
    // straight to the client and never reaches this path — see
    // `client_error_bad_request_returns_to_client_without_failover`.
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_chat(ChatBehavior::NonRetryable(AppError::Upstream {
        provider: "first".into(),
        status: 403,
        body: "permission denied".into(),
    }));
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
async fn client_error_4xx_returns_to_client_without_failover() {
    // A 400 (client-caused 4xx) must be returned straight to the client:
    // no retry on the failing provider, no failover to the next target,
    // no pin movement, no circuit change. (Pre-policy this failed over
    // and B's success would have repinned every user onto B.)
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_chat(ChatBehavior::NonRetryable(AppError::Upstream {
        provider: "first".into(),
        status: 400,
        body: "unsupported parameter".into(),
    }));
    // m2 would succeed if ever called — it must not be.
    m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    let state = build_state(vec![m1.clone(), m2.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

    // Pre-pin A so "pin unchanged" is observable.
    rt.active_targets.write().await.insert(
        "gpt-4o".to_string(),
        sag_state::ActiveTargetEntry::new(
            "first",
            "gpt-4o",
            "gpt-4o",
            Some("gpt-4o".to_string()),
            std::time::Instant::now(),
        ),
    );

    let mut errors = Vec::new();
    let resp = dispatch_chat(make_ctx(&state, &rt, &req, &targets, false, &mut errors)).await;

    // 400 straight to the client — not B's 200, not a 502.
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(m1.chat_calls(), 1, "client 4xx should not retry A");
    assert_eq!(m2.chat_calls(), 0, "client 4xx must not failover to B");

    // Pin unchanged (still A).
    let active = rt.active_targets.read().await;
    assert_eq!(
        active.get("gpt-4o").map(|e| e.provider.as_str()),
        Some("first"),
        "client-caused 4xx must not move the pin"
    );

    // Circuit stays closed.
    let key = sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o");
    assert!(
        rt.circuit_breaker.read().await.is_closed(&key),
        "client-caused 4xx must not open the circuit"
    );
}

#[tokio::test]
async fn unsupported_param_422_does_not_move_pin() {
    // Regression: a provider rejecting an unsupported parameter (422)
    // must not move the shared sticky pin — pre-policy every non-
    // retryable 4xx failed over and B's success would have repinned
    // all users from A to B for one client's bad parameter.
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_chat(ChatBehavior::NonRetryable(AppError::Upstream {
        provider: "first".into(),
        status: 422,
        body: "unsupported parameter 'foo'".into(),
    }));
    m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    let state = build_state(vec![m1.clone(), m2.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

    // pin = A
    rt.active_targets.write().await.insert(
        "gpt-4o".to_string(),
        sag_state::ActiveTargetEntry::new(
            "first",
            "gpt-4o",
            "gpt-4o",
            Some("gpt-4o".to_string()),
            std::time::Instant::now(),
        ),
    );

    let mut errors = Vec::new();
    let resp = dispatch_chat(make_ctx(&state, &rt, &req, &targets, false, &mut errors)).await;

    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(m1.chat_calls(), 1);
    assert_eq!(
        m2.chat_calls(),
        0,
        "B would have succeeded — must not be tried"
    );

    let active = rt.active_targets.read().await;
    assert_eq!(
        active.get("gpt-4o").map(|e| e.provider.as_str()),
        Some("first"),
        "pin must stay on A — B must not become the shared active target"
    );
}

#[tokio::test]
async fn client_error_bad_request_returns_to_client_without_failover() {
    // Regression: a locally-raised `AppError::BadRequest` (request
    // input / parameter error) must be classified as client-caused —
    // returned straight to the client, no retry, no failover, no pin
    // movement, no circuit change. The formal event must be
    // `client_error` with reason `invalid_request` (no route identity,
    // no target attempts), NOT model_failure. Pre-policy BadRequest
    // fell through the client-caused predicate, failed over, and —
    // if every target failed — inflated the model-failure rate.
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_chat(ChatBehavior::NonRetryable(AppError::BadRequest(
        "unsupported parameter: temperature=-1".into(),
    )));
    // m2 would succeed if ever called — it must not be.
    m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    let (state, pool) = build_state_with_event_pool(vec![m1.clone(), m2.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

    // Pre-pin A so "pin unchanged" is observable.
    rt.active_targets.write().await.insert(
        "gpt-4o".to_string(),
        sag_state::ActiveTargetEntry::new(
            "first",
            "gpt-4o",
            "gpt-4o",
            Some("gpt-4o".to_string()),
            std::time::Instant::now(),
        ),
    );

    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
    let trace_id = ctx.trace_id;
    let resp = dispatch_chat(ctx).await;

    // 400 straight to the client — not B's 200, not a 502.
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(m1.chat_calls(), 1, "BadRequest should not retry A");
    assert_eq!(m2.chat_calls(), 0, "BadRequest must not failover to B");

    // Pin unchanged (still A).
    let active = rt.active_targets.read().await;
    assert_eq!(
        active.get("gpt-4o").map(|e| e.provider.as_str()),
        Some("first"),
        "client-caused BadRequest must not move the pin"
    );

    // Circuit stays closed.
    let key = sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o");
    assert!(
        rt.circuit_breaker.read().await.is_closed(&key),
        "client-caused BadRequest must not open the circuit"
    );

    // Formal event: `client_error` / `invalid_request`, no route
    // identity, no target attempts — and definitely not model_failure.
    state.event_collector.shutdown().await;

    let (final_status, failure_reason, route_id): (String, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT final_status, failure_reason, route_id \
             FROM request_results WHERE request_id = ?",
        )
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .expect("client_error fact must be persisted");
    assert_eq!(final_status, "client_error");
    assert_eq!(
        failure_reason.as_deref(),
        Some("invalid_request"),
        "BadRequest carries no payload-too-large signal — reason must be invalid_request"
    );
    assert!(
        route_id.is_none(),
        "client_error event must carry no route identity"
    );

    let attempt_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM target_attempts WHERE request_id = ?")
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        attempt_count, 0,
        "client_error event must carry no target attempts"
    );
}

#[tokio::test]
async fn auth_401_failovers_and_moves_pin() {
    // Regression: a 401 (invalid provider key — provider-persistent,
    // not the client's fault) must still failover and move the pin to
    // the working backup, without opening A's circuit.
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_chat(ChatBehavior::NonRetryable(AppError::Upstream {
        provider: "first".into(),
        status: 401,
        body: "invalid api key".into(),
    }));
    m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    let state = build_state(vec![m1.clone(), m2.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

    // pin = A
    rt.active_targets.write().await.insert(
        "gpt-4o".to_string(),
        sag_state::ActiveTargetEntry::new(
            "first",
            "gpt-4o",
            "gpt-4o",
            Some("gpt-4o".to_string()),
            std::time::Instant::now(),
        ),
    );

    let mut errors = Vec::new();
    let resp = dispatch_chat(make_ctx_with_token(
        &state,
        &rt,
        &req,
        &targets,
        &mut errors,
        sag_state::StickyToken::new(0, 0, state.config.routes_version()),
    ))
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(m1.chat_calls(), 1);
    assert_eq!(m2.chat_calls(), 1);

    // Pin moved to B (B succeeded → sticky writeback).
    let active = rt.active_targets.read().await;
    assert_eq!(
        active.get("gpt-4o").map(|e| e.provider.as_str()),
        Some("second"),
        "provider-persistent 401 should move the pin to the working backup"
    );

    // A's circuit stays closed (no open on non-retryable 4xx).
    let key = sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o");
    assert!(
        rt.circuit_breaker.read().await.is_closed(&key),
        "401 must not open the circuit"
    );
}

#[tokio::test]
async fn model_404_failovers_and_moves_pin() {
    // Regression: a 404 (model removed upstream — provider-persistent)
    // must still failover and move the pin, without opening the circuit.
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_chat(ChatBehavior::NonRetryable(AppError::Upstream {
        provider: "first".into(),
        status: 404,
        body: "model not found".into(),
    }));
    m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    let state = build_state(vec![m1.clone(), m2.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

    // pin = A
    rt.active_targets.write().await.insert(
        "gpt-4o".to_string(),
        sag_state::ActiveTargetEntry::new(
            "first",
            "gpt-4o",
            "gpt-4o",
            Some("gpt-4o".to_string()),
            std::time::Instant::now(),
        ),
    );

    let mut errors = Vec::new();
    let resp = dispatch_chat(make_ctx_with_token(
        &state,
        &rt,
        &req,
        &targets,
        &mut errors,
        sag_state::StickyToken::new(0, 0, state.config.routes_version()),
    ))
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(m1.chat_calls(), 1);
    assert_eq!(m2.chat_calls(), 1);

    let active = rt.active_targets.read().await;
    assert_eq!(
        active.get("gpt-4o").map(|e| e.provider.as_str()),
        Some("second"),
        "provider-persistent 404 should move the pin to the working backup"
    );

    let key = sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o");
    assert!(
        rt.circuit_breaker.read().await.is_closed(&key),
        "404 must not open the circuit"
    );
}

#[tokio::test]
async fn quota_403_opens_circuit() {
    // A 403 whose body signals quota exhaustion is retryable: with the
    // retry budget spent, A's circuit opens with the quota cooldown
    // (default 3600s) and the request fails over to B.
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_chat(ChatBehavior::Retryable(AppError::Upstream {
        provider: "first".into(),
        status: 403,
        body: "quota exceeded".into(),
    }));
    m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    // Default circuit_breaker config: quota_exhausted_secs = 3600.
    let state = build_state(vec![m1.clone(), m2.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

    let mut errors = Vec::new();
    let resp = dispatch_chat(make_ctx(&state, &rt, &req, &targets, false, &mut errors)).await;

    assert_eq!(resp.status(), StatusCode::OK, "fails over to B");
    assert_eq!(m1.chat_calls(), 1);
    assert_eq!(m2.chat_calls(), 1);

    // A's circuit opened with the quota cooldown (~3600s).
    let key = sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o");
    let breaker = rt.circuit_breaker.read().await;
    assert!(!breaker.is_closed(&key), "quota 403 must open the circuit");
    let snap = breaker.snapshot();
    let entry = snap
        .iter()
        .find(|e| e.provider == "first")
        .expect("first must have a circuit entry");
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let remaining = entry.cooldown_until_secs.saturating_sub(now_secs);
    assert!(
        (3500..=3600).contains(&remaining),
        "quota cooldown should be ~3600s, got {remaining}s remaining"
    );
}

#[tokio::test]
async fn non_quota_403_failovers_and_moves_pin() {
    // A 403 without quota keywords (perms / region / content policy) is
    // a provider-persistent failure: failover + move pin + no circuit.
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_chat(ChatBehavior::NonRetryable(AppError::Upstream {
        provider: "first".into(),
        status: 403,
        body: "permission denied".into(),
    }));
    m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    let state = build_state(vec![m1.clone(), m2.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

    // pin = A
    rt.active_targets.write().await.insert(
        "gpt-4o".to_string(),
        sag_state::ActiveTargetEntry::new(
            "first",
            "gpt-4o",
            "gpt-4o",
            Some("gpt-4o".to_string()),
            std::time::Instant::now(),
        ),
    );

    let mut errors = Vec::new();
    let resp = dispatch_chat(make_ctx_with_token(
        &state,
        &rt,
        &req,
        &targets,
        &mut errors,
        sag_state::StickyToken::new(0, 0, state.config.routes_version()),
    ))
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(m1.chat_calls(), 1);
    assert_eq!(m2.chat_calls(), 1);

    let active = rt.active_targets.read().await;
    assert_eq!(
        active.get("gpt-4o").map(|e| e.provider.as_str()),
        Some("second"),
        "non-quota 403 should move the pin to the working backup"
    );

    let key = sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o");
    assert!(
        rt.circuit_breaker.read().await.is_closed(&key),
        "non-quota 403 must not open the circuit"
    );
}

#[tokio::test]
async fn unknown_4xx_returns_to_client() {
    // Unknown 4xx (418) defaults to client-caused: return to the client,
    // no failover, no pin movement, no circuit change.
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_chat(ChatBehavior::NonRetryable(AppError::Upstream {
        provider: "first".into(),
        status: 418,
        body: "i'm a teapot".into(),
    }));
    m2.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    let state = build_state(vec![m1.clone(), m2.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];

    // pin = A
    rt.active_targets.write().await.insert(
        "gpt-4o".to_string(),
        sag_state::ActiveTargetEntry::new(
            "first",
            "gpt-4o",
            "gpt-4o",
            Some("gpt-4o".to_string()),
            std::time::Instant::now(),
        ),
    );

    let mut errors = Vec::new();
    let resp = dispatch_chat(make_ctx(&state, &rt, &req, &targets, false, &mut errors)).await;

    assert_eq!(resp.status(), StatusCode::IM_A_TEAPOT);
    assert_eq!(m1.chat_calls(), 1);
    assert_eq!(m2.chat_calls(), 0, "unknown 4xx must not failover");

    let active = rt.active_targets.read().await;
    assert_eq!(
        active.get("gpt-4o").map(|e| e.provider.as_str()),
        Some("first"),
        "unknown 4xx must not move the pin"
    );

    let key = sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o");
    assert!(
        rt.circuit_breaker.read().await.is_closed(&key),
        "unknown 4xx must not open the circuit"
    );
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

    // Pre-record "second" as the active target (which has priority 5,
    // lower than "first"'s 10). The route targets are passthrough
    // (`real_model: None`), so the pin records `target_real_model: None`.
    {
        let mut active = rt.active_targets.write().await;
        active.insert(
            "gpt-4o".to_string(),
            sag_state::ActiveTargetEntry::new(
                "second",
                "gpt-4o",
                "gpt-4o",
                None,
                std::time::Instant::now(),
            ),
        );
    }

    let trace_id = Uuid::new_v4();
    let start = Instant::now();

    // Resolve route (should place "second" first due to sticky active target)
    let ResolvedRoute {
        targets: resolved_targets,
        api_endpoint,
        strategy,
        sticky_token,
        route_name,
        started_at,
    } = crate::dispatch::resolve_route(
        &state,
        &rt,
        &req,
        trace_id,
        start,
        chrono::Utc::now(),
        false,
        None,
        None,
    )
    .await
    .unwrap();

    assert_eq!(resolved_targets[0].provider, "second");
    assert_eq!(
        route_name, "gpt-4o",
        "route carrier must record the matched route model_name"
    );

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
        route_name: &route_name,
        started_at,
        errors: &mut errors,
        strategy,
        sticky_token,
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
    let ResolvedRoute {
        targets: resolved_targets,
        api_endpoint,
        strategy,
        sticky_token,
        route_name,
        started_at,
    } = crate::dispatch::resolve_route(
        &state,
        &rt,
        &req,
        trace_id,
        start,
        chrono::Utc::now(),
        false,
        None,
        None,
    )
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
        route_name: &route_name,
        started_at,
        errors: &mut errors,
        strategy,
        sticky_token,
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
        assert_eq!(active_target.provider, "second");
    }
}

#[tokio::test]
async fn stale_writeback_after_clear_does_not_repin() {
    // Regression for the DELETE /api/sticky-targets race: a request that
    // resolved its route *before* the clear must not pin its target on
    // success afterwards. The writeback is conditional on the generation
    // captured at decision time.
    let m1 = Arc::new(MockProvider::new("first"));
    m1.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    let state = build_state_with_strategy(vec![m1.clone()], Strategy::StickyFailover).await;

    // Share the active_targets Arc between AppState and DispatchRuntime,
    // mirroring production wiring — this is the "shared runtime" a stale
    // writeback would corrupt.
    let rt = DispatchRuntime::with_shared_state(
        state.circuit_breaker.clone(),
        state.active_targets.clone(),
        state.fill_first_orders.clone(),
    );
    let req = build_request("gpt-4o", false);
    let trace_id = Uuid::new_v4();
    let start = Instant::now();

    // Routing decision happens now: captures the current generation (0).
    let ResolvedRoute {
        targets: resolved_targets,
        api_endpoint,
        strategy,
        sticky_token,
        route_name,
        started_at,
    } = crate::dispatch::resolve_route(
        &state,
        &rt,
        &req,
        trace_id,
        start,
        chrono::Utc::now(),
        false,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(sticky_token.generation, 0);
    assert_eq!(
        sticky_token.routes_version,
        state.config.routes_version(),
        "token must carry the routes_version captured at decision time"
    );

    // DELETE /api/sticky-targets lands between decision and writeback.
    rt.active_targets.write().await.clear("gpt-4o");

    // Dispatch succeeds, but the stale token must block the pin.
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
        route_name: &route_name,
        started_at,
        errors: &mut errors,
        strategy,
        sticky_token,
        user_agent: None,
        client_ip: None,
    };
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        rt.active_targets.read().await.get("gpt-4o").is_none(),
        "stale writeback after a clear must not re-pin the deleted target"
    );
}

#[tokio::test]
async fn stale_writeback_after_config_mutation_does_not_repin() {
    // Regression: a request whose route decision predates a config
    // mutation (route / provider CRUD, hot-reload) must not pin its
    // target on success afterwards — even when the model never had an
    // entry or generation, which the per-model generation alone cannot
    // invalidate. The `routes_version` component of the token closes
    // that window.
    let m1 = Arc::new(MockProvider::new("first"));
    m1.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    m1.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    let state = build_state_with_strategy(vec![m1.clone()], Strategy::StickyFailover).await;
    let rt = DispatchRuntime::with_shared_state(
        state.circuit_breaker.clone(),
        state.active_targets.clone(),
        state.fill_first_orders.clone(),
    );
    let req = build_request("gpt-4o", false);
    let trace_id = Uuid::new_v4();
    let start = Instant::now();

    // Routing decision happens now: captures (generation 0, version V).
    let ResolvedRoute {
        targets: resolved_targets,
        api_endpoint,
        strategy,
        sticky_token,
        route_name,
        started_at,
    } = crate::dispatch::resolve_route(
        &state,
        &rt,
        &req,
        trace_id,
        start,
        chrono::Utc::now(),
        false,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(sticky_token.generation, 0, "never had an entry");

    // A config mutation lands between decision and writeback: re-create
    // the route (any route change moves routes_version).
    {
        let mut cfg = state.config.write().await;
        cfg.routes.retain(|r| r.model_name != "gpt-4o");
        cfg.routes.push(sag_config::RouteConfig {
            model_name: "gpt-4o".into(),
            api_endpoint: "/chat/completions".into(),
            retries: None,
            timeout_secs: None,
            strategy: sag_config::Strategy::StickyFailover,
            targets: vec![sag_config::RouteTarget {
                provider: "first".into(),
                real_model: Some("gpt-4o".into()),
                priority: 10,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
            }],
        });
        state.config.bump_routes_version();
    }
    assert!(state.config.routes_version() > sticky_token.routes_version);

    // Dispatch succeeds, but the stale routes_version must block the pin.
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
        route_name: &route_name,
        started_at,
        errors: &mut errors,
        strategy,
        sticky_token,
        user_agent: None,
        client_ip: None,
    };
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        rt.active_targets.read().await.get("gpt-4o").is_none(),
        "stale writeback after a config mutation must not re-pin"
    );

    // The entry recorded by the writeback path carries the route
    // identity (route_model + target spec) used by prune / admin.
    // Sanity: a fresh decision after the mutation captures the new
    // version and pins normally.
    let ResolvedRoute {
        targets: resolved_targets2,
        api_endpoint: api_endpoint2,
        strategy: strategy2,
        sticky_token: token2,
        route_name,
        started_at,
    } = crate::dispatch::resolve_route(
        &state,
        &rt,
        &req,
        trace_id,
        start,
        chrono::Utc::now(),
        false,
        None,
        None,
    )
    .await
    .unwrap();
    let mut errors2 = Vec::new();
    let ctx2 = DispatchCtx {
        state: &state,
        runtime: &rt,
        request: &req,
        targets: &resolved_targets2,
        api_endpoint: &api_endpoint2,
        is_stream: false,
        route_name: &route_name,
        started_at,
        trace_id,
        start,
        errors: &mut errors2,
        strategy: strategy2,
        sticky_token: token2,
        user_agent: None,
        client_ip: None,
    };
    let resp2 = dispatch_chat(ctx2).await;
    assert_eq!(resp2.status(), StatusCode::OK);
    let active = rt.active_targets.read().await;
    let entry = active.get("gpt-4o").expect("fresh decision must pin");
    assert_eq!(entry.provider, "first");
    assert_eq!(entry.route_model, "gpt-4o");
    assert_eq!(entry.target_real_model.as_deref(), Some("gpt-4o"));
}

#[tokio::test]
async fn stale_writeback_after_coordinator_mutation_does_not_repin() {
    // Same guarantee as `stale_writeback_after_config_mutation_does_not_repin`
    // but the config mutation goes through the production
    // `ConfigMutationCoordinator` (config.write() with the version bump
    // atomic inside the guard, then providers.write()) — the exact path
    // the sticky writeback's config read guard serializes with. A
    // mutation between decision and writeback must invalidate the pin.
    let m1 = Arc::new(MockProvider::new("first"));
    m1.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    let state = build_state_with_strategy(vec![m1.clone()], Strategy::StickyFailover).await;
    let rt = DispatchRuntime::with_shared_state(
        state.circuit_breaker.clone(),
        state.active_targets.clone(),
        state.fill_first_orders.clone(),
    );
    let req = build_request("gpt-4o", false);
    let trace_id = Uuid::new_v4();
    let start = Instant::now();

    // Routing decision happens now: captures (generation 0, version V).
    let ResolvedRoute {
        targets: resolved_targets,
        api_endpoint,
        strategy,
        sticky_token,
        route_name,
        started_at,
    } = crate::dispatch::resolve_route(
        &state,
        &rt,
        &req,
        trace_id,
        start,
        chrono::Utc::now(),
        false,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(sticky_token.generation, 0, "never had an entry");

    // A config mutation lands between decision and writeback, through
    // the real coordinator (no-op route update still bumps the version).
    {
        let cfg = state.config.read().await;
        let existing = cfg
            .routes
            .iter()
            .find(|r| r.model_name == "gpt-4o")
            .cloned()
            .expect("route must exist");
        drop(cfg);
        state
            .coordinator
            .update_route("gpt-4o", existing)
            .await
            .expect("coordinator update must succeed");
    }
    assert!(
        state.config.routes_version() > sticky_token.routes_version,
        "the coordinator mutation must have bumped routes_version"
    );

    // Dispatch succeeds, but the stale routes_version must block the pin.
    let mut errors = Vec::new();
    let ctx = DispatchCtx {
        state: &state,
        runtime: &rt,
        request: &req,
        targets: &resolved_targets,
        route_name: &route_name,
        started_at,
        api_endpoint: &api_endpoint,
        is_stream: false,
        trace_id,
        start,
        errors: &mut errors,
        strategy,
        sticky_token,
        user_agent: None,
        client_ip: None,
    };
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        rt.active_targets.read().await.get("gpt-4o").is_none(),
        "stale writeback after a coordinator mutation must not re-pin"
    );
}

#[tokio::test]
async fn dispatch_streaming_empty_response_retries_then_succeeds() {
    // First attempt: empty response (Data with ct=0 + finish_reason=stop)
    // Second attempt: normal content response
    let mock = Arc::new(MockProvider::new("openai"));
    mock.push_stream(StreamBehavior::Ok(vec![StreamChunk::Data(
        b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"total_tokens\":100}}\n\n".to_vec(),
    )]));
    mock.push_stream(StreamBehavior::Ok(vec![
        StreamChunk::Data(b"data: hello\n\n".to_vec()),
        StreamChunk::Usage(sag_core::types::Usage {
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
    let targets = vec![target("openai", "gpt-4o", 1)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(mock.stream_calls(), 2, "should retry after empty response");

    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&body[..], b"data: hello\n\n");
}

#[tokio::test]
async fn dispatch_streaming_empty_response_falls_through_to_next_target() {
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    // First provider returns empty response
    m1.push_stream(StreamBehavior::Ok(vec![StreamChunk::Data(
        b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"total_tokens\":100}}\n\n".to_vec(),
    )]));
    // Second provider returns normal content
    m2.push_stream(StreamBehavior::Ok(vec![
        StreamChunk::Data(b"data: hello\n\n".to_vec()),
        StreamChunk::Done,
    ]));
    let state = build_state(vec![m1.clone(), m2.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", true);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(m1.stream_calls(), 1, "first provider tried once");
    assert_eq!(m2.stream_calls(), 1, "should fall through to second");

    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(&body[..], b"data: hello\n\n");
}

#[tokio::test]
async fn dispatch_non_streaming_empty_response_retries_then_succeeds() {
    let mock = Arc::new(MockProvider::new("openai"));
    // First attempt: ct=0, no content in choices → empty
    mock.push_chat(ChatBehavior::Ok(build_response(
        "gpt-4o",
        Some(sag_core::types::Usage {
            prompt_tokens: 100,
            completion_tokens: 0,
            total_tokens: 100,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        }),
    )));
    // Second attempt: ct>0 with content → success
    mock.push_chat(ChatBehavior::Ok(build_response_with_content(
        "gpt-4o",
        Some(sag_core::types::Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        }),
        "hello",
    )));
    let state = build_state(vec![mock.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("openai", "gpt-4o", 1)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(mock.chat_calls(), 2, "should retry after ct=0 response");
}

#[tokio::test]
async fn dispatch_streaming_empty_resp_disabled_passes_through() {
    // retry_on_empty_response = false → ct=0 stream passes through
    let mock = Arc::new(MockProvider::new("openai"));
    mock.push_stream(StreamBehavior::Ok(vec![
        StreamChunk::Data(
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"total_tokens\":100}}\n\n"
                .to_vec(),
        ),
        StreamChunk::Done,
    ]));
    let state = build_state(vec![mock.clone()]).await;
    // Disable empty-response detection
    state.config.write().await.backoff.retry_on_empty_response = false;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", true);
    let targets = vec![target("openai", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(mock.stream_calls(), 1, "single attempt, no retry");
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert!(
        !body.is_empty(),
        "raw empty frame forwarded as-is when retry_on_empty_response=false"
    );
}

#[tokio::test]
async fn dispatch_non_streaming_empty_resp_disabled_passes_through() {
    // retry_on_empty_response = false → ct=0 non-stream response passes through
    let mock = Arc::new(MockProvider::new("openai"));
    mock.push_chat(ChatBehavior::Ok(build_response(
        "gpt-4o",
        Some(sag_core::types::Usage {
            prompt_tokens: 100,
            completion_tokens: 0,
            total_tokens: 100,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        }),
    )));
    let state = build_state(vec![mock.clone()]).await;
    // Disable empty-response detection
    state.config.write().await.backoff.retry_on_empty_response = false;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("openai", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(mock.chat_calls(), 1, "single attempt, no retry");
}
