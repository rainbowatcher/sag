---
created: 2026-10-22
updated: 2026-10-22
type: 修复
---

# 0007 — 流式终点帧竞态误分类修复

## 动机

流式请求的正常收尾存在一条竞态：客户端收到终点帧（Chat 的 `data: [DONE]`、Anthropic 的 `message_stop`、Responses 的 `response.completed` / `response.incomplete` / `response.cancelled`）后按协议关连接是**合法行为**，但此时上游的 `End` 事件常因上游 FIN 尾延迟尚未入队，hyper 于是 drop body，触发 `TerminalGuard::Drop`。旧实现只按「是否观察到 mid-stream `Error`」二分分类，没有 error 就一律判 `client_cancelled` / 499 —— 于是「客户端实际已完整收全、SAG 却记 499」成为高频误分类（历史上每天几十到几百条）。

同一条 drop 路径还有数据完整性缺陷：499 行的 `call_logs` / analytics 全列 NULL（usage / `ttft_ms` / `upstream_duration_ms` 均缺失），因为这些状态原先只活在 body future 的局部 `state` 里，`TerminalGuard` 拿不到。

此外发现 Anthropic ingress + 非 Anthropic upstream（translated 桥接）路径**永远不向客户端发送 `message_stop`**：上游不发该帧，而 ingress 编码器的 `End => message_stop` 分支因 `End` 被内部消费而不执行，属协议缺口。

### 评审返工（同日 Minor 收敛）

首轮实现通过评审后，评审又提出 4 条 Minor，均已修复并并入本变更（同一未发布变更的收尾，不另开编号）：

1. **判定补 `response.cancelled`**：Responses 的流终点事件有三个（`completed` / `incomplete` / `cancelled`），首轮只覆盖前两个，取消流会被漏判为「未终止」。现 `is_client_terminal_frame` 覆盖全部三个，`response.failed` 仍排除（走 502）。行锚定前缀 8→10、引号形 4→6，并补内容伪造负例。`empty.rs` 的 `responses_raw_verdict` 同步对齐（镜像 `response.incomplete`：无 output → `Empty`，有 output → `Content`），保证「终点帧知识」两侧一致——取消且无输出的流仍走 `retry_on_empty`。
2. **Chat ingress + Responses upstream 补 `data: [DONE]` 终止帧**（既有协议缺口，与 Anthropic upstream 同类）：桥接 #1 的启用条件由 `is_chat_ingress && use_anthropic` 放宽为 `is_chat_ingress && (use_anthropic || use_responses)`。响应帧形态仍是上游原样透传（Raw 字节恒等），`[DONE]` 只补 Chat 协议的终止符；帧形态转换不在本次范围。
3. **Anthropic translated 桥接补块闭合 / `stop_reason`**（不合规上游防护）：首个 `End` 且此前无 `Finish` 时，先合成 `Finish { finish_reason: None }`（编码器渲染 `content_block_stop` + `message_delta`（`stop_reason: end_turn`））再发 `message_stop`；合规上游（带 `finish_reason`）行为不变，不多出 `message_delta`。
4. **Responses translated 桥接补 `response.completed`**（不合规上游防护）：Responses ingress + 非 Responses upstream 时，首个 `End` 且此前无 `Finish` 时合成 `Finish { finish_reason: None }` → 编码器渲染 `response.completed`（`status: completed`）；合规上游不重复合成，native 透传不变。

实现上把 #2 / #3 统一为 `apply_end_synthesis_bridge`（`EndSynthesis` 枚举 + `flat_map` + `finish_seen` / `end_sent` 两个标志）。次序不变式不变：#1 在 empty-peek **之前**（依赖 `classify_raw_frame` 对 `data: [DONE]` 的 Empty 特判），#2 / #3 在 peek **之后**（合成帧无 Empty 特判，前置于 peek 会吞掉 `retry_on_empty`）；该次序已写入代码注释。

## 需求变更

- **修改 499 判定语义（统计口径变化）**：仅「客户端终点帧转发**之前**」的断连记为 `client_cancelled` / 499；终点帧**已转发之后**的断连记为 `Success` / 200（属正常完成尾，而非取消）。`error` 优先于终点帧标志——`stream_error_after_commit` / 502 的既有分类不变，`response.failed` 永不被升级为成功。
  - **运营影响**：499 计数下降、成功计数上升（历史量级每天几十到几百条），dashboard 曲线会出现一次性台阶。
