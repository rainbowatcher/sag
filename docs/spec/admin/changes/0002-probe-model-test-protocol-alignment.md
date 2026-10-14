---
created: 2026-09-30
updated: 2026-09-30
type: 修改
---

# 0002 — 探针与模型测试的协议对齐（共享最小请求 + model/test 走 provider adapter）

## 动机

两个 admin 端点都在「向上游发一条最小请求」，但协议适配程度不同，且协议知识有两份拷贝：

1. **协议知识散落**：`provider_chat_endpoint`（`sag-providers/src/util.rs`）与 `sag_core::protocol_path` 是同一张路径表的两份拷贝；最小请求体也在 `probe_body`（probe 用）与 `test_single_key`（model/test 用）两处手写。
2. **model/test 对三协议存在假阴性**：`responses` 协议发的是 chat 形状 body（`messages` + `max_tokens`，而该协议要 `input` + `max_output_tokens`）→ 上游 400；`image` 协议把 chat body 发到 `/images/generations` → 必然 400；reasoning 模型拒收已弃用的 `max_tokens`。
3. **model/test 与生产路径不等价**：手搓 HTTP 绕过了 `ProviderBase::build_request` 整条链——per-API headers 分层、`body_overrides`、`strip_fields` / `model_strip_fields`、`role_mappings`、`anthropic_cache_control`、超时策略。于是「测试通过」并不等于「生产能跑」。

## 需求变更

- `docs/spec/admin/requirements.md` 「模型测试」节：
  - 支持 `chat_completions` / `responses` / `anthropic_messages` 三种协议，`image` 返回 400 `UNSUPPORTED_API_KIND`（对已上线端点的行为变更：原先前端拿到的是上游 400 与 `upstream returned status 400`）；
  - 上游请求经由 provider adapter 发送，与生产派发路径一致，因此 HTTP 200 但响应体携带 `error` 对象时同样判定为失败；
  - 每次测试最多 16 个 key（超出 400 `TOO_MANY_KEYS`）；模型级 override 只有两层且 `api.model_overrides[<model>]` 优先于 provider 级 `model_overrides[<model>]`；
  - 显式列出非目标：路由 `target.extra` 合并、模型解析/映射、`enabled_models` 门控、provider 级 ttfb/chunk 超时；测试对所选协议使用非流式原生调用。
