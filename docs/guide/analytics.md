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

> **注意**：该脚本是「库里还没有冷表 / 视图」时代的一次性迁移，它 `DROP TABLE call_logs` 并改名 `call_logs_v2`。已经跑过保留窗口轮转的库（存在 `call_logs_arch_*` 冷表与 `call_logs_hot` / `call_logs_all` 视图）**不要**直接执行——视图定义与历史数据都在 `call_logs` 这个名字上（见下文「冷表只增不改」与 changefile 的迁移禁忌）。

迁移逻辑：
- `INTEGER` / `REAL` 类型直接使用
- 数字格式的 `TEXT`（如 `"1735689600000"`）通过 `CAST` 转换
- ISO 8601 格式的 `TEXT`（如 `"2025-01-01T00:00:00Z"`）通过 `strftime('%s')` 转为秒后乘 1000
- 无法解析的值会违反 `NOT NULL` 约束，事务自动回滚

## 诊断接口

读 `call_logs` 明细的 HTTP 面只剩两个端点，它们都是**诊断面**（人工排错用），不是分析面：

| 端点 | 返回 | 口径 |
|---|---|---|
| `GET /v1/analytics/stats` | 按 `(provider, model, real_model)` 分组的聚合行 | 热窗口 · 近似 |
| `GET /api/logs` | 明细分页（`{"data": [CallLog...], "total": n}`，`offset` / `limit` 默认 50，支持 `provider` / `model` / `real_model` / `status_code` 过滤） | 热窗口 · 可丢行 |

### GET /v1/analytics/stats

查询参数 `provider`、`model`、`real_model` 均可选（空串视同未传）。响应是 `data` 数组，**每组一行**：

```json
{
  "data": [
    {
      "provider": "openai",
      "model": "gpt-4o",
      "real_model": "gpt-4o-2024-08-06",
      "call_count": 100,
      "total_prompt_tokens": 5000,
      "total_completion_tokens": 3000,
      "total_tokens": 8000,
      "avg_duration_ms": 1200.5,
      "error_count": 2
    }
  ]
}
```

无数据时 `data` 为空数组。前端**不再调用**此端点（分析页全部走 `/v1/analytics/v2/*`），保留它是给 `curl` / 脚本排错用的。

已知不精确性（**不修**，属诊断口径本身）：

- Token / 缓存列为 `NULL` 时按 `0` 计入求和（`COALESCE(SUM(...), 0)`），无法区分「未知」与「零」。
- `error_count` 以 `error` 文本非空为准，不是最终状态分类：`client_error` / `no_route` / `all_disabled` / `client_cancelled` 没有分母剔除规则，不能当失败率用。
- 一行 = 一条 `call_logs` 行；一请求多行（重试 / fallback / 取消流）就多行。
- 写入链路可丢弃（`try_send` 队列满即丢），统计可能比实际请求少。
- 只覆盖热窗口，且**随时间变化**（见下文「保留窗口与冷热轮转」）。

## 诊断口径 vs 分析口径

SAG 有两套互不兼容的数字口径。**它们对不上是预期行为，不是 bug。**

| 维度 | 诊断口径（`/v1/analytics/stats`、`/api/logs`） | 分析口径（`/v1/analytics/v2/summary\|trend`） |
|---|---|---|
| 数据源 | `call_logs_hot`（明细） | `request_results` / `target_attempts`（+ 1h Rollup） |
| 写入 | 可丢弃（队列满即丢） | 不丢弃（背压 + 关停排空 + 单请求原子） |
| 时间覆盖 | 热窗口（默认 90 天），随每日轮转缩水 | 全历史 |
| 计数 | 一行 = 一条日志行 | route 级一请求一条、target 级一尝试一条 |
| `NULL` | 当 `0` | 未知，单独计数，不进请求命中率分母 |
| 失败率 | `error_count / call_count`，无分母剔除 | 按最终状态分类，分母明确 |
| 稳定性 | 同一查询跨日期可能不同 | 同一区间稳定可复现 |
| 适合 | 看错误文本、TTFT、丢包摘要、逐条时间线 | 验收 / 报表：失败率、延迟分位、Token 与缓存命中率 |

选端点的简单规则：

- 要**验收数字**（成功率、P95、缓存命中率）→ `/v1/analytics/v2/summary` / `/v1/analytics/v2/trend`。
- 要**按时间看趋势** → `v2/trend`（自定义桶宽）；旧 `GET /v1/analytics/trend` 已下线。
- 要**按 provider / real_model 看分布与占比** → `v2/summary?scope=target` 的 `groups`；旧 `GET /v1/analytics/distribution`、`GET /v1/analytics/cache` 已下线。
- 要**看一条请求到底错在哪**（上游错误文本、TTFT、丢包摘要）→ `/api/logs`，或在库里查 `call_logs_all`。

跨口径 / 跨冷热表追溯只能用 `request_id`（UUID v4，全局唯一）：`call_logs.id` 在每次轮转后从 1 重置，不能做关联键。

## 保留窗口与冷热轮转

`call_logs` 默认只保留 **90 天**热数据（`[analytics] call_log_retention_days`，`0` = 禁用）。更早的行不会被删除：整张热表会被转成冷表，历史数据仍可用 SQL 查。

### 冷表命名与粒度

