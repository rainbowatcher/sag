---
created: 2026-06-03
updated: 2026-06-27
---

# 变更时间线

按时间倒序列出所有模块的变更记录。

## 2026-07-06

- **调度**：[0002 路由删除后缓存泄漏修复](dispatch/changes/0002-cache-leak-fix.md) — `resolve_route()` 开头惰性清理已删除路由的 `fill_first_orders`、`route_counters`、`active_targets` 缓存条目

## 2026-06-27

- **UI**：[0002 表格固定表头与玻璃拟态效果](ui/changes/0002-sticky-header.md) — 四个表格页面 sticky 表头、`[&>div]:h-full` 高度传递模式、玻璃拟态表头

## 2026-06-26

- **UI**：[0001 初始化与大屏幕响应式优化](ui/changes/0001-initial.md) — 管理面板规格文档、`xl:`/`2xl:` 断点响应式布局
- **日志**：[0002 日志轮转与清理](logging/changes/0002-rotation-and-cleanup.md) — TTY 自适应输出、每日轮转、启动时清理 7 天前日志

## 2026-06-14

- **熔断器**：[0001 初始化](circuit-breaker/changes/0001-initial.md) — 按错误类型分类冷却

## 2026-06-03

- **架构**：[0001 初始化](architecture/changes/0001-initial.md) — 8 子包 workspace、启动编排
- **配置**：[0001 初始化](config/changes/0001-initial.md) — TOML 加载、热重载、原位保存
- **路由**：[0001 初始化](routing/changes/0001-initial.md) — 路由匹配、目标选择、shuffle 缓存
- **调度**：[0001 初始化](dispatch/changes/0001-initial.md) — 分发、重试退避、流式处理
- **供应商**：[0001 初始化](providers/changes/0001-initial.md) — OpenAI/Anthropic 适配器
- **认证**：[0001 初始化](auth/changes/0001-initial.md) — Bearer token 可选认证
- **分析**：[0001 初始化](analytics/changes/0001-initial.md) — SQLite 异步调用日志
- **日志**：[0001 初始化](logging/changes/0001-initial.md) — tracing、HTTP 日志、trace ID
- **管理 API**：[0001 初始化](admin/changes/0001-initial.md) — 供应商/路由 CRUD
