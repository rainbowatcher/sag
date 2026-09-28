//! Image / pipeline end-to-end regression (plan §10 image/pipeline item).
//!
//! Covers the three `pipeline == Image` discrimination sites in
//! `sag-router/src/routes.rs` after the protocol-catalog migration:
//!
//! 1. `/v1/chat/completions` + `pipeline = "image"` → real image dispatch:
//!    the captured upstream request must hit
//!    `apis.image.base_url` + route.`api_endpoint` through the real
//!    `CatalogProvider::chat_image(request, real_model, api_endpoint, …)`
//!    chain, and the image response passes back to the client.
//! 2. `/v1/responses` + image route → 422 (OpenAI envelope), no dispatch.
//! 3. `/v1/messages` + image route → 400 (Anthropic envelope), no dispatch.
//!
//! A chat-pipeline route never accepts `api_endpoint` (rejected at config
//! validation — `rejects_chat_route_with_api_endpoint` in sag-config).

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

/// Parse captured upstream HTTP request bytes → `(request_line, JSON body)`.
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

// ── state ─────────────────────────────────────────────────────────

/// State with one image-pipeline route (`dall-e-3`) backed by a provider
/// declaring `apis.image` (address) + `apis.chat_completions` (protocol key).
async fn build_image_state(base_url: &str, key: &str) -> AppState {
    let mut pc = ProviderConfig {
        api_keys: vec!["sk-test".into()],
        key_strategy: KeyStrategy::FillFirst,
        proxy_mode: ProxyMode::default(),
        timeout_secs: 10,
        anthropic_cache_control: false,
        ..ProviderConfig::default()
    };
    pc.apis.insert(
        ApiKind::Image,
        ApiConfig {
            base_url: base_url.to_string(),
            ..ApiConfig::default()
        },
    );
    pc.apis.insert(
        ApiKind::ChatCompletions,
        ApiConfig {
            base_url: base_url.to_string(),
            ..ApiConfig::default()
        },
    );
    let mut providers = HashMap::new();
    providers.insert("openai".to_string(), pc);

    let config = GatewayConfig {
        gateway_auth: Some(GatewayAuthConfig {
            api_keys: vec![key.to_string()],
        }),
        providers,
        routes: vec![RouteConfig {
            model_name: "dall-e-3".into(),
            pipeline: Pipeline::Image,
            api_endpoint: Some("/images/generations".into()),
            retries: Some(0),
            timeout_secs: None,
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                extra: HashMap::new(),
                retries: Some(0),
                timeout_secs: None,
                provider: "openai".into(),
                real_model: Some("dall-e-3".into()),
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

fn chat_image_request(body: &str, key: &str) -> Request<Body> {
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

// ── tests ─────────────────────────────────────────────────────────

#[tokio::test]
async fn image_pipeline_dispatch_hits_api_endpoint_and_returns_image() {
    let upstream_resp = r#"{
        "created": 1700000000,
        "data": [{"url": "https://cdn.example/cat.png"}]
    }"#;
    let expected = upstream_resp.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            respond_json(&mut stream, &expected).await;
        }
    })
    .await;

    let state = build_image_state(&base_url, "img-key").await;
    let app = build_router(state, DispatchRuntime::default());

    let client_body = r#"{
        "model": "dall-e-3",
        "messages": [{"role": "user", "content": "a cat"}],
        "n": 1
    }"#;
    let resp = app
        .oneshot(chat_image_request(client_body, "img-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Upstream URL = apis.image.base_url + route.api_endpoint.
    let req = req_rx.await.expect("upstream request bytes");
    let (request_line, body) = parse_upstream(&req);
    assert!(
        request_line.starts_with("POST /images/generations "),
        "image pipeline must POST to the route api_endpoint: {request_line}"
    );

    // chat_image signature chain: prompt extracted from the last message,
    // real_model substitution applied (both equal here, asserted anyway).
    assert_eq!(body["prompt"], "a cat", "prompt must be extracted: {body}");
    assert_eq!(body["model"], "dall-e-3", "model must be forwarded: {body}");

    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json["data"][0]["url"], "https://cdn.example/cat.png",
        "image response must pass through to the client: {json}"
    );
}

#[tokio::test]
async fn image_pipeline_rejected_on_responses_and_messages_ingress() {
    // No upstream needed — both ingresses reject before dispatch. Point
    // the apis nodes at an unroutable address to prove nothing is sent.
    let state = build_image_state("http://127.0.0.1:1", "img-key").await;
    let app = build_router(state, DispatchRuntime::default());

    // /v1/responses → 422, OpenAI envelope.
    let resp = app
        .clone()
        .oneshot(responses_request(
            r#"{"model":"dall-e-3","input":"hi"}"#,
            "img-key",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("image generation is not supported on /v1/responses"),
        "expected image rejection envelope, got {json}"
    );

    // /v1/messages → 400, Anthropic envelope.
    let resp = app
        .oneshot(messages_request(
            r#"{"model":"dall-e-3","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#,
            "img-key",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["type"], "error", "anthropic envelope shape: {json}");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("image generation is not supported on /v1/messages"),
        "expected anthropic image rejection, got {json}"
    );
}
