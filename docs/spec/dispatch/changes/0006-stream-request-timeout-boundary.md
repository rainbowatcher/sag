---
created: 2026-08-26
updated: 2026-08-26
type: 修复
---

# 0006 — 流式请求超时边界修复

## 动机

共享 reqwest client 原先通过 `ClientBuilder::timeout(timeout_secs)` 对所有请求设置总请求时限。该时限也覆盖成功流式响应的 response body，导致健康但耗时较长的 Qwen/OMLX 流在 600 秒后被 SAG 截断，客户端收到 `Stream ended without finish_reason`。

本次修复将超时责任按请求阶段拆开：非流式请求保留完整 request-level timeout；流式请求只对响应头使用 TTFB timeout，成功 response body 不再有总时长 deadline，并继续使用可选的 chunk idle timeout。流式非 2xx 错误 body 另使用 TTFB 预算进行有界诊断读取，避免异常上游导致无限等待。

## 需求变更

- 修改调度超时分类：非流式总超时来自 request-level `RequestBuilder::timeout()`；流式初始超时来自 `tokio::time::timeout` 包装的 TTFB 阶段。
- 新增行为约束：成功流式响应不受 provider `timeout_secs` 的总时长限制，body 仅受 chunk idle timeout 约束。
- 明确流式非 2xx 错误 body 必须有界读取，同时保留上游 HTTP 状态和现有重试分类。
- 记录当前 chunk idle timeout 的实际外部协议仍为通用 `STREAM_ERROR` / CallLog 502；专用 `STREAM_CHUNK_TIMEOUT` / 504 taxonomy 留待后续独立变更。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-providers/src/base.rs`](../../../../sag-providers/src/base.rs) | 移除共享 reqwest client 的总 timeout；保留非流式 request-level timeout；为流式非 2xx error body 增加局部读取超时；补充 HTTP timeout 回归测试。 |
| [`sag-providers/src/openai.rs`](../../../../sag-providers/src/openai.rs) | 增加持续流和 chunk idle timeout 的本地 HTTP mock 测试。 |
| [`sag-providers/src/anthropic.rs`](../../../../sag-providers/src/anthropic.rs) | 增加持续流和 chunk idle timeout 的本地 HTTP mock 测试。 |
| [`sag-providers/src/test_support.rs`](../../../../sag-providers/src/test_support.rs) | 提供 provider 测试复用的 loopback TCP server、配置和请求 fixture。 |
| [`sag-providers/src/traits.rs`](../../../../sag-providers/src/traits.rs) | 明确 `chat_stream` 的 `timeout_secs` 不作为成功流总时长限制。 |
| [`sag-config/src/config.rs`](../../../../sag-config/src/config.rs) | 更新 provider timeout 配置注释，区分非流式总 deadline 与流式阶段 timeout。 |
| [`docs/spec/dispatch/requirements.md`](../requirements.md) | 同步 timeout 触发机制、状态码、错误码和成功流无总 deadline 的需求描述。 |
| [`docs/spec/dispatch/spec.md`](../spec.md) | 同步 chunk idle、TTFB 和成功流总时长的实现决策与不变式。 |

## 测试

- 验证成功流 response body 总时长超过 `timeout_secs` 时仍完整消费。
- 验证非流式 `send_json(..., Some(timeout_secs))` 在 response body 延迟时仍由 reqwest timeout 终止。
- 验证流式 TTFB timeout 仍返回 `AppError::TtfbTimeout`。
- 验证流式非 2xx error body 在 TTFB 预算内返回 `AppError::Upstream`，不会无限等待。
- OpenAI/Anthropic 持续 chunk 测试验证完整 payload、上游 `[DONE]`/`message_stop` 和内部 `Done`；idle timeout 测试验证产生 `StreamChunk::Error` 且不产生 `Done`。
- `just test --tests`、provider 测试、格式化和文档构建通过；全仓 `just lint` 仍受既有 `sag-dispatch/src/dispatch/resolve.rs:36` `result_large_err` 阻断。
