---
created: 2026-06-03
updated: 2026-07-22
---

# 调度

## 目标

协调路由选择、供应商调用、重试退避和流式处理的请求分发逻辑。

## 预期行为

### 分发流程

1. 接收客户端请求（ChatCompletionRequest / ImageGenerationRequest）
2. 检查 `routes_version`，仅在路由集发生变更时执行惰性缓存清理（`prune_stale_entries`），移除 config 路由中已不存在的 model_name 对应的 `fill_first_orders`、`route_counters`、`active_targets` 条目；版本未变则跳过，避免不必要的写锁竞争
3. 匹配路由 → 按策略排序 targets
4. 对每个 target，反复尝试至重试耗尽：
   a. 检查熔断器状态（冷却中则跳过）
   b. 检查 provider 是否 enabled
   c. 调用 provider（chat / chat_stream / chat_image）
   d. 成功 → 返回响应
   e. 失败 → 根据错误类型决定重试或 fallthrough：
      - **non-retryable 4xx**（除 408 超时、429 限流外）：不重试、不开熔断，直接 failover 到下一 target
      - **retryable 错误**（408/429/5xx/超时/网络错误）：按退避策略重试，耗尽后开熔断（429 走 `CircuitHint::RateLimited` 短冷却路径）
5. 全部 targets 耗尽 → 返回聚合错误

**失败路径**在写入 `call_logs` 时携带 `request.reasoning_effort`，确保失败请求的推理意图可被审计——与成功路径一致。图片生成路径传 `None`（不适用）。

### 重试退避

每次重试前等待指数退避延迟：
- 公式：`min(base_ms * 2^attempt, max_ms)`
- `attempt` 从 0 计数（第 0 次重试前等待 `base_ms`）
- `jitter = true` 时：`delay = random(0, computed_cap)`（防 thundering herd）
- `jitter = false` 时：`delay = computed_cap`（确定性，便于测试）

### 上游 4xx 错误分类

上游返回的 HTTP 4xx 状态码按以下规则分类，决定重试与熔断行为：

| 状态码 | 分类 | 行为 |
|--------|------|------|
| 408 Request Timeout | 可重试 | 按退避策略重试，耗尽后开熔断 |
| 429 Too Many Requests | 可重试（短冷却） | 走 `CircuitHint::RateLimited` 短冷却路径，不按常规退避重试 |
| 400/403/422 等其余 4xx | 不可重试 | 不重试、不开熔断，立即 failover 到下一 target |

**原则**：上游返回的业务性 4xx（请求被拒）意味着请求体本身有问题，重试相同请求体不会改变结果；而 408 表示上游可能还活着但暂时忙不过来，429 表示上游限流——这两类值得重试或冷却等待。

### 流式处理

- SSE chunk 通过 tokio channel（`mpsc::unbounded`）从 provider 传给 axum streaming body
- 非流式错误在 channel 中传递为 error chunk
- `chunk_timeout_secs` 通过 `tokio::time::timeout` 实现 chunk 间超时

### 图片生成

- 调用 provider 的 `chat_image()` 方法
- 响应只取 `data[0]`（单张图片），`created` 取 `SystemTime::now()` 或上游原值

### 超时错误区分

网关在供给上游响应时发生的超时与"上游不可达"是不同的失败模式，需要可观测：

- **504 vs 502 区分**：504 `GATEWAY_TIMEOUT` 表示上游可能还活着但响应太慢；502 `BAD_GATEWAY` 表示上游返回无效响应或完全无法通信
- **超时分类**：

| 超时类型 | 发生层 | 触发机制 | HTTP 状态码 | 错误码 |
|---------|--------|---------|------------|--------|
| 请求总超时（非流式） | `ProviderBase::send_json` | reqwest `Client::timeout` 或 request 级 `timeout()` | **504** | `UPSTREAM_TIMEOUT` |
| 流式初始超时 | `ProviderBase::send_json_stream` | 同上 | **504** | `UPSTREAM_TIMEOUT` |
| chunk 空闲超时 | provider `chat_stream` spawned task | `tokio::time::timeout` 包裹 `byte_stream.next()` | **504**（CallLog） | `STREAM_CHUNK_TIMEOUT` |

- **检测方式**：`ProviderBase::send_json` / `send_json_stream` 中利用 reqwest 的 `is_timeout()` 方法区分超时与连接拒绝/DNS 失败/TLS 错误
- **重试行为**：`UpstreamTimeout` 是 retryable 错误——超时后重试同一 provider 是合理的（可能只是那次请求排队太久）
- **CallLog 记录**：超时错误在 `call_logs` 中记录 `status_code = 504`（非流式）或 `504`（流式 chunk 超时，HTTP 响应头已发送后记录到 analytics）。`error` 字段包含超时类型和阈值
- **流式 chunk 超时特殊处理**：HTTP 响应头（200 + `text/event-stream`）已发送给客户端后无法再改状态码，通过 SSE error 事件（`STREAM_CHUNK_TIMEOUT` 错误码）通知客户端

## 配置 Schema

参见 [配置 — 重试退避](/guide/configuration#重试退避)

## 功能边界

### 包含
- 请求分发（chat / stream / image）
- 指数退避重试
- 流式 SSE 透传
- 错误聚合（返回所有失败原因）
- 超时错误的 504 vs 502 区分
- 流式 chunk 超时的 SSE error 通知

### 不包含
- 请求排队 / 优先级队列
- 并发控制 / 信号量限制
- 请求缓存（由上游 Anthropic cache_control 处理）
- 流式请求的总时长上限（当前仅 chunk 间空闲超时）
- 超时错误的告警/通知（属于外部监控系统职责）

## 与其它模块的交互

- **路由**：提供 ordered targets
- **供应商**：调用 provider trait 方法
- **熔断器**：查询/更新熔断器状态
- **分析**：异步记录调用日志

## 变更记录

- [0004 上游 4xx 不再触发重试与熔断](changes/0004-upstream-4xx-non-retryable.md)
- [0003 失败路径补全 reasoning_effort 记录](changes/0003-reasoning-effort-failure-paths.md)
- [0002 路由删除后缓存泄漏修复](changes/0002-cache-leak-fix.md)
- [0001 初始化](changes/0001-initial.md)
