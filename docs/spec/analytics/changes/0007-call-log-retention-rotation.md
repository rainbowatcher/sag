---
created: 2026-10-17
updated: 2026-10-17
type: 新增
---

# 0007 — call_logs 保留窗口 + 表轮转（热窗口视图）

## 动机

`call_logs` 是 append-only 表，此前**没有任何保留期 / 清理 / 归档机制**，数据单调增长。实测开发库：3.5 个月 68,846 行（≈ 2 万行/月、660 行/天），主库文件 77MB。按此速率主库文件会持续膨胀，备份、`VACUUM`、页缓存全部跟着变大，而明细的排查价值集中在近期窗口（`error` 文本 / `ttft_ms` / `loss_summary` 只有明细能提供）；超期数据的价值只剩统计，而统计已由正式 analytics（`request_results` / `target_attempts` + 1h Rollup）承担。

本变更给 `call_logs` 加保留窗口，并把超窗数据以**同库冷表**的形式留下（用户要求历史数据保留参考价值、不用外部归档文件），查询面通过两个视图维持「近 N 天可见 + 全史可 SQL 查」。

## 消费方画像（代码证据）

全仓 grep `FROM call_logs` 核实，消费方只有以下几处：

| 消费方 | 查询模式 | 轮转后的行为 |
|---|---|---|
| 日志页 `GET /api/logs` | `query_logs` + `count_logs`，**无时间过滤**，恒定 `ORDER BY timestamp DESC LIMIT/OFFSET` | 只消费最新页；`total` 在超窗数据退出视图时递减；翻页途中轮转会让 offset 漂移（低危 UX 瑕疵，不修） |
| 旧统计 `GET /v1/analytics/stats` | `query_stats`，可选 from/to **在 store 层存在但 HTTP 层不暴露**（`StatsQuery` 只有 provider / model / real_model） | 聚合数字从「全生命周期」静默变「热窗口」 |
| 旧统计 `GET /v1/analytics/trend` | `query_trend`，强制 `from`（毫秒） | `from < cutoff` 的图表断崖：旧区间 bucket 清零 |
| 旧统计 `GET /v1/analytics/cache`、`/distribution` | `query_cache_stats` / `query_distribution`（后者复用 `query_stats`） | 同 stats：热窗口语义。前端已全部迁 V2 不再调用，但接口仍对外暴露 |
| 正式 analytics（facts + rollup） | `request_results` + 1h rollup | **零影响**（完全不读 `call_logs`） |

两条结论写死：

- **无任何 JOIN 消费方**：`call_logs` 只在单表聚合 / 单表分页里出现，facts / rollup 链路与它完全正交，因此「表改名 + 视图并回」不会波及任何 JOIN 计划。
- **无按 `id` / `request_id` 精确查单条的端点**：轮转不会让任何历史详情页 404，也没有任何调用方依赖 `id` 的全局唯一性。

## 方案对比与否决理由

存储形态选**同库冷热分层**。评估过的替代方案：

| 方案 | 否决理由 |
|---|---|
| 简单 `DELETE` | 用户要求历史数据保留参考价值 |
| JSONL.zst 月度归档文件 | 分析摩擦大：每次分析要解压 + 写脚本 + jq；此量级下 SQL 可查性 > 空间极限优化 |
| 独立 SQLite 月度文件 | 可 SQL 查，但需管理多文件 + 每连接 `ATTACH` 重放 / 查询切库，属过度设计 |
| `call_logs_daily` 摘要表 | 与 facts + rollup 在长程统计上重复建设，需维护两套聚合口径 |
| v2 的「双表 + `DELETE … RETURNING` 行级搬运」 | 见下 |

**v2 行级搬运被否决的具体理由**：它是一条自造的数据路径——① 需要自己维护行映射（`RETURNING` 列清单与 `INSERT` 列清单必须同源，加列即双处改）；② 要绕开 SQLite 绑定参数上限（5000 行 × 28 列 = 14 万参数，只能分片）；③ 冷热两表 `id` 空间冲突需要专门处理（PK 冲突）；④ 因此背上**双表 schema 永久同步义务**（未来每次给 `call_logs` 加列的迁移都得同步 ALTER 冷表，漏改就每轮归档必败）。这些风险是「搬行」这个动作本身带来的，不是需求带来的；换来的收益只有「逐行精确的窗口边界」和「查询仍直查单表」，而消费方画像显示查询以「看最新页」为主，边际价值很小。

