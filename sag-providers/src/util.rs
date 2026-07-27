use std::collections::HashMap;

use reqwest::RequestBuilder;
use sag_config::ProviderType;
use sag_core::{StreamChunk, Usage};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Return the (name, value) pairs for auth headers that a provider type uses
/// for chat and models API requests.
///
/// - `OpenAI` → `Authorization: Bearer {key}`
/// - `Anthropic` → `x-api-key: {key}` + `anthropic-version: 2023-06-01`
pub fn provider_auth_headers(
    provider_type: ProviderType,
    api_key: &str,
) -> Vec<(&'static str, String)> {
    match provider_type {
        ProviderType::OpenAI => vec![("Authorization", format!("Bearer {api_key}"))],
        ProviderType::Anthropic => vec![
            ("x-api-key", api_key.to_string()),
            ("anthropic-version", "2023-06-01".to_string()),
        ],
    }
}

/// Return the canonical chat API endpoint path for a provider type.
///
/// - `OpenAI`   → `/chat/completions`
/// - `Anthropic` → `/messages`
pub fn provider_chat_endpoint(provider_type: ProviderType) -> &'static str {
    match provider_type {
        ProviderType::OpenAI => "/chat/completions",
        ProviderType::Anthropic => "/messages",
    }
}

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

/// Iterate the byte buffer, extract complete SSE frames (delimited by `\n\n`),
/// parse them, and send the resulting `StreamChunk` items through the channel.
pub fn drain_sse_frames(buffer: &mut Vec<u8>, tx: &mpsc::UnboundedSender<StreamChunk>) {
    while let Some(end) = find_sse_boundary(buffer) {
        let frame: Vec<u8> = buffer.drain(..end).collect();
        // Remove the delimiter (\n\n or \r\n\r\n)
        let delim_len = if buffer.len() >= 4 && buffer[..4] == *b"\r\n\r\n" {
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

        // Try to extract token usage from the JSON payload.
        //
        // Each token count field is extracted independently with a
        // default of 0 so that cache/reasoning metadata is still
        // captured even when upstream omits one of the counts
        // (e.g. nvidia/glm sometimes omits completion_tokens from
        // intermediate usage chunks).
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str)
            && let Some(usage_val) = val.get("usage")
            && !usage_val.is_null()
        {
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
            let cached_tokens = usage_val
                .get("prompt_tokens_details")
                .and_then(|v| v.get("cached_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let cache_creation_tokens = usage_val
                .get("prompt_tokens_details")
                .and_then(|v| v.get("cache_creation_input_tokens"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let reasoning_tokens = usage_val
                .get("completion_tokens_details")
                .and_then(|v| v.get("reasoning_tokens"))
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

            // Diagnostic for "Completion=0" in call logs. A frame that
            // credits prompt tokens but zero completion tokens is always
            // anomalous for an OpenAI-compatible stream and is exactly what
            // leaves the call log with completion_tokens=0 on an otherwise
            // successful (status 200) request. WARN so it surfaces under the
            // default `info` filter; the raw `usage` object is included to
            // reveal the upstream field layout (explicit 0 vs. absent field,
            // top-level reasoning_tokens, etc.) for root-causing.
            if ct == 0 && pt > 0 {
                warn!(
                    pt = pt,
                    ct = ct,
                    usage = %usage_val,
                    "upstream usage frame credits zero completion_tokens despite non-zero prompt; \
                     this is the source of Completion=0 in the call log"
                );
            }

            // Skip entirely-zero usage objects to avoid noise; partial
            // data (e.g. prompt_tokens present but completion_tokens
            // absent) is still valuable for cache/reasoning extraction.
            if pt == 0 && ct == 0 {
                debug!(usage = %usage_val, "skipping all-zero usage frame");
                continue;
            }

            let total = total_from_upstream.unwrap_or(pt + ct);
            let _ = tx.send(StreamChunk::Usage(Usage {
                prompt_tokens: pt,
                completion_tokens: ct,
                total_tokens: total,
                cached_tokens,
                cache_creation_tokens,
                reasoning_tokens,
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// When upstream omits `completion_tokens` but includes
    /// `prompt_tokens` and nested cache fields, `cached_tokens` must
    /// still be extracted and `completion_tokens` defaults to 0.
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
        assert_eq!(chunks.len(), 2, "Data + Usage");

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
}
