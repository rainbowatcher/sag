use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::loss::LossInventory;

/// 消息角色。已知角色按 snake_case 字符串序列化；`Other` 保留原词。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum IrRole {
    System,
    #[default]
    User,
    Assistant,
    Tool,
    Developer,
    /// 未知角色原词。**禁止存放已知 token**（`system`/`user`/`assistant`/`tool`/`developer`）——请用 [`IrRole::from_raw`]，避免与已知变体同词不同值。
    Other(String),
}

impl IrRole {
    /// 从原始字符串构造：已知 token 折叠为对应已知变体，否则 `Other(s)`。
    /// 这是 `Other` 的唯一安全入口，反序列化也走此路径。
    pub fn from_raw(s: impl Into<String>) -> Self {
        let s = s.into();
        match s.as_str() {
            "system" => Self::System,
            "user" => Self::User,
            "assistant" => Self::Assistant,
            "tool" => Self::Tool,
            "developer" => Self::Developer,
            _ => Self::Other(s),
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
            Self::Developer => "developer",
            Self::Other(s) => s,
        }
    }
}

impl Serialize for IrRole {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if let Self::Other(s) = self {
            debug_assert!(
                matches!(Self::from_raw(s.as_str()), Self::Other(_)),
                "IrRole::Other must not hold a known token ({s:?}); use IrRole::from_raw"
            );
        }
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for IrRole {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::from_raw(s))
    }
}

/// 协议中立的内容块。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IrContentBlock {
    Text {
        text: String,
    },
    /// 覆盖 Chat `reasoning_content` / Anthropic thinking / Responses reasoning。
    Reasoning {
        text: String,
    },
    /// url / b64 / media_type 原样保留
    Image {
        data: Value,
    },
    /// file / document（Anthropic document block、Chat file part）
    Document {
        data: Value,
    },
    Audio {
        data: Value,
    },
    ToolUse {
        id: String,
        name: String,
        arguments: Value,
    },
    ToolResult {
        id: Option<String>,
        tool_call_id: String,
        content: Vec<IrContentBlock>,
        is_error: bool,
    },
    /// 未知/新 content block 类型——原样携带，不解析、不丢弃
    Raw(Value),
}

/// 已完成的工具调用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IrToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    pub index: Option<u32>,
}

/// 工具定义。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IrTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub raw: Map<String, Value>,
}

/// 采样参数归一化（全部可选）。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Sampling {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub seed: Option<i64>,
    pub presence_penalty: Option<f64>,
    pub frequency_penalty: Option<f64>,
    pub logit_bias: Option<Value>,
}

/// reasoning 的归一化描述；`effort` 保留原词（none/minimal/low/medium/high/max/xhigh…）。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Reasoning {
    pub effort: Option<String>,
    pub budget_tokens: Option<u32>,
    pub thinking: Option<Value>,
}

/// 协议中立的请求。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct IrRequest {
    pub model: String,
    pub messages: Vec<IrMessage>,
    pub sampling: Sampling,
    /// 归一化：Chat 取 `max_completion_tokens` 优先，否则 `max_tokens`
    pub max_tokens: Option<u32>,
    pub stream: bool,
    pub tools: Vec<IrTool>,
    /// 保留原样（三种协议形状不同）
    pub tool_choice: Value,
    pub response_format: Value,
    /// effort / budget / thinking 的归一化描述
    pub reasoning: Reasoning,
    pub stop: Vec<String>,
    pub metadata: Value,
    /// 入口协议无法映射为 IR 字段的原始请求字段，透传给 target。
    pub raw: Map<String, Value>,
    pub loss: LossInventory,
}

