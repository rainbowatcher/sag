use axum::{
    body::{Body, Bytes},
    extract::Request,
    middleware::Next,
    response::Response,
};
use futures_util::StreamExt as _;
use http_body::Frame;
use http_body_util::{BodyExt as _, BodyStream, StreamBody};
use std::time::{Duration, Instant};
use tracing::{debug, trace, warn};

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

    // `known_small` guarantees the declared length fits the cap, so this is
    // normally `Complete`. A body that lies about its length (or dies
    // mid-stream) is forwarded unchanged and logged with the `truncated` /
    // `body_error` markers instead of being silently replaced by an empty one.
    let (body, collected, body_error) = collect_or_stream(body, REQUEST_PREVIEW_MAX).await;
    if body_error {
        warn!(%method, %uri, body_error = true, "request body error");
    }

    match &collected {
        Collected::Complete(bytes) => {
            let body_size = bytes.len();
            if !bytes.is_empty() {
                let preview = truncate_body(bytes);
                if let Some(ref auth) = auth {
                    debug!(%method, %uri, auth, body_size, preview, "request");
                } else {
                    debug!(%method, %uri, body_size, preview, "request");
                }
                // Full body at trace level with dedicated target
                let body_str = String::from_utf8_lossy(bytes);
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
        }
        Collected::Truncated { collected, sample } => {
            let body_size = *collected;
            let preview = preview_from_sample(sample, body_size);
            if let Some(ref auth) = auth {
                debug!(%method, %uri, auth, body_size, preview, truncated = true, "request");
            } else {
                debug!(%method, %uri, body_size, preview, truncated = true, "request");
            }
        }
    }

    let request = Request::from_parts(parts, body);
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

    // The body is collected up to `MAX_BODY_READ`. Anything bigger (or a body
    // that errors mid-stream) is forwarded as a frame chain — headers, body
    // bytes and trailers all reach the client untouched; only the log preview
    // is truncated. Previously the over-cap body was dropped by
    // `to_bytes(..).unwrap_or_default()` and the client got 0 bytes while
    // `content-length` still advertised the full size.
    let (body, collected, body_error) = collect_or_stream(body, MAX_BODY_READ).await;
    let declared_body_size: Option<u64> = parts
        .headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());

    match collected {
        Collected::Complete(bytes) => {
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
        Collected::Truncated { collected, sample } => {
            let preview = preview_from_sample(&sample, collected);
            warn!(
                status,
                truncated = true,
                collected,
                body_size = declared_body_size,
                preview,
                "response"
            );
            trace!(
                target: "sag_router::http_log::full",
                status, truncated = true, collected, body = %preview,
                "response truncated body"
            );
            if body_error {
                warn!(status, body_error = true, "response body error");
            }
            Response::from_parts(parts, body)
        }
    }
}

/// What [`collect_or_stream`] managed to read from a body.
///
/// The variant only describes the *log*; the body returned alongside it is
/// always a faithful stand-in for the original one.
pub(crate) enum Collected {
    /// Every frame was read and fit in the cap.
    Complete(Bytes),
    /// The body was not (fully) collected: it either exceeded the cap — the
    /// frame crossing it is split, so at most `cap` bytes are ever buffered —
    /// or the stream produced an `Err`.
    Truncated {
        /// Data bytes read before giving up on buffering (`== cap` on overflow).
        collected: usize,
        /// First [`BODY_PREVIEW_MAX`] data bytes, for the log preview.
        sample: Bytes,
    },
}

