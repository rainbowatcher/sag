---
created: 2026-06-03
updated: 2026-10-27
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
  - Chat ingress + Anthropic / Responses upstream → `data: [DONE]`（响应帧仍为上游原样透传）
  - Anthropic ingress + 非 Anthropic upstream → `message_stop`；上游未发 `finish_reason` 时先闭合 content block 并补 `message_delta`（`stop_reason`），合规上游不重复
  - Responses ingress + 非 Responses upstream → `response.completed`；上游未发 `finish_reason` 时合成，合规上游不重复
  - native 透传路径字节不变，空响应重试（empty-peek）语义不变
- **运营影响**：桥接只补协议缺失的终止帧（包括不合规上游的块闭合与 `stop_reason`），属协议合规修复，不改变分类与统计口径

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
| chunk 空闲超时 | provider `chat_stream` spawned task | `tokio::time::timeout` 包裹 `byte_stream.next()` | **502**（CallLog，`error_category = stream_error`） | `STREAM_ERROR`（通用 SSE error 事件；专用 `STREAM_CHUNK_TIMEOUT` 未实现，属后续独立任务） |

- **成功流不受 `timeout_secs` 约束**：共享 reqwest client 不设置总超时（无 `Client::timeout`），`timeout_secs` 只作为非流式请求的 request 级 deadline；成功流式响应头到达后，body 总时长只受 `chunk_timeout_secs`（chunk 空闲）约束
- **检测方式**：非流式 `send_json` 的 request 级超时产生 reqwest `is_timeout()` 错误（映射为 `AppError::Network`）；流式 TTFB 超时由 `tokio::time::timeout` 判定并返回 `AppError::TtfbTimeout`，不依赖 reqwest `is_timeout()`
- **重试行为**：`AppError::TtfbTimeout` 与超时导致的 `AppError::Network` 均属 retryable（见 `retry.rs::is_retryable_error`）——超时后重试同一 provider 是合理的（可能只是那次请求排队太久）
- **CallLog 记录**：超时错误在 `call_logs` 中记录 `status_code`（非流式超时 502，流式 TTFB 超时 504，chunk 超时在响应头已发送后记录到 analytics）。`error` 字段包含超时类型和阈值
- **流式 chunk 超时特殊处理**：HTTP 响应头（200 + `text/event-stream`）已发送给客户端后无法再改状态码，通过**通用 SSE error 事件**通知客户端——provider 发出 `StreamChunk::Error("stream chunk idle timeout after Ns")`，经 `sag-core::types::StreamChunk::to_sse_bytes` 固定序列化为 `data: {"error":{"code":"STREAM_ERROR","message":"..."}}`，且不发送 `Done`；CallLog 记录 `status_code = 502`、`error_category = stream_error`。**当前并未实现**专用 `STREAM_CHUNK_TIMEOUT` 错误码 / 504 状态——让 SSE error 事件携带专用错误码属于后续独立任务（不改变外部错误协议、不做 taxonomy 重构）

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

- [0008 SystemOne 决策接口接入（原样透传 + 深度 dispatch）](changes/0008-systemone-ingress.md) — 新增 `POST /v1/systemone` 无 IR 分发路径：只校验路由所需字段（400 / 422）、body 与响应逐字透传（仅改写 `model`）、`apis.systemone` presence 即能力、错误统一 OpenAI 信封、恒非流式，`fallback` 取值域收窄为 chat IR 三协议
- [0003 per-route 在途计数与 `GET /api/inflight` SSE 端点](../admin/changes/0003-per-route-in-flight-counters-sse.md)（跨模块：入口 acquire + 流式 guard 移交 `StreamState`）
- [0007 流式终点帧竞态误分类修复](changes/0007-stream-terminal-frame-race.md) — 终点帧转发后的客户端断连改记成功（499 计数下降、成功计数上升），并补 drop-as-success 行的 usage / TTFT / upstream 时长，以及三条 translated 终点帧桥接（`[DONE]` / `message_stop` / `response.completed`）与不合规上游的块闭合 / `stop_reason`
- [0006 流式请求超时边界修复](changes/0006-stream-request-timeout-boundary.md) — 成功流式响应不再受共享 client 总 timeout 限制，保留非流式 request-level、流式 TTFB 和 chunk idle timeout
- [0001 4xx Failover 归因策略](../failover-4xx-policy/changes/0001-4xx-attribution-policy.md)（跨模块：4xx 归因三分 + 403-quota 重分类）
- [0004 上游 4xx 不再触发重试与熔断](changes/0004-upstream-4xx-non-retryable.md)
- [0003 失败路径补全 reasoning_effort 记录](changes/0003-reasoning-effort-failure-paths.md)
- [0002 路由删除后缓存泄漏修复](changes/0002-cache-leak-fix.md)
- [0001 初始化](changes/0001-initial.md)
