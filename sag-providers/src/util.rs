use std::collections::HashMap;
use std::time::Duration;

use reqwest::RequestBuilder;
use sag_core::{ApiKind, StreamChunk, Usage};
use tokio::sync::mpsc;
use tracing::{Instrument, debug, error, warn};

/// Return the (name, value) pairs for auth headers used for a given API kind.
///
/// - `AnthropicMessages` → `x-api-key: {key}` + `anthropic-version: 2023-06-01`
/// - everything else (`ChatCompletions` / `Responses` / `Image`) →
///   `Authorization: Bearer {key}`
pub fn provider_auth_headers(kind: ApiKind, api_key: &str) -> Vec<(&'static str, String)> {
    match kind {
        ApiKind::AnthropicMessages => vec![
            ("x-api-key", api_key.to_string()),
            ("anthropic-version", "2023-06-01".to_string()),
        ],
        _ => vec![("Authorization", format!("Bearer {api_key}"))],
    }
}

/// Apply extra and custom headers to a reqwest request builder.
/// Custom headers override default/extra headers (such as Content-Type or Auth headers)
/// when key names match (case-insensitive).
pub fn apply_headers(
    req: RequestBuilder,
    extra_headers: &[(&str, &str)],
    custom_headers: &HashMap<String, String>,
) -> RequestBuilder {
    let mut map = reqwest::header::HeaderMap::new();
    map.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    for (k, v) in extra_headers {
        if let (Ok(k), Ok(v)) = (
            reqwest::header::HeaderName::try_from(*k),
            reqwest::header::HeaderValue::try_from(*v),
        ) {
            map.insert(k, v);
        }
    }
    for (k, v) in custom_headers {
        if let (Ok(k), Ok(v)) = (
            reqwest::header::HeaderName::try_from(k.as_str()),
            reqwest::header::HeaderValue::try_from(v.as_str()),
        ) {
            map.insert(k, v);
        }
    }
    req.headers(map)
}

/// Merge body overrides into a serialized JSON value at the top level.
///
/// Each key in `overrides` is inserted into (or overwritten in) the body's
/// top-level object. Fields with nested objects are replaced wholesale.
pub fn apply_body_overrides(
    mut body: serde_json::Value,
    overrides: &HashMap<String, serde_json::Value>,
) -> serde_json::Value {
    if overrides.is_empty() {
        return body;
    }
    if let serde_json::Value::Object(map) = &mut body {
        for (key, value) in overrides {
            map.insert(key.clone(), value.clone());
        }
    }
    body
}

/// Remove the listed top-level fields from a serialized JSON body.
///
/// Applied after all override layers (`body_overrides`, `model_overrides`,
/// `RouteTarget.extra`) so a stripped field is guaranteed absent from the
/// final upstream body. No-op for non-object bodies or empty `fields`.
pub fn apply_body_strip(mut body: serde_json::Value, fields: &[String]) -> serde_json::Value {
    if fields.is_empty() {
        return body;
    }
    if let serde_json::Value::Object(map) = &mut body {
        for key in fields {
            map.remove(key);
        }
    }
    body
}

// ── SSE stream parsing helpers ─────────────────────────────────────

/// Iterate the byte buffer, extract complete SSE frames (delimited by `\n\n` or `\r\n\r\n`),
/// and pass each frame to `process_frame`.
///
/// Returns `true` when `process_frame` signals an error; draining stops
/// immediately in that case.
pub fn drain_sse_frames_with<F>(
    buffer: &mut Vec<u8>,
    tx: &mpsc::UnboundedSender<StreamChunk>,
    mut process_frame: F,
) -> bool
where
    F: FnMut(&[u8], &mpsc::UnboundedSender<StreamChunk>) -> bool,
{
    let mut saw_error = false;
    while let Some(end) = find_sse_boundary(buffer) {
        let frame: Vec<u8> = buffer.drain(..end).collect();
        // Remove the delimiter (\n\n or \r\n\r\n)
        let delim_len = if buffer.starts_with(b"\r\n\r\n") {
            4
        } else {
            2
        };
        buffer.drain(..delim_len);
        if process_frame(&frame, tx) {
            saw_error = true;
            break;
        }
    }
    saw_error
}

/// Iterate the byte buffer, extract complete SSE frames (delimited by `\n\n` or `\r\n\r\n`),
/// parse them, and send the resulting `StreamChunk` items through the channel.
///
/// Returns `true` when a frame carried an upstream error delivered inside a
/// 200 SSE data frame (the frame was forwarded and an `Error` chunk sent by
/// `process_sse_frame`); the caller must stop forwarding further frames.
pub fn drain_sse_frames(buffer: &mut Vec<u8>, tx: &mpsc::UnboundedSender<StreamChunk>) -> bool {
    drain_sse_frames_with(buffer, tx, process_sse_frame)
}

/// Drain remaining frames and flush any un-delimited trailing frame upon stream end,
/// passing each frame to `process_frame`.
///
/// Returns `true` when the trailing frame (or a frame drained first) signaled
/// an error.
pub fn flush_sse_buffer_with<F>(
    buffer: &mut Vec<u8>,
    tx: &mpsc::UnboundedSender<StreamChunk>,
    mut process_frame: F,
) -> bool
where
    F: FnMut(&[u8], &mpsc::UnboundedSender<StreamChunk>) -> bool,
{
    let mut saw_error = drain_sse_frames_with(buffer, tx, &mut process_frame);
    if !saw_error && !buffer.is_empty() {
        let frame = std::mem::take(buffer);
        saw_error = process_frame(&frame, tx);
    }
    saw_error
}

