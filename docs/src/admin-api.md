# 管理 API

SAG 在 `/api` 前缀下暴露 REST 管理 API，用于运行时管理。

## 认证

管理端点需要使用 `Authorization: Bearer <key>` 请求头携带有效的网关 API 密钥。

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
  "type": "openai",
  "api_key": "...",
  "base_url": "https://api.openai.com/v1",
  "priority": 100
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
  "model": "gpt-4",
  "endpoint": "chat",
  "priority": 100
}
```

### 列出路由

```
GET /api/routes
```

返回所有配置的路由规则。