**表轮转是纯 DDL，没有数据路径**：整表改名 + 空表重建，拿到 90% 的价值，代价是接受主库文件缓慢增长（冷表索引退役后约 200B/行，几十 MB/年）。

## 轮转机制

**触发**：调度器每日一轮（`ROTATION_INTERVAL = 24h`，`MissedTickBehavior::Skip`），**spawn 后的第一动作是完整一轮**（不等 tick）。每轮先算 `cutoff_ms = now_ms - retention_days * 86_400_000`，然后探测 `SELECT MIN(timestamp) FROM call_logs`；`MIN(timestamp) < cutoff_ms` 才轮转，热表为空或全部在窗内则本轮跳过。`retention_days == 0` 整轮直接 return（不轮转、不退役、不收窗）。

**轮转 = 单写事务内 6 步 DDL**（`rotate_once(pool, cutoff_ms)`，`BEGIN IMMEDIATE`）：

```
⓪ DROP VIEW IF EXISTS call_logs_hot / call_logs_all
① 从 sqlite_master 捕获 table DDL + 全部 index DDL（此刻原文仍引用 call_logs）
② ALTER TABLE call_logs RENAME TO "call_logs_arch_<UTC yyyyMMdd_HHmmss>"
③ DROP 冷表从 ② 继承来的全部索引（释放索引名）
④ 原样回放 ① 捕获的 DDL（重建空 call_logs + 5 个标准索引）
⑤ assert_replay_faithful：列形状 + 索引集合 + 冷表零索引，任一不符整事务回滚
⑥ build_views_in_tx(Some(cutoff))：同一事务内重建两个视图
COMMIT
```

要点：

- **零字符串替换**：「事前捕获 + 原样回放」——捕获时 DDL 原文引用的表名本来就是 `call_logs`，回放恰好重建同名新表，实现里没有任何 `replace` 调用，也就不存在误伤 `idx_call_logs_ts` 之类 token 的可能。
- **列真相只有 `sqlite_master`**：SQLite 的 `ALTER TABLE … ADD COLUMN` 会改写 `sqlite_master.sql` 里存的 `CREATE TABLE` 文本，所以捕获到的 `table_ddl` 恒等于当前 28 列全集（含后加列）。「加列不用改轮转代码」由此成立。
- **`AUTOINCREMENT` / `NOT NULL` / `DEFAULT` / `PRIMARY KEY` 原样保留**；`call_logs` 无外键、无 CHECK、无触发器，回放无连带对象。
- **冷表命名**：`call_logs_arch_<UTC yyyyMMdd_HHmmss>`（秒精度，同秒冲突加 `_1`、`_2`…）。视图成员枚举用同一个 `ARCH_PREFIX` 常量匹配，两边不会漂移；所有插值标识符仍走 `quote_ident` 双引号引用。
- **事务形态**：`pool.begin_with("BEGIN IMMEDIATE")`（全库同款；`pool.begin()` 是 DEFERRED，不用）。任何一步失败整轮回滚，库回到轮转前状态。
- **维护任务永不 panic / 永不让 gateway 启动失败**：DB 错误 warn + 下轮重试。

**每轮顺序**：`retire_indexes` → `rotate_once` → `refresh_views`。`refresh_views` 无条件跑（轮转未发生也要推进视图里的 cutoff 字面量，这正是「每日重建」的原因）；轮转失败则提前返回（视图未被破坏，下轮重试两步）。

## 两个视图的语义

两个视图是 store 的查询面，共用 `build_views_in_tx(tx, Option<i64>)` 构建器：

**`call_logs_hot`**（旧接口查询面，热窗口）：

- 成员：`call_logs` 恒在（恒为第一分支）+ `call_logs_arch_*` 中满足 `MAX(timestamp) >= cutoff` 的冷表（按表名降序）。成员判定用 **MAX**（宽，不漏掉部分过窗表里窗内的那部分）。
- 行过滤：**每个分支**都带 `WHERE timestamp >= <cutoff>`（字面量内联，视图不能绑参数），含热表分支（热表里也可能有旧行）。成员判定宽 + 行过滤准，两层合起来把部分过窗表的窗口偏差收干净。
- 列：以热表 `pragma_table_info('call_logs')` 的 **cid 序**为唯一真相；冷表缺的列写 `NULL AS "<col>"`，多出的列忽略。各分支列数 / 列序恒等，视图列名取第一分支（热表）的真实列名——这也是热表必须排第一的原因。

