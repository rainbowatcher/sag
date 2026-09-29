//! OAuth credential-flow integration tests (plan Phase 5).
//!
//! Full gateway path: mock OAuth token endpoint + mock LLM upstream,
//! provider configured with `auth_mode = "oauth"`, request through
//! `POST /v1/chat/completions`. Asserts the upstream sees
//! `Authorization: Bearer <access_token>` (not a static key), and the
//! 401 invalidate+retry / static no-invalidate behaviors survive the
//! dispatch layer (retries, failover, error envelope).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use sag_config::{
    ApiConfig, AuthMode, ConfigHandle, GatewayAuthConfig, GatewayConfig, KeyStrategy, OAuthConfig,
    ProviderConfig, ProxyMode, RouteConfig, RouteTarget, Strategy,
};
use sag_router::{AppState, DispatchRuntime, build_router, test_app_state};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tower::ServiceExt;

const GW_KEY: &str = "oauth-gw-key";

// ── mock servers ──────────────────────────────────────────────────

/// Read one HTTP request (headers + Content-Length body) from `stream`.
async fn read_http_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut buf = vec![0u8; 8192];
    let mut req = Vec::new();
    loop {
        match stream.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                req.extend_from_slice(&buf[..n]);
                if let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&req[..pos]);
                    let body_len = head
                        .lines()
                        .find_map(|l| {
                            let l = l.to_ascii_lowercase();
                            l.strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    let have = req.len() - (pos + 4);
                    if have >= body_len {
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    req
}

fn extract_authorization(req: &[u8]) -> String {
    let text = String::from_utf8_lossy(req);
    let head = text.split_once("\r\n\r\n").map(|(h, _)| h).unwrap_or(&text);
    head.lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.trim().eq_ignore_ascii_case("authorization") {
                Some(v.trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_default()
}

/// Multi-request mock IdP. Returns sequential `tok-{n}` tokens (n = 1-based
/// hit count) so the retry path can prove a fresh credential was used.
struct MockIdp {
    base_url: String,
    hit_count: Arc<AtomicUsize>,
}

async fn spawn_mock_idp() -> MockIdp {
    let hit_count = Arc::new(AtomicUsize::new(0));
    let hits = hit_count.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let hits = hits.clone();
            tokio::spawn(async move {
                let _req = read_http_request(&mut stream).await;
                let n = hits.fetch_add(1, Ordering::SeqCst) + 1;
                let body = format!(
                    r#"{{"access_token":"tok-{n}","token_type":"Bearer","expires_in":3600}}"#
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.flush().await;
                drop(stream);
            });
        }
    });
    MockIdp {
        base_url: format!("http://{addr}"),
        hit_count,
    }
}

/// LLM upstream policy: when to answer 401.
#[derive(Clone, Copy)]
enum LlmPolicy {
    /// Always 200.
    Always200,
    /// 401 only on the first request carrying `Bearer tok-*` (oauth cold
    /// start); later oauth requests succeed. Static keys always succeed.
    RejectFirstOauth,
    /// 401 on every request.
    Always401,
}

struct MockLlm {
    base_url: String,
    hit_count: Arc<AtomicUsize>,
    auths: Arc<tokio::sync::Mutex<Vec<String>>>,
}

async fn spawn_mock_llm(policy: LlmPolicy) -> MockLlm {
    let hit_count = Arc::new(AtomicUsize::new(0));
    let auths = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let hits = hit_count.clone();
    let auths2 = auths.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // Shared across connection tasks: first oauth-style request gets
        // 401 under RejectFirstOauth (bool is Copy, so a per-task local
        // would reset to true on every accept).
        let first_oauth_seen = Arc::new(AtomicUsize::new(0));
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let hits = hits.clone();
            let auths = auths2.clone();
            let first_oauth_seen = first_oauth_seen.clone();
            tokio::spawn(async move {
                let req = read_http_request(&mut stream).await;
                let auth = extract_authorization(&req);
                auths.lock().await.push(auth.clone());

                let oauth_style = auth.to_ascii_lowercase().starts_with("bearer tok-");
                let status = match policy {
                    LlmPolicy::Always200 => 200u16,
                    LlmPolicy::Always401 => 401,
                    LlmPolicy::RejectFirstOauth => {
                        let is_first_oauth =
                            oauth_style && first_oauth_seen.fetch_add(1, Ordering::SeqCst) == 0;
                        if is_first_oauth { 401 } else { 200 }
                    }
                };
                let _ = hits.fetch_add(1, Ordering::SeqCst);

                let body = if status == 200 {
                    // Valid ChatCompletionResponse with content so the
                    // dispatch empty-response gate does not fire.
                    r#"{"id":"chatcmpl-oauth","object":"chat.completion","created":1700000000,"model":"real-oauth","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}"#.to_string()
                } else {
                    r#"{"error":{"message":"unauthorized"}}"#.to_string()
                };
                let resp = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.flush().await;
                drop(stream);
            });
        }
    });
    MockLlm {
        base_url: format!("http://{addr}"),
        hit_count,
        auths,
    }
}

