---
created: 2026-10-25
updated: 2026-10-25
type: 修复
---

# 0005 — 会话墓碑集合上限与全局 epoch 递增（内存有界）

## 动机

0003 的线性化协议靠 per-key generation 墓碑拒绝陈旧写回：移除/驱逐一个键时 bump 它的
generation，决策点捕获的 generation 与写回时读到的 generation 不一致即拒绝。墓碑**只增
不减**，而键域由客户端控制（会话 id），且每次 LRU 驱逐都会为一个键制造一条墓碑 —— 一个
`session_max_entries = 10000` 的空间在持续 churn 下，pin map 被容量钉住，墓碑集合却随
会话 id 数量线性增长，长跑实例可被会话 id 洪水推爆内存。

不能只清墓碑：墓碑清零等于把键的 generation 退回「从未 bump」状态，决策早于清空的在途
写回会被 generation 检查放行（ABA），重新 pin 回已被移除的条目。

## 需求变更

- 「预期行为 → 会话粘性路由」第 4 条补一句内存有界约束：驱逐/清除留下的 generation
  墓碑有上限（每 space `max(session_max_entries, 1024)`）；溢出时整批清空并**在同一
  临界区**递增全局 epoch，代价是可能拒绝一次在途写回（宁可丢一次写回，不泄漏内存）。
- 阈值下限 `MIN_SESSION_TOMBSTONES = 1024` 是行为性的：容量很小时若不留底，几乎每次
  驱逐都会触发清空 + epoch 递增，把无关会话的在途写回一起拒掉。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-state/src/session_affinity.rs`](../../../../sag-state/src/session_affinity.rs) | 新增 `MIN_SESSION_TOMBSTONES`(1024)、`RouteSessionSpace::tombstone_budget()`（`capacity.max(MIN_SESSION_TOMBSTONES)`）、`bump_generation()` 与 `enforce_tombstone_budget()`（超限清空墓碑 + `epoch += 1`，须在调用方临界区内） |
| [`sag-state/src/session_affinity.rs`](../../../../sag-state/src/session_affinity.rs) | 所有移除路径改走 `bump_generation`：LRU 驱逐（`insert`）、`clear_entries`、`clear`、`clear_model`、`clear_all`、`retain_sticky_routes`；这些方法改用局部 `epoch` 累加后统一写回 `self.epoch`，避免同一临界区内多次清空只记一次递增 |

## 测试

- `tombstone_overflow_clears_the_set_and_bumps_epoch` — 容量 1 的空间 churn 到溢出后
  墓碑数不超预算，且全局 epoch 严格递增
- `tombstone_clear_rejects_writeback_its_generation_would_allow` — 溢出清空后 per-key
  generation 回到 0（generation 检查单独失效），由 epoch 检查拒绝清空前决策的写回，
  且被拒写回不会重新 pin
- `tombstones_below_budget_are_kept_and_still_reject` — 未溢出时墓碑保留、epoch 不动，
  驱逐前的写回仍被 generation 拒绝（正常路径行为不变）
