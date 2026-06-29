---
created: 2026-06-03
updated: 2026-06-26
---

# 日志 — 需求

## 目标

提供结构化日志输出，支持两种格式、环境自适应输出目标和自动轮转清理。

## 预期行为

### 输出目标

| 环境 | stdout | 日志文件 |
|------|--------|---------|
| TTY（开发） | ✅ 输出 | ❌ 不写文件 |
| 非 TTY（生产/test-env） | ❌ DEVNULL | ✅ `logs/sag.YYYY-MM-DD.log` 每日轮转 |

### 日志格式

通过 `[logging].format` 控制：

| 值 | 格式 |
|----|------|
| `compact` | 人类可读单行，带时间戳和 target |
| `json` | 结构化 JSON，适合日志聚合器 |

TTY 环境启用 ANSI 颜色。

### 日志级别

通过 `[logging].level` 或 `RUST_LOG` 环境变量控制（后者优先）：
- 支持标准级别：`trace`, `debug`, `info`, `warn`, `error`
- 支持 `EnvFilter` 指令：`info,sag_router=debug`

### 每日轮转

非 TTY 环境使用 `tracing_appender::rolling::Rotation::DAILY`：
- 每天自动切换到新文件
- 文件名格式：`sag.2026-06-26.log`
- 目录自动创建（`logs/`）

### 启动时清理

每次启动自动删除 7 天前的旧日志文件：
- 扫描 `logs/sag.*.log`，按 `modified` 时间判断
- 正在写入的当天文件不受影响
- 清理失败不阻塞启动（best-effort）

### HTTP 请求日志

每个 HTTP 请求在 `debug` 级别记录：
- method、uri、auth（masked）、trace_id
- 请求体预览（前 512 字节）和总大小
- 响应状态码和体预览
- SSE 流式响应只记录 status 和 streaming 标记（不消费 body）

完整请求/响应体在 `trace` 级别（target: `sag_router::http_log::full`）记录。

### Trace ID

每个请求生成 UUID v4 作为 `trace_id`，注入 `request.extensions()`。所有中间件和 handler 的日志携带同一 trace_id，实现全链路追踪。

## 配置 Schema

```toml
[logging]
level = "info"
format = "compact"
```

## 功能边界

### 包含
- TTY 检测 + 自适应输出
- 每日轮转日志文件
- 启动时清理旧日志
- HTTP 请求日志
- Trace ID 链路追踪
- EnvFilter 支持

### 不包含
- 远程日志采集（用户可配合外部工具 tail 日志文件）
- 按文件大小轮转（仅按时间）
- 日志压缩
- 审计日志与业务日志分离

## 与其它模块的交互

- **架构**：tracing 在最早时刻初始化
- **部署**：test-env 启动器不再重定向 stdout/stderr

## 变更记录

- [0001 初始化](changes/0001-initial.md)
- [0002 轮转与清理](changes/0002-rotation-and-cleanup.md)
