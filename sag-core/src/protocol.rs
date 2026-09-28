use std::collections::HashSet;

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

    /// 对应的协议 key（[`ApiKind`]）——lane 判定与 capability keying
    /// 都以协议 key 为准。
    pub fn api_kind(&self) -> ApiKind {
        match self {
            Self::ChatCompletions => ApiKind::ChatCompletions,
            Self::Responses => ApiKind::Responses,
            Self::AnthropicMessages => ApiKind::AnthropicMessages,
        }
    }
}

/// 协议目录 key：`apis` 表的键。三种协议 + `image`（非协议条目）。
///
/// 未知 key 在反序列化层直接报错（枚举严格），与其余类型化配置枚举一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKind {
    ChatCompletions,
    Responses,
    AnthropicMessages,
    /// 非协议条目：不参与 lane 判定，路径唯一来源是 route.`api_endpoint`。
    Image,
}

impl ApiKind {
    /// 三个协议 key 的固定集合（不含 `image`）。
    pub const PROTOCOLS: [ApiKind; 3] = [
        ApiKind::ChatCompletions,
        ApiKind::Responses,
        ApiKind::AnthropicMessages,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
            Self::AnthropicMessages => "anthropic_messages",
            Self::Image => "image",
        }
    }

    /// 是否为协议条目（`image` 返回 `false`）。
    pub fn is_protocol(&self) -> bool {
        !matches!(self, Self::Image)
    }

    /// 迭代三个协议 key（不含 `image`）。
    pub fn protocols() -> impl Iterator<Item = ApiKind> {
        Self::PROTOCOLS.into_iter()
    }

    /// 三个协议 key 的集合（不含 `image`）。
    pub fn protocol_set() -> HashSet<ApiKind> {
        Self::PROTOCOLS.into_iter().collect()
    }
}

/// 协议 key → 固定上游 path（相对 `apis.<key>.base_url`，不含前导 `/`）。
///
/// `image` 不适用：其路径唯一来源是 route.`api_endpoint`，返回 `None`。
pub fn protocol_path(kind: ApiKind) -> Option<&'static str> {
    match kind {
        ApiKind::ChatCompletions => Some("chat/completions"),
        ApiKind::Responses => Some("responses"),
        ApiKind::AnthropicMessages => Some("messages"),
        ApiKind::Image => None,
    }
}

