use axum::{body::Body, extract::Request, middleware::Next, response::Response};
use tracing::{debug, trace};

/// Maximum bytes to read from a single request/response body.
const MAX_BODY_READ: usize = 10_485_760; // 10 MB
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

    let bytes = axum::body::to_bytes(body, MAX_BODY_READ)
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
