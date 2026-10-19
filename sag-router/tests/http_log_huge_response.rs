//! Regression: `log_http` must never shrink a response body it cannot buffer.
//!
//! `sag-router/src/middleware/http_log.rs` used to collect the response body
//! with `to_bytes(..).unwrap_or_default()`. Above the 10 MiB cap that turned
//! the payload into an empty body while `content-length` still advertised the
//! full size, so the client received `0` bytes behind a `200`; a body error
//! mid-stream was swallowed the same way. The fix reads the body up to the cap
//! and then forwards a frame chain (buffered frames + untouched remainder), so
//! the client always gets the real bytes — and the log records
//! `truncated=true, collected=<n>` (or `body_error=true`) instead of lying.
//!
//! These tests drive the middleware through the full HTTP path over a real
//! TCP socket (not `oneshot`): only the wire can prove that a chunked body
//! stays chunked, that a declared `content-length` survives, and that a
//! half-written body ends in a *broken* transfer instead of a clean one.
//!
//! The stack mirrors `build_router` (`log_http` inside `trace_middleware`, so
//! the root span supplies `trace_id` / `uri` for the log assertions).

use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::{
    Router,
    body::{Body, Bytes},
    http::{StatusCode, header},
    middleware,
    response::Response,
    routing::{MethodRouter, post},
};
use futures_util::stream;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tracing_subscriber::fmt::MakeWriter;

/// `MAX_BODY_READ` of `http_log` (== `DEFAULT_MAX_REQUEST_BODY_BYTES`).
const CAP: usize = 10 * 1024 * 1024;
/// Comfortably above the cap, so the truncated path is taken.
const HUGE: usize = 11 * 1024 * 1024;
/// Declared-but-never-delivered length of the half-written body.
const DECLARED_20M: usize = 20 * 1024 * 1024;

const URI_HUGE: &str = "/probe/huge-content-length";
const URI_CHUNKED: &str = "/probe/huge-chunked";
const URI_BROKEN: &str = "/probe/half-written";
const URI_EXACT_CAP: &str = "/probe/exact-cap";

// ── payload helpers ──────────────────────────────────────────────
// A 10–20 MiB fixture is milliseconds of work, so the tests use real bytes
// instead of injecting a smaller cap (which would stop exercising the
// constant the shipped binary actually uses).

/// Deterministic non-uniform bytes: `payload(n)` is a prefix of
/// `payload(m > n)`, which lets the half-written body be verified without
/// allocating the full 20 MiB twice.
fn payload(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push((x & 0xff) as u8);
    }
    out
}

/// FNV-1a over the whole payload: comparing digests avoids keeping a second
/// 11 MiB copy around just to `assert_eq!` the bodies.
fn digest(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

// ── log capture ──────────────────────────────────────────────────

/// Minimal `MakeWriter` that appends every formatted event to a buffer.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
    }

    /// The first captured line for `uri` that contains `needle`.
    fn line(&self, uri: &str, needle: &str) -> String {
        let text = self.text();
        text.lines()
            .find(|l| l.contains(uri) && l.contains(needle))
            .map(str::to_string)
            .unwrap_or_else(|| {
                panic!("no log line for {uri} containing {needle:?}; captured:\n{text}")
            })
    }

    /// The `response` event of `uri` (message field first, then `status=`).
    fn response_line(&self, uri: &str) -> String {
        self.line(uri, "response status=")
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

// ── server fixture ───────────────────────────────────────────────

fn app(uri: &'static str, handler: MethodRouter) -> Router {
    Router::new()
        .route(uri, handler)
        .layer(middleware::from_fn(
            sag_router::middleware::http_log::log_http,
        ))
        .layer(middleware::from_fn(
            sag_router::middleware::trace::trace_middleware,
        ))
}

/// Serve `app` on a real loopback socket so the wire framing (chunked vs
/// `content-length`, premature close) is observable by the client.
async fn spawn_server(uri: &'static str, handler: MethodRouter) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app(uri, handler)).await.unwrap();
    });
    addr
}

// ── raw HTTP/1.1 client ──────────────────────────────────────────

struct RawResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// `true` when the transfer ended exactly where the peer announced it
    /// would: the terminal `0` chunk arrived, or `content-length` bytes were
    /// read. `false` means the peer closed mid-body (a broken transfer).
    framing_complete: bool,
}

impl RawResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// A tiny request body. It is what routes the middleware through the
/// *buffered response* path: a request without a small declared
/// `content-length` (a bare `GET`, say) takes `log_http`'s early-return
/// branch and its response is never collected — so the probes must POST.
const PROBE_BODY: &str = r#"{"probe":true}"#;

