use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// 入口协议：由挂载路径决定，不写入配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngressProtocol {
    ChatCompletions,
    Responses,
    AnthropicMessages,
    /// `POST /v1/systemone`（TypeSafe System One 决策接口）。协议 key 是不带
    /// 连字符的单词 `systemone`，故显式 rename 覆盖 `rename_all` 的产物。
    #[serde(rename = "systemone")]
    SystemOne,
}

impl IngressProtocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
            Self::AnthropicMessages => "anthropic_messages",
            Self::SystemOne => "systemone",
        }
    }

    /// 对应的协议 key（[`ApiKind`]）——lane 判定与 capability keying
    /// 都以协议 key 为准。
    pub fn api_kind(&self) -> ApiKind {
        match self {
            Self::ChatCompletions => ApiKind::ChatCompletions,
            Self::Responses => ApiKind::Responses,
            Self::AnthropicMessages => ApiKind::AnthropicMessages,
            Self::SystemOne => ApiKind::SystemOne,
        }
    }
}

/// 协议目录 key：`apis` 表的键。四种协议 + `image`（非协议条目）。
///
/// 未知 key 在反序列化层直接报错（枚举严格），与其余类型化配置枚举一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKind {
    ChatCompletions,
    Responses,
    AnthropicMessages,
    /// TypeSafe System One 决策协议（`POST /v1/systemone`）。没有 chat IR 映射，
    /// 也没有 Translated lane：目标 `apis` 里出现即代表能力（presence 即能力）。
    /// 协议 key 是不带连字符的单词 `systemone`，故显式 rename 覆盖 `rename_all`。
    #[serde(rename = "systemone")]
    SystemOne,
    /// 非协议条目：不参与 lane 判定，路径唯一来源是 route.`api_endpoint`。
    Image,
}

impl ApiKind {
    /// 四个协议 key 的固定集合（不含 `image`）。
    pub const PROTOCOLS: [ApiKind; 4] = [
        ApiKind::ChatCompletions,
        ApiKind::Responses,
        ApiKind::AnthropicMessages,
        ApiKind::SystemOne,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
            Self::AnthropicMessages => "anthropic_messages",
            Self::SystemOne => "systemone",
            Self::Image => "image",
        }
    }

    /// 是否为协议条目（`image` 返回 `false`）。
    pub fn is_protocol(&self) -> bool {
        !matches!(self, Self::Image)
    }

    /// 迭代四个协议 key（不含 `image`）。
    pub fn protocols() -> impl Iterator<Item = ApiKind> {
        Self::PROTOCOLS.into_iter()
    }

    /// 四个协议 key 的集合（不含 `image`）。
    pub fn protocol_set() -> HashSet<ApiKind> {
        Self::PROTOCOLS.into_iter().collect()
    }
}

