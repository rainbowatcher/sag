---
created: 2026-07-19
updated: 2026-07-19
type: 新增
---

# 0002 — Anthropic reasoning_effort 翻译为 extended thinking

## 动机

客户端传的 `reasoning_effort` 字段在 Anthropic 路由中被静默丢弃——`to_anthropic_request` 从不读取该字段，且 `AnthropicRequest` 结构体无 `thinking` 参数。结果：客户端意图从未送达上游，但 `CallLog` 仍原样记录该值，形成**虚构意图**，破坏「请求意图 × 成败」关联分析。

同时，`reasoning_effort` 在请求入口处无归一化，大小写变体（如 `"High "`、`"XHIGH"`）原样落入日志与上游，造成存储分裂。

## 需求变更

- **新增** `reasoning_effort` 归一化：请求入口 `trim` + `to_ascii_lowercase`，空字符串归一为 `None`
- **新增** Anthropic `reasoning_effort` → `thinking: {type: "enabled", budget_tokens: N}` 翻译
- **新增** 7 档位映射表（`none`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`），含递归降级策略
- **新增** Anthropic 响应中 `thinking_tokens` → `reasoning_tokens` 捕获
- **新增** `CallLog.reasoning_effort` 注释免责声明
- **修改** `ChatCompletionRequest.reasoning_effort` 注释补全 `max` 档位

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-core/src/types.rs`](../../../sag-core/src/types.rs) | `reasoning_effort` 注释补全 7 个 OpenAI 官方值（+ `max`）；新增 `normalize_reasoning_effort()` 方法（6 个测试）；`CallLog.reasoning_effort` 注释增加 Anthropic 免责声明 |
| [`sag-router/src/routes.rs`](../../../sag-router/src/routes.rs) | 请求入口调用 `normalize_reasoning_effort()`，在 dispatch 前归一化 |
| [`sag-providers/src/anthropic.rs`](../../../sag-providers/src/anthropic.rs) | 新增 `AnthropicThinking` 结构体 + `thinking` 字段；新增 `OUTPUT_MARGIN` + `resolve_budget()` 递归降级函数；`to_anthropic_request` 集成 thinking 翻译；`from_anthropic_response` / `extract_anthropic_usage` 捕获 `thinking_tokens` → `reasoning_tokens`（18 个测试） |

## 设计决策

### 档位 → budget_tokens 映射

| 档位 | `budget_tokens` |
|------|----------------|
| `none` | 省略 `thinking` 字段 |
| `minimal` | 1,024 |
| `low` | 2,048 |
| `medium` | 4,096 |
| `high` | 8,192 |
| `xhigh` | 16,384 |
| `max` | 32,768 |

### 递归降级

约束 `budget_tokens ≤ max_tokens - OUTPUT_MARGIN`（`OUTPUT_MARGIN = 1,024`）。从目标档位向下搜索第一个满足约束的档位；连 `minimal(1024)` 都塞不下则放弃 thinking（不悄悄提高 `max_tokens`，避免成本不可预期）。

## 测试

- `normalize_reasoning_effort` 6 个单元测试（trim + lowercase、空字符串归一、未知值保持、None 不 panic）
- `resolve_budget` 11 个单元测试（直接命中、递归降级 ×3、边界刚好/差 1、全部失败、未知档位）
- `to_anthropic_request` 5 个集成测试（high 启用 thinking、None 不启用、"none" 不启用、降级耗尽不启用、未知档位不启用）
- `AnthropicUsage` 1 个测试（`thinking_tokens` 反序列化）

## 影响的需求条目

- 「预期行为 — 请求翻译」（新增 `reasoning_effort → thinking` 步骤）
- 「预期行为 — 响应翻译」（Usage 中新增 `thinking_tokens` 捕获）
