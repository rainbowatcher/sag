---
created: 2026-06-03
updated: 2026-11-09
---

# 调度 — 规格

## 关键文件

- [sag-dispatch/src/dispatch/chat/](../../../sag-dispatch/src/dispatch/chat/) — 聊天补全分发模块（主调度、尝试结果与事实构造、空响应校验、translated 终点帧桥接、raw 帧工具参数窥探与 Responses 帧过滤）
- [sag-core/src/ir/tool_args.rs](../../../sag-core/src/ir/tool_args.rs) — 工具调用参数质量谓词（`ToolArgsQuality` / `args_quality` / `args_value_quality` / `ToolArgWatch`），`sag-dispatch` 的唯一质量来源
- [sag-dispatch/src/dispatch/image.rs](../../../sag-dispatch/src/dispatch/image.rs) — 图片生成分发
- [sag-dispatch/src/dispatch/systemone.rs](../../../sag-dispatch/src/dispatch/systemone.rs) — SystemOne 决策分发（`dispatch_systemone` 镜像 `dispatch_image` 的尝试 / 降级骨架，`build_systemone_body` 从 carrier `extra` 重建 wire body）
- [sag-dispatch/src/dispatch/stream.rs](../../../sag-dispatch/src/dispatch/stream.rs) — SSE 流式处理（`StreamAnalytics` 共享状态、`TerminalGuard` 三路终点分类、`StreamState._in_flight` 持 per-route 在途 guard）
- [sag-core/src/protocol.rs](../../../sag-core/src/protocol.rs) — `is_client_terminal_frame`：三协议终点帧的字节级判定
- [sag-dispatch/src/dispatch/mod.rs](../../../sag-dispatch/src/dispatch/mod.rs) — `failure_log`、`success_log` 等日志构造函数
- [sag-dispatch/src/dispatch/retry.rs](../../../sag-dispatch/src/dispatch/retry.rs) — 重试退避逻辑 + `is_retryable_error` 错误分类判定
- [sag-dispatch/src/dispatch/resolve.rs](../../../sag-dispatch/src/dispatch/resolve.rs) — 路由匹配 + 目标选择（含 `filter_systemone_targets` 的 `apis.systemone` presence 闸门，以及「route 选中后、目标过滤前」的 pipeline × ingress 守卫）
- [sag-dispatch/src/order.rs](../../../sag-dispatch/src/order.rs) — 目标排序

## 核心设计

### 分发数据流

```mermaid
flowchart TD
    A[Request] --> B[resolve_route]
    B --> C[order_targets]
    C --> D{try next target}
    D --> E{circuit breaker open?}
    E -->|yes| D
    E -->|no| F{provider enabled?}
    F -->|no| D
    F -->|yes| G[resolve_timeout]
    G --> H[resolve_retries]
    H --> I{stream?}
    I -->|yes| J[chat_stream with chunk_timeout]
    I -->|no| K[chat with timeout_secs]
    J --> L{success?}
    K --> L
    L -->|yes| M[return response]
    L -->|retryable (408/429/5xx/timeout/403-quota)| O{retries left?}
    O -->|yes| P[backoff sleep]
    P --> K
    O -->|no| Q[open circuit breaker]
    Q --> D
    L -->|client 4xx(1)| R1[return error to client]
    L -->|provider-persistent 4xx(2)| N[stop retries → failover]
    N --> D
    D -->|all exhausted| R[502 aggregated errors]
```

(1) 400/413/422/未知 4xx（客户端归因）：直接返客户端，不 failover、不挪 pin、不开熔断。
(2) 401/404/403-非quota（provider 持续性）：failover + 挪 pin、不开熔断。403-quota 归入可重试分支。

## 关键决策

### 同步重试 vs 异步重试

重试在单个请求处理中同步执行（循环内 await），而非 spawn 新任务。

**Trade-off**:

- ✅ 实现简单，请求上下文天然传递（不需要为每个重试创建新的 span）
- ✅ 自然利用 tokio 的协作调度——sleep 时让出 CPU
- ❌ 长时间重试会占用一个 tokio task。但 LLM API 的典型延迟是秒级，task 数量本就不高

### SSE 流式：channel 模式

使用 `tokio::sync::mpsc::unbounded_channel()` 连接 provider 流和 axum streaming body：

```rust
let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
// spawn provider call → tx.send(chunk)
// return axum streaming body from rx
```

**Trade-off**:

- ✅ 解耦 provider（生产者）和 axum（消费者），任一端可以独立处理
- ✅ unbounded channel 避免 provider 生产速度受限于消费速度——SSE chunk 通常很小
- ❌ 错误无法通过 `?` 传播，需要通过 channel 发送错误 chunk
- ❌ unbounded 可能内存无限增长（但 SSE chunk 很小 + 通常及时消费）

### chunk_timeout 实现

```rust
tokio::time::timeout(Duration::from_secs(n), stream.next()).await
```

每次等待下一个 chunk 时加上 timeout。超时后 provider 发出 `StreamChunk::Error`（不会被转成 `Done`）并停止读取；分发侧把它映射为 `IrStreamEvent::Error`，**先**记 analytics（CallLog 记 `502` / `error_category = stream_error`），**再**按车道决定外发：翻译车道交给出口编码器合成一帧协议正确的错误帧——Chat 出口是裸 `data: {"error":{…}}`，Anthropic 出口是 `event: error` + `{"type":"error",…}`，Responses 出口是 `data: {"type":"error",…}`，三者后面都不再跟终止符；直通车道（如 Chat→Chat）的**入站上游错误帧**不合成任何帧（上游自己的字节原样转发），但**网关本地产生**的中断（chunk 空闲超时、读错误）没有上游字节可转发，同样在终点合成恰好一帧 ingress 协议错误帧后关闭 body（详见「上游流内错误的客户端可见错误帧」一节）。旧路径的 `StreamChunk::to_sse_bytes` 固定信封（`{"error":{"code":"STREAM_ERROR","message":…}}`，见 `sag-core/src/types.rs`）不再用于流式出口；专用 `STREAM_CHUNK_TIMEOUT` 错误码 / 504 状态尚未实现，属后续独立任务。

**Trade-off**: timeout 作用于单个 chunk 的等待，不是整个请求的总时长。这意味着即使单个 chunk 都很快，长时间的静默期（silence）也会触发。这正是想要的行为——LLM 推理中长时间无输出通常意味着卡住了。

