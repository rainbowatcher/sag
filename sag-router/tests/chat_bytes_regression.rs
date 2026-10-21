//! Router-level byte-gate regression tests for `/v1/chat/completions`.
//!
//! Covers the real `chat_completions` handler path (`routes.rs` passes
//! `ir: Some(&ir)` into `dispatch_chat`) with a mock upstream TCP server
//! (same pattern as `sag-providers` `spawn_server`). Asserts that the
//! client-visible HTTP body is byte-identical to the upstream payload:
//!
//! - non-stream: body == `serde_json::to_vec(ChatCompletionResponse)` of
//!   the fixed upstream JSON;
//! - stream: body == the concatenation of upstream SSE Data frames
//!   (including the terminal `[DONE]` frame).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use sag_config::{
    ApiConfig, ConfigHandle, GatewayAuthConfig, GatewayConfig, KeyStrategy, ProviderConfig,
    ProxyMode, RouteConfig, RouteTarget, Strategy,
};
use sag_core::ChatCompletionResponse;
use sag_router::{AppState, DispatchRuntime, build_router, test_app_state};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

// ── mock upstream (mirrors sag-providers test_support::spawn_server) ──

/// Spawn a one-shot TCP server. `handler` receives the accepted stream
/// after the request bytes are (best-effort) drained, and drives the
/// response however the test needs. Returns the bound `http://` base URL.
async fn spawn_server<F, Fut>(handler: F) -> String
where
    F: FnOnce(tokio::net::TcpStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // Drain the request headers/body so the client's send side completes.
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf).await;
        handler(stream).await;
    });
    format!("http://{addr}")
}

// ── app state wiring ─────────────────────────────────────────────

