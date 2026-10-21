//! Response-compression integration tests (`tower-http::compression`).
//!
//! These drive the real [`build_router`] stack, so they pin the two things
//! that could silently regress in `sag-router/src/routes.rs`:
//!
//! 1. the layer sits *outside* `http_log` (the log middleware keeps seeing
//!    plaintext + `content-length`), and
//! 2. the predicate is `SizeAbove::new(1024)` plus the SSE / gRPC / image
//!    exclusions — i.e. small JSON stays identity and `text/event-stream`
//!    streams are never compressed (tower-http treats unknown body sizes as
//!    "compress", which would otherwise catch every SSE response).
//!
//! Compressed responses are chunked (tower-http strips `content-length`), so
//! bodies are collected in memory through the `oneshot` service rather than
//! read off a TCP socket.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{HeaderMap, Request, StatusCode, header},
};
use flate2::read::GzDecoder;
use http_body_util::BodyExt;
use sag_config::{
    ApiConfig, ConfigHandle, GatewayAuthConfig, GatewayConfig, KeyStrategy, ProviderConfig,
    ProxyMode, RouteConfig, RouteTarget, Strategy,
};
use sag_router::{AppState, DispatchRuntime, build_router, test_app_state};
use tower::ServiceExt;

const AUTH_KEY: &str = "compression-key";

/// Compression floor asserted by the fixture (mirrors the router predicate).
const SIZE_FLOOR: usize = 1024;

/// Number of filler routes on `/v1/models`. Each entry is >150 bytes once
/// `ingress_protocols` / `lanes` are included, so this comfortably clears the
/// 1 KiB floor (the tests assert the resulting size, they don't assume it).
const FILLER_ROUTES: usize = 12;

// ── fixtures ─────────────────────────────────────────────────────

fn provider_config(base_url: &str) -> ProviderConfig {
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
}

fn route_config(model_name: &str) -> RouteConfig {
    RouteConfig {
        session_ttl_secs: None,
        session_max_entries: None,
        model_name: model_name.into(),
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
            real_model: Some(model_name.into()),
            priority: 1,
            capabilities: None,
        }],
    }
}

/// State with one chat route (`gpt-4o`) plus enough filler routes that
/// `/v1/models` answers with a >1 KiB JSON body.
async fn build_state(base_url: &str) -> AppState {
    let mut providers = HashMap::new();
    providers.insert("openai".to_string(), provider_config(base_url));

    let mut routes = vec![route_config("gpt-4o")];
    for i in 0..FILLER_ROUTES {
        routes.push(route_config(&format!(
            "filler-model-{i:02}-with-a-deliberately-long-name"
        )));
    }

    let config = GatewayConfig {
        gateway_auth: Some(GatewayAuthConfig {
            api_keys: vec![AUTH_KEY.into()],
        }),
        providers,
        routes,
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

async fn build_app(base_url: &str) -> Router {
    build_router(build_state(base_url).await, DispatchRuntime::default())
}

fn authed_get(uri: &str, accept_encoding: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {AUTH_KEY}"));
    if let Some(value) = accept_encoding {
        builder = builder.header(header::ACCEPT_ENCODING, value);
    }
    builder.body(Body::empty()).unwrap()
}

fn chat_request(body: &str, accept_encoding: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::AUTHORIZATION, format!("Bearer {AUTH_KEY}"))
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(value) = accept_encoding {
        builder = builder.header(header::ACCEPT_ENCODING, value);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

/// `(status, headers, fully collected body)` for a single request.
async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let resp = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = resp.into_parts();
    let bytes = body.collect().await.unwrap().to_bytes();
    (parts.status, parts.headers, bytes.to_vec())
}

fn content_encoding(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::CONTENT_ENCODING)
        .map(|v| v.to_str().unwrap().to_string())
}

fn vary_mentions_accept_encoding(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::VARY)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.to_ascii_lowercase().contains("accept-encoding"))
}

fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .expect("gzip body must decode");
    out
}

