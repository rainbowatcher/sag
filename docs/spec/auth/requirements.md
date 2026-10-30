---
created: 2026-06-03
updated: 2026-06-03
---

# 认证

## 目标

为网关的模型 API 和管理 API 提供可选 Bearer token 认证。

## 预期行为

### 认证模式

| 配置 | 行为 |
|------|------|
| 无 `[gateway_auth]` 段 | 完全开放，无需认证 |
| `api_keys = []` | 加载时归一化为「段缺失」，即完全开放（同无配置） |
| `api_keys = ["key1", "key2"]` | 需要 `Authorization: Bearer <key>` |

补充约束：

- 空列表在加载（启动与热重载）时被归一化为「无 `[gateway_auth]` 段」，即关闭认证，并记录一条 `gateway authentication is disabled` warning。
- 只含空白字符的 key 列表（`api_keys = [""]`、`["   "]`）被校验拒绝，进程不启动；`gateway_auth` 段必须至少含一个非空白 key。
- 非空白 key 与空白条目混写可通过校验，但空白条目永远匹配不到任何凭据。
- 内存中绕过加载归一化、直接构造出的空列表或全空白列表，由认证中间件一律拒绝（`401 UNAUTHORIZED`），包括 `Authorization: Bearer ` 携带空凭据的请求；该分支绝不 fail-open。

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

## 变更记录

- [0001 初始化](changes/0001-initial.md)
- [0009 空 `api_keys` 归一化为关闭鉴权并告警](../config/changes/0009-empty-api-keys-normalize-to-disabled-auth.md) — 空列表归一化为段缺失、空白 key 被拒、中间件 fail-closed
