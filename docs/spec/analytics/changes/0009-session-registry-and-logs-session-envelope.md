---
created: 2026-10-26
updated: 2026-10-26
type: 新增
---

# 0009 — 会话注册表 `session_registry` 与 `/api/logs` 的 `session` 信封

## 动机

日志页的 Session 列（[UI 0009](../../ui/changes/0009-session-affinity-pins-and-log-session-column.md)）只能显示
`call_logs.session_key` 的前 8 位哈希。哈希不可读：运维看到一行 502 时，无法判断它属于哪次对话、
这段对话开头问的是什么。要在管理面给出**人类可读的会话身份**，必须有一处地方保存
「哈希 → 展示信息」的映射。

放哪里都有代价：

- **不给 `call_logs` 加列**：它是诊断明细表（可丢弃写入、随轮转缩水），不该承载展示型元数据；
  且 0007 的 replay 测试把「热表 29 列 / 6 索引」写成了不变量，加列等于改这条不变量。
- **不能回填历史**：哈希是单向的，注册表只能从这一刻起记录；旧行永远没有展示信息。

本次新增独立表 `session_registry`（每会话一行）+ 采集链路写入 + `GET /api/logs` 信封上的可选
`session` 字段，让日志页用**一次请求**同时拿到明细页与当前筛选键的展示信息。

## 用户裁定：隐私边界（PII carve-out）

脱敏（`[privacy]`）只作用于**发往上游大模型的请求内容**；`session_registry` 属于**本地管理面可见数据**，
因此它存的是原始值：

- `conversation_id` 存**原始**会话 id（仅 `meta` / `top` 来源，截断到 128 字符；`pc` / `fb` 是不透明值，
  不落库）；
- `first_message_preview` 存**原始（未脱敏）**的首条用户消息预览（去 markup、折叠空白，截断到 160 字符，被截断时追加 `…`）；
- 采集点取的是 masking **之前**的请求与 IR（`chat/mod.rs` 的 `registry_request` / `registry_ir`），
  因此开启脱敏也不会让 `[[KIND_h12]]` 占位符进入注册表。

这是 `sag_core::session.rs` 中「No PII ever leaves this type unhashed」不变式的**唯一**文档化例外：
原始值仍不得进入 `CallLog`、会话亲和 map 或 admin snapshot，例外只覆盖 `session_registry` 一张表，
且该表有独立保留期（见下）。

## 需求变更

- `docs/spec/analytics/requirements.md`
  - 「保留与轮转」新增 `session_registry` 独立保留期条目（默认 7 天、`0` = 永久保留、超期直接删行不归档、
    不参与 `call_logs` 轮转、配置每轮热读取）。
  - 「配置 Schema」的 `[analytics]` TOML 块补 `session_retention_days = 7`。
  - 「API 与 UI」的诊断接口条目补充「`/api/logs` 信封向后兼容地新增可选 `session` 字段」。
- `docs/spec/analytics/spec.md`
  - 「配置与身份来源」补 `AnalyticsConfig` 的新字段 `session_retention_days`。
  - 「API 与 UI」的「诊断接口保留、签名与行为不变」改为「信封向后兼容地新增可选 `session` 字段」。
  - 新增「会话注册表 `session_registry`（已实现，0009）」一节：表结构、写入点、合并语义、保留期、不变式。
- `docs/spec/admin/requirements.md`「日志查询」补 `session` 查询参数与响应 `session` 字段的三态说明。
- `docs/guide/analytics.md` 诊断接口表补 `session` 过滤参数与 `session` 三态信封说明。
- `docs/guide/configuration.md` `[analytics]` 块补 `session_retention_days` 键（省略即启用 7 天清理）。
- `docs/spec/ui/changes/0011-logs-session-info-card.md` 记录消费侧（卡片）的展示与降级形态。

## 数据模型

```sql
CREATE TABLE IF NOT EXISTS session_registry (
    session_key           TEXT PRIMARY KEY,   -- SessionKey::map_id（32 hex），与 call_logs.session_key 同域
    session_source        TEXT NOT NULL,      -- 抽取链来源：meta / top / pc / fb
    conversation_id       TEXT,               -- 原始会话 id，≤128 字符；pc / fb 为 NULL
    first_message_preview TEXT,               -- 首条用户消息预览，≤160 字符（被截断时追加 `…`，最长 161）
    first_seen            INTEGER NOT NULL,   -- unix ms
    last_seen             INTEGER NOT NULL    -- unix ms
);
CREATE INDEX IF NOT EXISTS idx_session_registry_last_seen ON session_registry(last_seen);
```