- **新增行为约束（drop-as-success 行数据完整性）**：竞态成功行的 `call_logs` / analytics 必须携带完整 usage / `ttft_ms` / `upstream_duration_ms`；错误与取消终点的这些列仍为 NULL。
- **新增行为约束（translated 桥接终点帧）**：translated 路径（ingress 协议与 upstream 协议不一致）在首个 `End` 合成 ingress 协议的终止帧转发给客户端；native 透传路径字节不变。三条桥接：
  - #1 Chat ingress + Anthropic / Responses upstream → `data: [DONE]`（置于 empty-peek **之前**）
  - #2 Anthropic ingress + 非 Anthropic upstream → `message_stop`（置于 peek **之后**）
  - #3 Responses ingress + 非 Responses upstream → `response.completed`（置于 peek **之后**）
- **新增行为约束（不合规上游的终点完整性）**：上游未发 `finish_reason`（不合规）时，`#2` / `#3` 在首个 `End` 先合成 `Finish { finish_reason: None }` 再发终止帧——Anthropic 侧因此多出 `content_block_stop` + `message_delta`（`stop_reason: end_turn`），Responses 侧因此得到 `response.completed`；合规上游不重复合成（不产生第二条 `message_delta` / 第二个 `response.completed`）。
- **明确不变项**：502 / `stream_error_after_commit` 分类不变；pre-commit 阶段的 `DispatchCancelGuard` 499 不变；`call_logs` schema 不变；native 透传字节恒等；`retry_on_empty` 空响应重试语义不变（#2 / #3 位于 empty-peek 之后，#1 的 Empty 特判不变）；Chat ingress + Responses upstream 的响应帧形态不变（仍为上游原样 Raw 透传）。
- **运营影响补充**：上述第 2 条（Chat ingress + Responses upstream 末尾多一帧 `data: [DONE]`）与第 3 / 4 条（仅「无 `finish_reason` 的不合规上游」多出 `content_block_stop` + `message_delta`，或补出 `response.completed`）都是**协议合规修复**，不改变分类与统计口径——499 / 成功 / 502 的计数逻辑不变，`call_logs` 列语义不变。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-core/src/protocol.rs`](../../../../sag-core/src/protocol.rs) | 新增 `is_client_terminal_frame(&[u8]) -> bool`：对编码后的完整 SSE 帧做三协议终点帧并集判定（Chat `data: [DONE]` / Anthropic `message_stop` / Responses `response.completed` + `response.incomplete` + `response.cancelled`），行形态标记逐行锚定前缀、`"type":"..."` 引号形用子串匹配，防内容 delta 伪造；补 5 例判定测试（含伪造负例）。 |
| [`sag-core/src/lib.rs`](../../../../sag-core/src/lib.rs) | 重新导出 `is_client_terminal_frame`。 |
| [`sag-dispatch/src/dispatch/stream.rs`](../../../../sag-dispatch/src/dispatch/stream.rs) | `StreamAnalytics` 新增 `terminal_frame_forwarded`（yield 前置位）、共享 `usage` / `first_byte_at` / `upstream_start`；`TerminalGuard::Drop` 改三路分类（error → 502 / 标志 → 200 Success 带完整 usage+TTFT+upstream 时长 / 否则 499），新增 `DropTerminal` 承载三路结果；补 5 例竞态与回归测试。 |
| [`sag-dispatch/src/dispatch/chat/mod.rs`](../../../../sag-dispatch/src/dispatch/chat/mod.rs) | 新增 `apply_end_synthesis_bridge`（`EndSynthesis` 枚举：`AnthropicMessageStop` / `ResponsesCompleted`）：translated upstream 首个 `End` 且此前无 `Finish` 时先合成 `Finish { finish_reason: None }`，再合成 ingress 协议的终止帧（`message_stop` / `response.completed`）；仅作用于交给 `ChatAttempt::Stream` 的流（empty-peek 之后）。桥接 #1 启用条件放宽为 `is_chat_ingress && (use_anthropic || use_responses)`；补注释说明三个桥接的次序不变式。 |
| [`sag-dispatch/src/dispatch/chat/tests.rs`](../../sag-dispatch/src/dispatch/chat/tests.rs) | 新增 8 例桥接 / 回归测试：`message_stop` 可见性、native 字节恒等、空响应重试语义不变，以及 `[DONE]` 终止帧、块闭合、无重复 `message_delta`、合成 `response.completed`。 |
| [`sag-dispatch/src/dispatch/chat/empty.rs`](../../../../sag-dispatch/src/dispatch/chat/empty.rs) | `responses_raw_verdict` 补 `response.cancelled`（镜像 `response.incomplete`：无 output → `Empty`，有 output → `Content`），注释交叉引用 `is_client_terminal_frame`；补 2 例判定测试。 |
| [`docs/spec/dispatch/requirements.md`](../requirements.md) | 流式处理节补终点帧判定与 499 语义收紧；末尾「变更记录」追加条目。 |
| [`docs/spec/dispatch/spec.md`](../spec.md) | 补终点帧判定实现位置、`TerminalGuard` 三路分类、共享状态与桥接次序的不变式；末尾「变更记录」追加条目。 |

## 测试

- `sag-core/src/protocol.rs`：5 例终点帧判定——`client_terminal_frame_detects_chat_done`（含无空格变体、前导空白/CRLF、带 `event:` 行）、`client_terminal_frame_detects_anthropic_message_stop`、`client_terminal_frame_detects_responses_terminals`（`completed` / `incomplete`）、`client_terminal_frame_detects_responses_cancelled`（`event:` 行 / 紧凑 `type` / 带空格 `type` / 仅 `event:` 行），以及 `client_terminal_frame_ignores_non_terminal_frames`（内容 delta 中伪造的 `data: [DONE]` / `message_stop` / `response.completed` / `response.cancelled` 字样必须判为**非**终点帧）。
- `sag-dispatch/src/dispatch/stream.rs`：5 例——
  - `race_client_drop_after_done_classifies_success`：`[DONE]` 已转发、`End` 未入队时 drop body（`stream::pending()` 固定竞态窗口），断言 fact 为 `success`、`target_attempts` 为 `success`、`call_logs` 为 200 且 usage / cached / cache_creation / reasoning / `ttft_ms` / `upstream_duration_ms` 全部有值。
  - `midstream_disconnect_still_499`：终点帧前断连仍为 `client_cancelled` / 499 且 usage 列为 NULL。
  - `fake_terminal_text_in_content_still_499`：内容 delta 里伪造的终点帧字样不置位标志，断连仍判 499。
  - `upstream_error_then_drop_still_502`：error 优先于终点帧标志（502 / `stream_error_after_commit`）。
  - `terminal_frame_flag_set_on_encoded_terminal`：判定作用于**编码后**字节，Responses translated lane 由结构化 `Finish` 合成的终点帧同样置位。
- `sag-dispatch/src/dispatch/chat/tests.rs`：8 例——`anthropic_bridge_done_synthesis_race`（既有 `[DONE]` 桥接在竞态下同样判成功）、`anthropic_translated_message_stop_bridge`（translated 路径客户端可见 `message_stop`）、`anthropic_translated_message_stop_bridge_does_not_mask_empty_retry`（empty-peek 仍识别空响应并重试）、`anthropic_translated_message_stop_bridge_race_classifies_success`（桥接帧之后 drop 判 200），以及返工新增的 4 例：`chat_ingress_responses_upstream_gets_done_terminator`（Chat ingress + Responses upstream 末尾出现 `data: [DONE]`，响应帧仍为上游原样字节）、`anthropic_bridge_closes_block_without_finish_reason`（无 `finish_reason` 的上游得到 `content_block_stop` + `message_delta` + `message_stop`）、`anthropic_bridge_no_duplicate_message_delta_with_finish`（合规上游不产生第二条 `message_delta`）、`responses_bridge_synthesizes_completed_without_finish`（Responses translated 合成 `response.completed`，native 不重复合成）。
- `cargo test -p sag-dispatch`：268 通过；`cargo test -p sag-core`：142 通过；`just verify-rust` 全绿。
