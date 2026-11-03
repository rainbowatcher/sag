---
created: 2026-11-03
updated: 2026-11-03
type: 修复
---

# 0010 — Responses 流式 item 载荷保真与已知 bookkeeping 帧不外泄

## 动机

Responses 流式链路的编码器（`ResponsesCodec::stream_encoder`）与解码器（`ResponsesCodec::decode_stream_frame` / `stream_decoder`）各有一组缺陷，共同表现为「工具调用的 `arguments` 在某一跳丢失」或「客户端收到不属于自己协议的帧」。三个缺陷各自独立、可单独复现：

1. **编码器发出空壳 item**。`close_open_item` 对 `function_call` 只写 `{"type": "function_call", "status": "completed"}`——`call_id` / `name` / `arguments` 全丢；对 `message` 写 `content: []`；`encode_terminal_event` 又把 `response.completed` 的 `output` 硬编码成 `[]`。按 Responses schema，客户端是从 `response.output` 里取最终结果的，于是整条流只「看起来」有工具调用：真收到空参数的客户端会把 `get_weather({})` 当合法调用发出去。
2. **解码器丢掉 `output_item.done`**。旧 `decode_stream_frame` 是无状态的自由函数，把 `response.output_item.done` 归进「bookkeeping — 无 IR 事件」臂。这条注释只对 `message` 成立（内容由 delta 供给，且后续 delta 会重新开块）；对 `function_call`，`done` 可能是 `arguments` 的**唯一**载体，载荷就此消失。
3. **已知 bookkeeping 事件以 `Raw` 泄漏**。修好第 2 项之后，跨协议车道（Anthropic 入口 × Responses 上游）客户端仍会收到零散 Responses 帧：未识别的事件名落到 `IrStreamEvent::Raw`，而 Anthropic 编码器把 `Raw` 逐字转发，客户端于是收到它解析不了的 `response.content_part.added` 一类帧。

三者的共同点是「同一条流里 item 的载荷语义没有被显式建模」：编码器不知道 item 关闭时该带什么，解码器不知道 `done` 该不该供载荷，落不到 IR 的帧则默认走 `Raw` 透传。

## 需求变更

[`docs/spec/providers/requirements.md`](../requirements.md) 的「预期行为」各子节（`reasoning_effort` 归一化 / 请求翻译 / 工具定义跨 codec 保真 / 上游协议请求响应体 typed 化 / 响应翻译 / 角色映射 …）都没有定义 Responses 流式 **item 生命周期**与终止帧载荷的契约——该行为一直只存在于实现与 dispatch 侧描述中，故本次未新增需求子节。行为契约落在 [`docs/spec/providers/spec.md`](../spec.md) 新增的「Responses 流式 item 生命周期与终止载荷」决策节，本文件给出映射表与不变式。

