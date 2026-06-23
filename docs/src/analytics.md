# 分析

SAG 将每次 API 调用记录到 SQLite，用于使用追踪和调试。

## 表结构

`call_logs` 表存储以下字段：

| 字段 | 类型 | 说明 |
|--------|------|------|
| `id` | INTEGER | 自增主键 |
| `request_id` | TEXT | UUID v4 |
| `provider` | TEXT | 供应商名称 |
| `model` | TEXT | 请求模型名称 |
| `real_model` | TEXT | 实际调用模型名称 |
| `stream` | INTEGER | 是否流式请求 |
| `status_code` | INTEGER | HTTP 状态码 |
| `duration_ms` | INTEGER | 总请求耗时 (ms) |
| `prompt_tokens` | INTEGER | 输入 Token 数 |
| `completion_tokens` | INTEGER | 输出 Token 数 |
| `total_tokens` | INTEGER | 总 Token 数 |
| `error` | TEXT | 错误信息（如果有） |
| `timestamp` | INTEGER | Unix 毫秒时间戳 |
| `reasoning_effort` | TEXT | 推理 effort |
| `upstream_duration_ms` | INTEGER | 上游请求耗时 (ms) |
| `ttft_ms` | INTEGER | 首 token 延迟 (ms) |
| `cached_tokens` | INTEGER | 命中缓存的 Token 数 |
| `cache_creation_tokens` | INTEGER | 创建缓存的 Token 数 |
| `reasoning_tokens` | INTEGER | 推理 Token 数 |

> **注意**: `timestamp` 列的类型为 `INTEGER`，存储 Unix 毫秒时间戳，不允许为 `NULL`。  
> 不要将 `timestamp` 改为 `TEXT` 或 `REAL` 类型，否则会导致 `sqlx` 反序列化失败。

## 数据库迁移

### TEXT → INTEGER timestamp 迁移

如果 `timestamp` 列的类型为 `TEXT`（从早期版本遗留），启动时 `query_logs` 会报错：

```
error occurred while decoding column "timestamp": mismatched types;
Rust type `i64` (as SQL type `INTEGER`) is not compatible with SQL type `TEXT`
```

执行以下 SQL 进行一次性迁移（迁移前建议备份 `analytics.db`）：

```sql
BEGIN;

CREATE TABLE call_logs_v2 (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id          TEXT    NOT NULL,
    provider            TEXT    NOT NULL DEFAULT '',
    model               TEXT    NOT NULL DEFAULT '',
    real_model          TEXT    NOT NULL DEFAULT '',
    stream              INTEGER NOT NULL DEFAULT 0,
    status_code         INTEGER NOT NULL DEFAULT 0,
    duration_ms         INTEGER NOT NULL DEFAULT 0,
    prompt_tokens       INTEGER,
    completion_tokens   INTEGER,
    total_tokens        INTEGER,
    error               TEXT,
    timestamp           INTEGER NOT NULL,
    reasoning_effort       TEXT,
    upstream_duration_ms   INTEGER,
    ttft_ms                INTEGER,
    cached_tokens          INTEGER,
    cache_creation_tokens  INTEGER,
    reasoning_tokens       INTEGER
);

INSERT INTO call_logs_v2
SELECT
    id, request_id, provider, model, real_model,
    stream, status_code, duration_ms,
    prompt_tokens, completion_tokens, total_tokens,
    error,
    CASE
        WHEN typeof(timestamp) = 'integer' THEN timestamp
        WHEN typeof(timestamp) = 'real'   THEN CAST(timestamp AS INTEGER)
        ELSE COALESCE(
            CAST((strftime('%s', timestamp) * 1000) AS INTEGER),  -- ISO 8601 → millis
            CAST(timestamp AS INTEGER)                              -- 纯数字 TEXT → millis
        )
    END,
    reasoning_effort, upstream_duration_ms, ttft_ms,
    cached_tokens, cache_creation_tokens, reasoning_tokens
FROM call_logs;

DROP TABLE call_logs;
ALTER TABLE call_logs_v2 RENAME TO call_logs;

CREATE INDEX IF NOT EXISTS idx_call_logs_ts           ON call_logs (timestamp);
CREATE INDEX IF NOT EXISTS idx_call_logs_provider_ts  ON call_logs (provider, timestamp);
CREATE INDEX IF NOT EXISTS idx_call_logs_model_ts     ON call_logs (model, timestamp);
CREATE INDEX IF NOT EXISTS idx_call_logs_real_model   ON call_logs (real_model, timestamp);

COMMIT;
```

迁移逻辑：
- `INTEGER` / `REAL` 类型直接使用
- 数字格式的 `TEXT`（如 `"1735689600000"`）通过 `CAST` 转换
- ISO 8601 格式的 `TEXT`（如 `"2025-01-01T00:00:00Z"`）通过 `strftime('%s')` 转为秒后乘 1000
- 无法解析的值会违反 `NOT NULL` 约束，事务自动回滚

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
