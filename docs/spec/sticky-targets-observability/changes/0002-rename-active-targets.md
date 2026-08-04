---
created: 2026-08-04
type: 修改
---

# 0002 — 端点重命名 /api/active-targets

## 动机

0001 之后 `/api/sticky-targets` 的语义已从「StickyFailover pin 可观测性」扩展为
「路由生效目标可观测性」：GET 同时报 sticky pin（`pinned`/`expired`）与 fill-first
缓存顺序位置，DELETE 按 route strategy 分发（解除 pin / 丢弃洗牌缓存）。端点名
`sticky-targets` 名不副实——fill-first 路由的状态与 sticky 记忆无关，且前端列早已
叫「Active Target」。本次全链路重命名为 `/api/active-targets`，硬切换不留旧路径。

## 需求变更

- 端点路径 `GET|DELETE /api/sticky-targets` → `GET|DELETE /api/active-targets`（不再提供旧路径 alias）。
- 后端 DTO `StickyTargetEntry` → `ActiveTargetDto`（避免与 `sag_state::ActiveTargetEntry`
  pin 存储类型撞名；前端类型名为 `ActiveTargetEntry`）。
- 后端 handler `list_sticky_targets` / `delete_sticky_target` → `list_active_targets` /
  `delete_active_target`；请求体类型 `DeleteStickyTargetRequest` → `DeleteActiveTargetRequest`。
- 前端 store `useStickyTargetStore`（`stores/stickyTarget.ts`）→ `useActiveTargetStore`
  （`stores/activeTarget.ts`），方法 `fetchStickyTargets` / `clearStickyTarget` →
  `fetchActiveTargets` / `clearActiveTarget`；组件函数 `clearSticky` → `clearActiveTarget`。

## 代码变更

| 文件 | 变更 |
| --- | --- |
| [`sag-admin/src/admin_active_targets.rs`](../../../sag-admin/src/admin_active_targets.rs) | 由 `admin_sticky.rs` 重命名；DTO / handler / 请求体类型 / 模块 doc / 测试 URL 全部改为 active-targets 命名 |
| [`sag-admin/src/admin.rs`](../../../sag-admin/src/admin.rs) | 路由路径 `/sticky-targets` → `/active-targets`；import handler 新名 |
| [`sag-admin/src/lib.rs`](../../../sag-admin/src/lib.rs) | `pub mod admin_sticky` → `admin_active_targets` |
| [`sag-ui/src/stores/activeTarget.ts`](../../../sag-ui/src/stores/activeTarget.ts) | 由 `stickyTarget.ts` 重命名；store 名 / 方法名 / API URL 更新 |
| [`sag-ui/src/types/api.ts`](../../../sag-ui/src/types/api.ts) | `StickyTargetEntry` → `ActiveTargetEntry` |
| [`sag-ui/src/components/routes/RouteList.vue`](../../../sag-ui/src/components/routes/RouteList.vue) | import store/类型新名；`clearSticky` → `clearActiveTarget`；列 id `"sticky"` → `"activeTarget"` |
| [`sag-ui/src/pages/RoutesPage.vue`](../../../sag-ui/src/pages/RoutesPage.vue) | import store 新名 |

## 测试

- 现有 `sag-admin/src/admin_active_targets.rs` 18 个内联测试全部随重命名更新（helper URL
  改 `/api/active-targets`，断言不变——响应 JSON 结构未变，仅端点路径与标识符名）。
- 全 workspace `cargo test` / `cargo clippy --all-targets` / `cargo fmt --check`、
  前端 `pnpm typecheck` / `pnpm lint` 通过。

## 注意

- 硬切换：旧路径 `/api/sticky-targets` 不再响应，调用方（脚本/外部集成）需同步更新。
- 响应 JSON 结构不变（`{ "targets": [...] }`，字段名不变），仅路径与代码标识符重命名，
  纯 rename 无行为变更。
