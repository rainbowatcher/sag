---
created: 2026-08-17
updated: 2026-08-17
type: 新增
---

# 0003 — 记录 request_id(v4) 与身份 ID(v7) 的版本分层决策

## 动机

代码审计发现仓库同时使用 UUID v4（`request_id` / `trace_id`）与 v7（`route_id` / `target_id`），曾担心是历史遗留技术债、是否应统一为单一版本。从「最终编译产物大小」与「ID 随机性对业务的实际增益」两个角度分析后确认：v4/v7 共存是有意的语义分层，不是漂移；统一为任一版本都会失去真实业务增益且换不来产物大小收益。本变更把该决策与理据落档，避免后续误清理。

## 需求变更

- `requirements.md`「身份模型」节新增一条：`request_id` 与 `route_id`/`target_id` 使用不同 UUID 版本的分工与理由（v4 给请求追踪、v7 给持久身份），作为既有行为的正式记录（非新行为）。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`docs/spec/analytics/spec.md`](../../../docs/spec/analytics/spec.md) | 「关键决策」节新增 `### request_id 用 v4、身份 ID 用 v7 的分层`，记录两层 ID 的业务角色、版本选择理由、共存代价与统一为单一版本的代价。 |
| [`docs/spec/analytics/requirements.md`](../../../docs/spec/analytics/requirements.md) | 「身份模型」节追加 `request_id` 用 v4 的分工说明；末尾「变更记录」节追加 0003 条目。 |
| [`docs/spec/CHANGELOG.md`](../../../docs/spec/CHANGELOG.md) | 顶部按日期插入 0003 条目。 |

本变更不涉及任何代码改动——仅记录已存在的设计决策。

## 测试

无新增测试。本变更是文档记录，不改变运行时行为。