/// Drain remaining frames and flush any un-delimited trailing frame upon stream end.
///
/// Returns `true` when the trailing frame (or a frame drained first) carried a
/// 200-delivered upstream error, mirroring [`drain_sse_frames`].
pub fn flush_sse_buffer(buffer: &mut Vec<u8>, tx: &mpsc::UnboundedSender<StreamChunk>) -> bool {
    flush_sse_buffer_with(buffer, tx, process_sse_frame)
}

/// Emit the SSE chunk idle-timeout error path: log + `Error` chunk.
///
/// Returns `true` (a timeout is always an error terminal — the caller must
/// not emit `Done`). The `Error` chunk message is only sent when
/// `chunk_timeout` is `Some` (the timeout branch is only reachable then).
pub fn emit_idle_timeout(
    tx: &mpsc::UnboundedSender<StreamChunk>,
    provider_label: &str,
    chunk_timeout: &Option<Duration>,
) -> bool {
    error!(chunk_timeout_secs = ?chunk_timeout, provider = provider_label, "stream chunk idle timeout");
    if !tx.is_closed()
        && let Some(s) = chunk_timeout
    {
        let _ = tx.send(StreamChunk::Error(format!(
            "stream chunk idle timeout after {}s",
            s.as_secs()
        )));
    }
    true
}

/// Spawn `fut` with the caller's current [`tracing::Span`] attached.
///
/// Provider stream readers run in a detached task that outlives
/// `chat_stream` / `messages_stream`, so without this the task's own events
/// (upstream chunk read errors, chunk idle timeouts) land in the log with
/// no provider / model / request `trace_id` context and cannot be
/// correlated with the request that caused them.
///
/// `Instrument` is used rather than `Span::enter()` because an `Entered`
/// guard must never be held across an `.await` point (it would leak the
/// span into unrelated tasks polled on the same worker thread).
pub fn spawn_in_current_span<F>(fut: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(fut.instrument(tracing::Span::current()))
}

/// Search for the SSE frame delimiter `\n\n` (or `\r\n\r\n`), returning the
/// earliest match in the buffer.
pub(crate) fn find_sse_boundary(buf: &[u8]) -> Option<usize> {
    let p_crlf = if buf.len() >= 4 {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    } else {
        None
    };
    let p_lf = buf.windows(2).position(|w| w == b"\n\n");

    match (p_crlf, p_lf) {
        (Some(c), Some(l)) => Some(c.min(l)),
        (Some(c), None) => Some(c),
        (None, Some(l)) => Some(l),
        (None, None) => None,
    }
}

/// Parse a single SSE frame and send chunks through the channel.
///
/// Returns `true` when the frame carried a 200-delivered upstream error (a
/// data line whose JSON has a top-level `error` object): the raw
/// frame is still forwarded to the client for behavior compatibility, an
/// `Error` chunk is emitted, and the caller must stop processing further
/// frames.
fn process_sse_frame(frame: &[u8], tx: &mpsc::UnboundedSender<StreamChunk>) -> bool {
    let text = String::from_utf8_lossy(frame);

    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let json_str = rest.trim_start();

        if json_str == "[DONE]" {
            let _ = tx.send(StreamChunk::Data(b"data: [DONE]\n\n".to_vec()));
            return false;
        }

        // Compute the raw SSE data line; forwarding is deferred until after
        // JSON/usage parsing so that anomalous usage frames (see
        // `handle_usage_frame`) can be selectively intercepted rather than
        // forwarded unconditionally.
        let raw = format!("data: {json_str}\n\n").into_bytes();

        // Non-JSON text (e.g. provider-specific keepalive comments) is forwarded
        // verbatim without further inspection.
        let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) else {
            let _ = tx.send(StreamChunk::Data(raw));
            continue;
        };

        // An upstream error delivered inside a 200 SSE data frame.
        if crate::base::body_has_error_object(&val) {
            return forward_error_frame(tx, &val, raw);
        }

        if let Some(usage_val) = val.get("usage") {
            handle_usage_frame(tx, &val, usage_val, raw);
        } else {
            // JSON frame without a usage field — forward verbatim.
            let _ = tx.send(StreamChunk::Data(raw));
        }
    }
    false
}