/// 车道判定（全自动，无显式开关）：
///
/// ```text
/// lane = (ingress ∈ 协议 key 集（apis keys ∖ {image}）) ? Native : Translated
/// ```
///
/// `supported` 是 provider `apis` 的 key 集（可含 `image`——非协议 key
/// 不参与判定；空集恒为 Translated）。集合形态不限：`&HashSet<ApiKind>`、
/// `Vec<ApiKind>`、`IndexMap` 的 keys 迭代器等皆可。
pub fn resolve_lane<I, K>(supported: I, ingress: IngressProtocol) -> Lane
where
    I: IntoIterator<Item = K>,
    K: std::borrow::Borrow<ApiKind>,
{
    let native = supported.into_iter().any(|k| {
        let kind = *k.borrow();
        kind.is_protocol() && kind == ingress.api_kind()
    });
    if native {
        Lane::Native
    } else {
        Lane::Translated
    }
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
    fn api_kind_snake_case_roundtrip() {
        for (kind, name) in [
            (ApiKind::ChatCompletions, "chat_completions"),
            (ApiKind::Responses, "responses"),
            (ApiKind::AnthropicMessages, "anthropic_messages"),
            (ApiKind::Image, "image"),
        ] {
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::json!(name),
                "serde name mismatch"
            );
            assert_eq!(kind.as_str(), name, "as_str() mismatches serde name");
            roundtrip(&kind);
        }
    }

    #[test]
    fn api_kind_rejects_unknown_value() {
        assert!(serde_json::from_str::<ApiKind>("\"nope\"").is_err());
        assert!(serde_json::from_str::<ApiKind>("\"protocol\"").is_err());
        assert!(serde_json::from_str::<IngressProtocol>("\"nope\"").is_err());
        assert!(serde_json::from_str::<Lane>("\"nope\"").is_err());
    }

    #[test]
    fn api_kind_is_protocol_and_helpers() {
        assert!(ApiKind::ChatCompletions.is_protocol());
        assert!(ApiKind::Responses.is_protocol());
        assert!(ApiKind::AnthropicMessages.is_protocol());
        assert!(!ApiKind::Image.is_protocol());

        let protocols: Vec<ApiKind> = ApiKind::protocols().collect();
        assert_eq!(protocols, ApiKind::PROTOCOLS.to_vec());
        assert_eq!(protocols.len(), 3);
        assert!(!protocols.contains(&ApiKind::Image));

        let set = ApiKind::protocol_set();
        assert_eq!(set.len(), 3);
        assert!(!set.contains(&ApiKind::Image));

        assert_eq!(
            IngressProtocol::ChatCompletions.api_kind(),
            ApiKind::ChatCompletions
        );
        assert_eq!(IngressProtocol::Responses.api_kind(), ApiKind::Responses);
        assert_eq!(
            IngressProtocol::AnthropicMessages.api_kind(),
            ApiKind::AnthropicMessages
        );
    }

    #[test]
    fn protocol_path_fixed_derivation() {
        assert_eq!(
            protocol_path(ApiKind::ChatCompletions),
            Some("chat/completions")
        );
        assert_eq!(protocol_path(ApiKind::Responses), Some("responses"));
        assert_eq!(protocol_path(ApiKind::AnthropicMessages), Some("messages"));
        // image：非协议，路径唯一来源是 route.api_endpoint。
        assert_eq!(protocol_path(ApiKind::Image), None);
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

    /// lane 全矩阵：3 种 ingress × 协议集（空集 / 单协议 / 同址多协议 /
    /// image-only / 含 image 的混合集 / 全集）。
    ///
    /// 公式：`lane = (ingress ∈ 协议 key 集) ? Native : Translated`。
    #[test]
    fn resolve_lane_full_matrix() {
        let empty: [ApiKind; 0] = [];
        assert_eq!(
            resolve_lane(empty.iter(), IngressProtocol::ChatCompletions),
            Lane::Translated,
            "empty set is always Translated"
        );

        // (supported set, ingress, expected)
        let matrix: Vec<(Vec<ApiKind>, IngressProtocol, Lane)> = {
            let mut v = Vec::new();
            for ingress in [
                IngressProtocol::ChatCompletions,
                IngressProtocol::Responses,
                IngressProtocol::AnthropicMessages,
            ] {
                // 空集 → Translated。
                v.push((vec![], ingress, Lane::Translated));
                // image-only → image 不参与，恒 Translated。
                v.push((vec![ApiKind::Image], ingress, Lane::Translated));
                // 单协议：仅 ingress ∈ set 时 Native。
                for p in ApiKind::protocols() {
                    let expected = if ingress.api_kind() == p {
                        Lane::Native
                    } else {
                        Lane::Translated
                    };
                    v.push((vec![p], ingress, expected));
                }
                // 同址多协议（chat + responses + image 混合集）。
                let multi = vec![ApiKind::ChatCompletions, ApiKind::Responses, ApiKind::Image];
                let expected = if ingress.api_kind() == ApiKind::ChatCompletions
                    || ingress.api_kind() == ApiKind::Responses
                {
                    Lane::Native
                } else {
                    Lane::Translated
                };
                v.push((multi, ingress, expected));
                // 全协议集 → 恒 Native。
                v.push((ApiKind::PROTOCOLS.to_vec(), ingress, Lane::Native));
            }
            v
        };

        assert!(!matrix.is_empty());
        for (supported, ingress, expected) in matrix {
            assert_eq!(
                resolve_lane(supported.iter(), ingress),
                expected,
                "supported={supported:?}, ingress={ingress:?}"
            );
            // 各调用形态等价：拥有所有权的 Vec、集合皆可。
            assert_eq!(
                resolve_lane(supported.clone(), ingress),
                expected,
                "owned Vec form, supported={supported:?}, ingress={ingress:?}"
            );
        }
    }

    /// 集合形态兼容：`HashSet<ApiKind>` 与借用迭代均可直接传入。
    #[test]
    fn resolve_lane_accepts_hashset_and_refs() {
        let set: HashSet<ApiKind> = [ApiKind::Responses, ApiKind::Image].into_iter().collect();
        assert_eq!(resolve_lane(&set, IngressProtocol::Responses), Lane::Native);
        assert_eq!(
            resolve_lane(&set, IngressProtocol::ChatCompletions),
            Lane::Translated
        );
        assert_eq!(
            resolve_lane(set, IngressProtocol::AnthropicMessages),
            Lane::Translated
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