// ── app state wiring ─────────────────────────────────────────────

async fn build_oauth_state(llm_base: &str, token_url: &str) -> AppState {
    let mut providers = HashMap::new();
    let mut pc = ProviderConfig {
        api_keys: vec![],
        key_strategy: KeyStrategy::FillFirst,
        auth_mode: AuthMode::OAuth,
        oauth: Some(OAuthConfig {
            token_url: token_url.to_string(),
            client_id: "cid".into(),
            client_secret: "sec".into(),
            scope: None,
            audience: None,
            refresh_token: None,
        }),
        proxy_mode: ProxyMode::default(),
        timeout_secs: 10,
        ..ProviderConfig::default()
    };
    pc.apis.insert(
        sag_core::ApiKind::ChatCompletions,
        ApiConfig {
            base_url: llm_base.to_string(),
            ..ApiConfig::default()
        },
    );
    providers.insert("azure-oauth".to_string(), pc);
    build_state_with(providers).await
}

async fn build_static_state(llm_base: &str) -> AppState {
    let mut providers = HashMap::new();
    let mut pc = ProviderConfig {
        api_keys: vec!["sk-static".into()],
        key_strategy: KeyStrategy::FillFirst,
        auth_mode: AuthMode::Static,
        oauth: None,
        proxy_mode: ProxyMode::default(),
        timeout_secs: 10,
        ..ProviderConfig::default()
    };
    pc.apis.insert(
        sag_core::ApiKind::ChatCompletions,
        ApiConfig {
            base_url: llm_base.to_string(),
            ..ApiConfig::default()
        },
    );
    providers.insert("openai".to_string(), pc);
    build_state_with(providers).await
}

async fn build_state_with(providers: HashMap<String, ProviderConfig>) -> AppState {
    let mut config = GatewayConfig {
        gateway_auth: Some(GatewayAuthConfig {
            api_keys: vec![GW_KEY.into()],
        }),
        providers,
        routes: vec![RouteConfig {
            model_name: "oauth-model".into(),
            api_endpoint: None,
            pipeline: sag_config::Pipeline::Chat,
            // One-shot mocks: no dispatch-level retries (avoids extra
            // LLM accepts). Provider-internal 401 invalidate+retry is
            // independent of this.
            retries: Some(0),
            timeout_secs: None,
            strategy: Strategy::FillFirst,
            targets: vec![RouteTarget {
                extra: HashMap::new(),
                retries: Some(0),
                timeout_secs: None,
                provider: "azure-oauth".into(),
                real_model: Some("real-oauth".into()),
                priority: 1,
                capabilities: None,
            }],
        }],
        ..sag_config::gateway_config_base!()
    };
    // Static-state builder uses provider name "openai" as the route target.
    if config.providers.contains_key("openai") && !config.providers.contains_key("azure-oauth") {
        config.routes[0].targets[0].provider = "openai".into();
    }
    // Empty-response retry would re-hit the mock after a 200 with empty
    // choices; disable for deterministic hit counts.
    config.backoff.retry_on_empty_response = false;

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

fn chat_request() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("Authorization", format!("Bearer {GW_KEY}"))
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"model":"oauth-model","messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

// ── tests ─────────────────────────────────────────────────────────

/// Full chain: oauth provider → mock IdP → mock LLM. Upstream must see
/// `Authorization: Bearer <access_token>` from the token endpoint, never
/// a static key. Client receives a 200 completion.
#[tokio::test]
async fn oauth_full_chain_upstream_receives_bearer_access_token() {
    let idp = spawn_mock_idp().await;
    let llm = spawn_mock_llm(LlmPolicy::Always200).await;
    let token_url = format!("{}/token", idp.base_url);

    let state = build_oauth_state(&llm.base_url, &token_url).await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app.oneshot(chat_request()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "oauth success must be 200");
    let json = body_json(resp).await;
    assert_eq!(json["object"], "chat.completion");
    assert_eq!(json["choices"][0]["message"]["content"], "ok");

    let recorded = llm.auths.lock().await.clone();
    assert_eq!(
        recorded,
        vec!["Bearer tok-1".to_string()],
        "upstream must receive the oauth access token; got {recorded:?}"
    );
    assert!(
        !recorded[0].contains("sk-static") && !recorded[0].contains("sec"),
        "upstream must not receive static key or client_secret"
    );
    assert_eq!(idp.hit_count.load(Ordering::SeqCst), 1);
    assert_eq!(llm.hit_count.load(Ordering::SeqCst), 1);
}

/// Upstream 401 on the cold-start token → invalidate + re-fetch → second
/// attempt carries a new token → client gets 200. Token endpoint hit twice.
#[tokio::test]
async fn oauth_401_invalidate_retries_with_new_token() {
    let idp = spawn_mock_idp().await;
    let llm = spawn_mock_llm(LlmPolicy::RejectFirstOauth).await;
    let token_url = format!("{}/token", idp.base_url);

    let state = build_oauth_state(&llm.base_url, &token_url).await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app.oneshot(chat_request()).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "client must receive success after invalidate+retry"
    );
    let json = body_json(resp).await;
    assert_eq!(json["choices"][0]["message"]["content"], "ok");

    let recorded = llm.auths.lock().await.clone();
    assert_eq!(
        recorded,
        vec!["Bearer tok-1".to_string(), "Bearer tok-2".to_string()],
        "second upstream attempt must carry the refreshed token; got {recorded:?}"
    );
    assert_eq!(
        idp.hit_count.load(Ordering::SeqCst),
        2,
        "cold start + 401 invalidate → token endpoint called twice"
    );
    assert_eq!(
        llm.hit_count.load(Ordering::SeqCst),
        2,
        "provider-level 401 retry issues exactly two LLM attempts"
    );
}

