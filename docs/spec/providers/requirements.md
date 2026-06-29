---
created: 2026-06-03
updated: 2026-06-03
---

# 供应商

## 目标

将 OpenAI 格式的请求翻译为不同 LLM 供应商的原生 API 格式，屏蔽上游差异，对客户端暴露统一的接口。

## 预期行为

### 请求翻译

1. 客户端发送 OpenAI 兼容请求（JSON body）
2. 根据 `provider_type` 选择适配器
3. 适配器将请求翻译为上游格式：
   - **OpenAI**：透传（本身就是 OpenAI 格式）
   - **Anthropic**：提取 system messages → 合并为 `system` 参数，翻译 content blocks、stop_sequences、tool_choice
4. 发送请求到上游（认证根据供应商类型：OpenAI → `Authorization: Bearer`，Anthropic → `x-api-key`）
5. 收到响应后翻译回 OpenAI 格式

### 响应翻译（Anthropic → OpenAI）

- `content` 块映射：`text` → `text`，`tool_use` → `tool_calls`
- `stop_reason` 映射：`end_turn` / `stop_sequence` → `stop`，`max_tokens` → `length`，`tool_use` → `tool_calls`
- Usage 直译：`input_tokens` → `prompt_tokens`，`output_tokens` → `completion_tokens`

### 角色映射

通过 `role_mappings` 配置将请求中的角色映射为上游支持的角色：
- 例如 `{ "developer" = "system" }` 将 developer 消息转为 system 消息
- 大小写敏感
- 空映射时零开销（不额外遍历）

> **Anthropic 边界情况**：Anthropic 仅接受单条 system prompt。如果映射后产生多条 system 消息，只有第一条生效，其余被丢弃。

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

- [0001 初始化](changes/0001-initial.md)
