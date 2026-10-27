//! Capability pre-check skeleton (M3): hard-requirement filtering for
//! `resolve_route`, after `filter_enabled_targets` and before ordering.
//!
//! Conservative: unknown provider/protocol combinations only claim a
//! minimal capability set, so exotic requests prefer targets that clearly
//! declare support. Chat ingress never requires stateful / hosted_tools /
//! background (those are Responses-only hard rejects handled later).

use sag_config::{ProviderConfig, RouteTarget};
use sag_core::{ApiKind, Capabilities, ChatCompletionRequest, IngressProtocol, Lane};
use tracing::info;

/// The protocol this request actually talks to upstream for a provider
/// whose `apis` keys are `apis_keys` and whose translated-lane protocol is
/// `fallback`:
///
/// - native lane → the ingress protocol;
/// - translated lane → `fallback` (config validation guarantees it is one
///   of the chat-IR protocol keys), or the provider's single chat-IR
///   protocol key when `fallback` was left implicit;
/// - undetermined (no provider / no protocol key) → `ChatCompletions`.
///
/// `SystemOne` is never a fallback candidate: it is a decision protocol
/// with no chat IR and no translated lane, so an explicit
/// `fallback = "systemone"` that slipped past config validation is ignored
/// rather than trusted as the chat translation protocol.
pub(crate) fn used_protocol(
    lane: Lane,
    ingress: IngressProtocol,
    apis_keys: &[ApiKind],
    fallback: Option<ApiKind>,
) -> ApiKind {
    if lane == Lane::Native {
        return ingress.api_kind();
    }
    if let Some(fb) = fallback
        && fb != ApiKind::SystemOne
    {
        return fb;
    }
    let mut protocols = apis_keys
        .iter()
        .copied()
        .filter(|k| k.is_protocol() && *k != ApiKind::SystemOne);
    match (protocols.next(), protocols.next()) {
        (Some(only), None) => only,
        _ => ApiKind::ChatCompletions,
    }
}

/// Extract hard capability requirements from a Chat-completion request.
///
/// - `hosted_tools`: set when any tool has `type == "web_search"` (or
///   `type == "hosted"` with `name == "web_search"`).
/// - `stateful`: set when `previous_response_id` rides in `extra`
///   (Responses ingress synthesis) or `store == Some(true)`.
/// - `background`: always `false` (background execution is globally 422).
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
        // Stateful: previous_response_id rides in Chat extra (Responses
        // ingress synthesis) or store=true on the typed field.
        stateful: req.extra.contains_key("previous_response_id") || req.store == Some(true),
        hosted_tools: req
            .tools
            .as_ref()
            .and_then(|t| t.as_array())
            .is_some_and(|arr| {
                arr.iter().any(|t| {
                    let ty = t.get("type").and_then(|x| x.as_str()).unwrap_or("");
                    ty == "web_search"
                        || (ty == "hosted"
                            && t.get("name").and_then(|n| n.as_str()) == Some("web_search"))
                })
            }),
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