- 冷表命名 `call_logs_arch_<UTC yyyyMMdd_HHmmss>`，例 `call_logs_arch_20261017_031502`；同一秒内发生多次轮转会追加 `_1`、`_2`。
- 90 天是 **UTC 滚动窗口**，不是日历月 / 日历日：切分点是「轮转那一刻往前 90 天」，轮转发生在启动首轮与每日维护巡检，不是每月 1 日。
- 一次轮转搬走的是**整张热表**（约等于 retention 天的量），所以冷表是**约 90 天粒度的批次**（≈ 6 万行 / 12MB），不是日表；下一次轮转大约在 90 天之后。

### 两个视图

| 视图 | 内容 | 用途 |
|---|---|---|
| `call_logs_hot` | 热表 + 仍有窗内行的冷表，每个分支都带 `WHERE timestamp >= cutoff` | 诊断接口（`/api/logs`、`/v1/analytics/stats`）的查询面 |
| `call_logs_all` | 热表 + 全部冷表，无时间过滤 | 历史明细 SQL 查询（不接 HTTP 接口） |

两个视图由 gateway 自动维护（启动时、每日巡检、每次轮转后重建），**不要手工 `DROP VIEW` / `CREATE VIEW`**。`call_log_retention_days = 0` 时两个视图都覆盖全史，与启用前语义一致。

### 查询示例

列出所有冷表（新的在前）：

```sql
SELECT name FROM sqlite_master
 WHERE type = 'table' AND name LIKE 'call_logs\_arch\_%' ESCAPE '\'
 ORDER BY name DESC;
```

单张冷表的规模与时间范围（表名换成上一步查到的）：

```sql
SELECT COUNT(*) AS rows,
       datetime(MIN(timestamp) / 1000, 'unixepoch') AS oldest_utc,
       datetime(MAX(timestamp) / 1000, 'unixepoch') AS newest_utc
  FROM "call_logs_arch_20261017_031502";
```

热窗口翻页（等价于 `/api/logs` 的查询形状）：

```sql
SELECT id, request_id, provider, model, status_code, duration_ms,
       datetime(timestamp / 1000, 'unixepoch') AS ts_utc
  FROM call_logs_hot
 ORDER BY timestamp DESC
 LIMIT 50 OFFSET 0;
```

按窗口过滤全史（不依赖视图里内联的 cutoff 字面量）：

```sql
SELECT COUNT(*) AS n
  FROM call_logs_all
 WHERE timestamp >= CAST(strftime('%s', 'now', '-90 days') AS INTEGER) * 1000;
```

按 `request_id` 追溯一条请求（`id` 每张表都从 1 重置，跨表只能用 `request_id`）：

```sql
SELECT 'call_logs_all' AS src, id, request_id, provider, model, status_code,
       datetime(timestamp / 1000, 'unixepoch') AS ts_utc
  FROM call_logs_all
 WHERE request_id = '<uuid>';
```

### 冷表只增不改

冷表是轮转当天的 schema 快照，**唯一允许的写操作是 `DROP INDEX`（索引退役）**：

- 不要 `ALTER` 冷表、不要 INSERT / UPDATE / DELETE、不要改名；
- 不要对单张冷表 `VACUUM`（要回收空间就按下一节整库 `VACUUM`）；
- 新加的列只改热表，旧冷表缺的列由视图以 `NULL` 补齐，查询面看到的永远是热表当前形状。

### 空间回收（手动 VACUUM）

轮转只做 DDL，不会缩小库文件；索引退役释放的页会被后续写入复用，但文件本身不会自动变小。需要真正回收磁盘时：

```bash
# 1) 先停 gateway：VACUUM 需要独占访问，有并发读写会报 database is locked
just test-env stop        # 测试轨；开发轨停掉 `just dev` 实例

# 2) 把 WAL 内容并回主库并截断 WAL 文件
sqlite3 ~/.local/share/sag/analytics.db "PRAGMA wal_checkpoint(TRUNCATE);"

# 3) 重整主库文件
sqlite3 ~/.local/share/sag/analytics.db "VACUUM;"

# 4) 可选：校验完整性
sqlite3 ~/.local/share/sag/analytics.db "PRAGMA integrity_check;"
```

注意：

- `VACUUM` 会写出一份新库再替换原文件，**需要约 2 倍库文件大小的空闲磁盘余量**，库大时耗时可达分钟级。
- `VACUUM` 不会删除任何数据，`call_logs` 与各冷表的行数都不变，只重整页。
- 测试轨的库在 `.test-env/xdg/data/sag/analytics-test.db`，把路径换掉即可。

### 逃生舱：冷表派生名索引

冷表出生即无索引，`call_logs_hot` 的冷表分支是全表扫描。若查询量级增长到分支扫描不可接受，可手工给某张冷表建**派生名**索引：

```sql
CREATE INDEX "call_logs_arch_20261017_031502_ts"
    ON "call_logs_arch_20261017_031502" (timestamp);
```

索引名**必须避开** 5 个标准名（`idx_call_logs_ts` / `idx_call_logs_provider_ts` / `idx_call_logs_model_ts` / `idx_call_logs_real_model` / `idx_call_logs_status_ts`）：索引名是 schema 全局命名空间，若冷表占用了标准名，下一次轮转回放 `CREATE INDEX IF NOT EXISTS …` 时会被静默跳过，保真校验随即失败、整轮轮转回滚（日志里会出现 `rotation failed; retrying next cycle`）。这是应急手段，默认不加：稳态窗口约 6 万行，扫描 + 排序在 10–30ms 量级。