/// 协议中立的消息。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct IrMessage {
    pub role: IrRole,
    pub content: Vec<IrContentBlock>,
    pub name: Option<String>,
    pub tool_call_id: Option<String>,
    /// assistant 消息的已完成调用
    pub tool_calls: Vec<IrToolCall>,
    /// 该消息无法映射的原始字段
    pub raw: Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ir_role_snake_case_roundtrip() {
        assert_eq!(
            serde_json::to_value(IrRole::System).unwrap(),
            serde_json::json!("system")
        );
        assert_eq!(
            serde_json::to_value(IrRole::User).unwrap(),
            serde_json::json!("user")
        );
        assert_eq!(
            serde_json::to_value(IrRole::Assistant).unwrap(),
            serde_json::json!("assistant")
        );
        assert_eq!(
            serde_json::to_value(IrRole::Tool).unwrap(),
            serde_json::json!("tool")
        );
        assert_eq!(
            serde_json::to_value(IrRole::Developer).unwrap(),
            serde_json::json!("developer")
        );

        for role in [
            IrRole::System,
            IrRole::User,
            IrRole::Assistant,
            IrRole::Tool,
            IrRole::Developer,
        ] {
            let json = serde_json::to_value(&role).unwrap();
            let back: IrRole = serde_json::from_value(json).unwrap();
            assert_eq!(back, role);
        }
    }

    #[test]
    fn ir_role_other_preserves_raw_word() {
        let role = IrRole::Other("custom".into());
        let json = serde_json::to_value(&role).unwrap();
        assert_eq!(json, serde_json::json!("custom"));
        let back: IrRole = serde_json::from_value(json).unwrap();
        assert_eq!(back, IrRole::Other("custom".into()));

        let round: IrRole = serde_json::from_str("\"whatever\"").unwrap();
        assert_eq!(round, IrRole::Other("whatever".into()));
    }

    #[test]
    fn ir_role_from_raw_folds_known_tokens() {
        for (raw, expected) in [
            ("system", IrRole::System),
            ("user", IrRole::User),
            ("assistant", IrRole::Assistant),
            ("tool", IrRole::Tool),
            ("developer", IrRole::Developer),
        ] {
            assert_eq!(IrRole::from_raw(raw), expected, "from_raw({raw:?})");
        }
        assert_eq!(IrRole::from_raw("custom"), IrRole::Other("custom".into()));
    }

    #[test]
    fn ir_role_deserialize_folds_known_tokens() {
        for (json, expected) in [
            ("\"system\"", IrRole::System),
            ("\"user\"", IrRole::User),
            ("\"assistant\"", IrRole::Assistant),
            ("\"tool\"", IrRole::Tool),
            ("\"developer\"", IrRole::Developer),
        ] {
            let back: IrRole = serde_json::from_str(json).unwrap();
            assert_eq!(back, expected, "from_str({json})");
            assert_eq!(back, IrRole::from_raw(expected.as_str()));
            assert_ne!(back, IrRole::Other(expected.as_str().into()));
        }
    }

    #[test]
    fn ir_request_roundtrip() {
        let mut loss = LossInventory::default();
        loss.push_loss(
            super::super::loss::LossScope::Request,
            "n",
            super::super::loss::LossReason::UnsupportedByIr,
            serde_json::json!(2),
        );

        let req = IrRequest {
            model: "m".into(),
            messages: vec![IrMessage {
                role: IrRole::System,
                content: vec![
                    IrContentBlock::Text { text: "hi".into() },
                    IrContentBlock::ToolUse {
                        id: "c1".into(),
                        name: "fn".into(),
                        arguments: serde_json::json!({"a":1}),
                    },
                    IrContentBlock::ToolResult {
                        id: None,
                        tool_call_id: "c1".into(),
                        content: vec![IrContentBlock::Text { text: "ok".into() }],
                        is_error: false,
                    },
                    IrContentBlock::Raw(serde_json::json!({"type": "mystery"})),
                ],
                name: Some("n".into()),
                tool_call_id: None,
                tool_calls: vec![IrToolCall {
                    id: "c1".into(),
                    name: "fn".into(),
                    arguments: serde_json::json!({}),
                    index: Some(0),
                }],
                raw: Map::new(),
            }],
            sampling: Sampling {
                temperature: Some(0.2),
                top_p: Some(0.9),
                top_k: Some(40),
                seed: Some(7),
                presence_penalty: None,
                frequency_penalty: None,
                logit_bias: Some(serde_json::json!({"1": -1})),
            },
            max_tokens: Some(128),
            stream: true,
            tools: vec![IrTool {
                name: "fn".into(),
                description: Some("d".into()),
                input_schema: serde_json::json!({"type": "object"}),
                raw: Map::new(),
            }],
            tool_choice: serde_json::json!("auto"),
            response_format: serde_json::json!({"type": "text"}),
            reasoning: Reasoning {
                effort: Some("high".into()),
                budget_tokens: Some(1024),
                thinking: Some(serde_json::json!({"type": "enabled"})),
            },
            stop: vec!["END".into()],
            metadata: serde_json::json!({"k": "v"}),
            raw: {
                let mut m = Map::new();
                m.insert("modalities".into(), serde_json::json!(["text"]));
                m
            },
            loss,
        };

        let json = serde_json::to_value(&req).unwrap();
        let back: IrRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, req);
    }
}
