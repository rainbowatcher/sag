//! Router-level protocol matrix tests (plan §10.1): remaining 3-ingress ×
//! 3-upstream cells not covered by `responses_ingress.rs` /
//! `anthropic_messages_ingress.rs` / `chat_bytes_regression.rs`.
//!
//! Covered cells:
//! - Chat × Anthropic (translated) non-stream + stream
//! - Chat × Responses (explicit declaration) non-stream + stream
//! - Anthropic × Responses (explicit declaration) non-stream + stream
//! - Anthropic × Chat stream (non-stream already covered upstream)

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use sag_config::{
    ApiConfig, ConfigHandle, GatewayAuthConfig, GatewayConfig, KeyStrategy, ProviderConfig,
    ProxyMode, RouteConfig, RouteTarget, Strategy,
};
use sag_router::{AppState, DispatchRuntime, build_router, test_app_state};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

// ── mock upstream ─────────────────────────────────────────────────

async fn spawn_server_capturing<F, Fut>(
    handler: F,
) -> (String, tokio::sync::oneshot::Receiver<Vec<u8>>)
where
    F: FnOnce(tokio::net::TcpStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        let _ = tx.send(buf[..n].to_vec());
        handler(stream).await;
    });
    (format!("http://{addr}"), rx)
}

async fn spawn_server<F, Fut>(handler: F) -> String
where
    F: FnOnce(tokio::net::TcpStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 8192];
        let _ = stream.read(&mut buf).await;
        handler(stream).await;
    });
    format!("http://{addr}")
}

async fn respond_json(stream: &mut tokio::net::TcpStream, body: &str) {
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await.unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let _ = stream.shutdown().await;
}

async fn respond_sse(stream: &mut tokio::net::TcpStream, body: &str) {
    let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
    stream.write_all(header.as_bytes()).await.unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();
    stream.flush().await.unwrap();
    let _ = stream.shutdown().await;
}

// ── app state wiring ─────────────────────────────────────────────

/// Upstream target shape for the matrix state builder.
#[derive(Clone, Copy)]
enum Upstream {
    /// Only `chat_completions` declared → the Chat bridge.
    Chat,
    /// Only `responses` declared → `/responses` (native on the Responses
    /// ingress, translated down from the other two).
    ResponsesExplicit,
    /// Only `anthropic_messages` declared → the Chat bridge translates
    /// through Anthropic's `/messages`.
    Anthropic,
}

async fn build_state(base_url: &str, key: &str, upstream: Upstream) -> AppState {
    // One declared `apis` key per upstream shape: it decides both the lane
    // (set membership) and the upstream path (fixed derivation).
    let (name, api_key) = match upstream {
        Upstream::Chat => ("openai", sag_core::ApiKind::ChatCompletions),
        Upstream::ResponsesExplicit => ("openai", sag_core::ApiKind::Responses),
        Upstream::Anthropic => ("anthropic", sag_core::ApiKind::AnthropicMessages),
    };

    let mut providers = HashMap::new();
    let mut pc = ProviderConfig {
        api_keys: vec!["sk-test".into()],
        key_strategy: KeyStrategy::FillFirst,
        proxy_mode: ProxyMode::default(),
        timeout_secs: 10,
        anthropic_cache_control: false,
        ..ProviderConfig::default()
    };
    pc.apis.insert(
        api_key,
        ApiConfig {
            base_url: base_url.to_string(),
            ..ApiConfig::default()
        },
    );
    providers.insert(name.to_string(), pc);
    let config = GatewayConfig {
        gateway_auth: Some(GatewayAuthConfig {
            api_keys: vec![key.to_string()],
        }),
        providers,
        routes: vec![RouteConfig {
            model_name: "route-model".into(),
            api_endpoint: None,
            pipeline: sag_config::Pipeline::Chat,
            // One-shot mocks: no retries (avoids a second accept).
            retries: Some(0),
            timeout_secs: None,
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                extra: HashMap::new(),
                retries: Some(0),
                timeout_secs: None,
                provider: name.into(),
                real_model: Some("real-model".into()),
                priority: 1,
                capabilities: None,
            }],
        }],
        ..sag_config::gateway_config_base!()
    };
    let handle = ConfigHandle::from_config(config);
    let mut registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
    let cfg_read = handle.read().await;
    for (pname, pc) in cfg_read.providers.iter() {
        let provider = sag_providers::create_provider(pname.clone(), pc).unwrap();
        registry.insert(pname.clone(), provider);
    }
    drop(cfg_read);
    test_app_state(handle, Arc::new(tokio::sync::RwLock::new(registry))).await
}

