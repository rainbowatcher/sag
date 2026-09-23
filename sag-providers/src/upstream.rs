//! `TargetUpstream` — dispatch-facing upstream interface (M3).
//!
//! Compatibility bridge over [`LlmProvider::chat`] / [`chat_stream`]:
//! `Lane::Translated` encodes IR → Chat, calls the existing methods, and
//! decodes back to IR. `Lane::Native` currently reuses the same Chat call
//! and returns `UpstreamOutcome::Native` (response JSON); true structural
//! native passthrough lands in M4+. Without an explicit
//! `upstream_protocol` equal to the ingress protocol, dispatch never
//! selects Native (see `sag_core::resolve_lane` §4.2).

use std::collections::HashMap;
use std::pin::Pin;

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use sag_core::{
    ChatCompletionRequest, IrRequest, IrResponse, IrStreamEvent, Lane, StreamChunk,
    UpstreamProtocol,
};

use crate::codec::ChatCompletionsCodec;
use crate::traits::LlmProvider;

/// Raw native-lane SSE bytes (independent of IR).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeBytes(pub Vec<u8>);

/// Per-target context for upstream calls.
pub struct UpstreamTargetCtx<'a> {
    /// Effective protocol (explicit declaration or derived from provider_type).
    pub upstream_protocol: UpstreamProtocol,
    pub api_endpoint: &'a str,
    pub real_model: &'a str,
    pub target_extra: &'a HashMap<String, serde_json::Value>,
    pub timeout_secs: u64,
}

/// Result of a non-streaming upstream call.
pub enum UpstreamOutcome {
    Translated(IrResponse),
    Native(serde_json::Value),
}

/// Stream event dual channel: translated → IR; native → raw bytes.
pub enum UpstreamStreamEvent {
    Ir(IrStreamEvent),
    Native(NativeBytes),
}

/// Dispatch-facing upstream interface with dual-lane support.
#[async_trait]
pub trait TargetUpstream: Send + Sync {
    async fn complete(
        &self,
        ir: &IrRequest,
        target: &UpstreamTargetCtx<'_>,
        lane: Lane,
    ) -> sag_core::Result<UpstreamOutcome>;

    async fn complete_stream(
        &self,
        ir: &IrRequest,
        target: &UpstreamTargetCtx<'_>,
        lane: Lane,
    ) -> sag_core::Result<Pin<Box<dyn Stream<Item = UpstreamStreamEvent> + Send>>>;
}

/// Map a legacy [`StreamChunk`] into IR stream events for the translated bridge.
fn stream_chunk_to_ir_events(chunk: StreamChunk) -> Vec<IrStreamEvent> {
    match chunk {
        StreamChunk::Data(bytes) => ChatCompletionsCodec::decode_stream_frame(&bytes),
        StreamChunk::Usage(u) => vec![IrStreamEvent::Usage(u)],
        StreamChunk::Error(e) => vec![IrStreamEvent::Error(e)],
        StreamChunk::Done => vec![IrStreamEvent::End],
    }
}

/// Shared compatibility-bridge implementation used by both concrete providers.
async fn bridge_complete<P: LlmProvider + ?Sized>(
    provider: &P,
    ir: &IrRequest,
    target: &UpstreamTargetCtx<'_>,
    lane: Lane,
) -> sag_core::Result<UpstreamOutcome> {
    let (encoded, loss) = ChatCompletionsCodec::encode_request_with_loss(ir);
    if !loss.is_empty() {
        tracing::warn!(loss = ?loss.entries, "chat encode recorded losses");
    }
    let resp = provider
        .chat(
            &encoded,
            target.real_model,
            target.api_endpoint,
            target.target_extra,
            target.timeout_secs,
        )
        .await?;
    match lane {
        Lane::Translated => Ok(UpstreamOutcome::Translated(
            ChatCompletionsCodec::decode_response(&resp),
        )),
        Lane::Native => {
            let value = serde_json::to_value(&resp).map_err(|e| {
                sag_core::AppError::Internal(format!("encode native response: {e}"))
            })?;
            Ok(UpstreamOutcome::Native(value))
        }
    }
}

