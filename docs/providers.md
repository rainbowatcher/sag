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