**`call_logs_all`**（历史分析面）：热表 + **全部**冷表，同样 NULL 补齐，**不带 timestamp 过滤**。不接任何 HTTP 接口，只供直连 SQL。

**原子换**：轮转事务内 ⓪ 先 DROP、⑥ 再 CREATE（同事务）；独立的 `refresh_views` 自己开 `BEGIN IMMEDIATE` → DROP → CREATE → commit。WAL 下读者要么看到旧视图、要么看到新视图，**不存在「视图缺失」或「视图漏掉刚轮转数据」的窗口**。

**`retention_days == 0`**：`cutoff = None`，两个视图同构——成员 = 全部表、无过滤，即「全史可见」，与启用前的语义一致。0 是「功能关闭」，不是「保留 0 天」。

**视图列形状跟随热表**：任何改动 `call_logs` 列集合的操作之后必须重建视图（当前只有启动路径的 `ensure_schema_consistency` 会 ALTER 热表，它之后紧跟 `refresh_views(pool, None)`；轮转路径的回放文本已含全部列，事务内 ⑥ 步重建）。store 打开顺序不可颠倒：`migrate` → `ensure_schema_consistency`（先补列）→ `refresh_views(pool, None)`（后取 pragma 建视图），否则视图会把补列前的形状烘死。

## 索引退役契约

- `retire_indexes(pool, cutoff)` 的**契约**：对 `MAX(timestamp) < cutoff` 的冷表 `DROP` 其全部索引；**窗内冷表的索引一概不碰**。实现是通用巡检（枚举 `sqlite_master` 里 `tbl_name` = 冷表的全部 index 并 DROP），幂等，返回本轮 drop 的索引数。
- **冷表生而无索引**（本轮实现的既成事实）：索引名是 schema 全局命名空间，② RENAME 后冷表仍持有 `idx_call_logs_ts` 等 5 个原名索引；此时若先回放 `CREATE INDEX IF NOT EXISTS idx_call_logs_ts ON call_logs(…)`，SQLite 发现名字已存在会**静默跳过**（不报错）→ 新热表 0 索引。因此在「原样回放」约束下「冷表保留原名索引」物理不可能：必须 ③ 先 DROP 释放名字 → ④ 再回放。⑤ 的保真校验是第二道防线（热表索引集合 != 捕获集合 → 整轮回滚 + warn），所以这类顺序回归/外来同名索引不会静默上线，只会让轮转失败并下轮重试。稳态下 `retire_indexes` 因此是**幂等 no-op**，只扫手工加的索引；保留通用契约是为了不把「只 DROP 那 5 个名字」写死。
- **查询性能裁定**（`/api/logs` 的 `UNION ALL` 展开）：热表分支能吃到 `idx_call_logs_ts` 范围；冷表分支 0 索引 → 每分支全表扫描（`WHERE` 是逐行过滤器）。稳态视图行数 ≈ 660 行/天 × 90 天 ≈ 6 万行，轮转切换期最多热表 + 2 个窗内冷表 ≈ 10–12 万行，扫描 + 排序 ≈ 10–30ms，对人工点页的管理后台可接受。逃生舱是「冷表派生名索引」（见下文），本期不做。

## 语义变化声明

轮转上线后，**旧接口覆盖的是热窗口，不再是全生命周期**：

- `/api/logs` 的 `total` 会在超窗数据退出视图时递减；翻页途中轮转会让 `offset` 漂移（可能漏/重一页）。
- `query_stats` / `query_cache_stats` 的聚合从「全生命周期」变「热窗口」——**变化的不只是 `total`**。
- `query_trend`：`from < cutoff` 的图表在超窗数据退役后「断崖」，旧区间 bucket 清零；**同一个查询在不同日期可能返回不同数值**。
- 长程统计请用正式 analytics（facts + rollup）；历史明细请用 `call_logs_all` 的直连 SQL。
- 冷表不接任何现有接口。

## `id` 每表重置

