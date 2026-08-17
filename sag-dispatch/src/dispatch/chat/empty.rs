//! Empty-response detection and first-chunk validation for chat completions.

use futures_util::StreamExt;
use sag_core::{AppError, ChatCompletionResponse, StreamChunk};

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

/// Peek at the first chunk of a streaming response. If the upstream
/// returned an empty response (zero `completion_tokens`, no content),
/// return [`AppError::EmptyResponse`] so the retry loop retries the same
/// target with backoff, falling through to the next target only when the
/// retry budget is exhausted. Otherwise, prepend the consumed chunk and
/// return the stream for normal forwarding.
pub(crate) async fn validate_stream_first_chunk(
    mut stream: futures_util::stream::BoxStream<'static, StreamChunk>,
    provider: &str,
) -> Result<futures_util::stream::BoxStream<'static, StreamChunk>, AppError> {
    let first = stream.next().await;
    match first {
        // Empty stream or immediate Done -> no output at all.
        None | Some(StreamChunk::Done) => Err(AppError::EmptyResponse {
            provider: provider.into(),
        }),

        // Mid-stream error from the provider -> propagate as retryable.
        Some(StreamChunk::Error(msg)) => Err(AppError::Upstream {
            provider: provider.into(),
            status: 502,
            body: msg,
        }),

        // Pure Usage chunk (Data was intercepted by process_sse_frame).
        // If ct == 0 and pt > 0, the response is empty.
        Some(StreamChunk::Usage(u)) => {
            // NOTE: A `Usage` chunk carries only token counts, not `choices`, so we
            // cannot apply the same choices-based emptiness check as `is_empty_response_data`.
            // We fall back to the conservative ct==0 && pt>0 signal here. A Data frame
            // carrying usage + choices is checked more precisely in the Data branch below.
            if u.completion_tokens == 0 && u.prompt_tokens > 0 {
                Err(AppError::EmptyResponse {
                    provider: provider.into(),
                })
            } else {
                Ok(futures_util::stream::iter(vec![StreamChunk::Usage(u)])
                    .chain(stream)
                    .boxed())
            }
        }

        // Data chunk -- check for the [DONE] marker or an empty usage frame.
        Some(StreamChunk::Data(raw)) => {
            if raw.starts_with(b"data: [DONE]") || is_empty_response_data(&raw) {
                Err(AppError::EmptyResponse {
                    provider: provider.into(),
                })
            } else {
                Ok(futures_util::stream::iter(vec![StreamChunk::Data(raw)])
                    .chain(stream)
                    .boxed())
            }
        }
    }
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
        let stream = futures_util::stream::iter(vec![StreamChunk::Data(
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"total_tokens\":100}}\n\n".to_vec(),
        )]);
        let result = validate_stream_first_chunk(stream.boxed(), "cpa").await;
        assert!(matches!(result, Err(AppError::EmptyResponse { .. })));
    }

    #[tokio::test]
    async fn validate_stream_content_data_passes_through() {
        let stream = futures_util::stream::iter(vec![
            StreamChunk::Data(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n".to_vec(),
            ),
            StreamChunk::Done,
        ]);
        let mut s = validate_stream_first_chunk(stream.boxed(), "cpa")
            .await
            .expect("content frame should pass through");
        let first = s.next().await.unwrap();
        assert!(matches!(first, StreamChunk::Data(_)));
        let second = s.next().await.unwrap();
        assert!(matches!(second, StreamChunk::Done));
    }

    #[tokio::test]
    async fn validate_stream_done_marker_returns_error() {
        let stream =
            futures_util::stream::iter(vec![StreamChunk::Data(b"data: [DONE]\n\n".to_vec())]);
        let result = validate_stream_first_chunk(stream.boxed(), "cpa").await;
        assert!(matches!(result, Err(AppError::EmptyResponse { .. })));
    }

    #[tokio::test]
    async fn validate_stream_usage_ct0_returns_error() {
        let stream = futures_util::stream::iter(vec![StreamChunk::Usage(Usage {
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
        let stream = futures_util::stream::iter(vec![StreamChunk::Usage(Usage {
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
        assert!(matches!(s.next().await, Some(StreamChunk::Usage(_))));
    }

    #[tokio::test]
    async fn validate_stream_empty_stream_returns_error() {
        let stream = futures_util::stream::empty::<StreamChunk>();
        let result = validate_stream_first_chunk(stream.boxed(), "cpa").await;
        assert!(matches!(result, Err(AppError::EmptyResponse { .. })));
    }
}
