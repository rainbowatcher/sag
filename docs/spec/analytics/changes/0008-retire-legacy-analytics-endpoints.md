---
created: 2026-10-17
updated: 2026-10-17
type: 移除
---

# 0008 — 退役旧分析端点，固化两套口径契约分层

## 动机

0007 把 `call_logs` 变成**热窗口**视图（超窗数据轮转进冷表、视图逐分支收窗）之后，旧分析端点的对外语义已经不可能兑现：

- `/v1/analytics/{cache,trend,distribution}` 与 `/v1/analytics/stats` 是**四个平级端点**，但对外的措辞一直是「全生命周期统计」。轮转上线后它们静默变成「热窗口近似」，`from < cutoff` 的趋势图还会断崖——**同一个请求在不同日期返回不同数值**，且没有任何响应字段能提示调用方「这不是稳定历史统计」。
- 两套口径以平级端点并列暴露，契约上没有分层：旧链路是「单表明细聚合、`NULL` 当 0、失败率无分母剔除、可丢弃写入」，v2 是「两级事实、可验收的失败率 / 延迟 / Token 口径、原子且不丢弃的写入」。调用方无法从 API 形状判断哪一套数字是可以拿来验收的。
- 三个端点**零消费方**：前端自 0002 起全部走 `/v1/analytics/v2/*`（`sag-ui/src/stores/analytics.ts` 只剩 v2 调用），仓库内无任何调用点；dashboard 也不展示统计。它们纯粹是残留的维护面——每加一个 call_logs 列、每调一次轮转语义，都要在这里保证「旧形状不变」。

继续保留的成本是**永久的双口径维护义务 + 一份会撒谎的对外承诺**，收益是零。本变更把不可兑现的部分下线，只留下真正在用的诊断面。

## 用户决策（已拍板）

1. `/v1/analytics/{cache,trend,distribution}` 三端点**全删**（不做 410 / 不做重定向 / 不留兼容壳）。
2. `/v1/analytics/stats` 与 `/api/logs` **保留**，定位为**诊断面**（人工排查用的热窗口近似视图），不是分析面。
3. dashboard **不展示统计**：不在首页补一个「概览统计」卡片来补偿被删的端点；统计只看分析页（v2）。
4. `/api/logs` **不迁移**到两级事实口径：它读的是 `call_logs_hot`（明细 + `error` 文本 + `ttft_ms` + `loss_summary`），这些字段事实表本来就不承载，迁移等于给事实表加诊断列。

## 契约分层（本变更的产出）

同一份数据在系统里有两条完全不同的口径，从今往后由端点边界显式区分：

| 维度 | 诊断口径（`/v1/analytics/stats`、`/api/logs`） | 分析口径（`/v1/analytics/v2/summary\|trend`） |
| --- | --- | --- |
| 数据源 | `call_logs_hot` 视图（明细） | `request_results` / `target_attempts`（+ 1h Rollup） |
| 写入契约 | **可丢弃**：bounded mpsc + `try_send`，队列满即丢 | **不丢弃**：队列满背压，shutdown 前排空，同一请求事实原子提交 |
| 时间覆盖 | 热窗口（`call_log_retention_days`，默认 90 天），随每日轮转缩水 | 全历史（Rollup 只加速、不删明细） |
| 计数口径 | 一行 = 一条 `call_logs` 行（一请求可多行） | route 级一请求一条、target 级一尝试一条 |
| NULL 处理 | `COALESCE(SUM(...), 0)`：未知缓存 / Token **当 0** | NULL = 未知，字典单独计数；未知样本不进请求命中率分母 |
| 失败率 | `error_count / call_count`，以 `error` 文本非空为准，**无分母剔除规则** | 按最终状态分类，client_error / no_route / all_disabled / client_cancelled 各有明确分母 |
| 稳定性 | **不稳定**：同一查询跨日期可能不同（窗口推进、轮转、写入丢弃） | 稳定：同一区间同一结果（事实不可变、Rollup 可重算） |
| 用途 | 人工排查（错误文本、TTFT、丢包摘要、逐条时间线） | 验收与报表（失败率、延迟分位、Token / 缓存命中率） |

