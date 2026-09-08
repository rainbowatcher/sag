---
created: 2026-09-08
type: 修改
---

# 0005 — target_index 权威首项判定与策略切换残留清理

## 动机

0003/0004 的「首项判定」依赖前端按响应顺序 + 对象身份推断 pin 落在 route.targets 哪个 target，
脆弱且无法跨请求比对。此外三个策略切换残留问题此前没有系统清理路径：

- 路由从 StickyFailover 切到 FillFirst / RoundRobin 后，`active_targets` 里的旧 pin 不再被
  写入/清除，GET 仍会把它报成 pinned/expired，误导运维；
- DELETE 按当前 strategy 分发时，切到 FillFirst 的旧 pin 没有手动清理入口；
- `prune_stale_entries` 只在下一 dispatch 发现 `routes_version` 变化时执行，低流量路由的
  残留 pin 会长时间可见。

本次：后端 DTO 直接携带 `target_index`（0-based 原始 route.targets 索引）作为首项判定的
权威来源；GET 只对 StickyFailover 路由展示 pin；DELETE 对每种策略都清 `active_targets`、
FillFirst 额外清 order cache；route CRUD 与 gateway hot-reload 成功后立即 proactive prune。

## 需求变更

- 本模块无 `requirements.md`（只有 spec.md），按要求跳过需求文档更新，不新建空文件。
- 0003/0004 中「`isActiveFirstTarget` 用对象身份」的描述被本节取代：改为后端
  `ActiveTargetDto.target_index === 0` 判首项（spec.md「状态圆点颜色语义」节同步更新）。
- GET 语义补充：非 StickyFailover 路由不展示 `active_targets` 残留 pin（报 idle）。
- DELETE 语义补充：所有策略都 `active_targets.clear(model)`（清除跨策略残留 pin），
  FillFirst 额外 `fill_first_orders.remove(model)`。
- 新增 proactive prune：route CRUD（create/update/delete）与 gateway 主 hot-reload（含
  unstable-file fallback retry 成功路径）成功后立即 `retain_sticky_routes` /
  `prune_stale_entries`。

## 代码变更

| 文件 | 变更 |
| --- | --- |
| [`sag-admin/src/admin_active_targets.rs`](../../../../sag-admin/src/admin_active_targets.rs) | `ActiveTargetDto` 新增 `target_index`（0-based 原始 route.targets 索引，与 priority 无关）；GET 只对 `Strategy::StickyFailover` 路由匹配 `active_targets`（FillFirst/RoundRobin 残留 pin 报 idle）；DELETE 对每种策略都 `active_targets.clear(model)`，FillFirst 额外清 order cache |
| [`sag-admin/src/admin_routes.rs`](../../../../sag-admin/src/admin_routes.rs) | create/update/delete route 成功后调 `prune_stale_active_targets`（`retain_sticky_routes`），立即清策略切换/删路由残留 pin |
| [`sag-state/src/lib.rs`](../../../../sag-state/src/lib.rs) | `ActiveTargetsInner` 新增 `retain_sticky_routes`：key 必须仍解析到记录的 route **且**该 route 仍为 StickyFailover，否则 drop 并 bump generation；generation-only tombstone 只要 key 仍解析到任意路由就保留 |
| [`sag-dispatch/src/state.rs`](../../../../sag-dispatch/src/state.rs) | `prune_stale_entries` 的 active_targets 分支改用 `retain_sticky_routes`（策略感知）；测试新增策略切换用例 |
| [`sag-gateway/src/main.rs`](../../../../sag-gateway/src/main.rs) | `GatewayReloadHandler` 主 reload 成功后 `prune_stale_entries`；unstable-file fallback retry 成功路径同样先读 routes 再 prune（与主路径一致，避免死锁/重复 prune） |
| [`sag-ui/src/types/api.ts`](../../../../sag-ui/src/types/api.ts) | `ActiveTargetEntry` 新增 `target_index: number` |
| [`sag-ui/src/lib/activeTarget.ts`](../../../../sag-ui/src/lib/activeTarget.ts) | `isActiveFirstTarget` 改 `target_index === 0` 判定（缺失/非 0 保守返回 false）；`selectActiveTarget` 增加 route 当前 strategy 过滤（非 sticky_failover 返回 undefined） |
| [`sag-ui/src/components/routes/RouteList.vue`](../../../../sag-ui/src/components/routes/RouteList.vue) | Active Target 列 sortingFn 同时计入 sticky pin 与 fill-first 当前 target（`selectFillFirstTarget`，无缓存 order 不算 active） |
| [`sag-ui/src/lib/activeTarget.test.ts`](../../../../sag-ui/src/lib/activeTarget.test.ts) | 测试随 target_index / strategy 过滤更新；新增 fill_first 无缓存 order 不算 active 用例 |

## 测试

- `target_index_mirrors_route_targets_array_order` — `target_index` 取自 route.targets 数组位置（与 priority 无关），pin 后仍报其配置数组索引
- `fill_first_route_ignores_stale_active_targets_entry` — 切到 FillFirst 后残留 sticky pin（含 expired）报 `idle`、无 key、age 0
- `delete_fill_first_resets_order_cache_and_clears_stale_pin` — fill-first DELETE 清 order cache + 清策略切换残留 pin，重复 DELETE 幂等（取代原 `delete_fill_first_resets_order_cache_and_is_idempotent`）
- `update_route_strategy_switch_prunes_stale_active_target`（admin_routes.rs）— admin API 把 StickyFailover 改成 FillFirst 后立即清残留 pin
- `prune_stale_entries_drops_active_pin_after_strategy_switch_to_fill_first`（sag-dispatch state.rs）— prune 策略感知；fill_first_orders / route_counters 对仍解析的 key 保留
- `retain_sticky_routes_drops_entries_for_non_sticky_routes`（sag-state lib.rs）— keep 规则（key 仍解析到记录的 route 且该 route 仍为 StickyFailover）
- 前端 `activeTarget.test.ts`：target_index 0/1+/缺失/非法值判首项、expired 灰绿/灰橙、`selectActiveTarget` 按 route 当前 strategy 过滤、fill_first 无缓存 order 不算 active（sorting 不误判）

## 注意

- **target_index 权威来源在后端**：`target_index === 0` 即 route 配置首项，前端不依赖响应顺序/对象身份/`real_model` 字符串；缺失或非 0 保守返回 false（渲染橙/灰橙，绝不误报绿）。
- **GET 隐藏残留 pin 不改变 dispatch**：GET 只是不展示非 StickyFailover 路由的残留 pin，`active_targets` 内存与 dispatch 语义不变；残留由 proactive prune 或下一 dispatch prune 清除。
- **DELETE 跨策略清理保持幂等**：对不存在 key 的 clear/remove 是 no-op，仍返回 200。
- **prune 的 fill_first_orders / route_counters 非策略感知**：按请求 model key 重新解析，wildcard key 只要仍解析到任意路由就保留（下次请求按新 config 重新洗牌/计数）；只有 active_targets 是策略感知的。
- **gateway fallback retry prune**：`main.rs` unstable-file retry 成功路径现在与主 reload 路径一致（先读 config 快照再 `prune_stale_entries`），`DispatchRuntime` / `ConfigHandle` 均为 Arc-backed Clone，闭包 move 捕获，无锁序交互与重复 prune。