async fn build_state(base_url: &str) -> AppState {
    let mut providers = HashMap::new();
    providers.insert("openai".to_string(), {
        let mut pc = ProviderConfig {
            api_keys: vec!["sk-test".into()],
            key_strategy: KeyStrategy::FillFirst,
            proxy_mode: ProxyMode::default(),
            timeout_secs: 10,
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
            api_keys: vec!["bytes-key".into()],
        }),
        providers,
        routes: vec![RouteConfig {
            session_ttl_secs: None,
            session_max_entries: None,
            model_name: "gpt-4o".into(),
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

fn chat_request(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", "Bearer bytes-key")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

// ── non-stream byte gate ─────────────────────────────────────────

#[tokio::test]
async fn chat_non_stream_body_is_byte_identical_to_upstream_json() {
    // Canonical upstream response: build the struct first, then serialize
    // once — both the mock upstream body and the expected gateway body use
    // this exact byte string, so any re-encoding drift fails the gate.
    let upstream_resp = ChatCompletionResponse {
        id: "chatcmpl-bytes-1".into(),
        object: "chat.completion".into(),
        created: 1_700_000_000,
        model: "gpt-4o".into(),
        choices: vec![sag_core::types::ChatChoice {
            index: 0,
            message: sag_core::types::ChatMessage {
                role: "assistant".into(),
                content: serde_json::Value::String("byte-exact".into()),
                name: None,
                extra: serde_json::Map::new(),
            },
            finish_reason: Some("stop".into()),
        }],
        usage: Some(sag_core::types::Usage {
            prompt_tokens: 10,
            completion_tokens: 2,
            total_tokens: 12,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        }),
    };
    let upstream_bytes = serde_json::to_vec(&upstream_resp).unwrap();

    let expected = upstream_bytes.clone();
    let base_url = spawn_server(move |mut stream| async move {
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            expected.len()
        );
        stream.write_all(header.as_bytes()).await.unwrap();
        stream.write_all(&expected).await.unwrap();
        stream.flush().await.unwrap();
        let _ = stream.shutdown().await;
    })
    .await;

    let state = build_state(&base_url).await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app
        .oneshot(chat_request(
            r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        body.as_ref(),
        upstream_bytes.as_slice(),
        "non-stream gateway body must be byte-identical to the upstream JSON \
         (equivalently: ChatCompletionResponse serde_json::to_vec)"
    );
}

// ── n > 1 entry rejection (plan §3.1) ────────────────────────────

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn chat_n2_rejected_422_with_openai_envelope() {
    // No upstream needed — the entry rejects before route resolution.
    let state = build_state("http://127.0.0.1:1").await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app
        .oneshot(chat_request(
            r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"n":2}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let json = body_json(resp).await;
    assert!(
        json.get("type").is_none(),
        "must be OpenAI envelope, not Anthropic: {json}"
    );
    assert_eq!(json["error"]["code"], "UNPROCESSABLE_ENTITY");
    let msg = json["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("n > 1 is not supported"),
        "message must name the reject reason: {msg}"
    );
}

#[tokio::test]
async fn chat_n1_and_absent_n_pass_entry_gate() {
    // n:1 and omitted n must pass the entry check and reach route
    // resolution (unknown model → 404 no_route proves the gate passed).
    let state = build_state("http://127.0.0.1:1").await;
    let app = build_router(state, DispatchRuntime::default());

    for body in [
        r#"{"model":"does-not-exist","messages":[{"role":"user","content":"hi"}],"n":1}"#,
        r#"{"model":"does-not-exist","messages":[{"role":"user","content":"hi"}]}"#,
    ] {
        let resp = app.clone().oneshot(chat_request(body)).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "n<=1 / absent must pass entry and hit no_route: {body}"
        );
        let json = body_json(resp).await;
        assert_eq!(json["error"]["code"], "MODEL_NOT_FOUND");
    }
}

// ── missing `content` tolerance (zcode regression) ───────────────

/// Spawn a mock upstream that captures the full client request
/// (headers + body, Content-Length framed — unlike [`spawn_server`] it
/// does NOT pre-drain) and replies with a fixed non-stream 200
/// completion body. Returns the base URL and the captured request bytes.
async fn spawn_capturing_server(response: &'static [u8]) -> (String, Arc<Mutex<Vec<u8>>>) {
    let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let cap = captured.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = stream.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&buf);
            let Some(idx) = text.find("\r\n\r\n") else {
                continue;
            };
            let content_length = text[..idx]
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    if k.trim().eq_ignore_ascii_case("content-length") {
                        v.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            if buf.len() >= idx + 4 + content_length {
                break;
            }
        }
        *cap.lock().unwrap() = buf;
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response.len()
        );
        let _ = stream.write_all(header.as_bytes()).await;
        let _ = stream.write_all(response).await;
        let _ = stream.flush().await;
        let _ = stream.shutdown().await;
    });
    (format!("http://{addr}"), captured)
}

#[tokio::test]
async fn chat_assistant_tool_calls_without_content_passes_ingress() {
    // zcode regression: an assistant message carrying only `tool_calls`
    // and no `content` key is legal per OpenAI ("Required unless
    // tool_calls or function_call is specified"). Before the fix, ingress
    // deserialization rejected it with 422 "missing field `content`".
    // Now it must pass the extractor and reach route resolution
    // (unknown model → 404 no_route proves the gate passed).
    let state = build_state("http://127.0.0.1:1").await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app
        .oneshot(chat_request(
            r#"{"model":"does-not-exist","messages":[
                {"role":"user","content":"hi"},
                {"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}]}
            ]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let json = body_json(resp).await;
    assert_eq!(json["error"]["code"], "MODEL_NOT_FOUND");
}

#[tokio::test]
async fn chat_message_without_content_forwards_content_null_to_upstream() {
    // Native OpenAI passthrough re-serializes the request; a message that
    // omitted `content` must reach upstream as "content": null (the
    // canonical OpenAI form for assistant tool_calls messages), not be
    // dropped or rejected. Unlike [`spawn_server`], the capturing upstream
    // does not pre-drain the request: it frames on Content-Length and hands
    // back the exact bytes the gateway sent, which is what the passthrough
    // assertions below inspect.
    let body: &'static [u8] = br#"{"id":"chatcmpl-c1","object":"chat.completion","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
    let (base_url, captured) = spawn_capturing_server(body).await;

    let state = build_state(&base_url).await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app
        .oneshot(chat_request(
            r#"{"model":"gpt-4o","messages":[
                {"role":"user","content":"hi"},
                {"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}]}
            ]}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let raw = captured.lock().unwrap().clone();
    let text = String::from_utf8(raw).unwrap();
    let body_start = text.find("\r\n\r\n").expect("header/body separator") + 4;
    let forwarded: serde_json::Value = serde_json::from_str(&text[body_start..]).unwrap();
    let msgs = forwarded["messages"].as_array().unwrap();
    assert_eq!(msgs[1]["role"], serde_json::json!("assistant"));
    assert_eq!(
        msgs[1].get("content"),
        Some(&serde_json::json!(null)),
        "omitted content must re-serialize as content:null, not be dropped: {:?}",
        msgs[1]
    );
    assert!(msgs[1]["tool_calls"].is_array());
}

// ── stream byte gate ─────────────────────────────────────────────

#[tokio::test]
async fn chat_stream_body_is_byte_identical_to_upstream_data_frames() {
    // Fixed SSE payload. No `usage` field on content frames so
    // `process_sse_frame` forwards each Data frame verbatim; first frame
    // carries content so `validate_stream_first_chunk` does not trip the
    // empty-response retry.
    let frames: [&[u8]; 3] = [
        br#"data: {"id":"chatcmpl-s1","object":"chat.completion.chunk","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}"#,
        br#"data: {"id":"chatcmpl-s1","object":"chat.completion.chunk","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        b"data: [DONE]",
    ];
    // Normalize to the exact on-wire form the fixture serves (and that
    // `process_sse_frame` re-emits): `data: <payload>\n\n`.
    let mut expected_body: Vec<u8> = Vec::new();
    for f in frames.iter() {
        let frame = format!("{}\n\n", std::str::from_utf8(f).unwrap());
        expected_body.extend_from_slice(frame.as_bytes());
    }

    let expected = expected_body.clone();
    let base_url = spawn_server(move |mut stream| async move {
        let header =
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";
        stream.write_all(header.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();
        stream.write_all(&expected).await.unwrap();
        stream.flush().await.unwrap();
        let _ = stream.shutdown().await;
    })
    .await;

    let state = build_state(&base_url).await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app
        .oneshot(chat_request(
            r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        body.as_ref(),
        expected_body.as_slice(),
        "stream gateway body must be the byte-exact concatenation of upstream \
         Data frames including [DONE]"
    );
}