结论写死：**两套数字不可对比**。`stats.call_count` 与 v2 `overall.requests` 对不上是**预期行为**，不是 bug——不同的计数对象、不同的时间覆盖、不同的丢弃语义。任何「让两者对齐」的改动都是把诊断口径污染成事实口径。

跨表 / 跨口径追溯只有一个稳定键：`request_id`（UUID v4，全局唯一）。`call_logs.id` 每张表从 1 重置（0007 的 `ALTER TABLE … RENAME` 会把 `sqlite_sequence` 一起改名），**不得**作为关联键。

维护任务边界同样按口径切分，互不越界：

- **Rollup**（1h 批处理）只服务 v2：读 `request_results` / `target_attempts`，写 rollup 表，完全不碰 `call_logs`。
- **Rotation**（保留窗口 / 索引退役 / 视图重建）只服务诊断链路：只动 `call_logs` 与 `call_logs_arch_*` / 两个视图，完全不碰事实表与 rollup 表。

## 变更内容

- 删除 `GET /v1/analytics/cache`、`GET /v1/analytics/trend`、`GET /v1/analytics/distribution` 三条路由与对应 handler；store 侧随之删除 `query_cache_stats` / `query_trend` / `query_distribution` 与 `CacheStatsRow` / `TrendRow` / `DistributionRow`。
- `GET /v1/analytics/stats` 保留原样：查询参数 `provider` / `model` / `real_model`（均可选），响应 `{"data": [StatsRow...]}`，数据源 `call_logs_hot`。
- `GET /api/logs` 保留原样：`call_logs_hot` 分页 + `total`，不迁移、不改字段。
- 分析能力全部由 `/v1/analytics/v2/summary|trend` 承担：需要时间趋势（按小时 / 自定义桶）用 `v2/trend`；需要 provider / real_model 维度的分布与占比用 `v2/summary?scope=target` 的 `groups`。
- 文档侧按新契约改写：`requirements.md`（API 与 UI、兼容性说明、包含项）、`spec.md`（新增「两套口径契约分层」节）、`docs/guide/analytics.md`（修正 `stats` 响应示例漂移、删除被下线端点文档、补「诊断口径 vs 分析口径」边界）、`docs/guide/models-api.md`（清理被删端点引用）。

## 边界结论

1. **不做软下线**：不保留 410 / 301 壳。契约已不成立（数字不稳定），留一个「还能调到但数字会变」的端点比 404 更危险。
2. **`/v1/analytics/stats` 的已知不精确性照旧存在**：`NULL` 当 0、`error_count` 以文本非空为准、热窗口缩水、写入丢行。这些**不在本变更修复**——它是诊断端点，口径已由端点定位声明，修它等于把诊断面往事实口径上靠。
3. **前端无功能改动**：`sag-ui` 生产路径早已只用 v2；本次删除的是 `stores/analytics.ts` / `types/api.ts` 中**已死的旧端点死代码**（4 个 fetch + 4 个类型）与 `DashboardPage` 的虚假 `fetchStats()` 调用（调用了端点但消费方已被删）。dashboard 维持「不展示统计」。
4. **`LogStore` trait 收窄**：删除三个方法后 trait 只剩 `insert_batch` / `query_stats` / `query_logs` / `count_logs` / `shutdown`。`call_logs_hot` 的查询面只剩「分页 + 一个按 `(provider, model, real_model)` 分组的聚合」。
5. **0007 的热窗口语义不变**：轮转 / 视图 / 索引退役机制一行不改。本次只是把「只有诊断端点消费热窗口」这件事写进契约——0007 时期被描述的「轮转后旧统计口径变化」风险面因此收窄到 stats + `/api/logs` 两处。
6. **测试面收窄**：`sag-analytics/tests/call_logs_rotation.rs` 中依赖 `query_trend` 的热窗口回归断言改为以 `query_stats` / `call_logs_hot` 直查断言；`sqlite_store` 侧 `query_cache_stats` 的内联单测随方法删除。
7. **不做替代端点**：不新增「兼容趋势端点」或「聚合分布端点」——v2 已覆盖，重复建设等于再开一条双口径。

## 需求变更

