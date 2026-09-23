use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::loss::LossInventory;
use crate::types::Usage;

/// 结束原因。已知值按 snake_case 字符串序列化；`Other` 保留原词。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IrFinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    FunctionCall,
    Cancelled,
    /// 未知结束原因原词。**禁止存放已知 token**（`stop`/`length`/`tool_calls`/`content_filter`/`function_call`/`cancelled`）——请用 [`IrFinishReason::from_raw`]，避免与已知变体同词不同值。
    Other(String),
}

impl IrFinishReason {
    /// 从原始字符串构造：已知 token 折叠为对应已知变体，否则 `Other(s)`。
    /// 这是 `Other` 的唯一安全入口，反序列化也走此路径。
    pub fn from_raw(s: impl Into<String>) -> Self {
        let s = s.into();
        match s.as_str() {
            "stop" => Self::Stop,
            "length" => Self::Length,
            "tool_calls" => Self::ToolCalls,
            "content_filter" => Self::ContentFilter,
            "function_call" => Self::FunctionCall,
            "cancelled" => Self::Cancelled,
            _ => Self::Other(s),
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolCalls => "tool_calls",
            Self::ContentFilter => "content_filter",
            Self::FunctionCall => "function_call",
            Self::Cancelled => "cancelled",
            Self::Other(s) => s,
        }
    }
}

impl Serialize for IrFinishReason {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if let Self::Other(s) = self {
            debug_assert!(
                matches!(Self::from_raw(s.as_str()), Self::Other(_)),
                "IrFinishReason::Other must not hold a known token ({s:?}); use IrFinishReason::from_raw"
            );
        }
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for IrFinishReason {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from_raw(s))
    }
}

/// 响应中的输出项。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IrOutputItem {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall(super::request::IrToolCall),
    /// 无法映射的响应项——原样保留 + 记 loss
    Raw(Value),
}

/// 流/响应级元信息。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct IrResponseMeta {
    pub id: Option<String>,
    pub created: Option<u64>,
    pub model: Option<String>,
}

/// 协议中立的非流式响应。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IrResponse {
    pub id: String,
    pub created: Option<u64>,
    pub model: String,
    pub output: Vec<IrOutputItem>,
    pub finish_reason: Option<IrFinishReason>,
    pub usage: Option<Usage>,
    pub raw: Map<String, Value>,
    pub loss: LossInventory,
}

pub(crate) fn usage_eq(a: &Usage, b: &Usage) -> bool {
    a.prompt_tokens == b.prompt_tokens
        && a.completion_tokens == b.completion_tokens
        && a.total_tokens == b.total_tokens
        && a.cached_tokens == b.cached_tokens
        && a.cache_creation_tokens == b.cache_creation_tokens
        && a.reasoning_tokens == b.reasoning_tokens
}

impl PartialEq for IrResponse {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.created == other.created
            && self.model == other.model
            && self.output == other.output
            && self.finish_reason == other.finish_reason
            && match (&self.usage, &other.usage) {
                (None, None) => true,
                (Some(a), Some(b)) => usage_eq(a, b),
                _ => false,
            }
            && self.raw == other.raw
            && self.loss == other.loss
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::request::IrToolCall;

    #[test]
    fn ir_finish_reason_snake_case_roundtrip() {
        assert_eq!(
            serde_json::to_value(IrFinishReason::Stop).unwrap(),
            serde_json::json!("stop")
        );
        assert_eq!(
            serde_json::to_value(IrFinishReason::ToolCalls).unwrap(),
            serde_json::json!("tool_calls")
        );
        assert_eq!(
            serde_json::to_value(IrFinishReason::ContentFilter).unwrap(),
            serde_json::json!("content_filter")
        );
        assert_eq!(
            serde_json::to_value(IrFinishReason::FunctionCall).unwrap(),
            serde_json::json!("function_call")
        );

        for reason in [
            IrFinishReason::Stop,
            IrFinishReason::Length,
            IrFinishReason::ToolCalls,
            IrFinishReason::ContentFilter,
            IrFinishReason::FunctionCall,
            IrFinishReason::Cancelled,
        ] {
            let json = serde_json::to_value(&reason).unwrap();
            let back: IrFinishReason = serde_json::from_value(json).unwrap();
            assert_eq!(back, reason);
        }
    }

    #[test]
    fn ir_finish_reason_other_preserves_raw_word() {
        let reason = IrFinishReason::Other("content_filter_v2".into());
        let json = serde_json::to_value(&reason).unwrap();
        assert_eq!(json, serde_json::json!("content_filter_v2"));
        let back: IrFinishReason = serde_json::from_value(json).unwrap();
        assert_eq!(back, IrFinishReason::Other("content_filter_v2".into()));
    }

    #[test]
    fn ir_finish_reason_from_raw_folds_known_tokens() {
        for (raw, expected) in [
            ("stop", IrFinishReason::Stop),
            ("length", IrFinishReason::Length),
            ("tool_calls", IrFinishReason::ToolCalls),
            ("content_filter", IrFinishReason::ContentFilter),
            ("function_call", IrFinishReason::FunctionCall),
            ("cancelled", IrFinishReason::Cancelled),
        ] {
            assert_eq!(IrFinishReason::from_raw(raw), expected, "from_raw({raw:?})");
        }
        assert_eq!(
            IrFinishReason::from_raw("custom"),
            IrFinishReason::Other("custom".into())
        );
    }

    #[test]
    fn ir_finish_reason_deserialize_folds_known_tokens() {
        for (json, expected) in [
            ("\"stop\"", IrFinishReason::Stop),
            ("\"length\"", IrFinishReason::Length),
            ("\"tool_calls\"", IrFinishReason::ToolCalls),
            ("\"content_filter\"", IrFinishReason::ContentFilter),
            ("\"function_call\"", IrFinishReason::FunctionCall),
            ("\"cancelled\"", IrFinishReason::Cancelled),
        ] {
            let back: IrFinishReason = serde_json::from_str(json).unwrap();
            assert_eq!(back, expected, "from_str({json})");
            assert_eq!(back, IrFinishReason::from_raw(expected.as_str()));
            assert_ne!(back, IrFinishReason::Other(expected.as_str().into()));
        }
    }

    #[test]
    fn ir_response_roundtrip() {
        let mut loss = LossInventory::default();
        loss.push_loss(
            super::super::loss::LossScope::Response,
            "choices[0].refusal",
            super::super::loss::LossReason::UnsupportedByIr,
            serde_json::json!("refused"),
        );

        let resp = IrResponse {
            id: "resp-1".into(),
            created: Some(1_700_000_000),
            model: "m".into(),
            output: vec![
                IrOutputItem::Text {
                    text: "hello".into(),
                },
                IrOutputItem::Reasoning {
                    text: "think".into(),
                },
                IrOutputItem::ToolCall(IrToolCall {
                    id: "c1".into(),
                    name: "fn".into(),
                    arguments: serde_json::json!({"x":1}),
                    index: Some(0),
                }),
                IrOutputItem::Raw(serde_json::json!({"type": "web_search_call"})),
            ],
            finish_reason: Some(IrFinishReason::ToolCalls),
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cached_tokens: Some(1),
                cache_creation_tokens: None,
                reasoning_tokens: Some(2),
            }),
            raw: {
                let mut m = Map::new();
                m.insert("system_fingerprint".into(), serde_json::json!("fp"));
                m
            },
            loss,
        };

        let json = serde_json::to_value(&resp).unwrap();
        let back: IrResponse = serde_json::from_value(json).unwrap();
        assert_eq!(back, resp);
    }
}
