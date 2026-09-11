---
created: 2026-09-08
updated: 2026-09-08
type: 新增
---

# 0005 — 出站代理配置（proxy_url + provider proxy_mode）

## 动机

SAG 之前的出站流量完全依赖底层 reqwest 的默认行为：未显式配置代理时，reqwest 会读取系统代理环境变量（`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` 及大小写变体），这带来两个问题：

1. **隐式行为不可控**：宿主机或 CI 环境一旦设置了代理环境变量，网关的全部 outbound 流量（上游 LLM 请求、fetch-models、临时测试）都会被意外路由到代理，用户无法在配置层面显式关闭。
2. **无法按 provider 差异化**：没有配置层级的出口策略，无法表达「某些 provider 走代理、某些强制直连」。

本次新增显式出站代理配置：顶层 `proxy_url` 定义全局 HTTP/HTTPS 代理，provider 级 `proxy_mode` 三态控制每个 provider 如何解析该代理。SAG 从此**不依赖**系统代理环境变量——没有显式代理时所有 outbound client 一律直连（`no_proxy`），使代理行为完全由配置决定、可预测、可审计。

## 需求变更

- `requirements.md`「预期行为」新增「出站代理」节：顶层 `proxy_url: Option<String>`（None/空值直连）；provider 三态 `proxy_mode`（`inherit` 默认 / `enabled` / `disabled`）；HTTP/HTTPS only、拒绝 userinfo；SAG 默认屏蔽系统代理环境变量。
- `requirements.md`「验证规则」追加：`proxy_url` 必须为合法 `http://` / `https://` URL；`proxy_mode='enabled'` 的 provider 要求存在可用的顶层 `proxy_url`，否则报错；空白值归一为「未配置」。
- `requirements.md`「保存」追加：保存路径写/移除顶层 `proxy_url` 与 provider `proxy_mode`（默认值跳过）；清除全局代理不得残留空 key。
- `requirements.md` 末尾「变更记录」追加 0005 条目。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-config/src/config.rs`](../../../../sag-config/src/config.rs) | 顶层新增 `proxy_url: Option<String>`；新增 `ProxyMode` 三态枚举（`inherit` / `enabled` / `disabled`）；新增 `ProxyConfig`（仅存 scheme/host/port，不存原始 URL / userinfo）；新增 `normalize_proxy_url()`（HTTP/HTTPS only、拒绝 userinfo、None/空白归一为未配置）与 `resolve_effective_proxy()` 纯解析；`validate()` 增加 `proxy_url` 与 `enabled` 依赖校验。 |
| [`sag-config/src/loader/save.rs`](../../../../sag-config/src/loader/save.rs) | 保存路径写/移除顶层 `proxy_url` 与 provider `proxy_mode`（默认 `inherit` 跳过序列化），保证清空全局代理后不残留空 key；新增 round-trip 测试。 |
| [`sag-core/src/http_client.rs`](../../../../sag-core/src/http_client.rs) | 新增 `build_http_client()`：显式代理时 `reqwest::Proxy::all` 走单一代理，否则 `no_proxy()` 显式屏蔽系统代理环境变量（HTTP/HTTPS/ALL_PROXY 及大小写变体），杜绝隐式代理路由。 |
| [`sag-providers/src/lib.rs`](../../../../sag-providers/src/lib.rs) | 新增 `create_provider_with_proxy()` 公开构造入口，按有效代理构建 adapter。 |
| [`sag-providers/src/base.rs`](../../../../sag-providers/src/base.rs) | `ProviderBase::new_with_proxy()` 支持有效代理构建；默认 `new()` 显式直连；新增系统代理环境变量屏蔽测试。 |
| [`sag-state/src/config_mutation.rs`](../../../../sag-state/src/config_mutation.rs) | 新增 `effective_proxy_for()`（全局 proxy + provider `proxy_mode` 统一解析，供启动/热重载/mutation 共用）；新增 `update_settings()`（统一 mutation 协议：校验、原子持久化、registry 同步）；provider sync 按 **effective proxy 差异** 决定是否重建 adapter（disabled 不受全局代理变化影响）。 |
| [`sag-admin/src/admin_settings.rs`](../../../../sag-admin/src/admin_settings.rs) | 新增 `GET /api/settings` 与 `PUT /api/settings` handler，读写顶层 `proxy_url`；配置错误映射 400。 |
| [`sag-admin/src/admin_providers.rs`](../../../../sag-admin/src/admin_providers.rs) | provider DTO / 请求体支持 `proxy_mode`（可选 patch，缺省保留现值）；CRUD 校验 `enabled` 依赖与配置错误 400。 |
| [`sag-admin/src/admin_testing.rs`](../../../../sag-admin/src/admin_testing.rs) | `fetch-models` / `model-test` 临时测试按**当前内存**全局 `proxy_url` + 请求体 `proxy_mode` 解析代理（不要求先保存）；`enabled` 无全局代理时 400（网络请求前拦截）。 |
| [`sag-gateway/src/main.rs`](../../../../sag-gateway/src/main.rs) | 启动时对每个 provider 用 `effective_proxy_for()` 解析有效代理后构造 adapter；解析失败仅告警并跳过该 provider，永不 panic、不泄漏代理 URL/凭据。 |
| [`sag-ui/src/types/api.ts`](../../../../sag-ui/src/types/api.ts) | 新增 `Settings` / `UpdateSettingsPayload`（`proxy_url`）与 `ProxyMode` 类型；provider DTO 增加 `proxy_mode`。 |
| [`sag-ui/src/stores/settings.ts`](../../../../sag-ui/src/stores/settings.ts) | 新增 settings store：`fetchSettings()`（GET /api/settings）、`saveSettings()`（PUT /api/settings）。 |
| [`sag-ui/src/pages/SettingsPage.vue`](../../../../sag-ui/src/pages/SettingsPage.vue) | 新增全局代理 URL 编辑页：URL 校验（HTTP/HTTPS only）、保存、清空。 |
| [`sag-ui/src/components/providers/ProviderAddForm.vue`](../../../../sag-ui/src/components/providers/ProviderAddForm.vue) | 新增 proxy_mode 三态选择；临时测试（fetch-models / model-test）透传当前 dialog 内存 `proxy_mode`，不要求先保存。 |
| [`sag-ui/src/components/providers/ProviderEditForm.vue`](../../../../sag-ui/src/components/providers/ProviderEditForm.vue) | 同上：三态选择、临时测试透传当前内存 `proxy_mode`。 |

## 测试

- `sag-config/src/config.rs` tests — `normalize_proxy_url_*` 系列（None/空白视为未配置、http/https 通过、ftp/socks5 拒绝、缺 host 拒绝、IPv6/非标准端口通过、userinfo 拒绝、周边空白 trim）、`effective_proxy_*` 系列（inherit 有/无全局、enabled 需全局否则 Err、disabled 恒直连）。
- `sag-core/src/http_client.rs` tests — `direct_client_builds`、`explicit_http_proxy_client_builds`、`explicit_https_proxy_client_builds`、`invalid_proxy_url_is_rejected`。
- `sag-providers/src/base.rs` tests — `provider_base_accepts_direct_and_explicit_proxy` 及系统代理环境变量屏蔽/恢复（串行化锁保护）。
- `sag-state/src/config_mutation.rs` tests — `effective_proxy_*` 系列（默认 inherit 无全局直连、inherit 用全局、空白全局视为 None、disabled 忽略全局、enabled 用全局 / 无全局 Err、畸形全局 Err）、`prepare_sync_rebuild_on_adapter_config_change`（含 effective proxy 变化触发重建）。
- `sag-state/tests/settings_proxy.rs` — 集成测试：`update_settings_persists_and_hot_rebuilds_providers`、`update_settings_rejects_malformed_proxy_url`、`update_settings_rejects_clearing_global_while_enabled_provider_exists`、`update_settings_normalizes_blank_proxy_to_none`、`update_settings_validates_even_with_empty_provider_set`、`reload_from_path_rebuilds_only_affected_providers`。
- `sag-admin/src/admin_settings.rs` tests — `get_settings_returns_current_proxy_url` / `get_settings_returns_null_when_no_proxy`、`put_settings_sets_proxy_url` / `put_settings_empty_string_clears_proxy` / `put_settings_null_clears_proxy`、`put_settings_invalid_url_is_400` / `put_settings_unsupported_scheme_is_400`、`put_settings_enabled_conflict_is_400_and_config_unchanged`、`put_settings_rebuilds_affected_provider_adapters`。
- `sag-admin/src/admin_providers.rs` tests — `add_provider_request_defaults_proxy_mode_to_inherit`、`update_provider_request_proxy_mode_is_optional_patch`、`add_provider_with_explicit_proxy_mode_and_info_echo`、`update_provider_proxy_mode_keeps_value_when_omitted`、`update_provider_proxy_mode_reset_to_inherit`、`add_provider_proxy_enabled_without_global_proxy_is_400`、`add_provider_proxy_enabled_with_global_proxy_succeeds`、`update_provider_proxy_enabled_without_global_proxy_is_400_and_config_unchanged`。
- `sag-admin/src/admin_testing.rs` tests — `fetch_models_request_defaults_proxy_mode_to_inherit`、`model_test_request_defaults_proxy_mode_to_inherit`、`fetch_models_enabled_without_global_proxy_is_400_before_network`、`model_test_enabled_without_global_proxy_is_400_before_network`。
- 全库验证：`just lint`、`just test --tests`、`just build-test` 均 exit 0。
