---
created: 2026-08-04
updated: 2026-08-04
type: 修改
---

# 0001 — 4xx Failover 归因策略

## 动机

StickyFailover 下，pin（按 `request.model` 索引、全员共享的"当前主用 target"）在 failover 成功后写回备用 target。原实现对**所有非可重试 4xx**（400/401/403/404/413/422，除 408/429 外）走同一路径：不重试、不开熔断、failover、成功则挪 pin。由此产生爆炸半径：单个用户用某客户端发了一个 provider 不支持的参数（400/422），主用 target A 返 4xx → failover B → B 成功 → pin 改成 B → 所有用户后续都走 B，而 A 明明没坏。

另外，403 中的 quota/额度耗尽（provider 容量信号，会随计费周期自愈）被当成普通非可重试 4xx 处理（failover+挪 pin），未走熔断探针自愈路径，与 429-quota 的处理不对称。

## 需求变更

- dispatch/requirements.md「上游 4xx 错误分类」重写为三类归因：客户端参数错（400/413/422/未知 4xx）返客户端不 failover；provider 持续性（401/404/403-非quota）failover+挪 pin 不开闸；provider 瞬时（429/408/5xx/**403-quota**）可重试+熔断。其中 403-quota 为新增可重试类
- circuit-breaker/requirements.md「错误分类」追加：HTTP 403 + body 含 quota/billing 关键词 → 额度耗尽（3600s），与 429-quota 对齐
- failover-4xx-policy 模块新增（本 changefile 所在模块），承载该策略的完整设计

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-dispatch/src/dispatch/retry.rs`](../../../sag-dispatch/src/dispatch/retry.rs) | `RetryOutcome::NonRetryable` 由 unit 改为 `NonRetryable(AppError)` 携带错误；新增 `is_client_caused_4xx`（4xx 且非 401/403/404/408/429 → 客户端归因）；`is_retryable_error` 对 403 单独判定 `circuit_hint() == QuotaExhausted` |
| [`sag-core/src/error.rs`](../../../sag-core/src/error.rs) | `circuit_hint` 的 quota body 解析 arm 从 `status: 429` 扩为 `status: 429 \| 403`，复用同一套关键词；403-非quota 落 `Generic` |
| [`sag-dispatch/src/dispatch/chat.rs`](../../../sag-dispatch/src/dispatch/chat.rs) | dispatch 循环在 `apply_circuit_outcome` 消费 outcome 前按值剥离客户端错误（`AppError` 不实现 `Clone`，给 breaker 传占位 `NonRetryable`，其 payload 被 `_` 忽略）；else 分支客户端 4xx `return err.into_response()`，provider 持续性 4xx 仍 `continue` |
| [`sag-dispatch/src/dispatch/image.rs`](../../../sag-dispatch/src/dispatch/image.rs) | 与 chat 路径对称的剥离 + 早返/继续分支 |
| [`sag-dispatch/src/dispatch/mod.rs`](../../../sag-dispatch/src/dispatch/mod.rs) | `apply_circuit_outcome` 的 `NonRetryable` arm 适配为 `NonRetryable(_)`（逻辑不变：healthy 不开闸，is_probe 仍 `record_probe_result(false)` 释放探针槽） |

## 测试

- `client_error_4xx_returns_to_client_without_failover` — 400 返客户端 400、B 未调用、pin 未变、熔断未开
- `unsupported_param_422_does_not_move_pin` — StickyFailover pin=A、A 返 422、响应 422、pin 仍 A
- `auth_401_failovers_and_moves_pin` — 401 failover、pin→B、不开闸（回归现有行为）
- `model_404_failovers_and_moves_pin` — 404 failover、pin→B、不开闸（回归）
- `quota_403_opens_circuit` — 403 + "quota exceeded" → 熔断开闸 ~3600s、failover B
- `non_quota_403_failovers_and_moves_pin` — 403 + "permission denied" → failover、pin→B、不开闸
- `unknown_4xx_returns_to_client` — 418 返客户端、B 未调用、pin 未变
- `is_client_caused_4xx_attributes_status_codes` / `non_retryable_outcome_carries_the_error`（retry.rs）+ 5 个 `circuit_hint` 403 单测（error.rs）

## 影响的需求条目

- 「调度 — 预期行为 · 上游 4xx 错误分类」「调度 — 预期行为 · 分发流程 step 4e」
- 「熔断器 — 预期行为 · 错误分类」

## 残余风险（已知，非本次范围）

- `log_per_target_failure` 的 analytics `status_code` 硬编码 502（`mod.rs`），客户端 4xx 返客户端前仍记 502 一条——预存模式，列表为后续
- 403 body 同时含 "rate limit" + quota 关键词时，`circuit_hint` 先匹配 "rate limit" 返回 `RateLimited`，403 该形被 `is_retryable_error` 判为非 quota（非可重试）；与 429 同序的预存边缘情况
