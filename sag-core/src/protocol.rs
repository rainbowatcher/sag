use serde::{Deserialize, Serialize};

/// 入口协议：由挂载路径决定，不写入配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngressProtocol {
    ChatCompletions,
    Responses,
    AnthropicMessages,
}

impl IngressProtocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
            Self::AnthropicMessages => "anthropic_messages",
        }
    }
}

/// 上游协议：target 显式声明（或由 provider_type 推导——推导值仅决定 translated 编码器）；
/// native 仅当显式声明且 == ingress。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamProtocol {
    ChatCompletions,
    Responses,
    AnthropicMessages,
}

impl UpstreamProtocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
            Self::AnthropicMessages => "anthropic_messages",
        }
    }
}

/// 缺省由 provider_type 推导：openai→ChatCompletions, anthropic→AnthropicMessages。
/// 推导值仅用于 translated 编码器选择，不参与 native 判定（§4.2 / D4）。
pub fn derive_upstream_protocol(provider_type: &str) -> UpstreamProtocol {
    match provider_type {
        "anthropic" => UpstreamProtocol::AnthropicMessages,
        // openai 及未知类型：保守按 Chat Completions（与 provider_type 默认一致）。
        _ => UpstreamProtocol::ChatCompletions,
    }
}

/// native 仅当显式声明且 == ingress（§4.2）。
///
/// - `resolve_lane(Some(u), i)` = if u == i { Native } else { Translated }
/// - `resolve_lane(None, _)` = Translated（推导值永不触发 native）
pub fn resolve_lane(explicit: Option<UpstreamProtocol>, ingress: IngressProtocol) -> Lane {
    match explicit {
        Some(u)
            if matches!(
                (u, ingress),
                (
                    UpstreamProtocol::ChatCompletions,
                    IngressProtocol::ChatCompletions
                ) | (UpstreamProtocol::Responses, IngressProtocol::Responses)
                    | (
                        UpstreamProtocol::AnthropicMessages,
                        IngressProtocol::AnthropicMessages
                    )
            ) =>
        {
            Lane::Native
        }
        _ => Lane::Translated,
    }
}

/// 显式优先，否则用推导值——**只用于 translated 编码器选择**，不参与 lane 判定。
pub fn effective_upstream_protocol(
    explicit: Option<UpstreamProtocol>,
    derived: UpstreamProtocol,
) -> UpstreamProtocol {
    explicit.unwrap_or(derived)
}

/// 车道
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lane {
    Native,
    Translated,
}

impl Lane {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Translated => "translated",
        }
    }
}

