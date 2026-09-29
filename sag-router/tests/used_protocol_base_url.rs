//! base_url routing keyed by the actually-used upstream protocol.
//!
//! Regression: a multi-protocol provider with
//! `apis.chat_completions.base_url = A` + `apis.anthropic_messages.base_url
//! = B` and `fallback = anthropic_messages` must hit A for a native Chat
//! request and B (never A) for a Responses-ingress request translated down
//! to AnthropicMessages.

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

/// Parse the captured upstream request bytes → `(request_line, JSON body)`.
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

// ── app state wiring ─────────────────────────────────────────────

async fn build_state_with(providers: HashMap<String, ProviderConfig>, key: &str) -> AppState {
    let config = GatewayConfig {
        gateway_auth: Some(GatewayAuthConfig {
            api_keys: vec![key.to_string()],
        }),
        providers,
        routes: vec![RouteConfig {
            model_name: "route-model".into(),
            api_endpoint: None,
            pipeline: sag_config::Pipeline::Chat,
            retries: Some(0),
            timeout_secs: None,
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                extra: HashMap::new(),
                retries: Some(0),
                timeout_secs: None,
                provider: "p".into(),
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

fn provider_with(
    apis: &[(sag_core::ApiKind, &str)],
    fallback: Option<sag_core::ApiKind>,
) -> ProviderConfig {
    let mut pc = ProviderConfig {
        api_keys: vec!["sk-test".into()],
        key_strategy: KeyStrategy::FillFirst,
        proxy_mode: ProxyMode::default(),
        timeout_secs: 10,
        anthropic_cache_control: false,
        fallback,
        ..ProviderConfig::default()
    };
    for (kind, base) in apis {
        pc.apis.insert(
            *kind,
            ApiConfig {
                base_url: (*base).to_string(),
                ..ApiConfig::default()
            },
        );
    }
    pc
}

/// Multi-protocol provider: chat node → `chat_base`, anthropic node →
/// `anthropic_base`, `fallback = anthropic_messages`.
async fn build_multi_state(chat_base: &str, anthropic_base: &str, key: &str) -> AppState {
    let pc = provider_with(
        &[
            (sag_core::ApiKind::ChatCompletions, chat_base),
            (sag_core::ApiKind::AnthropicMessages, anthropic_base),
        ],
        Some(sag_core::ApiKind::AnthropicMessages),
    );
    let mut providers = HashMap::new();
    providers.insert("p".to_string(), pc);
    build_state_with(providers, key).await
}

/// Single-protocol provider declaring only `anthropic_messages` at
/// `anthropic_base`.
async fn build_single_anthropic_state(anthropic_base: &str, key: &str) -> AppState {
    let pc = provider_with(
        &[(sag_core::ApiKind::AnthropicMessages, anthropic_base)],
        None,
    );
    let mut providers = HashMap::new();
    providers.insert("p".to_string(), pc);
    build_state_with(providers, key).await
}

/// Single-protocol provider declaring only `chat_completions` at `chat_base`.
async fn build_single_chat_state(chat_base: &str, key: &str) -> AppState {
    let pc = provider_with(&[(sag_core::ApiKind::ChatCompletions, chat_base)], None);
    let mut providers = HashMap::new();
    providers.insert("p".to_string(), pc);
    build_state_with(providers, key).await
}

// ── client requests ──────────────────────────────────────────────

fn chat_request(body: &str, key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn responses_request(body: &str, key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/responses")
        .header("Authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

const CHAT_UPSTREAM_JSON: &str = r#"{
    "id": "chatcmpl-a",
    "object": "chat.completion",
    "created": 1700000000,
    "model": "real-model",
    "choices": [{
        "index": 0,
        "message": {"role": "assistant", "content": "from chat node"},
        "finish_reason": "stop"
    }],
    "usage": {"prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5}
}"#;

const ANTHROPIC_UPSTREAM_JSON: &str = r#"{
    "id": "msg_b",
    "type": "message",
    "role": "assistant",
    "model": "real-model",
    "content": [{"type": "text", "text": "from anthropic node"}],
    "stop_reason": "end_turn",
    "usage": {"input_tokens": 5, "output_tokens": 4}
}"#;

// ── multi-protocol provider routing ──────────────────────────────

#[tokio::test]
async fn chat_ingress_native_hits_chat_base_url() {
    // Chat ∈ apis → Native lane → upstream ChatCompletions → node A.
    let (chat_base, chat_rx) = spawn_server_capturing(move |mut stream| async move {
        respond_json(&mut stream, CHAT_UPSTREAM_JSON).await;
    })
    .await;
    let (anthropic_base, mut anthropic_rx) = spawn_server_capturing(move |mut stream| async move {
        respond_json(&mut stream, ANTHROPIC_UPSTREAM_JSON).await;
    })
    .await;

    let state = build_multi_state(&chat_base, &anthropic_base, "k").await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{"model":"route-model","messages":[{"role":"user","content":"hi"}]}"#;
    let resp = app.oneshot(chat_request(body, "k")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["object"], "chat.completion");
    assert_eq!(
        json["choices"][0]["message"]["content"], "from chat node",
        "native Chat must egress the chat node's response"
    );

    // Node A was contacted on the chat protocol path.
    let req = chat_rx.await.expect("chat node must be contacted");
    let (request_line, upstream) = parse_upstream_request(&req);
    assert!(
        request_line.starts_with("POST /chat/completions "),
        "native Chat must hit chat_completions.base_url: {request_line}"
    );
    assert_eq!(upstream["model"], "real-model");

    // Node B (anthropic) must not be contacted at all.
    assert!(
        anthropic_rx.try_recv().is_err(),
        "anthropic_messages node must not receive the native Chat request"
    );
}

#[tokio::test]
async fn responses_ingress_translated_to_anthropic_hits_anthropic_base_url() {
    // responses ∉ apis → Translated lane, fallback = anthropic_messages →
    // upstream AnthropicMessages → node B (never A).
    let (chat_base, mut chat_rx) = spawn_server_capturing(move |mut stream| async move {
        respond_json(&mut stream, CHAT_UPSTREAM_JSON).await;
    })
    .await;
    let (anthropic_base, anthropic_rx) = spawn_server_capturing(move |mut stream| async move {
        respond_json(&mut stream, ANTHROPIC_UPSTREAM_JSON).await;
    })
    .await;

    let state = build_multi_state(&chat_base, &anthropic_base, "k").await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{"model":"route-model","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
    let resp = app.oneshot(responses_request(body, "k")).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "translated Responses→Anthropic must succeed"
    );
    let json = body_json(resp).await;
    assert_eq!(json["object"], "response");
    assert_eq!(
        json["output"][0]["content"][0]["text"], "from anthropic node",
        "egress must carry the anthropic node's content, re-encoded as Responses"
    );

    // Node B was contacted on the anthropic protocol path.
    let req = anthropic_rx
        .await
        .expect("anthropic node must be contacted");
    let (request_line, upstream) = parse_upstream_request(&req);
    assert!(
        request_line.starts_with("POST /messages "),
        "translated anthropic must hit anthropic_messages.base_url: {request_line}"
    );
    assert_eq!(upstream["model"], "real-model");
    assert!(
        upstream.get("messages").is_some() && upstream.get("max_tokens").is_some(),
        "Anthropic body must carry messages + max_tokens: {upstream}"
    );

    // Node A (chat) must not be contacted.
    assert!(
        chat_rx.try_recv().is_err(),
        "chat_completions node must not receive the translated anthropic request"
    );
}

#[tokio::test]
async fn responses_ingress_stream_translated_to_anthropic_hits_anthropic_base_url() {
    // Streaming twin of the non-stream regression above.
    let sse = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_b\",\"model\":\"real-model\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"from anthropic node\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let (chat_base, mut chat_rx) = spawn_server_capturing(move |mut stream| async move {
        respond_json(&mut stream, CHAT_UPSTREAM_JSON).await;
    })
    .await;
    let (anthropic_base, anthropic_rx) = spawn_server_capturing(move |mut stream| async move {
        respond_sse(&mut stream, sse).await;
    })
    .await;

    let state = build_multi_state(&chat_base, &anthropic_base, "k").await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{"model":"route-model","stream":true,"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
    let resp = app.oneshot(responses_request(body, "k")).await.unwrap();
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
    assert!(
        text.contains("from anthropic node"),
        "Anthropic SSE must be re-rendered as Responses SSE: {text}"
    );
    assert!(
        text.contains("response."),
        "egress must be Responses-shaped: {text}"
    );
    assert!(
        !text.contains("[DONE]"),
        "Responses stream must not emit [DONE]: {text}"
    );

    let req = anthropic_rx
        .await
        .expect("anthropic node must be contacted");
    let (request_line, _) = parse_upstream_request(&req);
    assert!(
        request_line.starts_with("POST /messages "),
        "translated anthropic stream must hit anthropic_messages.base_url: {request_line}"
    );
    assert!(
        chat_rx.try_recv().is_err(),
        "chat_completions node must not receive the translated anthropic stream"
    );
}

// ── single-protocol provider custom base_url (anti-regression) ───

#[tokio::test]
async fn single_protocol_anthropic_provider_hits_its_custom_base_url() {
    let (base, rx) = spawn_server_capturing(move |mut stream| async move {
        respond_json(&mut stream, ANTHROPIC_UPSTREAM_JSON).await;
    })
    .await;

    let state = build_single_anthropic_state(&base, "k").await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{"model":"route-model","messages":[{"role":"user","content":"hi"}]}"#;
    let resp = app.oneshot(chat_request(body, "k")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["object"], "chat.completion");
    assert_eq!(
        json["choices"][0]["message"]["content"],
        "from anthropic node"
    );

    let req = rx.await.expect("anthropic provider must be contacted");
    let (request_line, _) = parse_upstream_request(&req);
    assert!(
        request_line.starts_with("POST /messages "),
        "single-protocol anthropic must hit its custom base_url: {request_line}"
    );
}

#[tokio::test]
async fn single_protocol_chat_provider_hits_its_custom_base_url() {
    let (base, rx) = spawn_server_capturing(move |mut stream| async move {
        respond_json(&mut stream, CHAT_UPSTREAM_JSON).await;
    })
    .await;

    let state = build_single_chat_state(&base, "k").await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{"model":"route-model","messages":[{"role":"user","content":"hi"}]}"#;
    let resp = app.oneshot(chat_request(body, "k")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["object"], "chat.completion");
    assert_eq!(json["choices"][0]["message"]["content"], "from chat node");

    let req = rx.await.expect("chat provider must be contacted");
    let (request_line, _) = parse_upstream_request(&req);
    assert!(
        request_line.starts_with("POST /chat/completions "),
        "single-protocol chat must hit its custom base_url: {request_line}"
    );
}