**成功流无总 deadline**：共享 reqwest client 不设置 `Client::timeout`，`timeout_secs` 对成功流式请求是 no-op（只覆盖非流式完整往返）。流式响应头到达由 `tokio::time::timeout` 按 `ttfb_timeout_secs` 约束（TTFB），之后 body 只受 chunk 空闲超时约束——长寿命健康流不会被总时长截断。非 2xx 流式响应的 error body 读取同样用 `ttfb_timeout_secs` 的局部 timeout 兜底，避免上游挂着不送 body 导致无界等待。

### Retry 和 Backoff 的实现位置

重试逻辑在 dispatch 层（不在 provider 层），每个 target 的 retry 独立计数。

**Trade-off**:

- ✅ Provider 不需要关心重试，保持简单
- ✅ Dispatch 可以看到完整的 targets 列表，做跨 target 的 fallthrough
- ❌ Provider 内部的错误（如网络）和业务错误（如 4xx）在 dispatch 层区分，增加了判断逻辑

### 错误聚合

当所有 targets 耗尽时，返回一个聚合的 502 错误，附带所有 target 的失败原因。不返回最后一个错误（那会误导）。

**Trade-off**: 返回所有错误信息有助于排障（知道哪些供应商都失败了），但响应体可能较大。在实际中，最多几个 targets，错误信息主要是状态码和简短描述。

### 终点帧竞态与断连分类

客户端收到终点帧后关连接是合法的，但上游 `End` 事件可能因 FIN 尾延迟尚未入队，hyper 随即 drop body 并触发 `TerminalGuard::Drop`。若仅按「是否观察到 mid-stream `Error`」二分，这类正常收尾会被误记为 `client_cancelled` / 499。

实现分两层：

1. **判定层**（`sag_core::is_client_terminal_frame`）：对**编码后**的 SSE 帧字节做三协议形态并集匹配（Chat `data: [DONE]`、Anthropic `"type":"message_stop"`、Responses `"type":"response.completed"` / `"response.incomplete"` / `"type":"response.cancelled"`，兼 `event:` 行变体）。Responses 的三个流终点事件类型全部覆盖（`response.failed` 不算终点：它在 analytics 侧仍走 502 `stream_error_after_commit`，客户端侧在翻译车道则渲染成协议错误帧，见「上游流内错误的客户端可见错误帧」）。行形态标记逐行锚定前缀（防内容 delta 伪造），`"type":"..."` 引号形用子串匹配（字符串值内必被转义）。不按 ingress 分派——跨协议组合下帧形态由 upstream / lane 决定。
2. **状态层**（`StreamAnalytics`）：`terminal_frame_forwarded: AtomicBool` 在 yield **之前**置位（yield 后置位会输给 hyper 的 drop），并与 `TerminalGuard` 共享 `usage` / `first_byte_at` / `upstream_start`，使 drop-as-success 行能写出完整 usage、`ttft_ms`、`upstream_duration_ms`。

`TerminalGuard::Drop` 三路分类：error → 502 `stream_error_after_commit`（错误优先，`response.failed` 不升级）；无 error 但标志已置位 → `Success` 200；否则 → `client_cancelled` 499。

**Trade-off**：这是**统计口径变更**（历史误分类的 499 会被正确地计为成功），dashboard 曲线会有一次性台阶；换来的是「客户端实际体验」与「网关记录」一致，且不再产生全 NULL 的成功行。

### translated 终点帧桥接

translated 路径（ingress 协议 ≠ upstream 协议）下，upstream 不会发出 ingress 协议的终止帧，而 ingress 编码器的 `End => <终止帧>` 分支因 `End` 被内部消费而不执行，客户端可能永远收不到协议终点。四条桥接按「谁缺什么」分工：

| # | 组合 | 合成的帧 | 位置 |
|---|---|---|---|
| 1 | Chat ingress + Anthropic / Responses upstream | `data: [DONE]\n\n` | empty-peek **之前** |
| 2 | Anthropic ingress + 非 Anthropic upstream | `data: {"type":"message_stop"}\n\n` | empty-peek **之后** |
| 3 | Responses ingress + 非 Responses upstream | `response.completed` / `response.incomplete`（由 `Finish` 渲染，`length` / `content_filter` 取后者） | empty-peek **之后** |
| 4 | Chat ingress + IR 解码车道（翻译车道；Anthropic / Responses upstream） | `finish_reason: stop` chunk（上游全程未给 `finish_reason` 时） | empty-peek **之后** |

`#2` / `#3` 共用 `apply_end_synthesis_bridge`（`EndSynthesis` 枚举 + `flat_map`，状态为 `finish_seen` / `end_sent`）：首个 `End` 若此前未见 `Finish`（即上游未给 `finish_reason`，不合规），先合成 `Finish { finish_reason: None }` 再发终止帧。编码器据此渲染：Anthropic → `content_block_stop`（闭合悬挂的 content block）+ `message_delta`（协议要求的 `stop_reason`，`None` 映射 `end_turn`）+ `message_stop`；Responses → `response.completed`（`status: completed`）。合规上游（已有 `Finish`）不重复合成，故不会多出第二条 `message_delta` 或第二个 `response.completed`。native 路径（`use_anthropic` / `use_responses`）不启用任何桥接，字节恒等。

`#4` 是 `#2` / `#3` 的 Chat 对偶，同一 `apply_end_synthesis_bridge` 的第四个 `EndSynthesis` 变体：Chat 的终止信号是 `finish_reason` chunk 而不是终止符，所以它只合成 `Finish { finish_reason: Some(Stop) }`，**不**合成终止符——`End` 继续往下流，由桥接 `#1` 渲染成 `data: [DONE]`。之所以必须跳过 `#2` / `#3` 的 `None`：Chat 没有“协议要求的占位 reason”，一段有内容、正常结束、但上游漏了 `finish_reason` 的流应报 `stop`，而 `None` 会渲染成没有 `finish_reason` 的 chunk（客户端仍然读成截断）。由于 `#1` 在更早一层已把首个 `End` 替换成 `Raw(data: [DONE])`，`#4` 还额外认这个 `Raw` 帧作为终止载体（同样只在未见 `Finish` 时补），且不得影响 empty-peek：`data: [DONE]` 在那里已被判为 `RawVerdict::Empty`。