/// `/v1/models` body fetched without `Accept-Encoding`, asserting the
/// fixture really is above the compression floor so the compression
/// assertions below can't pass vacuously.
async fn large_models_body(app: &Router) -> Vec<u8> {
    let (status, headers, body) = send(app, authed_get("/v1/models", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        content_encoding(&headers).is_none(),
        "the baseline fetch must stay identity"
    );
    assert!(
        body.len() > SIZE_FLOOR,
        "/v1/models fixture must exceed the {SIZE_FLOOR}-byte compression floor, got {} bytes",
        body.len()
    );
    body
}

/// One-shot TCP server answering with a raw HTTP response (same helper shape
/// as `tests/chat_bytes_regression.rs`).
async fn spawn_upstream(content_type: &'static str, payload: Vec<u8>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf).await;
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        stream.write_all(header.as_bytes()).await.unwrap();
        stream.write_all(&payload).await.unwrap();
        stream.flush().await.unwrap();
        let _ = stream.shutdown().await;
    });
    format!("http://{addr}")
}

// ── non-stream JSON compression ──────────────────────────────────

#[tokio::test]
async fn large_json_is_gzipped_and_decodes_to_the_identity_body() {
    let app = build_app("http://127.0.0.1:1").await;
    let identity_body = large_models_body(&app).await;

    let (status, headers, body) = send(&app, authed_get("/v1/models", Some("gzip"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        content_encoding(&headers).as_deref(),
        Some("gzip"),
        "a >1 KiB JSON response must be gzip-encoded when the client accepts gzip"
    );
    assert!(
        vary_mentions_accept_encoding(&headers),
        "compressed responses must advertise `Vary: accept-encoding` for caches"
    );
    assert!(
        !headers.contains_key(header::CONTENT_LENGTH),
        "tower-http strips content-length on compressed responses"
    );
    assert!(
        body.len() < identity_body.len(),
        "gzip body ({} bytes) must be smaller than the identity body ({} bytes)",
        body.len(),
        identity_body.len()
    );
    assert_eq!(
        gunzip(&body),
        identity_body,
        "the decompressed body must be byte-identical to the identity response"
    );
}

#[tokio::test]
async fn large_json_is_brotli_encoded() {
    let app = build_app("http://127.0.0.1:1").await;
    let identity_body = large_models_body(&app).await;

    let (status, headers, body) = send(&app, authed_get("/v1/models", Some("br"))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        content_encoding(&headers).as_deref(),
        Some("br"),
        "brotli must be negotiated when the client only accepts br"
    );
    assert!(vary_mentions_accept_encoding(&headers));
    assert!(body.len() < identity_body.len());
}

#[tokio::test]
async fn a_single_encoding_is_chosen_when_several_are_acceptable() {
    // tower-http ranks by q-value then by encoding preference; a plain
    // `gzip, br` header must still yield exactly one `Content-Encoding`.
    let app = build_app("http://127.0.0.1:1").await;
    let (status, headers, body) = send(&app, authed_get("/v1/models", Some("gzip, br"))).await;
    assert_eq!(status, StatusCode::OK);
    let encoding = content_encoding(&headers).expect("one encoding must be chosen");
    assert!(
        encoding == "gzip" || encoding == "br",
        "unexpected negotiated encoding: {encoding}"
    );
    assert!(!body.is_empty());
}

/// No `Accept-Encoding` at all → identity. `Vary` is still emitted (the
/// representation does depend on the request header), which is what keeps
/// caches from serving a gzip body to a non-gzip client.
#[tokio::test]
async fn missing_accept_encoding_serves_identity() {
    let app = build_app("http://127.0.0.1:1").await;
    let (status, headers, body) = send(&app, authed_get("/v1/models", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        content_encoding(&headers).is_none(),
        "without Accept-Encoding the response must stay identity"
    );
    assert!(vary_mentions_accept_encoding(&headers));
    assert!(body.starts_with(b"{"), "identity body must be plain JSON");
}

#[tokio::test]
async fn small_response_below_the_size_floor_is_not_compressed() {
    let app = build_app("http://127.0.0.1:1").await;
    // `/health` is public and answers a small JSON object.
    let (status, headers, body) = send(&app, authed_get("/health", Some("gzip, br"))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        content_encoding(&headers).is_none(),
        "responses under the {SIZE_FLOOR}-byte floor must not be compressed"
    );
    assert!(
        body.len() < SIZE_FLOOR,
        "/health fixture is expected to stay under the floor, got {} bytes",
        body.len()
    );
}

/// `Accept-Encoding: identity;q=0` leaves the server no satisfiable encoding.
/// tower-http overwrites the status with 406 and passes the body through
/// untouched (RFC 9110 §12.5.3) — this asserts that documented tower-http
/// behaviour, it is not a SAG-specific choice.
#[tokio::test]
async fn identity_q_zero_yields_406_from_tower_http() {
    let app = build_app("http://127.0.0.1:1").await;
    let (status, headers, body) = send(&app, authed_get("/v1/models", Some("identity;q=0"))).await;
    assert_eq!(
        status,
        StatusCode::NOT_ACCEPTABLE,
        "tower-http answers 406 when identity is excluded and nothing else is acceptable"
    );
    assert!(
        content_encoding(&headers).is_none(),
        "the 406 body is passed through unencoded"
    );
    assert!(vary_mentions_accept_encoding(&headers));
    assert!(!body.is_empty(), "the handler body is passed through");
}

// ── SSE passthrough ──────────────────────────────────────────────

#[tokio::test]
async fn sse_stream_is_never_compressed() {
    // Frames are padded past the 1 KiB floor on purpose: a stream has no
    // known size, and tower-http's `SizeAbove` returns `true` for unknown
    // sizes, so only `NotForContentType::SSE` keeps this test green.
    let frames: [&[u8]; 3] = [
        br#"data: {"id":"chatcmpl-c1","object":"chat.completion.chunk","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello from the compression fixture padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding padding"},"finish_reason":null}]}"#,
        br#"data: {"id":"chatcmpl-c1","object":"chat.completion.chunk","created":1700000000,"model":"gpt-4o","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        b"data: [DONE]",
    ];
    let mut expected: Vec<u8> = Vec::new();
    for f in frames.iter() {
        expected.extend_from_slice(format!("{}\n\n", std::str::from_utf8(f).unwrap()).as_bytes());
    }
    assert!(
        expected.len() > SIZE_FLOOR,
        "the SSE fixture must exceed the size floor to prove the content-type \
         predicate (not the size floor) blocks compression: {} bytes",
        expected.len()
    );

    let base_url = spawn_upstream("text/event-stream", expected.clone()).await;
    let app = build_app(&base_url).await;

    let (status, headers, body) = send(
        &app,
        chat_request(
            r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
            Some("gzip, br"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream"),
        "the stream response must keep its SSE content-type"
    );
    assert!(
        content_encoding(&headers).is_none(),
        "SSE streams must never be compressed (buffering them would break streaming)"
    );
    assert_eq!(
        body, expected,
        "the SSE body must pass through byte-identically"
    );
}

/// The same chat route compresses as JSON when `stream` is false — proves the
/// exemption above is about the response content-type, not about the chat
/// route being excluded wholesale.
#[tokio::test]
async fn non_stream_chat_response_is_compressed() {
    let upstream = format!(
        r#"{{"id":"chatcmpl-c2","object":"chat.completion","created":1700000000,"model":"gpt-4o","choices":[{{"index":0,"message":{{"role":"assistant","content":"{}"}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}}}"#,
        "padding ".repeat(200)
    )
    .into_bytes();
    assert!(
        upstream.len() > SIZE_FLOOR,
        "the upstream chat payload must exceed the size floor: {} bytes",
        upstream.len()
    );

    let base_url = spawn_upstream("application/json", upstream.clone()).await;
    let app = build_app(&base_url).await;

    let (status, headers, body) = send(
        &app,
        chat_request(
            r#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#,
            Some("gzip"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_encoding(&headers).as_deref(), Some("gzip"));
    assert_eq!(
        gunzip(&body),
        upstream,
        "the decompressed chat body must be the upstream JSON verbatim"
    );
}
