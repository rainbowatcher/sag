# 管理 API

管理 API 在 `/api` 前缀下暴露 REST 端点，用于运行时管理供应商和路由。

## 认证

管理端点使用 Bearer token 认证，与[模型 API](/models-api#认证) 共享相同的 `[gateway_auth]` 配置：

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
  "base_url": "https://api.openai.com/v1",
  "models_endpoint": "models",
  "timeout_secs": 120
}
```

`models_endpoint` 是可选字段，表示 provider 级 models 列表路径，默认 `models`。传入 `/v1/models/` 会被规范化为 `v1/models` 并持久化。

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

只更新请求体中传入的字段，未传入的字段保持原值。

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

向指定的模型发送一条最短消息（`say 'hi'`），验证连接和认证是否正常。

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
