//! Native-lane override chain tests (plan §10.6 / R3 / C8).
//!
//! On the **native path** (Responses native for the full chain, Anthropic
//! native for a slice), assert each layer of the three-tier override chain
//! against the **captured upstream request body**:
//!
//! 1. `body_overrides` — provider-level JSON override wins over client value
//! 2. `strip_fields` — listed top-level fields absent from the upstream body
//! 3. `model_strip_fields` — strip keyed by the post-substitution `real_model`
//! 4. `real_model` — upstream body `model` is the substitution target
//! 5. `target.extra` — extra fields merged into the upstream body
//! 6. Full-chain body equals the expected body under **normalized JSON**
//!    equality (key order / whitespace insensitive)
//! 7. (optional) Native stream usage side-channel does not write into the
//!    client-visible byte stream

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use sag_config::{
    ApiConfig, ApiKind, ConfigHandle, GatewayAuthConfig, GatewayConfig, KeyStrategy, Pipeline,
    ProviderConfig, ProxyMode, RouteConfig, RouteTarget, Strategy,
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

// ── helpers ───────────────────────────────────────────────────────

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

/// Parse the captured upstream HTTP request → `(request_line, JSON body)`.
fn parse_upstream(req: &[u8]) -> (String, serde_json::Value) {
    let text = String::from_utf8_lossy(req);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .expect("upstream request must have headers/body separator");
    let request_line = head.lines().next().unwrap_or_default().to_string();
    let json: serde_json::Value =
        serde_json::from_str(body).expect("upstream body must be valid JSON");
    (request_line, json)
}

// ── state builders ────────────────────────────────────────────────

/// Provider-level override knobs for the Responses-native state.
#[derive(Default, Clone)]
struct ResponsesOverrideCfg {
    body_overrides: HashMap<String, serde_json::Value>,
    strip_fields: Vec<String>,
    model_strip_fields: HashMap<String, Vec<String>>,
    target_extra: HashMap<String, serde_json::Value>,
    real_model: String,
}

async fn build_responses_native_state(
    base_url: &str,
    key: &str,
    cfg: ResponsesOverrideCfg,
) -> AppState {
    let mut providers = HashMap::new();
    let mut pc = ProviderConfig {
        api_keys: vec!["sk-test".into()],
        key_strategy: KeyStrategy::FillFirst,
        proxy_mode: ProxyMode::default(),
        timeout_secs: 10,
        headers: HashMap::new(),
        body_overrides: cfg.body_overrides,
        strip_fields: cfg.strip_fields,
        model_strip_fields: cfg.model_strip_fields,
        model_overrides: HashMap::new(),
        role_mappings: HashMap::new(),
        enabled_models: vec![],
        enabled: true,
        anthropic_cache_control: false,
        chunk_timeout_secs: None,
        ttfb_timeout_secs: None,
        ..ProviderConfig::default()
    };
    // Declaring `responses` → ingress ∈ protocol keys → Native lane on
    // /v1/responses; the upstream path is derived (POST /responses).
    pc.apis.insert(
        ApiKind::Responses,
        ApiConfig {
            base_url: base_url.to_string(),
            ..ApiConfig::default()
        },
    );
    providers.insert("openai".to_string(), pc);
    let config = GatewayConfig {
        gateway_auth: Some(GatewayAuthConfig {
            api_keys: vec![key.to_string()],
        }),
        providers,
        routes: vec![RouteConfig {
            session_ttl_secs: None,
            session_max_entries: None,
            model_name: "route-model".into(),
            pipeline: Pipeline::Chat,
            api_endpoint: None,
            retries: Some(0),
            timeout_secs: None,
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                extra: cfg.target_extra,
                retries: Some(0),
                timeout_secs: None,
                provider: "openai".into(),
                real_model: Some(cfg.real_model),
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

/// Provider-level override knobs for the Anthropic-native state.
#[derive(Default, Clone)]
struct AnthropicOverrideCfg {
    body_overrides: HashMap<String, serde_json::Value>,
    strip_fields: Vec<String>,
    model_strip_fields: HashMap<String, Vec<String>>,
    target_extra: HashMap<String, serde_json::Value>,
    real_model: String,
}

async fn build_anthropic_native_state(
    base_url: &str,
    key: &str,
    cfg: AnthropicOverrideCfg,
) -> AppState {
    let mut providers = HashMap::new();
    let mut pc = ProviderConfig {
        api_keys: vec!["sk-ant-test".into()],
        key_strategy: KeyStrategy::FillFirst,
        proxy_mode: ProxyMode::default(),
        timeout_secs: 10,
        headers: HashMap::new(),
        body_overrides: cfg.body_overrides,
        strip_fields: cfg.strip_fields,
        model_strip_fields: cfg.model_strip_fields,
        model_overrides: HashMap::new(),
        role_mappings: HashMap::new(),
        enabled_models: vec![],
        enabled: true,
        anthropic_cache_control: false,
        chunk_timeout_secs: None,
        ttfb_timeout_secs: None,
        ..ProviderConfig::default()
    };
    // Declaring `anthropic_messages` → ingress ∈ protocol keys → Native
    // lane on /v1/messages; the upstream path is derived (POST /messages).
    pc.apis.insert(
        ApiKind::AnthropicMessages,
        ApiConfig {
            base_url: base_url.to_string(),
            ..ApiConfig::default()
        },
    );
    providers.insert("anthropic".to_string(), pc);
    let config = GatewayConfig {
        gateway_auth: Some(GatewayAuthConfig {
            api_keys: vec![key.to_string()],
        }),
        providers,
        routes: vec![RouteConfig {
            session_ttl_secs: None,
            session_max_entries: None,
            model_name: "claude-native".into(),
            pipeline: Pipeline::Chat,
            api_endpoint: None,
            retries: Some(0),
            timeout_secs: None,
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                extra: cfg.target_extra,
                retries: Some(0),
                timeout_secs: None,
                provider: "anthropic".into(),
                real_model: Some(cfg.real_model),
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

fn responses_request(body: &str, key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/responses")
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

// ── Responses native: full chain + per-item assertions ────────────

/// Mock Responses JSON returned for non-stream native calls.
const RESPONSES_OK: &str = r#"{
    "id": "resp_ov",
    "object": "response",
    "created_at": 1700000000,
    "status": "completed",
    "model": "gpt-real",
    "output": [
        {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
         "content": [{"type": "output_text", "text": "ok", "annotations": []}]}
    ],
    "usage": {"input_tokens": 3, "output_tokens": 2, "total_tokens": 5}
}"#;

/// Client body used across the Responses override tests: includes a client
/// `temperature` (overridden), a `user` field (stripped), and a `metadata`
/// field (stripped by `model_strip_fields` keyed on `real_model`).
const RESPONSES_CLIENT_BODY: &str = r#"{
    "model": "route-model",
    "input": "hi",
    "temperature": 0.1,
    "user": "alice",
    "metadata": {"a": 1}
}"#;

fn responses_full_cfg() -> ResponsesOverrideCfg {
    let mut body_overrides = HashMap::new();
    body_overrides.insert("temperature".to_string(), serde_json::json!(0.9));
    let mut model_strip_fields = HashMap::new();
    model_strip_fields.insert("gpt-real".to_string(), vec!["metadata".to_string()]);
    let mut target_extra = HashMap::new();
    target_extra.insert("seed".to_string(), serde_json::json!(42));
    ResponsesOverrideCfg {
        body_overrides,
        strip_fields: vec!["user".to_string()],
        model_strip_fields,
        target_extra,
        real_model: "gpt-real".to_string(),
    }
}

#[tokio::test]
async fn responses_native_body_overrides_temperature() {
    let expected = RESPONSES_OK.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            respond_json(&mut stream, &expected).await;
        }
    })
    .await;

    let mut cfg = ResponsesOverrideCfg {
        real_model: "gpt-real".into(),
        ..Default::default()
    };
    cfg.body_overrides
        .insert("temperature".to_string(), serde_json::json!(0.9));

    let state = build_responses_native_state(&base_url, "ov-key", cfg).await;
    let app = build_router(state, DispatchRuntime::default());
    let resp = app
        .oneshot(responses_request(RESPONSES_CLIENT_BODY, "ov-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = req_rx.await.expect("request bytes");
    let (_, body) = parse_upstream(&req);
    // Provider body_overrides replaces the client's temperature (0.1 → 0.9).
    assert_eq!(
        body["temperature"].as_f64(),
        Some(0.9),
        "body_overrides must replace client temperature: {body}"
    );
    assert_ne!(
        body["temperature"].as_f64(),
        Some(0.1),
        "client temperature must not survive: {body}"
    );
}

#[tokio::test]
async fn responses_native_strip_fields_removes_user() {
    let expected = RESPONSES_OK.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            respond_json(&mut stream, &expected).await;
        }
    })
    .await;

    let cfg = ResponsesOverrideCfg {
        strip_fields: vec!["user".to_string()],
        real_model: "gpt-real".into(),
        ..Default::default()
    };
    let state = build_responses_native_state(&base_url, "ov-key", cfg).await;
    let app = build_router(state, DispatchRuntime::default());
    let resp = app
        .oneshot(responses_request(RESPONSES_CLIENT_BODY, "ov-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = req_rx.await.expect("request bytes");
    let (_, body) = parse_upstream(&req);
    assert!(
        body.get("user").is_none(),
        "strip_fields must remove `user` from the upstream body: {body}"
    );
    // Sanity: the body is still a valid object (strip is top-level only).
    assert!(body.is_object());
}

#[tokio::test]
async fn responses_native_model_strip_fields_by_real_model() {
    let expected = RESPONSES_OK.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            respond_json(&mut stream, &expected).await;
        }
    })
    .await;

    let mut model_strip_fields = HashMap::new();
    model_strip_fields.insert("gpt-real".to_string(), vec!["metadata".to_string()]);
    let cfg = ResponsesOverrideCfg {
        model_strip_fields,
        real_model: "gpt-real".into(),
        ..Default::default()
    };
    let state = build_responses_native_state(&base_url, "ov-key", cfg).await;
    let app = build_router(state, DispatchRuntime::default());
    let resp = app
        .oneshot(responses_request(RESPONSES_CLIENT_BODY, "ov-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = req_rx.await.expect("request bytes");
    let (_, body) = parse_upstream(&req);
    // model_strip_fields is keyed on the post-substitution `real_model`.
    assert_eq!(
        body["model"], "gpt-real",
        "real_model must be applied first"
    );
    assert!(
        body.get("metadata").is_none(),
        "model_strip_fields[gpt-real] must remove `metadata`: {body}"
    );
}

#[tokio::test]
async fn responses_native_real_model_replaces_model() {
    let expected = RESPONSES_OK.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            respond_json(&mut stream, &expected).await;
        }
    })
    .await;

    let cfg = ResponsesOverrideCfg {
        real_model: "gpt-real".into(),
        ..Default::default()
    };
    let state = build_responses_native_state(&base_url, "ov-key", cfg).await;
    let app = build_router(state, DispatchRuntime::default());
    // Client asks for route-model; target real_model is gpt-real.
    let resp = app
        .oneshot(responses_request(RESPONSES_CLIENT_BODY, "ov-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = req_rx.await.expect("request bytes");
    let (request_line, body) = parse_upstream(&req);
    assert!(
        request_line.starts_with("POST /responses "),
        "native path must be /responses: {request_line}"
    );
    assert_eq!(
        body["model"], "gpt-real",
        "upstream body model must be the real_model substitution: {body}"
    );
    assert_ne!(
        body["model"], "route-model",
        "route model must not leak upstream: {body}"
    );
}

#[tokio::test]
async fn responses_native_target_extra_merges_seed() {
    let expected = RESPONSES_OK.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            respond_json(&mut stream, &expected).await;
        }
    })
    .await;

    let mut target_extra = HashMap::new();
    target_extra.insert("seed".to_string(), serde_json::json!(42));
    let cfg = ResponsesOverrideCfg {
        target_extra,
        real_model: "gpt-real".into(),
        ..Default::default()
    };
    let state = build_responses_native_state(&base_url, "ov-key", cfg).await;
    let app = build_router(state, DispatchRuntime::default());
    let resp = app
        .oneshot(responses_request(RESPONSES_CLIENT_BODY, "ov-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = req_rx.await.expect("request bytes");
    let (_, body) = parse_upstream(&req);
    assert_eq!(
        body["seed"].as_i64(),
        Some(42),
        "target.extra must merge into the upstream body: {body}"
    );
}

#[tokio::test]
async fn responses_native_full_chain_normalized_equality() {
    // Full §10.6 chain in one request: body_overrides + strip_fields +
    // model_strip_fields + real_model + target.extra. Assert the captured
    // upstream body equals the expected body under normalized JSON equality.
    let expected_resp = RESPONSES_OK.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected_resp.clone();
        async move {
            respond_json(&mut stream, &expected).await;
        }
    })
    .await;

    let cfg = responses_full_cfg();
    let state = build_responses_native_state(&base_url, "ov-key", cfg).await;
    let app = build_router(state, DispatchRuntime::default());
    let resp = app
        .oneshot(responses_request(RESPONSES_CLIENT_BODY, "ov-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = req_rx.await.expect("request bytes");
    let (request_line, actual) = parse_upstream(&req);
    assert!(
        request_line.starts_with("POST /responses "),
        "got {request_line}"
    );

    // Expected after: encode(real_model) → body_overrides → target_extra →
    // strip_fields ∪ model_strip_fields[real_model].
    //
    // Pure-text EasyInputMessage content is emitted as a scalar string
    // (OpenAI `content: string | InputMessageContentList`).
    let expected_body = serde_json::json!({
        "model": "gpt-real",
        "input": [{
            "type": "message",
            "role": "user",
            "content": "hi"
        }],
        "temperature": 0.9,
        "seed": 42,
        // "user" stripped by strip_fields
        // "metadata" stripped by model_strip_fields["gpt-real"]
    });

    assert_eq!(
        normalize(actual.clone()),
        normalize(expected_body.clone()),
        "full override chain must produce the expected body (normalized equality)\nactual={actual:#}\nexpected={expected_body:#}"
    );
}

// ── Anthropic native: override chain slice ────────────────────────

const ANTHROPIC_OK: &str = r#"{
    "id": "msg_ov",
    "type": "message",
    "role": "assistant",
    "model": "claude-real",
    "content": [{"type": "text", "text": "ok"}],
    "stop_reason": "end_turn",
    "usage": {"input_tokens": 4, "output_tokens": 2}
}"#;

const ANTHROPIC_CLIENT_BODY: &str = r#"{
    "model": "claude-native",
    "max_tokens": 100,
    "messages": [{"role": "user", "content": "hi"}],
    "temperature": 0.2,
    "user": "bob"
}"#;

#[tokio::test]
async fn anthropic_native_body_overrides_and_strip_fields() {
    // Anthropic native slice: body_overrides replaces temperature;
    // strip_fields removes `user`; real_model substitution is asserted
    // as part of the same captured body.
    let expected = ANTHROPIC_OK.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            respond_json(&mut stream, &expected).await;
        }
    })
    .await;

    let mut body_overrides = HashMap::new();
    body_overrides.insert("temperature".to_string(), serde_json::json!(0.9));
    let cfg = AnthropicOverrideCfg {
        body_overrides,
        strip_fields: vec!["user".to_string()],
        real_model: "claude-real".into(),
        ..Default::default()
    };
    let state = build_anthropic_native_state(&base_url, "ov-key", cfg).await;
    let app = build_router(state, DispatchRuntime::default());
    let resp = app
        .oneshot(messages_request(ANTHROPIC_CLIENT_BODY, "ov-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = req_rx.await.expect("request bytes");
    let (request_line, body) = parse_upstream(&req);
    assert!(
        request_line.starts_with("POST /messages "),
        "Anthropic native must hit /messages: {request_line}"
    );

    // 1. body_overrides: temperature client 0.2 → 0.9
    assert_eq!(
        body["temperature"].as_f64(),
        Some(0.9),
        "body_overrides must replace temperature: {body}"
    );
    // 2. strip_fields: user absent
    assert!(
        body.get("user").is_none(),
        "strip_fields must remove `user`: {body}"
    );
    // 4. real_model substitution
    assert_eq!(
        body["model"], "claude-real",
        "real_model must replace model: {body}"
    );
    // Shape sanity: Anthropic body, not Chat.
    assert!(
        body.get("messages").is_some(),
        "must carry messages: {body}"
    );
    assert_eq!(body["max_tokens"].as_u64(), Some(100));
    assert!(body.get("choices").is_none(), "must not be Chat-shaped");
}

#[tokio::test]
async fn anthropic_native_target_extra_and_model_strip() {
    // Anthropic native slice: target.extra merges seed; model_strip_fields
    // keyed on real_model removes temperature (simulating a model-scoped strip
    // of a field that survived encode via raw).
    let expected = ANTHROPIC_OK.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            respond_json(&mut stream, &expected).await;
        }
    })
    .await;

    let mut target_extra = HashMap::new();
    target_extra.insert("seed".to_string(), serde_json::json!(42));
    let mut model_strip_fields = HashMap::new();
    // temperature is a known field emitted by encode — strip it by real_model key.
    model_strip_fields.insert("claude-real".to_string(), vec!["temperature".to_string()]);
    let cfg = AnthropicOverrideCfg {
        target_extra,
        model_strip_fields,
        real_model: "claude-real".into(),
        ..Default::default()
    };
    let state = build_anthropic_native_state(&base_url, "ov-key", cfg).await;
    let app = build_router(state, DispatchRuntime::default());
    let resp = app
        .oneshot(messages_request(ANTHROPIC_CLIENT_BODY, "ov-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = req_rx.await.expect("request bytes");
    let (_, body) = parse_upstream(&req);

    // 5. target.extra
    assert_eq!(
        body["seed"].as_i64(),
        Some(42),
        "target.extra must merge seed: {body}"
    );
    // 3. model_strip_fields keyed by real_model
    assert_eq!(body["model"], "claude-real");
    assert!(
        body.get("temperature").is_none(),
        "model_strip_fields[claude-real] must remove temperature: {body}"
    );
}

#[tokio::test]
async fn anthropic_native_full_chain_normalized_equality() {
    // Full chain on Anthropic native with normalized JSON equality of the
    // captured upstream body against the expected post-override body.
    let expected_resp = ANTHROPIC_OK.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected_resp.clone();
        async move {
            respond_json(&mut stream, &expected).await;
        }
    })
    .await;

    let mut body_overrides = HashMap::new();
    body_overrides.insert("temperature".to_string(), serde_json::json!(0.9));
    let mut target_extra = HashMap::new();
    target_extra.insert("seed".to_string(), serde_json::json!(42));
    let cfg = AnthropicOverrideCfg {
        body_overrides,
        strip_fields: vec!["user".to_string()],
        target_extra,
        real_model: "claude-real".into(),
        ..Default::default()
    };
    let state = build_anthropic_native_state(&base_url, "ov-key", cfg).await;
    let app = build_router(state, DispatchRuntime::default());
    let resp = app
        .oneshot(messages_request(ANTHROPIC_CLIENT_BODY, "ov-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = req_rx.await.expect("request bytes");
    let (_, actual) = parse_upstream(&req);

    // Anthropic encode of a single pure-text user turn collapses content
    // to a bare string; max_tokens is always present; user stripped;
    // temperature overridden; seed merged.
    let expected_body = serde_json::json!({
        "model": "claude-real",
        "max_tokens": 100,
        "messages": [{"role": "user", "content": "hi"}],
        "temperature": 0.9,
        "seed": 42,
    });

    assert_eq!(
        normalize(actual.clone()),
        normalize(expected_body.clone()),
        "Anthropic native full chain must match expected body (normalized)\nactual={actual:#}\nexpected={expected_body:#}"
    );
}

// ── §10.6 optional: native stream usage side-channel ──────────────

#[tokio::test]
async fn responses_native_stream_usage_bypass_not_in_client_bytes() {
    // Native Responses stream: usage is extracted on a side channel from
    // `response.completed` for analytics; the client byte stream must be the
    // upstream frames verbatim (no synthesized usage frame injected).
    let upstream_sse = concat!(
        r#"data: {"type":"response.created","response":{"id":"r_u","object":"response","status":"in_progress","output":[]}}"#,
        "\n\n",
        r#"data: {"type":"response.output_text.delta","item_id":"m1","output_index":0,"content_index":0,"delta":"tok"}"#,
        "\n\n",
        r#"data: {"type":"response.completed","response":{"id":"r_u","object":"response","status":"completed","usage":{"input_tokens":11,"output_tokens":7,"total_tokens":18}}}"#,
        "\n\n",
    );
    let expected = upstream_sse.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            respond_sse(&mut stream, &expected).await;
        }
    })
    .await;

    let cfg = ResponsesOverrideCfg {
        real_model: "gpt-real".into(),
        ..Default::default()
    };
    let state = build_responses_native_state(&base_url, "ov-key", cfg).await;
    let app = build_router(state, DispatchRuntime::default());

    let body = r#"{"model":"route-model","input":"hi","stream":true}"#;
    let resp = app
        .oneshot(responses_request(body, "ov-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let req = req_rx.await.expect("request bytes");
    let (request_line, upstream_body) = parse_upstream(&req);
    assert!(
        request_line.starts_with("POST /responses "),
        "got {request_line}"
    );
    assert_eq!(upstream_body["stream"], true);

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);

    // Client stream carries the upstream data frames as-is.
    for marker in [
        "response.created",
        "response.output_text.delta",
        "response.completed",
        "tok",
    ] {
        assert!(text.contains(marker), "missing {marker}: {text}");
    }
    assert!(!text.contains("[DONE]"), "must not emit [DONE]: {text}");

    // Usage side-channel: the response.completed frame (which carries usage)
    // is forwarded once — not duplicated by a synthesized usage-only frame.
    let completed_count = text.matches("response.completed").count();
    assert_eq!(
        completed_count, 1,
        "response.completed must appear exactly once (usage side-channel must not inject a second frame): {text}"
    );

    // Parse data frames and verify each is one of the upstream types —
    // no extra "usage" pseudo-frame was appended for the client.
    let data_payloads: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<serde_json::Value>(d).ok())
        .collect();
    assert_eq!(data_payloads.len(), 3, "exactly 3 data frames expected");
    for v in &data_payloads {
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        assert!(
            matches!(
                ty,
                "response.created" | "response.output_text.delta" | "response.completed"
            ),
            "unexpected client frame type {ty}: {v}"
        );
    }
}
