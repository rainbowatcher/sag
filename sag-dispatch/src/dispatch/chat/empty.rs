//! Empty-response detection and first-chunk validation for chat completions.

use futures_util::StreamExt;
use sag_core::{AppError, ChatCompletionResponse, IrOutputItem, IrResponse, IrStreamEvent};

/// Quick byte search for `b"usage"` in a raw SSE data frame. Used as a
/// cheap pre-filter to avoid JSON parsing on content-only frames.
pub(crate) fn raw_contains_usage(raw: &[u8]) -> bool {
    raw.windows(5).any(|w| w == b"usage")
}

/// Check whether a raw `StreamChunk::Data` payload is an "empty response"
/// frame: a JSON object whose `usage` reports `prompt_tokens` > 0, and
/// whose `choices` carry no actual content (no non-empty `content` /
/// `reasoning_content` / `refusal` / `tool_calls`).
///
/// `completion_tokens > 0` does not by itself prove that the response is
/// non-empty because some providers report usage inaccurately. Choices with
/// real content are the only signal that counts here. In particular,
/// `finish_reason` alone is not content: a stop with an empty delta is still
/// an empty response. This intentionally differs from
/// `process_sse_frame`'s `has_meaningful_choice`, which counts `finish_reason`
/// to preserve the stream-termination signal.
pub(crate) fn is_empty_response_data(raw: &[u8]) -> bool {
    if !raw_contains_usage(raw) {
        return false;
    }

    let text = std::str::from_utf8(raw).unwrap_or("");
    let Some(rest) = text
        .strip_prefix("data:")
        .or_else(|| text.strip_prefix("data: "))
    else {
        return false;
    };
    let json_str = rest.trim();

    let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) else {
        return false;
    };

    let Some(usage) = val.get("usage") else {
        return false;
    };

    let pt = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    if pt == 0 {
        return false;
    }

    // Check if any choice carries real content. finish_reason alone does
    // NOT count -- a "stop" with an empty delta is still an empty response.
    let has_content = val
        .get("choices")
        .and_then(|c| c.as_array())
        .is_some_and(|arr| {
            arr.iter().any(|choice| {
                choice.get("delta").is_some_and(|d| {
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
                })
            })
        });

    !has_content
}

/// Check whether a `ChatMessage` carries actual content, including
/// tool_calls in the `extra` map.
/// Kept alongside [`is_empty_ir_response`] for Chat-schema fixtures / tests.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn choice_has_message_content(msg: &sag_core::types::ChatMessage) -> bool {
    // Check content field
    match &msg.content {
        serde_json::Value::String(s) if !s.is_empty() => return true,
        serde_json::Value::Array(arr) if !arr.is_empty() => return true,
        _ => {}
    }
    // Check for tool_calls in the extra map
    if let Some(tc) = msg.extra.get("tool_calls")
        && tc.as_array().is_some_and(|a| !a.is_empty())
    {
        return true;
    }
    false
}

/// Check whether a non-streaming response is "empty" — i.e. has non-zero
/// `prompt_tokens` but no actual content in any `choice`. Choices with
/// real content (string/array message content, or tool_calls) are
/// considered non-empty even if `completion_tokens` is 0, because some
/// providers report ct inaccurately.
/// Dispatch now prefers [`is_empty_ir_response`]; this Chat-schema form
/// remains for fixtures / alignment tests.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn is_empty_non_stream_response(r: &ChatCompletionResponse) -> bool {
    let Some(u) = r.usage.as_ref() else {
        return false;
    };
    if u.prompt_tokens == 0 {
        return false;
    }
    let has_content = r
        .choices
        .iter()
        .any(|c| choice_has_message_content(&c.message));
    if has_content {
        return false;
    }
    true
}