**错误优先于一切终止帧（仅翻译车道）**：翻译车道上任一层见到 `IrStreamEvent::Error` 后，本流后续的合成都被抑制（`#1` 的 `data: [DONE]`、`#4` 的 `finish_reason`、Anthropic 编码器的 `message_delta` / `message_stop`）。`errored` 标志是**每流**作用域（闭包内部状态），同一条路由上的健康流不受影响；`End` 仍会流入分发循环以完成终点记账，只是不再产出客户端可见字节。直通车道不在这条规则里：那里没有任何桥接，也没有合成帧，上游错误帧之后的终止符（含 `data: [DONE]`）照旧原样转发。

桥接 #1 只补 Chat 协议的终止符，不改变帧形态归属：Chat ingress + **Anthropic** upstream 的响应帧仍为上游原样 Raw 透传（字节恒等）；Chat ingress + **Responses** upstream 的帧则先解码为 IR、再重编码为 Chat chunk（`needs_ir_decode` 含该组合），`data: [DONE]` 只是加在重编码后的 Chat 帧之上。字节恒等因此**只对同构车道（chat→chat、native→native）成立**，跨协议车道不在此列。

**次序不变式**：`#1` 必须在 empty-peek **之前**——`classify_raw_frame` 把 `data: [DONE]` 判为 `RawVerdict::Empty`，因此真空上游仍能报 `EmptyResponse` 并走 `retry_on_empty`；`#2` / `#3` / `#4` 必须在 peek **之后**——它们合成的帧（`message_stop` / Responses 终点帧 `response.completed` 或 `response.incomplete` / Chat `finish_reason` chunk）没有 Empty 特判，前置于 peek 会被 `classify_raw_frame` 读作 `Content`，使真空上游不再重试。该次序写在 `chat/mod.rs` 的注释里，改动桥接位置时需同时复核。

### 工具调用参数质量告警（warn-only）

上游确实会发出 `arguments` 为空、纯空白、或根本不是 JSON 对象的工具调用（`""`、`null`、`[1]`、`{bad`）。这类响应在 chat IR 里与非空的**同形**——`tool_calls` 数组非空即算内容——因此既不重试也不降级，`call_logs` 是一行干净的 200，客户端拿到 `get_weather({})` 也无任何预警。本轮只补信号，不改判定。

**谓词层**（`sag-core/src/ir/tool_args.rs`，纯逻辑、无 I/O 无日志）：`ToolArgsQuality` 三档 `Ok` < `Blank` < `NotJson`，`Ord` 即严重度序。`{}` 是合法参数；空串 / 纯空白是「缺失」；不可解析或非对象（含 `null` / 数组 / 数字 / JSON 字符串）一律不可用——工具参数在协议上恒为对象。`args_value_quality(&Value)` 是已解析值入口，其中 `Value::String` 会被**重新按文本分类**（chat codec 只在上游 `arguments` 解析失败或本就发了个字符串时才退化到 `Value::String`）。

**累积层**：`ToolArgWatch` 按 tool-call `index` 累积证据，`quality()` 取所有调用的最差档（无调用时 `Ok`）。两个 API 是同一套累积逻辑的两个入口：`observe(&IrStreamEvent)`（已解码车道）与 `observe_tool_call_start` / `observe_arguments`（裸片段，字节透传车道的窥探结果）。**登记本身就是证据**——起了头却一个 delta 都没收到的调用判 `Blank`（正是要抓的缺陷），而非「不存在」；**空 delta 也要喂**，否则 `arguments: ""` 这个最典型的样本会被跳过。

**发射层**（仅 `sag-dispatch`）：

| 场景 | 位置 | 时机 |
|---|---|---|
| 流式 | `dispatch/stream.rs::finish_stream_terminal` | claim 终点之后、终止帧投递之前，每流最多一条 |
| 非流式 | `dispatch/chat/mod.rs` 的 `ChatAttempt::NonStream` 成功出口 | egress 之前，每响应最多一条 |

两处条件同为 `tool_calls > 0 && quality != ToolArgsQuality::Ok`。流的聚合值在 `StreamState.tool_args: ToolArgWatchHandle`（`Arc<Mutex<ToolArgWatch>>`）里跨整个生命周期累积，分发侧把每个交给客户端的 `IrStreamEvent` 在 fold 里喂进同一个 watch，终点只读一次。

**`TerminalGuard::Drop` 有意不发**：drop 路径代表客户端提前断连；在那里报 `Blank` 会让「参数完整的流被客户端在第一个 delta 后关掉」也变成假阳性——假阳性比漏报更损害信号可信度。故被取消的流没有这条日志，是设计而非遗漏。

**有意留盲**：两条同协议 native 透传车道（Anthropic→Anthropic、Responses→Responses）不观察。它们的帧是上游自己的事件形状，没有 `choices[].delta.tool_calls`，且契约是「客户端收到的就是上游的帧」，网关不仲裁其载荷质量。

### 流式解码归属与 Responses 帧过滤

帧的解码归属由 **upstream 协议**选定，不由 ingress 单独决定：

```rust
type StreamFrameDecoder = Box<dyn FnMut(&[u8]) -> Vec<IrStreamEvent> + Send>;
```

boxed `FnMut`（而非常规 `fn` 指针）是必需的：Responses 解码器已变为有状态（`output_item.done` 需对照同一 item 已投递的 delta 决定是否供载荷，见 [providers 0010](../providers/changes/0010-responses-stream-item-payload-fidelity.md)）。选择器三路：`use_anthropic` → Anthropic 解码器、`use_responses` → `ResponsesCodec::stream_decoder()`、其余 → Chat 解码器。

修掉的是**归属**缺陷，不是某个分支的局部 bug：此前只判 `use_anthropic`、其余一律落 Chat 解码器，于是 Anthropic ingress × Responses upstream 的帧因无 `choices` 全数退化为 `Raw`，Anthropic 编码器再逐字转发——「解码」形同虚设，文本永远变不成 `text_delta`，`message_start` 也从未被合成。同一处 fallthrough 还让 Chat ingress × Responses upstream 保持原样透传，Chat 协议客户端（OpenAI SDK / Codex CLI）收到完全没有 `choices` 的帧。`needs_ir_decode` 因此纳入 `(is_chat_ingress && use_responses)`：该组合解码为 IR 后按 Chat 协议重编码，桥接 #1 再补恰好一个 `data: [DONE]`。