/// Forward a 200-delivered upstream error frame and surface the in-stream
/// error. The JSON carries a top-level `error` object: forward the frame to
/// the client (behavior-compatible), then surface the error so the
/// dispatcher records the attempt as a FAILURE, not success or
/// client_cancelled. Returns `true` so the caller stops processing further
/// frames.
fn forward_error_frame(
    tx: &mpsc::UnboundedSender<StreamChunk>,
    val: &serde_json::Value,
    raw: Vec<u8>,
) -> bool {
    let err_obj = val.get("error").expect("checked error object");
    let msg = crate::base::body_error_message(err_obj);
    warn!(status = 200, %msg, "upstream returned error");
    // Enqueue the `Error` chunk BEFORE the forwarded `Data` frame.
    // The dispatch unfold loop (stream.rs) consumes `Error` inline
    // — recording the analytics error state — before it yields the
    // `Data` frame to the client. If the client disconnects
    // immediately after receiving the `Data` frame (the production
    // incident), the stream is dropped without polling the `Error`
    // chunk, so the error state must already be recorded by the
    // time the `Data` frame is delivered: this ordering makes the
    // request classify as `model_failure` / 502 (with the upstream
    // message), never `client_cancelled` / 499. The client still
    // receives the forwarded error frame (behavior-compatible).
    let _ = tx.send(StreamChunk::Error(msg));
    let _ = tx.send(StreamChunk::Data(raw));
    true
}