/// Upstream always 401s oauth tokens: provider retries once then surfaces
/// `Upstream{401}`; dispatch treats it as non-retryable / non-client-caused
/// and fails over → exhausted envelope. Token endpoint called exactly twice
/// (no infinite invalidate loop).
#[tokio::test]
async fn oauth_second_401_surfaces_upstream_and_stops() {
    let idp = spawn_mock_idp().await;
    let llm = spawn_mock_llm(LlmPolicy::Always401).await;
    let token_url = format!("{}/token", idp.base_url);

    let state = build_oauth_state(&llm.base_url, &token_url).await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app.oneshot(chat_request()).await.unwrap();
    assert_ne!(resp.status(), StatusCode::OK, "second 401 must not succeed");
    // Provider-persistent Upstream{401} is non-client-caused → failover →
    // single target exhausted → AllProvidersExhausted (502).
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let json = body_json(resp).await;
    assert_eq!(json["error"]["code"], "ALL_PROVIDERS_EXHAUSTED");
    let msg = json["error"]["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("401") || msg.contains("unauthorized"),
        "exhausted reason must mention the upstream 401: {msg}"
    );

    assert_eq!(
        idp.hit_count.load(Ordering::SeqCst),
        2,
        "cold + one invalidate retry = two token fetches, not a loop"
    );
    assert_eq!(
        llm.hit_count.load(Ordering::SeqCst),
        2,
        "exactly two LLM attempts (initial + single invalidate retry)"
    );
}

/// Static mode with an upstream 401: no invalidate branch, no token-endpoint
/// traffic, single LLM attempt (zero behavior change vs pre-OAuth).
#[tokio::test]
async fn static_401_does_not_invalidate_or_hit_token_endpoint() {
    // IdP is spawned but never pointed at by the static provider — any hit
    // would prove an accidental oauth path.
    let idp = spawn_mock_idp().await;
    let llm = spawn_mock_llm(LlmPolicy::Always401).await;

    let state = build_static_state(&llm.base_url).await;
    let app = build_router(state, DispatchRuntime::default());

    let resp = app.oneshot(chat_request()).await.unwrap();
    assert_ne!(resp.status(), StatusCode::OK);
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let json = body_json(resp).await;
    assert_eq!(json["error"]["code"], "ALL_PROVIDERS_EXHAUSTED");

    let recorded = llm.auths.lock().await.clone();
    assert_eq!(
        recorded,
        vec!["Bearer sk-static".to_string()],
        "static mode must inject the static key once; got {recorded:?}"
    );
    assert_eq!(
        llm.hit_count.load(Ordering::SeqCst),
        1,
        "static 401 must not retry at the provider layer"
    );
    assert_eq!(
        idp.hit_count.load(Ordering::SeqCst),
        0,
        "static mode must never hit a token endpoint"
    );
}
