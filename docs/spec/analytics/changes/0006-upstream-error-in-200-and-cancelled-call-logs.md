---
created: 2026-09-15
updated: 2026-09-15
type: 修复
---

# 0006 — 200 状态内上游错误识别 + 取消流写 call_logs

## 动机

上游（Volcano Ark 实测）有时以 HTTP 200 返回错误：要么是流式 SSE data frame 的 JSON 带顶层 `error` 对象，要么是 200 JSON body 内嵌 error 对象。此前 SAG 只检查 HTTP 状态码（`base.rs::check_upstream_error` / `ensure_success`），SSE 帧处理只提取 usage、从不检查 `error` 字段 —— 这类错误被原样透传给客户端、网关零记录；若客户端随后中断流，该请求还会被记为 `client_cancelled`，掩盖真实失败。

另外，`TerminalGuard` 的客户端断开路径（`stream.rs`）只通过 EventCollector 发分析事件，**不写 `call_logs`**；而管理面板日志列表（`GET /api/logs`）只读 `call_logs`，因此这类被取消的流式请求在 UI 中完全不可见（真实案例：2026-09-14 07:59:58 UTC deepseek-v4-flash 流式请求，客户端看到 Ark burst 错误、网关记 `client_cancelled`、UI 无记录）。

本次修复：① 取消/失败后断开的流式请求也写 `call_logs` 行；② 识别 200 状态内交付的上游错误，流式按流式中段失败路径记录为 `model_failure`，非流式记为失败尝试并可 failover，rate-limit 特征映射 `rate_limited`。

## 需求变更