/// 客户端可见流的终点帧判定（见 `.plan/stream-terminal-frame-race-fix.md`）。
///
/// 输入是**编码后**的完整 SSE 帧字节（可能含 `event:` 行 + 一行或多行
/// `data:`），命中下列任一形态即视为客户端可见的终点帧：
///
/// - Chat：任意一行 trim 后以 `data: [DONE]` / `data:[DONE]` 开头；
/// - Anthropic：含 `"type":"message_stop"` 或 `"type": "message_stop"`，或任意
///   一行 trim 后以 `event: message_stop` / `event:message_stop` 开头；
/// - Responses：含 `"type":"response.completed"` /
///   `"type": "response.completed"` / `"type":"response.incomplete"` /
///   `"type": "response.incomplete"` / `"type":"response.cancelled"` /
///   `"type": "response.cancelled"`，或任意一行 trim 后以
///   `event: response.completed` / `event:response.completed` /
///   `event: response.incomplete` / `event:response.incomplete` /
///   `event: response.cancelled` / `event:response.cancelled` 开头。
///
/// **三协议形态并集匹配，不按 ingress（[`ApiKind`]）分派**：跨协议组合下
/// 客户端收到的帧形态由 upstream / lane 决定（例如 Chat ingress + Responses
/// upstream 会 Raw 透传 `event: response.completed`），按 ingress 收窄会漏判。
///
/// 匹配刻意采用行锚定前缀与带引号的 `"type":"..."` 形态，而不是裸子串：
/// 帧里的模型文本内容可以原样出现 `data: [DONE]`、`message_stop`、
/// `response.completed`、`event: response.completed`（这些字符在 JSON 字符串值里
/// 无需转义），裸子串匹配会把普通内容 delta 误判为终点。因此**所有行形态**标记
/// （`data:` 行、`event:` 行）都必须逐行锚定到行首（trim 前导空白后 `starts_with`）
/// —— 内容 delta 里的伪造字样永远位于 `data: {...}` 这一行的 JSON 值中间，不可能
/// 出现在行首。而 `"type":"..."` 的引号形若出现在字符串值内部，必然被转义成
/// `\"type\":\"...`，故仍可用 `contains` 匹配。
///
/// `response.incomplete` 同样算终点：它是 Responses 流的正常收尾之一（生成被
/// 截断），usage 侧信道已按 completed / incomplete 提取（见
/// `sag-providers/src/openai.rs` 的 `extract_side_channel`）。
///
/// `response.cancelled` 同样算终点：`response.completed` 的 `status` 已有
/// `cancelled` 变体且已被现有判定命中，原始 `response.cancelled` 事件类型属于同一
/// 终态（取消），必须对齐处理，否则取消流会被漏判为「未终止」而走空流 / 悬挂路径。
///
/// `response.failed` **不**算终点帧：它的 Error 帧走 502 路径，错误分类优先于
/// 终点帧标志。
///
/// 与 `sag-dispatch/src/dispatch/chat/empty.rs` 的 `RawVerdict` 共享同一份
/// 终点知识（那边按 JSON 解析后的 `type` 判定，这边是字节级快速判定），两侧各有
/// 测试锚定，避免静默漂移。
pub fn is_client_terminal_frame(frame: &[u8]) -> bool {
    // 行形态标记（Chat / Anthropic / Responses 并集）：必须行锚定前缀匹配 ——
    // 文本内容里可能原样出现 `data: [DONE]`、`event: response.completed` 等字样，
    // 子串匹配会误报。无空格 / 带空格两种变体都覆盖（不同 upstream 编码不一）。
    const LINE_PREFIXES: [&[u8]; 10] = [
        b"data: [DONE]",
        b"data:[DONE]",
        b"event: message_stop",
        b"event:message_stop",
        b"event: response.completed",
        b"event:response.completed",
        b"event: response.incomplete",
        b"event:response.incomplete",
        b"event: response.cancelled",
        b"event:response.cancelled",
    ];
    if has_line_prefix(frame, &LINE_PREFIXES) {
        return true;
    }

    // Anthropic 终点：紧凑 / 带空格两种 JSON 形态。禁止裸 `message_stop`
    // 子串（文本内容可含该单词）。
    const ANTHROPIC_MARKERS: [&[u8]; 2] =
        [br#""type":"message_stop""#, br#""type": "message_stop""#];
    if ANTHROPIC_MARKERS
        .iter()
        .any(|marker| contains_bytes(frame, marker))
    {
        return true;
    }

    // Responses 终点：`type` 字段形，completed / incomplete / cancelled 都算流终点
    // （`response.failed` 排除，见函数文档）。禁止裸 `response.completed` 子串
    // （理由同上）；引号形在 JSON 字符串值内必被转义，因此 `contains` 安全。
    const RESPONSES_MARKERS: [&[u8]; 6] = [
        br#""type":"response.completed""#,
        br#""type": "response.completed""#,
        br#""type":"response.incomplete""#,
        br#""type": "response.incomplete""#,
        br#""type":"response.cancelled""#,
        br#""type": "response.cancelled""#,
    ];
    RESPONSES_MARKERS
        .iter()
        .any(|marker| contains_bytes(frame, marker))
}

/// 是否存在某一行（trim 前导 ASCII 空白后）以给定前缀之一开头。
fn has_line_prefix(frame: &[u8], prefixes: &[&[u8]]) -> bool {
    frame.split(|b| *b == b'\n').any(|line| {
        let line = line.trim_ascii_start();
        prefixes.iter().any(|prefix| line.starts_with(prefix))
    })
}

/// 字节子串查找（`[u8]` 没有 `contains`；不解析 JSON，热路径保持 O(n)）。
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// 协议 key → 固定上游 path（相对 `apis.<key>.base_url`，不含前导 `/`）。
///
/// `image` 不适用：其路径唯一来源是 route.`api_endpoint`，返回 `None`。
pub fn protocol_path(kind: ApiKind) -> Option<&'static str> {
    match kind {
        ApiKind::ChatCompletions => Some("chat/completions"),
        ApiKind::Responses => Some("responses"),
        ApiKind::AnthropicMessages => Some("messages"),
        ApiKind::SystemOne => Some("systemone"),
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
        assert_eq!(
            serde_json::to_value(IngressProtocol::SystemOne).unwrap(),
            serde_json::json!("systemone")
        );
        roundtrip(&IngressProtocol::ChatCompletions);
        roundtrip(&IngressProtocol::Responses);
        roundtrip(&IngressProtocol::AnthropicMessages);
        roundtrip(&IngressProtocol::SystemOne);
        assert_eq!(IngressProtocol::Responses.as_str(), "responses");
        // 协议 key 是单词形，`rename_all = "snake_case"` 会给 `system_one`。
        assert_eq!(IngressProtocol::SystemOne.as_str(), "systemone");
    }

    #[test]
    fn api_kind_snake_case_roundtrip() {
        for (kind, name) in [
            (ApiKind::ChatCompletions, "chat_completions"),
            (ApiKind::Responses, "responses"),
            (ApiKind::AnthropicMessages, "anthropic_messages"),
            (ApiKind::SystemOne, "systemone"),
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
        // 协议 key 是单词 `systemone`；`snake_case` 产物 `system_one` 不是合法值。
        assert!(serde_json::from_str::<ApiKind>("\"system_one\"").is_err());
        assert!(serde_json::from_str::<IngressProtocol>("\"system_one\"").is_err());
        assert!(serde_json::from_str::<IngressProtocol>("\"nope\"").is_err());
        assert!(serde_json::from_str::<Lane>("\"nope\"").is_err());
    }

    #[test]
    fn api_kind_is_protocol_and_helpers() {
        assert!(ApiKind::ChatCompletions.is_protocol());
        assert!(ApiKind::Responses.is_protocol());
        assert!(ApiKind::AnthropicMessages.is_protocol());
        assert!(ApiKind::SystemOne.is_protocol());
        assert!(!ApiKind::Image.is_protocol());

        let protocols: Vec<ApiKind> = ApiKind::protocols().collect();
        assert_eq!(protocols, ApiKind::PROTOCOLS.to_vec());
        assert_eq!(protocols.len(), 4);
        assert!(protocols.contains(&ApiKind::SystemOne));
        assert!(!protocols.contains(&ApiKind::Image));

        let set = ApiKind::protocol_set();
        assert_eq!(set.len(), 4);
        assert!(set.contains(&ApiKind::SystemOne));
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
        assert_eq!(IngressProtocol::SystemOne.api_kind(), ApiKind::SystemOne);
    }

    #[test]
    fn protocol_path_fixed_derivation() {
        assert_eq!(
            protocol_path(ApiKind::ChatCompletions),
            Some("chat/completions")
        );
        assert_eq!(protocol_path(ApiKind::Responses), Some("responses"));
        assert_eq!(protocol_path(ApiKind::AnthropicMessages), Some("messages"));
        assert_eq!(protocol_path(ApiKind::SystemOne), Some("systemone"));
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

    /// lane 全矩阵：4 种 ingress × 协议集（空集 / 单协议 / 同址多协议 /
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
                IngressProtocol::SystemOne,
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
    fn client_terminal_frame_detects_chat_done() {
        assert!(is_client_terminal_frame(b"data: [DONE]\n\n"));
        // 无空格变体（部分 upstream 不写空格）。
        assert!(is_client_terminal_frame(b"data:[DONE]\n\n"));
        // 带前导空白 / CRLF 的帧仍然命中（行锚定 trim 后判定）。
        assert!(is_client_terminal_frame(b"\ndata: [DONE]\r\n\r\n"));
        assert!(is_client_terminal_frame(b"event: done\ndata: [DONE]\n\n"));
    }

    #[test]
    fn client_terminal_frame_detects_anthropic_message_stop() {
        assert!(is_client_terminal_frame(
            b"data: {\"type\":\"message_stop\"}\n\n"
        ));
        // 带空格 JSON 形态。
        assert!(is_client_terminal_frame(
            b"data: {\"type\": \"message_stop\"}"
        ));
        // `event:` 行 + `data:` 行。
        assert!(is_client_terminal_frame(
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        ));
        // 仅 `event:` 行形态（无空格 / 带空格，data 行非终点）。
        assert!(is_client_terminal_frame(
            b"event: message_stop\ndata: {}\n\n"
        ));
        assert!(is_client_terminal_frame(
            b"event:message_stop\ndata: {}\n\n"
        ));
    }

    #[test]
    fn client_terminal_frame_detects_responses_terminals() {
        assert!(is_client_terminal_frame(
            b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\"}}\n\n"
        ));
        assert!(is_client_terminal_frame(
            b"data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"r1\"}}\n\n"
        ));
        // 仅 event 行形态（含无空格变体）。
        for frame in [
            &b"event: response.completed\n\n"[..],
            &b"event:response.completed\n\n"[..],
            &b"event: response.incomplete\n\n"[..],
            &b"event:response.incomplete\n\n"[..],
            // 仅 type 字段形态（含带空格变体）。
            &b"data: {\"type\": \"response.completed\"}\n\n"[..],
            &b"data: {\"type\": \"response.incomplete\"}\n\n"[..],
        ] {
            assert!(
                is_client_terminal_frame(frame),
                "expected terminal: {}",
                String::from_utf8_lossy(frame)
            );
        }
    }

    /// `response.cancelled` 是 Responses 的另一条终态路径（取消），形态与
    /// completed / incomplete 对齐。
    #[test]
    fn client_terminal_frame_detects_responses_cancelled() {
        for frame in [
            &b"event: response.cancelled\ndata: {\"type\":\"response.cancelled\"}\n\n"[..],
            &b"data: {\"type\":\"response.cancelled\"}\n\n"[..],
            &b"data: {\"type\": \"response.cancelled\"}\n\n"[..],
            &b"event: response.cancelled\n\n"[..],
            &b"event:response.cancelled\n\n"[..],
        ] {
            assert!(
                is_client_terminal_frame(frame),
                "expected terminal: {}",
                String::from_utf8_lossy(frame)
            );
        }
    }

    /// 防误报锚定：内容帧、非终点控制帧、空帧一律不命中。
    #[test]
    fn client_terminal_frame_ignores_non_terminal_frames() {
        for frame in [
            // 普通 chat delta。
            &b"data: {\"choices\":[{\"delta\":{\"text\":\"hi\"}}]}\n\n"[..],
            // 文本内容里原样出现终点字样（裸子串匹配会误报）。
            &b"data: {\"choices\":[{\"delta\":{\"text\":\"stream ends with data: [DONE] and message_stop\"}}]}\n\n"[..],
            &b"data: {\"choices\":[{\"delta\":{\"text\":\"response.completed\"}}]}\n\n"[..],
            // 伪造 `event:` 行形态（行锚定前缀防伪造，评审 Major-1）。
            &b"data: {\"delta\":\"event: response.completed\"}\n\n"[..],
            &b"data: {\"delta\":\"event:response.incomplete\"}\n\n"[..],
            &b"data: {\"delta\":\"event: response.cancelled\"}\n\n"[..],
            &b"data: {\"delta\":\"event: message_stop\"}\n\n"[..],
            &b"data: {\"choices\":[{\"delta\":{\"text\":\"ends with data: [DONE]\"}}]}\n\n"[..],
            // 非终点 Anthropic 控制帧。
            &b"data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":null}}\n\n"[..],
            // `response.failed` 不算终点（Error 帧走 502，错误分类优先）。
            &b"event: response.failed\ndata: {\"type\":\"response.failed\"}\n\n"[..],
            // 空帧 / 纯注释帧。
            &b""[..],
            &b": ping\n\n"[..],
        ] {
            assert!(
                !is_client_terminal_frame(frame),
                "expected non-terminal: {}",
                String::from_utf8_lossy(frame)
            );
        }
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
