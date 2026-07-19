---
created: 2026-07-19
updated: 2026-07-19
type: 新增
---

# 0003 — 失败路径补全 `reasoning_effort` 记录

## 动机

`CallLog.reasoning_effort: Option<String>` 记录客户端请求里的原始 `reasoning_effort` 值，设计意图是「请求意图」与「实际推理量」（`reasoning_tokens`）分离记录。

成功路径已正确记录（`success_log` 带 `reasoning_effort` 参数，流式经 `StreamState` 透传）。但所有「建流前」的失败经 `failure_log` 硬编码 `reasoning_effort: None`，导致带 `reasoning_effort=high` 的调用失败时在 `call_logs` 表里查不到意图，破坏「请求意图 × 成败」关联分析。

## 需求变更

- **新增** `failure_log` 增加 `reasoning_effort: Option<&str>` 参数
- **新增** `log_per_target_failure`、`failure_response`、`fallback_exhausted_response` 三个 wrapper 透传该参数
- **新增** 6 个调用点补传 `request.reasoning_effort.as_deref()`（聊天 4 个）或 `None`（图片 2 个）

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-dispatch/src/dispatch/mod.rs`](../../../sag-dispatch/src/dispatch/mod.rs) | `failure_log` 签名加 `reasoning_effort` 参数，`:669` 硬编码 `None` 改为 `reasoning_effort.map(\|s\| s.to_string())`；三个 wrapper 签名加参数并透传；新增 per-target 失败 + all-exhausted round-trip 测试 |
| [`sag-dispatch/src/dispatch/chat.rs`](../../../sag-dispatch/src/dispatch/chat.rs) | `log_per_target_failure` 调用点补传 `request.reasoning_effort.as_deref()`；`fallback_exhausted_response` 调用点同 |
| [`sag-dispatch/src/dispatch/image.rs`](../../../sag-dispatch/src/dispatch/image.rs) | 两个调用点补传 `None`（图片路径不适用）；新增 `image_failure_always_writes_reasoning_effort_none` 测试 |
| [`sag-dispatch/src/dispatch/resolve.rs`](../../../sag-dispatch/src/dispatch/resolve.rs) | `failure_response` 调用点（model-not-found + all-disabled）补传 `request.reasoning_effort.as_deref()`；新增 `resolve_route_failure_preserves_reasoning_effort` 测试 |

## 设计决策

### 参数位置

`reasoning_effort` 放在 `err`/`errors` 与 `user_agent` 之间，将「请求侧属性」归组。4 个函数与 6 个调用点保持一致。

### 图片路径始终 `None`

图片生成不适用 `reasoning_effort`（`image.rs` 已有显式置 `None` 模式），调用点传 `None` 保持语义一致。

## 测试

- `log_per_target_failure_preserves_reasoning_effort`：per-target 失败 round-trip，断言 `reasoning_effort == Some("high")`
- `fallback_exhausted_preserves_reasoning_effort`：全部耗尽 round-trip，断言入库为 `Some("high")`
- `resolve_route_failure_preserves_reasoning_effort`：resolve 失败 round-trip，断言入库为 `Some("low")`
- `image_failure_always_writes_reasoning_effort_none`：图片路径始终 `None`

## 影响的需求条目

- 「预期行为 — 分发流程」（失败路径新增 `reasoning_effort` 传递）
