use std::collections::HashMap;

use reqwest::RequestBuilder;
use sag_core::{StreamChunk, Usage};
use tokio::sync::mpsc;

/// Apply custom headers to a reqwest request builder.
/// Custom headers override any previously set headers since they are
/// applied last.
pub fn apply_headers(req: RequestBuilder, headers: &HashMap<String, String>) -> RequestBuilder {
    let mut req = req;
    for (key, value) in headers {
        req = req.header(key.as_str(), value.as_str());
    }
    req
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

// ── SSE stream parsing helpers ─────────────────────────────────────

/// Iterate the byte buffer, extract complete SSE frames (delimited by `\n\n`),
/// parse them, and send the resulting `StreamChunk` items through the channel.
pub fn drain_sse_frames(buffer: &mut Vec<u8>, tx: &mpsc::UnboundedSender<StreamChunk>) {
    while let Some(end) = find_sse_boundary(buffer) {
        let frame: Vec<u8> = buffer.drain(..end).collect();
        // Remove the delimiter (\n\n or \r\n\r\n)
        let delim_len = if buffer.len() >= 4 && buffer[..4] == [b'\r', b'\n', b'\r', b'\n'] {
            4
        } else {
            2
        };
        buffer.drain(..delim_len);
        process_sse_frame(&frame, tx);
    }
}

/// Search for the SSE frame delimiter `\n\n` (or `\r\n\r\n`).
pub(crate) fn find_sse_boundary(buf: &[u8]) -> Option<usize> {
    if buf.len() >= 4
        && let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n")
    {
        return Some(pos);
    }
    buf.windows(2).position(|w| w == b"\n\n")
}

/// Parse a single SSE frame and send chunks through the channel.
fn process_sse_frame(frame: &[u8], tx: &mpsc::UnboundedSender<StreamChunk>) {
    let text = String::from_utf8_lossy(frame);

    for line in text.lines() {
        let line = line.trim();
        let Some(json_str) = line.strip_prefix("data: ") else {
            continue;
        };
        let json_str = json_str.trim();

        if json_str == "[DONE]" {
            let _ = tx.send(StreamChunk::Data(b"data: [DONE]\n\n".to_vec()));
            return;
        }

        // Forward the raw SSE data line
        let raw = format!("data: {json_str}\n\n").into_bytes();
        let _ = tx.send(StreamChunk::Data(raw));

        // Try to extract token usage from the JSON payload
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str)
            && let Some(usage_val) = val.get("usage")
            && !usage_val.is_null()
            && let (Some(pt), Some(ct)) = (
                usage_val.get("prompt_tokens").and_then(|v| v.as_u64()),
                usage_val.get("completion_tokens").and_then(|v| v.as_u64()),
            )
        {
            let _ = tx.send(StreamChunk::Usage(Usage {
                prompt_tokens: pt as u32,
                completion_tokens: ct as u32,
                total_tokens: pt as u32 + ct as u32,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }));
        }
    }
}
