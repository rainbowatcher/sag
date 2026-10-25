//! Streaming restore of placeholders in an SSE byte stream.
//!
//! The gateway scrubs a request before it reaches an upstream provider; the
//! provider then echoes the placeholders back inside its SSE response. A
//! [`RestoreWriter`] sits on the response body and rewrites them to the
//! original plaintext while the stream flows, so the client sees its own
//! data and never learns that a placeholder existed.
//!
//! Only `data:` frames are inspected, and only their JSON string values are
//! touched: JSON escaping is re-applied by the serializer, so a plaintext
//! containing quotes or newlines lands correctly escaped in the payload.
//! Everything else (comments, `event:`/`id:` lines, non-JSON data such as
//! `data: [DONE]`) is forwarded byte-for-byte.
//!
//! # Known limitation
//!
//! A placeholder that is split across two SSE frames is not restored: each
//! frame is parsed on its own, so neither half is a JSON document containing
//! the whole placeholder. This is a cosmetic issue only — a placeholder is a
//! salted digest and contains no plaintext — so the client would at worst see
//! the opaque `[[KIND_h12]]` marker instead of its data.

use serde_json::Value;

use crate::placeholder::PlaceholderMap;

/// SSE frame separators. Frames are forwarded together with theirs.
///
/// The SSE grammar allows `\r\n`, `\n` or `\r` line endings, so a blank line
/// (the frame terminator) is `\r\n\r\n` or `\n\n`; both are recognized. A
/// bare-CR terminator (`\r\r`) is not, because no encoder in the request path
/// emits one. Each separator is matched as a whole, so a chunk boundary inside
/// one of them (`...\r\n\r` | `\n`) never splits a frame early.
const FRAME_SEPS: [&[u8]; 2] = [b"\n\n", b"\r\n\r\n"];

/// Prefix marking a data frame.
const DATA_PREFIX: &[u8] = b"data:";

/// Incremental placeholder restorer for an SSE byte stream.
pub struct RestoreWriter {
    map: PlaceholderMap,
    /// Bytes received but not yet forming a complete frame.
    buf: Vec<u8>,
}

impl RestoreWriter {
    /// Build a writer that restores the placeholders recorded in `map`.
    pub fn new(map: PlaceholderMap) -> Self {
        Self {
            map,
            buf: Vec::new(),
        }
    }

    /// Feed the next chunk of the stream, returning the bytes that are ready
    /// to be sent downstream.
    ///
    /// Every complete frame (up to and including its `\n\n` or `\r\n\r\n`
    /// separator) is emitted; a trailing partial frame stays buffered until
    /// [`Self::push`] or [`Self::finish`] completes it.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::with_capacity(chunk.len());
        let mut start = 0;
        while let Some((rel, sep_len)) = find_sep(&self.buf[start..]) {
            let end = start + rel + sep_len;
            let sep = &self.buf[start + rel..end];
            match self.restore_frame(&self.buf[start..end], sep) {
                Some(restored) => out.extend_from_slice(&restored),
                None => out.extend_from_slice(&self.buf[start..end]),
            }
            start = end;
        }
        if start > 0 {
            self.buf.drain(..start);
        }
        out
    }

    /// Flush the bytes left in the buffer, restoring nothing from them
    /// because they never formed a complete frame.
    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }

    /// Restore one complete frame, or `None` when it must be forwarded as-is
    /// (not a `data:` frame, not JSON, or no placeholder inside).
    ///
    /// `sep` is the frame separator `frame` ends with; the restored frame is
    /// re-terminated with the same one.
    fn restore_frame(&self, frame: &[u8], sep: &[u8]) -> Option<Vec<u8>> {
        if self.map.is_empty() {
            return None;
        }
        let (prefix, payload) = split_data_frame(frame)?;
        let text = std::str::from_utf8(payload).ok()?;
        let mut value: Value = serde_json::from_str(text).ok()?;
        if !replace_strings(&mut value, &self.map) {
            return None;
        }
        // Keys may be reordered by the round-trip; that is harmless for JSON
        // consumers and keeps this path allocation-simple.
        let json = serde_json::to_string(&value).ok()?;
        let mut restored = Vec::with_capacity(prefix.len() + json.len() + sep.len());
        restored.extend_from_slice(prefix);
        restored.extend_from_slice(json.as_bytes());
        restored.extend_from_slice(sep);
        Some(restored)
    }
}

/// Position and length of the earliest frame separator in `bytes`.
fn find_sep(bytes: &[u8]) -> Option<(usize, usize)> {
    FRAME_SEPS
        .iter()
        .filter_map(|sep| {
            bytes
                .windows(sep.len())
                .position(|window| window == *sep)
                .map(|pos| (pos, sep.len()))
        })
        .min_by_key(|(pos, _)| *pos)
}