`ALTER TABLE … RENAME` 会把 `sqlite_sequence` 里的行同步改名为冷表名（冷表保留历史最大 id），新热表首次 INSERT 时新建 sequence 行、**`id` 从 1 起**。因此 `id` 只在单表内唯一，**跨表追溯必须用 `request_id`**（UUID v4，全局唯一）。

## 边界结论

1. **启动瞬态**：store 打开路径的 `refresh_views(pool, None)` 只兜底视图存在性（全史形状，是正确窗口的**超集**——只会多看到历史，不会丢行、不会报错）；窗口正确性由 spawn 后立即跑的首轮调度轮负责，亚秒级收敛。不采用「同步阻塞启动路径 refresh 真实 cutoff」的变体：那会把维护 DDL 拖进启动关键路径并新增启动失败面。
2. **`ALTER TABLE … RENAME` 会隐式改写视图引用**（SQLite ≥ 3.25 起会把视图定义里对被改名表的引用一并改名）。因此 rotate 事务**开头先 DROP 两个视图**（⓪），**末尾同事务重建**（⑥），既不依赖这条隐式语义，又拿到零断档的换视图。
3. **sqlx 语句缓存**：DDL 提升 schema cookie 后，连接上缓存过的 prepared statement 再次使用可能冒 `database schema has changed`（`SQLITE_SCHEMA`）。查询层 5 个方法 + `insert_batch` 都包 `with_schema_retry`（**只重试一次**），仍失败则上抛（HTTP 500），**不 panic**。
4. **冷表是约 90 天粒度的批次，不是日表**：一次轮转搬走的是「整个热表 ≈ retention 天的量」，所以下一次轮转约在一个 retention 周期之后；日常轮次的职责是推进视图 cutoff 字面量 + 索引退役巡检。单个冷表最大 ≈ 6 万行 / 12MB 量级。若未来需要更细粒度，改触发条件即可（如阈值改 `now - 7d`），机制不变——本期不做。
5. **零 migration**：本特性不新增迁移文件。热表形状不变，视图与冷表都是**运行时 DDL 对象**。sqlx migrate 只按 `_sqlx_migrations`（文件名 + checksum）跟踪已执行脚本，不做 schema diff、不校验运行时对象，`call_logs_arch_*` 与两个视图的存在不影响任何迁移判定。
6. **并发边界**：`insert_batch` 与轮转都走 `BEGIN IMMEDIATE`，写写互斥；轮转事务是纯 DDL（毫秒级），插入方在 `busy_timeout=5s` 内排队，预期零 busy 错误。轮转 commit 后任何 `INSERT INTO call_logs` 都落新热表（语句按名字解析），冷表行只来自 RENAME。
7. **关闭顺序**：轮转调度器参与 graceful shutdown，接线点在 rollup 之后、event drain **之前**（**join 成功时**保证 in-flight DDL 事务已结束、event drain 仍是最后一个写库者；join 超时则 task 被 abort、其事务回滚，drain 可能与该回滚短暂并发，记 error 日志）。

## 迁移禁忌

**未来对 `call_logs` 的迁移不得写 `DROP TABLE call_logs`，也不得改表名**：`call_logs_hot` / `call_logs_all` 两个视图与全部 `call_logs_arch_*` 冷表的引用链都锚在 `call_logs` 这个名字上（视图体里是 `FROM "call_logs"`），DROP / RENAME 会让视图失效或把引用改写到别处。

安全形态：`ALTER TABLE call_logs ADD COLUMN …` 照旧安全——SQLite 会同步改写 `sqlite_master.sql` 里的建表文本，下一次轮转捕获到的 DDL 自然带上新列，冷表保持诞生时的快照形状，列差异由视图 `NULL AS` 消化。

## 运维出口与未来选项

本期**不实现**任何 admin 端点（冷表管理 / 手动轮转 / 导出 HTTP 接口）。运维出口是文档化的直连 SQL 与手动维护序列（见 [`docs/guide/analytics.md`](../../../guide/analytics.md)「保留窗口与冷热轮转」）：

