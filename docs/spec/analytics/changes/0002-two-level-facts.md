---
created: 2026-08-11
updated: 2026-08-11
type: 修改
---

# 0002 — 双层事实与 Rollup

## 动机（背景）

reviewer 评审 `docs/spec/analytics/spec.md` 与 `requirements.md` 后发现设计文档存在若干缺口与不一致：缓存命中率在 Rollup 结构中缺少可合并计数，>24h 历史查询无法正确聚合；dispatch flowchart 中 `client_error` / `no_route` / `all_disabled` 三个最终状态节点未连接到 AnalyticsEvent emit，与正文「写 request_results」不一致；`actual_model` 语义未定义；`failure_reason` 缺少分类映射；历史延迟分位的直方图合并规则未说明；target 级结果枚举未覆盖客户端取消；`target_attempts` 外键级联删除与「事实永久保留」冲突；时间戳存储格式未明确。本次修订补齐这些定义。

已确认的既有契约全部保留不变：UUID v7 只存在于数据库、`route_name` 稳定身份、旧 `call_logs` 兼容、旧历史不回填、24h 精确 + 1h Rollup、每轮最多 24 桶、UTC 完成时间分桶、scope API、Top20+others、`low_sample`、异常崩溃小窗口丢失。

## 行为变化

- **缓存命中率可合并**：缓存样本集与公式明确化（未知样本不计分母，NULL 不作 0）；三张 Rollup 表（global/route/target）均新增可合并计数 `cached_known_count` / `cached_hit_count` / `prompt_tokens_cached_known`，>24h 混合查询可正确计算请求命中率与 Token 命中率。
- **target 结果枚举三值化**：`target_attempts.status` 扩展为 `success` / `failure` / `cancelled`（客户端取消的在途尝试）；`rollup_target_hourly.attempts = success + failure + cancelled`（新增 `cancelled` 列）。
- **actual_model 语义定义**：运行时实际调用的模型；passthrough（`real_model=None`）= 请求模型，显式 `real_model` = 路由解析后的实际模型；`request_results` 仅成功非 NULL，`target_attempts` 新增恒非 NULL 的冗余列。
- **failure_reason 映射表**：至少 11 个 reason 映射到 5 个最终状态（`invalid_request`/`payload_too_large` → `client_error`；`model_not_found` → `no_route`；`all_disabled` → `all_disabled`；`timeout`/`network_error`/`provider_error`/`empty_response`/`rate_limited`/`stream_error_after_commit`/`all_exhausted` → `model_failure`；`client_cancelled` → `client_cancelled`）；允许扩展，但 `(reason, final_status)` 组合必须合法。
- **flowchart 修正**：`client_error` / `no_route` / `all_disabled` 最终状态节点连接到 AnalyticsEvent emit，与正文一致。
- **历史延迟分位合并规则**：global overall 合并选定时间范围内同一 `dim` 的全部 route 直方图行，按 `bucket_idx` 求和后按累计计数估算分位；route/target 分组按 `owner_id` 筛选。
- **事实永久保留**：`target_attempts.request_id` 外键由 `ON DELETE CASCADE` 改为 `ON DELETE RESTRICT`，禁止删除事实。
- **时间存储格式**：事件侧 `DateTime<Utc>` 在 writer 落库时转换为 Unix 毫秒 INTEGER；与 `from`/`to` 查询参数单位一致。
- **启动扫描与长期缺口**：启动只登记缺失/脏桶不计算；已登记但长期未补算的缺口在查询时始终回查明细补齐（精确）。

## 需求变更

- requirements.md「数据事实」：新增 `actual_model` 定义（passthrough / 显式 real_model 语义，两级事实各自保存）。
- requirements.md「最终状态与失败率」：新增 target 尝试结果枚举（`success` / `failure` / `cancelled`，cancelled 计入分母不计成败）与 client_cancelled 无在途尝试边界（`target_attempts` 为 0 条）。
- requirements.md「指标口径（Token 与缓存）」：明确缓存命中率样本集（未知样本不计分母、NULL 不作 0）与 1h Rollup 可合并要求。
- requirements.md「变更记录」：追加 0002 条目，保留 0001。

## 代码变更

| 文件 | 变更 |
|------|------|
| [docs/spec/analytics/spec.md](../../../../docs/spec/analytics/spec.md) | Rollup 表新增缓存可合并计数与 `cancelled` 列、flowchart 补 emit 边、failure_reason 映射表、`actual_model` 列与语义、FK 改 RESTRICT、时间戳转换说明、直方图合并规则、启动扫描/长期缺口说明、不变式与测试策略同步 |
| [docs/spec/analytics/requirements.md](../../../../docs/spec/analytics/requirements.md) | `actual_model` 定义、target 枚举与取消边界、缓存命中率样本集与可合并性、变更记录 |
| [docs/spec/CHANGELOG.md](../../../../docs/spec/CHANGELOG.md) | 顶部登记 analytics 0002 |

> 注：本次变更为纯设计文档修订，**无 Rust / 前端代码变更**；文中 schema 与行为均为「目标实现」设计，尚未实现，实现时以本文为准。

## 测试

- 设计层面新增/明确的测试场景（见 spec.md「测试策略」关键场景 11-13）：
  - **client_cancelled 边界**：无在途尝试时取消 → `request_results` 一条 `client_cancelled`、`target_attempts` 0 条；在途取消 → 尝试记 `cancelled`。
  - **缓存命中率 Rollup 对照**：`cached_known_count` / `cached_hit_count` / `prompt_tokens_cached_known` 与明细口径一致；unknown 不计分母；跨桶合并结果 == 明细计算值。
  - **失败原因映射合法组合**：`(failure_reason, final_status)` 组合符合映射表，非法组合不得产生。

## 验证

- 文档一致性检查：spec 与 requirements 口径对齐（actual_model 语义、缓存样本集、target 枚举、UTC/unix ms）；全部已确认契约保留（UUID v7 仅 DB、route_name 稳定身份、旧 call_logs 兼容、旧历史不回填、24h 精确 + 1h Rollup、每轮 ≤24 桶、UTC 完成时间、scope API、Top20+others、low_sample、崩溃小窗口丢失）。
- 运行时验证待实现后按 spec.md「测试策略」执行。

## 迁移兼容性

- 纯文档变更，无运行时迁移，不影响已实现的 `call_logs` 与旧 `/v1/analytics/*`、`/api/logs` 接口（行为不变）。
- 未来实现时 schema 以本文为准：新增列为新约束，`target_attempts.status` 三值枚举向后不冲突（新表尚未存在）。
- 旧历史不强制回填身份，旧记录身份字段保持为空。