async fn bridge_complete_stream<P: LlmProvider + ?Sized>(
    provider: &P,
    ir: &IrRequest,
    target: &UpstreamTargetCtx<'_>,
    lane: Lane,
) -> sag_core::Result<Pin<Box<dyn Stream<Item = UpstreamStreamEvent> + Send>>> {
    let (encoded, loss) = ChatCompletionsCodec::encode_request_with_loss(ir);
    if !loss.is_empty() {
        tracing::warn!(loss = ?loss.entries, "chat encode recorded losses");
    }
    let upstream = provider
        .chat_stream(
            &encoded,
            target.real_model,
            target.api_endpoint,
            target.target_extra,
            target.timeout_secs,
        )
        .await?;
    match lane {
        Lane::Translated => {
            let mapped = upstream.flat_map(|chunk| {
                let events: Vec<UpstreamStreamEvent> = stream_chunk_to_ir_events(chunk)
                    .into_iter()
                    .map(UpstreamStreamEvent::Ir)
                    .collect();
                futures_util::stream::iter(events)
            });
            Ok(Box::pin(mapped))
        }
        Lane::Native => {
            // M3: native streaming still rides the Chat channel but surfaces
            // raw SSE bytes on the Native path (true structural passthrough = M4+).
            let mapped = upstream.flat_map(|chunk| {
                let events: Vec<UpstreamStreamEvent> = match chunk {
                    StreamChunk::Data(bytes) => {
                        vec![UpstreamStreamEvent::Native(NativeBytes(bytes))]
                    }
                    other => stream_chunk_to_ir_events(other)
                        .into_iter()
                        .map(UpstreamStreamEvent::Ir)
                        .collect(),
                };
                futures_util::stream::iter(events)
            });
            Ok(Box::pin(mapped))
        }
    }
}

macro_rules! impl_target_upstream {
    ($ty:ty) => {
        #[async_trait]
        impl TargetUpstream for $ty {
            async fn complete(
                &self,
                ir: &IrRequest,
                target: &UpstreamTargetCtx<'_>,
                lane: Lane,
            ) -> sag_core::Result<UpstreamOutcome> {
                bridge_complete(self, ir, target, lane).await
            }

            async fn complete_stream(
                &self,
                ir: &IrRequest,
                target: &UpstreamTargetCtx<'_>,
                lane: Lane,
            ) -> sag_core::Result<Pin<Box<dyn Stream<Item = UpstreamStreamEvent> + Send>>> {
                bridge_complete_stream(self, ir, target, lane).await
            }
        }
    };
}

impl_target_upstream!(crate::openai::OpenAiProvider);
impl_target_upstream!(crate::anthropic::AnthropicProvider);