- 历史明细查询：`call_logs_all`；
- 空间回收：`PRAGMA wal_checkpoint(TRUNCATE)` → `VACUUM`（约需 2 倍库文件磁盘余量，需无并发写）；
- **文件导出降级路径（预留）**：未来空间真成问题时，冷表可整表导出（`VACUUM INTO` / `.dump` / `ATTACH` 后拷表）再 DROP，即降级为外部归档；反向不成立（一旦降级为外部文件，重新并入要写导入路径）。这也是「同库冷表」优先于「外部文件」的理由之一。
- **冷表派生名索引（逃生舱，本期不做）**：若查询量级增长到分支全扫不可接受，可给冷表建 `call_logs_arch_<ts>_ts` 这类派生名索引（名字必须避开 5 个标准名，否则下一次轮转回放 `CREATE INDEX` 时名字已被占用，轮转会被保真校验拦下并反复失败）。仅在 change 文档记录为未来选项。

## 需求变更

- requirements.md「预期行为」新增「保留与轮转」节：热窗口语义、旧接口只覆盖热窗口的边界声明、旧统计随时间变化、`call_logs_all` 供历史分析、`retention_days` 配置语义（0 = 禁用）、`id` 每表重置 / `request_id` 追溯、冷表只增不改。
- requirements.md「配置 Schema」：`[analytics]` 增加 `call_log_retention_days`。
- requirements.md「兼容性说明（迁移前现状）」：旧 `/v1/analytics/*` 兼容说明补注「数据源为 `call_logs_hot` 视图，只覆盖热窗口」。
- requirements.md「变更记录」：追加 0007 条目。
- docs/guide/analytics.md：新增「保留窗口与冷热轮转」运维节，并在历史 TEXT → INTEGER 迁移脚本处补注「该脚本不适用于已含冷表 / 视图的库」。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-analytics/src/rotate.rs`](sag-analytics/src/rotate.rs) | 新增模块：`rotate_once`（探测 + 单事务 6 步 DDL）、`refresh_views` / `build_views_in_tx`（两视图构建，热表 cid 序 + `NULL AS` 补列）、`retire_indexes`（通用幂等索引退役巡检）、`unique_archive_name`（同秒去重）、`is_schema_change`（SQLITE_SCHEMA 分类）、`assert_replay_faithful`（回放保真校验）；常量 `HOT_TABLE` / `HOT_VIEW` / `ALL_VIEW` / `ARCH_PREFIX`；11 条内联单元测试 |
| [`sag-analytics/src/lib.rs`](sag-analytics/src/lib.rs) | 导出 `rotate` 模块与 `rotate_once` / `refresh_views` / `retire_indexes` / `is_schema_change` / 视图与表名常量；重新导出 `sqlx::sqlite::SqlitePool`，让 gateway 的维护任务不必直接依赖 sqlx |
| [`sag-analytics/src/sqlite_store.rs`](sag-analytics/src/sqlite_store.rs) | 5 个查询方法（`query_stats` / `query_logs` / `count_logs` / `query_cache_stats` / `query_trend`）由 `FROM call_logs` 改为 `FROM call_logs_hot`；新增 `with_schema_retry`（`SQLITE_SCHEMA` 只重试一次）并包住 5 个查询方法与 `insert_batch`；`new` / `from_pool` 在 `ensure_schema_consistency` 之后追加 `refresh_views(pool, None)`；新增 5 条内联测试（视图存在性、热表无冷表时读一致、全史兜底、retry 恰好一次、非 schema 错误不重试） |
| [`sag-gateway/src/main.rs`](sag-gateway/src/main.rs) | 新增 `spawn_rotation_scheduler` / `rotation_round` / `RotationSchedulerHandle`：首轮立即跑完整一轮（retire → rotate → refresh）、每轮热读配置（0 = 跳过）、24h interval + `MissedTickBehavior::Skip`、shutdown 走 oneshot + 5s join；`load_analytics_stack` 在 rollup 旁 spawn；`graceful_shutdown` 抽出 `shutdown_analytics_writers`，顺序为 rollup → rotation → event drain；新增 1 条 shutdown 顺序测试 |
| [`sag-analytics/tests/call_logs_rotation.rs`](sag-analytics/tests/call_logs_rotation.rs) | 新增 5 条集成测试（文件库 + 共享池）：启动积压首轮、并发写入轮转、视图原子性、`retention=0` 与热重载、轮转后旧统计的热窗口语义 |
| [`sag-gateway/Cargo.toml`](sag-gateway/Cargo.toml) | 测试期新增 `sqlx` 依赖（断言轮转产生的 `call_logs` / `sqlite_master` 原始状态） |
| [`sag-config/src/config.rs`](sag-config/src/config.rs) | `AnalyticsConfig` 新增 `call_log_retention_days: u32`（默认 90，0 = 禁用）+ 3 条解析测试（先行落地） |
| [`Cargo.lock`](Cargo.lock) | 依赖图更新 |

## 测试

**单元（`sag-analytics/src/rotate.rs`，`:memory:`）**：

1. `rotate_once_skips_when_hot_empty_or_all_in_window` — 空表 / 全部在窗内两例均 `Ok(None)`，无冷表产生。
2. `rotate_once_moves_all_rows_and_resets_hot` — 轮转后热表 0 行、冷表行数 == 原热表、冷表名匹配 `call_logs_arch_\d{8}_\d{6}`；随后 INSERT 的 `id == 1`（`sqlite_sequence` 随 RENAME 带走）。
3. `rotate_once_replayed_ddl_is_faithful` — 轮转前后 `pragma_table_info` 的 `(cid,name,type,notnull,dflt_value,pk)` 逐项相等（28 列）；`sqlite_master.sql` 含 `AUTOINCREMENT`；新热表索引集合与捕获的 5 条逐一相等；**冷表 0 索引**（锁 ③ 先 DROP 再回放的顺序坑）。
4. `rotate_once_rolls_back_atomically_on_failure` — 预置同名对象挡 ⓪ 步 → 返回 Err，热表仍在、无冷表、被 DROP 的视图由回滚恢复。
5. `unique_archive_name_resolves_same_second_collisions` — 同秒冲突得到 `_1`、`_2`。
6. `refresh_views_pads_missing_columns_in_hot_cid_order` — 缺列冷表：视图列数 == 热表列数、列名序 == 热表 cid 序、缺列值为 NULL。
7. `refresh_views_hot_membership_and_row_window` — 部分过窗冷表是成员且只输出窗内行；全过窗冷表不是成员；`call_logs_all` 含全部表全部行。
8. `refresh_views_none_and_retention_zero_semantics` — `None` 时成员 = 全部表、无过滤；不轮转不退役。
9. `retire_indexes_drops_only_expired_tables_indexes` — 合成两张带手工索引的冷表：过窗表 0 索引、窗内表索引原封不动；重复跑幂等（第二轮返回 0）。
10. `sqlx_cached_statements_survive_rotation` — 预热语句缓存 → `rotate_once`（含视图重建）→ 同一查询仍成功且行数正确；`is_schema_change` 正例 / 反例分类。
11. `refresh_views_is_idempotent` — 连跑两次 refresh 定义与结果不变；第二次 `rotate_once` 返回 `None`。

**单元（`sag-analytics/src/sqlite_store.rs`）**：

- `store_open_creates_read_views` / `hot_view_reads_match_base_table_without_cold_tables` / `store_open_refresh_covers_full_history` — 打开路径建出两视图、无冷表时视图与热表一致、兜底 refresh 覆盖全史。
- `schema_change_error_is_retried_exactly_once` / `schema_retry_gives_up_after_one_retry_and_skips_other_errors` — retry 恰好一次；非 schema 错误不重试。

**集成（`sag-analytics/tests/call_logs_rotation.rs`，文件库 + 共享池）**：

12. `startup_backlog_first_round_rotates_immediately` — 3,000 行跨 105 天：首轮即轮转（1 张冷表、热表空、`call_logs_hot` == 窗内行数、`call_logs_all` == 3,000）。
13. `concurrent_writes_during_rotation_rounds_lose_nothing` — 轮转轮循环跑动时多任务持续 `insert_batch`：无 busy 错误、总行数不丢、`MIN` 仍 ≥ cutoff 时不误轮转。
14. `queries_never_see_missing_or_stale_view` — 后台反复轮转 / refresh，前台循环查询：永不报 `no such table: call_logs_hot`，结果集始终符合当时的窗口语义。
15. `rotation_respects_retention_zero_and_reload` — `retention=0`：无冷表、无退役、视图全史；热改配置后下一轮生效。
16. `hot_window_semantics_of_legacy_stats_after_rotation` — 轮转后 `query_stats` / `query_trend` 的旧区间行为符合「热窗口」声明。

**gate（`sag-gateway/src/main.rs`）**：

17. `rotation_scheduler_shuts_down_before_event_drain` — shutdown 后 in-flight DDL 事务已结束、task 已停，接线点在 rollup 之后、event drain 之前。
