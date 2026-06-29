---
created: 2026-06-03
updated: 2026-06-03
---

# 分析 — 需求

## 目标

异步记录每次 API 调用到 SQLite，提供聚合统计查询，支持按供应商和模型过滤。

## 预期行为

### 调用日志

每次 API 调用（无论成功/失败）异步写入 `call_logs` 表：
- 记录 request_id（UUID v4）、provider、model、real_model、stream 标志
- 记录 HTTP status_code、duration_ms、upstream_duration_ms、ttft_ms
- 记录 token 用量（prompt/completion/total/cached/cache_creation/reasoning）
- 失败时记录 error 信息
- timestamp 使用 Unix 毫秒（INTEGER 类型）

### 统计聚合

`GET /v1/analytics/stats` 返回聚合数据：
- 总调用次数、总 token 数、总耗时
- 错误率
- 按 provider 分组统计
- 按 model 分组统计
- 支持 `?provider=` 和 `?model=` 查询参数过滤

### 异步写入

调用日志通过 `tokio::sync::mpsc` channel 异步发送给后台写入任务：
- 不阻塞请求处理
- channel 在 server shutdown 时关闭，后台任务等待所有 pending 写入完成后退出

## 配置 Schema

```toml
[analytics]
enabled = true
storage_dsn = "sqlite://analytics.db"   # 或 ":memory:"
```

## 功能边界

### 包含
- API 调用日志记录
- Token 用量统计
- 聚合查询 API
- SQLite 持久化

### 不包含
- 实时 dashboard（通过管理面板的定时轮询实现）
- 数据导出
- 日志级别过滤（所有调用都记录）
- PostgreSQL / MySQL 支持（仅 SQLite）

## 与其它模块的交互

- **调度**：每次调用完成后发送 `CallLog` 到 channel
- **管理面板**：调用 stats API 展示图表

## 变更记录

- [0001 初始化](changes/0001-initial.md)
