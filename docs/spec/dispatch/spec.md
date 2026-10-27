---
created: 2026-06-03
updated: 2026-10-27
---

# 调度 — 规格

## 关键文件

- [sag-dispatch/src/dispatch/chat/](../../../sag-dispatch/src/dispatch/chat/) — 聊天补全分发模块（主调度、尝试结果与事实构造、空响应校验、translated 终点帧桥接）
- [sag-dispatch/src/dispatch/image.rs](../../../sag-dispatch/src/dispatch/image.rs) — 图片生成分发
- [sag-dispatch/src/dispatch/systemone.rs](../../../sag-dispatch/src/dispatch/systemone.rs) — SystemOne 决策分发（`dispatch_systemone` 镜像 `dispatch_image` 的尝试 / 降级骨架，`build_systemone_body` 从 carrier `extra` 重建 wire body）
- [sag-dispatch/src/dispatch/stream.rs](../../../sag-dispatch/src/dispatch/stream.rs) — SSE 流式处理（`StreamAnalytics` 共享状态、`TerminalGuard` 三路终点分类、`StreamState._in_flight` 持 per-route 在途 guard）
- [sag-core/src/protocol.rs](../../../sag-core/src/protocol.rs) — `is_client_terminal_frame`：三协议终点帧的字节级判定
- [sag-dispatch/src/dispatch/mod.rs](../../../sag-dispatch/src/dispatch/mod.rs) — `failure_log`、`success_log` 等日志构造函数
- [sag-dispatch/src/dispatch/retry.rs](../../../sag-dispatch/src/dispatch/retry.rs) — 重试退避逻辑 + `is_retryable_error` 错误分类判定
- [sag-dispatch/src/dispatch/resolve.rs](../../../sag-dispatch/src/dispatch/resolve.rs) — 路由匹配 + 目标选择（含 `filter_systemone_targets` 的 `apis.systemone` presence 闸门）
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

每次等待下一个 chunk 时加上 timeout。超时后发送 `StreamChunk::Error`（不发送 `Done`）并停止读取。该 error chunk 经 `StreamChunk::to_sse_bytes` 序列化为**通用** SSE error 事件（`STREAM_ERROR`，见 `sag-core/src/types.rs`），CallLog 记为 `502` / `error_category = stream_error`；专用 `STREAM_CHUNK_TIMEOUT` 错误码 / 504 状态尚未实现，属后续独立任务。

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

1. **判定层**（`sag_core::is_client_terminal_frame`）：对**编码后**的 SSE 帧字节做三协议形态并集匹配（Chat `data: [DONE]`、Anthropic `"type":"message_stop"`、Responses `"type":"response.completed"` / `"response.incomplete"` / `"type":"response.cancelled"`，兼 `event:` 行变体）。Responses 的三个流终点事件类型全部覆盖（`response.failed` 不算终点，它的 Error 帧走 502）。行形态标记逐行锚定前缀（防内容 delta 伪造），`"type":"..."` 引号形用子串匹配（字符串值内必被转义）。不按 ingress 分派——跨协议组合下帧形态由 upstream / lane 决定。
2. **状态层**（`StreamAnalytics`）：`terminal_frame_forwarded: AtomicBool` 在 yield **之前**置位（yield 后置位会输给 hyper 的 drop），并与 `TerminalGuard` 共享 `usage` / `first_byte_at` / `upstream_start`，使 drop-as-success 行能写出完整 usage、`ttft_ms`、`upstream_duration_ms`。

`TerminalGuard::Drop` 三路分类：error → 502 `stream_error_after_commit`（错误优先，`response.failed` 不升级）；无 error 但标志已置位 → `Success` 200；否则 → `client_cancelled` 499。

**Trade-off**：这是**统计口径变更**（历史误分类的 499 会被正确地计为成功），dashboard 曲线会有一次性台阶；换来的是「客户端实际体验」与「网关记录」一致，且不再产生全 NULL 的成功行。

### translated 终点帧桥接