`requirements.md` 仅在其末尾「变更记录」节追加本文件回链。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-providers/src/codec/responses.rs`](../../../../sag-providers/src/codec/responses.rs) | 三处修复集中在同一文件：`StreamEncodeState` 新增 5 个字段累积在途 item 载荷并记录已关闭 item；`close_open_item` 对每个 `OpenItem` 变体输出完整 item（`added` / `done` 共用 index 派生 id）；`encode_terminal_event` 收 `completed: &[Value]` 并把真实 `response.output` 渲染出来；解码侧从无状态自由函数改为 `StreamDecodeState` + `stream_decoder()`（`FnMut`），`output_item.done` 按 `deltas_seen` 决定是否供载荷；已知 bookkeeping 事件族改为返回空 vec 而非 `Raw` |

### 编码器：item 关闭时带全载荷，终止帧带真实 `output`

`StreamEncodeState` 新增字段：

| 字段 | 作用 |
|---|---|
| `open_call_id` / `open_name` | 从 `ToolCallStart` 记住 `output_item.added` 帧发出的 `call_id` / `name`，供 `done` 复用同一对值 |
| `open_args` | 累积 `ToolCallDelta` 的 `arguments` 片段 |
| `open_text` | 累积文本 / reasoning delta 的正文 |
| `completed_items: BTreeMap<u32, Value>` | 已关闭 item 按 `output_index` 排序；终止帧按升序渲染 |

`close_open_item` 对每个 `OpenItem` 变体输出（`id` 由 `output_index` 派生，`added` 与 `done` 用同一个值，客户端可据此把两帧对上）：

| `OpenItem` | `id` | `done` 帧里的 `item` |
|---|---|---|
| `FunctionCall` | `fc_{index}` | `{type: "function_call", id, call_id, name, arguments, status: "completed"}` |
| `Message` | `msg_{index}` | `{type: "message", id, role: "assistant", status: "completed", content: [{type: "output_text", text, annotations: []}]}` |
| `Reasoning` | `rs_{index}` | `{type: "reasoning", id, summary, status: "completed"}`，`summary` 在有文本时为 `[{type: "summary_text", text}]`，无文本时为 `[]` |

`message` 关闭时先发 `response.content_part.done`（`part.text` 为累积正文），再发 `response.output_item.done`；关闭的 item 同时写入 `completed_items`。

`encode_terminal_event(finish, completed: &[Value])` 把 `completed` 直接渲染为 `response.output`，不再写死 `[]`。终止帧的其余部分与 0009 一致（`terminal_event_fields` 的 finish reason → 事件类型 / status / incomplete reason 映射不变）。`End` 分支同步调整：**流在 item 仍打开时结束也要先关闭它**，否则终止载荷仍不完整；`End` 与 `Finish` 都只在 `completed == false` 时合成终止帧，`Raw` 已带过 `response.completed` 时不重复发。

无状态入口 `encode_stream_event` 的 `Finish` 分支传 `&[]`——它没有 item 历史，因此仍是 `output: []`（既有的 `stream_finish_terminal_frame_is_shared_by_both_entry_points` 断言两入口在「无 item」状态下字节一致，仍然成立）。

### 解码器：`done` 只在 delta 缺席时供载荷

`decode_stream_frame` 保留为**逐帧无状态包装**（每次调用新建一个 `StreamDecodeState`），供单帧 / 测试场景使用（真实流走新增的 `stream_decoder() -> impl FnMut(&[u8]) -> Vec<IrStreamEvent> + Send`），两者共用同一个 `decode_frame` 函数体。

`StreamDecodeState`：

| 字段 | 作用 |
|---|---|
| `deltas_seen: HashSet<u32>` | 该 `output_index` 的载荷是否已经由 delta 供给过 |
| `open_items: HashMap<u32, Value>` | `response.output_item.added` 的 item 载荷，供 `done` 回填 `added` 缺席时缺的字段 |

**不变式：每个 item 的载荷恰好一次进入 IR** —— 只有该 `output_index` 从未出现过 delta 时，`done` 才是载荷的唯一供给者；出现过 delta 时 delta 是唯一供给者，`done` 保持静默。

`done` 分支按 item 类型取值：

| item `type` | 供给的 IR 事件 |
|---|---|
| `function_call` | `arguments` 非空时发 `ToolCallDelta`；`added` 帧缺席时兼发一次 `ToolCallStart`（`call_id` / `name` 优先取 `done` 的 item，缺失则回落到 `added` 的载荷），保证 IR 形状与有 `added` 时一致 |
| `message` | 把 `content` 各 part 的 `text` 拼起来（`item_part_text`），非空时发 `TextDelta` |
| `reasoning` | 取 `summary` 的 part 文本，为空则回落 `content`，非空时发 `ReasoningDelta` |
| 其余 / 空 item | 不发事件（bookkeeping） |

为什么 `done` 带全载荷不是「重复应用」风险：OpenAI 的真实流既在 `output_item.done` 发完整 item，也在 `response.completed.output` 发完整数组，所以按 delta 累积的客户端必须把 `done` 当**权威替换**而非增量。反过来说，若只发空壳，累积 delta 的客户端恰好「看起来正常」，SDK 客户端（直接读 `done` / `output`）则静默拿到空参数——缺陷因此长期未被发现。

### bookkeeping：已知事件返回空 vec，不落 `Raw`

已知 Responses bookkeeping 事件族改为**返回空 vec**（既非内容也非 `Raw`），原因是跨协议出口会把 `Raw` 逐字转发：

| 事件 | 处理 |
|---|---|
| `response.in_progress` | 空（纯状态帧） |
| `response.content_part.added` / `response.content_part.done` | 空（part 文本由 delta 供给，`done` 已带完整 item，从终止帧重建会把同一段文本发两遍） |
| `response.output_text.done` / `response.function_call_arguments.done` | 空（同上，内容由 delta + `output_item.done` 供给） |
| `response.reasoning_summary_text.done` / `response.reasoning_text.done` | 空 |
| `response.reasoning_summary_part.added` / `response.reasoning_summary_part.done` | 空 |
| `response.output_text.annotation.added` | 空（IR 无 annotation 槽位，citations 留在上游） |
| `response.output_item.added` 的宿主工具项（如 `web_search_call`） | 空（其载荷 IR 从不消费） |
| `response.output_item.added` 的 `message` / `reasoning` | 空（只宣告 item，内容由 delta 供给；`function_call` 仍发 `ToolCallStart`） |
| `response.created` | 仍是 `Start`（唯一携带 IR 内容的 bookkeeping 邻接帧，绝不落 `Raw`） |

**有意留在范围外的部分**：

- **没有加 catch-all 丢弃**。`Raw` 仍为「确实无法归类」的帧保留：非 JSON 的 `data:` 载荷、顶层 `{"error": ...}`、没有 `type` 的帧、SSE 注释 / 无 `data:` 的兜底、以及通用未知 `type` 的 catch-all。同协议车道（Responses 上游 × Responses 入口）靠 `Raw` 透传上游新增事件，因此未来出现新的未知事件类型时仍会照常转发，而不是被静默吞掉。
- **终止 / usage 语义不动**：`response.completed` 仍发 `Raw` **加** `Usage`；`response.failed` 仍发 `Error` + `Raw`；`response.incomplete` 仍发 `Finish` + `Usage` + `Raw`。本次只改 `Raw` 的**来源集合**（去掉已知 bookkeeping 族），不改终点帧与错误帧的分类。
- **native 同协议车道不受影响**：Responses 上游 × Responses 入口走 `Raw` 直通路径，解码器根本不参与，编码器只负责把 `Raw` 原样发出（并据 `response.completed` 抑制重复合成终止帧）。
- **非流式 `encode_response` / `decode_response` 与 `status_to_finish` 未改动**，与 0009 一致。

## 测试

`sag-providers/src/codec/responses.rs` 内联单元测试（新增 7 例）：

- `output_item_done_carries_function_call_payload` — 编码器关闭 `function_call` 时，`response.output_item.done` 的 `item` 必须带 `arguments`、非空 `call_id`、非空 `name`，且 `item.id` 与 `response.output_item.added` 帧里的 `id` 相同。**修复前失败**（旧实现只发 `{type, status}`）。
- `response_completed_output_lists_all_items_in_index_order` — `response.completed` 的 `response.output` 必须列出全部已关闭 item（本例 2 个），且按 `output_index` 升序（`fc_0` 在前、`msg_1` 在后）。**修复前失败**（旧实现硬编码 `output: []`）。
- `output_item_done_supplies_arguments_when_no_deltas` — 无 delta 时 `output_item.done` 是 `arguments` 的唯一供给者：`ToolCallDelta` 恰好一次，`ToolCallStart` 也恰好一次（来自 `added`）。**修复前失败**（旧实现把 `done` 归入 bookkeeping 直接丢弃）。
- `output_item_done_does_not_duplicate_streamed_deltas` — 已由两个 delta 供给载荷后，`done` 必须静默，`arguments` 不得被重发。
- `stream_roundtrip_encode_then_decode_delivers_arguments_once` — 编码 → 解码整链：`ToolCallStart` 一次、`arguments` 一次且逐字等于 `{"city":"SF"}`，钉住「编码器发全载荷」与「解码器只供一次」两半配合。
- `known_bookkeeping_events_yield_no_raw` — 用**同一个**顺序解码器逐帧喂入上表全部已知 bookkeeping 事件，断言每帧都解出空 vec（共享状态不得让某帧「碰巧」看起来像 bookkeeping）；`response.created` 仍是纯 `Start`；`web_search_call` 的 `output_item.added` 同样为空。
- `unknown_event_type_still_surfaces_as_raw` — 刻意保留的非 catch-all：未知 `type` 仍产出恰好一条 `Raw`，保证同协议透传车道不会被这次收紧误伤。

既有用例无一翻转：`stream_finish_terminal_frame_is_shared_by_both_entry_points`（无 item 状态下两入口字节一致）、`stream_encoder_native_completed_raw_marks_terminal`（`Raw` 已带 `response.completed` 时不重复合成）、usage 降级与 `response.incomplete` 分档用例均不依赖本次改动。