/// Conservative target capability table keyed purely by the actually-used
/// protocol ([`ApiKind`]). Unknown / non-protocol entries only claim
/// `streaming`.
pub fn target_capabilities(kind: ApiKind) -> Capabilities {
    match kind {
        // Chat Completions: file parts are passable on OpenAI-compatible
        // chat endpoints — do not hard-drop them at pre-check. Chat has no
        // hosted `web_search` tool type (only `web_search_options` on the
        // request body).
        ApiKind::ChatCompletions => Capabilities {
            streaming: true,
            reasoning: true,
            tools: true,
            images: true,
            documents: true,
            stateful: false,
            hosted_tools: false,
            background: false,
        },
        // Responses: native Responses supports hosted tools (web_search)
        // and stateful fields; its input schema takes no file parts.
        ApiKind::Responses => Capabilities {
            streaming: true,
            reasoning: true,
            tools: true,
            images: true,
            documents: false,
            stateful: true,
            hosted_tools: true,
            background: false,
        },
        // Anthropic Messages: supports the hosted `web_search` tool (M8).
        ApiKind::AnthropicMessages => Capabilities {
            streaming: true,
            reasoning: true,
            tools: true,
            images: true,
            documents: true,
            stateful: false,
            hosted_tools: true,
            background: false,
        },
        // `image` is a non-protocol catalog entry with no chat semantics —
        // claim the minimal set so nothing is assumed supported.
        ApiKind::Image => Capabilities {
            streaming: true,
            reasoning: false,
            tools: false,
            images: false,
            documents: false,
            stateful: false,
            hosted_tools: false,
            background: false,
        },
        // SystemOne 是决策协议，没有 chat IR / 流式 / 工具语义 —— 一律不声明，
        // 这样带 hard requirement 的 chat 请求永远不会落到 SystemOne-only 目标。
        ApiKind::SystemOne => Capabilities {
            streaming: false,
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
/// requirements. Filtering is keyed by each target's **actually-used
/// protocol** (native lane → ingress, translated lane → provider
/// `fallback`). Pushes `"{provider}/{model}: lacks capability X"` into
/// `errors` for each drop (same style as disabled-provider filtering).
///
/// A **presence** check runs first and unconditionally (no hard requirement
/// needed): the actually-used protocol must be a declared `apis` node of the
/// provider. Since `apis.systemone` alone became a valid config shape, a
/// provider may declare no chat-IR node at all — `used_protocol` then falls
/// back to `ChatCompletions`, which every chat target used to "have" by
/// construction. Without this check such a target reaches
/// `provider.chat()`, fails with `BadRequest` ("does not support chat
/// completions") and is misclassified by `is_client_caused_4xx` as a
/// client-caused terminal (400, no failover, no attempt row) — a config
/// mistake blamed on the caller. Symmetric to the SystemOne-side
/// [`super::resolve::filter_systemone_targets`].
///
/// Call **after** [`super::resolve::filter_enabled_targets`].
pub fn filter_capable_targets(
    req: &ChatCompletionRequest,
    providers: &std::collections::HashMap<String, ProviderConfig>,
    targets: Vec<RouteTarget>,
    ingress: IngressProtocol,
    errors: &mut Vec<String>,
) -> Vec<RouteTarget> {
    let required = request_hard_requirements(req);
    // No hard requirement → only the presence check below can drop a target.
    let has_hard_requirements = required.streaming
        || required.tools
        || required.images
        || required.documents
        || required.reasoning
        || required.stateful
        || required.hosted_tools
        || required.background;

    targets
        .into_iter()
        .filter(|t| {
            let pc = providers.get(&t.provider);
            let apis_keys: Vec<ApiKind> = pc
                .map(|p| p.apis.keys().copied().collect())
                .unwrap_or_default();
            let lane = sag_core::resolve_lane(apis_keys.iter(), ingress);
            let used = used_protocol(lane, ingress, &apis_keys, pc.and_then(|p| p.fallback));
            // Presence check (see the doc comment above): only for known
            // providers — an unknown name is reported later by
            // `resolve_provider` (`"{provider}: not found"`) and must stay
            // failover-able here, not hard-dropped as "incapable".
            if let Some(p) = pc
                && !p.apis.contains_key(&used)
            {
                let model = t.real_model.as_deref().unwrap_or(&req.model);
                info!(
                    provider = %t.provider,
                    model = %model,
                    protocol = used.as_str(),
                    "skipping target whose provider declares no apis node for the used protocol"
                );
                errors.push(format!(
                    "{}/{model}: lacks capability {}",
                    t.provider,
                    used.as_str()
                ));
                return false;
            }
            if !has_hard_requirements {
                return true;
            }
            let mut provided = target_capabilities(used);
            if let Some(ov) = &t.capabilities {
                ov.apply_to(&mut provided);
            }
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
    use sag_config::{ApiConfig, CapabilitiesOverride, KeyStrategy, ProxyMode};
    use std::collections::HashMap;

    /// Minimal enabled provider declaring exactly the given `apis` keys.
    /// `fallback` is left to the caller — only translated-lane tests with
    /// multiple protocol keys need it.
    fn provider(keys: &[ApiKind]) -> ProviderConfig {
        let mut pc = ProviderConfig {
            api_keys: vec!["sk".into()],
            key_strategy: KeyStrategy::FillFirst,
            proxy_mode: ProxyMode::default(),
            timeout_secs: 60,
            ..ProviderConfig::default()
        };
        for (i, key) in keys.iter().enumerate() {
            pc.apis.insert(
                *key,
                ApiConfig {
                    base_url: format!("https://example.com/{i}"),
                    ..ApiConfig::default()
                },
            );
        }
        pc
    }

    fn target(provider: &str) -> RouteTarget {
        RouteTarget {
            provider: provider.into(),
            capabilities: None,
            ..RouteTarget::default()
        }
    }

    fn req(json: serde_json::Value) -> ChatCompletionRequest {
        serde_json::from_value(json).expect("valid request")
    }

    /// Run the pre-check for a single target under `ingress`.
    fn filter(
        r: &ChatCompletionRequest,
        providers: &HashMap<String, ProviderConfig>,
        t: RouteTarget,
        ingress: IngressProtocol,
    ) -> (Vec<RouteTarget>, Vec<String>) {
        let mut errors = Vec::new();
        let out = filter_capable_targets(r, providers, vec![t], ingress, &mut errors);
        (out, errors)
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
        providers.insert("p".into(), provider(&[ApiKind::AnthropicMessages]));
        let (out, errors) = filter(
            &r,
            &providers,
            target("p"),
            IngressProtocol::ChatCompletions,
        );
        assert_eq!(out.len(), 1, "no requirements → no filtering");
        assert!(errors.is_empty());
    }

    #[test]
    fn image_part_request_drops_target_without_images() {
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
        // Every protocol row claims `images`; only an explicit override can
        // take it away.
        let mut t = target("noimg");
        t.capabilities = Some(CapabilitiesOverride {
            images: Some(false),
            ..Default::default()
        });
        providers.insert("noimg".into(), provider(&[ApiKind::ChatCompletions]));
        let (out, errors) = filter(&r, &providers, t, IngressProtocol::ChatCompletions);
        assert!(out.is_empty(), "target without images must be dropped");
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].contains("lacks capability images"),
            "got: {}",
            errors[0]
        );

        // Baseline chat row keeps the target.
        providers.insert("ok".into(), provider(&[ApiKind::ChatCompletions]));
        let (out2, errors2) = filter(
            &r,
            &providers,
            target("ok"),
            IngressProtocol::ChatCompletions,
        );
        assert_eq!(out2.len(), 1);
        assert!(errors2.is_empty());
    }

    #[test]
    fn reasoning_claimed_by_every_protocol_row() {
        let r = req(serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "high"
        }));
        assert!(request_hard_requirements(&r).reasoning);

        // Chat-lane protocol rows only: SystemOne 没有 chat IR / lane，
        // 不声明任何能力（见 `target_capabilities`）。
        for kind in ApiKind::protocols().filter(|k| !matches!(k, ApiKind::SystemOne)) {
            let mut providers = HashMap::new();
            providers.insert("p".into(), provider(&[kind]));
            let (out, errors) = filter(
                &r,
                &providers,
                target("p"),
                IngressProtocol::ChatCompletions,
            );
            assert_eq!(out.len(), 1, "{kind:?} must claim reasoning");
            assert!(errors.is_empty());
        }

        // …unless the target explicitly overrides reasoning off.
        let mut providers = HashMap::new();
        providers.insert("p".into(), provider(&[ApiKind::ChatCompletions]));
        let mut t = target("p");
        t.capabilities = Some(CapabilitiesOverride {
            reasoning: Some(false),
            ..Default::default()
        });
        let (out, errors) = filter(&r, &providers, t, IngressProtocol::ChatCompletions);
        assert!(out.is_empty());
        assert!(errors[0].contains("lacks capability reasoning"));
    }

    #[test]
    fn target_capabilities_keyed_by_protocol() {
        let chat = target_capabilities(ApiKind::ChatCompletions);
        assert!(chat.streaming && chat.tools && chat.images && chat.reasoning);
        assert!(
            chat.documents,
            "chat must claim documents so file parts are not hard-dropped"
        );
        assert!(!chat.stateful && !chat.hosted_tools);

        let responses = target_capabilities(ApiKind::Responses);
        assert!(
            responses.streaming && responses.tools && responses.stateful && responses.hosted_tools
        );
        assert!(
            !responses.documents,
            "responses input schema takes no file parts"
        );

        let anthropic = target_capabilities(ApiKind::AnthropicMessages);
        assert!(anthropic.documents && anthropic.reasoning && anthropic.hosted_tools);
        assert!(!anthropic.stateful);

        let image = target_capabilities(ApiKind::Image);
        assert!(image.streaming);
        assert!(
            !image.tools && !image.images && !image.documents,
            "non-protocol entry claims the minimal set"
        );

        let systemone = target_capabilities(ApiKind::SystemOne);
        assert!(
            !systemone.streaming
                && !systemone.reasoning
                && !systemone.tools
                && !systemone.images
                && !systemone.documents,
            "SystemOne has no chat semantics — it claims nothing"
        );
    }

    #[test]
    fn file_request_keyed_by_actually_used_protocol() {
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
        // chat / anthropic rows claim documents → kept.
        providers.insert("oa".into(), provider(&[ApiKind::ChatCompletions]));
        let (out, errors) = filter(
            &r,
            &providers,
            target("oa"),
            IngressProtocol::ChatCompletions,
        );
        assert_eq!(
            out.len(),
            1,
            "chat-only route must keep file-bearing requests; errors: {errors:?}"
        );
        assert!(errors.is_empty());

        // Responses row does not → dropped (even translated down from a
        // chat ingress, the actually-used protocol is still responses).
        providers.insert("rs".into(), provider(&[ApiKind::Responses]));
        let (out2, errors2) = filter(
            &r,
            &providers,
            target("rs"),
            IngressProtocol::ChatCompletions,
        );
        assert!(out2.is_empty());
        assert!(errors2[0].contains("lacks capability documents"));
    }

    #[test]
    fn filtering_uses_actually_used_protocol() {
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
        let mut providers = HashMap::new();

        // chat-only provider × Responses ingress → translated down to the
        // single protocol key (`chat_completions`) → documents claim holds.
        providers.insert("chat-only".into(), provider(&[ApiKind::ChatCompletions]));
        let (out, errors) = filter(
            &r,
            &providers,
            target("chat-only"),
            IngressProtocol::Responses,
        );
        assert_eq!(
            out.len(),
            1,
            "translated lane must use the chat row; errors: {errors:?}"
        );

        // Multi-protocol provider: chat ingress → native chat row (kept);
        // Responses ingress → native responses row (dropped).
        let mut multi = provider(&[ApiKind::ChatCompletions, ApiKind::Responses]);
        multi.fallback = Some(ApiKind::Responses);
        providers.insert("multi".into(), multi);
        let (out2, errors2) = filter(
            &r,
            &providers,
            target("multi"),
            IngressProtocol::ChatCompletions,
        );
        assert_eq!(out2.len(), 1, "native chat row kept; errors: {errors2:?}");

        let (out3, errors3) = filter(&r, &providers, target("multi"), IngressProtocol::Responses);
        assert!(out3.is_empty(), "native responses row dropped it");
        assert!(errors3[0].contains("lacks capability documents"));
    }

    #[test]
    fn used_protocol_matrix() {
        // Native lane → the ingress protocol, whatever the fallback says.
        assert_eq!(
            used_protocol(
                Lane::Native,
                IngressProtocol::AnthropicMessages,
                &[ApiKind::AnthropicMessages, ApiKind::ChatCompletions],
                Some(ApiKind::ChatCompletions)
            ),
            ApiKind::AnthropicMessages
        );
        // Translated lane → fallback.
        assert_eq!(
            used_protocol(
                Lane::Translated,
                IngressProtocol::Responses,
                &[ApiKind::ChatCompletions],
                Some(ApiKind::ChatCompletions)
            ),
            ApiKind::ChatCompletions
        );
        // Translated lane, implicit fallback → the single protocol key.
        assert_eq!(
            used_protocol(
                Lane::Translated,
                IngressProtocol::Responses,
                &[ApiKind::Image, ApiKind::AnthropicMessages],
                None
            ),
            ApiKind::AnthropicMessages
        );
        // Translated lane: SystemOne never counts as a chat-IR candidate.
        assert_eq!(
            used_protocol(
                Lane::Translated,
                IngressProtocol::Responses,
                &[ApiKind::SystemOne, ApiKind::ChatCompletions],
                None
            ),
            ApiKind::ChatCompletions
        );
        // Translated lane: an explicit SystemOne fallback is ignored
        // (defensive — config validation already rejects it).
        assert_eq!(
            used_protocol(
                Lane::Translated,
                IngressProtocol::Responses,
                &[ApiKind::SystemOne, ApiKind::ChatCompletions],
                Some(ApiKind::SystemOne)
            ),
            ApiKind::ChatCompletions
        );
        // Translated lane: only SystemOne configured → undetermined → chat.
        assert_eq!(
            used_protocol(
                Lane::Translated,
                IngressProtocol::Responses,
                &[ApiKind::SystemOne],
                Some(ApiKind::SystemOne)
            ),
            ApiKind::ChatCompletions
        );
        // Undetermined → chat (the pre-catalog default).
        assert_eq!(
            used_protocol(Lane::Translated, IngressProtocol::Responses, &[], None),
            ApiKind::ChatCompletions
        );
    }

    #[test]
    fn presence_check_drops_target_without_the_used_protocol_node() {
        // `apis = { systemone }` alone is a legal config shape (SystemOne is a
        // decision protocol). A chat request routed to it has no chat-IR node,
        // so `used_protocol` falls back to `ChatCompletions` — the presence
        // check must drop it here instead of letting `provider.chat()` fail
        // with a `BadRequest` that dispatch misclassifies as a client-caused
        // terminal (400, no failover, no attempt row).
        let r = req(serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}]
        }));
        let mut providers = HashMap::new();
        providers.insert("so".into(), provider(&[ApiKind::SystemOne]));

        let (out, errors) = filter(
            &r,
            &providers,
            target("so"),
            IngressProtocol::ChatCompletions,
        );
        assert!(
            out.is_empty(),
            "systemone-only provider must be dropped on the chat path"
        );
        assert_eq!(errors.len(), 1, "one pre-drop reason: {errors:?}");
        assert!(
            errors[0].contains("so/m: lacks capability chat_completions"),
            "got: {}",
            errors[0]
        );

        // Mixed route: the chat-capable target survives, the incapable one is
        // dropped, so dispatch still has a target to fail over to.
        providers.insert("oa".into(), provider(&[ApiKind::ChatCompletions]));
        let mut errors = Vec::new();
        let kept = filter_capable_targets(
            &r,
            &providers,
            vec![target("so"), target("oa")],
            IngressProtocol::ChatCompletions,
            &mut errors,
        );
        assert_eq!(kept.len(), 1, "only the capable target survives");
        assert_eq!(kept[0].provider, "oa");
        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert!(errors[0].contains("lacks capability chat_completions"));
    }

    #[test]
    fn presence_check_keeps_targets_whose_used_protocol_is_declared() {
        // No hard requirement → the presence check is the only gate, and a
        // declared used protocol passes it (native and translated lanes).
        let r = req(serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}]
        }));
        let mut providers = HashMap::new();
        providers.insert("am".into(), provider(&[ApiKind::AnthropicMessages]));
        providers.insert(
            "both".into(),
            provider(&[ApiKind::SystemOne, ApiKind::ChatCompletions]),
        );

        // Native anthropic lane: `anthropic_messages` is declared.
        let (out, errors) = filter(
            &r,
            &providers,
            target("am"),
            IngressProtocol::AnthropicMessages,
        );
        assert_eq!(out.len(), 1, "declared node keeps the target: {errors:?}");
        assert!(errors.is_empty());

        // Translated chat lane on a mixed provider: SystemOne does not count
        // as a chat-IR candidate, the chat node does — kept.
        let (out2, errors2) = filter(
            &r,
            &providers,
            target("both"),
            IngressProtocol::ChatCompletions,
        );
        assert_eq!(out2.len(), 1, "mixed provider kept: {errors2:?}");
        assert!(errors2.is_empty());
    }

    #[test]
    fn presence_check_skips_unknown_providers() {
        // An unregistered provider is reported later by `resolve_provider`
        // (`"{provider}: not found"`) and must stay failover-able instead of
        // being hard-dropped as incapable.
        let r = req(serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}]
        }));
        let (out, errors) = filter(
            &r,
            &HashMap::new(),
            target("ghost"),
            IngressProtocol::ChatCompletions,
        );
        assert_eq!(out.len(), 1);
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

        // Every protocol row claims streaming + tools, so only an override
        // can make a target fail this requirement.
        let mut providers = HashMap::new();
        providers.insert("bare".into(), provider(&[ApiKind::AnthropicMessages]));
        let mut t = target("bare");
        t.capabilities = Some(CapabilitiesOverride {
            tools: Some(false),
            ..Default::default()
        });
        let (out, errors) = filter(&r, &providers, t, IngressProtocol::ChatCompletions);
        assert!(out.is_empty());
        assert!(errors[0].contains("lacks capability tools"));
    }

    #[test]
    fn hosted_tools_override_keeps_target() {
        // Baseline chat row has hosted_tools=false; a hosted-tools
        // requirement needs an explicit target override to pass.
        let r = req(serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "web_search"}]
        }));
        assert!(
            request_hard_requirements(&r).hosted_tools,
            "web_search tool must require hosted_tools"
        );

        let mut providers = HashMap::new();
        providers.insert("oa".into(), provider(&[ApiKind::ChatCompletions]));

        let (out, errors) = filter(
            &r,
            &providers,
            target("oa"),
            IngressProtocol::ChatCompletions,
        );
        assert!(out.is_empty(), "baseline chat row must drop; {errors:?}");

        let mut t = target("oa");
        t.capabilities = Some(CapabilitiesOverride {
            hosted_tools: Some(true),
            ..Default::default()
        });
        let (out2, errors2) = filter(&r, &providers, t, IngressProtocol::ChatCompletions);
        assert_eq!(
            out2.len(),
            1,
            "hosted_tools=true override must keep target; errors: {errors2:?}"
        );
        assert!(errors2.is_empty());
    }

    #[test]
    fn tools_false_override_drops_target_for_tools_request() {
        let r = req(serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {}}}]
        }));
        assert!(request_hard_requirements(&r).tools);

        let mut providers = HashMap::new();
        providers.insert("oa".into(), provider(&[ApiKind::ChatCompletions]));
        let mut t = target("oa");
        t.capabilities = Some(CapabilitiesOverride {
            tools: Some(false),
            ..Default::default()
        });
        let (out, errors) = filter(&r, &providers, t, IngressProtocol::ChatCompletions);
        assert!(out.is_empty(), "tools=false must drop target");
        assert!(
            errors.iter().any(|e| e.contains("lacks capability tools")),
            "errors: {errors:?}"
        );
    }

    #[test]
    fn anthropic_and_responses_claim_hosted_tools() {
        let a = target_capabilities(ApiKind::AnthropicMessages);
        assert!(a.hosted_tools, "anthropic must claim hosted_tools");
        let r = target_capabilities(ApiKind::Responses);
        assert!(r.hosted_tools, "responses must claim hosted_tools");
        let c = target_capabilities(ApiKind::ChatCompletions);
        assert!(!c.hosted_tools, "chat must NOT claim hosted_tools");
    }
}