/// Whether a single IR output item carries real content.
///
/// `Raw` counts as content when its payload is non-null, a non-empty
/// object, or a non-empty array — aligned with the legacy "non-empty
/// content array" semantics (`decode_response_content` pushes non-text
/// content parts as `Raw`).
pub(crate) fn ir_item_has_content(item: &IrOutputItem) -> bool {
    match item {
        IrOutputItem::Text { text } => !text.is_empty(),
        IrOutputItem::Reasoning { text } => !text.is_empty(),
        IrOutputItem::ToolCall(_) => true,
        IrOutputItem::Raw(v) => match v {
            serde_json::Value::Null => false,
            serde_json::Value::Array(arr) => !arr.is_empty(),
            serde_json::Value::Object(obj) => !obj.is_empty(),
            serde_json::Value::String(s) => !s.is_empty(),
            // numbers / booleans are always payloads
            _ => true,
        },
    }
}

/// Whether any preserved multi-choice entry in `raw["choices"]` carries
/// message content. `ChatCompletionsCodec::decode_response` only folds
/// `choice[0]` into `output`; `choice[i>0]` is stashed verbatim in
/// `raw["choices"]` (and mirrored into `loss`), so emptiness must look
/// there too — otherwise a multi-choice response whose *later* choice has
/// content would be misclassified as empty.
fn raw_choices_have_content(r: &IrResponse) -> bool {
    fn content_is_nonempty(c: &serde_json::Value) -> bool {
        match c {
            serde_json::Value::String(s) => !s.is_empty(),
            serde_json::Value::Array(a) => !a.is_empty(),
            serde_json::Value::Null => false,
            _ => true,
        }
    }

    r.raw
        .get("choices")
        .and_then(|c| c.as_array())
        .is_some_and(|arr| {
            arr.iter().any(|choice| {
                choice
                    .get("message")
                    .and_then(|m| m.get("content"))
                    .is_some_and(content_is_nonempty)
                    || choice
                        .get("message")
                        .and_then(|m| m.get("tool_calls"))
                        .and_then(|t| t.as_array())
                        .is_some_and(|a| !a.is_empty())
            })
        })
}

/// IR-aware empty non-stream response: no content-bearing Text / Reasoning /
/// ToolCall / Raw output items **and** no content in preserved
/// `raw["choices"]`, while `usage.prompt_tokens > 0`. Scans **every**
/// output item (not just `[0]`), so a multi-output response where only a
/// later item carries content is non-empty. Aligned with the choices
/// semantics of [`is_empty_non_stream_response`] (content-bearing output =
/// non-empty; usage without content = empty; missing usage / pt==0 = not
/// empty).
pub(crate) fn is_empty_ir_response(r: &IrResponse) -> bool {
    let Some(u) = r.usage.as_ref() else {
        return false;
    };
    if u.prompt_tokens == 0 {
        return false;
    }
    !(r.output.iter().any(ir_item_has_content) || raw_choices_have_content(r))
}

