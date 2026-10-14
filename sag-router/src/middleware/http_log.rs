use axum::{body::Body, extract::Request, middleware::Next, response::Response};
use std::time::{Duration, Instant};
use tracing::{debug, trace};

/// Maximum bytes to buffer from a response body for logging.
const MAX_BODY_READ: usize = crate::routes::DEFAULT_MAX_REQUEST_BODY_BYTES;
/// Request bodies at or below this size are buffered for a log preview;
/// larger ones stream through untouched so `DefaultBodyLimit` / the JSON
/// extractor can reject with a proper 413 + analytics fact.
const REQUEST_PREVIEW_MAX: usize = 512 * 1024;
const BODY_PREVIEW_MAX: usize = 512;

/// Axum middleware that logs HTTP request/response bodies.
///
/// - `debug!` level: `method`, `uri`, `auth` (masked), body preview (first 512 bytes + total size)
/// - `trace_id` is provided by the root span from `trace_middleware`, not duplicated here
/// - `trace!` (target: "sag_router::http_log::full"): full body
/// - SSE streaming responses are logged without body consumption
pub async fn log_http(request: Request, next: Next) -> Response {
    // ── Request ──────────────────────────────────────────────────────
    let (parts, body) = request.into_parts();

    let method = &parts.method;
    let uri = &parts.uri;

    let auth = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            let prefix = s.trim();
            if prefix.len() > 12 {
                format!("{}...", &prefix[..12])
            } else if prefix.len() > 6 {
                format!("{}...", &prefix[..6])
            } else {
                "***".to_string()
            }
        });

    let content_length: Option<usize> = parts
        .headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());

    // Only buffer when we know the body is small enough for a preview.
    // Large / chunked (no content-length) bodies must stream through so
    // DefaultBodyLimit / JsonRejection can emit 413 + the client_error fact.
    let known_small = matches!(content_length, Some(cl) if cl <= REQUEST_PREVIEW_MAX);
    if !known_small {
        if let Some(ref auth) = auth {
            debug!(%method, %uri, auth, body_size = content_length, "request");
        } else {
            debug!(%method, %uri, body_size = content_length, "request");
        }
        let request = Request::from_parts(parts, body);
        let started = Instant::now();
        let response = next.run(request).await;
        // The body is deliberately not buffered on this path (it may be a
        // multi-MB payload or a live SSE stream), but the response itself
        // must still be observable: without this line a large request leaves
        // no status / latency trace at all.
        log_response_meta(&response, started.elapsed());
        return response;
    }

    let bytes = axum::body::to_bytes(body, REQUEST_PREVIEW_MAX)
        .await
        .unwrap_or_default();

    let body_size = bytes.len();

    if !bytes.is_empty() {
        let preview = truncate_body(&bytes);
        if let Some(ref auth) = auth {
            debug!(%method, %uri, auth, body_size, preview, "request");
        } else {
            debug!(%method, %uri, body_size, preview, "request");
        }
        // Full body at trace level with dedicated target
        let body_str = String::from_utf8_lossy(&bytes);
        trace!(
            target: "sag_router::http_log::full",
            %method, %uri, body_size, body = %body_str,
            "request full body"
        );
    } else {
        if let Some(ref auth) = auth {
            debug!(%method, %uri, auth, "request");
        } else {
            debug!(%method, %uri, "request");
        }
    }

    let request = Request::from_parts(parts, Body::from(bytes));
    let response = next.run(request).await;

    // ── Response ─────────────────────────────────────────────────────
    let (parts, body) = response.into_parts();

    let is_sse = parts
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);

    let status = parts.status.as_u16();

    if is_sse {
        debug!(status, "streaming response");
        return Response::from_parts(parts, body);
    }

    let bytes = axum::body::to_bytes(body, MAX_BODY_READ)
        .await
        .unwrap_or_default();

    if !bytes.is_empty() {
        let preview = truncate_body(&bytes);
        let body_size = bytes.len();
        debug!(status, body_size, preview, "response");
        let body_str = String::from_utf8_lossy(&bytes);
        trace!(
            target: "sag_router::http_log::full",
            status, body_size, body = %body_str,
            "response full body"
        );
    } else {
        debug!(status, "response");
    }

    Response::from_parts(parts, Body::from(bytes))
}

/// Log a response's status / latency (plus `content-length` when the peer
/// declared one) **without consuming the body**.
///
/// Used by the large-body path, which cannot buffer the body for a preview;
/// the buffered path keeps its own richer logging (body size + preview).
fn log_response_meta(response: &Response, elapsed: Duration) {
    let status = response.status().as_u16();
    let elapsed_ms = elapsed.as_millis() as u64;
    // `Option<u64>` records nothing when absent, so a chunked response
    // simply omits the field instead of logging a bogus size.
    let body_size: Option<u64> = response
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let is_sse = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);
    if is_sse {
        debug!(status, elapsed_ms, body_size, "streaming response");
    } else {
        debug!(status, elapsed_ms, body_size, "response");
    }
}

/// Truncate body bytes to a preview string.
fn truncate_body(bytes: &[u8]) -> String {
    if bytes.len() <= BODY_PREVIEW_MAX {
        String::from_utf8_lossy(bytes).to_string()
    } else {
        let preview = String::from_utf8_lossy(&bytes[..BODY_PREVIEW_MAX]);
        format!(
            "{preview}...(truncated, {total} bytes total)",
            total = bytes.len()
        )
    }
}
