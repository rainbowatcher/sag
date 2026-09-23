//! Capability pre-check skeleton (M3): hard-requirement filtering for
//! `resolve_route`, after `filter_enabled_targets` and before ordering.
//!
//! Conservative: unknown provider/protocol combinations only claim a
//! minimal capability set, so exotic requests prefer targets that clearly
//! declare support. Chat ingress never requires stateful / hosted_tools /
//! background (those are Responses-only hard rejects handled later).

use sag_config::{ProviderConfig, RouteTarget};
use sag_core::{
    Capabilities, ChatCompletionRequest, UpstreamProtocol, derive_upstream_protocol,
    effective_upstream_protocol,
};
use tracing::info;

/// Extract hard capability requirements from a Chat-completion request.
///
/// Chat path: streaming / tools / images / documents / reasoning only;
/// stateful / hosted_tools / background stay `false`.
pub fn request_hard_requirements(req: &ChatCompletionRequest) -> Capabilities {
    Capabilities {
        streaming: req.stream.unwrap_or(false),
        reasoning: req.reasoning_effort.is_some(),
        tools: req
            .tools
            .as_ref()
            .and_then(|t| t.as_array())
            .is_some_and(|arr| !arr.is_empty()),
        images: message_content_has_type(req, "image_url"),
        documents: message_content_has_type(req, "file")
            || message_content_has_type(req, "document"),
        stateful: false,
        hosted_tools: false,
        background: false,
    }
}

fn message_content_has_type(req: &ChatCompletionRequest, type_name: &str) -> bool {
    req.messages.iter().any(|m| match &m.content {
        serde_json::Value::Array(parts) => parts.iter().any(|p| {
            p.get("type").and_then(|t| t.as_str()) == Some(type_name)
                || (type_name == "image_url" && p.get("image_url").is_some())
        }),
        // Object-shaped content (uncommon) — check type key directly.
        serde_json::Value::Object(o) => o.get("type").and_then(|t| t.as_str()) == Some(type_name),
        _ => false,
    })
}

/// Conservative target capability table keyed by provider_type + effective
/// upstream protocol. Unknown combinations only claim `streaming`.
pub fn target_capabilities(
    provider_type: &str,
    effective_upstream: UpstreamProtocol,
) -> Capabilities {
    match (provider_type, effective_upstream) {
        ("openai", UpstreamProtocol::ChatCompletions) => Capabilities {
            streaming: true,
            reasoning: true,
            tools: true,
            images: true,
            // Chat file parts are passable on OpenAI-compatible chat
            // endpoints — do not hard-drop them at pre-check.
            documents: true,
            stateful: false,
            hosted_tools: false,
            background: false,
        },
        ("anthropic", _) => Capabilities {
            streaming: true,
            reasoning: true,
            tools: true,
            images: true,
            documents: true,
            stateful: false,
            hosted_tools: false,
            background: false,
        },
        // OpenAI-compatible Responses / unknown combos: conservative.
        (_, UpstreamProtocol::Responses) => Capabilities {
            streaming: true,
            reasoning: true,
            tools: true,
            images: true,
            documents: false,
            stateful: false,
            hosted_tools: false,
            background: false,
        },
        _ => Capabilities {
            streaming: true,
            reasoning: false,
            tools: false,
            images: false,
            documents: false,
            stateful: false,
            hosted_tools: false,
            background: false,
        },
    }
}

fn missing_capability(required: Capabilities, provided: Capabilities) -> Option<&'static str> {
    if required.streaming && !provided.streaming {
        return Some("streaming");
    }
    if required.tools && !provided.tools {
        return Some("tools");
    }
    if required.images && !provided.images {
        return Some("images");
    }
    if required.documents && !provided.documents {
        return Some("documents");
    }
    if required.reasoning && !provided.reasoning {
        return Some("reasoning");
    }
    // Chat path never sets stateful/hosted_tools/background; check anyway
    // so future non-chat ingress reuses the same helper.
    if required.stateful && !provided.stateful {
        return Some("stateful");
    }
    if required.hosted_tools && !provided.hosted_tools {
        return Some("hosted_tools");
    }
    if required.background && !provided.background {
        return Some("background");
    }
    None
}