- `docs/spec/admin/requirements.md` 新增「协议探测」节（`POST /api/providers/probe-protocols`）：此前该端点只在 `docs/guide/admin-api.md` 有描述，requirements 缺失；本次固化其判定语义——2xx 一律 `supported`（不做错误体嗅探），`message` 给出上游 body 预览。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-admin/src/probe_request.rs`](../../../sag-admin/src/probe_request.rs) | 新增。`minimal_request(kind, model)` 成为三协议最小请求体的唯一来源；`request_url` 由 `sag_core::protocol_path` 派生（`image` → `None`）；`minimal_request` 的 `image` 分支返回 `None`，调用方必须显式处理 |
| [`sag-admin/src/probe_provider.rs`](../../../sag-admin/src/probe_provider.rs) | 新增。`synthesize_config` 从表单态合成单 lane 临时 `ProviderConfig`（单 key、`FillFirst`、超时钳到 30s）；`test_protocol` 按 kind 分派 `chat` / `anthropic_messages` / `responses_native`，`target_extra` 是模型级 override 的唯一入口；`message_from_error` 把 `AppError` 映射为与旧 raw 路径逐字一致的文案 |
| [`sag-admin/src/admin_testing.rs`](../../../sag-admin/src/admin_testing.rs) | `model_test` 顶部对 `image` 早退 400，改用 `probe_provider` 走 provider 链，payload 扩展 `api` + provider 级字段，`api_keys` 上限 16；probe 段顶部补文档注释说明「不复用 adapter」的原因，2xx 的 `message` 改为 `ok_message(body)`（折叠空白 + 截断 160 字符）；删除本地 `probe_body` / `probe_url` |
| [`sag-admin/src/lib.rs`](../../../sag-admin/src/lib.rs) | 注册 `probe_request` / `probe_provider` 两个 `pub(crate)` 模块 |
| [`sag-admin/src/test_utils.rs`](../../../sag-admin/src/test_utils.rs) | 新增 TCP mock 上游（`spawn_upstream_capture_many` / `spawn_upstream_capture_scripted` + `CapturedRequest`），供跨 crate 的 admin handler 测试捕获真实上游请求 |
| [`sag-providers/src/util.rs`](../../../sag-providers/src/util.rs) | 删除 `provider_chat_endpoint`（与 `sag_core::protocol_path` 重复），路径一律取自 `protocol_path`；`provider_auth_headers` 保留 |
| [`sag-ui/src/composables/useProviderProbe.ts`](../../../sag-ui/src/composables/useProviderProbe.ts) | `onTestModel` 发送整节 `api` + provider 级默认值；新增 `getProviderDefaults` 选项 |
| [`sag-ui/src/stores/providers.ts`](../../../sag-ui/src/stores/providers.ts) | `modelTest` payload 类型扩展（`api` / `provider_headers` / `body_overrides` / `strip_fields` / `model_overrides` / `model_strip_fields` / `role_mappings` / `anthropic_cache_control`） |
| [`sag-ui/src/components/providers/ProviderAddForm.vue`](../../../sag-ui/src/components/providers/ProviderAddForm.vue) | 传入 `getProviderDefaults`（表单当前未保存的 provider 级默认值） |
| [`sag-ui/src/components/providers/ProviderEditForm.vue`](../../../sag-ui/src/components/providers/ProviderEditForm.vue) | 同上 |
| [`docs/guide/admin-api.md`](../../guide/admin-api.md) | 更新 model/test payload 表与协议说明；probe 段补「2xx 一律 supported，`message` 给出 body 预览」 |

关键设计决策：

- **probe 保持 raw HTTP，只共享协议知识、不共享传输层**。adapter 内建的「200 里夹带 error → `Upstream{429|502}`」改写会抹掉原始状态码，使「2xx = 支持」的判定语义不可复原；且 adapter 会解析响应体并要求 schema 合法，把 HTTP 层发现变成 schema 层发现。协议知识经 `probe_request::minimal_request` + `sag_core::protocol_path` + `sag_providers::provider_auth_headers` 共享。
- **两侧 200 语义有意分叉**：model/test 走 adapter 后，非流式的「200 + error body」判失败（生产既定语义）；probe 则 2xx 恒 `supported`。
- **`model_overrides` 不能靠合成 config 传递**：`ProviderBase` 不读该字段（生产消费点是 dispatch 的 `merge_model_overrides`），因此 handler 自行计算 `target_extra` 并经 `test_protocol` 传入；`model_strip_fields` 由 `ProviderBase` 直接消费，走合成 config 即可。
- **per-API `timeout_secs` 必须钳制**：`effective_timeout` 在 per-API 有值时整段替换 caller 值，沿用表单里的大值会让 admin 端点阻塞数百秒并谎报超时，故钳到 30s 并用生效值生成文案。

## 测试

- `sag-admin/src/probe_request.rs` 内联单元测试：
  - `minimal_request_chat_uses_max_completion_tokens`（并断言不出现 `max_tokens`）
  - `minimal_request_anthropic_uses_max_tokens`
  - `minimal_request_responses_uses_input_and_max_output_tokens`
  - `minimal_request_image_is_none`
  - `request_url_derives_from_protocol_path`（含尾斜杠 base_url 归一化与 `image → None`）
- `sag-admin/src/probe_provider.rs` 内联单元测试：
  - `synthesize_config_uses_single_key_fill_first_and_clamped_timeout`
  - `synthesize_config_only_contains_probed_kind`
  - `message_from_error_matches_legacy_wording` / `message_from_error_network_timeout_and_connection`
- `sag-admin/src/admin_testing.rs` 内联测试（mock 上游捕获请求）：
  - model/test：`model_test_responses_kind_posts_input_to_responses_path`、`model_test_anthropic_kind_sends_x_api_key_and_anthropic_version`、`model_test_chat_kind_posts_max_completion_tokens`、`model_test_applies_body_overrides_and_strip_fields`、`model_test_applies_per_api_headers_over_provider_headers`、`model_test_model_override_reaches_upstream_body`、`model_test_provider_headers_apply_when_api_declares_none`、`model_test_flat_headers_fallback_when_api_present`、`model_test_api_model_override_reaches_upstream_body`、`model_test_api_model_override_wins_over_provider_level`、`model_test_rejects_too_many_api_keys`、`model_test_treats_200_with_error_body_as_failure`、`model_test_per_key_results_preserved`、`model_test_legacy_flat_payload_still_works`、`model_test_image_kind_is_400_before_network`
  - probe：`probe_verdict_2xx_is_supported_even_with_error_body`、`probe_ok_message_truncates_and_flattens_body`、`probe_ignores_unknown_payload_fields_and_saved_config`；保留 `probe_image_target_is_400_before_network` 等既有 verdict 测试
