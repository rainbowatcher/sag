//! Router-level integration tests for the Anthropic Messages ingress (M5).
//!
//! Covers:
//! - Anthropic ingress × Chat upstream (translated): non-stream text/tool/usage
//! - Error envelope shape on `/v1/messages`
//! - x-api-key vs Bearer on the real router
//! - chat_bytes_regression stays green (separate file)

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

async fn build_state(base_url: &str, key: &str) -> AppState {
    let mut providers = HashMap::new();
    providers.insert("openai".to_string(), {
        let mut pc = ProviderConfig {
            api_keys: vec!["sk-test".into()],
            key_strategy: KeyStrategy::FillFirst,
            proxy_mode: ProxyMode::default(),
            timeout_secs: 10,
            anthropic_cache_control: false,
            ..ProviderConfig::default()
        };
        pc.apis.insert(
            sag_core::ApiKind::ChatCompletions,
            ApiConfig {
                base_url: base_url.to_string(),
                ..ApiConfig::default()
            },
        );
        pc
    });
    let config = GatewayConfig {
        gateway_auth: Some(GatewayAuthConfig {
            api_keys: vec![key.to_string()],
        }),
        providers,
        routes: vec![RouteConfig {
            session_ttl_secs: None,
            session_max_entries: None,
            model_name: "claude-test".into(),
            api_endpoint: None,
            pipeline: sag_config::Pipeline::Chat,
            retries: None,
            timeout_secs: None,
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                extra: HashMap::new(),
                retries: None,
                timeout_secs: None,
                provider: "openai".into(),
                real_model: Some("gpt-4o".into()),
                priority: 1,
                capabilities: None,
            }],
        }],
        ..sag_config::gateway_config_base!()
    };
    let handle = ConfigHandle::from_config(config);
    let mut registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
    let cfg_read = handle.read().await;
    for (name, pc) in cfg_read.providers.iter() {
        let provider = sag_providers::create_provider(name.clone(), pc).unwrap();
        registry.insert(name.clone(), provider);
    }
    drop(cfg_read);
    test_app_state(handle, Arc::new(tokio::sync::RwLock::new(registry))).await
}

