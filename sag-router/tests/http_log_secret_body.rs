//! Regression: the bodies of credential-bearing admin requests must never
//! reach a log line.
//!
//! `PUT /api/config` accepts `UpdatePrivacyRequest.salt` — the write-only
//! privacy salt that `sag-admin/src/admin_config.rs` and `PrivacyForm.vue`
//! pin as "the request is not logged / must never reach a log line". The HTTP
//! log middleware used to buffer every request body ≤ 512 KiB and emit a
//! `debug!` preview plus a `trace!` full body (target
//! `sag_router::http_log::full`), which wrote the salt verbatim into
//! `logs/sag.log.*` as soon as `[logging].level = "debug"` (or `RUST_LOG`) was
//! set.
//!
//! The middleware now treats the admin endpoints listed in
//! `BODY_LOG_EXCLUDED_PATHS` as metadata-only: neither request nor response
//! body is buffered. This test drives a `/api/config`-shaped `PUT` through the
//! real middleware stack (same layering as `build_router`, so the root span
//! supplies `trace_id` / `uri`) and asserts that the secret appears in no
//! captured line while the metadata lines survive.

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode, header},
    middleware,
    routing::{post, put},
};
use tower::ServiceExt;
use tracing_subscriber::fmt::MakeWriter;

/// The admin endpoint under test (`PUT /api/config`).
const CONFIG_URI: &str = "/api/config";
/// A regular endpoint, used to prove the skip is path-scoped.
const ECHO_URI: &str = "/probe/echo";
/// The value that must not be observable anywhere in the log output.
const SECRET_SALT: &str = "secret-value";

/// Shaped like the body `sag-ui/src/api/privacyConfig.ts` actually sends: a
/// full-object `privacy` replace carrying the write-only salt.
const CONFIG_BODY: &str = r#"{"privacy":{"enabled":true,"mode":"mask","restore":false,"builtins":{},"custom_regexes":[],"custom_words":{},"allowlist":{},"salt":"secret-value"}}"#;

/// Minimal `MakeWriter` that appends every formatted event to a buffer.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
    }

    /// Every captured line carrying `uri` (the root span stamps it on each
    /// event emitted by the middleware).
    fn lines(&self, uri: &str) -> Vec<String> {
        self.text()
            .lines()
            .filter(|l| l.contains(uri))
            .map(str::to_string)
            .collect()
    }
}

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Install the capturing subscriber once per test binary.
///
/// `TRACE` is the level of the full-body events, so it is the only setting
/// that can prove the `sag_router::http_log::full` bypass too.
fn capture() -> &'static Capture {
    static CAPTURE: OnceLock<Capture> = OnceLock::new();
    CAPTURE.get_or_init(|| {
        let capture = Capture::default();
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_writer(capture.clone())
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .finish(),
        )
        .expect("capture subscriber installs once");
        capture
    })
}

/// The middleware stack `build_router` uses: `log_http` inside
/// `trace_middleware`.
fn with_log(app: Router) -> Router {
    app.layer(middleware::from_fn(
        sag_router::middleware::http_log::log_http,
    ))
    .layer(middleware::from_fn(
        sag_router::middleware::trace::trace_middleware,
    ))
}

/// The admin surface, mounted exactly like `build_router` does it
/// (`nest("/api", build_admin_routes(..))`), so the middleware observes the
/// full `/api/config` path rather than a prefix-stripped one.
fn admin_app(route: &'static str, handler: axum::routing::MethodRouter) -> Router {
    with_log(Router::new().nest("/api", Router::new().route(route, handler)))
}

/// A `PUT /api/config` request. `content-length` is set explicitly, exactly
/// like hyper sets it for a real client — that is what makes the middleware
/// take the buffered (preview) path for small bodies.
fn config_request() -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(CONFIG_URI)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, CONFIG_BODY.len())
        .body(Body::from(CONFIG_BODY.to_string()))
        .unwrap()
}

/// ① The privacy salt must not leak through the `debug!` preview or the
/// `trace!` full body, while the metadata lines stay.
#[tokio::test]
async fn put_config_body_never_reaches_a_log_line() {
    let capture = capture();

    let resp = admin_app("/config", put(|| async { r#"{"ok":true}"# }))
        .oneshot(config_request())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The body must not be buffered for logging at all: no preview, no full
    // body — on the request *or* the response side.
    let lines = capture.lines(CONFIG_URI);
    assert!(!lines.is_empty(), "the request must still be observed");
    for line in &lines {
        assert!(
            !line.contains(SECRET_SALT),
            "the privacy salt reached a log line: {line}"
        );
        assert!(
            !line.contains("preview="),
            "an excluded path must not log a body preview: {line}"
        );
        assert!(
            !line.contains("full body"),
            "an excluded path must not log the full body: {line}"
        );
    }
    // Belt and braces: nothing in the whole capture (any target, any span)
    // may carry the value.
    let all = capture.text();
    assert!(
        !all.contains(SECRET_SALT),
        "the privacy salt must never reach a log line; captured:\n{all}"
    );

    // Metadata logging survives: request size + status + latency.
    assert!(
        lines.iter().any(|l| l.contains("request")
            && l.contains("body_size=")
            && l.contains("body_skipped=true")),
        "the request metadata line must survive: {lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("response status=200")),
        "the response metadata line must survive: {lines:#?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("elapsed_ms=")),
        "the response metadata line must carry latency: {lines:#?}"
    );
}

/// ② The skip is path-scoped: an ordinary endpoint keeps its body preview
/// (guards against an over-broad exclusion swallowing every request body).
#[tokio::test]
async fn non_credential_paths_still_log_a_preview() {
    let capture = capture();

    let body = r#"{"hello":"echo-control"}"#;
    let resp = with_log(Router::new().route(ECHO_URI, post(|| async { "ok" })))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(ECHO_URI)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::CONTENT_LENGTH, body.len())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let lines = capture.lines(ECHO_URI);
    assert!(
        lines
            .iter()
            .any(|l| l.contains("preview=") && l.contains("echo-control")),
        "a non-credential path must keep its body preview: {lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("body_skipped=true")),
        "a non-credential path must not be reported as skipped: {lines:#?}"
    );
}