/// Split a `data:` frame into the literal prefix (`data:` plus the optional
/// spaces/tabs after it) and the payload that follows.
fn split_data_frame(frame: &[u8]) -> Option<(&[u8], &[u8])> {
    let rest = frame.strip_prefix(DATA_PREFIX)?;
    let ws = rest
        .iter()
        .take_while(|byte| **byte == b' ' || **byte == b'\t')
        .count();
    let split = DATA_PREFIX.len() + ws;
    Some((&frame[..split], &frame[split..]))
}

/// Replace placeholders in every string of `value`, returning whether any hit
/// occurred.
fn replace_strings(value: &mut Value, map: &PlaceholderMap) -> bool {
    match value {
        Value::String(text) => {
            let (replaced, hits) = map.replace_in(text);
            if hits == 0 {
                return false;
            }
            *text = replaced;
            true
        }
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= replace_strings(item, map);
            }
            changed
        }
        Value::Object(fields) => {
            let mut changed = false;
            for field in fields.values_mut() {
                changed |= replace_strings(field, map);
            }
            changed
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placeholder::make;

    /// Build a complete SSE data frame from a JSON payload.
    fn data_frame(payload: &str) -> Vec<u8> {
        format!("data: {payload}\n\n").into_bytes()
    }

    /// Map holding one placeholder for `plaintext`.
    fn map_with(kind: &str, plaintext: &str) -> (PlaceholderMap, String) {
        let placeholder = make(kind, plaintext, "salt");
        let mut map = PlaceholderMap::default();
        map.insert(placeholder.clone(), plaintext.to_string());
        (map, placeholder)
    }

    /// Parse a restored `data:` frame back into a JSON value.
    fn parse_frame(frame: &[u8]) -> Value {
        let text = std::str::from_utf8(frame).expect("utf-8 frame");
        let payload = text
            .strip_prefix("data: ")
            .expect("data prefix")
            .strip_suffix("\n\n")
            .expect("frame separator");
        serde_json::from_str(payload).expect("json payload")
    }

    /// Build a complete SSE data frame terminated with `\r\n\r\n`.
    fn crlf_frame(payload: &str) -> Vec<u8> {
        format!("data: {payload}\r\n\r\n").into_bytes()
    }

    /// Parse a restored frame terminated by either supported separator.
    fn parse_frame_any(frame: &[u8]) -> Value {
        let text = std::str::from_utf8(frame).expect("utf-8 frame");
        let payload = text
            .strip_prefix("data: ")
            .expect("data prefix")
            .trim_end_matches(['\r', '\n']);
        serde_json::from_str(payload).expect("json payload")
    }

    #[test]
    fn restores_placeholder_in_nested_json_string_with_escaping() {
        let plaintext = "he said \"hi\"\nbye";
        let (map, placeholder) = map_with("note", plaintext);
        let payload = format!(r#"{{"choices":[{{"delta":{{"content":"{placeholder}"}}}}]}}"#);

        let mut writer = RestoreWriter::new(map);
        let out = writer.push(&data_frame(&payload));
        assert!(writer.finish().is_empty());

        let value = parse_frame(&out);
        assert_eq!(
            value["choices"][0]["delta"]["content"].as_str(),
            Some(plaintext)
        );
        // The plaintext is JSON-escaped in the raw bytes.
        let raw = std::str::from_utf8(&out).expect("utf-8");
        assert!(raw.contains(r#"he said \"hi\"\nbye"#));
    }

    #[test]
    fn restores_placeholders_in_arrays_and_objects() {
        let (map, placeholder) = map_with("email", "alice@example.com");
        let payload = format!(r#"["{placeholder}",{{"note":"{placeholder}!"}}]"#);

        let mut writer = RestoreWriter::new(map);
        let out = writer.push(&data_frame(&payload));
        let value = parse_frame(&out);
        assert_eq!(value[0].as_str(), Some("alice@example.com"));
        assert_eq!(value[1]["note"].as_str(), Some("alice@example.com!"));
    }

    #[test]
    fn handles_one_frame_per_push() {
        let (map, placeholder) = map_with("email", "alice@example.com");
        let mut writer = RestoreWriter::new(map);

        let first = data_frame(&format!(r#"{{"content":"{placeholder}"}}"#));
        let second = data_frame(r#"{"content":"plain"}"#);

        let out1 = writer.push(&first);
        assert_eq!(
            parse_frame(&out1)["content"].as_str(),
            Some("alice@example.com")
        );
        let out2 = writer.push(&second);
        // No placeholder: forwarded byte-for-byte.
        assert_eq!(out2, second);
        assert!(writer.finish().is_empty());
    }

    #[test]
    fn handles_multiple_frames_in_one_push() {
        let (map, placeholder) = map_with("email", "alice@example.com");
        let mut writer = RestoreWriter::new(map);

        let mut chunk = data_frame(&format!(r#"{{"n":1,"content":"{placeholder}"}}"#));
        chunk.extend_from_slice(&data_frame(r#"{"n":2,"content":"plain"}"#));
        chunk.extend_from_slice(&data_frame(&format!(
            r#"{{"n":3,"content":"{placeholder}"}}"#
        )));

        let out = writer.push(&chunk);
        let text = std::str::from_utf8(&out).expect("utf-8");
        let frames: Vec<&str> = text.split("\n\n").filter(|f| !f.is_empty()).collect();
        assert_eq!(frames.len(), 3);
        let restored: Vec<Value> = frames
            .iter()
            .map(|frame| serde_json::from_str(frame.strip_prefix("data: ").unwrap()).unwrap())
            .collect();
        assert_eq!(restored[0]["content"].as_str(), Some("alice@example.com"));
        assert_eq!(restored[1]["content"].as_str(), Some("plain"));
        assert_eq!(restored[2]["content"].as_str(), Some("alice@example.com"));
    }

    #[test]
    fn frame_split_mid_payload_waits_for_the_rest() {
        let (map, placeholder) = map_with("email", "alice@example.com");
        let mut writer = RestoreWriter::new(map);

        let frame = data_frame(&format!(r#"{{"content":"{placeholder}"}}"#));
        let (head, tail) = frame.split_at(frame.len() / 2);

        // Incomplete frame: nothing is emitted yet.
        assert!(writer.push(head).is_empty());
        // Completing it restores the whole frame.
        let out = writer.push(tail);
        assert_eq!(
            parse_frame(&out)["content"].as_str(),
            Some("alice@example.com")
        );
        assert!(writer.finish().is_empty());
    }

    #[test]
    fn frame_split_right_after_the_separator_emits_immediately() {
        let (map, placeholder) = map_with("email", "alice@example.com");
        let mut writer = RestoreWriter::new(map);

        let frame = data_frame(&format!(r#"{{"content":"{placeholder}"}}"#));
        let out = writer.push(&frame);
        assert_eq!(
            parse_frame(&out)["content"].as_str(),
            Some("alice@example.com")
        );
        // A following empty chunk yields nothing.
        assert!(writer.push(b"").is_empty());
        assert!(writer.finish().is_empty());
    }

    #[test]
    fn finish_flushes_trailing_partial_frame() {
        let (map, placeholder) = map_with("email", "alice@example.com");
        let mut writer = RestoreWriter::new(map);

        let partial = format!(r#"data: {{"content":"{placeholder}"}}"#).into_bytes();
        assert!(writer.push(&partial).is_empty());
        assert_eq!(writer.finish(), partial);
        // Buffer is drained after finish.
        assert!(writer.finish().is_empty());
    }

    #[test]
    fn non_json_data_frames_pass_through() {
        let (map, _) = map_with("email", "alice@example.com");
        let mut writer = RestoreWriter::new(map);

        for frame in [data_frame("[DONE]"), data_frame("not json"), data_frame("")] {
            let out = writer.push(&frame);
            assert_eq!(out, frame);
        }
        assert!(writer.finish().is_empty());
    }

    #[test]
    fn non_data_frames_pass_through() {
        let (map, placeholder) = map_with("email", "alice@example.com");
        let mut writer = RestoreWriter::new(map);

        for frame in [
            b": keep-alive\n\n".to_vec(),
            b"event: ping\n\n".to_vec(),
            b"id: 7\n\n".to_vec(),
            format!("{{\"content\":\"{placeholder}\"}}\n\n").into_bytes(),
        ] {
            let out = writer.push(&frame);
            assert_eq!(out, frame);
        }
        assert!(writer.finish().is_empty());
    }

    #[test]
    fn frame_without_hits_is_forwarded_byte_for_byte() {
        let (map, _) = map_with("email", "alice@example.com");
        let mut writer = RestoreWriter::new(map);

        // Spacing that the JSON round-trip would not preserve.
        let frame = data_frame(r#"{ "content" : "plain" }"#);
        let out = writer.push(&frame);
        assert_eq!(out, frame);
    }

    #[test]
    fn empty_map_forwards_everything() {
        let mut writer = RestoreWriter::new(PlaceholderMap::default());
        let frame = data_frame(r#"{"content":"[[EMAIL_0123456789ab]]"}"#);
        let out = writer.push(&frame);
        assert_eq!(out, frame);
        assert!(writer.finish().is_empty());
    }

    #[test]
    fn placeholder_split_across_frames_is_not_restored() {
        // Documented limitation: neither half is a JSON document containing
        // the whole placeholder, so both frames pass through untouched.
        let (map, placeholder) = map_with("email", "alice@example.com");
        let mut writer = RestoreWriter::new(map);

        let first = data_frame(&format!(r#"{{"content":"{}"#, &placeholder[..6]));
        let second = data_frame(&format!("{}\"}}", &placeholder[6..]));

        assert_eq!(writer.push(&first), first);
        assert_eq!(writer.push(&second), second);
        assert!(writer.finish().is_empty());
    }

    #[test]
    fn restores_frames_separated_by_crlf() {
        let (map, placeholder) = map_with("email", "alice@example.com");
        let mut writer = RestoreWriter::new(map);

        let frame = crlf_frame(&format!(r#"{{"content":"{placeholder}"}}"#));
        let out = writer.push(&frame);
        assert_eq!(
            parse_frame_any(&out)["content"].as_str(),
            Some("alice@example.com")
        );
        // The frame keeps its own CRLF terminator.
        assert!(out.ends_with(b"\r\n\r\n"), "out: {out:?}");
        assert!(writer.finish().is_empty());

        // A CRLF frame without a placeholder is forwarded byte for byte.
        let plain = crlf_frame(r#"{"content":"plain"}"#);
        assert_eq!(writer.push(&plain), plain);
    }

    #[test]
    fn restores_mixed_lf_and_crlf_frames() {
        let (map, placeholder) = map_with("email", "alice@example.com");
        let mut writer = RestoreWriter::new(map);

        let mut chunk = data_frame(&format!(r#"{{"n":1,"content":"{placeholder}"}}"#));
        chunk.extend_from_slice(&crlf_frame(&format!(
            r#"{{"n":2,"content":"{placeholder}"}}"#
        )));
        chunk.extend_from_slice(&data_frame(&format!(
            r#"{{"n":3,"content":"{placeholder}"}}"#
        )));

        let out = writer.push(&chunk);
        let text = std::str::from_utf8(&out).expect("utf-8");
        // Split on both separators: three frames came back, each restored.
        let frames: Vec<&str> = text
            .split("\r\n\r\n")
            .flat_map(|part| part.split("\n\n"))
            .filter(|frame| !frame.is_empty())
            .collect();
        assert_eq!(frames.len(), 3, "frames: {frames:?}");
        for (i, frame) in frames.iter().enumerate() {
            let value: Value = serde_json::from_str(
                frame
                    .strip_prefix("data: ")
                    .unwrap_or_else(|| panic!("frame {i}: {frame}")),
            )
            .expect("json payload");
            assert_eq!(value["content"].as_str(), Some("alice@example.com"));
        }
        assert!(writer.finish().is_empty());
    }

    #[test]
    fn separator_split_across_chunks_waits_for_the_rest() {
        let (map, placeholder) = map_with("email", "alice@example.com");
        let mut writer = RestoreWriter::new(map);

        let payload = format!(r#"{{"content":"{placeholder}"}}"#);

        // `\r\n\r` | `\n`: the half separator must not terminate the frame.
        let mut head = format!("data: {payload}\r\n\r").into_bytes();
        assert!(writer.push(&head).is_empty());
        let out = writer.push(b"\n");
        assert_eq!(
            parse_frame_any(&out)["content"].as_str(),
            Some("alice@example.com")
        );
        assert!(out.ends_with(b"\r\n\r\n"), "out: {out:?}");

        // `\r\n` | `\r\n` is the same separator, still no early split.
        head = format!("data: {payload}\r\n").into_bytes();
        assert!(writer.push(&head).is_empty());
        let out = writer.push(b"\r\n");
        assert_eq!(
            parse_frame_any(&out)["content"].as_str(),
            Some("alice@example.com")
        );

        // `\n` | `\n` behaves the same way.
        head = format!("data: {payload}\n").into_bytes();
        assert!(writer.push(&head).is_empty());
        let out = writer.push(b"\n");
        assert_eq!(
            parse_frame(&out)["content"].as_str(),
            Some("alice@example.com")
        );
        assert!(writer.finish().is_empty());
    }

    #[test]
    fn data_frame_without_space_after_colon_is_restored() {
        let (map, placeholder) = map_with("email", "alice@example.com");
        let mut writer = RestoreWriter::new(map);

        let frame = format!("data:{{\"content\":\"{placeholder}\"}}\n\n").into_bytes();
        let out = writer.push(&frame);
        let text = std::str::from_utf8(&out).expect("utf-8");
        assert!(text.starts_with("data:{"));
        assert!(text.ends_with("\n\n"));
        let payload = text.strip_prefix("data:").unwrap().trim_end_matches('\n');
        let value: Value = serde_json::from_str(payload).expect("json payload");
        assert_eq!(value["content"].as_str(), Some("alice@example.com"));
    }
}
