---
created: 2026-11-03
updated: 2026-11-04
type: 修复
---

# 0009 — 工具调用参数质量告警与 Chat×Responses 车道的解码归属修正

## 动机

本轮修的是四个各自独立、但共同表现为「网关看得见问题却什么都没说」的缺陷：

1. **空 / 非法 `arguments` 静默通过**。上游确实会发出 `arguments` 为空、空白、或根本不是 JSON 对象的工具调用（`""`、`null`、`[1]`、`{bad`）。这类响应在 chat IR 里与非空的完全同形——`tool_calls` 数组非空即算内容——因此既不重试也不降级，`call_logs` 是一行干净的 200，客户端拿到 `get_weather({})` 也没有任何预警。需要一个**只观测、不干预**的信号，而不是顺手改掉空响应判定（那会动到重试与降级语义）。
2. **最常用的一条车道上该信号是瞎的**。Chat ingress × Chat upstream 是字节恒等透传，每个事件都是 `IrStreamEvent::Raw(Vec<u8>)`（完整 SSE 帧，含 `data: ` 前缀与尾部 `\n\n`），IR 层的观察器在这一条车道上什么也看不到。要覆盖它，只能回头读一遍**已经成帧**的 SSE 字节——但绝不允许改写它们。
3. **解码器选择器漏了 Responses 分支**。`StreamFrameDecoder` 的选择只判 `use_anthropic`，其余一律落到 `ChatCompletionsCodec::decode_stream_frame`。于是 **Anthropic ingress × Responses upstream 一直在用 Chat 解码器解 Responses 帧**：Responses 帧里没有 `choices`，每帧都退化成 `Raw`，Anthropic 编码器再把 `Raw` 逐字转发——客户端收到 `response.*` 事件名，文本永远变不成 `text_delta`，`message_start` 也从未被合成。同一处 fallthrough 还让 **Chat ingress × Responses upstream** 保持原样透传：OpenAI SDK / Codex CLI 收到的是完全没有 `choices` 的 Responses 帧。
4. **跨协议出口的 Responses `Raw` 帧泄漏**。Responses 解码器为了不让同协议透传车道丢帧，必须把「无法表达为 IR」的帧作为 `Raw` 交出来（`response.completed`、未知事件类型、不可解析的 data）。这些帧在别的协议的出口上没有对应物，逐字转发即协议泄漏。

这几项的共同点是「判断归属时只看了一半的座标」：看 IR 不看原始字节，或看 ingress 不看 upstream 协议。本文件只覆盖 `sag-dispatch` 侧；同一轮修复中 codec 侧「`output_item.done` 丢载荷 / 编码器发空壳 item / 已知 bookkeeping 帧落 `Raw`」的那一半见 [`docs/spec/providers/changes/0010-responses-stream-item-payload-fidelity.md`](../../providers/changes/0010-responses-stream-item-payload-fidelity.md)。

## 需求变更

[`docs/spec/dispatch/requirements.md`](../requirements.md) 的「流式处理」节新增四条行为约束、并修正两条已失效的描述：

