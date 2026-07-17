---
created: 2026-07-17
---

# 0004 — 熔断状态可观测化 (Circuit Breaker Observability)

## 动机

熔断器三态机 (`Closed → Open → HalfOpen`) 已完整实现，但状态完全不可观测：
- 无 admin API 暴露当前熔断的 route target；
- `AppState`（admin handler 使用）与 `DispatchRuntime`（持有熔断器）是分离对象；
- 前端无任何熔断相关 UI。

用户无法直观看到「哪些 target 正在熔断、何时恢复」。

## 改动

### 后端

- **类型迁移**：`RouteCircuitBreaker` / `RouteTargetKey` 从 `sag-dispatch` 迁移到 `sag-core`，
  消除 `sag-state → sag-dispatch` 的循环依赖。`sag-dispatch/src/circuit_breaker.rs` 改为
  re-export shim，保持 `crate::circuit_breaker::` 路径向后兼容。
  `sag-core/Cargo.toml` 新增 `fastrand = "2"`（`jittered_interval` 所需）。
  `CircuitHint`（错误分类）已在 `sag-core/error.rs`，迁移后两者同处一 crate，内聚性提升。

- **快照 API**：新增 `RouteCircuitBreaker::snapshot()` 返回
  `Vec<CircuitBreakerEntry>`（可序列化）。
  - `CircuitStateKind` 枚举（`#[serde(rename_all = "snake_case")]`）投影为 `"open"` / `"half_open"`。
  - `next_probe_at_secs: Option<u64>` — `None` for HalfOpen（无未来调度时刻）。
  - `probe_in_flight: bool` — HalfOpen 时是否有探测在飞。
  - Instant→SystemTime 换算使用 `checked_duration_since` 防止过期 entry panic。
  - 输出按 `model → provider → real_model` 排序，保证 API 响应确定性。

- **AppState 共享句柄**：`AppState` 新增 `circuit_breaker: Arc<RwLock<RouteCircuitBreaker>>` 字段。
  `main.rs` 先创建共享 `Arc`，分别注入 `AppState` 和 `DispatchRuntime`（新方法
  `DispatchRuntime::with_circuit_breaker()`），确保两者观察同一份熔断器实例。

- **Admin 端点**：新增 `GET /api/circuit-breaker`，返回 `{ "breakers": [...] }`，
  挂载在 `/api` 下，受 `gateway_auth` 保护。

### 前端

- `sag-ui/src/types/api.ts`：新增 `CircuitBreakerEntry` 接口。
- `sag-ui/src/stores/circuitBreaker.ts`：新增 Pinia store（`fetchBreakers()`、
  `openBreakers` / `halfOpenBreakers` computed），遵循现有 401 处理模式。
- `sag-ui/src/lib/utils.ts`：新增 `formatRemaining(epochSecs, expiredLabel)` 助手
  （秒→毫秒单位转换，区分「已可恢复」/「已可探测」）。
- `sag-ui/src/components/circuit/CircuitBreakerPanel.vue`：新组件，列出所有非 Closed
  target，显示状态徽章（Open=红/HalfOpen=黄）、探测中指示、静态剩余恢复时间 + 下次探测时间。
- `sag-ui/src/pages/DashboardPage.vue`：挂载 `CircuitBreakerPanel`，`onMounted` 新增
  `fetchBreakers()` 调用。

## 对外 API

```
GET /api/circuit-breaker
→ 200 { "breakers": [CircuitBreakerEntry, ...] }
```

`CircuitBreakerEntry`:
```json
{
  "model": "gpt-4o",
  "provider": "openai",
  "real_model": "gpt-4o",
  "state": "open",          // "open" | "half_open"
  "cooldown_until_secs": 1784276491,
  "next_probe_at_secs": 1784272973,  // null for half_open
  "probe_in_flight": false
}
```

## 不变式

- 快照是只读的，不触发 `prune_expired`（仅在写路径调用）。
- 过期 entry 的 `cooldown_until_secs` / `next_probe_at_secs` 被 clamp 到 `now`（不 panic）。
- `AppState` 与 `DispatchRuntime` 共享同一个 `Arc<RwLock<RouteCircuitBreaker>>`。
- 前端不轮询，手动刷新才更新（静态剩余时间）。

## 测试

- `sag-core/src/circuit_breaker.rs` — snapshot 单测：空态、Open（`next_probe_at` = Some）、
  HalfOpen（`next_probe_at` = None）、过期不 panic、serde snake_case。
- `sag-admin/src/admin_circuit_breaker.rs` — 端点集成测试：空态返回空数组、
  Open entry 出现在响应、HalfOpen entry 的 `next_probe_at_secs` 为 null、过期 entry 不 panic。
