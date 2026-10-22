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
use sag_core::IrStreamEvent;
use sag_core::types::{ChatCompletionRequest, ChatCompletionResponse, StreamChunk, Usage};
use sag_providers::{ChatCompletionsCodec, LlmProvider};
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
    /// Yields the chunks, then suspends forever (never ends): models the
    /// upstream FIN tail-latency window where the client-visible terminal
    /// frame has already been forwarded but the upstream `End` has not
    /// arrived yet (terminal-frame race tests).
    OkThenPending(Vec<StreamChunk>),
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
            Some(StreamBehavior::OkThenPending(chunks)) => Ok(Box::pin(
                futures_util::stream::iter(chunks).chain(futures_util::stream::pending()),
            )),
            Some(StreamBehavior::Err(e)) => Err(e),
            None => Ok(Box::pin(futures_util::stream::empty())),
        }
    }
}

/// Provider serving a **native Anthropic** SSE stream (the Chat-ingress
/// `[DONE]` bridge / translated-egress tests). Only
/// `anthropic_messages_stream` is used; the Chat-shaped calls are
/// unreachable in those tests and panic if ever taken.
struct AnthropicStreamProvider {
    name: String,
    stream: Mutex<Option<Vec<StreamChunk>>>,
    stream_calls: AtomicU32,
}

impl AnthropicStreamProvider {
    fn new(name: &str, chunks: Vec<StreamChunk>) -> Self {
        Self {
            name: name.to_string(),
            stream: Mutex::new(Some(chunks)),
            stream_calls: AtomicU32::new(0),
        }
    }

    fn stream_calls(&self) -> u32 {
        self.stream_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmProvider for AnthropicStreamProvider {
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
        unreachable!("AnthropicStreamProvider only serves streaming requests")
    }

    async fn chat_stream(
        &self,
        _request: &ChatCompletionRequest,
        _real_model: &str,
        _api_endpoint: &str,
        _target_extra: &HashMap<String, serde_json::Value>,
        _timeout_secs: u64,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        unreachable!("AnthropicStreamProvider only serves anthropic_messages_stream")
    }

    async fn anthropic_messages_stream(
        &self,
        _body: serde_json::Value,
        _target_extra: &HashMap<String, serde_json::Value>,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        let chunks = self.stream.lock().unwrap().take().unwrap_or_default();
        // `chain(pending)` keeps the stream open after the last frame so the
        // test can drop the body inside the race window (the upstream `End`
        // has not been enqueued yet).
        Ok(Box::pin(
            futures_util::stream::iter(chunks).chain(futures_util::stream::pending()),
        ))
    }
}

/// Provider serving a **native Responses** SSE stream (Chat-ingress
/// `[DONE]` bridge / Responses-ingress bridge tests). Only
/// `responses_stream_native` is used; the Chat-shaped calls are unreachable
/// in those tests and panic if ever taken.
struct ResponsesStreamProvider {
    name: String,
    stream: Mutex<Option<Vec<StreamChunk>>>,
    stream_calls: AtomicU32,
}

impl ResponsesStreamProvider {
    fn new(name: &str, chunks: Vec<StreamChunk>) -> Self {
        Self {
            name: name.to_string(),
            stream: Mutex::new(Some(chunks)),
            stream_calls: AtomicU32::new(0),
        }
    }

