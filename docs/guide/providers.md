# 供应商

供应商是 SAG 代理请求的后端 LLM 服务。每个供应商实现了 `LlmProvider` trait。

## 公共字段

所有供应商类型支持以下通用配置字段（详见[配置 → Provider 完整字段](/guide/configuration#provider-完整字段)）：

| 字段 | 说明 |
|------|------|
| `api_keys` | API 密钥列表 |
| `key_strategy` | 多密钥选择策略（`fill_first` / `round_robin`） |
| `base_url` | 供应商 API 基础 URL |
| `timeout_secs` | 上游请求超时（秒），默认 600。流式请求不受此约束（见「流式超时」），仅作为非流式请求的整体安全网 |
| `chunk_timeout_secs` | 流式 chunk 间最大空闲时间（秒），省略则无限等待 |
| `ttfb_timeout_secs` | 流式请求等待上游响应头（首字节）的最大时间（秒），默认 60。收到响应头后停止计时，不约束后续 body 流 |
| `enabled` | 是否启用，默认 `true` |
| `enabled_models` | 模型白名单，空列表 = 全部允许 |
| `headers` | 附加到上游请求的自定义 HTTP 头 |
| `body_overrides` | 合并到上游请求体的字段覆盖，对该供应商**所有模型**生效。优先级低于 `model_overrides` 和路由 target 的 `extra` |
| `model_overrides` | 逐模型 body 字段覆盖，以 `real_model` 为 key。优先级高于 `body_overrides`，低于路由 target 的 `extra`。推荐用于避免在多个路由中重复配置同一模型的参数 |
| `role_mappings` | 角色名映射表（如 `{ "developer" = "system" }`） |

## 内置供应商

### OpenAI

接受任何兼容 OpenAI 的端点（包括 Azure OpenAI、本地 LLM 服务器等）。

- **聊天**：`POST /v1/chat/completions`
- **流式**：SSE 流式透传
- **图像**：`POST /v1/images/generations`
- **模型列表**：管理面板默认请求 `models`，可通过 provider 的 `models_endpoint` 配置覆盖路径。

### Anthropic

将 OpenAI 格式的请求转换为 Anthropic Messages API 格式。

- 系统提示从 `messages` 中提取，作为 `system` 参数发送
- 内容块转换为 Anthropic 的格式
- 认证使用 `x-api-key` 请求头

#### Anthropic 缓存控制

当 `anthropic_cache_control = true`（默认启用）时，SAG 自动在 Anthropic 请求的 system prompt、tools 和对话历史中注入 `cache_control: {"type": "ephemeral"}` 断点。这可以显著降低具有稳定前缀的重复请求的 API 成本——上游会缓存 system prompt 和 conversation history，后续请求只需为增量内容付费。

此功能仅对 `provider_type = "anthropic"` 的供应商有效，其他供应商类型会忽略该字段。可在 provider 配置中显式设为 `false` 关闭：

```toml
[providers.anthropic]
provider_type = "anthropic"
anthropic_cache_control = false   # 关闭缓存控制
```

## Body Override

SAG 提供三层 body override 机制，让你在不同粒度上控制上游请求参数。优先级从低到高：

1. **`body_overrides`** — 供应商全局默认值，影响该供应商的所有模型
2. **`model_overrides.<real_model>`** — 模型专属覆盖，仅对匹配的 `real_model` 生效
3. **`RouteTarget.extra`** — 路由目标级覆盖，最高优先级

### body_overrides vs model_overrides

| 特性 | `body_overrides` | `model_overrides` |
|------|------------------|-------------------|
| 作用范围 | 供应商的所有模型 | 指定的 `real_model` |
| 典型用途 | 全局默认参数（如统一 `max_tokens`） | 避免在多个路由中重复配置同一模型的参数 |
| 优先级 | 低（可被 `model_overrides` 和 `extra` 覆盖） | 中（覆盖 `body_overrides`，可被 `extra` 覆盖） |
| TOML 格式 | 扁平 key-value | 两级 map，以模型名为 key |

```toml
# body_overrides：所有 openai 请求都带 max_tokens=4096
[providers.openai.body_overrides]
max_tokens = 4096

# model_overrides：仅对指定模型生效
[providers.openai.model_overrides]
gpt-4o = { temperature = 0.7 }
gpt-4o-mini = { max_tokens = 2048 }  # 覆盖全局的 max_tokens
```

> **最佳实践**：优先使用 `model_overrides` 配置模型专属参数，将 `extra` 保留给路由级差异。详见[配置 → Body Override 优先级详解](/guide/configuration#body-override-优先级详解)。

## 流式超时

流式请求（`stream: true`）的超时分为两个独立阶段，互不影响：

### 首字节超时 `ttfb_timeout_secs`

`ttfb_timeout_secs` 控制从发起请求到收到上游**响应头**（time-to-first-byte）的最大等待时间，覆盖建连、TLS 握手与上游首字节延迟。

- 默认 60 秒；省略时使用该默认值
- 计时在收到响应头的一刻**停止**，此后 body 流式传输不再受任何整体死线约束——因此正常的长对话不会在固定时长（如旧版的 120s）被截断
- 超时被视为**可重试**错误（HTTP 504 / `TTFB_TIMEOUT`），调度器会 fallback 到下一个 target
- 仅对流式请求生效

### chunk 空闲超时 `chunk_timeout_secs`

`chunk_timeout_secs` 控制流式响应中两个 SSE chunk 之间的最大空闲时间。当上游长时间无数据时，SAG 会主动断开连接并返回超时错误，而非无限等待。这对防止「挂起」连接占用网关资源非常重要。

- 为 `None` 时（默认）：流式连接在收到响应头后无限等待后续 chunk
- 为 `Some(n)` 时：超过 `n` 秒无 chunk 则断开
- 仅在收到响应头之后生效（与 `ttfb_timeout_secs` 阶段衔接）

### 非流式

非流式请求（`stream: false`）不使用上述两个超时，仍由 `timeout_secs`（默认 600）控制整个 HTTP round-trip。

## 角色映射

某些 LLM 供应商不支持 OpenAI 的所有角色（如 `developer`）。角色映射功能允许将请求中的角色名映射为上游供应商支持的角色。

- **配置方式**：在 `[providers.<name>]` 下设置 `role_mappings` 字段
- **行为**：键值对匹配，大小写敏感，空映射时零开销
- **示例**：`role_mappings = { "developer" = "system" }` 将所有 `developer` 消息转为 `system`

**边界情况**：Anthropic 仅接受单条 system prompt。如果映射后产生多条 system 消息（例如原始请求同时包含 `system` 和 `developer` 消息且映射 `developer → system`），只有第一条 system 消息生效，其余被丢弃。这是 Anthropic Messages API 的约束。

## 自定义供应商

实现 `LlmProvider` trait 以添加对其他供应商的支持：

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat(&self, request: ChatCompletionRequest) -> Result<ChatCompletionResponse>;
    async fn chat_stream(&self, request: ChatCompletionRequest) -> Result<SseResponse>;
    async fn chat_image(&self, request: ImageGenerationRequest) -> Result<ImageGenerationResponse>;
}
```