- requirements.md「最终状态与失败率」：追加取消的流式请求必须写 `call_logs` 行（499 / `client_cancelled`），流式出错后断开写 502 + 上游错误消息，保证日志列表可见。
- requirements.md「重试与故障转移」：追加 200 状态内交付的上游错误识别规则（SSE data frame 顶层 `error` 对象 / 200 body 顶层 `error` 对象）：流式 → 流式已开始输出后的失败路径（`model_failure` / `stream_error_after_commit`）；非流式 → 失败尝试 + failover，最终耗尽保持标准错误 envelope 并保留原始消息；限流特征 → `failure_reason=rate_limited`，否则 `provider_error`。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-providers/src/base.rs`](sag-providers/src/base.rs) | 新增集中辅助函数 `body_has_error_object`（顶层 `error` **对象**判定——仅真实 JSON 对象计为错误，`"error": null` / 缺失 / 标量值（string/bool/array/number）均视为无）、`body_error_message`（提取 message）、`body_error_is_rate_limited`（保守限流判定：数字 code 429 或 code/message 含 rate limit/burst/flow/quota） |
| [`sag-providers/src/util.rs`](sag-providers/src/util.rs) | `process_sse_frame` / `drain_sse_frames` / `flush_sse_buffer` 返回 `bool`；data frame 顶层 `error` 对象 → 仍转发该帧 + 发 `StreamChunk::Error` + 停止继续处理；**错误 chunk 先于被转发帧入队**（`Error` → `Data` 顺序，见下「chunk 排序竞态」） |
| [`sag-providers/src/openai.rs`](sag-providers/src/openai.rs) | `chat()` 对 200 body 顶层 error 对象返回 `AppError::Upstream`（限流 → 429，否则 502）；`chat_stream()` 消费 drain/flush 返回值，错误帧后清空缓冲、不发 Done |
| [`sag-providers/src/anthropic.rs`](sag-providers/src/anthropic.rs) | `chat()` 对 200 body 顶层 error 对象返回 `AppError::Upstream`（限流 → 429，否则 502）；`process_anthropic_sse_frame` / `drain_anthropic_sse_frames` / `flush_anthropic_sse_buffer` 返回 `bool`，`chat_stream()` 消费返回值——Anthropic 流内错误事件（`type: "error"` 或顶层 error 对象）→ 仍转发该帧 + 发 `StreamChunk::Error`（消息取自内层 error 对象）+ 停止处理、清空缓冲、不发 Done，与 OpenAI 流式路径一致 |
| [`sag-dispatch/src/dispatch/stream.rs`](sag-dispatch/src/dispatch/stream.rs) | `StreamAnalytics` 增加 `call_log_collector` / `start` / `error_message` / `reasoning_effort` / `user_agent` / `client_ip`；`TerminalGuard::drop` 在发分析事件之外同时写 `call_logs` 行（取消 → 499 + `client_cancelled`；出错后断开 → 502 + 上游消息 + `stream_error`）；Error chunk 记录上游消息；正常完成臂把 `done` 标记提前到写 `call_logs` 之前（防日志构造 panic 产生重复行）；**round-3**：`had_error` 的正常 EOF/Done 臂与 drop 臂写成相同失败形态（usage / cache / reasoning / `ttft_ms` / `upstream_duration_ms` 均 NULL） |
| [`sag-dispatch/src/dispatch/mod.rs`](sag-dispatch/src/dispatch/mod.rs) | `DispatchAnalytics` / `DispatchCancelGuard` 增加 `call_log_collector` / `start` / `reasoning_effort` / `user_agent` / `client_ip`；`DispatchCancelGuard::drop`（请求在提交前被中止——resolve/backoff/handshake 期间）在发 `client_cancelled` 分析事件之外同时写 `call_logs` 行（499 / `client_cancelled`，usage NULL；在途 target 提供 provider/real_model，否则用 `__cancelled__` 哨兵）；**round-3**：新增 `record_target_fact` + `completed` 镜像，drop 时拼接已完成 target 事实与在途 `cancelled` 事实 |
| [`sag-dispatch/src/dispatch/chat/tests.rs`](sag-dispatch/src/dispatch/chat/tests.rs) | 新增非流式 200 错误 failover、rate_limited 映射、全耗尽保消息测试 |

## 测试

- `sag-providers::base::tests`：`body_has_error_object` 真实对象/`null`/缺失/标量（string/bool/array/number）判定——非对象一律视为无；`body_error_message` 提取；`body_error_is_rate_limited` 命中与保守不命中。
- `sag-providers::util::tests`：SSE frame 顶层 error 对象 → `Error` 先于被转发 `Data` 入队且返回 true；`"error": null` → 正常；正常 content/usage 帧不受影响；drain/flush 传播错误帧。
- `sag-providers::openai::tests` / `anthropic::tests`：200 body 带 error 对象 → `AppError::Upstream`（502）；rate-limit 特征 → 429；`chat_stream` 端到端错误帧后不再转发后续帧、不发 Done。
- `sag-providers::anthropic::tests`（流式新增）：Anthropic 错误事件（`type: "error"`）→ `Error` chunk + 仍转发该帧 + 停止处理、无 Done；`"error": null` / 非对象 → 正常事件处理不受影响；正常 message_start / content_block_delta / message_delta（usage）帧不受影响；drain/flush 传播错误事件；`chat_stream` 端到端错误事件后不发 Done。
- `sag-dispatch::dispatch::chat::tests`：200 错误 → 尝试失败 + failover 到下一 target；rate-limit → `rate_limited`；全耗尽 → BAD_GATEWAY 且保留上游消息；**round-3**：后续 target 在途取消保留前序失败事实（2 条 attempt，顺序 failure→cancelled）；正常成功 fallback 不产生重复 attempt 行。
- `sag-dispatch::dispatch::mod::tests`（新增）：提交前取消（中止于 resolve/backoff/handshake）→ `request_results` 恰一条 `client_cancelled` + `call_logs` 恰一条（499 / `client_cancelled`，usage NULL）；无在途 attempt 时仍写一条 `call_logs`（provider/real_model 用 `__cancelled__` 哨兵）；`mark_done` 后 drop 严格无副作用（不写行、不发事件）；**round-3**：已完成事实 + 在途 cancelled 事实合并为单事件（顺序保证）；backoff 间隙取消保留已完成事实但不捏造 attempt。
- `sag-dispatch::dispatch::image::tests`（round-3 新增）：后续 target 在途取消保留前序失败事实（顺序 failure→cancelled）。
- `sag-dispatch::dispatch::stream::tests`（round-3 新增）：`had_error` 后正常 EOF/Done → `call_logs` 恰一条（502 / `stream_error`，usage / cache / reasoning / `ttft_ms` / `upstream_duration_ms` 均 NULL）+ 事实保持 `model_failure` / NULL；取消终止即使已转发 Data 也保持 timing NULL。
- `sag-dispatch::dispatch::stream::tests`：模拟中断 → `request_results` 一条 `client_cancelled` + `call_logs` 恰一条（499 / `client_cancelled`）；流式中段错误后断开 → `model_failure`（非 `client_cancelled`）+ `call_logs` 一条（502 + 上游消息）；`drop_guard_falls_back_to_try_emit_outside_runtime` 增加 `call_logs` 断言；**chunk 排序竞态回归**：Error→Data 顺序下 Data 帧仅被拉取一次即断开（不再 poll）→ `model_failure` / 502 + 上游消息，绝不落到 `client_cancelled` / 499。

## 附：round-2 复核修复

- **`body_has_error_object` 误报**：原实现 `!e.is_null()` 对 `""` / `false` / `[]` / `0` 等标量 `error` 值也返回 true，可能把正常 200 负载误判为错误。改为仅 `e.is_object()` 计为错误，标量 / null / 缺失一律视为无，并同步更新 util / openai / anthropic 调用处与测试。
- **chunk 排序竞态**：`process_sse_frame`（及 Anthropic 路径）原先先入队被转发 `Data` 再入队 `Error`。dispatch unfold 循环对 `Error` 是内联消费（记录 `analytics.error` 后继续循环、不 yield），因此若客户端收到 `Data` 帧后立即断开（生产事故），流被丢弃、`Error` 永不被 poll → 误判为 `client_cancelled` / 499。修复为 `Error` 先入队、`Data` 后入队：unfold 循环在 yield `Data` 前已记录错误状态，断开时按 `model_failure` / 502 + 上游消息分类。客户端可见行为不变（错误帧仍转发给客户端）。
- **提交前取消可见性**：`DispatchCancelGuard::drop` 原先只发分析事件、不写 `call_logs`，管理面板日志列表（`GET /api/logs` 只读 `call_logs`）里这类请求完全不可见。现补写一条 499 / `client_cancelled` 行（usage NULL）。单请求单行不变量：提交前取消永远到不了 stream `TerminalGuard`（所有正常终止路径——含 stream 移交——都在 `build_streaming_response` 提交前 `mark_done`），故不会与其重叠。

## 附：round-3 复核修复

### Major-2 — 中段错误后的正常终止须遵守失败契约

`stream.rs` 的 `Some(Done) | None` 终止分支此前对 `had_error == true` 仍写入成功形态的 token/timing 样本：`usage` / `cached_tokens` / `cache_creation_tokens` / `reasoning_tokens` / `ttft_ms` / `upstream_duration_ms` 全部带值。这违反已有的失败契约（`failure_log` / `log_per_target_failure` 与 `TerminalGuard::drop` 均写 NULL），会让失败请求污染缓存统计与延迟覆盖率分母。

- 修复：`had_error == true` 时以上六个字段一律写 `None`，`status_code` 保持 502、`error` / `error_category=\"stream_error\"` 不变；成功路径不变。
- 同步 `usage: if had_error { None } else { Some(usage.clone()) }`，与 drop guard（客户端断开）终止完全对齐——两条终止路径现在写相同的失败形态行。

### Major-3 — 多 target fallback 取消丢失已完成 target 事实

`DispatchCancelGuard` 此前只记录在途 attempt：多 target fallback 中，若前序 target 已完成（形成 `TargetAttemptFact`）而后续 target 在途时客户端断开，已完成的事实随 `chat`/`image` 循环局部 `target_facts` 一起被丢弃，最终 `client_cancelled` 事件只带当前 cancelled attempt。

- `DispatchAnalytics` 新增 `completed: std::sync::Mutex<Vec<TargetAttemptFact>>`；`DispatchCancelGuard::record_target_fact`（由 `chat`/`image` 调度循环在每次 push 循环局部 vec 之前镜像调用）追加出去。
- `Drop` 侧改为「已完成事实 + 在途 `cancelled` 事实」拼接（按 attempt 顺序）——单 target retry/backoff 间隙取消仍为 0 条新事实（spec scenario 11 不变），但已完成事实不再丢失。
- 生命周期安全：正常终止路径先消费循环局部 vec 再 `mark_done()`，guard 的镜像副本随即被丢弃、永不被读取，因此不会双写；`target_attempts` 的 `ON CONFLICT(request_id, target_id) DO NOTHING` 作为第二道防线。
- 覆盖 chat 与 image 两条路径。
