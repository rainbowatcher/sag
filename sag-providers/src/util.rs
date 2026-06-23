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
            let total = usage_val
                .get("total_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(pt + ct) as u32;
            // OpenAI nests cache/reasoning tokens inside details objects.
            let cached_tokens = usage_val
                .get("prompt_tokens_details")
                .and_then(|v| v.get("cached_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let reasoning_tokens = usage_val
                .get("completion_tokens_details")
                .and_then(|v| v.get("reasoning_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let _ = tx.send(StreamChunk::Usage(Usage {
                prompt_tokens: pt as u32,
                completion_tokens: ct as u32,
                total_tokens: total,
                cached_tokens,
                cache_creation_tokens: None,
                reasoning_tokens,
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "OpenAI does not produce cache_creation_tokens"
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
}