/// Collect `body` up to `cap` bytes without ever destroying the payload.
///
/// Returns `(body, collected, body_error)`:
///
/// - `body` is either the buffered bytes (`Collected::Complete`) or a frame
///   chain that replays every frame already read and then continues with the
///   untouched remainder — same data, same trailers, same error, so the
///   caller can forward it as-is.
/// - `body_error` is `true` when the source body produced an `Err`. That error
///   is forwarded through `body` (never swallowed) so the client observes a
///   stream error instead of a body that looks complete.
///
/// Buffering is bounded by `cap`: the truncated path stops at exactly `cap`
/// bytes, and the complete path only ever copies frames it already read.
pub(crate) async fn collect_or_stream(body: Body, cap: usize) -> (Body, Collected, bool) {
    let mut body = body;
    let mut frames: Vec<Result<Frame<Bytes>, axum::Error>> = Vec::new();
    let mut collected = 0usize;
    let mut sample: Vec<u8> = Vec::new();

    loop {
        // `BodyExt::frame` only borrows the body, so the unconsumed remainder
        // stays owned here and can be chained into the forwarded body below.
        let next = std::future::poll_fn(|cx| std::pin::Pin::new(&mut body.frame()).poll(cx)).await;
        match next {
            // End of stream: everything fit in `cap`.
            None => {
                let bytes = concat_data(&frames);
                return (Body::from(bytes.clone()), Collected::Complete(bytes), false);
            }
            Some(Ok(frame)) => match frame.into_data() {
                Ok(data) => {
                    let room = cap.saturating_sub(collected);
                    if data.len() > room {
                        // The frame that crosses the cap: forward the part
                        // that still fits now, stream the rest with the
                        // untouched body. No byte is lost, no byte is
                        // buffered twice.
                        let head = data.slice(..room);
                        let tail = data.slice(room..);
                        if !head.is_empty() {
                            extend_sample(&mut sample, &head);
                            collected += head.len();
                            frames.push(Ok(Frame::data(head)));
                        }
                        frames.push(Ok(Frame::data(tail)));
                        return (
                            Body::new(StreamBody::new(
                                futures_util::stream::iter(frames).chain(BodyStream::new(body)),
                            )),
                            Collected::Truncated {
                                collected,
                                sample: Bytes::from(sample),
                            },
                            false,
                        );
                    }
                    extend_sample(&mut sample, &data);
                    collected += data.len();
                    frames.push(Ok(Frame::data(data)));
                }
                // Trailers frame: carries no data, kept verbatim so it still
                // reaches the client.
                Err(frame) => frames.push(Ok(frame)),
            },
            // Body error: forward it, never swallow it.
            Some(Err(err)) => {
                frames.push(Err(err));
                return (
                    Body::new(StreamBody::new(
                        futures_util::stream::iter(frames).chain(BodyStream::new(body)),
                    )),
                    Collected::Truncated {
                        collected,
                        sample: Bytes::from(sample),
                    },
                    true,
                );
            }
        }
    }
}

/// Concatenate the data payload of the already-collected frames.
fn concat_data(frames: &[Result<Frame<Bytes>, axum::Error>]) -> Bytes {
    // The common case is a single data frame: `Bytes` is refcounted, so
    // cloning it keeps the ≤ cap path allocation-free.
    if frames.len() == 1
        && let Ok(frame) = &frames[0]
        && let Some(data) = frame.data_ref()
    {
        return data.clone();
    }
    let capacity = frames
        .iter()
        .filter_map(|f| f.as_ref().ok()?.data_ref().map(Bytes::len))
        .sum();
    let mut buf: Vec<u8> = Vec::with_capacity(capacity);
    for frame in frames {
        if let Ok(frame) = frame
            && let Some(data) = frame.data_ref()
        {
            buf.extend_from_slice(data);
        }
    }
    Bytes::from(buf)
}

/// Keep at most [`BODY_PREVIEW_MAX`] data bytes for the log preview.
fn extend_sample(sample: &mut Vec<u8>, data: &[u8]) {
    let room = BODY_PREVIEW_MAX.saturating_sub(sample.len());
    if room > 0 {
        sample.extend_from_slice(&data[..data.len().min(room)]);
    }
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
        preview_with_total(&bytes[..BODY_PREVIEW_MAX], bytes.len())
    }
}

/// Preview for a body that was only partially collected: `sample` holds at
/// most [`BODY_PREVIEW_MAX`] bytes, `collected` is the real number of bytes
/// read (which may be far larger).
fn preview_from_sample(sample: &[u8], collected: usize) -> String {
    if collected <= BODY_PREVIEW_MAX {
        String::from_utf8_lossy(sample).to_string()
    } else {
        preview_with_total(sample, collected)
    }
}