fn chat_request(body: &str, key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn messages_request(body: &str, key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

/// Parse the HTTP request bytes captured from the mock upstream and return
/// `(request_line, JSON body as Value)`.
fn parse_upstream_request(req: &[u8]) -> (String, serde_json::Value) {
    let text = String::from_utf8_lossy(req);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .expect("upstream request must have headers/body separator");
    let request_line = head.lines().next().unwrap_or_default().to_string();
    let json: serde_json::Value =
        serde_json::from_str(body).expect("upstream body must be valid JSON");
    (request_line, json)
}

// ── Chat × Anthropic (translated) ─────────────────────────────────

#[tokio::test]
async fn chat_x_anthropic_nonstream() {
    // AnthropicProvider::chat translates Chat→Anthropic internally and
    // always posts to `/messages` (D11). Egress is the Chat form.
    let anthropic_resp = r#"{
        "id": "msg_cx",
        "type": "message",
        "role": "assistant",
        "model": "real-model",
        "content": [{"type": "text", "text": "Hello from anthropic"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 4}
    }"#;
    let expected = anthropic_resp.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            respond_json(&mut stream, &expected).await;
        }
    })
    .await;

    let state = build_state(&base_url, "mx-key", Upstream::Anthropic).await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{"model":"route-model","messages":[{"role":"user","content":"hi"}]}"#;
    let resp = app.oneshot(chat_request(body, "mx-key")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;

    // Chat egress shape.
    assert_eq!(json["object"], "chat.completion");
    assert_eq!(
        json["choices"][0]["message"]["content"],
        "Hello from anthropic"
    );
    assert_eq!(json["choices"][0]["finish_reason"], "stop");
    assert_eq!(json["usage"]["prompt_tokens"], 5);
    assert_eq!(json["usage"]["completion_tokens"], 4);

    // Upstream: POST /messages with an Anthropic-shaped body.
    let req = req_rx.await.expect("request bytes");
    let (request_line, upstream) = parse_upstream_request(&req);
    assert!(
        request_line.starts_with("POST /messages "),
        "Chat×Anthropic must hit /messages: {request_line}"
    );
    assert_eq!(upstream["model"], "real-model");
    assert!(
        upstream.get("messages").is_some(),
        "Anthropic body must carry messages: {upstream}"
    );
    assert!(
        upstream.get("max_tokens").is_some(),
        "Anthropic body must carry max_tokens: {upstream}"
    );
    assert!(
        upstream.get("choices").is_none(),
        "must not be Chat-shaped: {upstream}"
    );
}

#[tokio::test]
async fn chat_x_anthropic_stream() {
    // AnthropicProvider::chat_stream normalizes Anthropic SSE → Chat SSE
    // (including the terminal [DONE] frame). Chat ingress Raw-passthroughs.
    let sse = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s\",\"model\":\"real-model\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let expected = sse.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            respond_sse(&mut stream, &expected).await;
        }
    })
    .await;

    let state = build_state(&base_url, "mx-key", Upstream::Anthropic).await;
    let app = build_router(state, DispatchRuntime::default());

    let body =
        r#"{"model":"route-model","messages":[{"role":"user","content":"hi"}],"stream":true}"#;
    let resp = app.oneshot(chat_request(body, "mx-key")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(ct.contains("text/event-stream"), "got {ct}");

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);

    // Chat egress: Chat-shaped chunks + [DONE].
    assert!(
        text.contains("chat.completion.chunk"),
        "egress must be Chat chunks: {text}"
    );
    assert!(text.contains("Hello"), "missing delta: {text}");
    assert!(
        text.contains("[DONE]"),
        "Chat stream must end with [DONE]: {text}"
    );
    assert!(
        !text.contains("message_stop"),
        "Anthropic frames must be normalized away: {text}"
    );

    // Upstream path is /messages with stream:true.
    let req = req_rx.await.expect("request bytes");
    let (request_line, upstream) = parse_upstream_request(&req);
    assert!(
        request_line.starts_with("POST /messages "),
        "got {request_line}"
    );
    assert_eq!(upstream["stream"], true);
}

// ── Chat × Responses (explicit) ───────────────────────────────────

