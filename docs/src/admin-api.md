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

### 删除供应商

```
DELETE /api/providers/{name}
```

删除供应商及其关联的路由目标。

### 添加路由目标

```
POST /api/providers/{name}/routes
Content-Type: application/json

{
  "model_name": "gpt-4o",
  "real_model": "gpt-4o",
  "priority": 10
}
```

### 列出路由

```
GET /api/routes
```

返回所有配置的路由规则。
