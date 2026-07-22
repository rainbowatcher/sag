---
created: 2026-07-22
---

# 0005 — 手动关闭熔断时清除 StickyFailover 粘性记忆

## 动机

在 `StickyFailover` 策略下，当高优先级目标 A 的熔断器打开后，流量 failover 到
备份目标 B，`dispatch_chat` / `dispatch_image` 在成功时将 B 写入
`active_targets[model]`。此后 `order_targets` 把 B prepend 到有序列表首位，
即使 A 的熔断器被手动关闭（`POST /api/circuit-breaker/close`），A 也不会被尝试——
B 仍然是 sticky active target，且 B 成功后提前 return，A 永远排在后面。

**根因**：`close_circuit_breaker` admin handler 只能访问 `AppState`，而
`active_targets` 住在 `DispatchRuntime`。两者此前只共享 `circuit_breaker` 这一个
`Arc`，admin handler 无法触及 sticky 记忆。

## 改动

### `active_targets` 提升为共享状态

仿照 `circuit_breaker` 的现有共享 `Arc` 模式，将 `active_targets` 从
`DispatchRuntime` 私有字段提升为 `AppState` 字段，通过同一个 `Arc` 注入两边：

- `sag-state/src/lib.rs`：新增类型别名 `ActiveTargets` 与 `AppState::active_targets`
  字段；`test_app_state` 初始化为 `Arc::default()`。
- `sag-dispatch/src/state.rs`：`DispatchRuntime::active_targets` 类型改为
  `sag_state::ActiveTargets`；新增 `with_shared_state(breaker, active_targets)`
  构造器，移除原 `with_circuit_breaker`（它使用 `..Default::default()`
  会创建非共享的 `active_targets` Arc，若被调用会静默重现本 bug）。
- `sag-gateway/src/main.rs`：创建一个共享 `active_targets` `Arc`，分别注入
  `AppState` 和 `DispatchRuntime::with_shared_state`。
- `sag-router/src/lib.rs`：重导出 `ActiveTargets`。

### `close_circuit_breaker` 清除粘性记忆

`sag-admin/src/admin_circuit_breaker.rs`：在 `close(&key)` 之后无条件执行
`state.active_targets.write().await.remove(&req.model)`。

**语义**：手动关闭熔断是「宣告目标恢复，让路由重新按优先级评估」的信号，与
`sticky_failover_reset_secs` 自然过期走同一分支——清除 sticky 记忆后排序回退到
fill-first 优先级顺序，高优先级目标自然回到队首。

**无条件清除**：不检查当前 sticky 是否指向被关闭的目标。如果 sticky 已指向被关闭
的目标（A 健康 sticky），清除后下一请求按 fill-first 仍命中 A，A 成功后重新成为
sticky——仅一次请求的微小 churn，换取最简语义。

**隐式 reset-sticky 入口**：`close` 幂等（关闭已 Closed 的 breaker 是 no-op），
因此 API 用户可对任意目标 POST `close` 来重置该 model 的 sticky，无需专用端点。

## 代码变更

| 文件 | 变更 |
|---|---|
| `sag-state/src/lib.rs` | 新增 `ActiveTargets` 类型别名与 `AppState::active_targets` 字段；`test_app_state` 初始化 |
| `sag-dispatch/src/state.rs` | `active_targets` 字段类型改为 `sag_state::ActiveTargets`；新增 `with_shared_state`；移除 `with_circuit_breaker` |
| `sag-gateway/src/main.rs` | 创建共享 `active_targets` Arc 注入 AppState 与 DispatchRuntime |
| `sag-admin/src/admin_circuit_breaker.rs` | `close_circuit_breaker` 清除 `active_targets[model]`；2 个回归测试 |
| `sag-router/src/lib.rs` | 重导出 `ActiveTargets` |

## 需求变更

`requirements.md`「冷却期结束恢复路径」节中 StickyFailover 行为补充：

> 手动关闭熔断器（`POST /api/circuit-breaker/close`）时，同步清除该 model 的
> sticky active target 记忆，使下一次请求回退到 fill-first 优先级顺序。语义与
> `sticky_failover_reset_secs` 自然过期一致。

## 不变式

- `AppState` 与 `DispatchRuntime` 共享同一个 `Arc<RwLock<HashMap<…>>>`
  active_targets 实例（与 `circuit_breaker` 共享模式对齐）。
- `close_circuit_breaker` 的两把写锁（breaker、active_targets）顺序持有、不嵌套，
  无锁序死锁风险。
- `DispatchRuntime::default()` 仍创建自己的 `active_targets` Arc（仅供测试
  fixture），生产路径必须通过 `with_shared_state` 共享。

## 测试

- `sag-admin/src/admin_circuit_breaker.rs`：
  - `force_close_clears_sticky_failover_active_target` — 打开 breaker、设置
    active_targets 指向其他 provider、调用 close、断言 active_targets 已清除。
  - `force_close_on_already_closed_clears_sticky_anyway` — 无 open breaker、
    设置 active_targets、调用 close、断言 sticky 已清除（验证隐式 reset 入口）。
- 既有 `sticky_failover_resets_active_after_ttl`（`order.rs`）已覆盖
  active_targets 为空时回退 fill-first baseline 的排序行为，与 admin 测试
  间接拼接为完整覆盖。