/// 目标协议能力位图（固定键集合）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Capabilities {
    pub streaming: bool,
    /// thinking / reasoning_content 支持
    pub reasoning: bool,
    pub tools: bool,
    pub images: bool,
    pub documents: bool,
    /// Responses store / previous_response_id
    pub stateful: bool,
    /// Responses hosted tool 类型
    pub hosted_tools: bool,
    /// Responses background 执行
    pub background: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(value: &T) -> T
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let json = serde_json::to_value(value).expect("serialize");
        let back: T = serde_json::from_value(json).expect("deserialize");
        assert_eq!(&back, value);
        back
    }

    #[test]
    fn ingress_protocol_snake_case_roundtrip() {
        assert_eq!(
            serde_json::to_value(IngressProtocol::ChatCompletions).unwrap(),
            serde_json::json!("chat_completions")
        );
        assert_eq!(
            serde_json::to_value(IngressProtocol::AnthropicMessages).unwrap(),
            serde_json::json!("anthropic_messages")
        );
        roundtrip(&IngressProtocol::ChatCompletions);
        roundtrip(&IngressProtocol::Responses);
        roundtrip(&IngressProtocol::AnthropicMessages);
        assert_eq!(IngressProtocol::Responses.as_str(), "responses");
    }

    #[test]
    fn upstream_protocol_snake_case_roundtrip() {
        assert_eq!(
            serde_json::to_value(UpstreamProtocol::ChatCompletions).unwrap(),
            serde_json::json!("chat_completions")
        );
        assert_eq!(
            serde_json::to_value(UpstreamProtocol::Responses).unwrap(),
            serde_json::json!("responses")
        );
        assert_eq!(
            serde_json::to_value(UpstreamProtocol::AnthropicMessages).unwrap(),
            serde_json::json!("anthropic_messages")
        );
        roundtrip(&UpstreamProtocol::ChatCompletions);
        roundtrip(&UpstreamProtocol::Responses);
        roundtrip(&UpstreamProtocol::AnthropicMessages);
        assert_eq!(
            UpstreamProtocol::AnthropicMessages.as_str(),
            "anthropic_messages"
        );
    }

    #[test]
    fn lane_snake_case_roundtrip() {
        assert_eq!(
            serde_json::to_value(Lane::Native).unwrap(),
            serde_json::json!("native")
        );
        assert_eq!(
            serde_json::to_value(Lane::Translated).unwrap(),
            serde_json::json!("translated")
        );
        roundtrip(&Lane::Native);
        roundtrip(&Lane::Translated);
    }

    #[test]
    fn upstream_protocol_rejects_unknown_value() {
        assert!(serde_json::from_str::<UpstreamProtocol>("\"nope\"").is_err());
        assert!(serde_json::from_str::<IngressProtocol>("\"nope\"").is_err());
        assert!(serde_json::from_str::<Lane>("\"nope\"").is_err());
    }

    #[test]
    fn resolve_lane_formula_three_cases() {
        // §4.2: 显式 Chat + Chat ingress → Native
        assert_eq!(
            resolve_lane(
                Some(UpstreamProtocol::ChatCompletions),
                IngressProtocol::ChatCompletions
            ),
            Lane::Native
        );
        // §4.2: 显式 Chat + Responses ingress → Translated
        assert_eq!(
            resolve_lane(
                Some(UpstreamProtocol::ChatCompletions),
                IngressProtocol::Responses
            ),
            Lane::Translated
        );
        // §4.2: 未显式声明 → 恒 Translated（任意 ingress）
        for ingress in [
            IngressProtocol::ChatCompletions,
            IngressProtocol::Responses,
            IngressProtocol::AnthropicMessages,
        ] {
            assert_eq!(resolve_lane(None, ingress), Lane::Translated);
        }
        // 显式声明但与 ingress 不等
        assert_eq!(
            resolve_lane(
                Some(UpstreamProtocol::Responses),
                IngressProtocol::ChatCompletions
            ),
            Lane::Translated
        );
        assert_eq!(
            resolve_lane(
                Some(UpstreamProtocol::AnthropicMessages),
                IngressProtocol::AnthropicMessages
            ),
            Lane::Native
        );
    }

    #[test]
    fn effective_upstream_prefers_explicit_over_derived() {
        assert_eq!(
            effective_upstream_protocol(
                Some(UpstreamProtocol::Responses),
                UpstreamProtocol::ChatCompletions
            ),
            UpstreamProtocol::Responses
        );
        assert_eq!(
            effective_upstream_protocol(None, UpstreamProtocol::AnthropicMessages),
            UpstreamProtocol::AnthropicMessages
        );
    }

    #[test]
    fn derive_upstream_protocol_by_provider_type() {
        assert_eq!(
            derive_upstream_protocol("openai"),
            UpstreamProtocol::ChatCompletions
        );
        assert_eq!(
            derive_upstream_protocol("anthropic"),
            UpstreamProtocol::AnthropicMessages
        );
        assert_eq!(
            derive_upstream_protocol("unknown"),
            UpstreamProtocol::ChatCompletions
        );
    }

    #[test]
    fn capabilities_default_all_false() {
        let caps = Capabilities::default();
        assert!(!caps.streaming);
        assert!(!caps.reasoning);
        assert!(!caps.tools);
        assert!(!caps.images);
        assert!(!caps.documents);
        assert!(!caps.stateful);
        assert!(!caps.hosted_tools);
        assert!(!caps.background);
        roundtrip(&caps);
    }
}