fn preview_with_total(sample: &[u8], total: usize) -> String {
    let preview = String::from_utf8_lossy(sample);
    format!("{preview}...(truncated, {total} bytes total)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    const CAP: usize = 64;

    /// Body that yields exactly the given frames.
    fn body_from(frames: Vec<Result<Frame<Bytes>, axum::Error>>) -> Body {
        Body::new(StreamBody::new(futures_util::stream::iter(frames)))
    }

    /// Drain a body, returning its data, last trailers and whether any frame
    /// was an error.
    async fn read_all(mut body: Body) -> (Vec<u8>, Option<HeaderMap>, bool) {
        let mut data = Vec::new();
        let mut trailers = None;
        let mut errored = false;
        while let Some(frame) = body.frame().await {
            match frame {
                Ok(frame) => match frame.into_data() {
                    Ok(chunk) => data.extend_from_slice(&chunk),
                    Err(frame) => trailers = frame.into_trailers().ok(),
                },
                Err(_) => errored = true,
            }
        }
        (data, trailers, errored)
    }

    /// ① Exactly at the cap is still a complete buffered body.
    #[tokio::test]
    async fn exactly_cap_is_complete() {
        let payload = vec![b'a'; CAP];
        let (body, collected, body_error) =
            collect_or_stream(Body::from(payload.clone()), CAP).await;
        assert!(!body_error);
        match collected {
            Collected::Complete(bytes) => assert_eq!(&bytes[..], &payload[..]),
            Collected::Truncated { .. } => {
                panic!("a body of exactly `cap` bytes must stay complete")
            }
        }
        assert_eq!(
            http_body::Body::size_hint(&body).exact(),
            Some(CAP as u64),
            "the complete path keeps an exact size hint"
        );
        let (data, trailers, errored) = read_all(body).await;
        assert_eq!(data, payload);
        assert!(trailers.is_none());
        assert!(!errored);
    }

    /// ② One byte over the cap is truncated for logging but forwarded in full.
    #[tokio::test]
    async fn over_cap_forwards_every_byte() {
        let payload: Vec<u8> = (0..CAP + 1).map(|i| b'a' + (i % 26) as u8).collect();
        let (body, collected, body_error) =
            collect_or_stream(Body::from(payload.clone()), CAP).await;
        assert!(!body_error);
        match collected {
            Collected::Truncated { collected, sample } => {
                assert_eq!(collected, CAP, "the buffer stops exactly at the cap");
                assert_eq!(&sample[..], &payload[..CAP]);
            }
            Collected::Complete(_) => panic!("cap + 1 bytes must be truncated"),
        }
        let (data, _, errored) = read_all(body).await;
        assert!(!errored);
        assert_eq!(data, payload, "the client must receive the whole payload");
    }

    /// ③ A mid-stream error is propagated, not turned into a clean end.
    #[tokio::test]
    async fn body_error_is_forwarded() {
        let payload = b"hello".to_vec();
        let (body, collected, body_error) = collect_or_stream(
            body_from(vec![
                Ok(Frame::data(Bytes::from(payload.clone()))),
                Err(axum::Error::new(std::io::Error::other("boom"))),
            ]),
            CAP,
        )
        .await;
        assert!(body_error, "the error must be reported to the caller");
        assert!(matches!(collected, Collected::Truncated { .. }));
        let (data, _, errored) = read_all(body).await;
        assert_eq!(data, payload, "already-read data is still forwarded");
        assert!(errored, "the client must observe the stream error");
    }

    /// ④ Trailers survive the truncated path.
    #[tokio::test]
    async fn trailers_survive_truncation() {
        let mut trailers = HeaderMap::new();
        trailers.insert("x-checksum", "abc".parse().unwrap());
        let (body, collected, body_error) = collect_or_stream(
            body_from(vec![
                Ok(Frame::data(Bytes::from(vec![b'a'; CAP / 2]))),
                Ok(Frame::trailers(trailers.clone())),
                Ok(Frame::data(Bytes::from(vec![b'b'; CAP]))),
            ]),
            CAP,
        )
        .await;
        assert!(!body_error);
        match collected {
            Collected::Truncated { collected, .. } => assert_eq!(collected, CAP),
            Collected::Complete(_) => panic!("the payload exceeds the cap"),
        }
        let (data, got_trailers, errored) = read_all(body).await;
        assert!(!errored);
        assert_eq!(data.len(), CAP / 2 + CAP, "no data frame may be dropped");
        assert_eq!(got_trailers, Some(trailers), "trailers must be preserved");
    }
}