translated 路径（ingress 协议 ≠ upstream 协议）下，upstream 不会发出 ingress 协议的终止帧，而 ingress 编码器的 `End => <终止帧>` 分支因 `End` 被内部消费而不执行，客户端可能永远收不到协议终点。三条桥接按「谁缺什么」分工：

| # | 组合 | 合成的帧 | 位置 |
|---|---|---|---|
| 1 | Chat ingress + Anthropic / Responses upstream | `data: [DONE]\n\n` | empty-peek **之前** |
| 2 | Anthropic ingress + 非 Anthropic upstream | `data: {"type":"message_stop"}\n\n` | empty-peek **之后** |
| 3 | Responses ingress + 非 Responses upstream | `response.completed`（由 `Finish` 渲染） | empty-peek **之后** |

`#2` / `#3` 共用 `apply_end_synthesis_bridge`（`EndSynthesis` 枚举 + `flat_map`，状态为 `finish_seen` / `end_sent`）：首个 `End` 若此前未见 `Finish`（即上游未给 `finish_reason`，不合规），先合成 `Finish { finish_reason: None }` 再发终止帧。编码器据此渲染：Anthropic → `content_block_stop`（闭合悬挂的 content block）+ `message_delta`（协议要求的 `stop_reason`，`None` 映射 `end_turn`）+ `message_stop`；Responses → `response.completed`（`status: completed`）。合规上游（已有 `Finish`）不重复合成，故不会多出第二条 `message_delta` 或第二个 `response.completed`。native 路径（`use_anthropic` / `use_responses`）不启用任何桥接，字节恒等。

桥接 #1 只补 Chat 协议的终止符：Chat ingress + Responses upstream 的响应帧仍为上游原样 Raw 透传（字节恒等），帧形态转换不在范围内。

**次序不变式**：`#1` 必须在 empty-peek **之前**——`classify_raw_frame` 把 `data: [DONE]` 判为 `RawVerdict::Empty`，因此真空上游仍能报 `EmptyResponse` 并走 `retry_on_empty`；`#2` / `#3` 必须在 peek **之后**——它们合成的帧（`message_stop` / `response.completed`）没有 Empty 特判，前置于 peek 会被 `classify_raw_frame` 读作 `Content`，使真空上游不再重试。该次序写在 `chat/mod.rs` 的注释里，改动桥接位置时需同时复核。

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
- Anthropic `message_stop` / Responses `response.completed` 桥接（`#2` / `#3`）必须位于 empty-peek 之后；Chat `data: [DONE]` 桥接（`#1`）必须位于 peek 之前（靠 Empty 特判）；native 透传路径字节恒等
- 桥接 `#2` / `#3` 仅在首个 `End` 且未见过 `Finish` 时合成 `Finish { finish_reason: None }`；合规上游不得因此多出 `message_delta` / `response.completed`
- `dispatch_systemone` 不得 decode 请求或响应体：wire body 由 `build_systemone_body` 从 carrier `extra` 逐键重建并只改写 `model`；响应 `Value` 直接回客户端，唯一允许的读取是 `usage`（缺失或形状不合法 ⇒ NULL，**不得编造 0**）
- SystemOne 恒非流式：`is_stream` 固定 `false`，不得伪造 `stream` 字段，也不得把 `InFlightGuard` 移交给 `StreamState`
- 形状预检（非对象 body / `model` 非字符串 / `questions` 非对象 → 422）必须在 carrier 构造与 `resolve_route_with_ingress` **之前**完成，不消耗路由上下文；`pipeline == Image` 的 422 靠路由解析出 `pipeline`，位于解析成功之后、`dispatch_systemone` 之前
- SystemOne 目标必须声明 `apis.systemone`（presence 即能力）；chat 的硬需求能力表与 Translated lane 对它一律不参与
- chat 路由的能力过滤先做 `apis` presence 检查：目标实际使用的协议必须是 provider 已声明的 `apis` 节点，缺则 pre-drop 并记 `lacks capability {used}`；未知 provider 名不在这一层丢弃（留给 `resolve_provider` 记 `not found`，保留 failover）
- `fallback` 的需求判定与取值域只统计 chat IR 三协议；`fallback = "systemone"` 必须在配置校验期拒绝，不得运行期降级
- 每请求在途计数（`InFlightGuard`）在 `dispatch_chat` / `dispatch_image` / `dispatch_systemone` 入口 acquire，只由 `Drop` 释放；流式路径必须把 guard 移交 `StreamState._in_flight`（`build_streaming_response` 的 `in_flight` 参数），不得留在 dispatch 栈帧上——否则响应返回即释放，整条流的传输时间不被计入。计数器与 `AppState` 共享同一 `Arc`（见 [admin 0003](../admin/changes/0003-per-route-in-flight-counters-sse.md)）