`session_source` 是抽取链的命中步骤（`meta` = `metadata.conversation_id`、`top` = 顶层
`conversation_id`、`pc` = `prompt_cache_key`、`fb` = 内容哈希兜底），与 `SessionKey` 的前缀同源，
因此注册表行与 `call_logs.session_key` 一一对应（同一 route model 空间）。

### 合并语义：最早请求者胜（earliest-requester-wins）

冲突时不是「先到先得」，而是**按请求时间戳判定**：

- `first_seen` 取 `MIN(已存, 新到)`，`last_seen` 取 `MAX(已存, 新到)`；
- `session_source` / `conversation_id` / `first_message_preview` **仅当新到 fact 的 `first_seen` 更早**时覆盖，
  相等时保留已存值（同一时间戳不替换 opener）。

即「opener 语义」：保留**最早那次请求**的会话 id 与首条消息预览，与写入到达顺序无关。
这条很重要——collector 的队列满/关兜底是**绕过 FIFO 直写**的，同一会话的两个 fact 可能乱序落库；
以到达顺序定赢家会让「预览」变成某条中途消息。判定只看请求时间戳，因此结果与交错顺序无关。

## 保留期与清理

- `[analytics] session_retention_days`，默认 **7**，`0` = 永久保留（清理关闭）。
- 清理点：`rotation_round` 的**末尾**（与 `call_logs` 轮转同一轮，但两者独立——轮转失败仍然清理注册表，
  清理失败也不影响本轮结果），每轮从热配置重读，热重载在下一轮生效。
- cutoff 严格小于（`last_seen < now - days × 24h`），恰在边界上的行存活；天数按
  `MAX_CALL_LOG_RETENTION_DAYS`（3650）同量级钳制，钳制走 `debug!`。
- **超期直接删行、不归档**：与 `call_logs` 的「轮转进冷表」相反，注册表没有冷表也没有视图，
  删掉就没了（日志页对这类会话显示降级文案）。
- 每轮打点：首轮与「有效值发生变化」时输出 `session registry prune armed`（含有效天数与来源
  `default` / `config`，便于发现「省略该键 → 默认 7 天开始删行」这件事）；删行数为 0 时 `debug!`，
  非 0 时 `info!`。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-analytics/migrations/20260820000001_add_session_registry.sqlite.sql`](../../../../sag-analytics/migrations/20260820000001_add_session_registry.sqlite.sql) | 新增建表 + `idx_session_registry_last_seen`；注释写明本表不参与 `call_logs` 轮转、自带保留期 |
| [`sag-analytics/src/session_registry.rs`](../../../../sag-analytics/src/session_registry.rs) | 新增：`upsert_session`（最早请求者胜的合并 SQL）、`get_session`（主键点查）、`prune_session_registry`（`0` 短路、cutoff 严格小于、饱和运算不 panic）；`SessionRegistryFact` 从 `sag_core::session_preview` re-export |
| [`sag-analytics/src/store.rs`](../../../../sag-analytics/src/store.rs) | `LogStore` trait 新增 `upsert_session` / `get_session`；新增 `SessionRecord`（admin 侧读取形状，`Serialize`） |
| [`sag-analytics/src/sqlite_store.rs`](../../../../sag-analytics/src/sqlite_store.rs) | `SqliteLogStore` 实现两个新 trait 方法，与其它读一致包 `with_schema_retry` |
| [`sag-analytics/src/collector.rs`](../../../../sag-analytics/src/collector.rs) | 新增 `CollectorCommand::UpsertSession` + `CallLogCollector::log_session`（与 `log` 同一 MPSC，FIFO / `flush` 语义覆盖两类写）；队列满 / 关闭时改为**绕开队列的直写兜底**而非丢弃（丢 opener 会让预览永久冻结在中途消息） |
| [`sag-analytics/src/lib.rs`](../../../../sag-analytics/src/lib.rs) | 导出 `session_registry` 模块与 `upsert_session` / `get_session` / `prune_session_registry` / `SessionRegistryFact` |
| [`sag-core/src/session_preview.rs`](../../../../sag-core/src/session_preview.rs) | 新增 `SessionRegistryFact` 与 `build_session_fact`（会话 id 截断、首条用户消息 preview 去 markup + 折叠空白 + 截断）、`CONVERSATION_ID_MAX_CHARS` / `PREVIEW_MAX_CHARS`；模块文档记录 PII carve-out |
| [`sag-core/src/session.rs`](../../../../sag-core/src/session.rs) | 模块文档新增「Approved PII exception: `session_registry`」一节，明确例外范围与保留期 |
| [`sag-core/src/lib.rs`](../../../../sag-core/src/lib.rs) | re-export `session_preview` 的公开项 |
| [`sag-config/src/config.rs`](../../../../sag-config/src/config.rs) | `AnalyticsConfig` 新增 `session_retention_days`（`#[serde(default)]` = `DEFAULT_SESSION_RETENTION_DAYS` = 7，`0` = 永久保留） |
| [`sag-gateway/src/main.rs`](../../../../sag-gateway/src/main.rs) | `rotation_round` 末尾清理注册表：一轮一次读配置同时取两个保留期、按 3650 钳制、`0` 原样透传；`SessionPruneLogState` 让 arm 行在首轮与变化时输出；清理失败只 `warn!` |
| [`sag-gateway/Cargo.toml`](../../../../sag-gateway/Cargo.toml) | `chrono` 从 dev-dependencies 提升为正式依赖（prune 需要 `DateTime<Utc>`，analytics 各 crate 不 re-export chrono） |
| [`sag-dispatch/src/dispatch/chat/mod.rs`](../../../../sag-dispatch/src/dispatch/chat/mod.rs) | 每请求写一次注册表 fact：在 `ir` 确定为 `&IrRequest` 之后、`cancel_guard` 之前；预览源取 **masking 之前**的 `registry_request` / `registry_ir` |
| [`sag-dispatch/src/dispatch/image.rs`](../../../../sag-dispatch/src/dispatch/image.rs) | 同上（image 路径无 IR，preview 为空，`conversation_id` 照常写） |
| [`sag-dispatch/src/dispatch/resolve.rs`](../../../../sag-dispatch/src/dispatch/resolve.rs) | `__no_route__` / `__all_disabled__` 两个 sentinel 终态也写注册表 fact（它们的 `CallLog` 行同样带 `session_key`，卡片必须能解析） |
| [`sag-admin/src/admin_logs.rs`](../../../../sag-admin/src/admin_logs.rs) | 响应信封改为 `LogsResponse { data, total, session }`：`session: Option<Option<SessionRecord>>` + `skip_serializing_if = "Option::is_none"`；`?session=` 命中查注册表，未命中或查询失败降级为 `null`（明细页是主载荷，不因装饰性卡片而失败） |
| [`gateway.example.toml`](../../../../gateway.example.toml) / [`gateway.test.example.toml`](../../../../gateway.test.example.toml) | `[analytics]` 补 `session_retention_days = 7` 及注释 |

