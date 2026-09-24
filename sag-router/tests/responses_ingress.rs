//! Router-level integration tests for the OpenAI Responses ingress (M6).
//!
//! Covers:
//! - D1 stateful/background rejects → 422 (OpenAI envelope) before dispatch
//! - Responses ingress × {Chat, Anthropic, Responses native} × {non-stream, stream}
//! - native structure fidelity (normalized JSON / SSE semantic equivalence)
//! - Bearer-only auth (x-api-key does not unlock /v1/responses)

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use sag_config::{
    ConfigHandle, GatewayAuthConfig, GatewayConfig, KeyStrategy, ProviderConfig, ProviderType,
    ProxyMode, RouteConfig, RouteTarget, Strategy,
};
use sag_router::{AppState, DispatchRuntime, build_router, test_app_state};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

// ── mock upstream ─────────────────────────────────────────────────

/// Spawn a one-shot TCP server. Returns `(base_url, oneshot receiver of the
/// first request bytes read before `handler` runs)`.
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

#[derive(Clone, Copy)]
enum Upstream {
    Chat,
    Anthropic,
    ResponsesNative,
}

async fn build_state(base_url: &str, key: &str, upstream: Upstream) -> AppState {
    let (name, ptype, provider_proto, target_proto, api_endpoint) = match upstream {
        Upstream::Chat => (
            "openai",
            ProviderType::OpenAI,
            None,
            None,
            "/chat/completions",
        ),
        Upstream::Anthropic => (
            "anthropic",
            ProviderType::Anthropic,
            None,
            None,
            "/messages",
        ),
        Upstream::ResponsesNative => (
            "openai",
            ProviderType::OpenAI,
            Some(sag_core::UpstreamProtocol::Responses),
            Some(sag_core::UpstreamProtocol::Responses),
            "/responses",
        ),
    };

    let mut providers = HashMap::new();
    providers.insert(
        name.to_string(),
        ProviderConfig {
            provider_type: ptype,
            api_keys: vec!["sk-test".into()],
            key_strategy: KeyStrategy::FillFirst,
            base_url: base_url.to_string(),
            proxy_mode: ProxyMode::default(),
            models_endpoint: "models".into(),
            timeout_secs: 10,
            headers: HashMap::new(),
            body_overrides: HashMap::new(),
            strip_fields: vec![],
            model_strip_fields: HashMap::new(),
            model_overrides: HashMap::new(),
            role_mappings: HashMap::new(),
            enabled_models: vec![],
            enabled: true,
            anthropic_cache_control: false,
            chunk_timeout_secs: None,
            ttfb_timeout_secs: None,
            upstream_protocol: provider_proto,
        },
    );
    let config = GatewayConfig {
        gateway_auth: Some(GatewayAuthConfig {
            api_keys: vec![key.to_string()],
        }),
        providers,
        routes: vec![RouteConfig {
            model_name: "route-model".into(),
            api_endpoint: api_endpoint.into(),
            retries: None,
            timeout_secs: None,
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                extra: HashMap::new(),
                retries: None,
                timeout_secs: None,
                provider: name.into(),
                real_model: Some("real-model".into()),
                priority: 1,
                upstream_protocol: target_proto,
            }],
        }],
        ..sag_config::gateway_config_base!()
    };
    let handle = ConfigHandle::from_config(config);
    let mut registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
    let cfg_read = handle.read().await;
    for (pname, pc) in cfg_read.providers.iter() {
        let provider: Arc<dyn sag_providers::LlmProvider> = match pc.provider_type {
            ProviderType::OpenAI => {
                Arc::new(sag_providers::OpenAiProvider::new(pname.clone(), pc).unwrap())
            }
            ProviderType::Anthropic => {
                Arc::new(sag_providers::AnthropicProvider::new(pname.clone(), pc).unwrap())
            }
        };
        registry.insert(pname.clone(), provider);
    }
    drop(cfg_read);
    test_app_state(handle, Arc::new(tokio::sync::RwLock::new(registry))).await
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