- requirements.md「目标」：兼容性措辞由「保留旧日志与旧统计接口的兼容性」改为「保留 `/v1/analytics/stats` 与 `/api/logs` 两个诊断接口」。
- requirements.md「数据事实」：`call_logs` 的数据源角色限定为「诊断接口的数据源」，不再是「旧分析接口的数据源」。
- requirements.md「保留与轮转」：热窗口覆盖范围由「`/v1/analytics/*`」收窄为「`/api/logs`、`/v1/analytics/stats`」；「旧统计结果随时间变化」条目改为只描述 `query_stats`（`/v1/analytics/stats`），并显式声明趋势 / 分布已下线。
- requirements.md「API 与 UI」：「保留旧分析接口与旧日志接口，行为与数据源不变」改为「保留诊断接口 + 三端点下线 + 分析能力由 v2 承担 + 两套数字不可对比」。
- requirements.md「兼容性说明」：「旧 `/v1/analytics/*` 接口按旧明细聚合，保持兼容，不迁移为两级事实口径」改为只对 `stats` / `/api/logs` 生效，并记录三个端点已下线。
- requirements.md「功能边界 / 包含」：由「旧日志接口与旧分析接口兼容保留」改为「诊断接口保留（stats + logs）」。
- requirements.md「与其它模块的交互」：管理面板措辞对齐（统计只看 v2 分析页，日志页保留）。
- spec.md：新增「两套口径契约分层（已实现，0008）」节；「关键文件」「API 与 UI」「不变式 → API」「迁移阶段表」同步收窄到 stats + `/api/logs`。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-router/src/routes.rs`](sag-router/src/routes.rs) | 删除 `/v1/analytics/cache`、`/v1/analytics/trend`、`/v1/analytics/distribution` 三条路由注册与对应 import；只留 `/v1/analytics/stats` + `/v1/analytics/v2/*` |
| [`sag-router/src/handlers/mod.rs`](sag-router/src/handlers/mod.rs) | 删除 `analytics_cache_stats` / `analytics_trend` / `analytics_distribution` 三个 handler；模块文档注释同步收窄为 `/v1/analytics/stats` |
| [`sag-analytics/src/store.rs`](sag-analytics/src/store.rs) | `LogStore` trait 删除 `query_cache_stats` / `query_trend` / `query_distribution`；删除 `CacheStatsRow` / `TrendRow` / `DistributionRow` 及对应 `TrendFilter` |
| [`sag-analytics/src/sqlite_store.rs`](sag-analytics/src/sqlite_store.rs) | 删除上述三个方法的 SQLite 实现与 `query_cache_stats` 的内联单测；`call_logs_hot` 查询面只剩 `query_stats` / `query_logs` / `count_logs` |
| [`sag-analytics/src/lib.rs`](sag-analytics/src/lib.rs) | 无需改动：`pub use store::*` 为 glob 再导出，被删类型的公开导出自动同步消失 |
| [`sag-analytics/tests/call_logs_rotation.rs`](sag-analytics/tests/call_logs_rotation.rs) | 依赖 `query_trend` 的热窗口回归断言改为 `query_stats` / `call_logs_hot` 直查（断言目标不变：超窗数据退出视图后聚合随之收缩） |
| [`sag-dispatch/src/dispatch/stream.rs`](sag-dispatch/src/dispatch/stream.rs) | 两处注释中 `query_cache_stats` → `query_stats`（原引用的方法已删除） |

## 测试

- 路由面：`sag-router/src/routes.rs` 的 `retired_analytics_endpoints_are_gone` 断言 `/v1/analytics/{cache,trend,distribution}` 均 404（无 410 / 301 壳），`/v1/analytics/stats` 仍 200 且保留 `{"data": [...]}` 信封；`/api/logs` 与 `/v1/analytics/v2/summary|trend` 仍正常（既有 v2 集成测试不改）。
- `sag-analytics`：删除随方法消失的 `query_cache_stats` 单测；`call_logs_rotation.rs` 的热窗口语义断言改用 `query_stats`（同一文件库场景下仍覆盖「轮转后旧口径缩水」）。
- 编译面：`cargo clippy --all-targets -- -D warnings` 无 `dead_code` / 未使用 import 告警（证明无残留引用）。