/// Extract a [`Usage`] from a JSON `usage` object, applying the
/// completion_tokens fallback strategies (see inline comments).
///
/// Shared by stream frame handling and the Chat Completions codec.
pub fn extract_usage_from_value(usage_val: &serde_json::Value) -> Usage {
    let pt = usage_val
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let mut ct = usage_val
        .get("completion_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;

    let total_from_upstream = usage_val
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    // OpenAI nests cache/reasoning tokens inside details objects.
    let pt_details = usage_val.get("prompt_tokens_details");
    let cached_tokens = pt_details
        .and_then(|v| v.get("cached_tokens"))
        .or_else(|| usage_val.get("prompt_cache_hit_tokens"))
        .or_else(|| usage_val.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .or(pt_details.is_some().then_some(0));
    let cache_creation_tokens = pt_details
        .and_then(|v| v.get("cache_creation_input_tokens"))
        .or_else(|| usage_val.get("cache_creation_input_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);
    let reasoning_tokens = usage_val
        .get("completion_tokens_details")
        .and_then(|v| v.get("reasoning_tokens"))
        .or_else(|| usage_val.get("reasoning_tokens"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32);

    // Fallback for missing/zero completion_tokens:
    if ct == 0
        && let Some(tot) = total_from_upstream
        && tot > pt
    {
        ct = tot - pt;
    }
    // Strategy 2: completion_tokens must be at least reasoning_tokens.
    // Note: If Strategy 2 intervenes due to self-contradictory upstream usage fields
    // (e.g. prompt=100, reasoning=50, total=110), pt + ct (150) may exceed upstream total (110).
    // We intentionally prioritize completion_tokens >= reasoning_tokens over total consistency.
    if let Some(rt) = reasoning_tokens
        && ct < rt
    {
        ct = rt;
    }

    let total = total_from_upstream.unwrap_or_else(|| pt.saturating_add(ct));
    Usage {
        prompt_tokens: pt,
        completion_tokens: ct,
        total_tokens: total,
        cached_tokens,
        cache_creation_tokens,
        reasoning_tokens,
    }
}

/// A frame that credits prompt tokens but zero completion tokens (after all
/// fallback strategies) is anomalous.
fn usage_is_anomalous(u: &Usage) -> bool {
    u.completion_tokens == 0 && u.prompt_tokens > 0
}

/// Guard against false positives: if the frame also carries a meaningful
/// choice (non-empty content / reasoning_content / refusal / tool_calls, or
/// a non-null finish_reason) we must forward it to avoid dropping content or
/// the stream-termination signal.
fn frame_has_meaningful_choice(val: &serde_json::Value) -> bool {
    val.get("choices")
        .and_then(|c| c.as_array())
        .is_some_and(|arr| {
            arr.iter().any(|choice| {
                let has_finish = choice.get("finish_reason").is_some_and(|f| !f.is_null());
                let has_delta = choice.get("delta").is_some_and(|d| {
                    d.get("content")
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| !s.is_empty())
                        || d.get("reasoning_content")
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| !s.is_empty())
                        || d.get("refusal")
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| !s.is_empty())
                        || d.get("tool_calls")
                            .and_then(|t| t.as_array())
                            .is_some_and(|a| !a.is_empty())
                });
                has_finish || has_delta
            })
        })
}

/// Handle a data frame that carries a `usage` object: extract token counts,
/// intercept anomalous usage-only frames (skip the Data send while still
/// emitting Usage for CallLog diagnosis), and emit Data/Usage chunks.
fn handle_usage_frame(
    tx: &mpsc::UnboundedSender<StreamChunk>,
    val: &serde_json::Value,
    usage_val: &serde_json::Value,
    raw: Vec<u8>,
) {
    let usage = extract_usage_from_value(usage_val);

    // When an anomalous frame also lacks meaningful choices, forwarding the
    // raw SSE data to the client is actively harmful:
    //
    //   1. Many third-party clients unconditionally index `choices[0]`
    //      and crash with IndexError on the empty `choices: []` that
    //      accompanies the usage-only final chunk.
    //   2. The OpenAI Python SDK's `CompletionUsage` pydantic model
    //      requires `completion_tokens` as a mandatory `int`; when the
    //      upstream omits the field entirely (as observed in production
    //      logs) the SDK raises `APIResponseValidationError`. An
    //      explicit `0` is a valid int, but the empty `choices` still
    //      triggers point 1.
    //
    // We therefore intercept the raw data frame (skip the Data send)
    // while still emitting `StreamChunk::Usage` so the gateway's own
    // CallLog records the anomalous token counts for diagnosis.
    let is_anomalous = usage_is_anomalous(&usage);
    let has_meaningful_choice = frame_has_meaningful_choice(val);
    let intercept = is_anomalous && !has_meaningful_choice;

    if is_anomalous {
        let choices_arr = val.get("choices").and_then(|c| c.as_array());
        warn!(
            pt = usage.prompt_tokens,
            ct = usage.completion_tokens,
            usage = %usage_val,
            intercepted = intercept,
            choices_count = choices_arr.map(|a| a.len()).unwrap_or(0),
            finish_reasons = ?choices_arr
                .map(|a| a.iter()
                    .map(|ch| ch.get("finish_reason").and_then(|f| f.as_str()))
                    .collect::<Vec<_>>()),
            "upstream usage frame credits zero completion_tokens despite non-zero prompt; \
             this is the source of Completion=0 in the call log",
        );
    }

    if !intercept {
        let _ = tx.send(StreamChunk::Data(raw));
    }

    // Skip entirely-zero usage objects to avoid noise; partial
    // data (e.g. prompt_tokens present but completion_tokens
    // absent) is still valuable for cache/reasoning extraction.
    if usage.prompt_tokens == 0 && usage.completion_tokens == 0 {
        debug!(usage = %usage_val, "skipping all-zero usage frame");
        return;
    }

    let _ = tx.send(StreamChunk::Usage(usage));
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── spawn_in_current_span ───────────────────────────────────

    /// Minimal `MakeWriter` capturing formatted events into a buffer.
    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Capture {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
        }
    }

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Install the capturing subscriber once for the whole test binary.
    fn capture_logs() -> Capture {
        static CAPTURE: std::sync::OnceLock<Capture> = std::sync::OnceLock::new();
        CAPTURE
            .get_or_init(|| {
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
            .clone()
    }

    /// A detached reader task must inherit the caller's span, otherwise its
    /// events (upstream chunk read errors, idle timeouts) carry no provider /
    /// model / request `trace_id` and cannot be correlated with a request.
    #[tokio::test]
    async fn spawn_in_current_span_keeps_span_context() {
        let capture = capture_logs();
        let span = tracing::info_span!("reader", provider = "volc", model = "glm-5.3-flash");
        let handle = {
            let _entered = span.enter();
            spawn_in_current_span(async {
                tracing::error!("chunk read failed");
            })
        };
        // The await happens *outside* the span scope: the context must come
        // from the instrumentation captured at spawn time.
        handle.await.unwrap();

        let text = capture.text();
        let line = text
            .lines()
            .find(|l| l.contains("chunk read failed"))
            .unwrap_or_else(|| panic!("event not captured: {text}"));
        assert!(
            line.contains("reader{provider=\"volc\" model=\"glm-5.3-flash\"}"),
            "spawned task must log inside the caller's span: {line}"
        );
    }

    // ── apply_body_strip tests ───────────────────────────────────

    #[test]
    fn strip_removes_specified_field() {
        let body = serde_json::json!({
            "model": "gpt-4o",
            "chat_template_kwargs": { "enable_thinking": false },
            "temperature": 0.7,
        });
        let fields = vec!["chat_template_kwargs".to_string()];
        let result = apply_body_strip(body, &fields);
        assert!(result.get("chat_template_kwargs").is_none());
        assert_eq!(result.get("model").unwrap(), "gpt-4o");
        assert_eq!(
            result.get("temperature").and_then(|v| v.as_f64()),
            Some(0.7)
        );
    }

    #[test]
    fn strip_empty_fields_is_noop() {
        let body = serde_json::json!({ "model": "gpt-4o", "temperature": 0.7 });
        let result = apply_body_strip(body.clone(), &[]);
        assert_eq!(result, body);
    }

    #[test]
    fn strip_applied_after_insert_removes_field() {
        // Simulate: body_overrides inserts chat_template_kwargs, then strip removes it.
        let body = serde_json::json!({ "model": "gpt-4o" });
        let mut with_override = body.clone();
        if let serde_json::Value::Object(map) = &mut with_override {
            map.insert(
                "chat_template_kwargs".into(),
                serde_json::json!({ "enable_thinking": false }),
            );
        }
        let fields = vec!["chat_template_kwargs".to_string()];
        let result = apply_body_strip(with_override, &fields);
        assert!(result.get("chat_template_kwargs").is_none());
        assert_eq!(result.get("model").unwrap(), "gpt-4o");
    }

    #[test]
    fn strip_non_object_body_is_noop() {
        let body = serde_json::json!([1, 2, 3]);
        let fields = vec!["chat_template_kwargs".to_string()];
        let result = apply_body_strip(body.clone(), &fields);
        assert_eq!(result, body, "array body must pass through unchanged");
    }

    #[test]
    fn strip_nonexistent_field_is_noop() {
        let body = serde_json::json!({ "model": "gpt-4o", "temperature": 0.7 });
        let fields = vec!["nonexistent".to_string()];
        let result = apply_body_strip(body.clone(), &fields);
        assert_eq!(
            result, body,
            "body unchanged when stripping nonexistent field"
        );
    }

    /// Construct an SSE frame with a usage chunk that includes the
    /// OpenAI-style nested cache/reasoning details, feed it through
    /// `process_sse_frame`, and assert the extracted `Usage` fields.
    #[test]
    fn process_sse_frame_extracts_nested_cache_and_reasoning_tokens() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        // A single SSE frame with an OpenAI usage payload that nests
        // cached_tokens inside prompt_tokens_details and reasoning_tokens
        // inside completion_tokens_details.
        let frame = b"data: {\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"total_tokens\":150,\"prompt_tokens_details\":{\"cached_tokens\":30},\"completion_tokens_details\":{\"reasoning_tokens\":12}}}\n\n";
        process_sse_frame(frame, &tx);
        drop(tx);

        // The frame produces two chunks: a Data chunk (raw SSE forward) and
        // a Usage chunk (extracted from the JSON). Collect all.
        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 2, "expected Data + Usage chunks");

        let usage = chunks
            .iter()
            .find_map(|c| match c {
                StreamChunk::Usage(u) => Some(u),
                _ => None,
            })
            .expect("must contain a Usage chunk");

        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.total_tokens, 150, "should prefer upstream total");
        assert_eq!(
            usage.cached_tokens,
            Some(30),
            "cached_tokens must be lifted from prompt_tokens_details"
        );
        assert_eq!(
            usage.reasoning_tokens,
            Some(12),
            "reasoning_tokens must be lifted from completion_tokens_details"
        );
        assert!(
            usage.cache_creation_tokens.is_none(),
            "absent cache_creation_input_tokens → None"
        );
    }

    /// When the upstream payload omits `total_tokens`, the fallback
    /// `prompt_tokens + completion_tokens` should be used.
    #[test]
    fn process_sse_frame_falls_back_total_tokens_when_absent() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        let frame = b"data: {\"usage\":{\"prompt_tokens\":80,\"completion_tokens\":20}}\n\n";
        process_sse_frame(frame, &tx);
        drop(tx);

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }
        let usage = chunks
            .iter()
            .find_map(|c| match c {
                StreamChunk::Usage(u) => Some(u),
                _ => None,
            })
            .expect("must contain a Usage chunk");

        assert_eq!(usage.total_tokens, 100, "80 + 20 fallback");
    }

    /// When upstream omits `completion_tokens` but includes `prompt_tokens`
    /// and nested cache fields, `cached_tokens` must still be extracted.
    /// Because `pt=500, ct=0` and the frame has no `choices`, the raw Data
    /// frame is intercepted (anomalous usage) and only the Usage chunk is
    /// emitted.
    #[test]
    fn cached_tokens_extracted_when_completion_tokens_absent() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        // nvidia/glm-style chunk: has prompt_tokens and cached_tokens
        // but no completion_tokens field.
        let frame = b"data: {\"usage\":{\"prompt_tokens\":500,\"prompt_tokens_details\":{\"cached_tokens\":400}}}\n\n";
        process_sse_frame(frame, &tx);
        drop(tx);

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 1, "Usage only, Data intercepted");

        let usage = chunks
            .iter()
            .find_map(|c| match c {
                StreamChunk::Usage(u) => Some(u),
                _ => None,
            })
            .expect("must contain a Usage chunk");

        assert_eq!(usage.prompt_tokens, 500);
        assert_eq!(usage.completion_tokens, 0, "default to 0 when absent");
        assert_eq!(usage.total_tokens, 500, "prompt + 0 fallback");
        assert_eq!(usage.cached_tokens, Some(400));
    }

    /// When upstream omits `prompt_tokens` but includes `completion_tokens`,
    /// cached_tokens should still be extracted (symmetric test).
    #[test]
    fn cached_tokens_extracted_when_prompt_tokens_absent() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        let frame = b"data: {\"usage\":{\"completion_tokens\":30,\"prompt_tokens_details\":{\"cached_tokens\":200}}}\n\n";
        process_sse_frame(frame, &tx);
        drop(tx);

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 2, "Data + Usage");

        let usage = chunks
            .iter()
            .find_map(|c| match c {
                StreamChunk::Usage(u) => Some(u),
                _ => None,
            })
            .expect("must contain a Usage chunk");

        assert_eq!(usage.prompt_tokens, 0, "default to 0 when absent");
        assert_eq!(usage.completion_tokens, 30);
        assert_eq!(usage.total_tokens, 30, "0 + completion fallback");
        assert_eq!(usage.cached_tokens, Some(200));
    }

    /// An entirely-zero usage object (pt=0, ct=0) should NOT produce a
    /// Usage chunk — it's noise.
    #[test]
    fn all_zero_usage_is_skipped() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        let frame = b"data: {\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":0}}\n\n";
        process_sse_frame(frame, &tx);
        drop(tx);

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }

        // Should only have the Data chunk, no Usage chunk.
        assert_eq!(chunks.len(), 1, "only Data, no Usage for all-zero");
        assert!(matches!(chunks[0], StreamChunk::Data(_)));
    }

    /// `cache_creation_input_tokens` from `prompt_tokens_details` must
    /// be lifted into `Usage.cache_creation_tokens`.
    #[test]
    fn cache_creation_input_tokens_extracted() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        let frame = b"data: {\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"prompt_tokens_details\":{\"cached_tokens\":30,\"cache_creation_input_tokens\":70}}}\n\n";
        process_sse_frame(frame, &tx);
        drop(tx);

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }

        let usage = chunks
            .iter()
            .find_map(|c| match c {
                StreamChunk::Usage(u) => Some(u),
                _ => None,
            })
            .expect("must contain a Usage chunk");

        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 50);
        assert_eq!(usage.cached_tokens, Some(30));
        assert_eq!(
            usage.cache_creation_tokens,
            Some(70),
            "must be lifted from prompt_tokens_details.cache_creation_input_tokens"
        );
    }

    #[test]
    fn stream_usage_extracts_cache_aliases_and_fallback() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        // Scenario 1: prompt_cache_hit_tokens and top-level reasoning_tokens
        let frame1 = b"data: {\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"total_tokens\":150,\"prompt_cache_hit_tokens\":40,\"reasoning_tokens\":20}}\n\n";
        process_sse_frame(frame1, &tx);

        // Scenario 2: cached_tokens top-level
        let frame2 = b"data: {\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"total_tokens\":150,\"cached_tokens\":50}}\n\n";
        process_sse_frame(frame2, &tx);

        // Scenario 3: prompt_tokens_details: {}
        let frame3 = b"data: {\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"total_tokens\":150,\"prompt_tokens_details\":{}}}\n\n";
        process_sse_frame(frame3, &tx);

        // Scenario 4: no details, no cache fields
        let frame4 = b"data: {\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"total_tokens\":150}}\n\n";
        process_sse_frame(frame4, &tx);

        drop(tx);

        let mut usages = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            if let StreamChunk::Usage(u) = chunk {
                usages.push(u);
            }
        }
        assert_eq!(usages.len(), 4);

        assert_eq!(usages[0].cached_tokens, Some(40));
        assert_eq!(usages[0].reasoning_tokens, Some(20));

        assert_eq!(usages[1].cached_tokens, Some(50));

        assert_eq!(usages[2].cached_tokens, Some(0));

        assert_eq!(usages[3].cached_tokens, None);
    }

    /// When completion_tokens is missing/0 but total_tokens and reasoning_tokens
    /// are present, we should fallback and derive the correct values.
    #[test]
    fn test_fallback_completion_tokens_when_missing_or_zero() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        // Scenario 1: Deriving completion_tokens via total_tokens - prompt_tokens
        let frame = b"data: {\"usage\":{\"completion_tokens_details\":{\"reasoning_tokens\":137},\"prompt_tokens\":53228,\"prompt_tokens_details\":{\"cached_tokens\":50008},\"total_tokens\":53365}}\n\n";
        process_sse_frame(frame, &tx);

        // Scenario 2: Deriving completion_tokens via reasoning_tokens when total_tokens is absent
        let frame2 = b"data: {\"usage\":{\"completion_tokens_details\":{\"reasoning_tokens\":42},\"prompt_tokens\":100}}\n\n";
        process_sse_frame(frame2, &tx);

        // Scenario 3: Strategy 1 derives completion_tokens (10), but reasoning_tokens is larger (50), so Strategy 2 raises completion_tokens to 50
        let frame3 = b"data: {\"usage\":{\"completion_tokens_details\":{\"reasoning_tokens\":50},\"prompt_tokens\":100,\"total_tokens\":110}}\n\n";
        process_sse_frame(frame3, &tx);

        drop(tx);

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            if let StreamChunk::Usage(u) = chunk {
                chunks.push(u);
            }
        }
        assert_eq!(chunks.len(), 3);

        // Verify Scenario 1 (derived from total_tokens - prompt_tokens)
        let usage1 = &chunks[0];
        assert_eq!(usage1.prompt_tokens, 53228);
        assert_eq!(usage1.completion_tokens, 137);
        assert_eq!(usage1.total_tokens, 53365);
        assert_eq!(usage1.cached_tokens, Some(50008));
        assert_eq!(usage1.reasoning_tokens, Some(137));

        // Verify Scenario 2 (derived from reasoning_tokens)
        let usage2 = &chunks[1];
        assert_eq!(usage2.prompt_tokens, 100);
        assert_eq!(usage2.completion_tokens, 42);
        assert_eq!(usage2.total_tokens, 142);
        assert_eq!(usage2.reasoning_tokens, Some(42));

        // Verify Scenario 3 (Strategy 1 gives 10, Strategy 2 corrects to 50)
        let usage3 = &chunks[2];
        assert_eq!(usage3.prompt_tokens, 100);
        assert_eq!(usage3.completion_tokens, 50);
        assert_eq!(usage3.total_tokens, 110);
        assert_eq!(usage3.reasoning_tokens, Some(50));
    }

    // ── anomalous usage frame interception tests ────────────────

    /// A pure anomalous usage frame (`pt > 0`, `ct == 0`, no `choices`) must
    /// have its raw Data intercepted while the Usage chunk is still emitted for
    /// CallLog accounting.
    #[test]
    fn anomalous_pure_usage_frame_data_intercepted() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        let frame = b"data: {\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":0,\"total_tokens\":100}}\n\n";
        process_sse_frame(frame, &tx);
        drop(tx);

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 1, "Data intercepted, only Usage emitted");

        let usage = chunks
            .iter()
            .find_map(|c| match c {
                StreamChunk::Usage(u) => Some(u),
                _ => None,
            })
            .expect("must contain a Usage chunk");
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 0);
        assert_eq!(usage.total_tokens, 100);
    }

    /// A combined frame carrying `finish_reason: "stop"` alongside an anomalous
    /// usage (`pt > 0`, `ct == 0`) must NOT be intercepted — the client needs
    /// the stream-termination signal.
    #[test]
    fn combined_frame_with_finish_reason_not_intercepted() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        let frame = b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":0}}\n\n";
        process_sse_frame(frame, &tx);
        drop(tx);

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 2, "Data + Usage both forwarded");

        assert!(
            chunks.iter().any(|c| matches!(c, StreamChunk::Data(_))),
            "must contain a Data chunk"
        );
        let usage = chunks
            .iter()
            .find_map(|c| match c {
                StreamChunk::Usage(u) => Some(u),
                _ => None,
            })
            .expect("must contain a Usage chunk");
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 0);
    }

    /// A combined frame carrying non-empty `content` delta alongside an
    /// anomalous usage must NOT be intercepted — the client needs the text.
    #[test]
    fn combined_frame_with_content_delta_not_intercepted() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        let frame = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":0}}\n\n";
        process_sse_frame(frame, &tx);
        drop(tx);

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 2, "Data + Usage both forwarded");

        assert!(
            chunks.iter().any(|c| matches!(c, StreamChunk::Data(_))),
            "must contain a Data chunk"
        );
        let usage = chunks
            .iter()
            .find_map(|c| match c {
                StreamChunk::Usage(u) => Some(u),
                _ => None,
            })
            .expect("must contain a Usage chunk");
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 0);
    }

    /// An empty `choices: []` array is not a meaningful choice — the frame is
    /// still a pure anomalous usage frame and must be intercepted.
    #[test]
    fn empty_choices_array_treated_as_no_meaningful_choice() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        let frame =
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":0}}\n\n";
        process_sse_frame(frame, &tx);
        drop(tx);

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 1, "Data intercepted, only Usage emitted");

        let usage = chunks
            .iter()
            .find_map(|c| match c {
                StreamChunk::Usage(u) => Some(u),
                _ => None,
            })
            .expect("must contain a Usage chunk");
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 0);
    }

    /// A choice with an empty `delta: {}` and `finish_reason: null` carries no
    /// meaningful payload - the frame must still be intercepted.
    #[test]
    fn empty_delta_null_finish_treated_as_no_meaningful_choice() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        let frame = b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":null}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":0}}\n\n";
        process_sse_frame(frame, &tx);
        drop(tx);

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 1, "Data intercepted, only Usage emitted");

        let usage = chunks
            .iter()
            .find_map(|c| match c {
                StreamChunk::Usage(u) => Some(u),
                _ => None,
            })
            .expect("must contain a Usage chunk");
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.completion_tokens, 0);
    }

    /// A combined frame carrying non-empty `reasoning_content` delta alongside
    /// an anomalous usage must NOT be intercepted.
    #[test]
    fn combined_frame_with_reasoning_content_not_intercepted() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        let frame = b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking...\"}}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":0}}\n\n";
        process_sse_frame(frame, &tx);
        drop(tx);

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 2, "Data + Usage both forwarded");

        assert!(
            chunks.iter().any(|c| matches!(c, StreamChunk::Data(_))),
            "must contain a Data chunk"
        );
    }

    /// A combined frame carrying non-empty `tool_calls` delta alongside an
    /// anomalous usage must NOT be intercepted.
    #[test]
    fn combined_frame_with_tool_calls_not_intercepted() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        let frame = b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"foo\"}}]}}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":0}}\n\n";
        process_sse_frame(frame, &tx);
        drop(tx);

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 2, "Data + Usage both forwarded");

        assert!(
            chunks.iter().any(|c| matches!(c, StreamChunk::Data(_))),
            "must contain a Data chunk"
        );
    }

    #[test]
    fn apply_headers_overrides_existing_headers() {
        let client = reqwest::Client::new();
        let req = client.post("https://example.com");

        let extra = [("Authorization", "Bearer old-token")];

        let mut custom = HashMap::new();
        custom.insert("authorization".into(), "Bearer new-token".into());
        custom.insert("content-type".into(), "text/plain".into());

        let req = apply_headers(req, &extra, &custom).build().unwrap();
        let headers = req.headers();

        assert_eq!(headers.get("authorization").unwrap(), "Bearer new-token");
        assert_eq!(headers.get("content-type").unwrap(), "text/plain");
        // Verify only 1 header value per key exists (not multiple via append)
        assert_eq!(headers.get_all("authorization").iter().count(), 1);
        assert_eq!(headers.get_all("content-type").iter().count(), 1);
    }

    #[test]
    fn find_sse_boundary_finds_earliest_delimiter() {
        // \n\n appears at pos 10, \r\n\r\n appears at pos 20
        let buf = b"data: AAAA\n\ndata: BBBBBBBB\r\n\r\n";
        assert_eq!(find_sse_boundary(buf), Some(10));

        // \r\n\r\n appears at pos 10, \n\n appears at pos 24
        let buf2 = b"data: AAAA\r\n\r\ndata: BBBBBBBB\n\n";
        assert_eq!(find_sse_boundary(buf2), Some(10));
    }

    #[test]
    fn process_sse_frame_supports_flexible_spaces() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        // data: without space
        let frame1 = b"data:{\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n";
        process_sse_frame(frame1, &tx);

        // data:  with multiple spaces
        let frame2 = b"data:   {\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":10}}\n\n";
        process_sse_frame(frame2, &tx);

        drop(tx);

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            if let StreamChunk::Usage(u) = chunk {
                chunks.push(u);
            }
        }

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].prompt_tokens, 10);
        assert_eq!(chunks[1].prompt_tokens, 20);
    }

    #[test]
    fn flush_sse_buffer_flushes_un_delimited_trailing_frame() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();
        let mut buffer =
            b"data: {\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":20}}".to_vec();

        flush_sse_buffer(&mut buffer, &tx);
        drop(tx);

        assert!(buffer.is_empty());

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            if let StreamChunk::Usage(u) = chunk {
                chunks.push(u);
            }
        }

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].prompt_tokens, 100);
        assert_eq!(chunks[0].completion_tokens, 20);
    }

    // ── 200-delivered upstream error detection in SSE frames ────

    /// A data frame whose JSON has a top-level `error` object must
    /// be forwarded to the client (behavior-compatible) and surfaced as an
    /// in-stream error: the `Error` chunk is enqueued before the `Data`
    /// frame (so the dispatch loop records the error state before the data
    /// can be delivered to a disconnecting client), and
    /// `process_sse_frame` returns `true`.
    #[test]
    fn sse_frame_with_top_level_error_surfaces_in_stream_error() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        let frame =
            b"data: {\"error\":{\"message\":\"burst tokens limit\",\"code\":429},\"id\":\"x\"}\n\n";
        let saw_error = process_sse_frame(frame, &tx);
        drop(tx);

        assert!(
            saw_error,
            "an error object frame must signal an in-stream error"
        );

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 2, "Error + Data chunks");
        // The `Error` chunk is enqueued first so the dispatch unfold loop
        // records the analytics error state before the `Data` frame is
        // delivered to the client.
        match &chunks[0] {
            StreamChunk::Error(msg) => assert_eq!(msg, "burst tokens limit"),
            other => panic!("expected an Error chunk first, got {other:?}"),
        }
        assert!(
            matches!(&chunks[1], StreamChunk::Data(d) if d == b"data: {\"error\":{\"message\":\"burst tokens limit\",\"code\":429},\"id\":\"x\"}\n\n"),
            "the error frame must still be forwarded verbatim: {:?}",
            chunks[1]
        );
    }

    /// `"error": null` must be treated as absent — the frame is a normal
    /// payload forwarded verbatim with no error signal.
    #[test]
    fn sse_frame_with_null_error_is_not_an_error() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        let frame = b"data: {\"error\":null,\"choices\":[]}\n\n";
        let saw_error = process_sse_frame(frame, &tx);
        drop(tx);

        assert!(!saw_error, "null error must be treated as absent");
        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 1, "only the forwarded Data chunk");
        assert!(matches!(chunks[0], StreamChunk::Data(_)));
    }

    /// Normal content / usage frames are unaffected: forwarded with no error
    /// signal and `process_sse_frame` returns `false`.
    #[test]
    fn sse_normal_frames_unaffected_by_error_detection() {
        let (tx, mut rx) = mpsc::unbounded_channel::<StreamChunk>();

        // Content delta.
        let content = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
        let saw_content = process_sse_frame(content, &tx);
        assert!(!saw_content);
        // Usage frame.
        let usage = b"data: {\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}\n\n";
        let saw_usage = process_sse_frame(usage, &tx);
        assert!(!saw_usage);
        // Non-JSON keepalive.
        let keepalive = b"data: : keep-alive\n\n";
        let saw_keepalive = process_sse_frame(keepalive, &tx);
        assert!(!saw_keepalive);
        drop(tx);

        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }
        assert_eq!(chunks.len(), 4, "3 Data + 1 Usage");
        assert!(chunks.iter().any(|c| matches!(c, StreamChunk::Usage(_))));
    }

    /// `drain_sse_frames` returns `true` when an error frame is encountered
    /// mid-buffer and stops draining; `flush_sse_buffer` surfaces an error in
    /// a trailing (un-delimited) frame.
    #[test]
    fn drain_and_flush_propagate_200_error_frames() {
        let (tx, _rx) = mpsc::unbounded_channel::<StreamChunk>();

        // Error frame delimited mid-buffer.
        let mut buffer =
            b"data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\ndata: {\"error\":{\"message\":\"boom\"}}\n\n".to_vec();
        let saw_error = drain_sse_frames(&mut buffer, &tx);
        assert!(saw_error, "mid-buffer error frame must be detected");

        // Trailing un-delimited error frame.
        let mut trailing = b"data: {\"error\":{\"message\":\"boom\"}}".to_vec();
        let saw_trailing = flush_sse_buffer(&mut trailing, &tx);
        assert!(saw_trailing, "trailing error frame must be detected");
        assert!(trailing.is_empty());
    }
}
