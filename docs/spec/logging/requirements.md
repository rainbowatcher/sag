---
created: 2026-06-03
updated: 2026-10-28
---

# 日志

## 目标

提供结构化日志输出，支持两种格式、环境自适应输出目标和自动轮转清理。

## 预期行为

### 输出目标

| 环境 | stdout | stderr | 日志文件 |
|------|--------|--------|---------|
| TTY 直跑（如直接 `just run`） | ✅ 输出 | ✅ 输出 | ❌ 不写文件 |
| 非 TTY dev 轨（`just dev` / 重定向） | ❌ 不写日志（管道由 `dev.py` 以 `[BE]` 回显） | 同左（stderr 合并进该管道） | ✅ `.dev/xdg/state/sag/logs/sag.log.YYYY-MM-DD` 每日轮转 |
| 非 TTY 生产与 test-env | ❌ DEVNULL | test-env 捕获到 `.test-env/stderr.log` | ✅ 各自 XDG 路径下 `logs/sag.log.YYYY-MM-DD` 每日轮转 |

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
- 文件名格式：`sag.log.YYYY-MM-DD`
- 目录自动创建（`$XDG_STATE_HOME/sag/logs`，XDG Base Directory 标准）

### 启动时清理

每次启动自动删除 7 天前的旧日志文件：
- 扫描 `$XDG_STATE_HOME/sag/logs/sag.*.log`，按 `modified` 时间判断
- 正在写入的当天文件不受影响
- 清理失败不阻塞启动（best-effort）

### HTTP 请求日志

每个 HTTP 请求在 `debug` 级别记录：
- method、uri、auth（masked）、trace_id
- 请求体预览（前 512 字节）和总大小
- 响应状态码和体预览
- SSE 流式响应只记录 status 和 streaming 标记（不消费 body）

请求体大于 512 KiB 时不缓冲、不产生 body 预览（保证 `DefaultBodyLimit` / JSON extractor 仍能返回 413 + `client_error` 事实），但**响应仍必须记录 status 与耗时 `elapsed_ms`**，content-length 可用时附带 `body_size`。

流式响应 commit 之后的客户端断开必须记录一条 WARN，携带 request_id / provider / model / real_model / duration_ms（此前只有 pre-commit 取消有日志）。

完整请求/响应体在 `trace` 级别（target: `sag_router::http_log::full`）记录。

日志中间件**不得改变或截断响应体**：body 收集必须有界，超过 10 MB（`MAX_BODY_READ`）时仍须把完整 body 转发给客户端且不改写 headers（`content-length` 保持与 body 一致），日志只记录 512 B 预览并打截断标记（`truncated=true` + `collected=<n>`）。

body 流中途错误**不得被吞成空 body**：必须记录一条 WARN（`body_error=true`，message `"response body error"` / `"request body error"`）并把错误传播给客户端，而不是返回一个与 `content-length` 不符的空 body 或让 handler 收到空请求体。

### Trace ID

每个请求生成 UUID v4 作为 `trace_id`，注入 `request.extensions()`。所有中间件和 handler 的日志携带同一 trace_id，实现全链路追踪。

脱离请求生命周期的后台任务（provider 的流读取任务，如 `chat_stream` 内的 `tokio::spawn`）必须继承调用方 span：其日志（上游 chunk 读取失败、chunk idle timeout）携带 provider / model / trace_id，否则无法与请求关联。

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

- [0006 日志目录迁移至 $XDG_STATE_HOME/sag/logs](../config/changes/0006-xdg-base-directories.md)
- [0001 初始化](changes/0001-initial.md)
- [0002 轮转与清理](changes/0002-rotation-and-cleanup.md)
- [0003 大请求响应日志与请求关联信息](changes/0003-request-correlation-and-large-body-response-log.md)
- [0004 响应体超限不再毁坏响应](changes/0004-response-body-truncation-fix.md)
