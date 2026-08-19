---
created: 2026-08-18
type: 修改
---

# 0006 — 熔断器逻辑概念内聚与探测槽生命周期治理（RAII）

## 动机

熔断器功能完整、状态机健全，但概念分散与生命周期细节泄露：

1. **探测槽生命周期暴露在调度主循环**：`dispatch_chat` / `dispatch_image` 手动传递
   `is_probe` 布尔，并在 `resolve_provider` 为 `None` 时手工调用跨函数的
   `release_probe_slot`。新增调度入口（Embeddings / Audio / Realtime）易漏还槽位；
2. **决策与时长计算散落**：错误分类、配置提取、时长计算与非对称 4xx 规则在
   `dispatch/mod.rs` 胶水代码中散布；
3. **跨模块状态联动缺乏领域封装**：手动恢复健康时清理 StickyFailover 记忆的语义
   硬编码在 admin handler 中，而非下沉到领域状态层；
4. **冗余 Shim 与命名分歧**：`sag-dispatch/src/circuit_breaker.rs` 为 10 行空壳 shim；
   `AppState.circuit_breaker` 与 `DispatchRuntime.route_circuit_breaker` 命名不一致。

本变更以「RAII 守卫」收敛槽位生命周期（判定/归还收敛为一次消费式调用，漏还由 Drop
兜底），并把业务语义下沉到 `sag-state`，同时移除 shim 统一命名。**行为零回归**——所有
既有不变式（三态机、周期探测、4xx 非对称、粘性路由协同）原样保留。

## 需求变更

- 无行为变更，无新增/删除需求条目。既有「`resolve_provider` 失败时若已占用探测槽须
  释放」与「手动关闭熔断器时同步清除 sticky 记忆」两条需求原样满足（实现方式分别收敛
  至 `ProbeGuard::release_probe_slot` 与 `AppState::force_close_circuit`）。

## 代码变更

| 文件 | 变更 |
|------|------|
| `sag-dispatch/src/dispatch/mod.rs` | 新增 `CircuitPermit`（`Allowed` / `Probe`）与 `ProbeGuard`（单次完成标记 + 消费式 `release_probe_slot` + `Drop` best-effort detach）；`circuit_allows_target` 接收整个 `CircuitBreakerConfig` 并返回 `Option<CircuitPermit>`；`apply_circuit_outcome` 改为按值消费 `CircuitPermit`；删除 `CircuitVerdict` 结构与顶层 `release_probe_slot` |
| `sag-dispatch/src/dispatch/chat/mod.rs` | 消除 `is_probe` 手动布尔；provider unresolved 分支改为 `guard.release_probe_slot().await` 同步归还；结果判定收敛为一次消费式 `apply_circuit_outcome(runtime, permit, …)` |
| `sag-dispatch/src/dispatch/image.rs` | 同上（image 路径） |
| `sag-state/src/lib.rs` | `AppState` 新增领域方法 `force_close_circuit(model, provider, real_model)`：顺序执行 `circuit_breaker.close` + `active_targets.clear`（依赖既有 generation/epoch 版本校验保证最终一致） |
| `sag-admin/src/admin_circuit_breaker.rs` | `close_circuit_breaker` handler 简化为直接调用 `state.force_close_circuit(...)`；移除顶层 `RouteTargetKey` 导入 |
| `sag-dispatch/src/circuit_breaker.rs` | **删除**（历史 re-export shim） |
| `sag-dispatch/src/lib.rs` | 删除 `pub mod circuit_breaker;` 及对应 re-export |
| `sag-dispatch/src/state.rs` | `DispatchRuntime.route_circuit_breaker` 更名为 `circuit_breaker`（含 `Default` / `with_shared_state`）；引用改 `sag_core::RouteCircuitBreaker` |
| `sag-dispatch/src/dispatch/chat/tests.rs`、`image.rs`（测试） | `crate::circuit_breaker::RouteTargetKey` → `sag_core::RouteTargetKey`；`rt.route_circuit_breaker` → `rt.circuit_breaker` |
| `sag-router/src/lib.rs` | `RouteCircuitBreaker` / `RouteTargetKey` 重导出从 `sag_dispatch` 移至 `sag_core` 直供 |
| `sag-gateway/src/main.rs` | 局部变量 / `spawn_threshold_watcher` 形参 `route_circuit_breaker` → `circuit_breaker` |

## 设计要点

- **生命周期模型**：半开探测槽正常归还路径为同步 `await`（保持「dispatch 返回即归还」），
  消费式 `apply_circuit_outcome` / `release_probe_slot` 先 `mark_completed()` 再 await；
  只有到不了任何 await 点的极端遍历（panic unwind 等）才落 `Drop` 兜底，此时才有
  detach 语义。
- **Drop 兜底**：`Handle::try_current()` 拿到 handle 则 `spawn` 异步归还，拿不到（无
  runtime / shutdown / panic unwind）只打 warn，槽位交给
  `try_acquire_probe` 的 `stale_probe_threshold` 过期回收——模式与
  `deliver_terminal_event` 一致。
- **`force_close_circuit` 一致性**：两把 `tokio::sync::RwLock` 写锁顺序执行、非跨锁原子，
  正确性来自既有 `StickyToken` generation/epoch 版本校验（stale writeback 拒绝）与
  半开探测结果因 entry 已不存在被静默丢弃的既有语义。
- **命名统一**：`AppState.circuit_breaker` 与 `DispatchRuntime.circuit_breaker` 一致；
  `with_shared_state` 方法名与其首参名均保留 `circuit_breaker`。

## 测试

- `sag-dispatch/src/dispatch/mod.rs`（新增 RAII 专项单元测试）：
  - `probe_guard_drop_without_runtime_does_not_panic` — 无 tokio runtime 下 drop 未消费
    的 guard 走 warn 分支而非 panic；
  - `probe_guard_marked_completed_drop_is_noop` — 已置完成标记的 guard drop 为严格 no-op；
  - `probe_guard_drop_with_runtime_releases_slot_detached` — runtime 存在时 Drop detach
    异步归还，槽位重新可抢占；
  - `apply_circuit_outcome_consumed_probe_failure_reschedules_once` — 消费式判定后 guard
    Drop no-op，`record_probe_result` 恰一次（entry Open + `probe_in_flight` 已清）；
  - `apply_circuit_outcome_success_closes_healthy_and_probe` — Success 关闭健康/探测目标；
  - `apply_circuit_outcome_exhausted_opens_healthy_with_duration` — Exhausted 按错误时长开闸；
  - `apply_circuit_outcome_non_retryable_healthy_does_not_open` — 健康目标 4xx 不开闸（非对称）；
  - `apply_circuit_outcome_non_retryable_probe_reschedules_not_cooldown` — 探测中 4xx 仅推后
    interval、不进入整段冷却；
  - `probe_guard_release_probe_slot_releases_once` — 消费式 `release_probe_slot` 单次完成、
    槽位同步归还。
- 既有高敏感回归（行为不变，全部通过）：
  - `dispatch_chat_releases_probe_slot_when_provider_unresolved` /
    `dispatch_image_releases_probe_slot_when_provider_unresolved` — provider unresolved 的
    同步归还 + 恰一次；
  - `dispatch_chat_half_open_probe_in_flight_skips_others` —
    `dispatch_image_half_open_probe_recovers_target` 等并发/泄漏测试；
  - `sag-admin` 的 `force_close_*` 系列（`force_close_removes_open_breaker`、
    `force_close_clears_sticky_failover_active_target`、
    `force_close_rejects_stale_writeback_without_per_model_tombstone` 等）验证
    `force_close_circuit` 语义不变。