#[tokio::test]
async fn chat_x_responses_native_nonstream() {
    // Explicit Responses declaration on a Chat ingress route: lane resolves
    // to Translated (ingress ≠ upstream), so the Chat bridge posts the
    // Chat-shaped body — but the route `api_endpoint = /responses` selects
    // the Responses path. Mock returns a Chat-shaped JSON the bridge can
    // parse (200 + Chat egress).
    // The upstream speaks Responses (that is the declared apis key), so
    // the translated lane posts a Responses-shaped body; the Chat egress
    // is re-encoded from IR.
    let resp_json = r#"{
        "id": "resp_cx",
        "object": "response",
        "created_at": 1700000000,
        "model": "real-model",
        "status": "completed",
        "output": [{
            "type": "message",
            "id": "msg_cx",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "Hello", "annotations": []}]
        }],
        "usage": {"input_tokens": 7, "output_tokens": 3, "total_tokens": 10}
    }"#;
    let expected = resp_json.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            respond_json(&mut stream, &expected).await;
        }
    })
    .await;

    let state = build_state(&base_url, "mx-key", Upstream::ResponsesExplicit).await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{"model":"route-model","messages":[{"role":"user","content":"hi"}]}"#;
    let resp = app.oneshot(chat_request(body, "mx-key")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "explicit Responses must 200");
    let json = body_json(resp).await;
    assert_eq!(json["object"], "chat.completion");
    assert_eq!(json["choices"][0]["message"]["content"], "Hello");

    // Upstream path is /responses (explicit declaration selects the path).
    let req = req_rx.await.expect("request bytes");
    let (request_line, _) = parse_upstream_request(&req);
    assert!(
        request_line.starts_with("POST /responses "),
        "Chat×Responses must hit /responses: {request_line}"
    );
}

#[tokio::test]
async fn chat_x_responses_native_stream() {
    // Stream with a Responses-shaped SSE mock: frames pass through the Chat
    // bridge's `process_sse_frame` (unknown JSON → Data) and Chat ingress
    // Raw-passthroughs them — so egress carries Responses event types and
    // **no** `[DONE]` (the mock never sends one, and none is synthesized).
    let sse = concat!(
        r#"data: {"type":"response.created","response":{"id":"r1","object":"response","status":"in_progress","output":[]}}"#,
        "\n\n",
        r#"data: {"type":"response.output_text.delta","item_id":"m1","output_index":0,"content_index":0,"delta":"Native"}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{"id":"r1","object":"response","status":"completed","usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}}}"#,
        "\n\n",
    );
    let expected = sse.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            respond_sse(&mut stream, &expected).await;
        }
    })
    .await;

    let state = build_state(&base_url, "mx-key", Upstream::ResponsesExplicit).await;
    let app = build_router(state, DispatchRuntime::default());

    let body =
        r#"{"model":"route-model","messages":[{"role":"user","content":"hi"}],"stream":true}"#;
    let resp = app.oneshot(chat_request(body, "mx-key")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(ct.contains("text/event-stream"), "got {ct}");

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("response.created"), "missing created: {text}");
    assert!(text.contains("Native"), "missing delta: {text}");
    assert!(
        text.contains("response.completed"),
        "missing completed: {text}"
    );
    assert!(
        !text.contains("[DONE]"),
        "Responses-style stream must not emit [DONE]: {text}"
    );

    // Upstream path is /responses.
    let req = req_rx.await.expect("request bytes");
    let (request_line, upstream) = parse_upstream_request(&req);
    assert!(
        request_line.starts_with("POST /responses "),
        "got {request_line}"
    );
    assert_eq!(upstream["stream"], true);
}

// ── Anthropic × Responses (explicit) ──────────────────────────────

#[tokio::test]
async fn anthropic_x_responses_native_nonstream() {
    // Anthropic ingress × explicit Responses target: lane is Translated
    // (ingress ≠ upstream), Chat bridge posts to `/responses`. Mock returns
    // a Chat-shaped JSON the bridge parses; Anthropic egress re-encodes
    // from IR.
    // Responses-shaped upstream (declared apis key); the Anthropic
    // ingress re-encodes the IR into its own envelope.
    let resp_json = r#"{
        "id": "resp_ax",
        "object": "response",
        "created_at": 1700000000,
        "model": "real-model",
        "status": "completed",
        "output": [{
            "type": "message",
            "id": "msg_ax",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "Hello from responses lane", "annotations": []}]
        }],
        "usage": {"input_tokens": 7, "output_tokens": 3, "total_tokens": 10}
    }"#;
    let expected = resp_json.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            respond_json(&mut stream, &expected).await;
        }
    })
    .await;

    let state = build_state(&base_url, "mx-key", Upstream::ResponsesExplicit).await;
    let app = build_router(state, DispatchRuntime::default());

    let body =
        r#"{"model":"route-model","max_tokens":100,"messages":[{"role":"user","content":"hi"}]}"#;
    let resp = app.oneshot(messages_request(body, "mx-key")).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "Anthropic×Responses must 200"
    );
    let json = body_json(resp).await;

    // Anthropic egress shape.
    assert_eq!(json["type"], "message");
    assert_eq!(json["role"], "assistant");
    assert_eq!(json["content"][0]["type"], "text");
    assert_eq!(json["content"][0]["text"], "Hello from responses lane");
    assert_eq!(json["stop_reason"], "end_turn");
    assert_eq!(json["usage"]["input_tokens"], 7);
    assert_eq!(json["usage"]["output_tokens"], 3);

    // Upstream path is /responses.
    let req = req_rx.await.expect("request bytes");
    let (request_line, _) = parse_upstream_request(&req);
    assert!(
        request_line.starts_with("POST /responses "),
        "Anthropic×Responses must hit /responses: {request_line}"
    );
}

