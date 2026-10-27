//! Router-level integration tests for the TypeSafe System One ingress
//! (`POST /v1/systemone`).
//!
//! Covers:
//! - semantic zero-loss (at the `Value` level) passthrough: `state` /
//!   `questions` / `assets` on the way in and `answers` / `id` / probability
//!   fields on the way out are never decoded into a gateway IR; only `model`
//!   is rewritten to the target's `real_model`
//! - Bearer auth (401) and the OpenAI error envelope
//! - shape 422s (`model` / `questions`) before dispatch
//! - extractor rejections rendered as the OpenAI envelope: malformed JSON
//!   (400 `BAD_REQUEST`), oversized body (413 `PAYLOAD_TOO_LARGE`), missing or
//!   non-JSON `Content-Type` (415 `UNSUPPORTED_MEDIA_TYPE`)
//! - resolution failure envelopes: unknown model (404), provider without
//!   `apis.systemone` (502 — no translated lane exists), image pipeline (422)

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use sag_config::{
    ApiConfig, ConfigHandle, GatewayAuthConfig, GatewayConfig, KeyStrategy, Pipeline,
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

/// Split a captured raw request into `(request_line, body)`.
fn split_request(bytes: &[u8]) -> (String, String) {
    let text = String::from_utf8_lossy(bytes).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let request_line = head.lines().next().unwrap_or_default().to_string();
    (request_line, body.to_string())
}

// ── app state wiring ─────────────────────────────────────────────

/// Which protocol node the mock provider declares.
#[derive(Clone, Copy)]
enum Upstream {
    /// `apis.systemone` — the only lane SystemOne can ever use.
    SystemOne,
    /// `apis.chat_completions` — never a SystemOne target (no translated lane).
    ChatOnly,
}

async fn build_state(base_url: &str, key: &str, upstream: Upstream) -> AppState {
    build_state_with_pipeline(base_url, key, upstream, Pipeline::Chat).await
}

async fn build_state_with_pipeline(
    base_url: &str,
    key: &str,
    upstream: Upstream,
    pipeline: Pipeline,
) -> AppState {
    let api_kind = match upstream {
        Upstream::SystemOne => sag_core::ApiKind::SystemOne,
        Upstream::ChatOnly => sag_core::ApiKind::ChatCompletions,
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
        api_kind,
        ApiConfig {
            base_url: base_url.to_string(),
            ..ApiConfig::default()
        },
    );
    providers.insert("so-up".to_string(), pc);

    let config = GatewayConfig {
        gateway_auth: Some(GatewayAuthConfig {
            api_keys: vec![key.to_string()],
        }),
        providers,
        routes: vec![RouteConfig {
            session_ttl_secs: None,
            session_max_entries: None,
            model_name: "so-model".into(),
            api_endpoint: (pipeline == Pipeline::Image).then(|| "/images/generations".to_string()),
            pipeline,
            retries: None,
            timeout_secs: None,
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                extra: HashMap::new(),
                retries: None,
                timeout_secs: None,
                provider: "so-up".into(),
                real_model: Some("so-real".into()),
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

fn systemone_request(body: &str, key: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/systemone")
        .header("Authorization", format!("Bearer {key}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

// ── semantic zero-loss passthrough ───────────────────────────────

/// The original wire body, with fields the gateway has no DTO for (unknown
/// question kind, unknown top-level field, assets) to prove nothing is
/// dropped or rejected.
const REQUEST_BODY: &str = r#"{
    "model": "so-model",
    "state": {"user": "u-1", "history": [{"role": "user", "text": "hi"}]},
    "questions": {
        "sentiment": {"type": "choice", "criteria": ["pos", "neg"]},
        "relevance": {"type": "noul", "instructions": "Is this relevant?"},
        "rating": {"type": "score", "criteria": {"min": 0, "max": 10}},
        "box": {"type": "future_kind", "criteria": {"anything": [1, 2]}}
    },
    "assets": {"img": {"type": "image", "format": "png", "data_base64": "AAA"}},
    "x_vendor_hint": {"nested": [1, 2, 3]}
}"#;

/// Upstream answer payload: `id` plus every answer flavour and its
/// probability fields.
const UPSTREAM_BODY: &str = r#"{
    "id": "so_resp_1",
    "model": "so-real",
    "answers": {
        "sentiment": {"type": "choice", "choice": "pos", "confidence": 0.93,
                      "probabilities": {"pos": 0.93, "neg": 0.07}},
        "relevance": {"type": "noul", "noul": 0.81},
        "rating": {"type": "score", "score": 7, "legend": {"0": "bad", "10": "good"},
                   "confidence": 0.6, "probabilities": {"7": 0.6}},
        "box": {"type": "box", "box": {"x0": 0.1, "y0": 0.2, "x1": 0.3, "y1": 0.4}}
    },
    "usage": {"input_tokens": 41, "output_tokens": 7}
}"#;

#[tokio::test]
async fn systemone_ingress_round_trips_with_semantic_zero_loss() {
    let expected = UPSTREAM_BODY.to_string();
    let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
        let expected = expected.clone();
        async move {
            respond_json(&mut stream, &expected).await;
        }
    })
    .await;

    let state = build_state(&base_url, "so-key", Upstream::SystemOne).await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app
        .oneshot(systemone_request(REQUEST_BODY, "so-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // ── upstream request: fixed protocol path + `model` rewritten only ──
    let req = req_rx.await.expect("request bytes");
    let (request_line, req_body) = split_request(&req);
    assert!(
        request_line.starts_with("POST /systemone "),
        "upstream path must be the fixed SystemOne protocol path: {request_line}"
    );
    let sent: serde_json::Value = serde_json::from_str(&req_body).expect("upstream body is JSON");
    let original: serde_json::Value = serde_json::from_str(REQUEST_BODY).unwrap();
    assert_eq!(
        sent["model"], "so-real",
        "model must be rewritten to the target's real_model"
    );
    for field in ["state", "questions", "assets", "x_vendor_hint"] {
        assert_eq!(
            sent[field], original[field],
            "`{field}` must reach the upstream with semantic zero loss"
        );
    }
    assert_eq!(
        sent.as_object().map(|o| o.len()),
        original.as_object().map(|o| o.len()),
        "no field may be added or dropped on the way in"
    );

    // ── response: semantic zero-loss passthrough ──
    let json = body_json(resp).await;
    let upstream: serde_json::Value = serde_json::from_str(UPSTREAM_BODY).unwrap();
    assert_eq!(
        json, upstream,
        "the upstream SystemOne body must be forwarded with semantic zero loss"
    );
    // Spot-checks for the fields a decode → encode round-trip used to drop.
    assert_eq!(json["id"], "so_resp_1");
    assert_eq!(json["answers"]["sentiment"]["probabilities"]["pos"], 0.93);
    assert_eq!(json["answers"]["relevance"]["noul"], 0.81);
    assert_eq!(json["answers"]["rating"]["legend"]["10"], "good");
    assert_eq!(json["answers"]["box"]["box"]["x1"], 0.3);
}

/// Handler-path zero-loss for the shapes that a typed DTO would erase: every
/// `state` form the spec allows (string / object / array / number / null),
/// explicit `null` on optional fields, plus an explicit `null` question.
/// None of these may be dropped, coerced or rejected on the way upstream.
const STRING_STATE_BODY: &str = r#"{
    "model": "so-model",
    "state": "plain-string-state",
    "questions": {"rate": {"type": "score", "criteria": {"min": 0, "max": 5}}},
    "instructions": null,
    "assets": null
}"#;
const OBJECT_STATE_BODY: &str = r#"{
    "model": "so-model",
    "state": {"user": "u-1", "nested": null},
    "questions": {},
    "instructions": null,
    "assets": null
}"#;
const ARRAY_STATE_BODY: &str = r#"{
    "model": "so-model",
    "state": [1, "two", null, {"four": 4}],
    "questions": {"q": {"type": null}},
    "instructions": null,
    "assets": null
}"#;
const NUMBER_STATE_BODY: &str = r#"{
    "model": "so-model",
    "state": 42.5,
    "questions": {"q": {"type": "choice"}},
    "instructions": null,
    "assets": null
}"#;
const NULL_STATE_BODY: &str = r#"{
    "model": "so-model",
    "state": null,
    "questions": {"q": {"type": "choice"}},
    "instructions": null,
    "assets": null
}"#;

#[tokio::test]
async fn systemone_ingress_keeps_state_shapes_and_explicit_nulls() {
    for body in [
        STRING_STATE_BODY,
        OBJECT_STATE_BODY,
        ARRAY_STATE_BODY,
        NUMBER_STATE_BODY,
        NULL_STATE_BODY,
    ] {
        let expected = UPSTREAM_BODY.to_string();
        let (base_url, req_rx) = spawn_server_capturing(move |mut stream| {
            let expected = expected.clone();
            async move {
                respond_json(&mut stream, &expected).await;
            }
        })
        .await;

        let state = build_state(&base_url, "so-key", Upstream::SystemOne).await;
        let app = build_router(state, DispatchRuntime::default());

        let resp = app
            .oneshot(systemone_request(body, "so-key"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "body={body}");

        let req = req_rx.await.expect("request bytes");
        let (_, req_body) = split_request(&req);
        let sent: serde_json::Value = serde_json::from_str(&req_body).expect("upstream JSON");
        let original: serde_json::Value = serde_json::from_str(body).unwrap();

        assert_eq!(
            sent.as_object().map(|o| o.len()),
            original.as_object().map(|o| o.len()),
            "no field may be added or dropped for body={body}"
        );
        // Field-by-field on the upstream request: explicit `null` must survive
        // as `Value::Null`, not disappear as `Option::None`.
        for (field, value) in original.as_object().expect("original is an object") {
            if field == "model" {
                assert_eq!(sent[field], "so-real", "model is the only rewrite");
                continue;
            }
            assert_eq!(
                &sent[field], value,
                "`{field}` must keep its exact Value for body={body}"
            );
        }
        assert!(
            sent.get("instructions").is_some_and(|v| v.is_null()),
            "explicit `instructions: null` must reach the upstream: {sent}"
        );
        assert!(
            sent.get("assets").is_some_and(|v| v.is_null()),
            "explicit `assets: null` must reach the upstream: {sent}"
        );
    }
}

// ── auth ──────────────────────────────────────────────────────────

#[tokio::test]
async fn systemone_ingress_requires_bearer_token() {
    // Dead upstream: auth must reject before any dispatch.
    let state = build_state("http://127.0.0.1:1", "so-key", Upstream::SystemOne).await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/systemone")
                .header("content-type", "application/json")
                .body(Body::from(REQUEST_BODY))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let json = body_json(resp).await;
    assert_eq!(json["error"]["code"], "UNAUTHORIZED");
    assert!(
        json.get("type").is_none(),
        "must be the OpenAI envelope: {json}"
    );
}

// ── shape 422s ────────────────────────────────────────────────────

#[tokio::test]
async fn systemone_ingress_shape_errors_are_422_before_dispatch() {
    // Dead upstream: a 422 (not a 502) also proves nothing was dispatched.
    let state = build_state("http://127.0.0.1:1", "so-key", Upstream::SystemOne).await;
    let app = build_router(state, DispatchRuntime::default());

    for (body, needle) in [
        (r#"{"questions":{"a":{"type":"noul"}}}"#, "`model`"),
        (
            r#"{"model":123,"questions":{"a":{"type":"noul"}}}"#,
            "`model`",
        ),
        (
            r#"{"model":null,"questions":{"a":{"type":"noul"}}}"#,
            "`model`",
        ),
        (r#"{"model":"so-model"}"#, "`questions`"),
        (r#"{"model":"so-model","questions":[]}"#, "`questions`"),
        (r#"{"model":"so-model","questions":"nope"}"#, "`questions`"),
        (r#"[]"#, "JSON object"),
    ] {
        let resp = app
            .clone()
            .oneshot(systemone_request(body, "so-key"))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "body={body}"
        );
        let json = body_json(resp).await;
        assert_eq!(json["error"]["code"], "UNPROCESSABLE_ENTITY", "body={body}");
        let message = json["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains(needle),
            "message must name the offending field ({needle}): {message}"
        );
        assert!(
            json.get("type").is_none(),
            "must be the OpenAI envelope: {json}"
        );
    }
}

#[tokio::test]
async fn systemone_ingress_malformed_json_is_400() {
    let state = build_state("http://127.0.0.1:1", "so-key", Upstream::SystemOne).await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app
        .oneshot(systemone_request("{ not valid json", "so-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // Extractor rejections must be re-rendered into the OpenAI envelope, not
    // forwarded as axum's plain-text body.
    let json = body_json(resp).await;
    assert_eq!(json["error"]["code"], "BAD_REQUEST", "{json}");
    assert!(
        json["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "the rejection text must be carried in the envelope: {json}"
    );
    assert!(
        json.get("type").is_none(),
        "must be the OpenAI envelope: {json}"
    );
}

#[tokio::test]
async fn systemone_ingress_oversized_body_is_413_envelope() {
    let state = build_state("http://127.0.0.1:1", "so-key", Upstream::SystemOne).await;
    let app = build_router(state, DispatchRuntime::default());

    let oversized = format!(
        r#"{{"model":"so-model","questions":{{}},"state":"{}"}}"#,
        "x".repeat(sag_router::DEFAULT_MAX_REQUEST_BODY_BYTES + 1)
    );
    let resp = app
        .oneshot(systemone_request(&oversized, "so-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let json = body_json(resp).await;
    assert_eq!(json["error"]["code"], "PAYLOAD_TOO_LARGE", "{json}");
    assert!(
        json["error"]["message"]
            .as_str()
            .is_some_and(|m| !m.is_empty()),
        "the rejection text must be carried in the envelope: {json}"
    );
    assert!(
        json.get("type").is_none(),
        "must be the OpenAI envelope: {json}"
    );
}

#[tokio::test]
async fn systemone_ingress_missing_or_wrong_content_type_is_415_envelope() {
    let state = build_state("http://127.0.0.1:1", "so-key", Upstream::SystemOne).await;
    let app = build_router(state, DispatchRuntime::default());

    // `None` = the header is absent, `Some(_)` = a non-JSON media type.
    for content_type in [None, Some("text/plain")] {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/v1/systemone")
            .header("Authorization", "Bearer so-key");
        if let Some(content_type) = content_type {
            builder = builder.header("content-type", content_type);
        }
        let resp = app
            .clone()
            .oneshot(builder.body(Body::from(REQUEST_BODY)).unwrap())
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content-type={content_type:?}"
        );
        let json = body_json(resp).await;
        assert_eq!(
            json["error"]["code"], "UNSUPPORTED_MEDIA_TYPE",
            "content-type={content_type:?}: {json}"
        );
        assert!(
            json["error"]["message"]
                .as_str()
                .is_some_and(|m| !m.is_empty()),
            "the rejection text must be carried in the envelope: {json}"
        );
        assert!(
            json.get("type").is_none(),
            "must be the OpenAI envelope: {json}"
        );
    }
}

// ── resolution failure envelopes ──────────────────────────────────

#[tokio::test]
async fn systemone_ingress_unknown_model_is_404() {
    let state = build_state("http://127.0.0.1:1", "so-key", Upstream::SystemOne).await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app
        .oneshot(systemone_request(
            r#"{"model":"missing","state":"s","questions":{"a":{"type":"noul"}}}"#,
            "so-key",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let json = body_json(resp).await;
    assert_eq!(json["error"]["code"], "MODEL_NOT_FOUND");
    assert!(
        json.get("type").is_none(),
        "must be the OpenAI envelope: {json}"
    );
}

#[tokio::test]
async fn systemone_ingress_provider_without_the_api_node_is_filtered() {
    // SystemOne has no translated lane: a provider declaring only
    // `apis.chat_completions` can never serve it, so the target is dropped and
    // the failure names the missing capability instead of the generic
    // "disabled provider" wording.
    let state = build_state("http://127.0.0.1:1", "so-key", Upstream::ChatOnly).await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app
        .oneshot(systemone_request(REQUEST_BODY, "so-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let json = body_json(resp).await;
    assert_eq!(json["error"]["code"], "ALL_PROVIDERS_EXHAUSTED");
    let message = json["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("lacks capability systemone") && message.contains("so-up/so-real"),
        "the failure must name the filtered target: {message}"
    );
}

#[tokio::test]
async fn systemone_ingress_on_image_route_is_422() {
    let state = build_state_with_pipeline(
        "http://127.0.0.1:1",
        "so-key",
        Upstream::SystemOne,
        Pipeline::Image,
    )
    .await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app
        .oneshot(systemone_request(REQUEST_BODY, "so-key"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let json = body_json(resp).await;
    assert_eq!(json["error"]["code"], "UNPROCESSABLE_ENTITY");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("not supported on /v1/systemone"),
        "{json}"
    );
}

// ── /v1/models advertisement ──────────────────────────────────────

#[tokio::test]
async fn models_list_advertises_systemone_ingress() {
    // The `ingress_protocols` union is derived from `ApiKind::protocols()`,
    // so a new protocol key shows up without touching the handler.
    let state = build_state("http://127.0.0.1:1", "so-key", Upstream::SystemOne).await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("Authorization", "Bearer so-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    let model = json["data"]
        .as_array()
        .and_then(|d| d.iter().find(|m| m["id"] == "so-model"))
        .expect("route must be listed");
    let protocols: Vec<&str> = model["ingress_protocols"]
        .as_array()
        .expect("ingress_protocols array")
        .iter()
        .filter_map(|p| p.as_str())
        .collect();
    assert_eq!(
        protocols,
        vec!["systemone"],
        "the apis.systemone node must surface in ingress_protocols"
    );
}
