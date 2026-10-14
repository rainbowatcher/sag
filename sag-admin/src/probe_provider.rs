//! 模型测试（`POST /api/model/test`）的临时 provider 合成与协议分派。
//!
//! 从表单当前（可能未保存）的状态合成一份单 lane 临时 `ProviderConfig`，
//! 经 `create_provider_with_proxy` 建临时实例（不加入 registry），直接调用
//! 其非流式原生方法——与生产 `ProviderBase::build_request` 链路一致。

use std::collections::HashMap;

use sag_config::{ApiConfig, ApiKind, KeyStrategy, ProviderConfig, ProxyMode};

/// 临时 provider 名（仅用于日志与错误文案，不入 registry）。
pub(crate) const TEMP_PROVIDER_NAME: &str = "__model_test__";
/// model_test 的硬上限；表单里更大的 per-API timeout 会被钳到这里。
pub(crate) const MODEL_TEST_TIMEOUT_SECS: u64 = 30;
/// 单次模型测试允许的最大 key 数，超出由 handler 返回 400 `TOO_MANY_KEYS`。
pub(crate) const MAX_MODEL_TEST_KEYS: usize = 16;

/// 表单当前（可能未保存）的 provider 级默认值。
/// DTO 里的 `provider_headers` 映射到本结构体的 `headers` 字段。
pub(crate) struct ProviderDefaults {
    pub headers: HashMap<String, String>,
    pub body_overrides: HashMap<String, serde_json::Value>,
    pub strip_fields: Vec<String>,
    pub model_overrides: HashMap<String, HashMap<String, serde_json::Value>>,
    pub model_strip_fields: HashMap<String, Vec<String>>,
    pub role_mappings: HashMap<String, String>,
    pub anthropic_cache_control: Option<bool>,
}

/// 合成一份最小 ProviderConfig：真实 base_url/headers/overrides + 单 key + 钳到 30s 的超时。
///
/// 纯函数：不落盘、不注册、不读 ConfigHandle。
/// - `apis` 只放被测试的那一个 `kind`（保证 lane 唯一，`CatalogProvider` 按
///   `supports(kind)` 静态挑 lane）。
/// - `timeout_secs` 为上限（handler 传 [`MODEL_TEST_TIMEOUT_SECS`]）；per-API 值 =
///   `min(表单值, 上限)`。
pub(crate) fn synthesize_config(
    kind: ApiKind,
    api: &ApiConfig,
    key: &str,
    defaults: &ProviderDefaults,
    timeout_secs: u64,
) -> ProviderConfig {
    let clamped = api
        .timeout_secs
        .map_or(timeout_secs, |t| t.min(timeout_secs));
    let mut api_node = api.clone();
    api_node.timeout_secs = Some(clamped);

    let mut apis = indexmap::IndexMap::new();
    apis.insert(kind, api_node);

    ProviderConfig {
        api_keys: vec![key.to_string()],
        key_strategy: KeyStrategy::FillFirst,
        proxy_mode: ProxyMode::default(), // 代理经 create_provider_with_proxy 显式传入
        enabled: true,
        enabled_models: Vec::new(), // enabled_models 门控是显式非目标
        fallback: Some(kind),
        timeout_secs,
        chunk_timeout_secs: None,
        ttfb_timeout_secs: None,
        headers: defaults.headers.clone(),
        body_overrides: defaults.body_overrides.clone(),
        strip_fields: defaults.strip_fields.clone(),
        model_strip_fields: defaults.model_strip_fields.clone(),
        // ProviderBase 不读 model_overrides（生产消费点是 dispatch 的
        // merge_model_overrides）；handler 改经 target_extra 传入。
        model_overrides: HashMap::new(),
        role_mappings: defaults.role_mappings.clone(),
        anthropic_cache_control: defaults
            .anthropic_cache_control
            .unwrap_or_else(|| ProviderConfig::default().anthropic_cache_control),
        apis,
    }
}

/// 按 kind 发一条最小请求。成功 `Ok(())`，失败保留原始 AppError。
///
/// `target_extra` 是模型级 override 的唯一入口（`ProviderBase::build_request`
/// 在 body_overrides 之后应用它）。
pub(crate) async fn test_protocol(
    provider: &dyn sag_providers::LlmProvider,
    kind: ApiKind,
    model: &str,
    target_extra: &HashMap<String, serde_json::Value>,
    timeout_secs: u64,
) -> sag_core::Result<()> {
    let Some(minimal) = crate::probe_request::minimal_request(kind, model) else {
        return Err(sag_core::AppError::BadRequest(format!(
            "model test does not support api kind '{}'",
            kind.as_str()
        )));
    };
    match minimal {
        crate::probe_request::MinimalRequest::Chat(ir) => {
            provider
                .chat(&ir, model, "", target_extra, timeout_secs)
                .await?;
        }
        crate::probe_request::MinimalRequest::Anthropic(body) => {
            provider
                .anthropic_messages(body, target_extra, timeout_secs)
                .await?;
        }
        crate::probe_request::MinimalRequest::Responses(body) => {
            provider
                .responses_native(body, target_extra, "", timeout_secs)
                .await?;
        }
    }
    Ok(())
}

