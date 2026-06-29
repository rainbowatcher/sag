---
created: 2026-06-03
updated: 2026-06-03
type: 新增
---

# 0001 — 日志系统初始化

## 动机
需要结构化日志输出，支持 TTY 自适应、HTTP 请求日志和 trace ID 链路追踪。

## 后续变更
- 0002：日志轮转与清理（2026-06-26）

## 需求变更
- requirements.md 全部初始定义

## 代码变更
| 文件 | 变更 |
|------|------|
| [sag-gateway/src/tracing_init.rs](../../../../sag-gateway/src/tracing_init.rs) | tracing-subscriber 初始化（TTY 检测） |
| [sag-router/src/middleware/trace.rs](../../../../sag-router/src/middleware/trace.rs) | Trace ID 中间件（UUID v4 注入） |
| [sag-router/src/middleware/http_log.rs](../../../../sag-router/src/middleware/http_log.rs) | HTTP 请求/响应体日志 |
| [sag-config/src/config.rs](../../../../sag-config/src/config.rs) | LoggingConfig、LogFormat |

## 影响的需求条目
- 全部（初始定义，不含轮转/清理）