**字节透传车道（`needs_ir_decode == false`）的盲区补洞**：字节恒等透传的每个事件都是 `IrStreamEvent::Raw(Vec<u8>)`（完整 SSE 帧，含 `data: ` 前缀与尾部 `\n\n`），IR 层的观察器在这里什么也看不到。`peek_chat_raw_frame` 复用 `sse_data_payload` 剥出 payload 后 `serde_json::from_slice`，读 `choices[].delta.tool_calls[].function.arguments` 并喂 `observe_tool_call_start` / `observe_arguments`。约束：

- **observation-only**：原始帧原封不动地转发，客户端收到的字节一个也不变。
- **门控在 `!needs_ir_decode`**：已解码车道上的 `Raw` 只是剩余副本，其载荷早已由 `ToolCallStart` / `ToolCallDelta` 计入；同时窥探会把同一段参数拼两次并产出假阳性 `NotJson`。
- `delta.tool_calls` 优先、`message.tool_calls` 只作非流式形状的兜底，两者**互斥**（同时读两边会拼出 `{"a":1}{"a":1}`）；缺 `index` 时以数组下标兜底；`arguments` 缺失 / 非字符串按 `""` 计入并仍登记该调用。
- 不可解析的帧（`data: [DONE]`、`: keep-alive`、截断数据）静默跳过——这是 warn-only 信号，绝不得把一帧透传数据变成错误。
- 解析器只有一份，落点有两个：`ToolArgSink` 让单元测试喂裸 `ToolArgWatch`、让分发 fold 喂 `ToolArgWatchHandle`（内层 watch 对 `dispatch::stream` 私有），后者把窥探结果重放成事件再 `observe`。

**`Raw` 帧过滤**：Responses 解码器把「无法表达为 IR」的帧作为 `Raw` 交出来是为保住同协议透传，但这些帧逐字转发到别的协议出口就是泄漏。解码分支直接滤除**每一个** `Raw`（`matches!(ev, IrStreamEvent::Raw(_))` 取反），不看载荷内容。

**契约**：过滤只在 `needs_ir_decode == true` 时生效，且只针对 Responses 上游——凡是「Responses 上游 + 已解码」的组合（Chat ingress、Anthropic ingress 皆属此族），**Responses 解码器产出的每一个 `Raw`** 一律不得流向非 Responses 出口。不看载荷内容：畸形 JSON、缺 `type`、未知 `type` 的帧同样带 Responses 上游的字节，内容嗅探（旧实现的 `type` 以 `response.` 前缀判定）对它们全部失效，正是原样泄漏的通道。`needs_ir_decode == false` 的同协议车道永不过滤、永不改写，那里的 `Raw` 就是上游原字节，是字节恒等不变式的一部分（该过滤落在解码分支内部，同协议车道根本进不到，因此不存在误过滤的可能）。

### 上游流内错误的客户端可见错误帧

HTTP 200 一旦提交，后续任何失败都改不了状态码。旧实现在 `dispatch/stream.rs` 的流循环里把 `IrStreamEvent::Error` 单独分支走掉：只 `record_stream_error` 记 analytics，**不进编码器**。于是“上游发了内容又失败”与“上游正常地短”在客户端侧完全同形——客户端看到一段内容后流就停了，照正常完成处理。

现在 `Error` **记录之后按车道分叉**：翻译车道（`needs_ir_decode == true`）**继续向下走同一条编码路径**（与 `Raw` / 结构化事件相同），由出口编码器按 ingress 协议合成一帧协议正确的错误帧；直通车道（`needs_ir_decode == false`）先**扣住**这个事件——provider 的 side-channel `Error` chunk 总是排在它所描述的上游错误帧**之前**，紧随其后的那个 `Raw` 就是上游自己的错误字节，原样转发并清掉扣留；若流结束时扣留仍在，说明这是**网关本地**产生的中断（读错误 / chunk 空闲超时，没有上游字节），终点合成恰好一帧 ingress 协议错误帧后关闭 body。顺序是硬约束：`record_stream_error` 在编码与 yield 之前调用，所以 attempt 状态与 `failure_reason` 口径不变，且字节外发前错误状态已就位（与 `TerminalGuard` 的分类依赖同一标志）。

下表是**翻译车道**的契约；直通车道的入站错误帧不查这张表（见表后的直通车道一节），但网关本地中断的合成帧与表内同形（出口编码器相同）：

| ingress | 帧形态 | 之后的终止符 | 实现 |
|---|---|---|---|
| Chat | 裸 `data: {"error":{"message":…,"type":"server_error","code":"STREAM_ERROR","param":null}}` | 不发 `data: [DONE]` | `sag-providers/src/codec/chat.rs` 的 `Error` 分支 |
| Anthropic | `event: error` + `data: {"type":"error","error":{"type":"api_error","message":…}}` | 不发 `message_delta` / `message_stop`（无 `stop_reason`） | `sag-providers/src/codec/anthropic.rs`；`StreamEncodeState.errored` 让 `Finish` / `End` 返回空 vec |
| Responses | `data: {"type":"error","code":"STREAM_ERROR","message":…,"param":null,"error":{"code":"STREAM_ERROR","message":…}}` | 不发 `response.completed` / `response.incomplete` | `responses.rs` 的 `encode_error_event`；`StreamEncodeState.errored` 让 `Raw` / `Finish` / `End` 返回空 vec |

三个形状都是上游自己定下的契约，不是网关自创：Chat Completions 的流内错误就是 `data: {"error":…}`，并不在错误帧后再发 `[DONE]`；Anthropic Messages 的错误流是 `event: error` + `type: "error"` 体，其后不再有 `message_stop`；Responses 的流内错误事件是 `type: "error"`，其后不再有 `response.completed`。IR 的 `Error` 事件只携带 message（不带错误类别），故 Anthropic 的 `error.type` 固定为通用的 `api_error`，Chat 的 `type` 为 `server_error`，Responses 的 `code` 与 Chat 的 `error.code` 同为网关的 `STREAM_ERROR` 标记（`param` 显式为 `null`，与 OpenAI 信封键集对齐）；Responses 帧额外带一份嵌套的 `error: {code, message}`，让只认网关通用信封（`{"error":{"code","message"}}`）的客户端也能读到上游消息。

**直通车道的入站错误帧相反**：`needs_ir_decode == false` 的 Chat→Chat / Anthropic→Anthropic native / Responses→Responses native 上，上游自己的错误帧**零合成**。上游自己的错误字节本来就以 `Raw` 在流里，逐字转发就是客户端与上游直连时看到的东西；上游在其错误帧之后发的一切（**包括它自己的 `data: [DONE]`**，以及 native 车道的 `message_stop` / `response.completed`）也照样逐字转发，不因「前面出过错」而抑制——字节保真优先。

