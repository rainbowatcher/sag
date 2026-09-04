---
created: 2026-09-04
updated: 2026-09-04
type: 修改
---

# 0004 — Token 命中率分母改为全部成功样本

## 动机

`token_cache_hit_rate` 原实现只把「缓存信息已知（`cached_tokens IS NOT NULL`）」的成功样本计入分母（known-only），导致 unknown 成功请求的 prompt_tokens 被排除在 Token 命中率之外：分母系统性偏小、命中率虚高，且不是真正的 token 加权平均。本次把 Token 命中率口径改为**所有成功样本**：`Σ COALESCE(cached_tokens, 0) / Σ prompt_tokens`，并同步修复明细查询与 rollup/长跨度混合路径，保证不同时间范围（≤24h 明细 vs >24h 混合）口径一致。

## 行为变化

- **Token 命中率分母**：由「缓存已知样本的 Σprompt_tokens」改为「**全部成功样本**的 Σprompt_tokens」；分子 = `Σ COALESCE(cached_tokens, 0)`（NULL / 0 贡献 0，其 prompt_tokens 仍计入分母）。分母为 0（无成功样本）时仍返回 `null`。
- **请求命中率不变**：仍 = `cache_hit / cache_known`（unknown 不计请求命中率分母，NULL 不作 0）。
- **失败请求仍排除**：失败 / 取消样本的 token 与 cached_tokens 均不进入统计。
- **明细与 Rollup 口径一致**：明细 SQL 的 `cache_prompt_tokens` 列改为 `SUM(CASE WHEN <success> THEN prompt_tokens)`（全部成功样本）；Rollup feed 改用各表 `prompt_tokens` 列作为 Token 命中率分母、`cached_tokens` 列作为分子。
- **`prompt_tokens_cached_known` 保留为参考计数**：Rollup 表该列继续写入（schema 与迁移不变），但不再是 Token 命中率分母；query.rs 内部读取结构移除该字段（避免死代码）。

## 需求变更

- requirements.md「指标口径（Token 与缓存）」：Token 命中率样本集改为**全部最终成功样本**，公式改为 `Σ COALESCE(cached_tokens, 0) / Σ prompt_tokens`（token 加权 SUM/SUM，非每请求比例的算术平均；NULL/0 分子贡献 0；分母为 0 返回 null）；请求命中率样本集保持「缓存信息已知的最终成功样本」不变。

## 代码变更

| 文件 | 变更 |
|------|------|
| [sag-analytics/src/query.rs](../../../../sag-analytics/src/query.rs) | 明细 SQL（route/target）`cache_prompt_tokens` 改为全部成功样本的 prompt_tokens 之和；`MetricSummary` / 模块文档注释更新；rollup feed（global/route/target）改用 `prompt_tokens` 列作 Token 命中率分母；移除内部 `RollupRouteRow` / `RollupTargetRow` 不再读取的 `prompt_tokens_cached_known` |
| [docs/spec/analytics/requirements.md](../../../../docs/spec/analytics/requirements.md) | Token 命中率口径更新；「变更记录」追加 0004 |
| [docs/spec/analytics/spec.md](../../../../docs/spec/analytics/spec.md) | 缓存归因与公式、Rollup 表列注释、缓存列口径、查询不变式、关键场景 12 更新；「变更记录」补 0003 并追加 0004 |
| [docs/spec/CHANGELOG.md](../../../../docs/spec/CHANGELOG.md) | 顶部按日期登记 analytics 0004 |

## 测试

- 更新 `cache_metrics_unknown_is_not_miss`：Token 命中率 = 5 / (10+10+10) = 5/30（unknown 计入分母），`cache_prompt_tokens = 30`。
- 更新 `cache_rates_none_when_denominator_zero_all_unknown`（改名 `cache_rates_unknown_in_token_denominator_not_in_request_denominator`）：全部 unknown 时请求命中率 `None`、Token 命中率 `0.0`（分母 = 全部成功 prompt_tokens）。
- 更新 `tokens_only_final_success_null_never_zero`：追加 Token 命中率 `0.0` 与 `cache_prompt_tokens = 10` 断言。
- 更新 `mixed_25h_rollup_plus_recent_facts`：追加 rollup + detail 混合的 Token 命中率断言（12 / 40）。
- 新增 `token_cache_hit_rate_all_success_and_detail_rollup_consistent`：覆盖 成功 + cached_tokens>0、成功 + cached_tokens=0、成功 + cached_tokens=NULL、失败请求被排除，并验证明细（≤24h）与 rollup（>24h 混合）在 route / target 两个 scope 下口径一致（30 / 175）。

## 验证

- `cargo test -p sag-analytics --lib`：89 passed，0 failed。
- `just test --tests`：全部 crate 单元 / 集成测试通过，0 failed。
- `cargo clippy -p sag-analytics --all-targets -- -D warnings`：无警告。
- `cargo fmt --all -- --check`：通过。
