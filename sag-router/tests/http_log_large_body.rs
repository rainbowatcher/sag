//! Regression: a request body above `REQUEST_PREVIEW_MAX` (512 KiB) must
//! still leave a **response** log line.
//!
//! `log_http` streams large bodies through untouched so `DefaultBodyLimit` /
//! the JSON extractor can still reject with 413 + a `client_error` fact. That
//! early-return path used to skip response logging entirely, so a >512 KiB
//! request produced a `request` line and nothing else — which made
//! large-request outcomes (status, latency) unattributable in `logs/sag.log.*`.
//!
//! The middleware stack mirrors `build_router` (`log_http` inside
//! `trace_middleware`, so the root span supplies `trace_id` / `uri`).

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use axum::{
    Router,
    body::Body,
    http::{Request, header},
    middleware,
    routing::post,
};
use tower::ServiceExt;
use tracing_subscriber::fmt::MakeWriter;

/// Above `REQUEST_PREVIEW_MAX` (512 KiB) — the large-body path.
const LARGE_BODY_BYTES: usize = 600 * 1024;
const JSON_URI: &str = "/probe/large-body";
const SSE_URI: &str = "/probe/large-body-sse";

/// Minimal `MakeWriter` that appends every formatted event to a buffer.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
    }

    /// The response log line of `uri`: the line carrying both the request
    /// identity (root-span `uri` field) and a response keyword.
    fn response_line(&self, uri: &str) -> String {
        let text = self.text();
        text.lines()
            .find(|l| l.contains(uri) && l.contains("response status="))
            .map(str::to_string)
            .unwrap_or_else(|| panic!("no response log line for {uri}; captured:\n{text}"))
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

/// Install the capturing subscriber once per test binary. The response lines
/// are emitted at `DEBUG`, so that level must be enabled.
fn capture() -> &'static Capture {
    static CAPTURE: OnceLock<Capture> = OnceLock::new();
    CAPTURE.get_or_init(|| {
        let capture = Capture::default();
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_writer(capture.clone())
                .with_max_level(tracing::Level::DEBUG)
                .with_ansi(false)
                .finish(),
        )
        .expect("capture subscriber installs once");
        capture
    })
}

fn app(uri: &'static str, handler: axum::routing::MethodRouter) -> Router {
    Router::new()
        .route(uri, handler)
        .layer(middleware::from_fn(
            sag_router::middleware::http_log::log_http,
        ))
        .layer(middleware::from_fn(
            sag_router::middleware::trace::trace_middleware,
        ))
}

/// A large body request. `content-length` is set explicitly: it is what makes
/// the middleware take the large-body path (hyper sets it for real clients).
fn large_request(uri: &'static str) -> Request<Body> {
    let body = "x".repeat(LARGE_BODY_BYTES);
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn large_body_request_still_logs_the_response() {
    let capture = capture();

    let resp = app(JSON_URI, post(|| async { "ok" }))
        .oneshot(large_request(JSON_URI))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let line = capture.response_line(JSON_URI);
    assert!(line.contains("status=200"), "unexpected line: {line}");
    assert!(
        line.contains("elapsed_ms="),
        "large-body response line must carry latency: {line}"
    );
    assert!(
        !line.contains("preview="),
        "large bodies must not be buffered for a preview: {line}"
    );
}

#[tokio::test]
async fn large_body_streaming_response_is_logged_as_streaming() {
    let capture = capture();

    let resp = app(
        SSE_URI,
        post(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                "data: ok\n\n",
            )
        }),
    )
    .oneshot(large_request(SSE_URI))
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);

    let line = capture.response_line(SSE_URI);
    assert!(
        line.contains("streaming response status=200"),
        "SSE responses must stay distinguishable: {line}"
    );
    assert!(
        line.contains("elapsed_ms="),
        "large-body response line must carry latency: {line}"
    );
}