**去重**：同一个失败可能被 provider 发两次（side-channel 的 `Error` chunk 加上解码出来的错误帧，两者在 `stream.rs` 里是两个 `Error` 事件），因此以 `StreamState.error` 是否已置位判定重复——第二次只进 analytics，不再产出帧（直通车道也只扣留第一个，终点最多合成一帧）。

**终端抑制的归处**：Chat 车道（即 Chat ingress 的 IR 解码车道）的两个 `errored` 标志都在 `dispatch/chat/mod.rs` 的每流闭包里（桥接 `#1` 抑制 `data: [DONE]`、桥接 `#4` 抑制合成的 `finish_reason`），Anthropic / Responses 车道的 `errored` 在各自 `StreamEncodeState` 里（Anthropic 让 `Finish` / `End` 返回空 vec，Responses 还额外让 `Raw` 返回空 vec）；错误帧本身就是该流最后一个客户端可见帧，`End` 仍会流到分发循环完成终点记账，只是编码为空。直通车道不走这条路：入站错误帧的清形下 `stream.rs` 根本不会把 `Error` 交给编码器，而网关本地中断的合成帧是**终点前最后一帧**，其后再无任何字节（body 立即关闭）。

**有意不变**：HTTP 状态恒为 200；attempt 的 `TargetStatus` / `failure_reason`、`call_logs` 的 `stream_error` 口径、`Usage` 合并都不变；Chat→Chat 透传车道的**上游帧**完全不变（上游自己的错误帧与其后的 `data: [DONE]` 都作为 `Raw` 原样转发，既有帧一个字节也不重写）；合成帧只出现在“客户端必须知道它失败了”且上游没给任何字节的场合（翻译车道的上游错误、两种车道的网关本地中断）；响应体已提交后不可能再退到 502，错误帧只是让客户端能分辨「失败」而不是「提前结束」。

### 无 IR 物种：SystemOne 镜像图片生成的分发骨架

SystemOne 没有 chat IR（`state` / `questions` / `answers` 与逐题概率字段无法映射），也不存在 Translated lane。于是 `dispatch_systemone` 逐段镜像 `dispatch_image`，只把「怎样请求」与「怎样读数」换成协议自己的：

| 步骤 | `dispatch_image` | `dispatch_systemone` |
|---|---|---|
| 在途计数 | 入口 `InFlightGuard`（非流式，guard 留在栈帧） | 同（`runtime.in_flight.acquire(route_name)`） |
| 会话 fact | 每请求一次 `log_session` | 同（无 IR ⇒ preview 传 `None`） |
| 取消检测 | 非流式 `DispatchCancelGuard` | 同（无 `stream` 字段 ⇒ 只有这一条路径） |
| 配置快照 | 一次性 `state.config.read().await.clone()` | 同（退避 / 熔断 / override 均读快照，不在循环里持锁） |
| 逐目标 | 熔断闸门 → `resolve_provider` → `run_with_retries_outcome` | 同 |
| 终点 | 成功 / 客户端 4xx / 全耗尽 502 三路 `mark_done()` 后发事件 | 同（`finish_systemone_{success,client_error,exhausted}`） |
| usage | 响应 usage → analytics | 响应 `usage.input_tokens/output_tokens` → `prompt/completion_tokens`，缺失留 NULL |

**carrier 契约（`build_systemone_body`）**：`POST /v1/systemone` handler 不把 body decode 成 IR，而是形状预检后把**整个原始 body map**放进取 `ChatCompletionRequest.extra`，`model` 单取为路由键——因此路由、会话键、日志身份、`extra` 三层覆盖全部复用 chat 的现成机制。`build_systemone_body` 从 `extra` 逐键拷回并只覆写 `model`（→ 目标 `real_model`），`state` / `questions` / `assets` / 未知字段逐字不离原形。响应侧同样不解码：`serde_json::Value` 直接 `Json(body)` 回给客户端，只有 `usage_from_body` 读一次 `usage`。

**能力闸门 `filter_systemone_targets`**：presence 即能力——只保留 `providers[name].apis` 含 `ApiKind::SystemOne` 的目标，其余 push `"{provider}/{model}: lacks capability systemone"` 到 `errors`（措辞与 chat 预检同款，丢弃前打 `info!`）。该检查在 `resolve_route_with_ingress` 内、enabled 过滤之后、ordering 之前，因此 chat 的硬需求能力表对 SystemOne **不适用**（`capability.rs` 中它的 streaming / reasoning / tools / images / documents 全为 false，且永不作为 Translated lane 的 fallback 候选）。全部目标被丢弃时走 `emit_all_disabled_failure`，即 502 信封带出这些原因。

**为什么不裸转发**：只写一个 handler 直接打上游会绕过重试退避、熔断、跨目标降级、`in_flight` 计数、`call_logs` / analytics 与会话注册表，决策路径的行为与可观测性会和 chat / image 分裂。复用骨架的代价只有一个：必须用一个 carrier 结构指代请求身份（见上方契约）。

**Trade-off**：`ChatCompletionRequest` 作为 carrier 略微误名（真正 payload 在 `extra`），但换来零新增分发路径——重试、熔断、粘性写回、terminal 记账全部不必重新实现，也不会与 chat 的修补错位。

### route 级 `pipeline = "systemone"`：身份、守卫与校验

`Pipeline`（`sag-config/src/config.rs`）取三值，serde 名为 `chat` / `image` / `systemone`——`systemone` 是单个单词，靠显式 `#[serde(rename = "systemone")]` 覆盖 `rename_all = "snake_case"` 的产物（否则会落成 `system_one`）。`as_str()` 与 serde 名保持一致，保存路径据此回写。

| 值 | 适用 ingress | 上游路径 |
|---|---|---|
| `chat`（默认） | chat 族三协议；**0008 起的兼容路径**：`/v1/systemone` 也接受 | 由协议 / provider `apis` 节点派生 |
| `image` | `/v1/chat/completions`（命中后分流到 `dispatch_image`）；`/v1/messages` / `/v1/responses` / `/v1/systemone` 显式拒绝 Image | `apis.image.base_url` + route.`api_endpoint` |
| `systemone` | **仅** `/v1/systemone` | `protocol_path(SystemOne)` 派生（`api_endpoint` 非法） |

