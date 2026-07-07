---
created: 2026-07-06
updated: 2026-07-06
type: 修复
---

# 0002 — 路由删除后 dispatch 缓存泄漏修复

## 动机

当路由通过 Admin API 删除（或配置热重载）后，`DispatchRuntime` 中的三个缓存 map——
`fill_first_orders`、`route_counters`、`active_targets`——不会自动清理对应的旧条目。
`resolve_route()` 在找不到路由时直接返回 `ModelNotFound`，不再进入 `order_targets()`，
导致已删除路由的缓存条目永远不会被访问或清理。频繁 create/delete 路由时，
这三个 `HashMap` 会无限增长，造成内存泄漏。

## 需求变更

- requirements.md「分发流程」节追加：每次 `resolve_route()` 调用开头执行惰性缓存清理

## 代码变更

| 文件 | 变更 |
|------|------|
| [sag-config/src/loader/handle.rs](../../../../sag-config/src/loader/handle.rs) | `ConfigHandle` 新增 `routes_version` 计数器 + `bump_routes_version()` / `routes_version()` 方法 |
| [sag-config/src/loader/reload.rs](../../../../sag-config/src/loader/reload.rs) | 热重载后递增 `routes_version` |
| [sag-admin/src/admin_routes.rs](../../../../sag-admin/src/admin_routes.rs) | create/update/delete 路由后调用 `bump_routes_version()` |
| [sag-dispatch/src/state.rs](../../../../sag-dispatch/src/state.rs) | 新增 `prune_stale_entries()` 方法 + `last_prune_version` 字段，清理三个缓存 map 中 config 不存在的路由条目 |
| [sag-dispatch/src/dispatch/resolve.rs](../../../../sag-dispatch/src/dispatch/resolve.rs) | `resolve_route()` 开头对比 `routes_version`，版本未变则跳过剪枝；版本变更则执行 `prune_stale_entries()` 并更新缓存版本 |
| [sag-dispatch/src/order.rs](../../../../sag-dispatch/src/order.rs) | `CachedOrder` 字段可见性改为 `pub(crate)` 以支持测试 |

## 测试

- `prune_stale_entries_removes_deleted_routes` — 验证过期条目删除、有效条目保留
- `prune_stale_entries_no_op_when_all_valid` — 验证全有效时无副作用
- `resolve_route_prunes_stale_entries_after_route_deletion` — 端到端：创建路由 → 填充缓存 → 删除路由 → 下一次请求清理三个 map

## 影响的需求条目

- 分发流程（追加惰性缓存清理行为）
