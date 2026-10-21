---
created: 2026-10-21
updated: 2026-10-21
type: 新增
---

# 0003 — 会话粘性路由（session_sticky）与 priority 加权随机基底

## 动机

客户端用 `conversation_id` / `prompt_cache_key` 标识会话时，希望同一会话的请求持续
落在同一个路由目标（prompt cache 亲和），而新会话按 `priority` 当权重做加权随机分配。
原路由策略只有 sticky_failover（模型级粘性）/ fill_first / round_robin，且 priority
只能表达分层、无法表达比例。

## 需求变更

- 「功能边界 → 包含」新增：会话识别与会话粘性路由（`strategy = "session_sticky"`）、
  加权随机基底（priority 即权重）、会话级管理端观测/清除、logs 页按会话过滤。
- 「功能边界 → 不包含」中「权重分配（不在当前 scope）」条目改写为
  `session_sticky` 的 scope 条目（加权随机只作其内部基底，不单独暴露策略）。
- 「预期行为 → 目标选择」表新增 `session_sticky` 行；新增「会话粘性路由」与
  「加权随机基底」两节。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-config/src/config.rs`](sag-config/src/config.rs) | `Strategy` 新增 `SessionSticky`；`RouteConfig` 新增 `session_ttl_secs` / `session_max_entries`（Option 风格，缺省 3600 / 10000） |
| [`sag-config/src/loader/save.rs`](sag-config/src/loader/save.rs) | `session_sticky` 字面量与两个新字段的写回 / 清除 |
| [`sag-core/src/session.rs`](sag-core/src/session.rs) | `SessionKey`（source 前缀 + 原值）、`map_id()` 128-bit 截断摘要、`sha256_hex` |
| [`sag-router/src/session.rs`](sag-router/src/session.rs) | 会话键提取链（conversation_id 族 → prompt_cache_key → 首条 user 文本哈希兜底）+ user_id 抑制闸 |
| [`sag-state/src/session_affinity.rs`](sag-state/src/session_affinity.rs) | `SessionAffinity` 状态机：TTL 滑动续期、惰性过期、LRU 驱逐、generation/epoch 令牌协议 |
| [`sag-dispatch/src/order.rs`](sag-dispatch/src/order.rs) | `OrderParams` 重构；`session_sticky` 排序（pin hoist + A-Res 加权排列）；共享身份判定 `target_matches_entry` |
| [`sag-dispatch/src/dispatch/chat/mod.rs`](sag-dispatch/src/dispatch/chat/mod.rs) | 成功路径按 strategy 分流写回（failover 成功后重绑）；`call_logs.session_key` 哈希列贯通 |
| [`sag-admin/src/admin_session_affinity.rs`](sag-admin/src/admin_session_affinity.rs) | `GET|DELETE /api/session-affinity`（`{all:true}` 需 `confirm: true`） |
| [`sag-analytics/migrations/20260816000001_add_call_logs_session_key.sqlite.sql`](sag-analytics/migrations/20260816000001_add_call_logs_session_key.sqlite.sql) | `call_logs` 加 `session_key` 列 + 索引 |
| [`sag-analytics/src/sqlite_store.rs`](sag-analytics/src/sqlite_store.rs) | `session_key` 写入 / 查询过滤（`LogFilter.session`） |
| [`sag-admin/src/admin_logs.rs`](sag-admin/src/admin_logs.rs) | `GET /api/logs` 加 `session` query 参数 |
| [`sag-ui/src/pages/LogsPage.vue`](sag-ui/src/pages/LogsPage.vue) | logs 页「按会话过滤」输入框（配合 `/api/session-affinity` 快照的哈希键） |

## 测试

- `sag-config`：session 字段解析 / serde round-trip / 未知 strategy 严格拒绝 / 未知字段容忍 / save 写回与 None 清除
- `sag-core`：`map_id` 稳定性、来源隔离、不含原值
- `sag-router`：提取链优先级、空值跳过、user_id 抑制、归一化确定性、typed/raw 等价
- `sag-state`：陈旧写回被拒、TTL 惰性过期不删、滑动续期、LRU 驱逐 bump generation、epoch 兜底、容量 1 边界
- `sag-dispatch`：pin hoist / 去重 / miss / 身份规则单测；调度集成（粘住、failover 重绑、会话独立、无会话无痕）
- `sag-admin`：快照只含哈希键（无 PII）、按 key/model 过滤、DELETE 三作用域、`{all:true}` 确认闸
- `sag-analytics`：`session_key` 迁移 round-trip、按 session 过滤、NULL 不误伤