/// AppError → UI 文案：与旧 raw 路径的 `test_single_key` 逐字一致。
pub(crate) fn message_from_error(
    err: &sag_core::AppError,
    model: &str,
    timeout_secs: u64,
) -> String {
    use sag_core::AppError;
    match err {
        AppError::Upstream {
            status: 401 | 403,
            body,
            ..
        } => format!("Authentication failed: {body}"),
        AppError::Upstream {
            status: 404, body, ..
        } => format!("Model '{model}' not found: {body}"),
        AppError::Upstream { status, body, .. } => {
            format!("upstream returned status {status}: {body}")
        }
        AppError::Network(e) if e.is_timeout() => {
            format!("request timed out after {timeout_secs}s")
        }
        AppError::Network(e) => format!("connection failed: {e}"),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> ProviderDefaults {
        ProviderDefaults {
            headers: HashMap::new(),
            body_overrides: HashMap::new(),
            strip_fields: Vec::new(),
            model_overrides: HashMap::new(),
            model_strip_fields: HashMap::new(),
            role_mappings: HashMap::new(),
            anthropic_cache_control: None,
        }
    }

    fn api_with_timeout(timeout_secs: Option<u64>) -> ApiConfig {
        ApiConfig {
            base_url: "http://h/v1".into(),
            timeout_secs,
            ..ApiConfig::default()
        }
    }

    #[test]
    fn synthesize_config_uses_single_key_fill_first_and_clamped_timeout() {
        let cfg = synthesize_config(
            ApiKind::ChatCompletions,
            &api_with_timeout(Some(600)),
            "sk-1",
            &defaults(),
            30,
        );
        assert_eq!(cfg.api_keys, vec!["sk-1".to_string()]);
        assert_eq!(cfg.key_strategy, KeyStrategy::FillFirst);
        assert_eq!(cfg.timeout_secs, 30);
        assert_eq!(
            cfg.apis[&ApiKind::ChatCompletions].timeout_secs,
            Some(30),
            "a form timeout above the cap is clamped to the cap"
        );

        // The clamp is a `min`, not "overwrite with the cap".
        let cfg = synthesize_config(
            ApiKind::ChatCompletions,
            &api_with_timeout(Some(5)),
            "sk-1",
            &defaults(),
            30,
        );
        assert_eq!(cfg.apis[&ApiKind::ChatCompletions].timeout_secs, Some(5));
    }

    #[test]
    fn synthesize_config_only_contains_probed_kind() {
        for kind in [
            ApiKind::ChatCompletions,
            ApiKind::Responses,
            ApiKind::AnthropicMessages,
        ] {
            let cfg = synthesize_config(kind, &api_with_timeout(None), "sk-1", &defaults(), 30);
            assert_eq!(cfg.apis.len(), 1, "exactly one lane for {kind:?}");
            assert!(cfg.apis.contains_key(&kind), "{kind:?} must be present");
        }
    }

    #[test]
    fn message_from_error_matches_legacy_wording() {
        let upstream = |status: u16, body: &str| sag_core::AppError::Upstream {
            provider: "p".into(),
            status,
            body: body.into(),
        };

        assert_eq!(
            message_from_error(&upstream(401, "bad"), "gpt-4o", 30),
            "Authentication failed: bad"
        );
        assert_eq!(
            message_from_error(&upstream(403, "nope"), "gpt-4o", 30),
            "Authentication failed: nope"
        );
        assert_eq!(
            message_from_error(&upstream(404, "x"), "gpt-4o", 30),
            "Model 'gpt-4o' not found: x"
        );
        assert_eq!(
            message_from_error(&upstream(500, "boom"), "gpt-4o", 30),
            "upstream returned status 500: boom"
        );
    }

    /// `AppError::Network` wraps a real `reqwest::Error`; a transport failure
    /// without network I/O is produced by sending an unparseable URL (a
    /// builder error, `is_timeout() == false`). The timeout branch keeps the
    /// legacy literal in the source and is covered by the integration suite
    /// (see the module docs of `admin_testing::tests`).
    #[tokio::test]
    async fn message_from_error_network_timeout_and_connection() {
        let err = reqwest::Client::new()
            .get("relative-url-without-a-base")
            .send()
            .await
            .expect_err("a URL without a scheme must fail to build");
        assert!(!err.is_timeout(), "builder errors are not timeouts");

        let msg = message_from_error(&sag_core::AppError::Network(err), "gpt-4o", 30);
        assert!(msg.starts_with("connection failed: "), "{msg}");
    }
}