/// Peek at the first meaningful event of a streaming response. If the upstream
/// returned an empty response (zero `completion_tokens`, no content),
/// return [`AppError::EmptyResponse`] so the retry loop retries the same
/// target with backoff, falling through to the next target only when the
/// retry budget is exhausted. Otherwise, prepend the consumed events and
/// return the stream for normal forwarding — **the original events are
/// unshifted unchanged** (raw bytes are never rewritten on peek).
///
/// A structured [`IrStreamEvent::Start`] that carries no metadata (id /
/// created / model all `None`) is not itself a content signal: keep peeking
/// past it (the empty Start is still unshifted on success). Metadata-bearing
/// Starts, TextDelta / ToolCallStart / ReasoningDelta, and other structured
/// events are content and pass immediately.
pub(crate) async fn validate_stream_first_chunk(
    mut stream: futures_util::stream::BoxStream<'static, IrStreamEvent>,
    provider: &str,
) -> Result<futures_util::stream::BoxStream<'static, IrStreamEvent>, AppError> {
    // Events consumed before the first meaningful one (empty Starts) —
    // unshifted in order on success so nothing is dropped.
    let mut pending: Vec<IrStreamEvent> = Vec::new();
    loop {
        let ev = stream.next().await;
        match ev {
            // Empty stream or immediate End -> no output at all.
            None | Some(IrStreamEvent::End) => {
                return Err(AppError::EmptyResponse {
                    provider: provider.into(),
                });
            }

            // Mid-stream error from the provider -> propagate as retryable.
            Some(IrStreamEvent::Error(msg)) => {
                return Err(AppError::Upstream {
                    provider: provider.into(),
                    status: 502,
                    body: msg,
                });
            }

            // Pure Usage event (Data was intercepted by process_sse_frame).
            // If ct == 0 and pt > 0, the response is empty.
            Some(IrStreamEvent::Usage(u)) => {
                // NOTE: A `Usage` event carries only token counts, not `choices`, so we
                // cannot apply the same choices-based emptiness check as `is_empty_response_data`.
                // We fall back to the conservative ct==0 && pt>0 signal here. A Raw frame
                // carrying usage + choices is checked more precisely in the Raw branch below.
                if u.completion_tokens == 0 && u.prompt_tokens > 0 {
                    return Err(AppError::EmptyResponse {
                        provider: provider.into(),
                    });
                }
                pending.push(IrStreamEvent::Usage(u));
                break;
            }

            // Raw SSE frame — check for the [DONE] marker or an empty usage frame.
            Some(IrStreamEvent::Raw(raw)) => {
                if raw.starts_with(b"data: [DONE]") || is_empty_response_data(&raw) {
                    return Err(AppError::EmptyResponse {
                        provider: provider.into(),
                    });
                }
                pending.push(IrStreamEvent::Raw(raw));
                break;
            }

            // Metadata-free Start is not content — keep peeking (unshift later).
            Some(ref ev @ IrStreamEvent::Start(ref meta))
                if meta.id.is_none() && meta.created.is_none() && meta.model.is_none() =>
            {
                pending.push(ev.clone());
            }

            // Structured events (TextDelta / ToolCallStart / ReasoningDelta /
            // Start with id, …) carry real content signals — non-empty; unshift
            // the original event unchanged.
            Some(ev) => {
                pending.push(ev);
                break;
            }
        }
    }
    Ok(futures_util::stream::iter(pending).chain(stream).boxed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sag_core::types::{ChatChoice, ChatMessage, Usage};

    fn build_response(model: &str, usage: Option<Usage>) -> ChatCompletionResponse {
        ChatCompletionResponse {
            id: "resp-1".into(),
            object: "chat.completion".into(),
            created: 0,
            model: model.into(),
            choices: vec![],
            usage,
        }
    }

    #[test]
    fn is_empty_response_data_detects_ct0_with_finish_reason() {
        let raw = b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"total_tokens\":100}}\n\n";
        assert!(
            is_empty_response_data(raw),
            "frame with ct=0 and finish_reason but no content is empty"
        );
    }

    #[test]
    fn is_empty_response_data_detects_ct0_without_choices() {
        let raw = b"data: {\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":0,\"total_tokens\":100}}\n\n";
        assert!(is_empty_response_data(raw));
    }

    #[test]
    fn is_empty_response_data_rejects_content_frame() {
        let raw = b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n";
        assert!(!is_empty_response_data(raw), "content frame is not empty");
    }

    #[test]
    fn is_empty_response_data_rejects_ct_positive_with_content() {
        let raw = b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"total_tokens\":150}}\n\n";
        assert!(
            !is_empty_response_data(raw),
            "ct>0 with content frame is not empty"
        );
    }

    #[test]
    fn is_empty_response_data_detects_ct_positive_no_content() {
        let raw = b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":50,\"total_tokens\":150}}\n\n";
        assert!(
            is_empty_response_data(raw),
            "ct>0 with empty choices is now empty"
        );
    }

    #[test]
    fn is_empty_response_data_rejects_pt_zero() {
        let raw = b"data: {\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":0}}\n\n";
        assert!(
            !is_empty_response_data(raw),
            "pt=0 is not an anomalous empty response"
        );
    }

    #[test]
    fn is_empty_response_data_rejects_non_json() {
        let raw = b"data: [DONE]\n\n";
        assert!(!is_empty_response_data(raw), "[DONE] is handled separately");
    }

    #[test]
    fn non_stream_empty_detects_ct0_no_content() {
        let r = ChatCompletionResponse {
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 0,
                total_tokens: 100,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            ..build_response("gpt-4o", None)
        };
        assert!(is_empty_non_stream_response(&r));
    }

    #[test]
    fn non_stream_empty_detects_ct_positive_no_content() {
        let r = ChatCompletionResponse {
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            ..build_response("gpt-4o", None)
        };
        assert!(
            is_empty_non_stream_response(&r),
            "ct>0 with empty choices should be detected as empty"
        );
    }

    #[test]
    fn non_stream_empty_rejects_string_content() {
        let r = ChatCompletionResponse {
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::String("hello".into()),
                    name: None,
                    extra: serde_json::Map::new(),
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 0,
                total_tokens: 100,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            ..build_response("gpt-4o", None)
        };
        assert!(
            !is_empty_non_stream_response(&r),
            "string content is not empty"
        );
    }

    #[test]
    fn non_stream_empty_rejects_array_content() {
        let r = ChatCompletionResponse {
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::Array(vec![serde_json::json!({
                        "type": "text",
                        "text": "hello"
                    })]),
                    name: None,
                    extra: serde_json::Map::new(),
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 0,
                total_tokens: 100,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            ..build_response("gpt-4o", None)
        };
        assert!(
            !is_empty_non_stream_response(&r),
            "array content is not empty"
        );
    }

    #[test]
    fn non_stream_empty_rejects_tool_calls() {
        let r = ChatCompletionResponse {
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::Null,
                    name: None,
                    extra: serde_json::Map::from_iter([(
                        "tool_calls".into(),
                        serde_json::json!([{"id": "call_1", "type": "function"}]),
                    )]),
                },
                finish_reason: Some("tool_calls".into()),
            }],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 0,
                total_tokens: 100,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            ..build_response("gpt-4o", None)
        };
        assert!(!is_empty_non_stream_response(&r), "tool_calls is not empty");
    }

    #[test]
    fn non_stream_empty_rejects_no_usage() {
        let r = ChatCompletionResponse {
            choices: vec![],
            usage: None,
            ..build_response("gpt-4o", None)
        };
        assert!(!is_empty_non_stream_response(&r));
    }

    #[test]
    fn non_stream_empty_rejects_pt_zero() {
        let r = ChatCompletionResponse {
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            ..build_response("gpt-4o", None)
        };
        assert!(!is_empty_non_stream_response(&r));
    }

    #[test]
    fn non_stream_empty_rejects_empty_string_content() {
        let r = ChatCompletionResponse {
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::String("".into()),
                    name: None,
                    extra: serde_json::Map::new(),
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 0,
                total_tokens: 100,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            ..build_response("gpt-4o", None)
        };
        assert!(
            is_empty_non_stream_response(&r),
            "empty string is not content"
        );
    }

    #[test]
    fn non_stream_empty_rejects_null_content() {
        let r = ChatCompletionResponse {
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::Null,
                    name: None,
                    extra: serde_json::Map::new(),
                },
                finish_reason: Some("stop".into()),
            }],
            usage: Some(Usage {
                prompt_tokens: 100,
                completion_tokens: 0,
                total_tokens: 100,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            ..build_response("gpt-4o", None)
        };
        assert!(is_empty_non_stream_response(&r), "null is not content");
    }

    #[tokio::test]
    async fn validate_stream_empty_data_with_usage_ct0_returns_error() {
        let stream = futures_util::stream::iter(vec![IrStreamEvent::Raw(
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"total_tokens\":100}}\n\n".to_vec(),
        )]);
        let result = validate_stream_first_chunk(stream.boxed(), "cpa").await;
        assert!(matches!(result, Err(AppError::EmptyResponse { .. })));
    }

    #[tokio::test]
    async fn validate_stream_content_data_passes_through() {
        let stream = futures_util::stream::iter(vec![
            IrStreamEvent::Raw(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n".to_vec(),
            ),
            IrStreamEvent::End,
        ]);
        let mut s = validate_stream_first_chunk(stream.boxed(), "cpa")
            .await
            .expect("content frame should pass through");
        let first = s.next().await.unwrap();
        assert!(matches!(first, IrStreamEvent::Raw(_)));
        let second = s.next().await.unwrap();
        assert!(matches!(second, IrStreamEvent::End));
    }

    #[tokio::test]
    async fn validate_stream_done_marker_returns_error() {
        let stream =
            futures_util::stream::iter(vec![IrStreamEvent::Raw(b"data: [DONE]\n\n".to_vec())]);
        let result = validate_stream_first_chunk(stream.boxed(), "cpa").await;
        assert!(matches!(result, Err(AppError::EmptyResponse { .. })));
    }

    #[tokio::test]
    async fn validate_stream_usage_ct0_returns_error() {
        let stream = futures_util::stream::iter(vec![IrStreamEvent::Usage(Usage {
            prompt_tokens: 100,
            completion_tokens: 0,
            total_tokens: 100,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        })]);
        let result = validate_stream_first_chunk(stream.boxed(), "cpa").await;
        assert!(matches!(result, Err(AppError::EmptyResponse { .. })));
    }

    #[tokio::test]
    async fn validate_stream_usage_ct_positive_passes_through() {
        let stream = futures_util::stream::iter(vec![IrStreamEvent::Usage(Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        })]);
        let mut s = validate_stream_first_chunk(stream.boxed(), "cpa")
            .await
            .expect("ct>0 should pass through");
        assert!(matches!(s.next().await, Some(IrStreamEvent::Usage(_))));
    }

    #[tokio::test]
    async fn validate_stream_empty_stream_returns_error() {
        let stream = futures_util::stream::empty::<IrStreamEvent>();
        let result = validate_stream_first_chunk(stream.boxed(), "cpa").await;
        assert!(matches!(result, Err(AppError::EmptyResponse { .. })));
    }

    #[tokio::test]
    async fn validate_stream_end_returns_empty() {
        let stream = futures_util::stream::iter(vec![IrStreamEvent::End]);
        let result = validate_stream_first_chunk(stream.boxed(), "cpa").await;
        assert!(matches!(result, Err(AppError::EmptyResponse { .. })));
    }

    #[tokio::test]
    async fn validate_stream_error_returns_upstream_502() {
        let stream = futures_util::stream::iter(vec![IrStreamEvent::Error("boom".into())]);
        let result = validate_stream_first_chunk(stream.boxed(), "cpa").await;
        match result {
            Err(AppError::Upstream { status, body, .. }) => {
                assert_eq!(status, 502);
                assert_eq!(body, "boom");
            }
            _ => panic!("expected Upstream 502"),
        }
    }

    #[tokio::test]
    async fn validate_stream_structured_text_delta_unshifts_original() {
        let ev = IrStreamEvent::TextDelta {
            index: 0,
            text: "hi".into(),
        };
        let stream = futures_util::stream::iter(vec![ev.clone(), IrStreamEvent::End]);
        let mut s = validate_stream_first_chunk(stream.boxed(), "cpa")
            .await
            .expect("structured event is non-empty");
        assert_eq!(s.next().await.unwrap(), ev);
        assert!(matches!(s.next().await, Some(IrStreamEvent::End)));
    }

    #[tokio::test]
    async fn validate_stream_structured_tool_call_start_unshifts_original() {
        let ev = IrStreamEvent::ToolCallStart {
            index: 0,
            id: Some("call_1".into()),
            name: Some("fn".into()),
        };
        let stream = futures_util::stream::iter(vec![ev.clone()]);
        let mut s = validate_stream_first_chunk(stream.boxed(), "cpa")
            .await
            .expect("tool call start is non-empty");
        assert_eq!(s.next().await.unwrap(), ev);
    }

    #[tokio::test]
    async fn validate_stream_structured_reasoning_delta_unshifts_original() {
        let ev = IrStreamEvent::ReasoningDelta {
            index: 0,
            text: "think".into(),
        };
        let stream = futures_util::stream::iter(vec![ev.clone()]);
        let mut s = validate_stream_first_chunk(stream.boxed(), "cpa")
            .await
            .expect("reasoning delta is non-empty");
        assert_eq!(s.next().await.unwrap(), ev);
    }

    #[tokio::test]
    async fn validate_stream_start_with_id_unshifts_original() {
        let ev = IrStreamEvent::Start(sag_core::IrResponseMeta {
            id: Some("chatcmpl-1".into()),
            created: Some(1),
            model: Some("gpt-4o".into()),
        });
        let stream = futures_util::stream::iter(vec![ev.clone()]);
        let mut s = validate_stream_first_chunk(stream.boxed(), "cpa")
            .await
            .expect("start with id is non-empty");
        assert_eq!(s.next().await.unwrap(), ev);
    }

    #[tokio::test]
    async fn validate_stream_raw_bytes_unshifted_unchanged() {
        let raw = b"data: {\"choices\":[{\"delta\":{\"content\":\"exact\"}}]}\n\n".to_vec();
        let expected = raw.clone();
        let stream = futures_util::stream::iter(vec![IrStreamEvent::Raw(raw)]);
        let mut s = validate_stream_first_chunk(stream.boxed(), "cpa")
            .await
            .expect("content raw passes");
        match s.next().await.unwrap() {
            IrStreamEvent::Raw(got) => assert_eq!(got, expected, "peek must not rewrite raw"),
            other => panic!("expected Raw, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn validate_stream_empty_start_peeks_past_to_content() {
        let empty_start = IrStreamEvent::Start(sag_core::IrResponseMeta {
            id: None,
            created: None,
            model: None,
        });
        let delta = IrStreamEvent::TextDelta {
            index: 0,
            text: "hi".into(),
        };
        let stream = futures_util::stream::iter(vec![empty_start.clone(), delta.clone()]);
        let mut s = validate_stream_first_chunk(stream.boxed(), "cpa")
            .await
            .expect("empty Start followed by content is non-empty");
        // Both events unshifted in order — peek must not drop the empty Start.
        assert_eq!(s.next().await.unwrap(), empty_start);
        assert_eq!(s.next().await.unwrap(), delta);
    }

    #[tokio::test]
    async fn validate_stream_empty_start_alone_then_end_returns_empty() {
        let empty_start = IrStreamEvent::Start(sag_core::IrResponseMeta {
            id: None,
            created: None,
            model: None,
        });
        let stream = futures_util::stream::iter(vec![empty_start, IrStreamEvent::End]);
        let result = validate_stream_first_chunk(stream.boxed(), "cpa").await;
        assert!(
            matches!(result, Err(AppError::EmptyResponse { .. })),
            "metadata-free Start carries no content — End after it is empty"
        );
    }

    // ── is_empty_ir_response ─────────────────────────────────────

    fn ir_with(output: Vec<sag_core::IrOutputItem>, usage: Option<Usage>) -> IrResponse {
        IrResponse {
            id: "resp-1".into(),
            created: Some(0),
            model: "gpt-4o".into(),
            output,
            finish_reason: Some(sag_core::IrFinishReason::Stop),
            usage,
            raw: Default::default(),
            loss: Default::default(),
        }
    }

    fn usage(pt: u32, ct: u32) -> Option<Usage> {
        Some(Usage {
            prompt_tokens: pt,
            completion_tokens: ct,
            total_tokens: pt + ct,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        })
    }

    #[test]
    fn ir_empty_detects_no_output_with_pt() {
        assert!(is_empty_ir_response(&ir_with(vec![], usage(100, 0))));
        assert!(is_empty_ir_response(&ir_with(vec![], usage(100, 50))));
    }

    #[test]
    fn ir_empty_rejects_text_output() {
        assert!(
            !is_empty_ir_response(&ir_with(
                vec![IrOutputItem::Text {
                    text: "hello".into()
                }],
                usage(100, 0)
            )),
            "text content is not empty"
        );
    }

    #[test]
    fn ir_empty_rejects_empty_text_output() {
        assert!(
            is_empty_ir_response(&ir_with(
                vec![IrOutputItem::Text {
                    text: String::new()
                }],
                usage(100, 0)
            )),
            "empty-string text is not content"
        );
    }

    #[test]
    fn ir_empty_rejects_reasoning_output() {
        assert!(
            !is_empty_ir_response(&ir_with(
                vec![IrOutputItem::Reasoning { text: "hmm".into() }],
                usage(100, 0)
            )),
            "reasoning is content"
        );
    }

    #[test]
    fn ir_empty_rejects_tool_call_output() {
        let tc = sag_core::IrToolCall {
            id: "c1".into(),
            name: "fn".into(),
            arguments: serde_json::json!({}),
            index: Some(0),
        };
        assert!(
            !is_empty_ir_response(&ir_with(vec![IrOutputItem::ToolCall(tc)], usage(100, 0))),
            "tool_calls is content"
        );
    }

    #[test]
    fn ir_empty_rejects_raw_output_payload() {
        assert!(
            !is_empty_ir_response(&ir_with(
                vec![IrOutputItem::Raw(serde_json::json!({
                    "type": "output_image",
                    "image_url": {"url": "https://x/y.png"}
                }))],
                usage(100, 0)
            )),
            "non-empty Raw payload is content (aligns with non-empty content-array semantics)"
        );
    }

    #[test]
    fn ir_empty_accepts_null_or_empty_raw_output() {
        for payload in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!([]),
        ] {
            assert!(
                is_empty_ir_response(&ir_with(
                    vec![IrOutputItem::Raw(payload.clone())],
                    usage(100, 0)
                )),
                "Raw {payload} carries no payload → empty"
            );
        }
    }

    #[test]
    fn ir_empty_rejects_later_output_item_content() {
        assert!(
            !is_empty_ir_response(&ir_with(
                vec![
                    IrOutputItem::Text {
                        text: String::new()
                    },
                    IrOutputItem::Reasoning {
                        text: String::new()
                    },
                    IrOutputItem::Text {
                        text: "only-later".into()
                    },
                ],
                usage(100, 0)
            )),
            "content in a later output item must prevent empty (full-scan, not [0]-only)"
        );
    }

    #[test]
    fn ir_empty_rejects_later_choice_content_in_raw() {
        // Multi-choice: only choice[1] has content. decode_response folds
        // choice[0] into output and stashes full choices in raw["choices"].
        let chat = ChatCompletionResponse {
            choices: vec![
                ChatChoice {
                    index: 0,
                    message: ChatMessage {
                        role: "assistant".into(),
                        content: serde_json::Value::Null,
                        name: None,
                        extra: serde_json::Map::new(),
                    },
                    finish_reason: Some("stop".into()),
                },
                ChatChoice {
                    index: 1,
                    message: ChatMessage {
                        role: "assistant".into(),
                        content: serde_json::Value::String("from-second".into()),
                        name: None,
                        extra: serde_json::Map::new(),
                    },
                    finish_reason: Some("stop".into()),
                },
            ],
            usage: usage(100, 0),
            ..build_response("gpt-4o", None)
        };
        let ir = sag_providers::ChatCompletionsCodec::decode_response(&chat);
        assert!(
            !is_empty_ir_response(&ir),
            "multi-choice with content only in choice[1] must be non-empty"
        );
    }

    #[test]
    fn ir_empty_detects_all_choices_no_content_with_pt() {
        let chat = ChatCompletionResponse {
            choices: vec![ChatChoice {
                index: 0,
                message: ChatMessage {
                    role: "assistant".into(),
                    content: serde_json::Value::Null,
                    name: None,
                    extra: serde_json::Map::new(),
                },
                finish_reason: Some("stop".into()),
            }],
            usage: usage(100, 0),
            ..build_response("gpt-4o", None)
        };
        let ir = sag_providers::ChatCompletionsCodec::decode_response(&chat);
        assert!(
            is_empty_ir_response(&ir),
            "pt>0 with no content anywhere is empty"
        );
        // Direct IR construction: no output items + pt>0 → empty.
        assert!(is_empty_ir_response(&ir_with(vec![], usage(100, 0))));
    }

    #[test]
    fn ir_empty_rejects_no_usage() {
        assert!(!is_empty_ir_response(&ir_with(vec![], None)));
    }

    #[test]
    fn ir_empty_rejects_pt_zero() {
        assert!(!is_empty_ir_response(&ir_with(vec![], usage(0, 0))));
    }

    #[test]
    fn ir_empty_aligned_with_chat_choices_semantics() {
        // Chat: choices=[] + pt>0 → empty; IR with no output + pt>0 → empty.
        let chat = ChatCompletionResponse {
            choices: vec![],
            usage: usage(100, 0),
            ..build_response("gpt-4o", None)
        };
        assert!(is_empty_non_stream_response(&chat));
        assert!(is_empty_ir_response(&ir_with(vec![], usage(100, 0))));
    }
}
