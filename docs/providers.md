# 供应商

供应商是 SAG 代理请求的后端 LLM 服务。每个供应商实现了 `LlmProvider` trait。

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