#[tokio::test]
async fn anthropic_x_responses_native_stream() {
    // Chat SSE from the bridge is decoded to IR and re-rendered as
    // Anthropic SSE on the client (no [DONE], message lifecycle present).
    let chat_sse = concat!(
        r#"data: {"id":"chatcmpl-axs","object":"chat.completion.chunk","created":1700000000,"model":"real-model","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}"#,
        "\n\n",
        r#"data: {"id":"chatcmpl-axs","object":"chat.completion.chunk","created":170000000,"model":"real-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        "\n\n",
        "data: [DONE]\n\n",
    );
    let expected = chat_sse.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            respond_sse(&mut stream, &expected).await;
        }
    })
    .await;

    let state = build_state(&base_url, "mx-key", Upstream::ResponsesExplicit).await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{"model":"route-model","max_tokens":100,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
    let resp = app.oneshot(messages_request(body, "mx-key")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(ct.contains("text/event-stream"), "got {ct}");

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("message_start"), "missing start: {text}");
    assert!(text.contains("Hello"), "missing delta: {text}");
    // Translated Anthropic egress: End is a protocol-control event that is
    // never re-encoded into the body (stream.rs), so `message_stop` is not
    // synthesized — the terminal `message_delta` (from finish_reason) is the
    // last client-visible frame. `[DONE]` must not leak through.
    assert!(
        text.contains("message_delta"),
        "missing terminal message_delta: {text}"
    );
    assert!(text.contains("end_turn"), "missing stop_reason: {text}");
    assert!(
        !text.contains("[DONE]"),
        "Anthropic stream must not emit [DONE]: {text}"
    );
    assert!(
        !text.contains("chat.completion.chunk"),
        "Chat frames must be re-rendered as Anthropic: {text}"
    );

    // Upstream path is /responses.
    let req = req_rx.await.expect("request bytes");
    let (request_line, upstream) = parse_upstream_request(&req);
    assert!(
        request_line.starts_with("POST /responses "),
        "got {request_line}"
    );
    assert_eq!(upstream["stream"], true);
}

// ── Anthropic × Chat stream ───────────────────────────────────────

#[tokio::test]
async fn anthropic_x_chat_stream() {
    // Existing coverage only has Anthropic×Chat non-stream. Stream: Chat
    // upstream SSE is decoded to IR and re-rendered as Anthropic SSE
    // (message lifecycle, no [DONE]).
    let chat_sse = concat!(
        r#"data: {"id":"chatcmpl-ac","object":"chat.completion.chunk","created":1700000000,"model":"real-model","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}"#,
        "\n\n",
        r#"data: {"id":"chatcmpl-ac","object":"chat.completion.chunk","created":170000000,"model":"real-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        "\n\n",
        "data: [DONE]\n\n",
    );
    let expected = chat_sse.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            respond_sse(&mut stream, &expected).await;
        }
    })
    .await;

    let state = build_state(&base_url, "mx-key", Upstream::Chat).await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{"model":"route-model","max_tokens":100,"stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
    let resp = app.oneshot(messages_request(body, "mx-key")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(ct.contains("text/event-stream"), "got {ct}");

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("message_start"), "missing start: {text}");
    assert!(text.contains("Hello"), "missing delta: {text}");
    // Translated Anthropic egress: End (from Chat [DONE] / Done) is a
    // protocol-control event and is never re-encoded — no synthesized
    // `message_stop`. The terminal `message_delta` carries stop_reason.
    assert!(
        text.contains("message_delta"),
        "missing terminal message_delta: {text}"
    );
    assert!(text.contains("end_turn"), "missing stop_reason: {text}");
    assert!(
        !text.contains("[DONE]"),
        "Anthropic stream must not emit [DONE]: {text}"
    );
    assert!(
        !text.contains("chat.completion.chunk"),
        "Chat frames must be re-rendered as Anthropic: {text}"
    );

    // Upstream path is /chat/completions with stream:true.
    let req = req_rx.await.expect("request bytes");
    let (request_line, upstream) = parse_upstream_request(&req);
    assert!(
        request_line.starts_with("POST /chat/completions "),
        "got {request_line}"
    );
    assert_eq!(upstream["stream"], true);
}

