# 管理 API

管理 API 在 `/api` 前缀下暴露 REST 端点，用于运行时管理供应商和路由。

## 认证

管理端点使用 Bearer token 认证，与[模型 API](/guide/models-api#认证) 共享相同的 `[gateway_auth]` 配置：

```
Authorization: Bearer <api_key>
```

---

## 端点

### 列出供应商

```
GET /api/providers
```

返回所有已配置的供应商。

### 添加供应商

```
POST /api/providers
Content-Type: application/json

{
  "name": "my-provider",
  "provider_type": "openai",
  "api_keys": ["..."],
  "key_strategy": "fill_first",
  "base_url": "https://api.openai.com/v1",
  "models_endpoint": "models",
  "timeout_secs": 600,
  "chunk_timeout_secs": null,
  "ttfb_timeout_secs": null,
  "enabled": true,
  "enabled_models": [],
  "headers": {},
  "body_overrides": {},
  "role_mappings": {},
  "anthropic_cache_control": true
}
```

**字段说明**：

| 字段 | 类型 | 必需 | 默认值 | 说明 |
|------|------|------|--------|------|
| `name` | string | 是 | — | 供应商唯一名称 |
| `provider_type` | string | 否 | `"openai"` | 供应商适配器类型：`"openai"` / `"anthropic"` |
| `api_keys` | array | 否 | `[]` | API 密钥列表 |
| `key_strategy` | string | 否 | `"fill_first"` | 多密钥选择策略：`"fill_first"` / `"round_robin"` |
| `base_url` | string | 否 | `"https://api.openai.com/v1"` | 供应商 API 基础 URL |
| `models_endpoint` | string | 否 | `"models"` | 模型列表端点路径，传入 `/v1/models/` 会被规范化为 `v1/models` |
| `timeout_secs` | integer | 否 | `600` | 上游请求超时（秒），仅约束非流式请求 |
| `chunk_timeout_secs` | integer \| null | 否 | `null` | 流式 chunk 间最大空闲时间（秒），`null` 表示无限等待 |
| `ttfb_timeout_secs` | integer \| null | 否 | `null`（默认 60） | 流式等待上游响应头（首字节）的最大时间（秒），收到响应头后停止计时 |
| `enabled` | boolean | 否 | `true` | 是否启用。`false` 时路由跳过且管理面板显示禁用 |
| `enabled_models` | array | 否 | `[]` | 模型白名单，空 = 全部允许 |
| `headers` | object | 否 | `{}` | 附加自定义 HTTP 头 |
| `body_overrides` | object | 否 | `{}` | 合并到上游请求体的字段覆盖 |
| `role_mappings` | object | 否 | `{}` | 角色映射表，如 `{ "developer": "system" }` |
| `anthropic_cache_control` | boolean | 否 | `true` | Anthropic 缓存控制注入，仅对 `"anthropic"` 类型有效 |

### 更新供应商

```
PUT /api/providers
Content-Type: application/json

{
  "name": "my-provider",
  "base_url": "https://custom.com/v1",
  "models_endpoint": "v1/models"
}
```

只更新请求体中传入的字段，未传入的字段保持原值。支持更新所有[添加供应商](#添加供应商)中列出的字段。

> `priority` 不是供应商字段，不会参与路由排序。路由目标的优先级应在 `targets[].priority` 上配置。

### 删除供应商

```
DELETE /api/providers
Content-Type: application/json

{
  "name": "my-provider"
}
```

删除供应商及其关联的路由目标。

### 列出供应商模型

```
GET /api/providers/{name}/models
```

从 provider 配置中返回已启用的 `enabled_models` 列表；不会实时调用上游 models endpoint。

#### 响应

```json
{
  "models": ["gpt-4o", "gpt-4o-mini", "gpt-3.5-turbo"]
}
```

> 如果上游返回错误或超时，返回空列表 `{ "models": [] }`。

### 测试模型连接

```
POST /api/model/test
Content-Type: application/json

{
  "model": "gpt-4o-mini",
  "api_keys": ["sk-xxx"],
  "base_url": "https://api.openai.com/v1",
  "provider_type": "openai",
  "headers": {}
}
```

向指定的模型发送一条最短消息（`hi`），验证连接和认证是否正常。

**请求体字段**：

| 字段 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `model` | string | 是 | 要测试的模型名 |
| `base_url` | string | 是 | 供应商 API 基础 URL |
| `provider_type` | string | 否 | 供应商类型，`"openai"` 或 `"anthropic"`，默认 `"openai"` |
| `api_keys` | array | 是 | 要测试的 API Keys，会逐一测试每个 key |
| `headers` | object | 否 | 附加的自定义请求头 |

**响应**：

```json
{
  "results": [
    {
      "key_prefix": "sk-xxx...",
      "success": true,
      "message": "OK"
    },
    {
      "key_prefix": "sk-yyy...",
      "success": false,
      "message": "Authentication failed: ..."
    }
  ]
}
```

### 创建路由

```
POST /api/routes
Content-Type: application/json

{
  "model_name": "gpt-4o",
  "api_endpoint": "/chat/completions",
  "targets": [
    {
      "provider": "openai",
      "real_model": "gpt-4o",
      "priority": 100,
      "extra": { "temperature": 0.7 }
    }
  ]
}
```

**字段说明**：

| 字段 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `model_name` | string | 是 | 客户端请求的模型名，`"*"` 为兜底路由 |
| `api_endpoint` | string | 否 | API 端点路径，默认 `"/chat/completions"` |
| `strategy` | string | 否 | 目标选择策略（`sticky_failover` / `fill_first` / `round_robin`），默认 `sticky_failover` |
| `targets` | array | 是 | 目标供应商列表 |

**target 字段说明**：

| 字段 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `provider` | string | 是 | 供应商名称 |
| `real_model` | string | 否 | 实际模型名，省略则透传客户端原始模型名 |
| `priority` | integer | 否 | 优先级，默认 0，值越大越优先 |
| `extra` | object | 否 | 针对该目标的额外 JSON 字段，合并到上游请求体 |

### 列出路由

```
GET /api/routes
```

返回所有配置的路由规则。

### 更新路由

```
PUT /api/routes
Content-Type: application/json

{
  "model_name": "gpt-4o",
  "api_endpoint": "/responses",
  "targets": [
    { "provider": "openai", "real_model": "gpt-4o", "priority": 5 }
  ]
}
```

### 删除路由

```
DELETE /api/routes
Content-Type: application/json

{
  "model_name": "gpt-4o"
}
```

### 读取网关设置

```
GET /api/settings
```

返回网关级配置快照：

```json
{
  "proxy_url": "http://proxy.example:8080",
  "api_keys": ["sk-client-a", "sk-client-b"]
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `proxy_url` | string \| null | 全局出站 HTTP/HTTPS 代理；`null` 表示直连 |
| `api_keys` | array | 网关客户端认证密钥（`[gateway_auth].api_keys`）；空数组表示未启用认证 |

### 更新网关设置

```
PUT /api/settings
Content-Type: application/json

{
  "proxy_url": "http://proxy.example:8080",
  "api_keys": ["sk-client-a"]
}
```

| 字段 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `proxy_url` | string \| null | 是 | 设置或清除全局代理（`null` / 空字符串清除） |
| `api_keys` | array | 否 | 替换网关客户端认证密钥列表；省略则保持不变；空数组关闭认证 |

**校验**：

- `proxy_url` 必须是合法 `http://` / `https://` URL，且不含 userinfo；否则 400
- `api_keys` 中不得包含空白条目；否则 400
- 将 `proxy_url` 清空时，若存在 `proxy_mode = "enabled"` 的供应商会返回 400

**副作用**：变更会立即写入配置文件并在内存生效；认证中间件每次请求读取当前配置，因此密钥变更即时生效，无需重启。