fn messages_request_x_api_key(body: &str, key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn messages_request_bearer(body: &str, key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("Authorization", format!("Bearer {key}"))
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

// ── Anthropic ingress × Chat upstream (translated, non-stream) ────

#[tokio::test]
async fn anthropic_ingress_chat_upstream_text_and_usage() {
    let chat_resp = r#"{
        "id": "chatcmpl-a1",
        "object": "chat.completion",
        "created": 1700000000,
        "model": "gpt-4o",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "Hello from upstream"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10}
    }"#;
    let expected = chat_resp.to_string();
    let base_url = spawn_server(move |mut stream| async move {
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            expected.len()
        );
        stream.write_all(header.as_bytes()).await.unwrap();
        stream.write_all(expected.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        let _ = stream.shutdown().await;
    })
    .await;

    let state = build_state(&base_url, "msg-key").await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{
        "model": "claude-test",
        "max_tokens": 100,
        "messages": [{"role": "user", "content": "hi"}]
    }"#;
    let resp = app
        .oneshot(messages_request_x_api_key(body, "msg-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    // Anthropic message shape.
    assert_eq!(json["type"], "message");
    assert_eq!(json["role"], "assistant");
    assert_eq!(json["id"], "chatcmpl-a1");
    assert_eq!(json["content"][0]["type"], "text");
    assert_eq!(json["content"][0]["text"], "Hello from upstream");
    assert_eq!(json["stop_reason"], "end_turn");
    // Usage shape aligned with Anthropic.
    assert_eq!(json["usage"]["input_tokens"], 7);
    assert_eq!(json["usage"]["output_tokens"], 3);
}

#[tokio::test]
async fn anthropic_ingress_chat_upstream_tool_use() {
    let chat_resp = r#"{
        "id": "chatcmpl-a2",
        "object": "chat.completion",
        "created": 1700000000,
        "model": "gpt-4o",
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
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            expected.len()
        );
        stream.write_all(header.as_bytes()).await.unwrap();
        stream.write_all(expected.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        let _ = stream.shutdown().await;
    })
    .await;

    let state = build_state(&base_url, "msg-key").await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{
        "model": "claude-test",
        "max_tokens": 200,
        "messages": [{"role": "user", "content": "weather in SF?"}],
        "tools": [{
            "name": "get_weather",
            "description": "Get weather",
            "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}
        }]
    }"#;
    let resp = app
        .oneshot(messages_request_bearer(body, "msg-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(json["type"], "message");
    assert_eq!(json["stop_reason"], "tool_use");
    // tool_use block preserved (§1.3 defect 1 fix).
    let content = json["content"].as_array().expect("content array");
    let tool_use = content
        .iter()
        .find(|b| b["type"] == "tool_use")
        .expect("tool_use block must be present");
    assert_eq!(tool_use["id"], "call_1");
    assert_eq!(tool_use["name"], "get_weather");
    assert_eq!(tool_use["input"]["city"], "SF");
    assert_eq!(json["usage"]["input_tokens"], 12);
    assert_eq!(json["usage"]["output_tokens"], 8);
}

// ── Error envelope on /v1/messages ────────────────────────────────

#[tokio::test]
async fn messages_no_route_returns_anthropic_error_envelope() {
    // No matching route → 404, Anthropic envelope.
    let state = build_state("http://127.0.0.1:1", "msg-key").await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{
        "model": "does-not-exist",
        "max_tokens": 10,
        "messages": [{"role": "user", "content": "hi"}]
    }"#;
    let resp = app
        .oneshot(messages_request_x_api_key(body, "msg-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["type"], "error");
    assert_eq!(json["error"]["type"], "not_found_error");
    assert!(json["error"]["message"].is_string());
    // Must NOT be the OpenAI envelope.
    assert!(json.get("error").and_then(|e| e.get("code")).is_none());
}

#[tokio::test]
async fn messages_malformed_json_returns_anthropic_error_envelope() {
    let state = build_state("http://127.0.0.1:1", "msg-key").await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app
        .oneshot(messages_request_x_api_key("{ not json", "msg-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["type"], "error");
    assert_eq!(json["error"]["type"], "invalid_request_error");
}

// ── Auth cross-path security on the real router ───────────────────

#[tokio::test]
async fn x_api_key_does_not_unlock_chat_completions_on_real_router() {
    let state = build_state("http://127.0.0.1:1", "msg-key").await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("x-api-key", "msg-key")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"claude-test","messages":[]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "x-api-key must not unlock /v1/chat/completions"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    // Chat path keeps OpenAI envelope.
    assert_eq!(json["error"]["code"], "UNAUTHORIZED");
    assert!(json.get("type").is_none());
}

#[tokio::test]
async fn x_api_key_does_not_unlock_api_paths_on_real_router() {
    let state = build_state("http://127.0.0.1:1", "msg-key").await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/providers")
                .header("x-api-key", "msg-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "x-api-key must not unlock /api/*"
    );
}

// ── Anthropic ingress × Anthropic upstream (native, explicit) ─────

async fn build_state_anthropic_native(base_url: &str, key: &str) -> AppState {
    let mut providers = HashMap::new();
    providers.insert("anthropic".to_string(), {
        let mut pc = ProviderConfig {
            api_keys: vec!["sk-ant-test".into()],
            key_strategy: KeyStrategy::FillFirst,
            proxy_mode: ProxyMode::default(),
            timeout_secs: 10,
            anthropic_cache_control: false,
            ..ProviderConfig::default()
        };
        // Declaring `anthropic_messages` makes the Anthropic ingress
        // resolve to the Native lane (protocol set membership).
        pc.apis.insert(
            sag_core::ApiKind::AnthropicMessages,
            ApiConfig {
                base_url: base_url.to_string(),
                ..ApiConfig::default()
            },
        );
        pc
    });
    let config = GatewayConfig {
        gateway_auth: Some(GatewayAuthConfig {
            api_keys: vec![key.to_string()],
        }),
        providers,
        routes: vec![RouteConfig {
            session_ttl_secs: None,
            session_max_entries: None,
            model_name: "claude-native".into(),
            api_endpoint: None,
            pipeline: sag_config::Pipeline::Chat,
            retries: None,
            timeout_secs: None,
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                extra: HashMap::new(),
                retries: None,
                timeout_secs: None,
                provider: "anthropic".into(),
                real_model: Some("claude-sonnet-4".into()),
                priority: 1,
                capabilities: None,
            }],
        }],
        ..sag_config::gateway_config_base!()
    };
    let handle = ConfigHandle::from_config(config);
    let mut registry: HashMap<String, Arc<dyn sag_providers::LlmProvider>> = HashMap::new();
    let cfg_read = handle.read().await;
    for (name, pc) in cfg_read.providers.iter() {
        let provider = sag_providers::create_provider(name.clone(), pc).unwrap();
        registry.insert(name.clone(), provider);
    }
    drop(cfg_read);
    test_app_state(handle, Arc::new(tokio::sync::RwLock::new(registry))).await
}

#[tokio::test]
async fn anthropic_ingress_native_anthropic_upstream_tool_use_thinking_usage() {
    // Native lane: upstream receives an Anthropic-shaped body and returns
    // an Anthropic-shaped response (never translated through Chat).
    let anthropic_resp = r#"{
        "id": "msg_native_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4",
        "content": [
            {"type": "thinking", "thinking": "I should call the tool"},
            {"type": "text", "text": "Checking weather"},
            {"type": "tool_use", "id": "toolu_1", "name": "get_weather",
             "input": {"city": "SF"}}
        ],
        "stop_reason": "tool_use",
        "usage": {
            "input_tokens": 25,
            "output_tokens": 15,
            "cache_read_input_tokens": 5,
            "cache_creation_input_tokens": 2,
            "thinking_tokens": 8
        }
    }"#;
    let expected = anthropic_resp.to_string();
    let base_url = spawn_server(move |mut stream| async move {
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            expected.len()
        );
        stream.write_all(header.as_bytes()).await.unwrap();
        stream.write_all(expected.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        let _ = stream.shutdown().await;
    })
    .await;

    let state = build_state_anthropic_native(&base_url, "msg-key").await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{
        "model": "claude-native",
        "max_tokens": 4096,
        "thinking": {"type": "enabled", "budget_tokens": 2048},
        "messages": [{"role": "user", "content": "weather in SF?"}],
        "tools": [{
            "name": "get_weather",
            "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}
        }]
    }"#;
    let resp = app
        .oneshot(messages_request_x_api_key(body, "msg-key"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "native Anthropic path must succeed"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

    assert_eq!(json["type"], "message");
    assert_eq!(json["id"], "msg_native_1");
    assert_eq!(json["stop_reason"], "tool_use");

    let content = json["content"].as_array().expect("content array");
    // thinking preserved
    let thinking = content
        .iter()
        .find(|b| b["type"] == "thinking")
        .expect("thinking block");
    assert_eq!(thinking["thinking"], "I should call the tool");
    // tool_use preserved
    let tool_use = content
        .iter()
        .find(|b| b["type"] == "tool_use")
        .expect("tool_use block");
    assert_eq!(tool_use["id"], "toolu_1");
    assert_eq!(tool_use["name"], "get_weather");
    assert_eq!(tool_use["input"]["city"], "SF");
    // usage complete (cache + thinking tokens)
    assert_eq!(json["usage"]["input_tokens"], 25);
    assert_eq!(json["usage"]["output_tokens"], 15);
    assert_eq!(json["usage"]["cache_read_input_tokens"], 5);
    assert_eq!(json["usage"]["cache_creation_input_tokens"], 2);
    assert_eq!(json["usage"]["thinking_tokens"], 8);
}

// ── B5: native streaming + multi-turn tool history ────────────────

#[tokio::test]
async fn anthropic_ingress_native_stream_message_stop() {
    // Native streaming: upstream SSE frames pass through as Anthropic SSE.
    let sse = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_s1\",\"model\":\"claude-sonnet-4\",\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let base_url = spawn_server(move |mut stream| async move {
        let header =
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
        stream.write_all(header.as_bytes()).await.unwrap();
        stream.write_all(sse.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        let _ = stream.shutdown().await;
    })
    .await;

    let state = build_state_anthropic_native(&base_url, "msg-key").await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{
        "model": "claude-native",
        "max_tokens": 100,
        "stream": true,
        "messages": [{"role": "user", "content": "hi"}]
    }"#;
    let resp = app
        .oneshot(messages_request_x_api_key(body, "msg-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        ct.contains("text/event-stream"),
        "native stream must be SSE, got {ct}"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("message_start"), "missing start: {text}");
    assert!(text.contains("Hello"), "missing delta: {text}");
    assert!(text.contains("message_stop"), "missing stop: {text}");
    assert!(
        !text.contains("[DONE]"),
        "Anthropic stream must not emit [DONE]"
    );
}

#[tokio::test]
async fn anthropic_ingress_multiturn_tool_history_reaches_native_upstream() {
    // Multi-turn tool_use + tool_result history must survive IR encode and
    // arrive at the native Anthropic upstream as exactly one tool_use and
    // one tool_result block (no double-write, no translation loss).
    let anthropic_resp = r#"{
        "id": "msg_mt",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4",
        "content": [{"type": "text", "text": "It is 72F."}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 40, "output_tokens": 5}
    }"#;
    let expected = anthropic_resp.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                expected.len()
            );
            stream.write_all(header.as_bytes()).await.unwrap();
            stream.write_all(expected.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
            let _ = stream.shutdown().await;
        }
    })
    .await;

    let state = build_state_anthropic_native(&base_url, "msg-key").await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{
        "model": "claude-native",
        "max_tokens": 2048,
        "messages": [
            {"role": "user", "content": "weather in SF?"},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_1", "name": "get_weather",
                 "input": {"city": "SF"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_1", "content": "72F"}
            ]},
            {"role": "assistant", "content": "thanks, and the forecast?"}
        ],
        "tools": [{
            "name": "get_weather",
            "input_schema": {"type": "object"}
        }]
    }"#;
    let resp = app
        .oneshot(messages_request_x_api_key(body, "msg-key"))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "native multi-turn tool path must succeed"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["id"], "msg_mt");
    assert_eq!(json["content"][0]["text"], "It is 72F.");

    // Assert the upstream request shape (received before the response).
    let req = req_rx.await.expect("request bytes");
    let req_text = String::from_utf8_lossy(&req);
    // Request headers + JSON body: isolate the body after \r\n\r\n.
    let req_body = req_text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or(&req_text);
    assert!(
        req_body.contains("\"tool_use\""),
        "upstream request must carry tool_use: {req_body}"
    );
    assert!(
        req_body.contains("\"tool_result\""),
        "upstream request must carry tool_result: {req_body}"
    );
    // Exactly one tool_use block in messages content (B1: no double-write).
    // The string "tool_use" also appears as a stop_reason only in responses;
    // here we count content block type occurrences in the request body.
    assert_eq!(
        req_body.matches("\"type\":\"tool_use\"").count(),
        1,
        "tool_use content block must appear exactly once: {req_body}"
    );
    assert_eq!(
        req_body.matches("\"type\":\"tool_result\"").count(),
        1,
        "tool_result content block must appear exactly once: {req_body}"
    );
}

// ── B2: native non-stream raw passthrough (structure fidelity) ───

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

#[tokio::test]
async fn anthropic_ingress_native_nonstream_structure_fidelity() {
    // Unknown fields + non-standard id must survive verbatim (raw egress,
    // aligned with responses_ingress_native_responses_nonstream_structure_fidelity).
    let anthropic_resp = r#"{
        "id": "msg_weird_非标准",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4",
        "stop_reason": "end_turn",
        "content": [{"type": "text", "text": "fidelity"}],
        "usage": {"input_tokens": 11, "output_tokens": 4},
        "unknown_vendor_field": {"nested": [1, 2, {"keep": true}]},
        "service_tier": "priority"
    }"#;
    let expected = anthropic_resp.to_string();
    let base_url = spawn_server(move |mut stream| async move {
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            expected.len()
        );
        stream.write_all(header.as_bytes()).await.unwrap();
        stream.write_all(expected.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        let _ = stream.shutdown().await;
    })
    .await;

    let state = build_state_anthropic_native(&base_url, "msg-key").await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{
        "model": "claude-native",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}]
    }"#;
    let resp = app
        .oneshot(messages_request_x_api_key(body, "msg-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "native path must succeed");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let upstream: serde_json::Value = serde_json::from_str(anthropic_resp).unwrap();

    assert_eq!(
        normalize(json.clone()),
        normalize(upstream.clone()),
        "native Anthropic non-stream must pass through the upstream body verbatim (semantic)"
    );
    // Explicit spot-checks for fields an IR round-trip would drop/rewrite.
    assert_eq!(json["id"], "msg_weird_非标准");
    assert_eq!(json["unknown_vendor_field"]["nested"][2]["keep"], true);
    assert_eq!(json["service_tier"], "priority");
}
