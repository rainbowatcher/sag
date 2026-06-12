use axum::{body::Body, extract::Request, middleware::Next, response::Response};
use tracing::debug;

/// Maximum bytes to read from a single request/response body.
const MAX_BODY_READ: usize = 10_485_760; // 10 MB

/// Axum middleware that logs HTTP request/response bodies at `debug!` level.
///
/// - Logs: `method`, `uri`, `auth` (masked Authorization header), `body`, `status`, `response_body`
/// - SSE streaming responses (`Content-Type: text/event-stream`) are logged without body consumption
/// - Bodies are NOT truncated at debug level
pub async fn log_http(request: Request, next: Next) -> Response {
    // ── Request ──────────────────────────────────────────────────────
    let (parts, body) = request.into_parts();

    let method = &parts.method;
    let uri = &parts.uri;

    let auth = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(mask_auth);

    let bytes = axum::body::to_bytes(body, MAX_BODY_READ)
        .await
        .unwrap_or_default();

    if !bytes.is_empty() {
        let body_str = String::from_utf8_lossy(&bytes);
        if let Some(auth) = &auth {
            debug!(%method, %uri, auth = %auth, body = %body_str, "request");
        } else {
            debug!(%method, %uri, body = %body_str, "request");
        }
    } else {
        if let Some(auth) = &auth {
            debug!(%method, %uri, auth = %auth, "request");
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
        debug!(%status, "streaming response");
        return Response::from_parts(parts, body);
    }

    let bytes = axum::body::to_bytes(body, MAX_BODY_READ)
        .await
        .unwrap_or_default();

    if !bytes.is_empty() {
        let body_str = String::from_utf8_lossy(&bytes);
        debug!(%status, body = %body_str, "response");
    } else {
        debug!(%status, "response");
    }

    Response::from_parts(parts, Body::from(bytes))
}

/// Mask an Authorization header value for safe logging.
///
/// Shows the first 12 characters followed by `...` — enough to distinguish
/// keys without exposing the full secret.
fn mask_auth(value: &str) -> String {
    let prefix = value.trim();
    if prefix.len() > 12 {
        format!("{}...", &prefix[..12])
    } else if prefix.len() > 6 {
        format!("{}...", &prefix[..6])
    } else {
        "***".to_string()
    }
}
