---
created: 2026-10-25
updated: 2026-10-25
type: 修复
---

# 0006 — 拒绝 `session_sticky` + `pipeline = "image"`，admin 侧补齐 session_affinity prune

## 动机

0003 留下两个静默缺口：

1. **image 管线的粘性从不发生**。`dispatch_image` 丢弃 `session_writeback`，因此
   `session_sticky` 的 image 路由永远不会记录 pin，实际行为退化成纯 A-Res 加权随机 ——
   配置看起来生效、粘性一次也不会命中。按仓库既有的「带语义的枚举值不静默回退」原则
   （静默退化比报错更危险），应在配置校验期 fail-fast。
2. **管理端路由变更后 pin 残留**。路由变更触发的共享内存 prune 只在 dispatch 路径上做
   （`DispatchRuntime::prune_stale_entries` 已包含 `session_affinity`），而 admin 的
   `prune_stale_routing_state` 只清了 `fill_first_orders` / `active_targets` 两项
   （`session_affinity` 正是本次补上的）。经管理 API 删除（或把 strategy 切走）一条
   `session_sticky` 路由后，其 pin 要等下一次请求触发 dispatch 侧 prune 才被清；期间
   `/api/session-affinity` 快照仍展示已失效路由的会话，且一个只读管理实例不会自己自愈。
   同时 `route_counters` 在本次之前只存在于 `DispatchRuntime`（`AppState` 无该字段），
   admin 物理上触达不到，两侧清理天然不同源——删除一条 `round_robin` 路由后其计数器同样
   要等下一次请求才被清。因此本次把 `RouteCounters` 提升为 `AppState` 共享句柄，让两侧
   走同一套清理实现（见「代码变更」）。

### 评审返工（同日 Minor 收敛）

首轮实现通过评审后，评审又提出 3 项修复（1 条 Major + 2 条 Minor），均已并入本变更
（同一未发布变更的收尾，不另开编号）：

1. **新增校验：通配路由 × `session_sticky` 直接拒绝**（`sag-config/src/config.rs:532-548`）。
   session affinity 的 space 按**请求模型**分片，每个 space 至多 `session_max_entries` 个
   pin 加 `max(capacity, 1024)` 条墓碑；`model_name = "*"` 匹配所有请求模型，space 键域
   由客户端模型名控制且**无上限**（内存随模型数 × 容量放大）。原代码只在 image 管线上
   fail-fast，这一组合被静默接受。现 `validate_routes` 对
   `Strategy::SessionSticky` + `route.model_name == "*"` 报错并提示「为每个模型写精确
   路由，或改用 `sticky_failover` / `fill_first` / `round_robin`」。附带效果：
   「space 数量 = 配置的路由模型数」这一有界前提重新成立，requirements / spec 已同步限定。
2. **admin 路由 CRUD 补齐 session 字段表面（原 Major）**（`sag-admin/src/admin_routes.rs`）。
   `session_ttl_secs` / `session_max_entries` 此前只在配置模型里存在，admin 侧完全触达不到，
   新建的 `session_sticky` 路由只能吃内置默认（3600 / 10000），唯一管理途径是手改 TOML。
   现 `CreateRouteRequest` 接受两个字段并在 `create_route` 直接透传；
   `UpdateRouteRequest` 沿用与 `retries` 一致的「不传 = 保留」合并语义（JSON `null` 与省略
   不可区分，故 update 只能设、不能清）；`RouteResponseItem` 带
   `skip_serializing_if = "Option::is_none"`，`GET /api/routes` 可见、未设时省略。
   `session_max_entries = 0` 仍由 coordinator 的全量 validate 拒绝（`AppError::Config` → 400），
   admin 侧不重复校验。
3. **`DELETE /api/session-affinity {"all":true}` 的 `deleted` 计数改单写锁原子**
   （`sag-admin/src/admin_session_affinity.rs:179-185`）。原实现先在一次读锁里
   `snapshot().len()` 计数、再在一次写锁里 `clear_all()`，两步之间插入的 pin 会被删掉却不
   计数（低估）。现计数与清理在同一个写锁临界区内完成，与其余 scope（`key` / `model`）的
   先数后清一致。

## 需求变更

- 「预期行为 → 会话粘性路由」新增第 6 条：`session_sticky` 仅支持 `pipeline = "chat"`，
  image 管线不做会话写回，`session_sticky` + `pipeline = "image"` 在配置校验阶段直接
  报错，不静默退化为纯加权随机。
- 新增第 7 条：通配路由（`model_name = "*"`）不得配置 `session_sticky`，同样在配置校验
  阶段报错（session affinity 的 space 按请求模型分片，通配路由的键域由客户端模型名控制）。