/// Send one `POST` with `Connection: close` and read the response to EOF.
///
/// Deliberately hand-rolled: the point of these tests is the bytes on the
/// wire, and an HTTP client that "fixes up" a truncated body would hide the
/// very regression under test.
async fn raw_post(addr: SocketAddr, path: &str) -> RawResponse {
    tokio::time::timeout(Duration::from_secs(30), async move {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{PROBE_BODY}",
            PROBE_BODY.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.flush().await.unwrap();

        let mut raw = Vec::new();
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
                // A reset is still a legitimate observation: the peer died
                // mid-response. Keep the bytes already received.
                Err(_) => break,
            }
        }
        parse_response(&raw)
    })
    .await
    .expect("the probe server must answer within 30s")
}

fn parse_response(raw: &[u8]) -> RawResponse {
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response must contain a header block");
    let head = String::from_utf8_lossy(&raw[..head_end]);
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("status line");
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            line.split_once(':')
                .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_string()))
        })
        .collect();
    let body_bytes = &raw[head_end + 4..];

    let chunked = headers
        .iter()
        .any(|(key, value)| key == "transfer-encoding" && value.contains("chunked"));
    let content_length: Option<usize> = headers
        .iter()
        .find(|(key, _)| key == "content-length")
        .and_then(|(_, value)| value.parse().ok());

    let (body, framing_complete) = if chunked {
        decode_chunked(body_bytes)
    } else if let Some(declared) = content_length {
        let taken = body_bytes.len().min(declared);
        (body_bytes[..taken].to_vec(), body_bytes.len() >= declared)
    } else {
        (body_bytes.to_vec(), true)
    };

    RawResponse {
        status,
        headers,
        body,
        framing_complete,
    }
}

/// Decode a chunked body, reporting whether the terminal `0` chunk was seen.
fn decode_chunked(mut data: &[u8]) -> (Vec<u8>, bool) {
    let mut body = Vec::new();
    loop {
        let Some(line_end) = data.windows(2).position(|w| w == b"\r\n") else {
            return (body, false);
        };
        let size_hex = String::from_utf8_lossy(&data[..line_end]);
        let size_hex = size_hex.split(';').next().unwrap_or("").trim().to_string();
        let Ok(size) = usize::from_str_radix(&size_hex, 16) else {
            return (body, false);
        };
        data = &data[line_end + 2..];
        if size == 0 {
            // Trailer section (if any) is irrelevant here.
            return (body, true);
        }
        if data.len() < size + 2 {
            body.extend_from_slice(&data[..data.len().min(size)]);
            return (body, false);
        }
        body.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

// ── probe handlers ───────────────────────────────────────────────

/// ~11 MiB with a truthful `content-length`.
async fn huge_with_content_length() -> impl axum::response::IntoResponse {
    let body = payload(HUGE);
    ([(header::CONTENT_LENGTH, body.len().to_string())], body)
}

/// ~11 MiB, chunked: no `content-length` at all.
async fn huge_chunked() -> Response {
    let body = payload(HUGE);
    let frames: Vec<Result<Bytes, std::io::Error>> = body
        .chunks(64 * 1024)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(Body::from_stream(stream::iter(frames)))
        .unwrap()
}

/// Declares 20 MiB, delivers half of it, then dies mid-body.
async fn half_written_body() -> Response {
    let delivered = payload(DECLARED_20M / 2);
    let frames: Vec<Result<Bytes, std::io::Error>> = vec![
        Ok(Bytes::from(delivered)),
        Err(std::io::Error::other("upstream died mid-body")),
    ];
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_LENGTH, DECLARED_20M.to_string())
        .body(Body::from_stream(stream::iter(frames)))
        .unwrap()
}

/// Exactly at the cap: the buffered (`Complete`) path, no truncation marker.
async fn exactly_cap() -> impl axum::response::IntoResponse {
    let body = payload(CAP);
    ([(header::CONTENT_LENGTH, body.len().to_string())], body)
}

// ── tests ────────────────────────────────────────────────────────

/// ① Over-cap body with a declared `content-length`: the client gets all of
/// it, the header is untouched, and the log says `truncated` + `collected`.
#[tokio::test]
async fn over_cap_content_length_body_reaches_the_client_in_full() {
    let capture = capture();
    let addr = spawn_server(URI_HUGE, post(huge_with_content_length)).await;
    let expected = payload(HUGE);

    let response = raw_post(addr, URI_HUGE).await;

    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("content-length").map(str::to_string),
        Some(HUGE.to_string()),
        "the middleware must not rewrite content-length"
    );
    assert!(
        response.framing_complete,
        "an 11 MiB response must not be cut short"
    );
    assert_eq!(
        response.body.len(),
        expected.len(),
        "the client must receive every byte the upstream produced"
    );
    assert_eq!(
        digest(&response.body),
        digest(&expected),
        "the forwarded bytes must be identical to the upstream payload"
    );
    assert_eq!(&response.body[..64], &expected[..64], "head sample");
    assert_eq!(
        &response.body[response.body.len() - 64..],
        &expected[expected.len() - 64..],
        "tail sample"
    );

    // The over-cap log line is emitted once the body has been handed over.
    let line = capture.response_line(URI_HUGE);
    assert!(line.contains("truncated=true"), "unexpected line: {line}");
    assert!(
        line.contains(&format!("collected={CAP}")),
        "the log must report the buffered byte count: {line}"
    );
}