- **修正（translated 桥接）**：原描述「Chat ingress + Anthropic / Responses upstream → `data: [DONE]`（响应帧仍为上游原样透传）」对 Responses 上游不再成立。Responses 上游的帧先解码为 IR、再按 Chat 协议重编码，`data: [DONE]` 只是在此之上补的终止符；字节恒等只对同构车道（chat→chat、native→native）成立。旧描述保留在本文件的动机第 3 项里。
- **新增（工具调用参数质量告警）**：`arguments` 为空 / 空白 / 非 JSON 对象时，**流式每流一次、非流式每响应一次**打一条 `warn!`，携带工具调用数与最差质量档；该信号**不改变任何判定**——不计失败、不触发重试或降级、不改写响应体字节、不参与 `call_logs` 分类。
- **新增（解码归属与帧过滤）**：帧的解码归属由 **upstream 协议**决定，不由 ingress 协议单独决定；`needs_ir_decode` 的判定包含 Chat ingress × Responses upstream。已解码车道上，Responses 形状的 `Raw` 帧不得流向非 Responses 出口；`needs_ir_decode == false` 的同协议车道不过滤、不改写。
- 「不包含」节不变：本变更不把空白 `arguments` 纳入空响应判定（属有意的行为变更，须单独决策，见下文「有意不改」）。
- **新增（`finish_reason` 完整性）**：Chat ingress 的 IR 解码车道上，`response.completed` 必须解码出 `Finish`（reason 由 `status_to_finish` 派生，`response.output` 含 `function_call` 时折叠为 `tool_calls`）；上游全程未发 `finish_reason` 时，由桥接 #4 在 `data: [DONE]` 之前补一个 `finish_reason: stop` chunk。否则 Chat 客户端拿到的是「有内容、无 `finish_reason`」的流，与截断无法区分。
- **新增（流内错误对客户端可见）**：HTTP 200 已提交后的上游失败，在**翻译车道**必须以**协议正确的错误帧**告知客户端（Chat 裸 `data: {"error":…}`、Anthropic `event: error` + `{"type":"error",…}`），且错误帧之后**不再发终止符**（`[DONE]` / `message_stop` / 合成的 `finish_reason` / `stop_reason`）；**直通车道**零合成帧，上游自己的错误字节与其后的终止符（含 `data: [DONE]`）原样转发；HTTP 状态、attempt 的 `TargetStatus` / `failure_reason`、analytics 错误记账与 `Usage` 处理均不变。
- **修正（chunk 超时的错误帧归属）**：旧描述称超时错误帧经 `StreamChunk::to_sse_bytes` 固定序列化为 `{"error":{"code":"STREAM_ERROR","message":…}}`；现在翻译车道由出口编码器按 ingress 协议合成，旧信封不再用于流式出口（直通车道不合成任何帧）。

