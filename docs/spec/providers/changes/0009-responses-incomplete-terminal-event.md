---
created: 2026-10-29
updated: 2026-10-29
type: 修复
---

# 0009 — Responses 流式终点帧按 finish reason 区分 `response.incomplete`

## 动机

Responses 入口 + Chat-Completions 上游（translated lane）时，上游不会发出 Responses 协议的终止帧，由 `sag-providers/src/codec/responses.rs` 的 `Finish` 分支合成。旧实现在合成帧里把 `length` / `content_filter` 渲染成 `status: "incomplete"`，但**事件类型恒为 `response.completed`**，且响应对象里没有 `incomplete_details` 键。

按 OpenAI Responses 流式 schema，`max_output_tokens` 截断的正确形态是 `response.incomplete` + `status: "incomplete"` + `incomplete_details.reason`，而不是「`response.completed` 携带 incomplete 状态」；`incomplete_details` 在 `Response` 上是 required-but-nullable，键缺失本身也不合规。

真实受害者是 pi 的 Responses 客户端：`openai-responses-shared.js:527-531` 计算 `incompleteReason = response?.incomplete_details?.reason`，`:748-757` 在 `status === "incomplete"` 而拿不到 reason 时直接抛 `Response incomplete without a provider reason`。截断响应因此变成客户端侧异常，而不是可处理的 `max_output_tokens` 停止。

## 需求变更

[`docs/spec/providers/requirements.md`](../requirements.md) 的「预期行为」节此前没有收录 Responses 流式终点帧的形态契约（该行为一直只在实现与 dispatch 侧描述），本次未改动该文件；行为契约记录于此，后续如补「响应翻译」子条目，可直接引用本文件的映射表。

[`docs/spec/dispatch/spec.md`](../../dispatch/spec.md) 与 [`docs/spec/dispatch/requirements.md`](../../dispatch/requirements.md) 中「translated 桥接 #3 恒为 `response.completed`」的措辞已同步为「按 finish reason 取 `response.completed` / `response.incomplete`」。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-providers/src/codec/responses.rs`](../../../../sag-providers/src/codec/responses.rs) | 新增 `terminal_event_fields(finish: Option<&IrFinishReason>) -> (&'static str, &'static str, Option<&'static str>)` 作为 finish reason → (事件类型, status, incomplete reason) 的唯一映射；新增 `encode_terminal_event(...)` 渲染帧并在有 reason 时写入 `response.incomplete_details: {"reason": …}`（无 reason 时该键完全缺省）；`ResponsesCodec::encode_stream_event` 与 `StreamEncodeState::encode` 两处 `Finish` 分支改为共用它，删除重复的 `encode_completed` |

映射表（仅 translated 合成路径；native 路径上游帧原样 Raw 透传，不经过此表）：

| `IrFinishReason` | 事件 `type` | `response.status` | `response.incomplete_details.reason` |
|---|---|---|---|
| `Length` | `response.incomplete` | `incomplete` | `max_output_tokens` |
| `ContentFilter` | `response.incomplete` | `incomplete` | `content_filter` |
| `Stop` / `ToolCalls` / `FunctionCall` / `Other(..)`（除 `"failed"`）/ `None` | `response.completed` | `completed` | 缺省 |
| `Cancelled` | `response.completed` | `cancelled` | 缺省 |
| `Other("failed")` | `response.completed` | `failed` | 缺省 |

信封其余部分不变：`{type, response: {object, status, output: []}}`，无 `sequence_number`（合成帧只保证终点语义，不做全量事件流编号）。

**有意留在范围外的部分**：`Cancelled` 仍发 `response.completed` + `status: "cancelled"`，`Other("failed")` 仍发 `response.completed` + `status: "failed"`。二者与本次修复的 `incomplete_details` 缺口无关（不涉及 reason 缺失），且把 `failed` 改成 `response.failed` 会与 dispatch 的终点帧 / 502 分类口径联动（`response.failed` 在 `is_client_terminal_frame` 中不算终点、其 Error 帧走 502），需要单独变更与回归，故不在本次范围内。

非流式 `encode_response` 与 `status_to_finish`（反方向的 `status` + `incomplete_details` → `IrFinishReason`）均未改动。

## 测试

- `sag-providers/src/codec/responses.rs` 内联单元测试（新增 4 例）：
  - `stream_finish_length_emits_incomplete_with_max_output_tokens` — `Length` → `response.incomplete` + `status: "incomplete"` + `incomplete_details.reason = "max_output_tokens"`，且帧内不出现 `response.completed`
  - `stream_finish_content_filter_emits_incomplete_with_content_filter` — `ContentFilter` → 同上，reason 为 `"content_filter"`
  - `stream_finish_stop_emits_completed_without_incomplete_details` — `Stop` → `response.completed` + `status: "completed"`，`incomplete_details` 键完全缺省
  - `stream_finish_terminal_frame_is_shared_by_both_entry_points` — 对 `Length` / `ContentFilter` / `Stop` / `ToolCalls` / `Cancelled` / `Other("failed")` / `Other("weird")` / `None` 逐一断言无状态入口（`encode_stream_event`）与有状态入口（`stream_encoder`）输出的终点帧字节一致
- [`sag-router/tests/responses_ingress.rs`](../../../../sag-router/tests/responses_ingress.rs) 入口级集成（新增 1 例）：`responses_ingress_chat_upstream_stream_length_emits_response_incomplete` — Responses ingress + Chat-Completions upstream 的 `length` 截断整链，把最后一帧 `data:` payload 解析成 JSON 后断言 `type = response.incomplete`、`response.status = incomplete`、`response.incomplete_details.reason = max_output_tokens`（结构断言：`incomplete_details` 若误写成 `response` 的兄弟节点会失败），并断言流内不出现 `response.completed` 与 `[DONE]`。与 codec 单元测试互补：单测钉帧形态，入口测试钉「translated 桥接 + 编码器」整链。
- 既有用例无一翻转：终点帧判定（`sag-core/src/protocol.rs` 的 `is_client_terminal_frame`）本已覆盖 `response.incomplete`，dispatch 侧的 `terminal_frame_forwarded`、IR 级 `Finish` / `End` 处理与 usage 累计均不依赖出站字节里的 `type` 字符串，analytics 无 finish-reason 列。
