---
created: 2026-10-11
updated: 2026-10-11
type: 修复
---

# 0003 — 大请求响应日志与请求关联信息

## 动机

排查 2026-10-09 / 10-10 的线上日志时发现三个可观测性缺口，都属于「日志缺失导致无法定位」，而非功能缺陷：

1. `log_http` 对 body > 512 KiB 的请求提前返回，把响应日志也一并跳过 —— 10-10 当天 1575 条请求里 917 条（58%）没有任何 status / 耗时记录，大 prompt 请求的结局完全不可见（当天 3 条 volc 流中断无法关联到具体请求）。
2. provider 的流读取任务（`chat_stream` 内的 `tokio::spawn`）脱离了调用方 span，`error reading upstream stream chunk` / `stream chunk idle timeout` 这类 ERROR 不带 provider / model / trace_id，无法与请求关联。
3. 流式响应 commit 之后的客户端断开（`client_cancelled`）只写 analytics 事件与 `call_logs` 行，日志里没有对应行；只有 pre-commit 取消（`DispatchCancelGuard`）会打日志。

## 需求变更

- requirements.md「HTTP 请求日志」：新增「请求体大于 512 KiB 时不缓冲、不产生预览，但仍必须记录响应 status 与耗时（`elapsed_ms`），content-length 可用时附带 `body_size`」
- requirements.md「HTTP 请求日志」：新增「流式响应 commit 之后的客户端断开必须记录一条 WARN（request_id / provider / model / real_model / duration_ms）」
- requirements.md「Trace ID」：新增「脱离请求生命周期的后台任务（provider 流读取任务）必须继承调用方 span，其日志携带 provider / model / trace_id」

## 代码变更

| 文件 | 变更 |
|------|------|
| [sag-router/src/middleware/http_log.rs](../../../../sag-router/src/middleware/http_log.rs) | 大 body 早退路径新增 `log_response_meta()`：记录 status / `elapsed_ms` / content-length，不消费 body、不产生 preview |
| [sag-providers/src/util.rs](../../../../sag-providers/src/util.rs) | 新增 `spawn_in_current_span()`：用 `tracing::Instrument` 把调用方 span 附着到脱离请求生命周期的任务上 |
| [sag-providers/src/openai.rs](../../../../sag-providers/src/openai.rs) | `chat_stream` / `responses_stream_native_inner` 的 reader task 改用 `spawn_in_current_span` |
| [sag-providers/src/anthropic.rs](../../../../sag-providers/src/anthropic.rs) | `messages_stream_native` / `chat_stream` 的 reader task 改用 `spawn_in_current_span` |
| [sag-dispatch/src/dispatch/stream.rs](../../../../sag-dispatch/src/dispatch/stream.rs) | `record_stream_error` 补 request_id / provider / model / real_model 字段；`TerminalGuard::drop` 的 post-commit 取消分支新增 WARN |
| [sag-providers/Cargo.toml](../../../../sag-providers/Cargo.toml) | 新增 `tracing-subscriber` dev-dependency（日志断言测试用） |
| [sag-router/Cargo.toml](../../../../sag-router/Cargo.toml) | 新增 `tracing-subscriber` dev-dependency（日志断言测试用） |

## 测试

- `sag-router/tests/http_log_large_body.rs`（新增）：600 KiB body 的普通响应必须出现 `response status=200 elapsed_ms=…` 行且不含 `preview=`；SSE 响应必须出现 `streaming response status=200`。已做反向验证：注释掉 `log_response_meta()` 调用后两条用例均失败。
- `sag-providers/src/util.rs::tests::spawn_in_current_span_keeps_span_context`（新增）：在 span 作用域内 spawn、在作用域外 `await`，断言事件仍携带 `provider` / `model` 字段（证明上下文来自 spawn 时刻的 instrumentation，而非调用点作用域）。
- `just test --tests`：22 个 suite、1265 条测试全绿；`cargo clippy --workspace --all-targets` 无新增告警。

## 影响的需求条目

- 「HTTP 请求日志」（新增两段约束：大 body 的响应日志、post-commit 断连日志）
- 「Trace ID」（新增后台任务继承 span 的约束）
