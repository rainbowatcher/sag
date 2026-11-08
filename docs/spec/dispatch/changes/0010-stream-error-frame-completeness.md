---
created: 2026-11-08
updated: 2026-11-08
type: 修复
---

# 0010 — 流内错误帧在两种车道上的完整性（Responses 出口 / 网关本地中断）

## 动机

0009 把 `IrStreamEvent::Error` 从「只进 analytics」改成「翻译车道交给出口编码器合成错误帧」，但留下了三个同源缺口，客户端侧的表现都是**静默未终止的 200 SSE**：

1. **Responses 出口没有错误帧**。`ResponsesCodec::encode_stream_event` 与 `StreamEncodeState::encode` 的 `Error` 分支都返回 `Vec::new()`，而 Responses ingress × Chat/Anthropic upstream（翻译车道）正是把 `Error` 交给这个编码器。`stream.rs` 的流循环见 `bytes.is_empty()` 就跳过，同时抑制后续所有事件，`response.completed` 的桥接又因 `errored` 让路——客户端只收到内容 delta，然后 body 结束。
2. **直通车道吞掉网关本地产生的中断**。`stream.rs` 对 `emit_synthesized_error == false` 的 `Error` 无条件 `continue`，前提是「上游自己的错误字节会作为独立 `Raw` 到达」。这对**入站上游错误帧**成立（provider 的 side-channel `Error` chunk 后紧跟该帧的 `Data`），但对**网关本地**中断（上游读错误、chunk 空闲超时）不成立：`sag-providers/src/util.rs::emit_idle_timeout`、`openai.rs` / `anthropic.rs` 的 `Ok(Some(Err(e)))` 分支只发 `StreamChunk::Error`，且各 adapter 置 `had_error` 抑制 `Done`——1:1 映射后唯一的载体就是 `Error`，被丢弃后客户端既没有错误帧也没有 `[DONE]` / `message_stop` / `response.completed`。
3. **畸形 / 非 Responses 帧泄漏到转译出口**。转译车道只丢弃 `is_responses_event_frame` 为真的 `Raw`（可解析 JSON 且 `type` 以 `response.` 开头），而 Responses 解码器对畸形 JSON、缺 `type`、未知 `type`、无 `data:` 行的帧同样发 `Raw`——这些字节原样进了 Chat / Anthropic 客户端。附带一个小缺陷：多行 `data:` 事件逐行解析，每行失败都 push 整帧，于是双畸形行事件产出两条相同 `Raw`。

## 需求变更

[`docs/spec/dispatch/requirements.md`](../requirements.md) 的「流式处理」节：

- **修正（流内错误对客户端可见）**：两种车道的最终形态统一为「恰好一帧 ingress 协议错误帧，其后无任何字节」。翻译车道不变（直接交给编码器）；直通车道改为先扣住 `Error`，紧随其后的 `Raw`（上游自己的错误帧）原样转发并清掉扣留，流结束时扣留仍在则说明是网关本地中断，终点合成一帧。新增 Responses 出口的错误帧形态（`data: {"type":"error","code","message","param","error":{…}}`）。
- **修正（Responses 帧过滤）**：已解码车道上丢弃 **Responses 解码器产出的所有 `Raw`**，不再做载荷内容嗅探；同协议车道不变。
- **修正（chunk 超时）**：这类中断是网关本地产生的，两种车道都会合成错误帧（旧描述称直通车道不合成）。

## 设计

- `sag-providers/src/codec/responses.rs`：新增 `encode_error_event(message)`——`{"type":"error","code":"STREAM_ERROR","message","param":null}` 加一份嵌套的 `error: {code, message}`（让只认网关通用信封 `{"error":{"code","message"}}` 的客户端也能读到上游消息）；无状态 `encode_stream_event` 与有状态 `StreamEncodeState` 共用它。`StreamEncodeState` 新增 `errored`：`Error` 之后 `Raw` / `Finish` / `End` 一律返回空 vec（`Start` 重置），打开中的 item 故意不闭合——错误帧就是该流最后一帧，与 native Responses 错误流同形。
- `sag-providers/src/codec/responses.rs`：`decode_frame` 增加 per-frame `raw_emitted` 标志（`push_frame_raw`），一帧最多产出一条 `Raw`。
- `sag-dispatch/src/dispatch/stream.rs`：`StreamState` 新增 `pending_local_error`。`Error` 在直通车道只记录并扣留（重复事件只进 analytics）；任何随后成功编码出字节的事件清掉扣留（上游自己的错误帧已发出）；`End | None` 终点先取扣留 → 交给 ingress 编码器 → 与隐私尾合并成同一批字节下发（`terminal_pending` 保证终点记账在最后），随后 body 关闭。翻译车道路径不变。
- `sag-dispatch/src/dispatch/chat/mod.rs`：`use_responses && needs_ir_decode` 分支改为丢弃所有 `Raw`，删除已无用的 `is_responses_event_frame`。

## 测试

- `sag-providers/src/codec/responses.rs`：`stream_encoder_error_renders_one_error_event_and_suppresses_terminal`（两个入口字节一致、恰好一帧、`message`/`code`/嵌套 `error` 齐全、其后无 `output_item.done` / `response.completed`）、`stream_multi_line_malformed_frame_yields_single_raw`。
- `sag-dispatch/src/dispatch/stream.rs`：`privacy_tail_flushes_when_upstream_errors_out` 改为断言「隐私尾 + 恰好一帧合成错误帧、无 `[DONE]`」。
- `sag-router/tests/protocol_matrix.rs`（新增 9 例）：三条直通车道（Chat→Chat、Anthropic→Anthropic native、Responses→Responses native）各跑**读错误**（chunked 无终止 chunk）与**空闲超时**（`chunk_timeout_secs = 1`）两种中断，断言一条 ingress 协议错误帧 + body 正常关闭 + 无成功终止符；`responses_ingress_chat_upstream_error_yields_single_responses_error_event`（Responses ingress × Chat upstream，错误帧是最后一帧、无 `response.completed`、上游 `rate_limit_exceeded` 不泄漏）；`responses_ingress_responses_upstream_failed_is_forwarded_verbatim`（native 车道 `response.failed` 原样转发、零合成帧）。

## 影响

- 客户端可见行为：Responses 出口与两种车道的网关本地中断从「静默未终止」变为「一帧协议错误帧 + body 关闭」。HTTP 仍 200，attempt / `failure_reason` / `call_logs` 口径不变。
- 上游错误帧先于任何内容到达时（empty-peek 判 `UpstreamError`）仍是**提交前 502**，不进入本路径。
- 转译车道上畸形 / 未知类型的 Responses 帧不再转发（跨协议出口本就没有它们的表示）。
