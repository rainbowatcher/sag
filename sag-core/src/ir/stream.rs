use serde::{Deserialize, Serialize};

use super::response::{IrFinishReason, IrResponseMeta, usage_eq};
use crate::types::{StreamChunk, Usage};

/// 结构化流事件（translated lane 内部统一载体）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IrStreamEvent {
    /// id / created / model（Anthropic message_start、Responses response.created）
    Start(IrResponseMeta),
    TextDelta {
        index: u32,
        text: String,
    },
    ReasoningDelta {
        index: u32,
        text: String,
    },
    ToolCallStart {
        index: u32,
        id: Option<String>,
        name: Option<String>,
    },
    ToolCallDelta {
        index: u32,
        arguments_delta: String,
    },
    Finish {
        index: u32,
        finish_reason: Option<IrFinishReason>,
    },
    Usage(Usage),
    Error(String),
    /// 流结束（对应 `[DONE]` / message_stop / response.completed）
    End,
    /// 未知/未映射的上游事件原样携带（translated lane）；
    /// native 直通不经 IR，走独立 NativeBytes 通道。
    Raw(Vec<u8>),
}

/// Map a legacy provider [`StreamChunk`] into an IR stream event.
///
/// `Data` becomes [`IrStreamEvent::Raw`] with the **original bytes**
/// (no re-encode / re-parse) so the chat ingress can forward client-visible
/// frames byte-identically. `Usage` / `Error` / `Done` map 1:1.
impl From<StreamChunk> for IrStreamEvent {
    fn from(chunk: StreamChunk) -> Self {
        match chunk {
            StreamChunk::Data(raw) => IrStreamEvent::Raw(raw),
            StreamChunk::Usage(u) => IrStreamEvent::Usage(u),
            StreamChunk::Error(e) => IrStreamEvent::Error(e),
            StreamChunk::Done => IrStreamEvent::End,
        }
    }
}

impl PartialEq for IrStreamEvent {
    fn eq(&self, other: &Self) -> bool {
        use IrStreamEvent as E;
        match (self, other) {
            (E::Start(a), E::Start(b)) => a == b,
            (
                E::TextDelta {
                    index: i1,
                    text: t1,
                },
                E::TextDelta {
                    index: i2,
                    text: t2,
                },
            ) => i1 == i2 && t1 == t2,
            (
                E::ReasoningDelta {
                    index: i1,
                    text: t1,
                },
                E::ReasoningDelta {
                    index: i2,
                    text: t2,
                },
            ) => i1 == i2 && t1 == t2,
            (
                E::ToolCallStart {
                    index: i1,
                    id: id1,
                    name: n1,
                },
                E::ToolCallStart {
                    index: i2,
                    id: id2,
                    name: n2,
                },
            ) => i1 == i2 && id1 == id2 && n1 == n2,
            (
                E::ToolCallDelta {
                    index: i1,
                    arguments_delta: a1,
                },
                E::ToolCallDelta {
                    index: i2,
                    arguments_delta: a2,
                },
            ) => i1 == i2 && a1 == a2,
            (
                E::Finish {
                    index: i1,
                    finish_reason: f1,
                },
                E::Finish {
                    index: i2,
                    finish_reason: f2,
                },
            ) => i1 == i2 && f1 == f2,
            (E::Usage(a), E::Usage(b)) => usage_eq(a, b),
            (E::Error(a), E::Error(b)) => a == b,
            (E::End, E::End) => true,
            (E::Raw(a), E::Raw(b)) => a == b,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ir_stream_event_roundtrip_variants() {
        let events = vec![
            IrStreamEvent::Start(IrResponseMeta {
                id: Some("id".into()),
                created: Some(1),
                model: Some("m".into()),
            }),
            IrStreamEvent::TextDelta {
                index: 0,
                text: "hi".into(),
            },
            IrStreamEvent::ReasoningDelta {
                index: 1,
                text: "hmm".into(),
            },
            IrStreamEvent::ToolCallStart {
                index: 0,
                id: Some("c1".into()),
                name: Some("fn".into()),
            },
            IrStreamEvent::ToolCallDelta {
                index: 0,
                arguments_delta: "{\"a\":1}".into(),
            },
            IrStreamEvent::Finish {
                index: 0,
                finish_reason: Some(IrFinishReason::Stop),
            },
            IrStreamEvent::Usage(Usage {
                prompt_tokens: 1,
                completion_tokens: 2,
                total_tokens: 3,
                cached_tokens: None,
                cache_creation_tokens: None,
                reasoning_tokens: None,
            }),
            IrStreamEvent::Error("boom".into()),
            IrStreamEvent::End,
            IrStreamEvent::Raw(b"event: ping\n\n".to_vec()),
        ];

        for event in events {
            let json = serde_json::to_value(&event).unwrap();
            let back: IrStreamEvent = serde_json::from_value(json).unwrap();
            assert_eq!(back, event);
        }
    }

    #[test]
    fn raw_variant_serializes_bytes() {
        let event = IrStreamEvent::Raw(vec![1, 2, 3]);
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json, serde_json::json!({"raw": [1, 2, 3]}));
        let back: IrStreamEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, IrStreamEvent::Raw(vec![1, 2, 3]));
    }

    #[test]
    fn stream_chunk_maps_to_ir_events() {
        let raw = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n".to_vec();
        assert_eq!(
            IrStreamEvent::from(StreamChunk::Data(raw.clone())),
            IrStreamEvent::Raw(raw)
        );

        let u = Usage {
            prompt_tokens: 1,
            completion_tokens: 2,
            total_tokens: 3,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        };
        assert!(matches!(
            IrStreamEvent::from(StreamChunk::Usage(u.clone())),
            IrStreamEvent::Usage(got) if got.total_tokens == 3
        ));
        assert!(matches!(
            IrStreamEvent::from(StreamChunk::Error("boom".into())),
            IrStreamEvent::Error(ref m) if m == "boom"
        ));
        assert!(matches!(
            IrStreamEvent::from(StreamChunk::Done),
            IrStreamEvent::End
        ));
    }
}
