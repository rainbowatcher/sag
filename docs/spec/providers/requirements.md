---
created: 2026-06-03
updated: 2026-10-19
---

# 供应商

## 目标

将 OpenAI 格式的请求翻译为不同 LLM 供应商的原生 API 格式，屏蔽上游差异，对客户端暴露统一的接口。

## 预期行为

### reasoning_effort 归一化

1. 请求入口（`sag-router/src/routes.rs`）在 dispatch 前对 `reasoning_effort` 做归一化：`trim()` + `to_ascii_lowercase()`
2. 空字符串（`""` 或纯空白）归一化为 `None`
3. 未知值不拒绝，仅小写化后透传（网关设计原则：拒绝未知值是反模式）

### 请求翻译

1. 客户端发送 OpenAI 兼容请求（JSON body）
2. `reasoning_effort` 在入口处归一化（见上节）
3. 根据 `provider_type` 选择适配器
4. 适配器将请求翻译为上游格式：
   - **OpenAI**：透传（本身就是 OpenAI 格式）
   - **Anthropic**：提取 system messages → 合并为 `system` 参数，翻译 content blocks、stop_sequences、tool_choice；**将 `reasoning_effort` 翻译为 `thinking: {type: "enabled", budget_tokens: N}`**（见「Anthropic Extended Thinking 翻译」节）
5. 发送请求到上游（认证根据供应商类型：OpenAI → `Authorization: Bearer`，Anthropic → `x-api-key`）
6. 收到响应后翻译回 OpenAI 格式

### 工具定义跨 codec 保真

工具定义在 IR 中以 `IrTool` 承载，`raw` 保留协议特有的工具字段。三条 codec（chat / Responses / Anthropic）共享同一套读写约定：

1. **两种布局**：chat 来源用双层结构 `{"__tool_extra": {...}, "__function_extra": {...}}`（工具级字段 / `function` 级字段分开存放）；Responses 与 Anthropic 来源用扁平平铺（工具对象除已映射字段外的其余字段原样保留）
2. **chat 来源判别式**：`raw` 中 `__tool_extra` 键的**存在性**即「chat 来源」的结构标记，不依赖任何业务字段
3. **同协议零合成**：chat → chat 往返逐字还原，源工具没有 `type` 就继续没有——绝不凭空发明 `type`；源 `type` 为 `""` 时同样逐字还原为 `""`（双层来源路径不做任何合成）
4. **跨协议允许合成默认值**：跨协议编码边界允许补齐目标协议必填项。Responses / Anthropic 的 function 工具在 wire 上不带 `type`，故 chat 出站对外来 IR 在 `type` 缺失或为 `""` 时合成 `"type": "function"`；Responses 出站保留（缺失时补）`"type": "function"`；Anthropic 出站**按值映射且对所有来源一致**：`type` 为 `"function"` 或 `""` 时省略（其 tool-type 枚举无 `function`，自定义工具本身即 function 语义），`"custom"` / `web_search*` 等其余值逐字保留，缺失维持缺失
5. **`strict` 位置由协议决定**：chat 为 `function.strict`，Responses 为工具根部；跨协议时按目标协议落位
6. **出站禁止内部伪字段**：`__tool_extra` / `__function_extra` 仅存在于 IR，任何出站 JSON 都不得包含它们
7. **hosted 工具丢弃必须记 loss**：hosted `web_search*` 工具的目标协议不支持时丢弃，并记 `LossReason::UnsupportedByTarget` 进 `CallLog`；禁止静默丢弃，也禁止压成假 function 形状

### 响应翻译（Anthropic → OpenAI）

- `content` 块映射：`text` → `text`，`tool_use` → `tool_calls`
- `stop_reason` 映射：`end_turn` / `stop_sequence` → `stop`，`max_tokens` → `length`，`tool_use` → `tool_calls`
- Usage 直译：`input_tokens` → `prompt_tokens`，`output_tokens` → `completion_tokens`，`thinking_tokens` → `reasoning_tokens`（Anthropic Extended Thinking 专属）

### 角色映射

