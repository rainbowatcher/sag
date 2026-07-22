---
created: 2026-07-22
updated: 2026-07-22
type: 新增
---

# 0002 — per-provider strip_fields 字段剥离

## 动机

客户端可能在请求体塞入 vLLM/Qwen 系专有字段（如 `chat_template_kwargs`），SAG 按 `#[serde(flatten)] extra` 透传给上游。当上游用严格 schema 校验（pydantic `extra=forbid`，如 opencode zen）时会 400 拒收。现有 `body_overrides` 只能 insert/overwrite 字段、不能 remove；且把字段 override 成 `null` 无效（pydantic 拒绝键存在本身，而非键值）。需要 per-provider 的字段剥离能力，让 SAG 在转发前从请求体移除指定字段。

## 需求变更

- requirements.md 新增「字段剥离 strip_fields」节：provider 级 `strip_fields: Vec<String>`，在三层 override（`body_overrides` → `model_overrides` → `RouteTarget.extra`）**之后**应用，作为硬性最终净化；配置矛盾时 strip 优先；空列表 no-op，非 object body no-op
- 配置 Schema 补充 strip_fields 的 TOML 和 API 描述

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-config/src/config.rs`](../../../sag-config/src/config.rs) | `ProviderConfig` 新增 `strip_fields: Vec<String>`（`#[serde(default, skip_serializing_if = "Vec::is_empty")]`） |
| [`sag-config/src/loader/save.rs`](../../../sag-config/src/loader/save.rs) | 持久化路径的 `ProviderConfig` 构造点补 `strip_fields` |
| [`sag-providers/src/util.rs`](../../../sag-providers/src/util.rs) | 新增 `apply_body_strip`：从 JSON body 顶层移除指定字段 |
| [`sag-providers/src/base.rs`](../../../sag-providers/src/base.rs) | `ProviderBase` 持有 `strip_fields`；`build_request` 在两行 `apply_body_overrides` 之后调用 `apply_body_strip` |
| [`sag-admin/src/admin_providers.rs`](../../../sag-admin/src/admin_providers.rs) | `AddProviderRequest`/`UpdateProviderRequest`/`ProviderInfo` 三 struct 加 `strip_fields`；add/update handler 传递字段 |
| [`sag-admin/src/admin_routes.rs`](../../../sag-admin/src/admin_routes.rs) | 测试 fixture 补字段 |
| [`sag-admin/src/test_utils.rs`](../../../sag-admin/src/test_utils.rs) | `one_provider_config` 补字段 |
| [`sag-ui/src/types/api.ts`](../../../sag-ui/src/types/api.ts) | `Provider` 接口加 `strip_fields: string[]` |
| [`sag-ui/src/stores/providers.ts`](../../../sag-ui/src/stores/providers.ts) | `addProvider`/`updateProvider` payload 加 `strip_fields?: string[]` |
| [`sag-ui/src/components/providers/ProviderEditForm.vue`](../../../sag-ui/src/components/providers/ProviderEditForm.vue) | 新增 strip_fields 字符串列表编辑器（add/remove + trim + 空串过滤 + 无变更不传 payload） |
| [`sag-ui/src/components/providers/ProviderAddForm.vue`](../../../sag-ui/src/components/providers/ProviderAddForm.vue) | 同上 |
| [`gateway.test.example.toml`](../../../gateway.test.example.toml) | 新增 `strip_fields` 注释示例 |
| 多处 `ProviderConfig { ... }` 构造点 | 补 `strip_fields: vec![]`（sag-dispatch/mod.rs、state.rs、testing.rs、sag-router/tests/auth_regression.rs 等） |

## 测试

- `apply_body_strip` 5 个单测：移除指定字段、空 fields no-op、strip 在 override 之后生效、非 object body no-op、strip 不存在字段 no-op
- `strip_fields_removes_field_after_all_overrides`：验证 `build_request` 链路中 strip 在 `body_overrides`/`target_extra` 之后生效

## 影响的需求条目

- 「预期行为」新增「字段剥离 strip_fields」节