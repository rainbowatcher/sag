# 分析

SAG 将每次 API 调用记录到 SQLite，用于使用追踪和调试。

## 表结构

`call_logs` 表存储以下字段：

| 字段 | 类型 | 说明 |
|--------|------|------|
| `id` | TEXT | UUID v4 |
| `timestamp` | TEXT | ISO 8601 时间戳 |
| `provider` | TEXT | 供应商名称 |
| `model` | TEXT | 模型名称 |
| `request_tokens` | INTEGER | 输入 Token 数 |
| `response_tokens` | INTEGER | 输出 Token 数 |
| `duration_ms` | INTEGER | 请求耗时 |
| `status` | TEXT | success 或 error |
| `error` | TEXT | 错误信息（如果有） |

## 统计接口

`GET /v1/analytics/stats` 返回聚合的使用统计数据：

```json
{
  "total_calls": 1234,
  "total_tokens": 50000,
  "total_duration_ms": 120000,
  "error_rate": 0.02,
  "by_provider": { ... },
  "by_model": { ... }
}
```