- 管理端 prune 属内部一致性（两处 prune 共用同一组清理实现：`retain_resolving_route_keys`
  与 `retain_sticky_routes`），不新增需求条目。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-config/src/config.rs`](../../../../sag-config/src/config.rs) | `validate_routes` 新增 strategy × pipeline 交叉校验：`Pipeline::Image` + `Strategy::SessionSticky` 报错并给出替代策略提示；`Strategy::SessionSticky` 文档补「仅 chat 管线」约束。评审返工又新增 strategy × 通配交叉校验（`model_name = "*"` + `session_sticky` 报错，`config.rs:532-548`） |
| [`sag-state/src/lib.rs`](../../../../sag-state/src/lib.rs) | 新增 `RouteCounters` 类型（`Arc<RwLock<HashMap<String, u64>>>`）与 `AppState::route_counters` 字段（与 `DispatchRuntime` 共享同一 `Arc`）；新增唯一清理实现 `retain_resolving_route_keys`（按**请求模型**重新 `resolve_model_route` 判定存活，wildcard 路由的具体模型键仍解析到通配符 → 保留） |
| [`sag-admin/src/admin_routes.rs`](../../../../sag-admin/src/admin_routes.rs) | `prune_stale_routing_state` 增加 `state.session_affinity.write().await.retain_sticky_routes(&routes)`（与 `active_targets` 同一条策略感知 keep 规则：策略切走 / 路由删除 / 精确路由遮蔽通配符 → 丢弃 pin），并补 `route_counters` 清理；`fill_first_orders` / `route_counters` 改调共享的 `sag_state::retain_resolving_route_keys`，四处清理的锁序与 dispatch 侧一致。评审返工补齐 session 字段表面：`CreateRouteRequest` / `UpdateRouteRequest` 新增 `session_ttl_secs` / `session_max_entries`，`create_route` 透传、`update_route` 「不传=保留」、`RouteResponseItem` 未设时省略 |
| [`sag-admin/src/admin_session_affinity.rs`](../../../../sag-admin/src/admin_session_affinity.rs) | 评审返工：`DELETE {"all":true}` 的 `deleted` 计数改在同一写锁临界区内先数后清，消除「计数与清理之间插入的 pin 被删而不计」的低估 |
| [`sag-dispatch/src/state.rs`](../../../../sag-dispatch/src/state.rs) | `DispatchRuntime::with_shared_state` 增 `route_counters` 参数；`prune_stale_entries` 的 `fill_first_orders` / `route_counters` 改调 `sag_state::retain_resolving_route_keys`，与 admin 侧同源 |
| [`sag-gateway/src/main.rs`](../../../../sag-gateway/src/main.rs) | 装配共享 `route_counters`：同一 `Arc` 注入 `AppState` 与 `DispatchRuntime`，使 admin prune 对 dispatch 立即可见 |
| [`sag-router/src/lib.rs`](../../../../sag-router/src/lib.rs) | re-export `RouteCounters`，供 gateway 与 admin 侧引用同一类型 |
| `AppState` 字面量 / `with_shared_state` 调用点 | 8 处 `AppState` 字面量（`sag-router/src/routes.rs`、`sag-router/src/handlers/analytics_v2.rs`、`sag-state/src/lib.rs` 的 `test_app_state`、`sag-gateway/src/main.rs`、`sag-dispatch/src/dispatch/{image,mod,resolve}.rs`，其中 `mod.rs` 2 处）与 4 处 `with_shared_state` 调用点（`sag-dispatch/src/dispatch/chat/tests.rs`）同步补 `route_counters` |
| [`sag-admin/tests/route_prune.rs`](../../../../sag-admin/tests/route_prune.rs) | 新增管理端 prune 集成测试（admin API → coordinator → 共享 `AppState` 内存全链路），覆盖 session pin 与 round-robin 计数器两侧 |

## 测试

- `rejects_session_sticky_on_image_pipeline`（`sag-config`）— image 路由配
  `session_sticky` 校验失败，错误信息含 strategy 与 pipeline 两侧
- `accepts_session_sticky_on_chat_pipeline`（`sag-config`）— chat 路由配
  `session_sticky` 正常通过
- `delete_route_prunes_session_affinity_pins`（`sag-admin/tests/route_prune.rs`）—
  删除 `session_sticky` 路由后其 pin 被清
- `strategy_switch_prunes_session_affinity_pins` — 路由 strategy 切走后 pin 被清
- `delete_route_prunes_round_robin_counters` — 删除 `round_robin` 路由后其
  `route_counters` 条目被清，仍存活的 `round_robin` 路由计数器不受影响（不过度清理）
- `create_image_route_with_session_sticky_is_rejected` — 经 admin API 创建
  `session_sticky` + `pipeline = "image"` 路由返回 400

评审返工新增：

- `rejects_session_sticky_on_wildcard_route`（`sag-config/src/config.rs:3372`）— 通配路由配
  `session_sticky` 校验失败，错误信息指明「不支持通配路由」与替代策略
- `accepts_session_sticky_on_explicit_route_beside_wildcard`（`config.rs:3428`）— 通配路由
  仍在、精确路由单独配 `session_sticky` 正常通过（不过度拒绝）
- `create_route_round_trips_session_fields`（`sag-admin/src/admin_routes.rs:1030`）—
  `POST /api/routes` 带 `session_ttl_secs` / `session_max_entries` 落盘并在 `GET` 可见
- `list_routes_omits_unset_session_fields`（`admin_routes.rs:1095`）— 未设时响应省略字段
- `update_route_sets_session_fields`（`admin_routes.rs:1120`）— `PUT` 可设置两个字段
- `update_route_keeps_session_fields_when_omitted`（`admin_routes.rs:1161`）— 不传即保留
- `delete_all_counts_pins_across_spaces`（`sag-admin/src/admin_session_affinity.rs:376`）—
  `{"all":true}` 的 `deleted` 跨 space 计数正确，重复清理返回 0
