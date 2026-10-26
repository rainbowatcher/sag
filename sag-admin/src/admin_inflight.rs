//! Live per-route in-flight counters over SSE.
//!
//! - `GET /api/inflight` — `text/event-stream` of [`InFlightSnapshot`]s.
//!
//! Mounted under `/api` by [`crate::admin::build_admin_routes`], so it is
//! auth-protected by the `gateway_auth` middleware exactly like the other
//! `/api/*` routes.
//!
//! The stream is **snapshot-only**: `InFlightRegistry` publishes a full
//! immutable snapshot through a `watch` channel on every acquire/release
//! (bursts are coalesced, so no per-request frames), which means
//!
//! - the first frame after connecting is the current state — a reconnecting
//!   client is healed without a separate "full state" request;
//! - every later frame is again the whole state, so a dropped frame can never
//!   desynchronise the consumer;
//! - the endpoint itself neither counts nor holds an in-flight slot: dropping
//!   the response body (client disconnect) touches no counter.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, Stream};
use sag_state::{AppState, InFlightSnapshot};
use tokio::sync::watch;

/// SSE event name of a snapshot frame (the UI listens for `inflight`).
pub const INFLIGHT_EVENT: &str = "inflight";

/// Idle heartbeat period: a `: ping` comment every 15 s keeps an otherwise
/// silent stream alive through proxies and gives the client a liveness tick.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// `GET /api/inflight`
///
/// Responds with an SSE stream that emits one `event: inflight` frame per
/// published snapshot (JSON `{"seq","total","routes":[{"model_name","count"}]}`),
/// starting with the snapshot current at connect time, plus a `: ping`
/// comment whenever the registry stays quiet for [`HEARTBEAT_INTERVAL`].
pub async fn stream_inflight(State(state): State<AppState>) -> Response {
    let mut response = Sse::new(inflight_stream(state.in_flight.subscribe())).into_response();
    // `Sse` already sets `Content-Type: text/event-stream` and a plain
    // `Cache-Control: no-cache`; the anti-buffering set below matches the
    // dispatch SSE responses (`no-transform` + `X-Accel-Buffering: no`) so an
    // intermediate proxy neither buffers nor re-compresses the frames.
    // `Connection: keep-alive` is deliberately absent — it is a hop-by-hop
    // header, which HTTP/2 forbids.
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

/// Encode one snapshot as an `event: inflight` frame.
pub fn inflight_event(snapshot: &InFlightSnapshot) -> Event {
    match Event::default().event(INFLIGHT_EVENT).json_data(snapshot) {
        Ok(event) => event,
        // `InFlightSnapshot` is numbers and strings only, so serialisation
        // cannot fail in practice. Degrading to a comment keeps the stream
        // alive instead of panicking inside `Stream::poll_next`.
        Err(_) => Event::default().comment("snapshot serialization failed"),
    }
}

/// The stream behind [`stream_inflight`]: current snapshot, then one frame per
/// change, with an idle heartbeat in between.
pub fn inflight_stream(
    rx: watch::Receiver<InFlightSnapshot>,
) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
    inflight_stream_with_heartbeat(rx, HEARTBEAT_INTERVAL)
}

/// [`inflight_stream`] with an explicit heartbeat period (tests shrink it).
fn inflight_stream_with_heartbeat(
    rx: watch::Receiver<InFlightSnapshot>,
    heartbeat: Duration,
) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
    stream::unfold(StreamState::Initial(rx), move |state| async move {
        match state {
            // First poll: hand out the snapshot current at connect time.
            // `borrow_and_update` marks it as seen, so the following
            // `changed()` only wakes for *later* publications — the initial
            // frame can never be a stale duplicate.
            StreamState::Initial(mut rx) => {
                let snapshot = rx.borrow_and_update().clone();
                Some((Ok(inflight_event(&snapshot)), StreamState::Watching(rx)))
            }
            StreamState::Watching(mut rx) => {
                match tokio::time::timeout(heartbeat, rx.changed()).await {
                    Ok(Ok(())) => {
                        let snapshot = rx.borrow_and_update().clone();
                        Some((Ok(inflight_event(&snapshot)), StreamState::Watching(rx)))
                    }
                    // The registry is gone (sender dropped): the process is
                    // shutting down, so end the stream cleanly.
                    Ok(Err(_)) => None,
                    // Quiet for a whole heartbeat period: comment frame.
                    Err(_) => Some((
                        Ok(Event::default().comment("ping")),
                        StreamState::Watching(rx),
                    )),
                }
            }
        }
    })
}

