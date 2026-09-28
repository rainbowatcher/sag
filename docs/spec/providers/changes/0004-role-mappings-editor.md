---
created: 2026-09-28
updated: 2026-09-28
type: 新增
---

# 0004 — 供应商 role_mappings 编辑入口

## 动机

后端已支持 provider 级 `role_mappings`（`HashMap<String, String>`，将请求角色名映射为上游支持的角色，例如 `developer` → `system`），但管理面板的 Provider 新增/编辑表单没有对应编辑入口——用户只能手工编辑配置文件才能设置角色映射，与表单内已有的 Headers、Body Overrides、Strip Fields 等高级配置形成能力缺口。

本次变更为 Provider 表单的 Advanced tab 补齐 `role_mappings` 的 KV 编辑入口，并遵循管理 API 的字段语义：后端 PUT 不传该字段 = 保留旧值，传 `{}` = 清空映射。

## 需求变更

- requirements.md：「角色映射」节已定义后端映射语义（大小写敏感、空映射零开销），本次补充前端编辑入口。requirements.md 本次未做条目修改，后端行为定义不变。
- 新增 UI 行为：Provider 新增/编辑表单的 Advanced tab 提供 Role Mappings 键值编辑区。

## 代码变更

| 文件 | 变更 |
|------|------|
| [sag-ui/src/types/api.ts](../../../../sag-ui/src/types/api.ts) | `Provider` interface 新增必需字段 `role_mappings: Record<string, string>` |
| [sag-ui/src/stores/providers.ts](../../../../sag-ui/src/stores/providers.ts) | `addProvider` / `updateProvider` 的 payload 类型新增可选 `role_mappings` |
| [sag-ui/src/components/providers/ProviderEditForm.vue](../../../../sag-ui/src/components/providers/ProviderEditForm.vue) | Advanced tab 新增 Role Mappings 的 `KvEditor` 编辑区；保存时对 `roleMappingsMap()` 做 `JSON.stringify` 脏检查，与 `provider.role_mappings` 相同则不发送该字段（后端保留旧值），清空发送 `{}`（清空映射） |
| [sag-ui/src/components/providers/ProviderAddForm.vue](../../../../sag-ui/src/components/providers/ProviderAddForm.vue) | Advanced tab 新增 Role Mappings 的 `KvEditor` 编辑区；仅当解析出的映射非空时才将 `role_mappings` 放入 POST payload |

## 设计决策

管理 API 的 PUT 采用「缺省字段 = 保留旧值」语义，因此编辑表单不能用「总是发送解析结果」的简单做法：若用户未触碰该字段却发送了 `{}`，会误清空服务端已有映射。编辑表单改为脏检查——解析后的映射与服务端快照做 `JSON.stringify` 比较，仅在不一致时发送，从而同时满足「未变化不覆盖」与「主动清空发 `{}`」两种意图。新增表单无旧值概念，故沿用其他可选字段的约定，仅在非空时携带。

## 测试

- `pnpm format`：通过
- `pnpm typecheck`（vue-tsc）：通过
- `pnpm lint`（oxlint）：通过
- 形式审查（独立 review）：PASS，0 Blocker / 0 Major / 0 Minor
- 手动验证场景：编辑既有 provider，Advanced tab 显示现有映射；不修改直接保存 → 请求体不含 `role_mappings`；清空所有条目后保存 → 请求体含 `role_mappings: {}`；新增 provider 时填写映射 → POST 携带映射，留空 → payload 不含该字段
