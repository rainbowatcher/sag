---
created: 2026-07-16
updated: 2026-07-16
type: 修改
---

# 0002 — 熔断半开探测与冷却期周期探测

## 动机

原熔断器是纯时间驱动的 `Closed → Open(cooldown) → Closed` 两态机：target 重试耗尽后被固定冷却，冷却期内所有请求直接跳过该 target，冷却结束才自动恢复。

这带来两个现实问题：

1. **无法提前感知恢复**：上游（如供应商额度）在冷却窗口中途就恢复了，但 target 仍被完整跳过整个冷却窗口（默认额度耗尽 1h），要傻等满冷却才第一次尝试。
2. **对 fill-first / sticky-failover 尤其伤**：首选 target 被熔断期间，流量全压到次选，且首选不会提前回来。

本变更引入三态机（`Closed` / `Open` / `HalfOpen`）+ 冷却期内**周期性探测**，使 target 在冷却窗口内就能被发现已恢复。

## 需求变更

- `requirements.md` → 「预期行为 / 冷却行为」：新增半开探测与周期探测小节，说明探测触发时机、并发控制、失败重排。
- `requirements.md` → 「功能边界 / 不包含」：删除「半开状态（half-open）探测」条目（该能力现已实现）。
- `requirements.md` → 「配置 Schema」：新增 `probe_interval_secs`（默认 60）。
- `requirements.md` → 「功能边界 / 包含」：新增「半开状态探测」「冷却期周期探测」。
- 旧描述（纯时间驱动、不包含半开）保留于此 changefile 的「被取代的旧行为」节，不删除于 requirements.md 历史认知。

## 代码变更

| 文件 | 变更 |
|------|------|
| [sag-dispatch/src/circuit_breaker.rs](../../../../sag-dispatch/src/circuit_breaker.rs) | 三态机 `Closed`/`Open{cooldown_until,next_probe_at}`/`HalfOpen`；`open` 增加 `probe_interval_secs`；新增 `can_probe_now`/`next_probe_in`/`to_half_open_if_due`/`jittered_interval`；`record_probe_result` 失败改为仅推 `next_probe_at`（周期重排） |
| [sag-dispatch/src/dispatch/mod.rs](../../../../sag-dispatch/src/dispatch/mod.rs) | `circuit_allows_target` 返回 `CircuitVerdict{key,is_probe}`，用 `can_probe_now` 决策并合并读锁；`apply_circuit_outcome` 区分 `is_probe` 结果（成功关闭 / 失败按 interval 重排 / 4xx 非对称处理）；新增 `release_probe_slot` |
| [sag-dispatch/src/dispatch/chat.rs](../../../../sag-dispatch/src/dispatch/chat.rs) | 透传 `is_probe`；`resolve_provider` 失败且为探测时调用 `release_probe_slot` 释放探测槽；新增 `build_state_with_cb` 与半开/周期探测测试 |
| [sag-dispatch/src/dispatch/image.rs](../../../../sag-dispatch/src/dispatch/image.rs) | 同 chat.rs 透传 `is_probe` 与 `release_probe_slot`；新增 `build_state_with_cb` 与 image 半开测试 |
| [sag-config/src/config.rs](../../../../sag-config/src/config.rs) | `CircuitBreakerConfig` 新增 `probe_interval_secs`（默认 60，校验非 0）；新增对应 validation 测试 |

## 测试

- `circuit_breaker.rs`：`open_marks_target_unavailable_until_first_probe_due`、`probe_due_transitions_to_half_open`、`probe_failure_reschedules_interval_not_full_cooldown`（验证失败推 interval 而非重开整段冷却）、`probe_success_closes`、`half_open_allows_only_one_probe`（并发仅 1 次探测）。
- `chat.rs`：`dispatch_chat_probes_during_cooldown_not_after`（**核心**：长 `generic_secs=3600` 但 `probe_interval_secs=0`，冷却期内探测即发现已恢复）、`dispatch_chat_half_open_probe_recovers_target`、`dispatch_chat_half_open_probe_failure_reschedules`、`dispatch_chat_half_open_probe_in_flight_skips_others`（并发仅放 1 次探测）、`dispatch_chat_releases_probe_slot_when_provider_unresolved`（探测槽泄漏回归测试）。
- `image.rs`：`dispatch_image_half_open_probe_recovers_target`、`dispatch_image_releases_probe_slot_when_provider_unresolved`。
- `config.rs`：`rejects_zero_circuit_breaker_probe_interval`。

## 被取代的旧行为（历史认知，保留于此）

- 旧：`Open` 冷却期结束后才转 `HalfOpen` 放行单次探测 → 等价于"最少等待 = 冷却时长"，冷却期内无法感知恢复。
- 旧：`apply_circuit_outcome` 探测失败直接 `open(key, duration)` 重开整段冷却 → 提前感知失效。
- 旧：功能边界明确「不包含 半开状态（half-open）探测」「仅时间驱动」。
- 旧：`resolve_provider` 返回 `None` 时直接 `continue`，探测槽（若存在）不释放。

## 已知属性（设计权衡，非缺陷）

- **无探测次数上限**：真耗尽的目标会按 `probe_interval_secs` 无限期低频探测直到恢复（带 jitter，开销可忽略）。调大 `probe_interval_secs` 或移除 target 即停。
- **4xx 行为非对称**：健康 target 收到 4xx 零惩罚；探测期收到 4xx 按失败探测重排（短暂跳过），以避免 half-open 槽卡死。已在 `apply_circuit_outcome` 文档中显式说明。
- **探测槽泄漏已修复**：`release_probe_slot` 在 `resolve_provider` 失败路径回填失败结果，避免 `probe_in_flight` 残留导致冷却窗口内无法再探测。注：原 reviewer 所述"永久跳过"在旧代码的成立性曾被误判为不成立（理由是 `can_probe_now` 忽略 `probe_in_flight` 且 `prune_expired` 到期清理）；经后续逐行核验，该自愈路径不存在——`to_half_open_if_due` 只在 `Open`+due 分支重置 `probe_in_flight`，`HalfOpen` 残留不触发；`prune_expired` 只删 `Open` entry。流式 future 被取消时 `HalfOpen + probe_in_flight=true` 永久残留，"永久跳过"成立。此问题由 0003 修复（`try_acquire_probe` 僵死槽兜底回收）