/// Unfold state: emit the connect-time snapshot first, then watch for changes.
enum StreamState {
    Initial(watch::Receiver<InFlightSnapshot>),
    Watching(watch::Receiver<InFlightSnapshot>),
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use http_body_util::BodyExt;
    use sag_state::InFlightRegistry;

    use super::*;

    /// Body of the SSE response built from `stream`, i.e. the bytes a client
    /// would actually receive (the encoded frames are not reachable otherwise:
    /// `Event::finalize` is private to axum).
    fn sse_body(stream: impl Stream<Item = Result<Event, Infallible>> + Send + 'static) -> Body {
        Sse::new(stream).into_response().into_body()
    }

    /// Read the next body frame as text, bounded by a short timeout.
    async fn next_frame(body: &mut Body) -> String {
        let frame = tokio::time::timeout(Duration::from_secs(1), body.frame())
            .await
            .expect("frame must arrive")
            .expect("stream must not end")
            .expect("stream must not error");
        String::from_utf8(frame.into_data().expect("data frame").to_vec()).unwrap()
    }

    #[tokio::test]
    async fn frame_encodes_event_name_and_json_payload() {
        let registry = Arc::new(InFlightRegistry::new());
        let guard = registry.acquire("gpt-4o");
        let mut body = sse_body(inflight_stream_with_heartbeat(
            registry.subscribe(),
            Duration::from_secs(60),
        ));

        assert_eq!(
            next_frame(&mut body).await,
            "event: inflight\ndata: {\"seq\":1,\"total\":1,\"routes\":[{\"model_name\":\"gpt-4o\",\"count\":1}]}\n\n",
        );
        drop(guard);
    }

    #[tokio::test]
    async fn stream_emits_connect_time_snapshot_then_changes() {
        let registry = Arc::new(InFlightRegistry::new());
        let mut body = sse_body(inflight_stream_with_heartbeat(
            registry.subscribe(),
            Duration::from_secs(60),
        ));

        let first = next_frame(&mut body).await;
        assert!(
            first.contains("data: {\"seq\":0,\"total\":0,\"routes\":[]}"),
            "first frame must be the empty connect-time snapshot, got {first:?}"
        );

        let guard = registry.acquire("gpt-4o");
        let second = next_frame(&mut body).await;
        assert!(
            second.contains("\"total\":1"),
            "acquire must publish a new snapshot, got {second:?}"
        );

        drop(guard);
        let third = next_frame(&mut body).await;
        assert!(
            third.contains("\"routes\":[]"),
            "release must publish an empty snapshot, got {third:?}"
        );
    }

    #[tokio::test]
    async fn stream_emits_heartbeat_comment_when_idle() {
        let registry = Arc::new(InFlightRegistry::new());
        let mut body = sse_body(inflight_stream_with_heartbeat(
            registry.subscribe(),
            Duration::from_millis(20),
        ));

        assert!(next_frame(&mut body).await.starts_with("event: inflight"));
        assert_eq!(
            next_frame(&mut body).await,
            ": ping\n\n",
            "an idle period must produce a heartbeat comment"
        );
    }

    #[tokio::test]
    async fn stream_ends_when_registry_is_dropped() {
        let registry = Arc::new(InFlightRegistry::new());
        let rx = registry.subscribe();
        let mut body = sse_body(inflight_stream_with_heartbeat(rx, Duration::from_secs(60)));

        let _ = next_frame(&mut body).await;
        drop(registry);
        let ended = tokio::time::timeout(Duration::from_secs(1), body.frame())
            .await
            .expect("dropping the registry must end the stream");
        assert!(ended.is_none(), "the stream must end, not hang");
    }
}