/// Helper: encode a Chat request into IR for callers that start from Chat.
/// (Convenience for tests / future dispatch wiring.)
pub fn chat_to_ir(chat: &ChatCompletionRequest) -> IrRequest {
    ChatCompletionsCodec::decode_request(chat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sag_core::{
        ChatCompletionRequest, ChatMessage, IngressProtocol, IrRequest, Lane, Usage,
        derive_upstream_protocol, effective_upstream_protocol, resolve_lane,
    };

    use crate::OpenAiProvider;
    use crate::test_support::{chat_request, spawn_server, test_config};

    #[test]
    fn lane_defaults_to_translated_without_explicit_declaration() {
        for ingress in [
            IngressProtocol::ChatCompletions,
            IngressProtocol::Responses,
            IngressProtocol::AnthropicMessages,
        ] {
            assert_eq!(resolve_lane(None, ingress), Lane::Translated);
        }
        // Explicit == ingress → Native (path exists).
        assert_eq!(
            resolve_lane(
                Some(UpstreamProtocol::ChatCompletions),
                IngressProtocol::ChatCompletions
            ),
            Lane::Native
        );
        // Derived value never feeds resolve_lane as explicit.
        let derived = derive_upstream_protocol("openai");
        assert_eq!(effective_upstream_protocol(None, derived), derived);
        assert_eq!(
            resolve_lane(None, IngressProtocol::ChatCompletions),
            Lane::Translated
        );
    }

    #[tokio::test]
    async fn target_upstream_translated_complete_decodes_ir_response() {
        let server = spawn_server(|mut stream| async move {
            use tokio::io::AsyncWriteExt;
            let body = r#"{
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "created": 1,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "hello"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7}
            }"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
        })
        .await;

        let config = test_config(&server, 5, Some(60), None);
        let provider = OpenAiProvider::new("openai".into(), &config).unwrap();
        let chat = chat_request();
        let ir = ChatCompletionsCodec::decode_request(&chat);
        let extra = HashMap::new();
        let target = UpstreamTargetCtx {
            upstream_protocol: UpstreamProtocol::ChatCompletions,
            api_endpoint: "/chat/completions",
            real_model: "test-model",
            target_extra: &extra,
            timeout_secs: 5,
        };

        let outcome = provider
            .complete(&ir, &target, Lane::Translated)
            .await
            .expect("complete ok");
        let UpstreamOutcome::Translated(resp) = outcome else {
            panic!("expected Translated outcome");
        };
        let text = resp
            .output
            .iter()
            .find_map(|item| match item {
                sag_core::IrOutputItem::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .expect("text output");
        assert_eq!(text, "hello");
        let usage = resp.usage.expect("usage");
        assert_eq!(usage.prompt_tokens, 3);
        assert_eq!(usage.completion_tokens, 4);
        assert_eq!(usage.total_tokens, 7);
    }

    #[tokio::test]
    async fn target_upstream_native_complete_returns_native_value() {
        let server = spawn_server(|mut stream| async move {
            use tokio::io::AsyncWriteExt;
            let body = r#"{
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "created": 1,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "hi"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            }"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
        })
        .await;

        let config = test_config(&server, 5, Some(60), None);
        let provider = OpenAiProvider::new("openai".into(), &config).unwrap();
        let ir = ChatCompletionsCodec::decode_request(&chat_request());
        let extra = HashMap::new();
        let target = UpstreamTargetCtx {
            upstream_protocol: UpstreamProtocol::ChatCompletions,
            api_endpoint: "/chat/completions",
            real_model: "test-model",
            target_extra: &extra,
            timeout_secs: 5,
        };

        let outcome = provider
            .complete(&ir, &target, Lane::Native)
            .await
            .expect("native complete ok");
        let UpstreamOutcome::Native(value) = outcome else {
            panic!("expected Native outcome");
        };
        assert_eq!(value["id"], "chatcmpl-1");
        assert_eq!(value["choices"][0]["message"]["content"], "hi");
    }

    #[tokio::test]
    async fn target_upstream_complete_stream_collects_ir_events() {
        let chunk = serde_json::json!({
            "id": "c1",
            "object": "chat.completion.chunk",
            "created": 1,
            "model": "test-model",
            "choices": [{
                "index": 0,
                "delta": {"content": "hey"},
                "finish_reason": null
            }]
        });
        let payload = format!("data: {chunk}\n\ndata: [DONE]\n\n");
        let server = spawn_server(move |mut stream| {
            let payload = payload.clone();
            async move {
                use tokio::io::AsyncWriteExt;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{}",
                    payload
                );
                stream.write_all(resp.as_bytes()).await.unwrap();
            }
        })
        .await;

        let config = test_config(&server, 5, Some(60), None);
        let provider = OpenAiProvider::new("openai".into(), &config).unwrap();
        let mut chat = chat_request();
        chat.stream = Some(true);
        let ir = ChatCompletionsCodec::decode_request(&chat);
        let extra = HashMap::new();
        let target = UpstreamTargetCtx {
            upstream_protocol: UpstreamProtocol::ChatCompletions,
            api_endpoint: "/chat/completions",
            real_model: "test-model",
            target_extra: &extra,
            timeout_secs: 5,
        };

        let mut stream = provider
            .complete_stream(&ir, &target, Lane::Translated)
            .await
            .expect("stream ok");
        let mut saw_text = false;
        let mut saw_end = false;
        while let Some(ev) = stream.next().await {
            match ev {
                UpstreamStreamEvent::Ir(IrStreamEvent::TextDelta { text, .. }) => {
                    assert_eq!(text, "hey");
                    saw_text = true;
                }
                UpstreamStreamEvent::Ir(IrStreamEvent::End) => saw_end = true,
                UpstreamStreamEvent::Ir(_) | UpstreamStreamEvent::Native(_) => {}
            }
        }
        assert!(saw_text, "expected TextDelta event");
        assert!(saw_end, "expected End event");
    }

    #[tokio::test]
    async fn target_upstream_stream_error_and_done_map_to_ir() {
        // Direct unit of the chunk mapper (no HTTP).
        assert!(matches!(
            stream_chunk_to_ir_events(StreamChunk::Done)[0],
            IrStreamEvent::End
        ));
        assert!(matches!(
            stream_chunk_to_ir_events(StreamChunk::Error("boom".into()))[0],
            IrStreamEvent::Error(ref m) if m == "boom"
        ));
        let u = Usage {
            prompt_tokens: 1,
            completion_tokens: 2,
            total_tokens: 3,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        };
        assert!(matches!(
            stream_chunk_to_ir_events(StreamChunk::Usage(u.clone()))[0],
            IrStreamEvent::Usage(ref got) if got.total_tokens == 3
        ));
        // Unparseable Data → Raw via codec.
        let events = stream_chunk_to_ir_events(StreamChunk::Data(b"data: {not-json}\n\n".to_vec()));
        assert!(matches!(events[0], IrStreamEvent::Raw(_)));
    }

    #[allow(dead_code)]
    fn _assert_chat_roundtrip(chat: &ChatCompletionRequest) -> ChatCompletionRequest {
        let ir: IrRequest = ChatCompletionsCodec::decode_request(chat);
        ChatCompletionsCodec::encode_request(&ir)
    }

    #[allow(dead_code)]
    fn _msg() -> ChatMessage {
        ChatMessage {
            role: "user".into(),
            content: serde_json::json!("hi"),
            name: None,
            extra: Default::default(),
        }
    }
}
