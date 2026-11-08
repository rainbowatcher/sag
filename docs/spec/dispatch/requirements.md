---
created: 2026-06-03
updated: 2026-11-04
---

# 调度

## 目标

协调路由选择、供应商调用、重试退避和流式处理的请求分发逻辑。

## 预期行为

### 分发流程

1. 接收客户端请求（ChatCompletionRequest / ImageGenerationRequest / SystemOneRequest）
2. 检查 `routes_version`，仅在路由集发生变更时执行惰性缓存清理（`prune_stale_entries`），移除 config 路由中已不存在的 model_name 对应的 `fill_first_orders`、`route_counters`、`active_targets` 条目；版本未变则跳过，避免不必要的写锁竞争
3. 匹配路由 → 按策略排序 targets
4. 对每个 target，反复尝试至重试耗尽：
   a. 检查熔断器状态（冷却中则跳过）
   b. 检查 provider 是否 enabled
   c. 调用 provider（chat / chat_stream / chat_image / systemone）
   d. 成功 → 返回响应
   e. 失败 → 根据错误类型决定重试或 fallthrough：
      - **客户端归因 4xx**（400/413/422/未知 4xx）：直接返客户端，不重试、不 failover、不挪 pin、不开熔断
      - **provider 持续性 4xx**（401/404/403-非quota）：不重试、不开熔断，failover 到下一 target（StickyFailover 下成功则挪 pin）
      - **可重试错误**（408/429/5xx/超时/网络错误/**403-quota**）：按退避策略重试，耗尽后开熔断（429 走 `CircuitHint::RateLimited` 短冷却，403-quota 走 `CircuitHint::QuotaExhausted` 长冷却）
5. 全部 targets 耗尽 → 返回聚合错误

**失败路径**在写入 `call_logs` 时携带 `request.reasoning_effort`，确保失败请求的推理意图可被审计——与成功路径一致。图片生成路径传 `None`（不适用）。

**在途计数**：每次派发在入口对所属路由的 `model_name` 计一（早于隐私过滤与重试调度，含退避 sleep），整条生命周期结束时自动释放；流式请求把计数持有者交给响应体，客户端断连同样释放。计数是进程内只读观测设施，不进 `call_logs`、不影响路由选择，经 `GET /api/inflight` 对外（见 [管理 API → 0003](../admin/changes/0003-per-route-in-flight-counters-sse.md)）。

### 重试退避

每次重试前等待指数退避延迟：

- 公式：`min(base_ms * 2^attempt, max_ms)`
- `attempt` 从 0 计数（第 0 次重试前等待 `base_ms`）
- `jitter = true` 时：`delay = random(0, computed_cap)`（防 thundering herd）
- `jitter = false` 时：`delay = computed_cap`（确定性，便于测试）

### 上游 4xx 错误归因

上游返回的 HTTP 4xx 状态码按归因三类，决定重试、failover 与熔断行为（详见 [4xx Failover 归因策略](../failover-4xx-policy/spec.md)）：

| 状态码 | 归因 | 行为 |
|--------|------|------|
| 400/413/422/未知 4xx | 客户端参数错 | 直接返客户端，不重试、不 failover、不挪 pin、不开熔断 |
| 401/404/403-非quota | provider 持续性 | 不重试、不开熔断，failover + 挪 pin（StickyFailover） |
| 403 + body 含 quota 关键词 | provider 瞬时（额度） | 可重试，耗尽后开熔断 `CircuitHint::QuotaExhausted`（3600s） |
| 408 Request Timeout | provider 瞬时（超时） | 可重试，耗尽后开熔断（60s） |
| 429 Too Many Requests | provider 瞬时（限流） | 可重试，走 `CircuitHint::RateLimited` 短冷却（10s） |

**原则**：客户端归因的 4xx（请求被拒）换一家 provider 同样会被拒，且改全员共享状态会有爆炸半径——故直接返客户端、诚实透传。provider 持续性故障（key 坏、model 下架）不会自愈，挪 pin 切走且待着、等人修。provider 瞬时故障（限流、超时、额度、5xx）会自愈，走熔断 + 周期探针等其恢复。

### 流式处理

- SSE chunk 通过 tokio channel（`mpsc::unbounded`）从 provider 传给 axum streaming body
- 非流式错误在 channel 中传递为 error chunk
- `chunk_timeout_secs` 通过 `tokio::time::timeout` 实现 chunk 间超时
- **终点帧与断连分类**：客户端收到终点帧（Chat `data: [DONE]`、Anthropic `message_stop`、Responses `response.completed` / `response.incomplete` / `response.cancelled`）后关连接是协议允许的正常收尾。SAG 以「终点帧是否已转发给客户端」为界判定断连性质：
  - 终点帧转发**之后**的断连 → `Success` / 200（上游 FIN 尾延迟，非取消），且该行携带完整 usage / `ttft_ms` / `upstream_duration_ms`
  - 终点帧转发**之前**的断连 → `client_cancelled` / 499，usage / `ttft_ms` / `upstream_duration_ms` 为 NULL
  - 流内已观察到上游 error 时，`stream_error_after_commit` / 502 优先于终点帧标志（`response.failed` 永不被升级为成功）
  - 终点帧判定基于**编码后字节**，三协议形态并集匹配且行锚定，内容 delta 中的伪造字样不误判
  - 终点帧判定基于**编码后字节**，三协议形态并集匹配且行锚定，内容 delta 中的伪造字样不误判；Responses 的三个流终点事件类型（completed / incomplete / cancelled）全部覆盖
- **translated 桥接终点帧**：ingress 协议与 upstream 协议不一致时，网关在首个 `End` 合成 ingress 协议的终止帧，保证客户端总能收到协议终点：
  - Chat ingress + Anthropic / Responses upstream → `data: [DONE]`。Anthropic 上游的响应帧仍为上游原样透传；Responses 上游的响应帧**解码为 IR 后重新编码为 Chat chunk**（Chat 协议客户端不得收到 `response.*` 事件名），字节恒等只对同构车道（chat→chat、native→native）成立
  - Chat ingress + IR 解码车道（Anthropic / Responses upstream）→ 上游未发 `finish_reason` 时先补一个 `finish_reason: stop` chunk，再发 `data: [DONE]`（桥接 #4）；合规 Responses 上游的 `response.completed` 现已解码为 `Finish`（`response.output` 含 `function_call` 时 reason 为 `tool_calls`），故自带 `finish_reason`
  - 流内出现上游错误时上述桥接全部让路（桥接只存在于翻译车道）：错误帧已是该流最后一个客户端可见帧，`[DONE]` / `message_stop` / `response.completed` 均不再发送，也不合成 `finish_reason` / `stop_reason`
  - Anthropic ingress + 非 Anthropic upstream → `message_stop`；上游未发 `finish_reason` 时先闭合 content block 并补 `message_delta`（`stop_reason`），合规上游不重复
  - Responses ingress + 非 Responses upstream → `response.completed`（`length` / `content_filter` 为 `response.incomplete` + `incomplete_details.reason`）；上游未发 `finish_reason` 时合成，合规上游不重复
  - native 透传路径字节不变，空响应重试（empty-peek）语义不变
- **运营影响**：桥接只补协议缺失的终止帧（包括不合规上游的块闭合与 `stop_reason`），属协议合规修复，不改变分类与统计口径
- **工具调用参数质量告警（warn-only）**：上游发出的工具调用若 `arguments` 为空 / 纯空白 / 非 JSON 对象（`null`、数组、数字、JSON 字符串、语法坏掉），流式每流一次、非流式每响应一次记一条 `warn!`，携带工具调用数与最差质量档。该信号**只观测不干预**：不计失败、不触发重试或降级、不改写响应体字节、不参与 `call_logs` 分类——空白参数响应仍记 `Success` + `failure_reason: None`、仍不触发 fallback。未收到任何 `arguments` delta 的工具调用按「空白」计（起了头却无参数正是要抓的缺陷）；被取消的流不报（客户端提前断连会把完整参数误报成空白）。同构 native 透传车道（Anthropic→Anthropic、Responses→Responses）不观察：客户端收到的就是上游自己的帧，网关不仲裁其载荷质量
- **帧的解码归属与 Responses 帧过滤**：解码器由 **upstream 协议**选定（Anthropic 帧走 Anthropic 解码器、Responses 帧走有状态的 Responses 解码器、其余走 Chat 解码器），不得只看 ingress 协议；`needs_ir_decode` 包含 Chat ingress × Responses upstream。已解码车道上，**Responses 解码器产出的所有 `Raw` 帧**都不得流向非 Responses 出口（不看载荷内容：畸形 JSON / 缺 `type` / 未知 `type` 同样带 Responses 字节）；`needs_ir_decode == false` 的同协议车道永不过滤、永不改写
- **上游流内错误对客户端可见**：HTTP 200 已提交后上游中途失败（流内 `error` 事件、`response.failed`、chunk 空闲超时、连接中断），`IrStreamEvent::Error` 此前只被分发循环记入 analytics，客户端看到的是一段「内容突然停住」的正常短响应。现在该事件**先记录 analytics**，之后按车道分叉，两种车道的最终形态都是**恰好一帧** ingress 协议错误帧、其后无任何字节（body 立即关闭）：
  - **翻译车道**（`needs_ir_decode == true`）：`Error` 直接交给出口编码器合成错误帧；后续事件全部丢弃（上游帧属于另一协议，不得泄漏）
  - **直通车道**（`needs_ir_decode == false`）：先扣住 `Error`。provider 的 side-channel `Error` chunk 总是排在它所描述的上游错误帧**之前**，紧随其后的 `Raw` 即上游自己的错误字节，原样转发并清掉扣留；流结束时扣留仍在 ⇒ 这是**网关本地**中断（读错误 / chunk 空闲超时），终点合成一帧错误帧
  - Chat 出口（OpenAI Chat Completions 契约）：一帧裸 `data: {"error":{"message":…,"type":"server_error","code":"STREAM_ERROR","param":null}}`，**其后不发 `data: [DONE]`**——OpenAI 在流内 `error` 帧之后不再发终止符
  - Anthropic 出口（Messages 契约）：`event: error` + `data: {"type":"error","error":{"type":"api_error","message":…}}`，**其后不发 `message_delta` / `message_stop`**（即不出现 `stop_reason`）——Anthropic 的错误流以 `event: error` 终止
  - Responses 出口（Responses 契约）：`data: {"type":"error","code":"STREAM_ERROR","message":…,"param":null,"error":{"code":"STREAM_ERROR","message":…}}`，**其后不发 `response.completed` / `response.incomplete`**——嵌套的 `error` 对象让只认网关通用信封 `{"error":{"code","message"}}` 的客户端也能读到上游消息
  - 直通车道上的**入站**上游错误帧：**零合成帧**——上游自己的错误字节作为 `Raw` 原样转发，上游其后发的一切（**包括它自己的 `data: [DONE]` / `message_stop` / `response.completed`**）同样原样转发；字节保真优先于「错误帧之后不再有帧」
  - 同一流上同一失败可能以两个 `Error` 事件到达（provider 的 side-channel 错误 chunk 与解码出的错误帧），只有第一个成为客户端可见帧（直通车道也只扣留第一个），后续只进 analytics
  - 不变的部分：HTTP 状态仍是 200；attempt 的 `TargetStatus` / `failure_reason` 与 analytics 错误记账口径不变；`Usage` 处理不变；Chat→Chat 字节恒等透传车道的**上游帧**一个字节也不变（新增的只有网关本地中断的合成帧）

### 图片生成

- 调用 provider 的 `chat_image()` 方法
- 响应只取 `data[0]`（单张图片），`created` 取 `SystemTime::now()` 或上游原值

### SystemOne 决策接口

入口 `POST /v1/systemone`（TypeSafe System One 决策协议，LiteLLM / OpenRouter / Ollama 以同一形状兼容）：请求 `{model, state, questions, assets?}`，响应 `{id?, model, answers, usage}`。`state` / `questions` / `answers` 与逐题概率字段**无法映射到 chat IR**，任何 decode → encode 往返都会丢字段，因此该协议与图片生成同属「无 IR 分发物种」——不走 `dispatch_chat`，而是调用 provider 的 `systemone()` 原生方法：

- **原样透传**：`state` / `questions` / `assets` 与响应 `answers` / `id` / 各题型概率字段零解码、零改写；wire body 的唯一改动是把 `model` 改写为目标的 `real_model`（与 chat / image 一致）。`state` 的三种形态（字符串 / 对象 / 数组）与 `questions` 的动态键名原样保留
- **完整复用分发骨架**：in-flight 计数、会话注册表 fact、熔断闸门、重试退避、跨目标降级与 analytics / `call_logs` 终态记账与 chat / image 一致，不做最小转发（裸转发会绕过这些设施，使决策路径运维不可见）
- **校验边界（只校验路由所需）**：JSON 语法错 → `400`；body 非对象、`model` 非字符串、`questions` 非对象 → `422 unprocessable`（形状预检先于 carrier 构造与派发）；`pipeline = "image"` 的路由 → `422`。**不校验** `question.type` 与 `criteria` 形状——非法值由上游拒绝（请求体容忍未知值，严格性只保留给类型化配置枚举）
- **presence 即能力**：SystemOne 既无 chat IR 也无 Translated lane，因此目标 provider 必须声明 `apis.systemone`。缺该节点的目标在解析阶段被 `filter_systemone_targets` 丢弃，`errors` 记 `"{provider}/{model}: lacks capability systemone"`，全部被丢弃时沿用 all-disabled / fallback-exhausted 的 `502` 信封并原样带出该原因
- **错误信封**：统一走 SAG 的 `{ "error": { "code", "message" } }`（OpenAI 信封），不采用 TypeSafe 原生 `{detail}`。上游以 HTTP 200 内嵌的 `error` 对象按上游错误升格——rate-limit 特征 → `429`，其它 → `502`
- **无流式**：协议没有 `stream` 字段，恒为一次性 JSON 响应；取消检测只走非流式 `DispatchCancelGuard` 路径
- **analytics 口径**：`usage.input_tokens/output_tokens` → `prompt_tokens/completion_tokens`，缺失或形状不合法一律留 NULL（**不编造 0**）；每次请求照常计入 `in_flight`、写一次会话注册表 fact、按目标产生 `TargetAttemptFact` 并落 `call_logs` 终态（成功 / 客户端 4xx / 全耗尽 `502` 模型失败三路）
- **`fallback` 只覆盖 chat IR 三协议**：`fallback` 的取值域与「是否需要 fallback」的判定只统计 `chat_completions` / `responses` / `anthropic_messages`。仅有 `apis.systemone` + 单个 chat 协议时仍不需要 `fallback`；显式 `fallback = "systemone"` 在配置校验阶段报错（`fallback='systemone' is not supported`）——静默回退会把决策协议当成 chat 翻译目标
- **派生视图**：`GET /v1/models` 的 `ingress_protocols` 由 `ApiKind::protocols()` 派生，自动包含 `systemone`；`POST /api/providers/probe-protocols` 明确不支持该协议（`minimal_request` 返回 `None` → 调用方转 `400`），因为决策协议的请求形状与 chat 无关，无法用 chat 探针探测

### 超时错误区分

网关在供给上游响应时发生的超时与"上游不可达"是不同的失败模式，需要可观测：

- **504 vs 502 区分**：504 `GATEWAY_TIMEOUT` 表示上游可能还活着但响应太慢；502 `BAD_GATEWAY` 表示上游返回无效响应或完全无法通信
- **超时分类**：

| 超时类型 | 发生层 | 触发机制 | HTTP 状态码 | 错误码 |
|---------|--------|---------|------------|--------|
| 请求总超时（非流式） | `ProviderBase::send_json` | request 级 `timeout()`（`RequestBuilder::timeout`），覆盖建连+响应头+body 完整往返 | **502** | `NETWORK_ERROR`（reqwest 超时经 `AppError::Network` 映射，`is_timeout()` 可判定） |
| 流式 TTFB 超时 | `ProviderBase::send_json_stream` | `tokio::time::timeout` 包裹 `request.send()`，仅覆盖到响应头到达 | **504** | `TTFB_TIMEOUT` |
| chunk 空闲超时 | provider `chat_stream` spawned task | `tokio::time::timeout` 包裹 `byte_stream.next()` | **502**（CallLog，`error_category = stream_error`） | `STREAM_ERROR`（两种车道都由出口编码器按 ingress 协议合成的错误帧：直通车道没有上游字节可转发；专用 `STREAM_CHUNK_TIMEOUT` 未实现，属后续独立任务） |

- **成功流不受 `timeout_secs` 约束**：共享 reqwest client 不设置总超时（无 `Client::timeout`），`timeout_secs` 只作为非流式请求的 request 级 deadline；成功流式响应头到达后，body 总时长只受 `chunk_timeout_secs`（chunk 空闲）约束
- **检测方式**：非流式 `send_json` 的 request 级超时产生 reqwest `is_timeout()` 错误（映射为 `AppError::Network`）；流式 TTFB 超时由 `tokio::time::timeout` 判定并返回 `AppError::TtfbTimeout`，不依赖 reqwest `is_timeout()`
- **重试行为**：`AppError::TtfbTimeout` 与超时导致的 `AppError::Network` 均属 retryable（见 `retry.rs::is_retryable_error`）——超时后重试同一 provider 是合理的（可能只是那次请求排队太久）
- **CallLog 记录**：超时错误在 `call_logs` 中记录 `status_code`（非流式超时 502，流式 TTFB 超时 504，chunk 超时在响应头已发送后记录到 analytics）。`error` 字段包含超时类型和阈值
- **流式 chunk 超时特殊处理**：HTTP 响应头（200 + `text/event-stream`）已发送给客户端后无法再改状态码，唯一可用的信号是**一帧客户端可见的协议错误帧**——provider 发出 `StreamChunk::Error("stream chunk idle timeout after Ns")`（转成 `IrStreamEvent::Error`），分发循环先记 analytics（`status_code = 502`、`error_category = stream_error`），再把同一个事件交给**出口编码器**按 ingress 协议合成错误帧（形状见「流式处理」节的「上游流内错误对客户端可见」），且**不再发送终止符**；这类中断是网关本地产生的（没有上游错误字节），所以翻译车道与直通车道都会合成恰好一帧，body 在错误帧之后立即关闭。该帧**不再**沿用 `sag-core::types::StreamChunk::to_sse_bytes` 的 `{"error":{"code":"STREAM_ERROR","message":...}}` 旧信封。**当前并未实现**专用 `STREAM_CHUNK_TIMEOUT` 错误码 / 504 状态——让 SSE error 事件携带专用错误码属于后续独立任务（不改变外部错误协议、不做 taxonomy 重构）

## 配置 Schema

参见 [配置 — 重试退避](/guide/configuration#重试退避)

## 功能边界

### 包含

- 请求分发（chat / stream / image / SystemOne 决策）
- 指数退避重试
- 流式 SSE 透传
- 错误聚合（返回所有失败原因）
- 超时错误的 504 vs 502 区分
- 流式 chunk 超时的 SSE error 通知
- 流内上游错误在翻译车道以协议正确的错误帧对客户端可见（Chat / Anthropic 出口），其后不再发送终止符；直通车道原样转发上游错误字节与其自身终止符
- 流式终点帧判定与断连分类（终点帧后断连记成功）
- translated 路径的终点帧桥接（`[DONE]` / `message_stop` / `response.completed`）

### 不包含

- 请求排队 / 优先级队列
- 并发控制 / 信号量限制
- 请求缓存（由上游 Anthropic cache_control 处理）
- 流式请求的总时长上限（当前仅 chunk 间空闲超时）
- 超时错误的告警/通知（属于外部监控系统职责）
- SystemOne 的流式响应（协议无 `stream` 字段，恒一次性 JSON）
- SystemOne 请求体的语义校验（`question.type` / `criteria` 形状交给上游判定）
- SystemOne 的 chat IR 往返或 translated 翻译（无 IR 物种，不存在翻译 lane）

## 与其它模块的交互

- **路由**：提供 ordered targets
- **供应商**：调用 provider trait 方法
- **熔断器**：查询/更新熔断器状态
- **分析**：异步记录调用日志

## 变更记录

- [0010 流内错误帧在两种车道上的完整性](changes/0010-stream-error-frame-completeness.md) — Responses 出口补 `{"type":"error"}` 错误帧（`StreamEncodeState.errored` 抑制其后的 `Raw` / `Finish` / `End`）；直通车道区分「入站上游错误帧（`Raw` 原样转发）」与「网关本地中断（读错误 / chunk 空闲超时，终点合成恰好一帧 ingress 协议错误帧后关闭 body）」；转译车道丢弃 Responses 解码器的**所有** `Raw`（畸形 JSON / 缺 `type` / 未知 `type` 不再泄漏）；多行畸形事件不再重复整帧
- [0009 工具调用参数质量告警与 Chat×Responses 车道的解码归属修正](changes/0009-tool-arg-quality-and-responses-lane-decode.md) — 空白 / 非法工具参数改为流式每流一次、非流式每响应一次的 `warn!`（warn-only，三处空响应谓词行为不变）；Chat ingress × Chat upstream 的透传车道补 raw 帧窥探；解码器按 upstream 协议选定（修掉 Anthropic ingress × Responses upstream 用错 Chat 解码器），Chat ingress × Responses upstream 改为解码重编码 + Responses `Raw` 帧过滤；上游流内错误在翻译车道改为客户端可见的协议错误帧（Chat `data: {"error":…}` / Anthropic `event: error`，其后不再发终止符；直通车道零合成帧、原样转发上游字节），`response.completed` 解码为 `Finish`（`function_call` → `tool_calls`）并补 Chat ingress 的 `finish_reason` 兜底
- [0008 SystemOne 决策接口接入（原样透传 + 深度 dispatch）](changes/0008-systemone-ingress.md) — 新增 `POST /v1/systemone` 无 IR 分发路径：只校验路由所需字段（400 / 422）、body 与响应逐字透传（仅改写 `model`）、`apis.systemone` presence 即能力、错误统一 OpenAI 信封、恒非流式，`fallback` 取值域收窄为 chat IR 三协议
- [0003 per-route 在途计数与 `GET /api/inflight` SSE 端点](../admin/changes/0003-per-route-in-flight-counters-sse.md)（跨模块：入口 acquire + 流式 guard 移交 `StreamState`）
- [0007 流式终点帧竞态误分类修复](changes/0007-stream-terminal-frame-race.md) — 终点帧转发后的客户端断连改记成功（499 计数下降、成功计数上升），并补 drop-as-success 行的 usage / TTFT / upstream 时长，以及三条 translated 终点帧桥接（`[DONE]` / `message_stop` / `response.completed`）与不合规上游的块闭合 / `stop_reason`
- [0006 流式请求超时边界修复](changes/0006-stream-request-timeout-boundary.md) — 成功流式响应不再受共享 client 总 timeout 限制，保留非流式 request-level、流式 TTFB 和 chunk idle timeout
- [0001 4xx Failover 归因策略](../failover-4xx-policy/changes/0001-4xx-attribution-policy.md)（跨模块：4xx 归因三分 + 403-quota 重分类）
- [0004 上游 4xx 不再触发重试与熔断](changes/0004-upstream-4xx-non-retryable.md)
- [0003 失败路径补全 reasoning_effort 记录](changes/0003-reasoning-effort-failure-paths.md)
- [0002 路由删除后缓存泄漏修复](changes/0002-cache-leak-fix.md)
- [0001 初始化](changes/0001-initial.md)