## 测试策略

- `sag-dispatch/src/testing.rs` — 集成测试
- `sag-dispatch/src/dispatch/retry.rs` — 退避公式单元测试
- `sag-dispatch/src/dispatch/mod.rs` — `failure_log` / `success_log` round-trip 测试（含 `reasoning_effort` 字段）
- `sag-dispatch/src/dispatch/resolve.rs` — resolve 失败路径 `reasoning_effort` round-trip 测试
- `sag-dispatch/src/dispatch/image.rs` — 图片路径 `reasoning_effort=None` 断言
- `sag-dispatch/src/dispatch/systemone.rs` — 透传与 usage 留 NULL、跨目标降级、全耗尽 502、熔断跳过、`build_systemone_body` 只改 `model`、provider 解析失败时半开探针槽同步释放
- `sag-dispatch/src/dispatch/capability.rs` — chat 侧 presence 门：provider 缺 used protocol 的 `apis` 节点 → pre-drop `lacks capability`（未知 provider 不在此层丢弃）
- `sag-router/tests/systemone_ingress.rs` — 入口级集成：逐字透传、401 / 400 / 422 顺序、`lacks capability systemone` 过滤、image 路由 422、`/v1/models` 派生
- 关键场景：熔断器交互、重试耗尽、流式超时、4xx 分类判定（non-retryable vs retryable）、时间穿越（UTC 时间戳）

## 变更记录

- [0008 SystemOne 决策接口接入（原样透传 + 深度 dispatch）](changes/0008-systemone-ingress.md) — 新增 `dispatch_systemone`（镜像 `dispatch_image` 骨架、无 IR、恒非流式）、`build_systemone_body` 的 carrier 契约（`extra` 逐字重建 + 仅改写 `model`）、`filter_systemone_targets` 的 presence 即能力闸门，并把 `fallback` 语义收窄为 chat IR 三协议
- [0007 流式终点帧竞态误分类修复](changes/0007-stream-terminal-frame-race.md) — `is_client_terminal_frame` 字节级三协议判定（含 `response.cancelled`）+ `TerminalGuard` 三路分类（error→502 / 标志→200 带完整 usage / 否则 499），并补三条 translated 终点帧桥接（`[DONE]` / `message_stop` / `response.completed`，含不合规上游的块闭合与 `stop_reason`）
- [0006 流式请求超时边界修复](changes/0006-stream-request-timeout-boundary.md) — 成功流式响应不再受共享 client 总 timeout 限制，按 TTFB、chunk idle 和错误 body 阶段分别设定边界
- [0005 聊天分发单文件复杂度解耦与模块化](changes/0005-chat-dispatch-modularization.md)
- [0001 4xx Failover 归因策略](../failover-4xx-policy/changes/0001-4xx-attribution-policy.md)（跨模块：4xx 归因三分 + 403-quota 重分类）
- [0004 上游 4xx 不再触发重试与熔断](changes/0004-upstream-4xx-non-retryable.md)
- [0003 失败路径补全 reasoning_effort 记录](changes/0003-reasoning-effort-failure-paths.md)
- [0002 路由删除后缓存泄漏修复](changes/0002-cache-leak-fix.md)
- [0001 初始化](changes/0001-initial.md)