## `GET /api/logs` 信封三态

`session` 字段用双层 `Option` 表达三种形状，前端必须能区分：

| 请求 | `session` 字段 |
|---|---|
| 无 `?session=`，或值为空串 | **键不存在**（旧 `{"data": [...], "total": n}` 信封逐字保留，老客户端与旧仪表盘不受影响） |
| `?session=<key>`，注册表无该行 | `null`（旧行 / 已过保留期 / 从未记录） |
| `?session=<key>`，注册表有该行 | `SessionRecord` 对象（`session_key` / `session_source` / `conversation_id` / `first_message_preview` / `first_seen` / `last_seen`，时间戳与 `CallLog::timestamp` 同为 RFC3339） |

查询参数 `session` 与 `provider` / `model` / `real_model` / `status_code` 一样按「空串视同未传」处理；
匹配是 `call_logs.session_key` 上的精确等值。

## 测试

- `sag-analytics/src/session_registry.rs`：`upsert` 合并（乱序写入后 opener 仍属于更早的 `first_seen`、
  `first_seen` 取 MIN、`last_seen` 取 MAX、相等时间戳不替换）、`get_session` 命中 / 未命中、
  `prune` 的 `0` 短路 / 边界严格小于 / 钳制不 panic。
- `sag-analytics/src/collector.rs`：`log_session` 经 `flush()` 后落库；队列满时直写兜底仍然落库。
- `sag-dispatch/src/dispatch/chat/tests.rs`：每请求恰写一条注册表 fact；开启隐私脱敏时 preview 仍是原始文本。
- `sag-dispatch/src/dispatch/image.rs`：image 派发写注册表 fact（无 preview，有 `conversation_id`）。
- `sag-dispatch/src/dispatch/resolve.rs`：两个 sentinel 终态写注册表 fact。
- `sag-admin/src/admin_logs.rs`：命中 / 未知键 `null` / 无 `?session=` 时无键 / 空串视同未传，四态各一例。
- `sag-gateway/src/main.rs`：`rotation_round` 按热配置清理、极端保留期钳制不 panic、arm 行的来源标注。
