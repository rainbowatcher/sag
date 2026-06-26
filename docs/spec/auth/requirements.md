# 认证 — 需求

## 目标

为网关的模型 API 和管理 API 提供可选 Bearer token 认证。

## 预期行为

### 认证模式

| 配置 | 行为 |
|------|------|
| 无 `[gateway_auth]` 段 | 完全开放，无需认证 |
| `api_keys = []` | 完全开放（同无配置） |
| `api_keys = ["key1", "key2"]` | 需要 `Authorization: Bearer <key>` |

### 错误响应

无效或缺失 key → `401 UNAUTHORIZED`：

```json
{
  "error": {
    "code": "UNAUTHORIZED",
    "message": "invalid api key"
  }
}
```

### Dashboard 持久化

前端登录后，连接信息（URL + API Key）保存在 `localStorage`，有效期 30 天。重启服务后自动重连，无需重新输入。

## 功能边界

### 包含
- Bearer token 认证
- 可选开关（无配置 = 开放）
- 多个 API key 支持（任一有效即可）

### 不包含
- JWT / OAuth2
- 基于角色的访问控制（RBAC）
- API Key 的独立过期时间
- 速率限制

## 与其它模块的交互

- **模型 API**：中间件拦截，认证失败返回 401
- **管理 API**：同一认证逻辑
- **Dashboard 前端**：localStorage 持久化
