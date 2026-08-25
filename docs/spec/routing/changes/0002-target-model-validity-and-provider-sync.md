---
created: 2026-08-25
updated: 2026-08-25
type: 修复
---

# 0002 — 路由目标模型有效性校验与 Provider 变更联动感知

## 动机
当 Provider 更新并删除了某些已启用模型（`enabled_models`）时，网关在运行时未对 Target 模型执行白名单过滤，导致仍会向已删除模型的上游发送请求；同时前端 Routes 页面未能及时感知 Provider 模型列表变化，列表中未展示 Target 失效诊断，编辑表单中因选项缺失导致当前模型显示为空白。

## 需求变更
- 细化 `requirements.md` 中的「与其它模块的交互」：明确 `resolve_route` / `filter_enabled_targets` 必须校验 Target 模型是否在 Provider 的 `enabled_models` 白名单内（非空时）。
- `GET /v1/models` 过滤逻辑同步排除所有 Target 指向的模型均不在对应 Provider `enabled_models` 白名单内的路由。
- 前端 Routes 页面在 Provider 变更后联动清除缓存，并在路由列表与编辑表中直观展示失效目标及警告。

## 代码变更
| 文件 | 变更 |
|------|------|
| [sag-dispatch/src/dispatch/resolve.rs](../../../../sag-dispatch/src/dispatch/resolve.rs) | `filter_enabled_targets` 增加 `enabled_models` 校验，自动过滤不在白名单中的 Target |
| [sag-router/src/handlers/mod.rs](../../../../sag-router/src/handlers/mod.rs) | `list_models` 同步过滤所有 Target 均不在 `enabled_models` 白名单内的路由 |
| [sag-ui/src/stores/routes.ts](../../../../sag-ui/src/stores/routes.ts) | 提供 `clearProviderModelsCache` 缓存清理接口 |
| [sag-ui/src/stores/providers.ts](../../../../sag-ui/src/stores/providers.ts) | Provider 增删改后联动清理路由 Store 的模型缓存并重新拉取列表 |
| [sag-ui/src/components/Combobox.vue](../../../../sag-ui/src/components/Combobox.vue) | 当值不在选项列表中时回退显示实际字符串而非空占位 |
| [sag-ui/src/components/routes/TargetEditor.vue](../../../../sag-ui/src/components/routes/TargetEditor.vue) | 动态识别已失效模型/不存在的 Provider 并标明警告 |
| [sag-ui/src/components/routes/RouteList.vue](../../../../sag-ui/src/components/routes/RouteList.vue) | 增加多维度 Target 有效性诊断（缺失 Provider、已禁用 Provider、无效模型）与 Tooltip 详情 |

## 测试
- `filter_enabled_targets_drops_targets_not_in_enabled_models`（单元测试）
- `list_models_excludes_routes_with_models_not_in_enabled_models`（集成测试）
- 前端 `pnpm typecheck` 与 `pnpm lint` 校验通过
