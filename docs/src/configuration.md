# 配置

SAG 使用基于 TOML 的配置文件（默认：`gateway.toml`）。

## 示例

```toml
[server]
host = "0.0.0.0"
port = 3000

[analytics]
enabled = true
storage_dsn = "analytics.db"

# 网关客户端认证（可选 — 删除此段以关闭认证）
[gateway_auth]
api_keys = [
    "your-api-key-1",
    "your-api-key-2",
]

[providers.openai]
provider_type = "openai"
api_key = "${OPENAI_API_KEY}"
base_url = "https://api.openai.com/v1"
priority = 10
max_retries = 2
timeout_secs = 120
supported_params = [
    "model", "messages", "temperature", "top_p", "n",
    "stream", "stop", "max_tokens", "presence_penalty",
    "frequency_penalty", "seed", "tools", "tool_choice",
    "response_format",
]

[providers.anthropic]
provider_type = "anthropic"
api_key = "${ANTHROPIC_API_KEY}"
base_url = "https://api.anthropic.com/v1"
priority = 1
max_retries = 2
timeout_secs = 120
supported_params = [
    "model", "messages", "temperature", "top_p",
    "stream", "stop", "max_tokens", "tools", "tool_choice",
]

[[routes]]
model_name = "gpt-4o"
targets = [
    { provider = "openai", real_model = "gpt-4o", priority = 10 },
]

[[routes]]
model_name = "claude-sonnet"
targets = [
    { provider = "anthropic", real_model = "claude-3-5-sonnet-20241022", priority = 10 },
    { provider = "openai", real_model = "gpt-4o-mini", priority = 1 },
]

# 兜底规则：未匹配的模型直接转发到指定供应商
[[routes]]
model_name = "*"
targets = [
    { provider = "openai", priority = 10 },
]
```

## 认证

`[gateway_auth]` 段配置 Dashboard 和管理 API 的访问凭证：

- `api_keys` — 允许的 API Key 列表，客户端需在请求头中携带 `Authorization: Bearer <key>` 进行认证
- 删除（或注释掉）整个 `[gateway_auth]` 段可关闭认证，允许所有请求通过
- 保留 `[gateway_auth]` 段但设 `api_keys = []`（空列表）等同于关闭认证，行为与删除该段一致

Dashboard 前端在登录页输入 URL 和 API Key 后，会将连接信息保存在浏览器 `localStorage` 中（有效期 30 天）。重启服务后自动使用保存的 key 重连，无需重新输入。

## 热重载

配置文件使用 `notify` 库监听变化。更新后，供应商和路由会在不重启服务器的情况下即时生效。认证配置（`[gateway_auth]`）和服务器设置（`[server]`）暂不支持热更新，需重启服务。

## 环境变量

供应商 API 密钥可以使用 `${VAR_NAME}` 语法通过环境变量设置。例如：

```toml
api_key = "${OPENAI_API_KEY}"
```

SAG 在启动时会读取环境变量 `OPENAI_API_KEY` 的值替换该配置项。环境变量也支持在 `[[routes]]` 的 `real_model` 中使用。