**守卫位置与语义**：在 `resolve_route_with_ingress` 内，route 已按 `model_name`（精确 → `"*"` 通配）选中之后、`filter_enabled_targets` 之前：

```rust
if route.pipeline == Pipeline::SystemOne && ingress_protocol != IngressProtocol::SystemOne {
    return Err(emit_no_route_failure(/* 既有 no-route 路径 */).await);
}
```

语义是「该模型在这个 ingress 上不存在」：复用既有的 no-route 失败路径与错误信封（`/v1/messages` / `/v1/responses` 各自保留自己的信封），**不**新增错误类型、不产生 attempt 行、不触达上游、不 fall through 到 chat / image dispatch。守位选在 target 过滤之前，是因为它不依赖于目标——这是「路由身份」层面的判定。

`/v1/systemone` 自己的三种 pipeline 组合与改动前完全一致：`systemone` / `chat` 都接受（都走 `filter_systemone_targets` 的 presence 过滤 + `dispatch_systemone`），`image` 先过 presence 过滤、存活则由 handler 拒 Image（`422`）。

**能力与校验**：

- target 缺 `apis.systemone` → 运行时跳过（与 unhealthy target 同一条诊断，`errors` 记 `lacks capability systemone`），配置校验只在 warnings 中记一句——不是硬错误，方便把「尚未配好」的 provider 放在低优先级兜底位。
- chat 的硬需求能力表（`filter_capable_targets`）与 Translated lane 对它不适用：presence 即能力。
- `api_endpoint`（仅 image 有效）与 `session_sticky`（`dispatch_systemone` 丢弃 session write-back，pin 永远写不进去）在配置校验阶段拒绝；admin CRUD 走同一份校验，违规即 `400` 且不落盘。
- target 的 `provider` 名字不存在 → **配置校验硬错误**：`validate_routes` 对不在 `[[providers]]` 名字集合内的 `target.provider` push `references unknown provider '<name>'`，启动失败（admin CRUD 同一份校验，即 `400`），systemone route 不豁免。与上一条「provider 存在但缺 `apis.systemone` 只记 warning」是两条不同层的规则：前者是**配置校验期**的配置写错（名字根本不存在），后者是**运行时** target 过滤的跳过 + 诊断（provider 存在，只是没声明该协议），不得合并叙述。
- 四条 strategy 中除 `session_sticky` 外全部适用；`fallback` 语义不变——仍只统计 chat IR 三协议，`systemone` 不可作 fallback。
- `capabilities` 字段保留既有形状校验，但 chat 能力预检对它不生效。

**`/v1/models`**：`list_models` 对显式 systemone route 走独立分支——`ingress_protocols` 只在「存在 eligible 且 provider 声明了 `apis.systemone` 的 target」时包含 `systemone`，且**永不含 chat 族协议**（chat 请求对该 route 只会 no-route）。旧 pipeline 的「provider `apis` 并集」聚合逐字不变，包括其既有怪癖。

**route 身份只有 `model_name`**：落盘 `sync_array_of_tables` 的 key、admin 创建 / 更新 / 删除的定位、解析的精确 + 通配两级匹配都只看它。因此同一名字不能并存 chat 与 systemone 两条 route；`"*"` 也只有一条，声明成 systemone 就会一并接管所有未命中模型的 chat 兜底。

### chat 路由的 `apis` presence 预检（SystemOne 放开后的连带修正）

`filter_capable_targets` 在能力比对**之前**先做一次**无条件**（不需要任何硬需求）的 presence 检查：该目标**实际使用的协议**（`used_protocol`——native lane → ingress，translated lane → provider `fallback`）必须是 provider 已声明的 `apis` 节点；缺则 pre-drop，push `"{provider}/{model}: lacks capability {used}"` 到 `errors` 并打 `info!`，与 SystemOne 侧的 `filter_systemone_targets` 对称。

这条检查是「`apis.systemone` 可单独成节」这个新配置形状的连带要求。改动前每个 provider 至少有一个 chat-IR key，「`used_protocol` 的候选集恒非空」是隐含不变量；SystemOne 放开后它可能被过滤成空、回落到 `ChatCompletions`，「provider 有」不再等于「provider 声明了该协议」。没有这道门时这种目标会一路走到 `provider.chat()`，被 `catalog` 的 `unsupported()` 以 `BadRequest` 拒绝，再被 `is_client_caused_4xx` 判为客户端错误——400 终态、不 failover、不落 `target_attempts`，等于把配置错误记成调用方错误。

未知 provider 名**不**在这一层丢弃：留给 `resolve_provider` 记 `"{provider}: not found"`，以保留 failover。

### `fallback` 只覆盖 chat IR 三协议

`fallback` 的取值域与「是否需要 fallback」的判定只统计 `chat_completions` / `responses` / `anthropic_messages`（`sag-config` 的 `is_chat_ir_protocol`）。`apis.systemone` 是 presence 即能力的独立物种：provider 声明它不需要 `fallback`，也**不能**当 `fallback` 值——显式 `fallback = "systemone"` 在配置校验阶段直接报错，因为静默回退会把决策协议当成 chat 翻译目标（跨协议投递必然丢字段），比报错更危险。这是「拒绝未知值反模式」的边界：请求体容忍未知值，类型化配置枚举保持严格。

## 不变式

