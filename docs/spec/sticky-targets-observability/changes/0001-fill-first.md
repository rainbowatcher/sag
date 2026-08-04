---
created: 2026-08-04
type: 新增
---

# 0001 — FillFirst 手动重置与可观测性

## 动机

`/api/sticky-targets` 的 GET/DELETE 语义完全锚定在 `active_targets` 内存上，而
`FillFirst` 策略从不读写这块内存——它的"粘性"是 `fill_first_orders` 缓存里的
洗牌顺序（index 0 吸收所有流量直到不可用）：

- GET 对 FillFirst 路由恒报 `idle`（误导：实际流量正被全部灌到缓存顺序首位）；
- DELETE 对 FillFirst 路由是 routing no-op（`active_targets.clear` 只 bump
  generation/epoch，FillFirst 决策从不读它们），用户无法手动触发重新洗牌。

FillFirst 只有 TTL（`shuffle_cache_ttl_secs`，默认 7200s）自动过期一条重置路径，
与 StickyFailover 的 `sticky_failover_reset_secs` 自动过期 + 手动 DELETE 两条路径
不对称。本次补齐手动重置：**扩展现有 `/api/sticky-targets`，DELETE 对 FillFirst
路由丢弃缓存顺序（下次请求重新洗牌，TTL 等价物），GET 报缓存顺序位置**。

附带修复：wildcard sticky 路由 DELETE 清除失效的 UI bug——改动前前端用
`route.model_name`（wildcard 时为 `"*"`）回填 DELETE body，后端
`active_targets.clear("*")` 找不到真实 pin key（记在具体请求模型名下），
对 wildcard sticky 路由是静默 no-op；新增 `key` 字段透传真实 map key 后修复。

## 需求变更

- `GET /api/sticky-targets`：对 FillFirst 路由的每个 target 新增
  `strategy` / `fill_first_position` / `fill_first_cache_age_secs` /
  `fill_first_cache_ttl_secs` 字段（position 0 = 当前 fill target）。
- `DELETE /api/sticky-targets`：按模型解析 route 的 strategy 分发——
  FillFirst 丢弃 `fill_first_orders[model]`，其余策略保持现有
  `active_targets.clear(model)` 行为。幂等，无缓存时仍 200。

## 代码变更

| 文件 | 变更 |
| --- | --- |
| [`sag-state/src/lib.rs`](../../../sag-state/src/lib.rs) | `CachedOrder` 从 `sag-dispatch` 下沉（字段转 pub），新增 `FillFirstOrders` type alias；`AppState` 新增 `fill_first_orders` 字段，与 `DispatchRuntime` 共享同一 `Arc` |
| [`sag-dispatch/src/order.rs`](../../../sag-dispatch/src/order.rs) | 删除本地 `CachedOrder` 定义，改用 `sag_state::CachedOrder` |
| [`sag-dispatch/src/state.rs`](../../../sag-dispatch/src/state.rs) | `DispatchRuntime.fill_first_orders` 类型改 `sag_state::FillFirstOrders`；`with_shared_state` 新增注入参数；`CachedOrder` import 移入测试模块（仅测试用，避免 clippy 未使用告警） |
| [`sag-dispatch/src/lib.rs`](../../../sag-dispatch/src/lib.rs) | `CachedOrder` re-export 改从 `sag_state` |
| [`sag-router/src/lib.rs`](../../../sag-router/src/lib.rs) | re-export `FillFirstOrders` |
| [`sag-gateway/src/main.rs`](../../../sag-gateway/src/main.rs) | 创建 `fill_first_orders` Arc 注入 `AppState` + `DispatchRuntime::with_shared_state` |
| [`sag-config/src/config.rs`](../../../sag-config/src/config.rs) | `Strategy` 新增 `as_str()`（snake_case 标识，供 admin API 标注 strategy） |
| [`sag-admin/src/admin_active_targets.rs`](../../../sag-admin/src/admin_sticky.rs) | `StickyTargetEntry` 新增 `strategy` / `fill_first_position` / `fill_first_cache_age_secs` / `fill_first_cache_ttl_secs`；GET 对 FillFirst 路由读 `fill_first_orders` 快照报位置与缓存元数据；DELETE 按 route strategy 分发（FillFirst → 清缓存，其余 → 清 pin） |
| [`sag-ui/src/types/api.ts`](../../../sag-ui/src/types/api.ts) | `StickyTargetEntry` 新增 4 个字段 |
| [`sag-ui/src/components/routes/RouteList.vue`](../../../sag-ui/src/components/routes/RouteList.vue) | Active Target 列对 FillFirst 路由显示 position 0 target（"Fill" 徽标 + 缓存 age tooltip）；清除按钮按策略传参（FillFirst 传 route model name，sticky 传 pin key）；toast 按策略区分文案 |

## 测试

- `fill_first_route_reports_strategy_and_null_cache_fields_when_absent` — FillFirst 路由无缓存时 `strategy: "fill_first"`、缓存字段全 null、status 仍 idle
- `fill_first_route_reports_cached_order_position_and_age` — 注入确定顺序缓存后 position 0/1 正确、cache age 范围断言、TTL 报默认 7200
- `fill_first_cache_reports_zero_ttl_as_never_expire` — `shuffle_cache_ttl_secs = 0` 时 TTL 字段报 0（非 null）
- `delete_fill_first_resets_order_cache_and_is_idempotent` — DELETE 清缓存、不清 active pin、重复 DELETE 仍 200
- `delete_sticky_does_not_touch_fill_first_cache` — sticky DELETE 只清 pin，不碰 fill-first 缓存（反向隔离）
- `delete_fill_first_on_wildcard_route_resets_by_route_name` — wildcard FillFirst 路由按 `"*"` key 清缓存

## 已知边界

- **GET 快照的 routes_version 不校验**：`list_active_targets` 读 `fill_first_orders` 快照时不比对
  `CachedOrder.routes_version` 与当前 config `routes_version`。配置变更（增删/替换 target）后、
  下一 dispatch 请求刷新缓存前，已变更 target 的 `fill_first_position` 报 `null`（观感层失真，
  下一请求即纠正）。
- **重复 target spec 报 position null**：同一 FillFirst 路由存在两个 `provider` + `real_model`
  完全相同的 target（配置错误场景）时，`position()` 只匹配首个索引，后续重复项
  `fill_first_position` 报 `null`——属用户配置责任，dispatch 行为不受影响。

全 workspace `cargo test` / `cargo clippy --all-targets` / `cargo fmt --check`、
前端 `pnpm typecheck` / `pnpm lint` 通过。