/// ② Chunked body, no `content-length`, > cap: the worst old branch (no log
/// evidence at all). The body must still arrive complete.
#[tokio::test]
async fn over_cap_chunked_body_without_content_length_reaches_the_client_in_full() {
    let capture = capture();
    let addr = spawn_server(URI_CHUNKED, post(huge_chunked)).await;
    let expected = payload(HUGE);

    let response = raw_post(addr, URI_CHUNKED).await;

    assert_eq!(response.status, 200);
    assert!(
        response.header("content-length").is_none(),
        "the upstream sent no content-length; none may be invented"
    );
    assert!(
        response.framing_complete,
        "the chunked transfer must terminate with a 0-chunk, not a close"
    );
    assert_eq!(response.body.len(), expected.len());
    assert_eq!(digest(&response.body), digest(&expected));

    let line = capture.response_line(URI_CHUNKED);
    assert!(line.contains("truncated=true"), "unexpected line: {line}");
    assert!(
        line.contains(&format!("collected={CAP}")),
        "the log must report the buffered byte count: {line}"
    );
}

/// ③ Body dies mid-stream: the client must not see "0 bytes behind a 200".
#[tokio::test]
async fn half_written_body_is_an_observable_failure_not_an_empty_200() {
    let capture = capture();
    let addr = spawn_server(URI_BROKEN, post(half_written_body)).await;
    let prefix = payload(CAP);

    let response = raw_post(addr, URI_BROKEN).await;

    // Headers were already on the wire, so the status is still 200 — the
    // point is that the *body* cannot masquerade as a complete response.
    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("content-length").map(str::to_string),
        Some(DECLARED_20M.to_string())
    );
    assert!(
        !response.body.is_empty(),
        "the already-delivered bytes must be forwarded, not replaced by an empty body"
    );
    assert!(
        response.body.len() < DECLARED_20M,
        "the transfer is incomplete by construction"
    );
    assert!(
        !response.framing_complete,
        "a truncated body must not look like a clean end-of-response"
    );
    // The abort resets the connection, so bytes still in flight are lost;
    // what matters is that the delivered prefix is substantial (the old bug
    // produced exactly 0) and byte-identical to the upstream payload.
    assert!(
        response.body.len() >= CAP / 2,
        "the ~10 MiB delivered before the error must reach the client, got {} bytes",
        response.body.len()
    );
    assert_eq!(
        &response.body[..],
        &prefix[..response.body.len()],
        "the forwarded prefix must be the upstream payload"
    );

    let error_line = capture.line(URI_BROKEN, "body_error=true");
    assert!(
        error_line.contains("response body error"),
        "the body error must be logged: {error_line}"
    );
}

/// ④ Exactly at the cap stays on the buffered path: full body, no marker.
#[tokio::test]
async fn body_of_exactly_the_cap_is_complete_and_not_truncated() {
    let capture = capture();
    let addr = spawn_server(URI_EXACT_CAP, post(exactly_cap)).await;
    let expected = payload(CAP);

    let response = raw_post(addr, URI_EXACT_CAP).await;

    assert_eq!(response.status, 200);
    assert_eq!(
        response.header("content-length").map(str::to_string),
        Some(CAP.to_string())
    );
    assert!(response.framing_complete);
    assert_eq!(response.body.len(), CAP);
    assert_eq!(digest(&response.body), digest(&expected));

    let line = capture.response_line(URI_EXACT_CAP);
    // `preview="..."` itself ends with `...(truncated, N bytes total)` for any
    // body over the 512-byte preview window, so only the *field* is asserted.
    assert!(
        !line.contains("truncated=true"),
        "a body of exactly `cap` bytes must stay on the complete path: {line}"
    );
    assert!(
        line.contains(&format!("body_size={CAP}")),
        "the complete path logs the real body size: {line}"
    );
}