- `resolve_route()` 通过对比 `ConfigHandle.routes_version` 与 `DispatchRuntime.last_prune_version` 实现惰性缓存清理；仅在版本差异时移除已不存在路由对应的 `fill_first_orders`、`route_counters`、`active_targets` 条目。路由变更方（admin API / 热重载）负责递增 `routes_version`
- 客户端归因 4xx（400/413/422/未知 4xx）直接返客户端，不 failover、不挪 pin、不开熔断
- provider 持续性 4xx（401/404/403-非quota）不重试、不开熔断，failover + 挪 pin
- 403-quota 与 408/429/5xx 同属可重试，403-quota 走 `CircuitHint::QuotaExhausted` 长冷却（3600s），429 走 `CircuitHint::RateLimited` 短冷却
- 非流式请求使用 `timeout_secs` 做整体超时（request 级 `timeout()`）；流式请求的响应头到达由 `ttfb_timeout_secs` 约束，body 由 `chunk_timeout_secs` 做 chunk 间超时。成功流式请求的总时长不受 `timeout_secs` 限制（共享 client 不设总超时）
- backoff 在每次重试前 sleep（不是在失败后立即 sleep）
- 流式请求的 response body 不等待（不缓存整个响应）
- 终点帧判定基于编码后字节，且三协议形态并集匹配：跨协议组合（Chat ingress + Responses upstream 等）下不得因按 ingress 收窄而漏判
- 终点帧标志必须在 yield **之前**置位：先 yield 后置位会与 hyper 的 body drop 竞态
- `TerminalGuard::Drop` 的 error 分支优先于终点帧分支；终点帧转发后的断连记 `Success` 200，转发前记 `client_cancelled` 499
- drop-as-success 行必须携带 usage / `ttft_ms` / `upstream_duration_ms`；error 与 cancel 行这些列保持 NULL
- Anthropic `message_stop` / Responses 终点帧（`response.completed` / `response.incomplete`）桥接（`#2` / `#3`）必须位于 empty-peek 之后；Chat `data: [DONE]` 桥接（`#1`）必须位于 peek 之前（靠 Empty 特判）；native 透传路径字节恒等
- 桥接 `#2` / `#3` 仅在首个 `End` 且未见过 `Finish` 时合成 `Finish { finish_reason: None }`；合规上游不得因此多出 `message_delta` / `response.completed`
- 桥接 `#4`（Chat ingress + IR 解码车道）只合成 `finish_reason` chunk（`Some(Stop)`），终止符仍由桥接 `#1` 渲染；两者均不得在流内已见 `Error` 后生成
- 上游流内 `Error` 必须先记 analytics 再决定外发：翻译车道只发一帧（同一流上重复的 `Error` 不得重复外发），`[DONE]` / `message_stop` / `response.completed` / `stop_reason` 一律不发；直通车道零合成帧，上游错误字节与其自身终止符（含 `data: [DONE]`）原样转发
- `response.completed` 必须解码出 `Finish`（`status` + `incomplete_details` 经 `status_to_finish`，`output[]` 含 `function_call` 时折叠为 `tool_calls`）；只发 `Raw` 会让 Chat 客户端丢失 `finish_reason`
- `dispatch_systemone` 不得 decode 请求或响应体：wire body 由 `build_systemone_body` 从 carrier `extra` 逐键重建并只改写 `model`；响应 `Value` 直接回客户端，唯一允许的读取是 `usage`（缺失或形状不合法 ⇒ NULL，**不得编造 0**）
- SystemOne 恒非流式：`is_stream` 固定 `false`，不得伪造 `stream` 字段，也不得把 `InFlightGuard` 移交给 `StreamState`
- 形状预检（非对象 body / `model` 非字符串 / `questions` 非对象 → 422）必须在 carrier 构造与 `resolve_route_with_ingress` **之前**完成，不消耗路由上下文；`pipeline == Image` 的 422 靠路由解析出 `pipeline`，位于解析成功之后、`dispatch_systemone` 之前
- SystemOne 目标必须声明 `apis.systemone`（presence 即能力）；chat 的硬需求能力表与 Translated lane 对它一律不参与
- `pipeline = "systemone"` 的 route 只接受 `IngressProtocol::SystemOne`；其它 ingress 必须在 target 过滤**之前**走 no-route，不得 fall through 到 chat / image dispatch，也不得新增错误类型
- `pipeline = "systemone"` 拒绝 `api_endpoint`（上游路径由协议派生）与 `session_sticky`（无 session write-back），两者都在配置校验期报错；target 缺 `apis.systemone` 只记 warning，不得升格为启动失败
- target 的 provider 名字不存在是配置校验期**硬错误**（`references unknown provider`），不得因 systemone pipeline 放宽；它与「provider 存在但缺 `apis.systemone` 只记 warning、运行时被 pre-drop」分层：前者发生在 `validate_routes`（拒绝启动 / `400`），后者发生在 `resolve_route_with_ingress` 的 target 过滤（跳过 + `lacks capability` 诊断）
- route 身份只有 `model_name`：解析、落盘、admin 定位三处必须一致（精确 → `"*"` 通配）；同一 `model_name` 不得同时存在 chat 与 systemone 两条 route
- `/v1/models` 的显式 systemone route 只通告 `systemone`，不得因 provider 同时声明 chat 节点而通告 chat 协议；旧 pipeline 的聚合行为不得改变
- chat 路由的能力过滤先做 `apis` presence 检查：目标实际使用的协议必须是 provider 已声明的 `apis` 节点，缺则 pre-drop 并记 `lacks capability {used}`；未知 provider 名不在这一层丢弃（留给 `resolve_provider` 记 `not found`，保留 failover）
- `fallback` 的需求判定与取值域只统计 chat IR 三协议；`fallback = "systemone"` 必须在配置校验期拒绝，不得运行期降级
- 工具调用参数质量告警是**纯观测**：不得计入 `failure_reason`、不得触发重试 / fallback / 粘性变更、不得改写响应体字节；流式告警只在 `finish_stream_terminal` 发（`TerminalGuard::Drop` 不发，否则取消的流会假报 `Blank`）
- 工具参数窥探只在 `needs_ir_decode == false` 的透传车道启用；已解码车道上的 `Raw` 不得再被窥探（同一段 `arguments` 会被拼两次）
- 帧的解码器由 upstream 协议选（Anthropic / Responses / Chat 三路）且必须是有状态 `FnMut`；`needs_ir_decode` 必须包含 Chat ingress × Responses upstream
- Responses 解码器产出的**所有** `Raw`（不看 `type`）不得到达非 Responses 出口；过滤只在 `needs_ir_decode == true` 时生效，`needs_ir_decode == false` 的同协议车道字节恒等
- 一条流最多一个客户端可见错误帧，且它是最后可见字节：翻译车道由出口编码器即时渲染并抑制后续事件；直通车道的入站错误帧零合成（上游字节原样转发），网关本地中断（无配对 `Raw` 的 `Error`）由终点合成恰好一帧后关闭 body；两种情形都不得在错误帧后再发 `[DONE]` / `message_stop` / `response.completed`
- 每请求在途计数（`InFlightGuard`）在 `dispatch_chat` / `dispatch_image` / `dispatch_systemone` 入口 acquire，只由 `Drop` 释放；流式路径必须把 guard 移交 `StreamState._in_flight`（`build_streaming_response` 的 `in_flight` 参数），不得留在 dispatch 栈帧上——否则响应返回即释放，整条流的传输时间不被计入。计数器与 `AppState` 共享同一 `Arc`（见 [admin 0003](../admin/changes/0003-per-route-in-flight-counters-sse.md)）