通过 `role_mappings` 配置将请求中的角色映射为上游支持的角色：
- 例如 `{ "developer" = "system" }` 将 developer 消息转为 system 消息
- 大小写敏感
- 空映射时零开销（不额外遍历）
- 可在管理面板 Provider 新增/编辑表单的 Advanced tab 中通过 Role Mappings 键值编辑器维护（无需手工编辑配置文件）

> **Anthropic 边界情况**：Anthropic 仅接受单条 system prompt。如果映射后产生多条 system 消息，只有第一条生效，其余被丢弃。

### Anthropic Extended Thinking 翻译

客户端 `reasoning_effort` 字段通过 7 档映射表翻译为 Anthropic 的 `thinking: {type: "enabled", budget_tokens: N}`：

| 档位 | `budget_tokens` | 场景 |
|------|----------------|------|
| `none` | **省略 thinking** | 不需要推理 |
| `minimal` | 1,024 | 简单事实查询 |
| `low` | 2,048 | 简单分类、格式化 |
| `medium` | 4,096 | 摘要、邮件、简单编码 |
| `high` | 8,192 | 复杂推理、多步逻辑 |
| `xhigh` | 16,384 | 深度分析、架构规划 |
| `max` | 32,768 | 极限推理 |

**递归降级**：约束 `budget_tokens ≤ max_tokens - 1024`（预留 1,024 token 输出余量）。若目标档位不满足，向下搜索第一个满足约束的档位；连 `minimal(1024)` 都塞不下则放弃 thinking。不悄悄抬高 `max_tokens`（避免成本不可预期）。

未知档位：不启用 thinking，与网关设计原则一致（不拒绝、不 panic）。

### Anthropic 缓存控制

当 `anthropic_cache_control = true` 时，自动在以下位置注入 `cache_control: {"type": "ephemeral"}`：
- System prompt 末尾
- 最后一条 tool 定义的末尾
- 对话历史中除最后一条外的所有 message

目的：上游缓存重复的 system prompt 和对话前缀，降低后续请求的 token 成本。

### API Key 策略

通过 `key_strategy` 控制多 key 时的选择方式：

| 策略 | 行为 |
|------|------|
| `fill_first` | 始终使用第一个 key，失败后尝试下一个 |
| `round_robin` | 请求轮询分发到每个 key |

单 key 时两种策略行为相同（validator 会对单 key + RoundRobin 发 warning）。

### 流式超时

`chunk_timeout_secs` 控制 SSE 流中两个 chunk 之间的最大空闲时间：
- `None`（默认）：无限等待
- `Some(n)`：超过 `n` 秒无数据 → 断开连接，返回超时错误
- 仅对流式请求生效；非流式请求使用 `timeout_secs`

### Provider 禁用

`enabled = false` 时：
- 路由跳过该 provider 的所有 target
- 管理面板中显示为禁用状态
- 管理 API 仍可对其进行 CRUD 操作

### 模型白名单

`enabled_models` 非空时，只有列表中的模型可以使用该 provider。空列表（默认）= 允许所有模型。

## 配置 Schema

参见 [配置 — Provider 完整字段](/guide/configuration#provider-完整字段)

## 功能边界

### 包含
- OpenAI 和 Anthropic 适配器
- 请求/响应格式翻译
- 角色映射
- Anthropic 缓存控制注入
- API Key 选择策略
- Provider 级别的 headers 和 body_overrides

### 不包含
- 流式响应的 chunk-by-chunk 翻译（SSE 透传）
- 图片生成以外的多模态（当前仅 OpenAI 图像生成端点）
- Provider 级别的 rate limiting（由熔断器处理上游 429）

## 与其它模块的交互

- **路由**：选择 provider 和 real_model
- **调度**：调用 provider.chat() / provider.chat_stream()
- **配置**：provider 配置加载和热重载

## 变更记录

- [0007 跨 codec 工具字段保真（统一 `IrTool.raw` 双层约定）](changes/0007-tool-raw-cross-codec-fidelity.md)
- [0004 供应商 role_mappings 编辑入口](changes/0004-role-mappings-editor.md)
- [0002 Anthropic reasoning_effort 翻译](changes/0002-anthropic-reasoning-effort.md)
- [0001 初始化](changes/0001-initial.md)
