---
created: 2026-06-03
updated: 2026-06-03
---

# 调度 — 需求

## 目标

协调路由选择、供应商调用、重试退避和流式处理的请求分发逻辑。

## 预期行为

### 分发流程

1. 接收客户端请求（ChatCompletionRequest / ImageGenerationRequest）
2. 匹配路由 → 按策略排序 targets
3. 对每个 target，反复尝试至重试耗尽：
   a. 检查熔断器状态（冷却中则跳过）
   b. 检查 provider 是否 enabled
   c. 调用 provider（chat / chat_stream / chat_image）
   d. 成功 → 返回响应
   e. 失败 → 根据错误类型决定重试或 fallthrough
4. 全部 targets 耗尽 → 返回聚合错误

### 重试退避

每次重试前等待指数退避延迟：
- 公式：`min(base_ms * 2^attempt, max_ms)`
- `attempt` 从 0 计数（第 0 次重试前等待 `base_ms`）
- `jitter = true` 时：`delay = random(0, computed_cap)`（防 thundering herd）
- `jitter = false` 时：`delay = computed_cap`（确定性，便于测试）

### 流式处理

- SSE chunk 通过 tokio channel（`mpsc::unbounded`）从 provider 传给 axum streaming body
- 非流式错误在 channel 中传递为 error chunk
- `chunk_timeout_secs` 通过 `tokio::time::timeout` 实现 chunk 间超时

### 图片生成

- 调用 provider 的 `chat_image()` 方法
- 响应只取 `data[0]`（单张图片），`created` 取 `SystemTime::now()` 或上游原值

## 配置 Schema

参见 [配置 — 重试退避](/guide/configuration#重试退避)

## 功能边界

### 包含
- 请求分发（chat / stream / image）
- 指数退避重试
- 流式 SSE 透传
- 错误聚合（返回所有失败原因）

### 不包含
- 请求排队 / 优先级队列
- 并发控制 / 信号量限制
- 请求缓存（由上游 Anthropic cache_control 处理）

## 与其它模块的交互

- **路由**：提供 ordered targets
- **供应商**：调用 provider trait 方法
- **熔断器**：查询/更新熔断器状态
- **分析**：异步记录调用日志

## 变更记录

- [0001 初始化](changes/0001-initial.md)