// ── optional: cross-lane fallback (native fail → translated success) ──

#[tokio::test]
async fn cross_lane_fallback_native_responses_fail_then_chat_translated_ok() {
    // Two targets: explicit Responses (priority 1, dead upstream) then
    // Chat translated (priority 2, live mock). First attempt fails with a
    // connection error (retryable, retries=0 → exhausted), dispatch falls
    // over to the translated Chat target which succeeds.
    use sag_config::BackoffConfig;

    let chat_resp = r#"{
        "id": "chatcmpl-fb",
        "object": "chat.completion",
        "created": 1700000000,
        "model": "real-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "fallback ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
    }"#;
    let expected = chat_resp.to_string();
    let base_url = spawn_server(move |mut stream| async move {
        respond_json(&mut stream, &expected).await;
    })
    .await;

    // Dead address for the native Responses target (connection refused).
    let dead = "http://127.0.0.1:1";

    let mut providers = HashMap::new();
    // First target's provider speaks only `responses` (native lane on the
    // Responses ingress); the second only `chat_completions` (translated).
    for (pname, purl, api_key) in [
        ("openai-native", dead, sag_core::ApiKind::Responses),
        (
            "openai-chat",
            base_url.as_str(),
            sag_core::ApiKind::ChatCompletions,
        ),
    ] {
        let mut pc = ProviderConfig {
            api_keys: vec!["sk-test".into()],
            key_strategy: KeyStrategy::FillFirst,
            proxy_mode: ProxyMode::default(),
            timeout_secs: 2,
            anthropic_cache_control: false,
            ..ProviderConfig::default()
        };
        pc.apis.insert(
            api_key,
            ApiConfig {
                base_url: purl.to_string(),
                ..ApiConfig::default()
            },
        );
        providers.insert(pname.to_string(), pc);
    }
    let backoff = BackoffConfig {
        retries: 0,
        base_ms: 1,
        jitter: false,
        ..BackoffConfig::default()
    };
    let config = GatewayConfig {
        gateway_auth: Some(GatewayAuthConfig {
            api_keys: vec!["mx-key".into()],
        }),
        providers,
        backoff,
        routes: vec![RouteConfig {
            model_name: "route-model".into(),
            api_endpoint: None,
            pipeline: sag_config::Pipeline::Chat,
            retries: Some(0),
            timeout_secs: None,
            strategy: Strategy::FillFirst,
            targets: vec![
                RouteTarget {
                    extra: HashMap::new(),
                    retries: Some(0),
                    timeout_secs: Some(2),
                    provider: "openai-native".into(),
                    real_model: Some("real-model".into()),
                    priority: 1,
                    capabilities: None,
                },
                RouteTarget {
                    extra: HashMap::new(),
                    retries: Some(0),
                    timeout_secs: Some(5),
                    provider: "openai-chat".into(),
                    real_model: Some("real-model".into()),
                    priority: 2,
                    // Chat-only provider — translated lane (the Responses
                    // ingress is not in its protocol set).
                    capabilities: None,
                },
            ],
        }],
        ..sag_config::gateway_config_base!()
    };
    // Use Responses ingress so the first target is genuinely Native lane
    // (explicit Responses == ingress) and the second is Translated.
    let handle = ConfigHandle::from_config(config);
    let mut registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
    let cfg_read = handle.read().await;
    for (pname, pc) in cfg_read.providers.iter() {
        let provider = sag_providers::create_provider(pname.clone(), pc).unwrap();
        registry.insert(pname.clone(), provider);
    }
    drop(cfg_read);
    let state = test_app_state(handle, Arc::new(tokio::sync::RwLock::new(registry))).await;
    let app = build_router(state, DispatchRuntime::default());

    // Responses ingress → native first target fails → translated Chat succeeds.
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("Authorization", "Bearer mx-key")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"route-model","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "cross-lane fallback must succeed via translated Chat target"
    );
    let json = body_json(resp).await;
    assert_eq!(json["object"], "response");
    assert_eq!(json["output"][0]["content"][0]["text"], "fallback ok");
}
