# 配置

SAG 使用基于 TOML 的配置文件（默认：`gateway.toml`）。

## 示例

```toml
[server]
host = "0.0.0.0"
port = 3000

[analytics]
enabled = true
storage = "sqlite://sag-analytics.db"

[auth]
api_keys = ["sk-your-api-key"]

[providers.openai]
type = "openai"
api_key = "${OPENAI_API_KEY}"
base_url = "https://api.openai.com/v1"
priority = 100
max_retries = 3
timeout = 30

[providers.anthropic]
type = "anthropic"
api_key = "${ANTHROPIC_API_KEY}"
base_url = "https://api.anthropic.com"
priority = 50
max_retries = 2
timeout = 60

[[routes]]
model = "gpt-4*"
endpoint = "chat"

[[routes.targets]]
provider = "openai"
model = "gpt-4"
priority = 100

[[routes.targets]]
provider = "anthropic"
model = "claude-3-opus-20240229"
priority = 50
```

## 热重载

配置文件使用 `notify` 库监听变化。更新会在不重启服务器的情况下即时生效。

## 环境变量

供应商 API 密钥可以使用 `${VAR_NAME}` 语法通过环境变量设置。
