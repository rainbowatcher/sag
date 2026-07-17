---
created: 2026-07-17
updated: 2026-07-17
type: 修复
---

# 0003 — 探测失败隐瞒冷却下界与流式取消探测槽泄漏修复

## 动机

上一轮 0002 引入的半开探测实现存在两个正确性缺陷，导致冷却保护被周期探测链路绕过、以及探测槽在 future 取消时永久泄漏。

## 需求变更

- 无行为需求变更（两个 bug 均是让实现回归到 spec/requirements 已声明的语义：「探测失败仅重排 next_probe_at、不重开整段冷却」「探测槽必须被释放」）。仅补充「自愈机制」说明：探测 future 被取消未归还槽位时，由 `try_acquire_probe` 在超过 `STALE_PROBE_THRESHOLD` 后兜底回收。

## 代码变更

| 文件 | 变更 |
|------|------|
| [sag-dispatch/src/circuit_breaker.rs](../../../../sag-dispatch/src/circuit_breaker.rs) | `cooldown_until` 从 `CircuitState::Open` 提升至 `CircuitEntry` 独立字段（跨 Open↔HalfOpen 保留）；`record_probe_result(ok=false)` 改为从 `entry.cooldown_until` 读原值并 `.max(now + probe_interval_secs)`，不再覆盖硬冷却下界；新增 `probe_acquired_at` 字段与 `STALE_PROBE_THRESHOLD`（3600s）常量，`try_acquire_probe` 入口回收超过阈值的僵死探测槽；`prune_expired` 改为基于 `entry.cooldown_until` 统一判断 Open/HalfOpen |

## 测试

- `probe_failure_preserves_cooldown_until`：用 `open(key, 3600, 0)` 使首次探测立即 due、真正进入 HalfOpen，断言探测失败后 `cooldown_until` 仍 > 3000s（修复前会红，可防回归）。
- `stale_probe_slot_is_reclaimed`：回拨 `probe_acquired_at` 4000s（超过 3600s 阈值），断言 `try_acquire_probe` 回收并重新获取槽位。
- `stale_probe_within_threshold_not_reclaimed`：回拨 100s（阈值内），断言第二次 `try_acquire_probe` 失败（正常探测期间不被误回收）。

## 被取代的旧行为（历史认知，保留于此）

- 旧：`CircuitState::Open` 变体携带 `cooldown_until`，`Open→HalfOpen` 转换时结构性丢失 → 探测失败从 `HalfOpen` 回 `Open` 时无法读回原 `cooldown_until`，`record_probe_result(ok=false)` 用 `now + probe_interval_secs` 覆盖，硬冷却（如 1h）被压成 ~60s，`prune_expired` 提前删除 entry，冷却保护失效。
- 旧：`probe_in_flight` 仅由 `apply_circuit_outcome`/`release_probe_slot` 释放；探测 future 被 axum 取消（客户端断连）时两个 await 点均被 drop，`apply_circuit_outcome` 不执行，`HalfOpen + probe_in_flight=true` 永久残留（`to_half_open_if_due` 只在 Open+due 分支重置，`prune_expired` 修复前只删 Open）——目标被永久跳过。**修正 0002 changefile 的错误论断**：0002 末段曾称"原 reviewer 所述永久跳过在旧代码并不成立"，经逐行核验，该论断不成立——自愈路径不存在，"永久跳过"成立。
