---
created: 2026-09-28
updated: 2026-09-28
type: 新增
---

# 0005 — Provider OAuth auth_mode 管理 CRUD 与表单

## 动机

Phase 1–3 已在配置层与运行时落地 `AuthMode{Static,OAuth}` / `OAuthConfig` 与 `CredentialProvider`（client-credentials + 401 单次重试），但管理 API 与管理面板仍无法创建或编辑 OAuth 凭据：`AddProviderRequest` / `UpdateProviderRequest` / `ProviderInfo` 未暴露 `auth_mode` / `oauth`，UI 表单也只有静态 `api_keys`。本次补齐 admin CRUD、secret 脱敏契约、UI 表单与文档。

## 需求变更

- Admin API：`POST/PUT/GET /api/providers` 增加 `auth_mode` + `oauth` 字段。
- Secret 脱敏：`ProviderInfo` 出参 `oauth.client_secret` 恒为 `"***"`，另附 `client_secret_set: bool`；更新请求收到 `"***"` 视为保持原密钥。
- 创建：`auth_mode = "oauth"` 允许空 `api_keys`，但缺 oauth 段 → 400；`static` 仍要求非空 `api_keys`。
- 更新：`auth_mode` patch 为 `static` 时同步清空 oauth 段；patch 为 `oauth` 且合并后缺 oauth 段 → 400。
- UI：Provider 新增/编辑表单提供 `auth_mode` 下拉与 oauth 字段组；oauth 时 `api_keys` 置灰；编辑表单 secret 显示 `***` 时提交保持。
- 文档：`configuration.md` / `providers.md` / `admin-api.md` 补充 auth_mode / oauth 与 secret 脱敏说明。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-admin/src/admin_providers.rs`](../../../../sag-admin/src/admin_providers.rs) | `AddProviderRequest` / `UpdateProviderRequest` / `ProviderInfo` 增加 `auth_mode` + `oauth`；secret 脱敏出参（`***` + `client_secret_set`）；update 时 `"***"` 保持原值、切 static 清空 oauth、oauth 缺段报错；新增 7 个 OAuth CRUD 单测 |
| [`sag-ui/src/types/api.ts`](../../../../sag-ui/src/types/api.ts) | 新增 `AuthMode` / `OAuthConfig` / `OAuthConfigOut` / `OAuthConfigForm` 与表单 helper；`Provider` / Add / Update payload 增加 auth_mode + oauth |
| [`sag-ui/src/components/providers/ProviderOAuthFields.vue`](../../../../sag-ui/src/components/providers/ProviderOAuthFields.vue) | 新增 OAuth 字段组（token_url / client_id / client_secret / scope / audience / refresh_token） |
| [`sag-ui/src/components/providers/ProviderBasicFields.vue`](../../../../sag-ui/src/components/providers/ProviderBasicFields.vue) | 新增 `auth_mode` 下拉；oauth 时 api_keys 置灰 + 渲染 oauth 字段组 |
| [`sag-ui/src/components/providers/ProviderAddForm.vue`](../../../../sag-ui/src/components/providers/ProviderAddForm.vue) | 新增表单接入 auth_mode；oauth 校验 token_url/client_id/client_secret；oauth 时提交空 api_keys |
| [`sag-ui/src/components/providers/ProviderEditForm.vue`](../../../../sag-ui/src/components/providers/ProviderEditForm.vue) | 编辑表单接入 auth_mode；secret 显示 `***` 时提交保持；切 static 发送 `oauth: null` |
| [`docs/guide/configuration.md`](../../../../docs/guide/configuration.md) | Provider 字段表增加 `auth_mode` / `[providers.*.oauth]` 与示例、约束 |
| [`docs/guide/providers.md`](../../../../docs/guide/providers.md) | 公共字段表补充凭据模式说明 |
| [`docs/guide/admin-api.md`](../../../../docs/guide/admin-api.md) | Create/Update 请求字段与 secret 脱敏、patch 语义说明 |

## 设计决策

- 出参脱敏统一为 `client_secret: "***"` + `client_secret_set: bool`（不返回任何明文）。`"***"` 作为保留原值的哨兵值，与编辑表单「不改 secret 则不覆盖」的 UI 契约对齐。
- `auth_mode` patch 为 `static` 时同步 `oauth = None`：避免管理面板切回 static 后配置文件仍残留被忽略的 oauth 段，与「拒绝静默回退」原则一致（该清除是显式 patch 的副作用，不是未知值回退）。
- Admin 层对 `auth_mode="oauth"` 缺 oauth 段做 early 400，与配置加载校验同文案语义，避免依赖 coordinator 全量 validate 才失败。

## 测试

- `cargo test -p sag-admin --lib`（OAuth CRUD：round-trip 脱敏、切 static 清空 oauth、oauth 空 keys 可创建、`***` 保持原 secret、缺 oauth 段创建失败、static 空 keys 仍拒绝、update 缺 oauth 段 400、未知 auth_mode 422）
- `cargo test --workspace --tests`
- `cd sag-ui && pnpm typecheck && pnpm lint && pnpm build`