    fn stream_calls(&self) -> u32 {
        self.stream_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmProvider for ResponsesStreamProvider {
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
        unreachable!("ResponsesStreamProvider only serves streaming requests")
    }

    async fn chat_stream(
        &self,
        _request: &ChatCompletionRequest,
        _real_model: &str,
        _api_endpoint: &str,
        _target_extra: &HashMap<String, serde_json::Value>,
        _timeout_secs: u64,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        unreachable!("ResponsesStreamProvider only serves responses_stream_native")
    }

    async fn responses_stream_native(
        &self,
        _body: serde_json::Value,
        _target_extra: &HashMap<String, serde_json::Value>,
        _api_endpoint: &str,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = StreamChunk> + Send>>> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        let chunks = self.stream.lock().unwrap().take().unwrap_or_default();
        Ok(Box::pin(futures_util::stream::iter(chunks)))
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
        ingress_protocol: sag_core::IngressProtocol::ChatCompletions,
        ir: None,
        entry_loss: Default::default(),
        session_key: None,
        session_writeback: None,
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
        ingress_protocol: sag_core::IngressProtocol::ChatCompletions,
        ir: None,
        entry_loss: Default::default(),
        session_key: None,
        session_writeback: None,
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

// ─── Fix B.2: upstream errors delivered inside HTTP 200 (non-streaming) ─
//
// The provider adapters surface a 200 body carrying a top-level `error`
// object as `AppError::Upstream` (502; 429 when rate-limit-ish). These tests
// cover how dispatch classifies and fails over that error.

/// A 200-delivered upstream error (surfaced by the provider as
/// `Upstream { status: 502 }`) is a failed attempt: the next target is
/// tried and the first target's attempt records `provider_error`.
#[tokio::test]
async fn dispatch_chat_200_error_body_fails_over_to_next_target() {
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_chat(ChatBehavior::Retryable(AppError::Upstream {
        provider: "first".into(),
        status: 502,
        body: "upstream burst error".into(),
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
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "fallback success is route success"
    );
    assert_eq!(m1.chat_calls(), 1, "first target attempted once");
    assert_eq!(m2.chat_calls(), 1, "second target must be tried (failover)");

    state.event_collector.shutdown().await;

    let final_status: String =
        sqlx::query_scalar("SELECT final_status FROM request_results WHERE request_id = ?")
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(final_status, "success");

    let (provider, status, reason): (String, String, Option<String>) = sqlx::query_as(
        "SELECT rt.provider, t.status, t.failure_reason \
         FROM target_attempts t \
         JOIN analytics_route_targets rt ON rt.target_id = t.target_id \
         WHERE t.request_id = ? ORDER BY t.started_at",
    )
    .bind(trace_id.to_string())
    .fetch_all(&pool)
    .await
    .unwrap()
    .into_iter()
    .next()
    .expect("first target attempt row");
    assert_eq!(provider, "first");
    assert_eq!(status, "failure");
    assert_eq!(
        reason.as_deref(),
        Some("provider_error"),
        "200-delivered error (non rate-limit) → provider_error"
    );
}

/// A rate-limit-ish error object (surfaced by the provider as
/// `Upstream { status: 429 }`) maps the failed attempt to `rate_limited`.
#[tokio::test]
async fn dispatch_chat_200_rate_limit_error_maps_to_rate_limited() {
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_chat(ChatBehavior::Retryable(AppError::Upstream {
        provider: "first".into(),
        status: 429,
        body: "burst tokens limit".into(),
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
    assert_eq!(m2.chat_calls(), 1, "failover to the next target");

    state.event_collector.shutdown().await;

    let (provider, status, reason): (String, String, Option<String>) = sqlx::query_as(
        "SELECT rt.provider, t.status, t.failure_reason \
         FROM target_attempts t \
         JOIN analytics_route_targets rt ON rt.target_id = t.target_id \
         WHERE t.request_id = ? ORDER BY t.started_at",
    )
    .bind(trace_id.to_string())
    .fetch_all(&pool)
    .await
    .unwrap()
    .into_iter()
    .next()
    .expect("first target attempt row");
    assert_eq!(provider, "first");
    assert_eq!(status, "failure");
    assert_eq!(
        reason.as_deref(),
        Some("rate_limited"),
        "rate-limit-ish 200 error → rate_limited"
    );
}

/// When every target returns a 200-delivered upstream error, the final
/// response is the standard all-exhausted envelope and preserves the
/// upstream error message.
#[tokio::test]
async fn dispatch_chat_200_error_body_all_exhausted_preserves_message() {
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    m1.push_chat(ChatBehavior::Retryable(AppError::Upstream {
        provider: "first".into(),
        status: 502,
        body: "ark burst error".into(),
    }));
    m2.push_chat(ChatBehavior::Retryable(AppError::Upstream {
        provider: "second".into(),
        status: 502,
        body: "ark burst error".into(),
    }));
    let (state, pool) = build_state_with_event_pool(vec![m1.clone(), m2.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
    let trace_id = ctx.trace_id;
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let body_str = String::from_utf8_lossy(&body).to_string();
    assert!(
        body_str.contains("ark burst error"),
        "final envelope must preserve the upstream message: {body_str}"
    );

    state.event_collector.shutdown().await;

    let (final_status, failure_reason): (String, Option<String>) = sqlx::query_as(
        "SELECT final_status, failure_reason FROM request_results WHERE request_id = ?",
    )
    .bind(trace_id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(final_status, "model_failure");
    assert_eq!(failure_reason.as_deref(), Some("all_exhausted"));
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

/// Major-3 e2e: a client disconnect on a *later* fallback target must not
/// drop the failure fact of an earlier target that already completed.
/// Target `first` fails fast; target `second` is cancelled mid-call, so
/// the single `client_cancelled` fact must carry **both** attempts —
/// `first` → `failure`, `second` → `cancelled` — in attempt order.
#[tokio::test]
#[allow(clippy::type_complexity)]
async fn dispatch_chat_cancel_on_later_target_keeps_prior_failure_facts() {
    let first = Arc::new(MockProvider::new("first"));
    first.push_chat(ChatBehavior::Retryable(AppError::Internal("boom".into())));
    let second = Arc::new(BlockingProvider::new("second"));
    let (state, pool) = build_state_with_event_pool(vec![first.clone(), second.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
    let trace_id = ctx.trace_id;

    // The second provider call never resolves; the timeout drops the
    // handler future mid-call — the client-disconnect signal.
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
    assert_eq!(stream, 0);

    // Both attempts must survive: the completed `first` failure and the
    // in-flight `second` cancel, in attempt order.
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT rt.provider, t.status \
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
        "cancel on a later target must keep the earlier completed failure fact; got {rows:?}"
    );
    assert_eq!(rows[0], ("first".to_string(), "failure".to_string()));
    assert_eq!(rows[1], ("second".to_string(), "cancelled".to_string()));
}

/// Major-3 e2e for the non-streaming success shape: after a completed
/// failure on target `first`, a client disconnect on target `second` must
/// still not downgrade or duplicate the request fact — exactly one
/// `client_cancelled` request row, and one attempt row per target (the
/// guard's mirror must never produce a second write on a normal
/// terminal).
#[tokio::test]
async fn dispatch_chat_cancel_on_later_target_writes_single_request_fact() {
    let first = Arc::new(MockProvider::new("first"));
    first.push_chat(ChatBehavior::Retryable(AppError::Internal("boom".into())));
    let second = Arc::new(BlockingProvider::new("second"));
    let (state, pool) = build_state_with_event_pool(vec![first.clone(), second.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
    let trace_id = ctx.trace_id;

    let result = tokio::time::timeout(Duration::from_millis(100), dispatch_chat(ctx)).await;
    assert!(result.is_err());
    state.event_collector.shutdown().await;

    let request_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_results WHERE request_id = ?")
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        request_rows, 1,
        "the guard's mirrored facts must never add a second request fact"
    );
}

/// Major-3 negative control: a normal successful fallback still emits the
/// loop-local facts exactly once (the guard's mirror is dropped unread
/// after `mark_done`) — no duplicated `target_attempts` rows.
#[tokio::test]
async fn dispatch_chat_successful_fallback_does_not_duplicate_attempts() {
    let first = Arc::new(MockProvider::new("first"));
    first.push_chat(ChatBehavior::Retryable(AppError::Internal("boom".into())));
    let second = Arc::new(MockProvider::new("second"));
    second.push_chat(ChatBehavior::Ok(build_response_with_content(
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
    let (state, pool) = build_state_with_event_pool(vec![first.clone(), second.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("first", "gpt-4o", 0), target("second", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
    let trace_id = ctx.trace_id;
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);
    state.event_collector.shutdown().await;

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM target_attempts WHERE request_id = ?")
        .bind(trace_id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        rows, 2,
        "a normal terminal must emit exactly one attempt row per target"
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
        sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o", ""),
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
        b.to_half_open_if_due(&sag_core::RouteTargetKey::new(
            "gpt-4o", "first", "gpt-4o", ""
        )),
        "after release, target should be probeable again (not stuck half-open)"
    );
    assert!(
        b.try_acquire_probe(&sag_core::RouteTargetKey::new(
            "gpt-4o", "first", "gpt-4o", ""
        )),
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
        sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o", ""),
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
    let key = sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o", "");
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
    let key = sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o", "");
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
    let key = sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o", "");
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

    let key = sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o", "");
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
    let key = sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o", "");
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

    let key = sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o", "");
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

    let key = sag_core::RouteTargetKey::new("gpt-4o", "first", "gpt-4o", "");
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
    let mut config_providers: HashMap<String, sag_config::ProviderConfig> = HashMap::new();
    for p in providers {
        config_providers.insert(p.name().to_string(), crate::testing::provider_config(true));
        registry.insert(p.name().to_string(), p);
    }
    // Routes may reference a fixed pair (first/second) even when the
    // fixture only registered one provider — seed the other so full
    // `GatewayConfig::validate` (run by coordinator mutations) passes.
    for name in ["first", "second"] {
        config_providers
            .entry(name.to_string())
            .or_insert_with(|| crate::testing::provider_config(true));
    }
    let config = GatewayConfig {
        gateway_auth: Some(GatewayAuthConfig {
            api_keys: vec!["k".into()],
        }),
        providers: config_providers,
        routes: vec![sag_config::RouteConfig {
            session_ttl_secs: None,
            session_max_entries: None,
            model_name: "gpt-4o".into(),
            api_endpoint: None,
            pipeline: sag_config::Pipeline::Chat,
            strategy,
            targets: vec![
                sag_config::RouteTarget {
                    provider: "first".into(),
                    real_model: None,
                    priority: 10,
                    retries: Some(0),
                    timeout_secs: None,
                    extra: HashMap::new(),
                    capabilities: None,
                },
                sag_config::RouteTarget {
                    provider: "second".into(),
                    real_model: None,
                    priority: 5,
                    retries: Some(0),
                    timeout_secs: None,
                    extra: HashMap::new(),
                    capabilities: None,
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
        pipeline: _,
        targets: resolved_targets,
        api_endpoint,
        strategy,
        sticky_token,
        session_writeback: _,
        route_name,
        started_at,
        precheck_errors: _,
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
        ingress_protocol: sag_core::IngressProtocol::ChatCompletions,
        ir: None,
        entry_loss: Default::default(),
        session_key: None,
        session_writeback: None,
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
        pipeline: _,
        targets: resolved_targets,
        api_endpoint,
        strategy,
        sticky_token,
        session_writeback: _,
        route_name,
        started_at,
        precheck_errors: _,
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
        ingress_protocol: sag_core::IngressProtocol::ChatCompletions,
        ir: None,
        entry_loss: Default::default(),
        session_key: None,
        session_writeback: None,
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
        state.session_affinity.clone(),
    );
    let req = build_request("gpt-4o", false);
    let trace_id = Uuid::new_v4();
    let start = Instant::now();

    // Routing decision happens now: captures the current generation (0).
    let ResolvedRoute {
        pipeline: _,
        targets: resolved_targets,
        api_endpoint,
        strategy,
        sticky_token,
        session_writeback: _,
        route_name,
        started_at,
        precheck_errors: _,
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
        ingress_protocol: sag_core::IngressProtocol::ChatCompletions,
        ir: None,
        entry_loss: Default::default(),
        session_key: None,
        session_writeback: None,
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
        state.session_affinity.clone(),
    );
    let req = build_request("gpt-4o", false);
    let trace_id = Uuid::new_v4();
    let start = Instant::now();

    // Routing decision happens now: captures (generation 0, version V).
    let ResolvedRoute {
        pipeline: _,
        targets: resolved_targets,
        api_endpoint,
        strategy,
        sticky_token,
        session_writeback: _,
        route_name,
        started_at,
        precheck_errors: _,
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
            session_ttl_secs: None,
            session_max_entries: None,
            model_name: "gpt-4o".into(),
            api_endpoint: None,
            pipeline: sag_config::Pipeline::Chat,
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
                capabilities: None,
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
        ingress_protocol: sag_core::IngressProtocol::ChatCompletions,
        ir: None,
        entry_loss: Default::default(),
        session_key: None,
        session_writeback: None,
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
        pipeline: _,
        targets: resolved_targets2,
        api_endpoint: api_endpoint2,
        strategy: strategy2,
        sticky_token: token2,
        session_writeback: _,
        route_name,
        started_at,
        precheck_errors: _,
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
        ingress_protocol: sag_core::IngressProtocol::ChatCompletions,
        ir: None,
        entry_loss: Default::default(),
        session_key: None,
        session_writeback: None,
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
        state.session_affinity.clone(),
    );
    let req = build_request("gpt-4o", false);
    let trace_id = Uuid::new_v4();
    let start = Instant::now();

    // Routing decision happens now: captures (generation 0, version V).
    let ResolvedRoute {
        pipeline: _,
        targets: resolved_targets,
        api_endpoint,
        strategy,
        sticky_token,
        session_writeback: _,
        route_name,
        started_at,
        precheck_errors: _,
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
        ingress_protocol: sag_core::IngressProtocol::ChatCompletions,
        ir: None,
        entry_loss: Default::default(),
        session_key: None,
        session_writeback: None,
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

// ─── M4 byte-regression golden tests ─────────────────────────────

/// Golden: non-stream gateway body is byte-identical to
/// `serde_json::to_vec(&original ChatCompletionResponse)` — the dual-hold
/// egress path must not re-encode via `encode_response` (which rewrites
/// `content: null` → `""` and can reorder fields).
#[tokio::test]
async fn m4_non_stream_body_bytes_match_original_response() {
    // Build an original response with null content + tool_calls in extra —
    // a shape that `encode_response` would rewrite.
    let original = ChatCompletionResponse {
        id: "chatcmpl-golden".into(),
        object: "chat.completion".into(),
        created: 1_700_000_000,
        model: "gpt-4o".into(),
        choices: vec![sag_core::types::ChatChoice {
            index: 0,
            message: sag_core::types::ChatMessage {
                role: "assistant".into(),
                content: serde_json::Value::Null,
                name: Some("golden".into()),
                extra: serde_json::Map::from_iter([(
                    "tool_calls".into(),
                    json!([{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "fn", "arguments": "{}"}
                    }]),
                )]),
            },
            finish_reason: Some("tool_calls".into()),
        }],
        usage: Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        }),
    };
    let golden_bytes = serde_json::to_vec(&original).expect("serialize golden");

    let mock = Arc::new(MockProvider::new("openai"));
    mock.push_chat(ChatBehavior::Ok(original.clone()));
    let state = build_state(vec![mock.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("openai", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        &body[..],
        &golden_bytes[..],
        "M4 non-stream egress must be byte-identical to the original ChatCompletionResponse"
    );

    // Prove the dual-hold rationale: encode_response would have diverged.
    let ir = ChatCompletionsCodec::decode_response(&original);
    let reencoded = serde_json::to_vec(&ChatCompletionsCodec::encode_response(&ir)).unwrap();
    assert_ne!(
        reencoded, golden_bytes,
        "fixture must be a case where encode_response diverges (null content / name / …)"
    );
}

/// Golden: streaming gateway body is byte-identical to the concatenation
/// of the provider's `StreamChunk::Data` frames (Raw passthrough; Usage /
/// Error / End never rewrite client bytes; `[DONE]` arrives as Data).
#[tokio::test]
async fn m4_stream_body_bytes_match_upstream_data_frames() {
    let f1 = b"data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n".to_vec();
    let f2 = b"data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n".to_vec();
    let f3 = b"data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n".to_vec();
    let f_done = b"data: [DONE]\n\n".to_vec();
    let golden: Vec<u8> = [f1.clone(), f2.clone(), f3.clone(), f_done.clone()].concat();

    let mock = Arc::new(MockProvider::new("openai"));
    mock.push_stream(StreamBehavior::Ok(vec![
        StreamChunk::Data(f1),
        StreamChunk::Data(f2),
        // Anomalous-looking usage-only meta chunk (analytics only — must
        // NOT appear in the client body).
        StreamChunk::Usage(Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        }),
        StreamChunk::Data(f3),
        // Mid-stream Error records analytics but emits no client bytes of
        // its own (the error Data frame is separate when present).
        StreamChunk::Error("ignored-for-body".into()),
        StreamChunk::Data(f_done),
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
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        &body[..],
        &golden[..],
        "M4 stream egress must be byte-identical to upstream Data frames (Raw passthrough)"
    );
}

/// Byte fidelity of the StreamChunk → IrStreamEvent mapping used at the
/// dispatch boundary: Data bytes are preserved exactly.
#[test]
fn m4_stream_chunk_to_ir_raw_preserves_bytes() {
    let raw = b"data: {\"choices\":[{\"delta\":{\"content\":\"exact-bytes\"}}]}\n\n".to_vec();
    let expected = raw.clone();
    let ev = IrStreamEvent::from(StreamChunk::Data(raw));
    match ev {
        IrStreamEvent::Raw(got) => assert_eq!(got, expected),
        other => panic!("expected Raw, got {other:?}"),
    }
}

/// Chat stream encoder: Raw yields original bytes; structured events go
/// through `ChatCompletionsCodec::encode_stream_event`.
#[test]
fn m4_chat_stream_encoder_raw_passthrough_and_structured() {
    let raw = b"data: {\"unparsed\":true}\n\n".to_vec();
    assert_eq!(
        ChatCompletionsCodec::encode_stream_event(&IrStreamEvent::Raw(raw.clone())),
        raw
    );
    // Structured TextDelta encodes to a data: JSON frame (M5+ path).
    let enc = ChatCompletionsCodec::encode_stream_event(&IrStreamEvent::TextDelta {
        index: 0,
        text: "hi".into(),
    });
    assert!(
        enc.starts_with(b"data: "),
        "structured frame has data prefix"
    );
    // Control frames align with StreamChunk::to_sse_bytes shapes.
    let usage = Usage {
        prompt_tokens: 1,
        completion_tokens: 2,
        total_tokens: 3,
        cached_tokens: None,
        cache_creation_tokens: None,
        reasoning_tokens: None,
    };
    let ir_u = ChatCompletionsCodec::encode_stream_event(&IrStreamEvent::Usage(usage.clone()));
    let chunk_u = StreamChunk::Usage(usage).to_sse_bytes();
    assert_eq!(ir_u, chunk_u, "usage frame must match to_sse_bytes");
    let ir_e = ChatCompletionsCodec::encode_stream_event(&IrStreamEvent::Error("x".into()));
    let chunk_e = StreamChunk::Error("x".into()).to_sse_bytes();
    assert_eq!(ir_e, chunk_e, "error frame must match to_sse_bytes");
    assert_eq!(
        ChatCompletionsCodec::encode_stream_event(&IrStreamEvent::End),
        b"data: [DONE]\n\n"
    );
}

/// S4: dispatch-level CallLog must carry a non-empty `loss_summary` when
/// the request-side IR already holds encode/decode loss entries.
#[tokio::test]
async fn dispatch_chat_call_log_carries_nonempty_loss_summary() {
    let mock = Arc::new(MockProvider::new("openai"));
    mock.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    let state = build_state(vec![mock.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("openai", "gpt-4o", 0)];
    let mut errors = Vec::new();

    // Seed request-side losses on the IR view so the dispatch summary
    // must surface them on the CallLog row.
    let mut ir = ChatCompletionsCodec::decode_request(&req);
    ir.loss.push_loss(
        sag_core::LossScope::Request,
        "n",
        sag_core::LossReason::UnsupportedByIr,
        json!(2),
    );
    ir.loss.push_loss(
        sag_core::LossScope::ContentBlock,
        "messages[0].content[1]",
        sag_core::LossReason::UnsupportedByTarget,
        json!({"type": "input_audio"}),
    );
    assert!(!ir.loss.is_empty(), "test IR must carry losses");

    let mut ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
    ctx.ir = Some(&ir);
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);

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
    assert_eq!(logs.len(), 1, "success must write one CallLog row");
    let summary = logs[0]
        .loss_summary
        .as_deref()
        .expect("loss_summary must be non-empty when ir.loss is non-empty");
    assert!(
        summary.contains("2 entries"),
        "summary must report the loss count: {summary}"
    );
    assert!(
        summary.contains("n:unsupported_by_ir"),
        "summary must include the request loss field: {summary}"
    );
}

/// Entry-route encode loss (`DispatchCtx.entry_loss`) must seed the
/// dispatch encode accumulator so CallLog `loss_summary` carries the
/// corresponding field/reason even when no attempt-side encode runs.
#[tokio::test]
async fn dispatch_chat_call_log_carries_entry_loss_seed() {
    let mock = Arc::new(MockProvider::new("openai"));
    mock.push_chat(ChatBehavior::Ok(build_response("gpt-4o", None)));
    let state = build_state(vec![mock.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", false);
    let targets = vec![target("openai", "gpt-4o", 0)];
    let mut errors = Vec::new();

    let mut ctx = make_ctx(&state, &rt, &req, &targets, false, &mut errors);
    let mut entry_loss = sag_core::LossInventory::default();
    entry_loss.push_loss(
        sag_core::LossScope::Request,
        "entry_field",
        sag_core::LossReason::UnsupportedByIr,
        json!("seed"),
    );
    ctx.entry_loss = entry_loss;
    assert!(!ctx.entry_loss.is_empty());

    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);

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
    assert_eq!(logs.len(), 1, "success must write one CallLog row");
    let summary = logs[0]
        .loss_summary
        .as_deref()
        .expect("loss_summary must be non-empty when entry_loss is seeded");
    assert!(
        summary.contains("entry_field:unsupported_by_ir"),
        "summary must include the entry_loss field/reason: {summary}"
    );
}

// ─── Session-sticky integration (P3) ─────────────────────────

/// Dispatch runtime sharing the state's session-affinity memory, so the
/// ordering decision and the success write-back see the same pins.
fn shared_runtime(state: &AppState) -> DispatchRuntime {
    DispatchRuntime::with_shared_state(
        state.circuit_breaker.clone(),
        state.active_targets.clone(),
        state.fill_first_orders.clone(),
        state.session_affinity.clone(),
    )
}

/// Full request cycle for the session tests: resolve (ordering + session
/// pin hoist + writeback capture) → dispatch (success rebind). Returns the
/// response and the ordered provider names (index 0 = the chosen target).
async fn run_session_request(
    state: &AppState,
    rt: &DispatchRuntime,
    req: &ChatCompletionRequest,
    key: Option<&sag_core::SessionKey>,
) -> (Response<Body>, Vec<String>) {
    let trace_id = Uuid::new_v4();
    let start = Instant::now();
    let ResolvedRoute {
        targets: resolved_targets,
        api_endpoint,
        strategy,
        sticky_token,
        session_writeback,
        route_name,
        started_at,
        ..
    } = crate::dispatch::resolve_route(
        state,
        rt,
        req,
        trace_id,
        start,
        chrono::Utc::now(),
        false,
        None,
        None,
        key,
    )
    .await
    .expect("route must resolve");
    let providers: Vec<String> = resolved_targets
        .iter()
        .map(|t| t.provider.clone())
        .collect();
    let mut errors = Vec::new();
    let ctx = DispatchCtx {
        state,
        runtime: rt,
        request: req,
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
        session_writeback,
        user_agent: None,
        client_ip: None,
        ingress_protocol: sag_core::IngressProtocol::ChatCompletions,
        ir: None,
        entry_loss: Default::default(),
        session_key: key.cloned(),
    };
    let resp = dispatch_chat(ctx).await;
    (resp, providers)
}

/// Pre-seed a session pin binding `key` to `provider` (the pin records the
/// same identity fields the real write-back would).
async fn bind_session_key(
    rt: &DispatchRuntime,
    model: &str,
    key: &sag_core::SessionKey,
    provider: &str,
    routes_version: u64,
) {
    let map_id = key.map_id(model);
    let mut guard = rt.session_affinity.write().await;
    let (_, token) = guard.get_with_token(
        model,
        &map_id,
        sag_state::DEFAULT_SESSION_TTL_SECS,
        routes_version,
        Instant::now(),
    );
    let wb = sag_state::SessionWriteback {
        map_id,
        model: model.into(),
        token,
        capacity: sag_state::DEFAULT_SESSION_MAX_ENTRIES,
    };
    assert!(guard.insert_if_generation(
        &wb,
        sag_state::SessionPin::new(provider, model, model, None, Instant::now(), routes_version,),
        routes_version,
    ));
}

#[tokio::test]
async fn session_sticky_binds_and_keeps_serving_the_same_target() {
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    let state =
        build_state_with_strategy(vec![m1.clone(), m2.clone()], Strategy::SessionSticky).await;
    let rt = shared_runtime(&state);
    let key = sag_core::SessionKey::new("meta", "conv-1");
    let req = build_request("gpt-4o", false);

    // Request 1: no pin yet — a weighted draw binds the session.
    let (resp, order1) = run_session_request(&state, &rt, &req, Some(&key)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bound = order1[0].clone();
    assert_eq!(
        m1.chat_calls() + m2.chat_calls(),
        1,
        "exactly one target absorbs the request"
    );

    // Request 2: the same session must lead with (and stay on) the bound target.
    let (resp, order2) = run_session_request(&state, &rt, &req, Some(&key)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(order2[0], bound, "session must stay on its bound target");
    let (bound_calls, other_calls) = if bound == "first" {
        (m1.chat_calls(), m2.chat_calls())
    } else {
        (m2.chat_calls(), m1.chat_calls())
    };
    assert_eq!(bound_calls, 2, "both requests served by the bound target");
    assert_eq!(other_calls, 0, "the other target must stay untouched");
}

#[tokio::test]
async fn session_sticky_failover_rebinds_to_the_successful_target() {
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    let state =
        build_state_with_strategy(vec![m1.clone(), m2.clone()], Strategy::SessionSticky).await;
    let rt = shared_runtime(&state);
    let key = sag_core::SessionKey::new("meta", "conv-2");
    let req = build_request("gpt-4o", false);

    // Pre-bind the session to "second"…
    let version = state.config.routes_version();
    bind_session_key(&rt, "gpt-4o", &key, "second", version).await;

    // …which then fails: "first" absorbs the failover and the pin must
    // follow the failover winner (rebind convergence).
    m2.push_chat(ChatBehavior::Retryable(AppError::Upstream {
        provider: "second".into(),
        status: 500,
        body: "boom".into(),
    }));
    let (resp, order1) = run_session_request(&state, &rt, &req, Some(&key)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(order1[0], "second", "pin must lead even when it then fails");
    assert_eq!(m2.chat_calls(), 1);
    assert_eq!(m1.chat_calls(), 1, "failover served by first");

    // Next request sticks to the rebound target.
    let (resp, order2) = run_session_request(&state, &rt, &req, Some(&key)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(order2[0], "first", "pin must follow the failover winner");
    assert_eq!(m1.chat_calls(), 2);
    assert_eq!(m2.chat_calls(), 1);
}

#[tokio::test]
async fn session_sticky_sessions_are_independent() {
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    let state =
        build_state_with_strategy(vec![m1.clone(), m2.clone()], Strategy::SessionSticky).await;
    let rt = shared_runtime(&state);
    let version = state.config.routes_version();
    let c1 = sag_core::SessionKey::new("meta", "conv-a");
    let c2 = sag_core::SessionKey::new("meta", "conv-b");
    let req = build_request("gpt-4o", false);

    bind_session_key(&rt, "gpt-4o", &c1, "first", version).await;
    bind_session_key(&rt, "gpt-4o", &c2, "second", version).await;

    let (_, order_c1) = run_session_request(&state, &rt, &req, Some(&c1)).await;
    let (_, order_c2) = run_session_request(&state, &rt, &req, Some(&c2)).await;
    assert_eq!(
        order_c1[0], "first",
        "session conv-a must lead with its pin"
    );
    assert_eq!(
        order_c2[0], "second",
        "session conv-b must lead with its pin"
    );
}

#[tokio::test]
async fn session_sticky_without_session_key_is_stateless() {
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    let state =
        build_state_with_strategy(vec![m1.clone(), m2.clone()], Strategy::SessionSticky).await;
    let rt = shared_runtime(&state);
    let req = build_request("gpt-4o", false);

    let (resp, order1) = run_session_request(&state, &rt, &req, None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(order1.len(), 2, "full permutation even without a session");
    assert!(rt.session_affinity.read().await.snapshot().is_empty());

    let (resp, _) = run_session_request(&state, &rt, &req, None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        rt.session_affinity.read().await.snapshot().is_empty(),
        "session-less requests must never pin anything"
    );
}

#[tokio::test]
async fn call_log_records_session_hash_for_every_strategy() {
    // The session grouping is a request fact, not a routing behavior:
    // a StickyFailover route must record `call_logs.session_key` exactly
    // like a session_sticky one (NULL only when no session was extracted).
    let m1 = Arc::new(MockProvider::new("first"));
    let m2 = Arc::new(MockProvider::new("second"));
    let state =
        build_state_with_strategy(vec![m1.clone(), m2.clone()], Strategy::StickyFailover).await;
    let rt = shared_runtime(&state);
    let key = sag_core::SessionKey::new("meta", "conv-9");
    let req = build_request("gpt-4o", false);

    let (resp, _) = run_session_request(&state, &rt, &req, Some(&key)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    state.collector.flush().await;

    let expected = key.map_id("gpt-4o");
    let logs = state
        .collector
        .store()
        .query_logs(sag_analytics::LogFilter {
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(logs.len(), 1, "one call-log row for one request");
    assert_eq!(
        logs[0].session_key.as_deref(),
        Some(expected.as_str()),
        "hash recorded even though the route is not session_sticky"
    );

    // Session-less request → NULL (the only NULL semantics left).
    let (resp, _) = run_session_request(&state, &rt, &req, None).await;
    assert_eq!(resp.status(), StatusCode::OK);
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
    assert_eq!(logs.len(), 2);
    assert_eq!(
        logs.iter().filter(|l| l.session_key.is_none()).count(),
        1,
        "NULL means 'no session identity', nothing else"
    );
}

// ─── Terminal-frame bridge regression ────────────────────────────
// `.plan/stream-terminal-frame-race-fix.md` §6 cases 5 / 6: a client that
// closes right after the client-visible terminal frame (upstream FIN tail
// latency) must classify as success, never as `client_cancelled` / 499.

/// One Anthropic SSE `data:` frame.
fn anthropic_frame(payload: &str) -> StreamChunk {
    StreamChunk::Data(format!("data: {payload}\n\n").into_bytes())
}

/// Drain the body until it suspends, returning the concatenated bytes seen so
/// far. Mirrors `stream::tests::drain_until_pending` — an all-`Ready`
/// `stream::iter` fixture cannot reproduce the race window.
fn drain_bytes_until_pending(body: &mut axum::body::Body) -> Vec<u8> {
    use http_body_util::BodyExt;
    use std::future::Future;

    let waker = futures_util::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    let mut out = Vec::new();
    let mut frames = 0usize;
    loop {
        let fut = body.frame();
        futures_util::pin_mut!(fut);
        match fut.poll(&mut cx) {
            std::task::Poll::Ready(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    out.extend_from_slice(&data);
                }
                frames += 1;
            }
            std::task::Poll::Ready(Some(Err(e))) => panic!("body error: {e}"),
            std::task::Poll::Ready(None) => panic!("body ended before the drop"),
            std::task::Poll::Pending => {
                assert!(frames > 0, "body suspended before forwarding any frame");
                return out;
            }
        }
        assert!(frames < 64, "body did not suspend");
    }
}

/// §6 case 5: Chat ingress + Anthropic upstream. The `[DONE]` bridge
/// synthesizes the OpenAI terminator on the first `End`; the client drops
/// right after receiving it, before the upstream's trailing `End` arrives.
/// The synthesized frame must be recognized as a terminal frame, so the drop
/// classifies as `Success` / 200 with the full usage payload — not
/// `client_cancelled` / 499.
#[tokio::test]
async fn anthropic_bridge_done_synthesis_race() {
    let upstream = Arc::new(AnthropicStreamProvider::new(
        "claude",
        vec![
            anthropic_frame(
                r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-3","usage":{"input_tokens":11,"output_tokens":1}}}"#,
            ),
            anthropic_frame(
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            ),
            anthropic_frame(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            ),
            anthropic_frame(r#"{"type":"content_block_stop","index":0}"#),
            anthropic_frame(
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
            ),
            // First `End` → bridged to `data: [DONE]`; the upstream stream
            // then suspends (the trailing `End` has not been enqueued yet).
            anthropic_frame(r#"{"type":"message_stop"}"#),
        ],
    ));
    let (state, pool) = build_state_with_event_pool(vec![upstream.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("claude-3", true);
    let targets = vec![ResolvedTarget {
        upstream_protocol: sag_core::ApiKind::AnthropicMessages,
        ..target("claude", "claude-3", 0)
    }];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
    let trace_id = ctx.trace_id;
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(upstream.stream_calls(), 1);

    let mut body = resp.into_body();
    let bytes = drain_bytes_until_pending(&mut body);
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("data: [DONE]"),
        "the first End must be bridged to the OpenAI terminator, got: {text}"
    );
    // The client closes after `[DONE]`; hyper drops the body.
    drop(body);
    tokio::task::yield_now().await;
    state.event_collector.shutdown().await;

    #[allow(clippy::type_complexity)]
    let (final_status, failure_reason, pt, ct): (
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
    ) = sqlx::query_as(
        "SELECT final_status, failure_reason, prompt_tokens, completion_tokens \
             FROM request_results WHERE request_id = ?",
    )
    .bind(trace_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("success fact must be persisted");
    assert_eq!(
        final_status, "success",
        "a drop after the synthesized `[DONE]` is a normal completion, not a cancel"
    );
    assert!(failure_reason.is_none());
    assert_eq!(pt, Some(11), "final accumulated usage enters the fact");
    assert_eq!(ct, Some(7));

    let target_status: String =
        sqlx::query_scalar("SELECT status FROM target_attempts WHERE request_id = ?")
            .bind(trace_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(target_status, "success");
}

/// §6 case 6 (normal path): Anthropic ingress + Chat upstream. The translated
/// bridge synthesizes `data: {"type":"message_stop"}` on the first `End`; a
/// later `End` terminates the loop without synthesizing a second terminator.
#[tokio::test]
async fn anthropic_translated_message_stop_bridge() {
    let mock = Arc::new(MockProvider::new("openai"));
    mock.push_stream(StreamBehavior::Ok(vec![
        StreamChunk::Data(
            b"data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n"
                .to_vec(),
        ),
        StreamChunk::Data(
            b"data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n"
                .to_vec(),
        ),
        // Upstream terminator #1 → decoded to End → synthesized message_stop.
        StreamChunk::Data(b"data: [DONE]\n\n".to_vec()),
        // Upstream terminator #2 → the second End terminates the loop.
        StreamChunk::Done,
    ]));
    let state = build_state(vec![mock.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", true);
    let targets = vec![target("openai", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let mut ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
    ctx.ingress_protocol = sag_core::IngressProtocol::AnthropicMessages;
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&body);

    assert_eq!(
        text.matches(r#""type":"message_stop""#).count(),
        1,
        "exactly one synthesized message_stop (the second End only terminates), got: {text}"
    );
    assert!(
        text.trim_end()
            .ends_with(r#"data: {"type":"message_stop"}"#),
        "the terminator is the last client-visible frame, got: {text}"
    );
    assert!(
        text.contains(r#""type":"text_delta""#) && text.contains("hi"),
        "the translated content still reaches the client, got: {text}"
    );
    assert!(
        !text.contains("[DONE]"),
        "the Anthropic encoder suppresses the OpenAI terminator, got: {text}"
    );
}

/// §6 case 6 (empty upstream): the `message_stop` bridge must run **after**
/// the empty-response peek. A vacuum Chat upstream (only `Done`, no content,
/// no usage) on an Anthropic ingress must still classify as empty →
/// `EmptyResponse` → retry, instead of being masked by the synthesized
/// `message_stop` (which the Chat peek would read as `Content`).
#[tokio::test]
async fn anthropic_translated_message_stop_bridge_does_not_mask_empty_retry() {
    let mock = Arc::new(MockProvider::new("openai"));
    // Vacuum upstream: zero content frames, no usage — the only event is the
    // terminator, so the peek sees a bare `End`.
    mock.push_stream(StreamBehavior::Ok(vec![StreamChunk::Done]));
    mock.push_stream(StreamBehavior::Ok(vec![StreamChunk::Done]));
    let state = build_state(vec![mock.clone()]).await;
    assert!(
        state.config.read().await.backoff.retry_on_empty_response,
        "this regression only exists with the empty-response peek enabled"
    );
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", true);
    let targets = vec![target("openai", "gpt-4o", 1)];
    let mut errors = Vec::new();
    let mut ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
    ctx.ingress_protocol = sag_core::IngressProtocol::AnthropicMessages;
    let resp = dispatch_chat(ctx).await;

    assert_eq!(
        mock.stream_calls(),
        2,
        "the empty peek must still see the raw `End` and retry (1 retry = 2 calls)"
    );
    assert_eq!(
        resp.status(),
        StatusCode::BAD_GATEWAY,
        "an empty upstream must fail (all providers exhausted), not commit a 200"
    );
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(
        !text.contains("message_stop"),
        "no synthesized terminator may leak on the failure path, got: {text}"
    );
    assert!(
        text.contains("empty response"),
        "the failure reason must be the empty-response one, got: {text}"
    );
    assert!(
        errors.iter().any(|e| e.contains("retries exhausted")),
        "the retry budget must be spent on the empty response, got: {errors:?}"
    );
}

/// §6 case 6 (race tail): the client drops right after the synthesized
/// `message_stop` frame, before the loop's terminating `End`. The drop must
/// classify as `Success` / 200 — the synthesized frame is what
/// `is_client_terminal_frame` matches.
#[tokio::test]
async fn anthropic_translated_message_stop_bridge_race_classifies_success() {
    let mock = Arc::new(MockProvider::new("openai"));
    mock.push_stream(StreamBehavior::OkThenPending(vec![
        StreamChunk::Data(
            b"data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n"
                .to_vec(),
        ),
        StreamChunk::Data(
            b"data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n"
                .to_vec(),
        ),
        // First End → synthesized message_stop, then the stream suspends.
        StreamChunk::Data(b"data: [DONE]\n\n".to_vec()),
    ]));
    let (state, pool) = build_state_with_event_pool(vec![mock.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", true);
    let targets = vec![target("openai", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let mut ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
    ctx.ingress_protocol = sag_core::IngressProtocol::AnthropicMessages;
    let trace_id = ctx.trace_id;
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let mut body = resp.into_body();
    let bytes = drain_bytes_until_pending(&mut body);
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.trim_end()
            .ends_with(r#"data: {"type":"message_stop"}"#),
        "the synthesized message_stop must be forwarded before the drop, got: {text}"
    );
    drop(body);
    tokio::task::yield_now().await;
    state.event_collector.shutdown().await;

    #[allow(clippy::type_complexity)]
    let (final_status, failure_reason, pt, ct): (
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
    ) = sqlx::query_as(
        "SELECT final_status, failure_reason, prompt_tokens, completion_tokens \
             FROM request_results WHERE request_id = ?",
    )
    .bind(trace_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("success fact must be persisted");
    assert_eq!(
        final_status, "success",
        "a drop after the synthesized message_stop is a normal completion"
    );
    assert!(failure_reason.is_none());
    assert_eq!(pt, Some(10));
    assert_eq!(ct, Some(5));

    state.collector.flush().await;
    let logs = state
        .collector
        .store()
        .query_logs(sag_analytics::LogFilter {
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap();
    let log = logs
        .iter()
        .find(|l| l.request_id == trace_id)
        .expect("one call_logs row for the request");
    assert_eq!(log.status_code, 200);
    assert!(log.error.is_none(), "success row carries no error message");
    assert!(log.error_category.is_none());
}

/// §6 case 5b: Chat ingress + **Responses** upstream. The `[DONE]` bridge
/// (#1) covers both translated upstream protocols, so the Chat-protocol
/// client gets its terminator even though the Responses upstream never emits
/// `data: [DONE]`. The upstream frames themselves stay a byte-identical Raw
/// passthrough.
#[tokio::test]
async fn chat_ingress_responses_upstream_gets_done_terminator() {
    let delta = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n".to_vec();
    let completed =
        b"data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n"
            .to_vec();
    let upstream = Arc::new(ResponsesStreamProvider::new(
        "oai",
        vec![
            StreamChunk::Data(delta.clone()),
            StreamChunk::Data(completed.clone()),
            StreamChunk::Done,
        ],
    ));
    let state = build_state(vec![upstream.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-5", true);
    let targets = vec![ResolvedTarget {
        upstream_protocol: sag_core::ApiKind::Responses,
        ..target("oai", "gpt-5", 0)
    }];
    let mut errors = Vec::new();
    let ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(upstream.stream_calls(), 1);

    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert_eq!(
        text.matches("data: [DONE]").count(),
        1,
        "exactly one Chat terminator, got: {text}"
    );
    assert!(
        text.ends_with("data: [DONE]\n\n"),
        "the synthesized [DONE] must be the last frame, got: {text}"
    );
    assert_eq!(
        body,
        [delta, completed, b"data: [DONE]\n\n".to_vec()].concat(),
        "the Responses frames must pass through byte-identically"
    );
}

/// §6 case 6 (non-compliant upstream): Anthropic ingress + Chat upstream that
/// never sends `finish_reason`. The bridge must synthesize a `Finish` before
/// the `message_stop` frame so the open content block is closed and the
/// protocol-required `message_delta` (with `stop_reason`) is emitted.
#[tokio::test]
async fn anthropic_bridge_closes_block_without_finish_reason() {
    let mock = Arc::new(MockProvider::new("openai"));
    mock.push_stream(StreamBehavior::Ok(vec![
        StreamChunk::Data(
            b"data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n"
                .to_vec(),
        ),
        // No `finish_reason` chunk: the upstream just terminates.
        StreamChunk::Done,
    ]));
    let state = build_state(vec![mock.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", true);
    let targets = vec![target("openai", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let mut ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
    ctx.ingress_protocol = sag_core::IngressProtocol::AnthropicMessages;
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    let stop = text
        .find(r#""type":"content_block_stop""#)
        .unwrap_or_else(|| panic!("missing content_block_stop, got: {text}"));
    let delta = text
        .find(r#""type":"message_delta""#)
        .unwrap_or_else(|| panic!("missing message_delta, got: {text}"));
    assert!(
        stop < delta,
        "the block must be closed before message_delta, got: {text}"
    );
    assert_eq!(
        text.matches(r#""type":"content_block_stop""#).count(),
        1,
        "exactly one content_block_stop, got: {text}"
    );
    assert_eq!(
        text.matches(r#""type":"message_delta""#).count(),
        1,
        "exactly one message_delta, got: {text}"
    );
    assert!(
        text.contains(r#""stop_reason":"end_turn""#),
        "the synthesized message_delta must carry a stop_reason, got: {text}"
    );
    assert_eq!(
        text.matches(r#""type":"message_stop""#).count(),
        1,
        "exactly one message_stop, got: {text}"
    );
    assert!(
        text.trim_end()
            .ends_with(r#"data: {"type":"message_stop"}"#),
        "message_stop must be the last frame, got: {text}"
    );
}

/// §6 case 6 (compliant upstream): a Chat upstream that *does* send
/// `finish_reason` already produced the `message_delta` — the bridge must not
/// add a second one, only the `message_stop` terminator.
#[tokio::test]
async fn anthropic_bridge_no_duplicate_message_delta_with_finish() {
    let mock = Arc::new(MockProvider::new("openai"));
    mock.push_stream(StreamBehavior::Ok(vec![
        StreamChunk::Data(
            b"data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n"
                .to_vec(),
        ),
        StreamChunk::Data(
            b"data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
                .to_vec(),
        ),
        StreamChunk::Done,
    ]));
    let state = build_state(vec![mock.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", true);
    let targets = vec![target("openai", "gpt-4o", 0)];
    let mut errors = Vec::new();
    let mut ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
    ctx.ingress_protocol = sag_core::IngressProtocol::AnthropicMessages;
    let resp = dispatch_chat(ctx).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert_eq!(
        text.matches(r#""type":"message_delta""#).count(),
        1,
        "a compliant upstream must not gain a second message_delta, got: {text}"
    );
    assert_eq!(
        text.matches(r#""type":"message_stop""#).count(),
        1,
        "exactly one message_stop, got: {text}"
    );
    assert!(
        text.trim_end()
            .ends_with(r#"data: {"type":"message_stop"}"#),
        "message_stop must be the last frame, got: {text}"
    );
}

/// §6 case 6 (Responses ingress): a translated upstream without
/// `finish_reason` must still deliver `response.completed`; a compliant one
/// must not get a second copy.
#[tokio::test]
async fn responses_bridge_synthesizes_completed_without_finish() {
    let non_compliant = Arc::new(MockProvider::new("openai"));
    non_compliant.push_stream(StreamBehavior::Ok(vec![
        StreamChunk::Data(
            b"data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n"
                .to_vec(),
        ),
        StreamChunk::Done,
    ]));
    let compliant = Arc::new(MockProvider::new("openai-compliant"));
    compliant.push_stream(StreamBehavior::Ok(vec![
        StreamChunk::Data(
            b"data: {\"id\":\"c2\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n"
                .to_vec(),
        ),
        StreamChunk::Data(
            b"data: {\"id\":\"c2\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
                .to_vec(),
        ),
        StreamChunk::Done,
    ]));
    let state = build_state(vec![non_compliant.clone(), compliant.clone()]).await;
    let rt = crate::testing::test_dispatch_runtime();
    let req = build_request("gpt-4o", true);

    for provider in ["openai", "openai-compliant"] {
        let targets = vec![target(provider, "gpt-4o", 0)];
        let mut errors = Vec::new();
        let mut ctx = make_ctx(&state, &rt, &req, &targets, true, &mut errors);
        ctx.ingress_protocol = sag_core::IngressProtocol::Responses;
        let resp = dispatch_chat(ctx).await;
        assert_eq!(resp.status(), StatusCode::OK, "provider {provider}");
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        assert_eq!(
            text.matches(r#""type":"response.completed""#).count(),
            1,
            "exactly one response.completed for {provider}, got: {text}"
        );
        assert!(
            text.contains(r#""status":"completed""#),
            "the terminal frame must report status=completed for {provider}, got: {text}"
        );
    }
    assert_eq!(non_compliant.stream_calls(), 1);
    assert_eq!(compliant.stream_calls(), 1);
}
