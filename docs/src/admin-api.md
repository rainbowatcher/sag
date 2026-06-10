# 管理 API

SAG 在 `/api` 前缀下暴露 REST 管理 API，用于运行时管理。Dashboard 前端通过这些端点管理供应商和路由。

## 认证

管理端点使用 Bearer token 认证：

```
Authorization: Bearer <api_key>
```

API Key 在服务端配置文件 `gateway.toml` 的 `[gateway_auth]` 段中定义。认证行为：

| 配置 | 行为 |
|------|------|
| 无 `[gateway_auth]` 段 | 完全开放，无需认证 |
| `api_keys = []` | 完全开放（同上） |
| `api_keys = ["key1", "key2"]` | 只有提供有效 key 的请求可以通过 |

### Dashboard 登录流程

Dashboard 前端（Vue SPA）在 `/connect` 页面提供登录表单：

1. 用户输入 Gateway URL 和 API Key，点击 Connect
2. 前端先调 `GET /health` 确认服务可达
3. 再调 `GET /api/providers`（带 `Authorization: Bearer <key>`）验证 key 有效性
4. 成功后，连接信息（URL + API Key）保存在浏览器 `localStorage` 中，有效期 30 天
5. 后续页面加载时自动读取 localStorage 重连，无需重复输入

### localStorage 持久化

- 重启服务后，浏览器中保存的连接信息不会丢失
- 如 key 无效或网络暂时不可达，前端会保留已保存的 key，跳转到 `/connect` 页面供用户修正
- 用户主动断开连接时才会清除 localStorage

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
  "api_key": "...",
  "base_url": "https://api.openai.com/v1",
  "priority": 10,
  "max_retries": 2,
  "timeout_secs": 120,
  "supported_params": ["model", "messages", "temperature", "stream", "max_tokens"]
}
```

### 更新供应商

```
PUT /api/providers
Content-Type: application/json

{
  "name": "my-provider",
  "base_url": "https://custom.com/v1",
  "priority": 5
}
```

只更新请求体中传入的字段，未传入的字段保持原值。

### 删除供应商

```
DELETE /api/providers
Content-Type: application/json

{
  "name": "my-provider"
}
```

删除供应商及其关联的路由目标。

### 添加路由目标

```
POST /api/routes
Content-Type: application/json

{
  "model_name": "gpt-4o",
  "api_endpoint": "/v1/chat/completions",
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
| `api_endpoint` | string | 否 | API 端点路径，默认 `"/v1/chat/completions"` |
| `strategy` | string | 否 | 目标选择策略（`failover` / `round_robin` / `fill_first`），默认 `failover` |
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

返回所有配置的路由规则。响应格式：

```json
[
  {
    "model_name": "gpt-4o",
    "api_endpoint": "/v1/chat/completions",
    "strategy": "failover",
    "targets": [
      {
        "provider": "openai",
        "real_model": "gpt-4o",
        "priority": 100,
        "extra": { "temperature": 0.7 }
      }
    ]
  }
]
```