## 测试策略

- `sag-dispatch/src/testing.rs` — 集成测试
- `sag-dispatch/src/dispatch/retry.rs` — 退避公式单元测试
- `sag-dispatch/src/dispatch/mod.rs` — `failure_log` / `success_log` round-trip 测试（含 `reasoning_effort` 字段）
- `sag-dispatch/src/dispatch/resolve.rs` — resolve 失败路径 `reasoning_effort` round-trip 测试
- `sag-dispatch/src/dispatch/image.rs` — 图片路径 `reasoning_effort=None` 断言
- `sag-dispatch/src/dispatch/systemone.rs` — 透传与 usage 留 NULL、跨目标降级、全耗尽 502、熔断跳过、`build_systemone_body` 只改 `model`、provider 解析失败时半开探针槽同步释放
- `sag-dispatch/src/dispatch/capability.rs` — chat 侧 presence 门：provider 缺 used protocol 的 `apis` 节点 → pre-drop `lacks capability`（未知 provider 不在此层丢弃）
- `sag-core/src/ir/tool_args.rs` — 参数质量三档判定（`{}` 为 `Ok`、空白为 `Blank`、非对象为 `NotJson`）、`ToolArgWatch` 的累积 / 最差档 / 空 delta / 重复 start，以及裸片段 API 与 `IrStreamEvent` API 等价
- `sag-dispatch/src/dispatch/chat/mod.rs` 的 `peek_tests` — raw 帧窥探：跨帧累积、空白参数、忽略 `[DONE]` / 非 JSON / keep-alive、缺 `index` 回退、IR 车道的门控（同一组事件 `peek=false` → `Ok`、`peek=true` → 假阳性 `NotJson`）
- `sag-dispatch/src/dispatch/chat/tests.rs` — 告警计数（内联 `CapturedEvents` 订阅器，流式每流一条 / 非流式每响应一条）、透传车道字节恒等、Chat×Responses 重编码、Anthropic×Responses 解码归属
- `sag-router/tests/systemone_ingress.rs` — 入口级集成：逐字透传、401 / 400 / 422 顺序、`lacks capability systemone` 过滤、image 路由 422、`/v1/models` 派生
- 关键场景：熔断器交互、重试耗尽、流式超时、4xx 分类判定（non-retryable vs retryable）、时间穿越（UTC 时间戳）

## 变更记录

- [0011 route 级 `pipeline = "systemone"`](changes/0011-route-pipeline-systemone.md) — `Pipeline` 三值（`systemone` 显式 serde rename）；`resolve_route_with_ingress` 的 pipeline × ingress 守卫（route 选中后、目标过滤前，不匹配走既有 no-route）；systemone 拒绝 `api_endpoint` / `session_sticky`、`apis.systemone` presence 缺失只记录 warning；`/v1/models` 对它只通告 `systemone`；route 身份只有 `model_name`
- [0010 流内错误帧在两种车道上的完整性](changes/0010-stream-error-frame-completeness.md) — `ResponsesCodec::encode_error_event` + `StreamEncodeState.errored`（错误帧后 `Raw` / `Finish` / `End` 返回空 vec）；`StreamState.pending_local_error` 让直通车道先扣住 `Error`、由紧随的 `Raw` 清除，流结束时仍扣留即为网关本地中断并在终点合成一帧；`use_responses && needs_ir_decode` 分支丢弃所有 Responses 解码器 `Raw`（删除 `is_responses_event_frame`）；解码器 `push_frame_raw` per-frame 去重
- [0009 工具调用参数质量告警与 Chat×Responses 车道的解码归属修正](changes/0009-tool-arg-quality-and-responses-lane-decode.md) — 新增 `sag-core` 纯谓词层 `ToolArgsQuality` / `ToolArgWatch`，流式（`finish_stream_terminal`）与非流式各发一条 warn-only 告警（不动空响应判定）；字节透传车道补 `peek_chat_raw_frame` 窥探（observation-only、门控在 `!needs_ir_decode`）；`StreamFrameDecoder` 改 boxed `FnMut` 并补 `use_responses` 分支，修掉 Anthropic ingress × Responses upstream 用错 Chat 解码器；`needs_ir_decode` 纳入 Chat ingress × Responses upstream（解码重编码为 Chat chunk），`is_responses_event_frame` 拦下已解码车道的 Responses `Raw` 帧；`response.completed` 解码出 `Finish`（`function_call` → `tool_calls`），补 Chat ingress 的 `finish_reason` 兜底与翻译车道协议正确的流内错误帧（Chat `data: {"error":…}` / Anthropic `event: error`，错误后不再发终止符；直通车道零合成帧）
- [0008 SystemOne 决策接口接入（原样透传 + 深度 dispatch）](changes/0008-systemone-ingress.md) — 新增 `dispatch_systemone`（镜像 `dispatch_image` 骨架、无 IR、恒非流式）、`build_systemone_body` 的 carrier 契约（`extra` 逐字重建 + 仅改写 `model`）、`filter_systemone_targets` 的 presence 即能力闸门，并把 `fallback` 语义收窄为 chat IR 三协议
- [0007 流式终点帧竞态误分类修复](changes/0007-stream-terminal-frame-race.md) — `is_client_terminal_frame` 字节级三协议判定（含 `response.cancelled`）+ `TerminalGuard` 三路分类（error→502 / 标志→200 带完整 usage / 否则 499），并补三条 translated 终点帧桥接（`[DONE]` / `message_stop` / `response.completed`，含不合规上游的块闭合与 `stop_reason`）
- [0006 流式请求超时边界修复](changes/0006-stream-request-timeout-boundary.md) — 成功流式响应不再受共享 client 总 timeout 限制，按 TTFB、chunk idle 和错误 body 阶段分别设定边界
- [0005 聊天分发单文件复杂度解耦与模块化](changes/0005-chat-dispatch-modularization.md)
- [0001 4xx Failover 归因策略](../failover-4xx-policy/changes/0001-4xx-attribution-policy.md)（跨模块：4xx 归因三分 + 403-quota 重分类）
- [0004 上游 4xx 不再触发重试与熔断](changes/0004-upstream-4xx-non-retryable.md)
- [0003 失败路径补全 reasoning_effort 记录](changes/0003-reasoning-effort-failure-paths.md)
- [0002 路由删除后缓存泄漏修复](changes/0002-cache-leak-fix.md)
- [0001 初始化](changes/0001-initial.md)