/// Drop targets whose capabilities cannot satisfy the request's hard
/// requirements. Pushes `"{provider}/{model}: lacks capability X"` into
/// `errors` for each drop (same style as disabled-provider filtering).
///
/// Call **after** [`super::resolve::filter_enabled_targets`].
pub fn filter_capable_targets(
    req: &ChatCompletionRequest,
    providers: &std::collections::HashMap<String, ProviderConfig>,
    targets: Vec<RouteTarget>,
    errors: &mut Vec<String>,
) -> Vec<RouteTarget> {
    let required = request_hard_requirements(req);
    // Fast path: no hard requirements → no filtering.
    if !(required.streaming
        || required.tools
        || required.images
        || required.documents
        || required.reasoning
        || required.stateful
        || required.hosted_tools
        || required.background)
    {
        return targets;
    }

    targets
        .into_iter()
        .filter(|t| {
            let pc = providers.get(&t.provider);
            let provider_type = pc.map(|p| p.provider_type.as_str()).unwrap_or("openai");
            let explicit = t
                .upstream_protocol
                .or_else(|| pc.and_then(|p| p.upstream_protocol));
            // Explicitness only matters for lane; for capabilities we use
            // the effective protocol (explicit else derived).
            let derived = derive_upstream_protocol(provider_type);
            let effective = effective_upstream_protocol(explicit, derived);
            let provided = target_capabilities(provider_type, effective);
            if let Some(cap) = missing_capability(required, provided) {
                let model = t.real_model.as_deref().unwrap_or(&req.model);
                info!(
                    provider = %t.provider,
                    model = %model,
                    capability = %cap,
                    "skipping target lacking required capability"
                );
                errors.push(format!("{}/{model}: lacks capability {cap}", t.provider));
                return false;
            }
            true
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sag_config::{KeyStrategy, ProviderType, ProxyMode};
    use std::collections::HashMap;

    fn provider(pt: ProviderType) -> ProviderConfig {
        ProviderConfig {
            provider_type: pt,
            api_keys: vec!["sk".into()],
            key_strategy: KeyStrategy::FillFirst,
            proxy_mode: ProxyMode::default(),
            base_url: "https://example.com".into(),
            models_endpoint: "models".into(),
            timeout_secs: 60,
            upstream_protocol: None,
            ..ProviderConfig::default()
        }
    }

    fn target(provider: &str) -> RouteTarget {
        RouteTarget {
            provider: provider.into(),
            ..RouteTarget::default()
        }
    }

    fn req(json: serde_json::Value) -> ChatCompletionRequest {
        serde_json::from_value(json).expect("valid request")
    }

    #[test]
    fn plain_request_has_no_hard_requirements() {
        let r = req(serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}]
        }));
        let caps = request_hard_requirements(&r);
        assert!(!caps.streaming && !caps.tools && !caps.images);
        assert!(!caps.documents && !caps.reasoning);
        let mut providers = HashMap::new();
        providers.insert("p".into(), provider(ProviderType::Anthropic));
        let mut errors = Vec::new();
        let out = filter_capable_targets(&r, &providers, vec![target("p")], &mut errors);
        assert_eq!(out.len(), 1, "no requirements → no filtering");
        assert!(errors.is_empty());
    }

    #[test]
    fn image_request_drops_target_without_images() {
        let r = req(serde_json::json!({
            "model": "m",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "what"},
                    {"type": "image_url", "image_url": {"url": "http://x/y.png"}}
                ]
            }]
        }));
        assert!(request_hard_requirements(&r).images);

        let mut providers = HashMap::new();
        // Unknown provider type → conservative caps (no images).
        providers.insert(
            "weird".into(),
            ProviderConfig {
                provider_type: ProviderType::OpenAI,
                upstream_protocol: Some(UpstreamProtocol::AnthropicMessages),
                ..provider(ProviderType::OpenAI)
            },
        );
        // Force unknown combo via wrong type string: use a provider with
        // OpenAI type but we'll simulate via target_capabilities unit below.
        // Here: OpenAI+chat has images=true, so craft one without images by
        // using openai + explicit anthropic_messages (unknown combo → no images).
        let mut errors = Vec::new();
        let out = filter_capable_targets(&r, &providers, vec![target("weird")], &mut errors);
        assert!(out.is_empty(), "target without images must be dropped");
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].contains("lacks capability images"),
            "got: {}",
            errors[0]
        );

        // OpenAI + chat_completions keeps images.
        let mut providers2 = HashMap::new();
        providers2.insert("ok".into(), provider(ProviderType::OpenAI));
        let mut errors2 = Vec::new();
        let out2 = filter_capable_targets(&r, &providers2, vec![target("ok")], &mut errors2);
        assert_eq!(out2.len(), 1);
        assert!(errors2.is_empty());
    }

    #[test]
    fn reasoning_request_drops_target_without_reasoning() {
        let r = req(serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "high"
        }));
        assert!(request_hard_requirements(&r).reasoning);

        // Unknown provider/protocol combo → no reasoning.
        let mut providers = HashMap::new();
        providers.insert(
            "unk".into(),
            ProviderConfig {
                provider_type: ProviderType::OpenAI,
                upstream_protocol: Some(UpstreamProtocol::AnthropicMessages),
                ..provider(ProviderType::OpenAI)
            },
        );
        let mut errors = Vec::new();
        let out = filter_capable_targets(&r, &providers, vec![target("unk")], &mut errors);
        assert!(out.is_empty());
        assert!(errors[0].contains("lacks capability reasoning"));

        let mut providers2 = HashMap::new();
        providers2.insert("oa".into(), provider(ProviderType::OpenAI));
        let mut errors2 = Vec::new();
        let out2 = filter_capable_targets(&r, &providers2, vec![target("oa")], &mut errors2);
        assert_eq!(out2.len(), 1, "openai+chat has reasoning");
    }

    #[test]
    fn target_capabilities_table_conservative_defaults() {
        let openai_chat = target_capabilities("openai", UpstreamProtocol::ChatCompletions);
        assert!(openai_chat.streaming && openai_chat.tools && openai_chat.images);
        assert!(
            openai_chat.documents,
            "openai+chat must claim documents so file parts are not hard-dropped"
        );

        let anthropic = target_capabilities("anthropic", UpstreamProtocol::AnthropicMessages);
        assert!(anthropic.documents && anthropic.reasoning);

        let unknown = target_capabilities("vllm", UpstreamProtocol::AnthropicMessages);
        // Unknown type with anthropic protocol still falls into `_` arm…
        // provider_type is not "anthropic"/"openai" → last arm unless Responses.
        assert!(unknown.streaming);
        // openai + responses is allowed (Responses arm).
        let oa_resp = target_capabilities("openai", UpstreamProtocol::Responses);
        assert!(oa_resp.streaming && oa_resp.tools);
    }

    #[test]
    fn file_request_keeps_openai_chat_target() {
        // S1: a Chat `file` part must not hard-drop every openai+chat target.
        let r = req(serde_json::json!({
            "model": "m",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "summarize"},
                    {"type": "file", "file": {"file_data": "data:application/pdf;base64,AA=="}}
                ]
            }]
        }));
        assert!(request_hard_requirements(&r).documents);

        let mut providers = HashMap::new();
        providers.insert("oa".into(), provider(ProviderType::OpenAI));
        let mut errors = Vec::new();
        let out = filter_capable_targets(&r, &providers, vec![target("oa")], &mut errors);
        assert_eq!(
            out.len(),
            1,
            "openai-only route must keep file-bearing requests; errors: {errors:?}"
        );
        assert!(errors.is_empty());
    }

    #[test]
    fn streaming_and_tools_requirements_filter() {
        let r = req(serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {}}}]
        }));
        let caps = request_hard_requirements(&r);
        assert!(caps.streaming && caps.tools);

        let mut providers = HashMap::new();
        providers.insert(
            "bare".into(),
            ProviderConfig {
                provider_type: ProviderType::OpenAI,
                upstream_protocol: Some(UpstreamProtocol::AnthropicMessages),
                ..provider(ProviderType::OpenAI)
            },
        );
        // openai + anthropic_messages → unknown → only streaming, no tools
        let mut errors = Vec::new();
        let out = filter_capable_targets(&r, &providers, vec![target("bare")], &mut errors);
        assert!(out.is_empty());
        assert!(errors[0].contains("lacks capability tools"));
    }
}