末尾「变更记录」追加本文件回链。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-core/src/ir/tool_args.rs`](../../../../sag-core/src/ir/tool_args.rs) | 新增纯谓词层：`ToolArgsQuality`（`Ok` < `Blank` < `NotJson`，`Ord` 即严重度序）、`args_quality(&str)`、`args_value_quality(&Value)`、`ToolArgWatch`（`observe` / `observe_tool_call_start` / `observe_arguments` / `quality` / `saw_tool_call` / `tool_call_count`） |
| [`sag-core/src/ir/mod.rs`](../../../../sag-core/src/ir/mod.rs) | 注册 `tool_args` 模块并 re-export 四个公开项 |
| [`sag-core/src/lib.rs`](../../../../sag-core/src/lib.rs) | 顶层 re-export 同步（`ir` 白名单） |
| [`sag-dispatch/src/dispatch/stream.rs`](../../../../sag-dispatch/src/dispatch/stream.rs) | 新增共享句柄 `ToolArgWatchHandle`（`Arc<Mutex<ToolArgWatch>>`，`observe` / `verdict`）；`build_streaming_response` 增尾部参数 `tool_args`；`finish_stream_terminal` 在 claim 终点之后、投递终止帧之前读一次 `verdict()` 并按需 `warn!`；流循环里的 `IrStreamEvent::Error` 不再单独消费，改为**先** `record_stream_error` **再**落回共享的编码路径（同一种失败第二次到只进 analytics，不重复外发） |
| [`sag-dispatch/src/dispatch/chat/mod.rs`](../../../../sag-dispatch/src/dispatch/chat/mod.rs) | 新增 `watch_tool_args`（fold 每个事件进 watch，`Raw` 帧按需走窥探）、`peek_chat_raw_frame`（raw SSE 帧窥探，observation-only）、`ToolArgSink`（一个解析器两个落点）；`non_stream_tool_args_quality` + `ChatAttempt::NonStream` 臂的 warn；`StreamFrameDecoder` 改 `Box<dyn FnMut ... + Send>` 并补 `use_responses` 分支；`needs_ir_decode` 纳入 `is_chat_ingress && use_responses`；新增 `is_responses_event_frame` 过滤已解码车道的 Responses 形状 `Raw` 帧；新增 `EndSynthesis::ChatFinishReason`（桥接 #4，`finish_reason: Some(Stop)`，并认已渲染的 `Raw(data: [DONE])` 作终止载体）；桥接 #1 与 #4 各自持有 per-stream `errored` 标志（错误帧后不再发 `[DONE]`、不再合成 `finish_reason`） |
| [`sag-dispatch/src/dispatch/chat/empty.rs`](../../../../sag-dispatch/src/dispatch/chat/empty.rs) | `is_empty_response_data` / `choice_has_message_content` / `ir_item_has_content` **只补文档注释**（指向 `args_quality`，并注明质量感知化必须挂在 `backoff.retry_on_empty_response` 之后、属有意的行为变更）；新增 4 例钉住当前判定的测试 |
| [`sag-dispatch/src/dispatch/chat/tests.rs`](../../../../sag-dispatch/src/dispatch/chat/tests.rs) | 新增内联 `CapturedEvents` 事件捕获订阅器与 5 例分发测试（含 warn 计数、字节恒等、跨协议重编码、Anthropic×Responses 解码归属）；新增 `chat_ingress_without_upstream_finish_still_emits_finish_reason`（桥接 #4），并给 `chat_ingress_responses_upstream_reencodes_as_chat_chunks` 补 `finish_reason` 断言 |
| [`sag-providers/src/codec/responses.rs`](../../../../sag-providers/src/codec/responses.rs) | `response.completed` 从「只发 `Raw` + `Usage`」改为先发 `Finish`（`status` + `incomplete_details` 经 `status_to_finish`，`output[]` 含 `function_call` 时折叠为 `tool_calls`，无 `status` 时按事件名回退 `stop` / `tool_calls`），再发 `Usage` 与 `Raw` |
| [`sag-providers/src/codec/anthropic.rs`](../../../../sag-providers/src/codec/anthropic.rs) | `encode_stream_event` 与 `StreamEncodeState::encode` 的 `Error` 分支渲染 `event: error` + `{"type":"error","error":{"type":"api_error","message":…}}`；新增 `StreamEncodeState.errored`，置位后 `Finish` / `End` 返回空 vec（不出 `stop_reason` / `message_stop`） |
| [`sag-providers/src/codec/chat.rs`](../../../../sag-providers/src/codec/chat.rs) | `Error` 分支改为官方 OpenAI 流内错误形状（`message` / `type: server_error` / `code: STREAM_ERROR` / `param: null`），与 `StreamChunk::to_sse_bytes` 的旧信封（仅 `code` / `message`）分道 |
| [`sag-router/tests/protocol_matrix.rs`](../../../../sag-router/tests/protocol_matrix.rs) | `anthropic_x_responses_native_stream` 的 mock 由 Chat chunk 改正为真正 Responses 形状的 SSE；`chat_x_responses_native_stream` 的断言从 `contains("response.created")` 翻转为「必须重编码为 Chat chunk」，并补「恰好一个 `finish_reason`」；新增 `chat_ingress_responses_failed_emits_error_frame` / `anthropic_ingress_responses_failed_emits_error_event` 两例流内错误端到端 |
| [`docs/spec/dispatch/requirements.md`](../requirements.md) | 「流式处理」节修正 translated 桥接描述，新增告警与解码归属两条约束；末尾追加回链 |
| [`docs/spec/dispatch/spec.md`](../spec.md) | 新增「工具调用参数质量告警」「流式解码归属与 Responses 帧过滤」两节决策；修正 translated 桥接节里 Chat×Responses 的字节恒等描述；补不变式与关键文件；末尾追加回链 |
| [`docs/spec/dispatch/changes/0007-stream-terminal-frame-race.md`](0007-stream-terminal-frame-race.md) | 修正一处失效引用：`chat_ingress_responses_upstream_gets_done_terminator` → 现名 `chat_ingress_responses_upstream_reencodes_as_chat_chunks`，并把「响应帧仍为上游原样字节」改为「解码后重编码为 Chat chunk，字节恒等只对同构车道成立」 |

### 谓词层：一个纯函数回答「这条工具调用的参数能用吗」

`sag-core/src/ir/tool_args.rs` 只有纯逻辑（无 I/O、无日志、无副作用），发不发日志由 `sag-dispatch` 决定：

| 输入 | `args_quality` |
|---|---|
| `{}` / `{"city":"SF"}` | `Ok` |
| `""` / `"   "` / `"\n\t"` | `Blank` |
| `{bad` / `null` / `"x"` / `[1]` / `42` / `true` | `NotJson` |

`{}` 是**合法**参数（无参函数），空白是**缺失**，其余一律视为不可用——工具参数在协议上恒为对象，`null` / 数组 / 数字与语法坏掉同样没法用。`args_value_quality(&Value)` 是同一判定的已解析值入口，`Value::String` 会被**重新按文本分类**：chat codec 只在上游 `arguments` 解析失败（或上游本就发了个 JSON 字符串）时才退化成 `Value::String`，两种情形都属畸形。

`ToolArgWatch` 按 tool-call `index` 累积证据：

- `ToolCallStart` → `observe_tool_call_start`：登记该 index。**登记本身就是证据**——「起了个头却一个 delta 都没有」正是要抓的缺陷，故未收到任何 delta 的调用判定为 `Blank`，而不是「不存在」。
- `ToolCallDelta` → `observe_arguments`：拼接片段。**空 delta 也要喂**（否则 `arguments: ""` 这种最典型的样本会被跳过）；未登记的 index 隐式登记。
- `quality()` 取所有调用的**最差**档（`Ord` 序即严重度序），无调用时 `Ok`。
- `observe` 与 `observe_tool_call_start` / `observe_arguments` 是同一套累积逻辑的两个入口：前者吃 `IrStreamEvent`（已解码车道），后者吃裸片段（字节透传车道的窥探结果）。`raw_api_agrees_with_ir_api` 钉住两者等价。

### 告警点：流式每流一次、非流式每响应一次

| 场景 | 位置 | 触发条件 |
|---|---|---|
| 流式 | `dispatch/stream.rs::finish_stream_terminal` | `tool_calls > 0 && quality != Ok` |
| 非流式 | `dispatch/chat/mod.rs` 的 `ChatAttempt::NonStream` 臂（成功出口，egress 之前） | `tool_calls > 0 && quality != Ok` |

流式告警放在 `finish_stream_terminal` 里、**claim 终点之后、终止帧投递之前**：此时流已判 `done`，聚合值也已是最终值（不是某个中间 delta 的视图）。日志字段固定为 `request_id` / `provider` / `model` / `real_model` / `tool_calls` / `reason`（`?Debug` 打印 `ToolArgsQuality`），消息体分别是 `upstream streamed tool calls with blank/invalid arguments` 与 `upstream returned tool calls with blank/invalid arguments`——两条消息共享 `blank/invalid arguments` 子串，测试据此计数。

**`TerminalGuard::drop` 有意不发这条告警**：drop 路径代表客户端提前断连，若在那里报 `Blank`，一个参数其实完整的流只因为客户端在第一个 delta 后就关连接就会被误报成空白参数——假阳性比漏报更伤害这条信号的可信度。因此告警只挂在正常终点上；被取消的流没有这条日志，是设计而非遗漏。

### 盲区补洞：`peek_chat_raw_frame`（只看不改）

Chat ingress × Chat upstream 的每个事件都是 `IrStreamEvent::Raw(bytes)`，`watch.observe` 对其无感。`watch_tool_args(stream, peek_raw_chat_frames)` 因此在 fold 里加一条旁路：`Raw` 帧交给 `peek_chat_raw_frame`，非 `Raw` 事件交给 `ToolArgWatch::observe`。

窥探复用已有的 `sse_data_payload`（剥 `data:` / `data:` 两种拼写、`\n` / `\r\n` / `\n\n` 尾、多行帧取首个 `data:` 行）再做一次 `serde_json::from_slice`，读 `choices[].delta.tool_calls[].function.arguments`，逐条喂 `observe_tool_call_start` / `observe_arguments`：

- **observation-only**：`ev` 原样返回，客户端收到的字节一个也不变（Chat→Chat 字节恒等不变式）。
- `delta.tool_calls` 优先，`message.tool_calls` 只作非流式形状的兜底；**二者互斥**——同一帧若同时读两边，同一份 `arguments` 会被拼两次（`{"a":1}{"a":1}`），产生假阳性 `NotJson`。
- 缺 `index` 时以数组下标兜底（非流式形状没有 `index`）；`arguments` 缺失 / 非字符串按 `""` 计入，仍然登记该调用。
- 不可解析的帧（`data: [DONE]`、`: keep-alive` 注释、截断数据）静默跳过——这是 warn-only 信号，绝不允许把一帧透传数据变成错误。
- 解析器只有一份，落点有两个：`ToolArgSink` 让单元测试喂裸 `ToolArgWatch`、让分发 fold 喂 `ToolArgWatchHandle`（其内层 watch 对 `dispatch::stream` 私有）。`ToolArgWatchHandle` 侧把窥探结果**重放成事件**再 `observe`，客户端仍收到原始 `Raw` 字节。

**门控在 `!needs_ir_decode`**：已解码车道上的 `Raw` 只是一个剩余副本，其载荷早已由 `ToolCallStart` / `ToolCallDelta` 计入；若同时窥探，同一段 `arguments` 会被拼两次并产出假阳性 `NotJson`（`peek_raw_frame_is_gated_off_on_ir_lanes` 用同一组事件跑 `false` / `true` 两种取值，把「省掉的是什么」变成可执行断言）。

**有意留盲**：两条同协议 native 透传车道 Anthropic→Anthropic、Responses→Responses 不过窥探。那里的帧是上游自己的事件形状，没有 `choices[].delta.tool_calls`，窥探也读不到任何东西；更重要的是这类车道的契约是「客户端收到的就是上游的帧」，网关不仲裁其载荷质量。

### 解码归属：有状态 `StreamFrameDecoder` + 显式 `use_responses`

```rust
type StreamFrameDecoder = Box<dyn FnMut(&[u8]) -> Vec<IrStreamEvent> + Send>;
```

从裸 `fn` 指针改为 boxed `FnMut`，因为 Responses 解码器已变为有状态（`output_item.done` 必须对照同一 item 已投递的 delta 决定是否供载荷，见 [providers 0010](../../providers/changes/0010-responses-stream-item-payload-fidelity.md)）。选择器补上显式 `use_responses` 分支：`use_anthropic` → Anthropic 解码器、`use_responses` → `ResponsesCodec::stream_decoder()`、其余 → Chat 解码器。

这修掉的是**归属**问题，不是某个分支的局部 bug：Anthropic ingress × Responses upstream 此前落到 Chat 解码器，Responses 帧因无 `choices` 全数退化为 `Raw`，Anthropic 编码器逐字转发，于是「解码」形同虚设。`sag-router/tests/protocol_matrix.rs` 的 `anthropic_x_responses_native_stream` 此前**一直靠这个错误的选择在通过**——它的 mock 发的是 `object: "chat.completion.chunk"` 帧却声明 Responses 上游，Chat 解码器恰好能解；fixture 已改正为真正的 Responses 形状 SSE，断言改为「合成 `message_start`、delta 恰好一次成为 `text_delta`、恰好一个 `message_stop`、不得出现 `chat.completion.chunk`」。

### Chat ingress × Responses upstream：解码重编码，不再泄流

`needs_ir_decode` 新增 `(is_chat_ingress && use_responses)`。此前这条车道落进 else 分支的 `Raw` 直通，Chat 协议客户端会收到 `response.output_text.delta` 这种没有 `choices` 的帧——对 OpenAI SDK / Codex CLI 而言等于坏帧。现在这些帧解码为 IR 后按 Chat 协议重编码，桥接 #1 再在其上补恰好一个 `data: [DONE]`。

未改变的：Chat→Chat 与 Responses→Responses 两条同协议透传（字节恒等不变式完好）；Chat ingress × **Anthropic** upstream 的解码归属与 `[DONE]` 桥接。

### Raw 帧过滤：`is_responses_event_frame`

Responses 解码器把「无法表达为 IR」的帧作为 `Raw` 交出来是为了保住同协议透传——但在跨协议车道上，这些帧逐字转发就是协议泄漏。`is_responses_event_frame(bytes)` 用同一个 `sse_data_payload` 剥出 payload，判定 `type` 是否以 `response.` 开头；已解码车道上，命中的 `Raw` 在到达编码器之前被丢弃。

**契约**：过滤只作用于 `needs_ir_decode == true` 的车道，且只针对 Responses 上游——凡是「Responses 上游 + 已解码」的组合（Chat ingress、Anthropic ingress 皆属此族），Responses 形状的 `Raw` 一律不得流向非 Responses 出口；`needs_ir_decode == false` 的同协议车道**永不过滤、永不改写**，那里的 `Raw` 就是上游原字节，是字节恒等不变式的一部分。过滤只认 `response.` 前缀，故 `data: [DONE]`、`: keep-alive`、非 JSON 帧与 Chat 帧不受影响。

**已知边界**：`response.completed` 现在解码为 `Finish` + `Usage` + `Raw`——末尾那个 `Raw` 是为同协议透传保留的权威终止帧，因此它仍正是本过滤的主要拦截对象：过滤改的是**谁能看到这个帧**（非 Responses 出口看不到），不是帧的类别，也不是终止 / 错误帧的分类语义。`response.failed` 同理仍发 `Error` + `Raw`：`Raw` 被本过滤拦下，`Error` 则按新契约渲染成客户端可见的错误帧（见下一节）。

### 有意不改：三个空响应谓词仍不感知参数质量

`is_empty_response_data` / `choice_has_message_content` / `ir_item_has_content` **行为零改动**：只要 `tool_calls` 数组非空就算内容。因此空白参数响应仍然记录 `TargetStatus::Success` + `failure_reason: None`，**不触发** provider fallback、不消耗重试、不影响粘性 pin，`call_logs` 依旧是 200。

三处只补了文档注释：指明质量判定的唯一来源是 `args_quality`，并明确「把空响应判定改成质量感知」是一次**有意的行为变更**，必须挂在 `backoff.retry_on_empty_response` 之后单独决策——把空白参数重新归类为「空响应」会改变重试次数、降级路径与全量统计口径，属于另一个变更而不是这个修复的副作用。`empty.rs` 用 4 例把这条边界钉死（见「测试」）。

### 流内错误与 `finish_reason`：客户端终于能分辨「失败」和「短响应」

这一小节是同一轮里剩下的三个客户端可见行为变更，与前面的参数告警无因果关系，但同样属于「网关知道、客户端不知道」：

**（a）`response.completed` 解码出 `Finish`**（`sag-providers/src/codec/responses.rs`）。此前该事件只发 `Raw` + `Usage`，理由是同协议透传车道需要原帧。但跨协议出口（尤其是 Chat ingress）拿到的是重编码结果，`Raw` 已被 `is_responses_event_frame` 拦下——于是 Chat 客户端只收内容 delta 与 `[DONE]`，一个 `finish_reason` 也没有，OpenAI SDK 读作截断。现在按 `response.status` + `incomplete_details.reason` 走与**非流式 decode 同一个** `status_to_finish`，无 `/response/status` 时按事件名（`completed`）回退；`response.output[]` 里出现 `type: "function_call"` 则把 `FunctionCall` 折叠为 `ToolCalls`（Chat / Anthropic 编码器渲染的 IR 词条），不另建第二张映射表。`Usage` 与 `Raw` 仍随后发出，同协议车道的字节恒等不变。

**（b）Chat ingress 的终止安全网**（`sag-dispatch/src/dispatch/chat/mod.rs` 的 `EndSynthesis::ChatFinishReason`，桥接 #4）。即便上游连 `Finish` 都不给、直接结束，Chat 客户端也必须拿到恰好一个 `finish_reason` 且位于 `data: [DONE]` 之前。三个约束：只用 `Some(Stop)`（Chat 没有“协议要求的占位 reason”，`None` 会渲染成没有 `finish_reason` 的 chunk）；只合成 `Finish`、不合成终止符（`End` 继续往下流，交给桥接 #1）；由于 #1 在更早一层已把首个 `End` 改写成 `Raw(data: [DONE])`，#4 还额外认这个 `Raw` 帧作为终止载体。门控在 `is_chat_ingress && needs_ir_decode`：Chat→Chat 字节恒等透传从不解码，上游自己的 `finish_reason` 原样透传，不得被劫持。

**（c）流内错误对客户端可见**。`dispatch/stream.rs` 的流循环过去把 `IrStreamEvent::Error` 单独分支消费掉（`record_stream_error` 后 `continue`），在 HTTP 200 已提交的前提下，客户端看到的是一条“内容停住、正常结束”的流。现在 `Error` 记完 analytics 后**按车道分叉**：翻译车道（`needs_ir_decode == true`）**落回与 `Raw` / 结构化事件相同的编码路径**——记在前、编码在后，所以 attempt 状态 / `failure_reason` 口径与字节外发前的错误标志都不变；直通车道（`needs_ir_decode == false`）只记 analytics 就丢掉该事件，一个合成帧也不发（上游自己的错误字节已经在流里作为 `Raw`，再合成一帧就是双错误帧）。同一流上重复的 `Error`（provider 的 side-channel chunk 加上解码出的错误帧）在翻译车道只外发一帧。翻译车道的两个编码器契约按上游官方形状：

| 出口（翻译车道） | 帧 | 终止符 |
|---|---|---|
| Chat（`chat.rs`） | 裸 `data: {"error":{"message":…,"type":"server_error","code":"STREAM_ERROR","param":null}}` | 不发 `data: [DONE]` |
| Anthropic（`anthropic.rs`） | `event: error` + `data: {"type":"error","error":{"type":"api_error","message":…}}` | 不发 `message_delta` / `message_stop`（无 `stop_reason`） |
| Responses（`responses.rs`） | 本轮不改：`Error` 仍不产生帧 | — |

IR 的 `Error` 事件只携带 message（无错误类别），所以 Anthropic 的 `error.type` 固定为通用的 `api_error`，Chat 的 `type` 为 `server_error`、`code` 保留网关的 `STREAM_ERROR` 标记而 `param` 显式为 `null`。终止抑制落在两个地方且只服务于翻译车道：Chat 车道是 `chat/mod.rs` 里两个 per-stream `errored` 标志（桥接 #1 抑制 `[DONE]`、桥接 #4 抑制合成的 `finish_reason`），Anthropic 车道是 `StreamEncodeState.errored`（`Finish` / `End` 返回空 vec）；直通车道两者都不会被置位——流循环根本不把 `Error` 交给编码器。`End` 仍会流到分发循环完成终点记账，只是编码为空。

**有意不变**：HTTP 状态恒为 200（已提交，无法退回 502）；`call_logs` 的 `stream_error` 口径、`TargetStatus` / `failure_reason`、`Usage` 合并全部不变；Chat→Chat 字节恒等透传完全不变：零合成帧，上游自己的错误帧与它后面的 `data: [DONE]` 都作为 `Raw` 原样转发；翻译车道的合成帧只服务于 provider 层失败（如 chunk 空闲超时与读错误）——那种场合旧实现什么都不会发。

## 测试

`ToolArgsQuality` / `ToolArgWatch`（`sag-core/src/ir/tool_args.rs` 内联，12 例）：`{}` 与带键对象为 `Ok`、空串与纯空白为 `Blank`、`{bad` / `null` / `"x"` / `[1]` / `42` 为 `NotJson`（`blank_arguments_are_blank_and_empty_object_is_ok` / `malformed_arguments_are_not_json`）、`args_value_quality` 对 `Value::String` 重新分类（`value_quality_handles_string_and_object`）、起了头无 delta 判 `Blank`（`watch_flags_started_call_with_no_deltas`）、跨 delta 拼接后判 `Ok`（`watch_accumulates_deltas`）、多调用取最差档（`watch_reports_worst_quality` / `multiple_indices_accumulate_independently`）、裸片段 API 隐式登记（`observe_arguments_without_start_registers_call`）、重复 `start` 不清空已累积参数（`repeated_start_does_not_reset_accumulated_arguments`）、空 delta 只留 `Blank`（`empty_deltas_only_yield_blank`）、裸片段 API 与 `IrStreamEvent` API 结论一致（`raw_api_agrees_with_ir_api`）、无工具调用时 `Ok`（`watch_with_no_tool_calls_is_ok`）。

窥探单元（`sag-dispatch/src/dispatch/chat/mod.rs` 的 `peek_tests`，5 例）：`peek_raw_frame_accumulates_tool_call_arguments`（`data: ` 与 `data:` 两种拼写都能剥）、`peek_raw_frame_blank_arguments_yield_blank`、`peek_raw_frame_ignores_done_and_non_json`（`[DONE]` / 非 JSON / keep-alive / 空帧都不贡献调用）、`peek_raw_frame_handles_missing_index`（两个无 `index` 条目不合并成一个）、`peek_raw_frame_is_gated_off_on_ir_lanes`（同一组事件下 `peek=false` → `(1, Ok)`、`peek=true` → `(1, NotJson)`，即门控省掉的正是假阳性）。

分发测试（`sag-dispatch/src/dispatch/chat/tests.rs`，新增内联 `CapturedEvents` 订阅器 + `FieldVisitor` 收集 `tracing` 字段）：

- `stream_with_blank_tool_arguments_still_succeeds_but_flags_quality`（Anthropic ingress × Chat upstream，走解码车道）：整条流恰好一条 `blank/invalid arguments` 告警、带 `tool_calls=1` 与 `reason=Blank`；`request_results.final_status = success`、`failure_reason` NULL、`target_attempts.status = success`、上游只被调一次——**告警不影响判定**。
- `non_stream_string_tool_arguments_are_flagged`：`"arguments": ""` 经 codec 退化 `Value::String` → `args_value_quality` 判 `Blank`；HTTP egress 与原始 body 逐字节相同；恰好一条非流式告警；目标仍 `success`、无重试。
- `chat_ingress_chat_upstream_blank_tool_arguments_keep_byte_fidelity`：字节透传车道的三帧黄金样本与响应体逐字节相等（窥探不重写任何字节），同时按 `request_id` 过滤后恰好一条 `reason=Blank` / `tool_calls=1` 的告警——盲区确实被补上；`stream_calls == 1`，空白参数不触发空响应重试。
- `chat_ingress_responses_upstream_reencodes_as_chat_chunks`：Chat ingress × Responses upstream 的响应体必须是 `object: "chat.completion.chunk"` + `choices[].delta.content`，不得出现 `"type":"response.` / `response.output_text.delta`，`data: [DONE]` 恰好一次并位于末尾，且 `finish_reason: stop` 恰好一次。
- `anthropic_ingress_responses_upstream_decodes_as_responses`：Anthropic ingress × Responses upstream 的响应体不得含 `response.output_text.delta`，`"text":"hi"` 恰好一次且为 `content_block_delta`，`message_stop` 恰好一次——钉住解码器选择修好后 Anthropic 出口的完整生命周期。

空响应边界（`sag-dispatch/src/dispatch/chat/empty.rs`，4 例钉住「故意不改」）：`blank_arguments_tool_call_still_counts_as_content`（`delta.tool_calls` 站点）、`blank_arguments_tool_call_counts_as_content_in_message`（非流式 `message.tool_calls` 站点）、`ir_tool_call_item_always_counts_as_content`（IR 站点，唯一的 item 是空白参数工具调用仍判非空）、`args_quality_is_the_documented_quality_source`（同一个 `Value::String("")` 被判 `Blank`，而 `ir_item_has_content` 仍返回 `true`——差距是文档化的，不是遗漏）。

入口协议矩阵（`sag-router/tests/protocol_matrix.rs`，2 例修正 + 2 例新增）：`anthropic_x_responses_native_stream` 的 mock 由 Chat chunk 换成真正的 Responses SSE（旧 fixture 恰好掩盖了错误解码器选择）；`chat_x_responses_native_stream` 的断言从「客户端能看见 `response.created`」翻转为「必须是 Chat chunk、必须带 `choices`、任何 `response.` 事件名都不得出现」，并钉「`"finish_reason":"stop"` 恰好一次」；`chat_ingress_responses_failed_emits_error_frame` 与 `anthropic_ingress_responses_failed_emits_error_event` 各跑一条“先发内容、再 `response.failed`”的上游：前者要求**恰好一帧** `error` 对象（`message` 原文、`type=server_error`、`code` / `param` 键存在）、`finish_reason` 与 `[DONE]` 均不得出现且 HTTP 仍是 200，后者要求 `event: error` + `{"type":"error",…}` 且不出现 `message_stop` / `stop_reason`。

`finish_reason` 与错误帧（同轮新增）：

- `sag-providers/src/codec/responses.rs` — `response_completed_emits_stop_finish_reason`（`Finish(Stop)` 后紧跟 `Usage`）、`response_completed_with_function_call_emits_tool_calls`（`output[]` 的 `function_call` → `ToolCalls`）、`response_completed_without_status_falls_back_to_stop`（缺 `/response/status` 时按事件名回退）。
- `sag-dispatch/src/dispatch/chat/tests.rs` — `chat_ingress_without_upstream_finish_still_emits_finish_reason`：上游（Anthropic 帧）只发 `message_start` / `content_block_start` / 一个 `content_block_delta` 就结束，既无 `message_delta` 也无 `message_stop`，响应体必须恰好一个 `finish_reason: stop` 且它在 `data: [DONE]` **之前**。
- `sag-dispatch/src/dispatch/stream.rs` — 既有错误路径测试的断言已从“错误 chunk 被消费后流挂起”改为“多出一帧客户端可见的错误帧”：`mid_stream_error_sets_call_log_status_502`、`upstream_error_then_drop_still_502`（`drain_until_pending` 由 1 变 2）、`response_drop_after_upstream_error_emits_stream_error_after_commit`、`drop_guard_after_in_stream_error_writes_call_logs_row_with_message`、`backpressure_pending_body_drop_error_terminal_still_lands_once`、`privacy_tail_flushes_when_upstream_errors_out`（隐私尾与错误帧同批下发）。
- `sag-dispatch/src/dispatch/chat/tests.rs` — `m4_stream_body_bytes_match_upstream_data_frames` 仍钉字节恒等；Chat 编码器的错误帧形状另由内联断言钉住（裸 `data: ` 帧、`type=server_error`、`code=STREAM_ERROR`、存在 `param` 键、不带 `event: ` 前缀）。