/// Canonicalize JSON for semantic equality (key order / whitespace insensitive).
fn normalize(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => {
            serde_json::Value::Object(m.into_iter().map(|(k, val)| (k, normalize(val))).collect())
        }
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.into_iter().map(normalize).collect())
        }
        other => other,
    }
}

// ── D1 422 rejects ────────────────────────────────────────────────

async fn assert_422(body: &str) {
    // Any upstream works — decode rejects before dispatch.
    let state = build_state("http://127.0.0.1:1", "resp-key", Upstream::Chat).await;
    let app = build_router(state, DispatchRuntime::default());
    let resp = app
        .oneshot(responses_request(body, "resp-key"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "body={body}"
    );
    let json = body_json(resp).await;
    // OpenAI error envelope (§8.2).
    assert!(
        json.get("type").is_none(),
        "must not be Anthropic envelope: {json}"
    );
    assert_eq!(json["error"]["code"], "UNPROCESSABLE_ENTITY");
    assert!(json["error"]["message"].is_string(), "{json}");
}

#[tokio::test]
async fn previous_response_id_rejected_422() {
    assert_422(r#"{"model":"route-model","previous_response_id":"resp_abc","input":"hi"}"#).await;
    // Message must name the unsupported capability.
    let state = build_state("http://127.0.0.1:1", "resp-key", Upstream::Chat).await;
    let app = build_router(state, DispatchRuntime::default());
    let resp = app
        .oneshot(responses_request(
            r#"{"model":"route-model","previous_response_id":"resp_abc","input":"hi"}"#,
            "resp-key",
        ))
        .await
        .unwrap();
    let json = body_json(resp).await;
    let msg = json["error"]["message"].as_str().unwrap();
    assert!(
        msg.contains("previous_response_id") && msg.contains("not supported"),
        "message must state the unsupported capability: {msg}"
    );
}

#[tokio::test]
async fn store_true_rejected_422() {
    assert_422(r#"{"model":"route-model","store":true,"input":"hi"}"#).await;
}

#[tokio::test]
async fn background_rejected_422() {
    assert_422(r#"{"model":"route-model","background":true,"input":"hi"}"#).await;
}

#[tokio::test]
async fn store_false_allows_request_to_dispatch() {
    // store=false must pass decode and reach route resolution (404 for an
    // unknown model would also prove it got past decode; use a known route
    // name so we get past resolve and hit the dead upstream → 5xx, or
    // simply use unknown model → 404 which proves decode passed).
    let state = build_state("http://127.0.0.1:1", "resp-key", Upstream::Chat).await;
    let app = build_router(state, DispatchRuntime::default());
    let resp = app
        .oneshot(responses_request(
            r#"{"model":"does-not-exist","store":false,"input":"hi"}"#,
            "resp-key",
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "store=false must pass decode and reach resolve (404 no_route)"
    );
    let json = body_json(resp).await;
    assert_eq!(json["error"]["code"], "MODEL_NOT_FOUND");
}

// ── auth: Bearer only ─────────────────────────────────────────────

#[tokio::test]
async fn x_api_key_does_not_unlock_responses() {
    let state = build_state("http://127.0.0.1:1", "resp-key", Upstream::Chat).await;
    let app = build_router(state, DispatchRuntime::default());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("x-api-key", "resp-key")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"route-model","input":"hi"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "x-api-key must not unlock /v1/responses"
    );
    let json = body_json(resp).await;
    // OpenAI envelope, not Anthropic.
    assert_eq!(json["error"]["code"], "UNAUTHORIZED");
    assert!(json.get("type").is_none());
}

// ── error envelope on /v1/responses ───────────────────────────────

#[tokio::test]
async fn responses_no_route_returns_openai_error_envelope() {
    let state = build_state("http://127.0.0.1:1", "resp-key", Upstream::Chat).await;
    let app = build_router(state, DispatchRuntime::default());
    let resp = app
        .oneshot(responses_request(
            r#"{"model":"missing","input":"hi"}"#,
            "resp-key",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let json = body_json(resp).await;
    assert_eq!(json["error"]["code"], "MODEL_NOT_FOUND");
    assert!(
        json.get("type").is_none(),
        "must be OpenAI envelope: {json}"
    );
}

// ── Responses × Chat upstream (translated, non-stream) ────────────

#[tokio::test]
async fn responses_ingress_chat_upstream_text_and_usage() {
    let chat_resp = r#"{
        "id": "chatcmpl-r1",
        "object": "chat.completion",
        "created": 1700000000,
        "model": "real-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "Hello from chat"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}
    }"#;
    let expected = chat_resp.to_string();
    let base_url = spawn_server(move |mut stream| async move {
        respond_json(&mut stream, &expected).await;
    })
    .await;

    let state = build_state(&base_url, "resp-key", Upstream::Chat).await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{
        "model": "route-model",
        "instructions": "Be brief.",
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
    }"#;
    let resp = app
        .oneshot(responses_request(body, "resp-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;

    // Responses egress shape.
    assert_eq!(json["object"], "response");
    assert_eq!(json["id"], "chatcmpl-r1");
    assert_eq!(json["status"], "completed");
    assert_eq!(json["output"][0]["type"], "message");
    assert_eq!(json["output"][0]["content"][0]["type"], "output_text");
    assert_eq!(json["output"][0]["content"][0]["text"], "Hello from chat");
    assert_eq!(json["usage"]["input_tokens"], 7);
    assert_eq!(json["usage"]["output_tokens"], 3);
    assert_eq!(json["usage"]["total_tokens"], 10);
}

#[tokio::test]
async fn responses_ingress_chat_upstream_function_call() {
    let chat_resp = r#"{
        "id": "chatcmpl-r2",
        "object": "chat.completion",
        "created": 1700000000,
        "model": "real-model",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"SF\"}"}
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 12, "completion_tokens": 8, "total_tokens": 20}
    }"#;
    let expected = chat_resp.to_string();
    let base_url = spawn_server(move |mut stream| async move {
        respond_json(&mut stream, &expected).await;
    })
    .await;

    let state = build_state(&base_url, "resp-key", Upstream::Chat).await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{
        "model": "route-model",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "weather?"}]},
            {"type": "function_call", "call_id": "call_prev", "name": "get_weather", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_prev", "output": "72F"}
        ],
        "tools": [{"type": "function", "name": "get_weather", "parameters": {"type": "object"}}]
    }"#;
    let resp = app
        .oneshot(responses_request(body, "resp-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;

    assert_eq!(json["object"], "response");
    let output = json["output"].as_array().expect("output array");
    let fc = output
        .iter()
        .find(|o| o["type"] == "function_call")
        .expect("function_call item");
    assert_eq!(fc["call_id"], "call_1");
    assert_eq!(fc["name"], "get_weather");
    let args: serde_json::Value = serde_json::from_str(fc["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["city"], "SF");
    assert_eq!(json["usage"]["input_tokens"], 12);
    assert_eq!(json["usage"]["output_tokens"], 8);
}

// ── Responses × Chat upstream (translated, stream) ────────────────

#[tokio::test]
async fn responses_ingress_chat_upstream_stream() {
    let frames = concat!(
        r#"data: {"id":"chatcmpl-s1","object":"chat.completion.chunk","created":1700000000,"model":"real-model","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}"#,
        "\n\n",
        r#"data: {"id":"chatcmpl-s1","object":"chat.completion.chunk","created":1700000000,"model":"real-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        "\n\n",
        "data: [DONE]\n\n",
    );
    let expected = frames.to_string();
    let base_url = spawn_server(move |mut stream| async move {
        respond_sse(&mut stream, &expected).await;
    })
    .await;

    let state = build_state(&base_url, "resp-key", Upstream::Chat).await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{
        "model": "route-model",
        "stream": true,
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
    }"#;
    let resp = app
        .oneshot(responses_request(body, "resp-key"))
        .await
        .unwrap();
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
    assert!(text.contains("Hello"), "missing delta: {text}");
    assert!(
        text.contains("response.completed"),
        "missing completed: {text}"
    );
    assert!(
        !text.contains("[DONE]"),
        "Responses stream must not emit [DONE]: {text}"
    );
}

// ── Responses × Anthropic upstream (translated) ───────────────────

#[tokio::test]
async fn responses_ingress_anthropic_upstream_nonstream() {
    let anthropic_resp = r#"{
        "id": "msg_r1",
        "type": "message",
        "role": "assistant",
        "model": "real-model",
        "content": [{"type": "text", "text": "Hello from anthropic"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 4}
    }"#;
    let expected = anthropic_resp.to_string();
    let base_url = spawn_server(move |mut stream| async move {
        respond_json(&mut stream, &expected).await;
    })
    .await;

    let state = build_state(&base_url, "resp-key", Upstream::Anthropic).await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{
        "model": "route-model",
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
    }"#;
    let resp = app
        .oneshot(responses_request(body, "resp-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["object"], "response");
    assert_eq!(
        json["output"][0]["content"][0]["text"],
        "Hello from anthropic"
    );
    assert_eq!(json["usage"]["input_tokens"], 5);
    assert_eq!(json["usage"]["output_tokens"], 4);
}

#[tokio::test]
async fn responses_ingress_anthropic_upstream_stream() {
    let sse = concat!(
        r#"data: {"type":"message_start","message":{"id":"msg_s","model":"real-model","usage":{"input_tokens":10,"output_tokens":1}}}"#,
        "\n\n",
        r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        "\n\n",
        r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
        "\n\n",
        r#"data: {"type":"content_block_stop","index":0}"#,
        "\n\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
        "\n\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );
    let expected = sse.to_string();
    let base_url = spawn_server(move |mut stream| async move {
        respond_sse(&mut stream, &expected).await;
    })
    .await;

    let state = build_state(&base_url, "resp-key", Upstream::Anthropic).await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{
        "model": "route-model",
        "stream": true,
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
    }"#;
    let resp = app
        .oneshot(responses_request(body, "resp-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("response.created"), "missing created: {text}");
    assert!(text.contains("Hello"), "missing delta: {text}");
    assert!(
        text.contains("response.completed"),
        "missing completed: {text}"
    );
    assert!(!text.contains("[DONE]"), "must not emit [DONE]: {text}");
    assert!(
        !text.contains("message_stop"),
        "Anthropic frames must be re-rendered as Responses: {text}"
    );
}

// ── Responses × Responses native ──────────────────────────────────

#[tokio::test]
async fn responses_ingress_native_responses_nonstream_structure_fidelity() {
    let responses_resp = r#"{
        "id": "resp_native_1",
        "object": "response",
        "created_at": 1700000000,
        "status": "completed",
        "model": "real-model",
        "instructions": "Be brief.",
        "temperature": 0.2,
        "tools": [{"type": "function", "name": "get_weather", "parameters": {"type": "object"}}],
        "output": [
            {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
             "content": [{"type": "output_text", "text": "Native answer",
               "annotations": [{"type": "url_citation", "url": "https://example.com"}]}]},
            {"type": "function_call", "id": "fc_1", "call_id": "call_n1",
             "name": "get_weather", "arguments": "{\"city\":\"SF\"}", "status": "completed"}
        ],
        "usage": {
            "input_tokens": 9,
            "output_tokens": 6,
            "total_tokens": 15,
            "input_tokens_details": {"cached_tokens": 2},
            "output_tokens_details": {"reasoning_tokens": 1}
        }
    }"#;
    let expected = responses_resp.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            respond_json(&mut stream, &expected).await;
        }
    })
    .await;

    let state = build_state(&base_url, "resp-key", Upstream::ResponsesNative).await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{
        "model": "route-model",
        "instructions": "Be brief.",
        "input": [
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "weather?"}]}
        ],
        "tools": [{"type": "function", "name": "get_weather", "parameters": {"type": "object"}}]
    }"#;
    let resp = app
        .oneshot(responses_request(body, "resp-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "native path must succeed");
    let json = body_json(resp).await;

    // Upstream received a Responses-shaped body (never Chat).
    let req = req_rx.await.expect("request bytes");
    let req_text = String::from_utf8_lossy(&req);
    let req_body = req_text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or(&req_text);
    assert!(
        req_body.contains("\"instructions\""),
        "native request must carry instructions: {req_body}"
    );
    assert!(
        req_body.contains("\"input\""),
        "native request must carry input: {req_body}"
    );
    assert!(
        !req_body.contains("\"messages\""),
        "native request must NOT be Chat-shaped: {req_body}"
    );
    assert!(
        !req_body.contains("\"chat/completions\""),
        "path must be /responses: {req_body}"
    );

    // B1 structure fidelity: native egress is the raw upstream JSON —
    // full normalized equality (instructions/tools/temperature/item ids/
    // non-empty annotations must survive).
    let upstream: serde_json::Value = serde_json::from_str(responses_resp).unwrap();
    assert_eq!(
        normalize(json.clone()),
        normalize(upstream.clone()),
        "native non-stream must pass through the upstream body verbatim (semantic)"
    );
    // Explicit spot-checks for fields IR round-trip used to drop/rewrite.
    assert_eq!(json["instructions"], "Be brief.");
    assert_eq!(json["temperature"], 0.2);
    assert_eq!(json["tools"][0]["name"], "get_weather");
    assert_eq!(json["output"][0]["id"], "msg_1");
    assert_eq!(
        json["output"][0]["content"][0]["annotations"][0]["type"],
        "url_citation"
    );
    assert_eq!(json["output"][1]["id"], "fc_1");
}

#[tokio::test]
async fn responses_ingress_native_responses_stream_passthrough() {
    // Native stream: upstream Responses SSE frames pass through as Responses
    // SSE (Raw passthrough), with usage side-channeled from response.completed.
    let sse = concat!(
        r#"data: {"type":"response.created","response":{"id":"resp_s1","object":"response","created_at":1700000000,"model":"real-model","status":"in_progress","output":[]}}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","status":"in_progress","content":[]}}"#,
        "\n\n",
        r#"data: {"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"Native"}"#,
        "\n\n",
        r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Native","annotations":[]}]}}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{"id":"resp_s1","object":"response","status":"completed","usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}}}"#,
        "\n\n",
    );
    let expected = sse.to_string();
    let base_url = spawn_server(move |mut stream| async move {
        respond_sse(&mut stream, &expected).await;
    })
    .await;

    let state = build_state(&base_url, "resp-key", Upstream::ResponsesNative).await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{
        "model": "route-model",
        "stream": true,
        "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
    }"#;
    let resp = app
        .oneshot(responses_request(body, "resp-key"))
        .await
        .unwrap();
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

    // Native structure fidelity: each upstream frame's type survives.
    for marker in [
        "response.created",
        "response.output_item.added",
        "response.output_text.delta",
        "response.output_item.done",
        "response.completed",
    ] {
        assert!(text.contains(marker), "missing {marker}: {text}");
    }
    assert!(text.contains("Native"), "missing delta text: {text}");
    assert!(!text.contains("[DONE]"), "must not emit [DONE]: {text}");
    assert!(
        !text.contains("chat.completion.chunk"),
        "must not re-render as Chat chunks: {text}"
    );

    // SSE semantic equivalence: parse data frames and compare `type` sequence
    // against the upstream sequence.
    let upstream_types: Vec<String> = sse
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<serde_json::Value>(d).ok())
        .filter_map(|v| v.get("type").and_then(|t| t.as_str()).map(String::from))
        .collect();
    let gateway_types: Vec<String> = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<serde_json::Value>(d).ok())
        .filter_map(|v| v.get("type").and_then(|t| t.as_str()).map(String::from))
        .collect();
    // The gateway may add a synthesized response.completed only when the
    // upstream one is missing; here the upstream provides it, so sequences
    // must match exactly.
    assert_eq!(
        gateway_types, upstream_types,
        "native SSE event type sequence must match upstream"
    );
}
