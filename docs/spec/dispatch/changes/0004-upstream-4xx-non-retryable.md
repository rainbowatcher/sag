---
created: 2026-07-22
updated: 2026-07-22
type: 修复
---

# 0004 — 上游 4xx 不再触发重试与熔断

## 动机

上游返回 4xx（如 opencode zen 用 pydantic `extra=forbid` 拒收客户端透传的 `chat_template_kwargs` 字段，返回 400）时，SAG 将其映射为 `AppError::Upstream { status, .. }`，但 `is_retryable_error` 只排除 `BadRequest | ModelNotFound | Unauthorized`，未排除 `Upstream`，导致 4xx 被判为可重试：同一请求被无谓重试 3 次（请求体不变结果不变），且 3 次耗尽后走 `RetryOutcome::Exhausted` 分支误开熔断器 60s。这绕过了 `mod.rs:165-176` 明确的设计意图——"non-retryable 4xx on healthy target does not open the circuit"。

## 需求变更

- requirements.md「重试退避」节追加：上游 4xx（除 408 超时、429 限流外）改为不可重试，立即 failover 到下一 target，不重试、不开熔断
- requirements.md「分发流程」step 4e 修正：失败路径按错误类型细化——non-retryable 4xx 直接 failover，retryable 错误走重试

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-dispatch/src/dispatch/retry.rs`](../../../sag-dispatch/src/dispatch/retry.rs) | `is_retryable_error` 对 `AppError::Upstream` 按 status 判定：429/408/5xx 可重试，其余 4xx 不可重试；更新 doc comment |
| [`sag-dispatch/src/dispatch/chat.rs`](../../../sag-dispatch/src/dispatch/chat.rs) | 修正 `dispatch_chat` 注释（non-retryable 4xx 是 failover 到下一 target 而非 bail out 整个请求）；新增端到端回归测试 |

## 测试

- `is_retryable_error_excludes_4xx` 扩展：覆盖 `Upstream{400/403/422}` 不可重试、`Upstream{429/408/500/503}` 可重试
- `dispatch_chat_upstream_4xx_does_not_retry_or_open_circuit`：`Upstream{400}` 只调用一次（不重试）、熔断器保持 closed、failover 到下一 target 成功

## 影响的需求条目

- 「预期行为 — 分发流程」step 4e（失败路径细化）
- 「预期行为 — 重试退避」（新增 4xx 分类规则）