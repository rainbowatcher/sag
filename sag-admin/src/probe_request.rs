//! 各协议「最小探测请求」的单一来源。
//!
//! 两条 admin 端点（`probe-protocols`、`model/test`）各自向上游发一条最小
//! 请求，body 形状此前在两处手写、易漂移。这里集中这份协议知识：每个
//! [`ApiKind`] 对应的最小 body 只有一处定义。
//!
//! 注：本模块由 `probe-protocols` 与 `model/test` 两条端点共用。

use sag_config::ApiKind;
use sag_core::{ChatCompletionRequest, ChatMessage};
use serde_json::json;

/// 一条最小探测请求。三协议显式，无 catch-all——新增 ApiKind 时必须在此显式处理。
pub(crate) enum MinimalRequest {
    /// chat_completions：IR 形态。raw 路径序列化为 JSON，adapter 路径直接喂 provider.chat()
    Chat(Box<ChatCompletionRequest>),
    /// anthropic_messages：Anthropic 形状 body
    Anthropic(serde_json::Value),
    /// responses：Responses 形状 body
    Responses(serde_json::Value),
}

/// 构造某一协议的最小探测请求。
///
/// image（非协议）→ None。调用方必须显式处理 None（返回 400）。
pub(crate) fn minimal_request(kind: ApiKind, model: &str) -> Option<MinimalRequest> {
    match kind {
        ApiKind::ChatCompletions => Some(MinimalRequest::Chat(Box::new(ChatCompletionRequest {
            model: model.to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: json!("hi"),
                name: None,
                extra: serde_json::Map::new(),
            }],
            // 输出 token 是最贵的部分，钉到最小可接受值。
            max_completion_tokens: Some(1),
            ..Default::default()
        }))),
        ApiKind::AnthropicMessages => Some(MinimalRequest::Anthropic(json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1,
        }))),
        ApiKind::Responses => Some(MinimalRequest::Responses(json!({
            "model": model,
            "input": "hi",
            // 部分厂商强制 16 为下限。
            "max_output_tokens": 16,
        }))),
        ApiKind::Image => None,
    }
}

/// 测试/探测请求的 URL：base_url 去尾斜杠后拼接协议路径；Image → None。
pub(crate) fn request_url(base_url: &str, kind: ApiKind) -> Option<String> {
    sag_core::protocol_path(kind).map(|p| format!("{}/{}", base_url.trim_end_matches('/'), p))
}

impl MinimalRequest {
    /// raw 路径（probe、Phase 1 的 model_test）用的 JSON body。
    pub(crate) fn to_body(&self) -> serde_json::Value {
        match self {
            Self::Chat(ir) => serde_json::to_value(ir).expect("ChatCompletionRequest 总能序列化"),
            Self::Anthropic(body) | Self::Responses(body) => body.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_request_chat_uses_max_completion_tokens() {
        let req = minimal_request(ApiKind::ChatCompletions, "gpt-4o").unwrap();
        let body = req.to_body();
        assert_eq!(
            body,
            json!({
                "model": "gpt-4o",
                "messages": [{"role": "user", "content": "hi"}],
                "max_completion_tokens": 1,
            })
        );
        assert!(
            body.get("max_tokens").is_none(),
            "chat IR must not emit max_tokens"
        );
    }

    #[test]
    fn minimal_request_anthropic_uses_max_tokens() {
        let req = minimal_request(ApiKind::AnthropicMessages, "claude-sonnet-4").unwrap();
        let body = req.to_body();
        assert_eq!(
            body,
            json!({
                "model": "claude-sonnet-4",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 1,
            })
        );
    }

    #[test]
    fn minimal_request_responses_uses_input_and_max_output_tokens() {
        let req = minimal_request(ApiKind::Responses, "gpt-5").unwrap();
        let body = req.to_body();
        assert_eq!(
            body,
            json!({
                "model": "gpt-5",
                "input": "hi",
                "max_output_tokens": 16,
            })
        );
    }

    #[test]
    fn minimal_request_image_is_none() {
        assert!(minimal_request(ApiKind::Image, "dall-e-3").is_none());
    }

    #[test]
    fn request_url_derives_from_protocol_path() {
        assert_eq!(
            request_url("http://h/v1", ApiKind::ChatCompletions).as_deref(),
            Some("http://h/v1/chat/completions")
        );
        assert_eq!(
            request_url("http://h/v1", ApiKind::Responses).as_deref(),
            Some("http://h/v1/responses")
        );
        assert_eq!(
            request_url("http://h/v1", ApiKind::AnthropicMessages).as_deref(),
            Some("http://h/v1/messages")
        );
        assert_eq!(request_url("http://h/v1", ApiKind::Image), None);
        // 尾斜杠 base_url 被去重后再拼接。
        assert_eq!(
            request_url("http://h/v1/", ApiKind::ChatCompletions).as_deref(),
            Some("http://h/v1/chat/completions")
        );
    }
}
