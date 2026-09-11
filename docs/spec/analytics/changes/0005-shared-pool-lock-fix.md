---
created: 2026-09-11
updated: 2026-09-11
type: 修改
---

# 0005 — 共享 SQLite 连接池 + WAL / BEGIN IMMEDIATE 锁修复

## 动机

生产网关此前为 analytics.db 开了**两个独立 SQLite 连接池**：正式分析路径（EventWriter / IdentityRegistry / RollupEngine / AnalyticsQueryEngine）经 `open_analytics_pool` 共享一个 pool，而旧链路 `CallLogCollector::new(&analytics_dsn)` 又用 `SqliteLogStore::new` 在同一个物理文件上另开一个 `max_connections=4` 的私有 pool，且该 store 的 `shutdown` 会关闭自己的 pool。双 pool 在同一文件上互相竞争数据库锁（DELETE journal 下并发写极易 `SQLITE_BUSY`），旧 store 关闭自身 pool 的行为在共享场景下会拖垮整个分析子系统。本次把旧链路并入共享 pool，统一各写入路径的并发安全连接选项（WAL + `synchronous=NORMAL` + busy_timeout），并把全部分析写事务改为 `BEGIN IMMEDIATE`，消除锁竞争与 deferred-BEGIN 升级竞态。

## 行为变化

- **单共享连接池**：生产装配（`sag-gateway/src/main.rs`）不再调用 `CallLogCollector::new(dsn)`，改为 `CallLogCollector::new_shared(analytics_pool.clone())` —— 正式 writer / 身份注册表 / rollup / 查询引擎 / 旧 call_logs store 共用一个 pool，不再对同一文件开第二个池。
- **store 生命周期**：`SqliteLogStore` 增加 `owns_pool` 标志；`from_pool(pool)` 构建的 store 不拥有 pool，`shutdown` 不 close 调用方的 pool；DSN 构造（`new`）仍拥有并 close 自己创建的 pool（测试 / 向后兼容）。`CallLogCollector::new_shared(pool)` 为对应共享构造器。
- **连接选项统一**：新增 `analytics_connect_options(dsn)` 集中构造连接选项（`foreign_keys` + `busy_timeout=5s`；file DSN 附加 WAL + `synchronous=NORMAL`）；`open_analytics_pool` / `SqliteLogStore::new` / `EventWriter::open` 全部改用它。`:memory:` 等内存 DSN 跳过 WAL（SQLite 不支持内存库 WAL，测试路径不受影响）。
- **写事务 BEGIN IMMEDIATE**：`EventWriter::write`、`RollupEngine::compute_bucket`、`IdentityRegistry::sync` 三个 `pool.begin()` 改为 `pool.begin_with("BEGIN IMMEDIATE")`（sqlx 0.8 原生支持），先取写锁避免 deferred BEGIN 升级竞态（SQLITE_BUSY_SNAPSHOT）；`SqliteLogStore::insert_batch` 从逐条 autocommit 改为整批一个 `BEGIN IMMEDIATE` 事务（原子落库 + 单次短写锁）。
- 公开 API 行为与 analytics schema 不变；`CallLogCollector::new` / `SqliteLogStore::new` / `new_memory` 保留原语义。

## 需求变更

- requirements.md「写入可靠性与原子性」：追加 call_logs 旧链路与正式分析**共用同一 SQLite 连接池**、file DSN 使用 WAL + `synchronous=NORMAL` + 5s busy_timeout、旧 store 批量插入整批走单个 `BEGIN IMMEDIATE` 事务的行为；「兼容性说明」追加旧 call_logs 链路仍可丢弃、但落库路径并入共享连接池（store 自身不关闭共享 pool）的说明。

## 代码变更

| 文件 | 变更 |
|------|------|
| [sag-analytics/src/lib.rs](../../../../sag-analytics/src/lib.rs) | 新增 `analytics_connect_options` 集中连接选项（foreign_keys + busy_timeout=5s + file DSN WAL/NORMAL，内存 DSN 跳过 WAL）；`open_analytics_pool` 改用它；新增连接选项回归测试 |
| [sag-analytics/src/sqlite_store.rs](../../../../sag-analytics/src/sqlite_store.rs) | `SqliteLogStore` 增加 `owns_pool`；新增 `from_pool`（不拥有 pool，shutdown 不 close）；`new` 改用共享连接选项并保持拥有语义；`insert_batch` 整批 `BEGIN IMMEDIATE` 单事务；`shutdown` 仅拥有时 close pool；新增共享/拥有生命周期与并发写回归测试 |
| [sag-analytics/src/collector.rs](../../../../sag-analytics/src/collector.rs) | 新增 `CallLogCollector::new_shared(pool)`（基于 `SqliteLogStore::from_pool`） |
| [sag-analytics/src/event_writer.rs](../../../../sag-analytics/src/event_writer.rs) | `write` 事务改 `begin_with("BEGIN IMMEDIATE")`；`open` 改用共享连接选项；`is_in_memory_dsn` 提为 `pub(crate)` 供选项助手复用 |
| [sag-analytics/src/rollup.rs](../../../../sag-analytics/src/rollup.rs) | `compute_bucket` 事务改 `begin_with("BEGIN IMMEDIATE")`；模块文档更新 |
| [sag-analytics/src/registry.rs](../../../../sag-analytics/src/registry.rs) | `sync` 事务改 `begin_with("BEGIN IMMEDIATE")`（热重载可与事实写入并发） |
| [sag-gateway/src/main.rs](../../../../sag-gateway/src/main.rs) | 生产装配改 `CallLogCollector::new_shared(analytics_pool.clone())`，不再 `new(dsn)` 开第二个 pool |

## 测试

- 新增 `sqlite_store::tests::shared_pool_store_shutdown_does_not_close_caller_pool`：`from_pool` 的 store shutdown 后调用方 pool 仍可查询。
- 新增 `sqlite_store::tests::owned_dsn_store_shutdown_closes_its_own_pool`：DSN 构造的 store shutdown 后 pool 返回 `PoolClosed`。
- 新增 `tests::file_pool_connections_report_wal_normal_and_busy_timeout`：file pool 连接报告 `journal_mode=wal`、`synchronous=1`、`busy_timeout=5000`。
- 新增 `tests::memory_pool_keeps_default_journal_mode`：`:memory:` pool 保持默认 journal mode，不被强制 WAL。
- 新增 `sqlite_store::tests::concurrent_writers_on_shared_file_pool_persist_all_events`：同一共享 file pool 上 4 个并发 writer（各 20 批 × 5 条）不产生 database-locked 错误，全部事件落库。
- 新增 `sqlite_store::tests::shared_pool_collector_shutdown_keeps_caller_pool_alive`：`CallLogCollector::new_shared` 持久化全部日志，shutdown 后共享 pool 存活。

## 验证

- `cargo test -p sag-analytics`：95 passed，0 failed（含 6 个新增回归测试）。
- `cargo test -p sag-gateway`：2 passed，0 failed。
- `cargo clippy -p sag-analytics -p sag-gateway --all-targets`：无警告。
- `cargo fmt -p sag-analytics -p sag-gateway --check`：通过。
