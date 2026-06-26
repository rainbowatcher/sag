# 技术规格

本目录包含 SAG 每个功能模块的**需求定义**（requirements.md）和**技术规格**（spec.md）。

## 阅读指南

| 文件 | 受众 | 说明 |
|------|------|------|
| `requirements.md` | 开发者 / Agent | 功能要解决什么问题、预期行为、边界、与其它模块交互 |
| `spec.md` | 开发者 / Agent | 关键文件、数据流、设计决策、trade-off、不变式、测试策略 |

需求文档定义 **"做什么"**，规格文档定义 **"怎么做"**。两者共同构成功能的单一真相来源（source of truth）。

## 模块索引

| 模块 | 说明 |
|------|------|
| [架构](/spec/architecture/requirements) | 项目结构、子包职责、请求生命周期 |
| [配置](/spec/config/requirements) | TOML 配置加载、热重载、验证 |
| [路由](/spec/routing/requirements) | 模型匹配、目标选择策略、故障转移 |
| [调度](/spec/dispatch/requirements) | 请求分发、重试退避、流式处理 |
| [供应商](/spec/providers/requirements) | Provider trait、格式翻译、适配器实现 |
| [熔断器](/spec/circuit-breaker/requirements) | 错误分类、冷却时长、自动恢复 |
| [认证](/spec/auth/requirements) | Bearer token、可选认证 |
| [分析](/spec/analytics/requirements) | SQLite 日志、统计聚合 |
| [日志](/spec/logging/requirements) | Tracing、日志轮转、清理 |
| [管理 API](/spec/admin/requirements) | REST 端点、CRUD 语义 |
