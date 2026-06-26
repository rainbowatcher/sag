# 管理 API — 需求

## 目标

提供 REST API 在运行时管理供应商和路由，支持 CRUD 操作和模型连接测试。

## 预期行为

### 供应商管理

- `GET /api/providers` — 列出所有供应商
- `POST /api/providers` — 添加供应商，返回 409 如果 name 已存在
- `PUT /api/providers` — 更新供应商，PATCH 语义（只更新传入字段）
- `DELETE /api/providers` — 删除供应商及其关联路由 target

### 路由管理

- `GET /api/routes` — 列出所有路由
- `POST /api/routes` — 创建路由，返回 409 如果 model_name 已存在
- `PUT /api/routes` — 更新路由（全量替换）
- `DELETE /api/routes` — 按 model_name 删除路由

### 模型测试

- `POST /api/model/test` — 发送最小消息到指定模型，验证连接和认证
- 支持多个 API key，逐一测试并返回每个 key 的结果

### 供应商模型查询

- `GET /api/providers/{name}/models` — 返回 provider 的 `enabled_models` 列表

### 日志查询

- `GET /api/logs` — 查询调用日志，支持分页和过滤

### 认证

所有管理端点使用与模型 API 相同的 Bearer token 认证。

## 功能边界

### 包含
- 供应商 CRUD
- 路由 CRUD
- 模型连接测试
- 配置持久化（修改后自动保存到 TOML）

### 不包含
- 批量操作
- 配置回滚
- 操作审计日志

## 与其它模块的交互

- **配置**：修改后调用 `save_config_to_file()` 持久化 + 热重载
- **供应商**：调用 provider 进行模型测试
- **分析**：查询调用日志
