---
created: 2026-06-03
updated: 2026-10-23
---

# 架构

## 目标

定义 SAG 项目的模块划分、子包职责边界和请求生命周期，确保代码组织清晰、可维护、可扩展。

## 子包职责

| 子包 | 职责 |
|------|------|
| `sag-core` | 共享类型（ChatCompletionRequest/Response）、错误枚举（AppError）、工具函数 |
| `sag-config` | TOML 配置定义与反序列化、热重载（notify 监听文件变化）、配置验证 |
| `sag-providers` | LLM 供应商适配器 trait、OpenAI / Anthropic 实现、格式翻译 |
| `sag-router` | Axum 路由注册、中间件链、Handler |
| `sag-dispatch` | 请求分发逻辑：路由匹配、目标选择、重试退避、熔断器、流式处理 |
| `sag-analytics` | SQLite 调用日志记录、统计聚合查询 |
| `sag-admin` | 管理 API 端点：CRUD 供应商/路由、模型测试、日志查询 |
| `sag-gateway` | 二进制入口：启动编排、配置加载、依赖注入、embed-ui 可选特性 |
| `sag-ui` | Vue 3 管理面板（Vite + shadcn-vue + Pinia） |

## 预期行为

### 启动流程

1. 解析 `--config` 参数，确定配置文件路径
2. Peek 配置文件中的 `[logging]` 段，初始化 tracing（TTY → stdout，非 TTY → 每日轮转文件）
3. 清理 7 天前的旧日志
4. 加载完整配置，启动热重载监听
5. 初始化 SQLite 分析收集器
6. 构建供应商注册表
7. 构建 AppState + DispatchRuntime
8. 构建 Axum 路由器 + 中间件链
9. bind + listen，注册 shutdown signal handler
10. shutdown 时：停止 server → 关闭热重载 → 等待分析任务完成 → WorkGuard drop（flush 日志）

### 请求生命周期

```
Client → Router (auth → trace → http_log → rate_limit) → Dispatch
  → Match Route → Select Target → Call Provider → Translate Response → Client
  ↓ (async)
  Analytics Collector (SQLite)
```

### 响应传输与压缩

响应离开进程有三条独立路径，各自负责自己的编码：

- **SSE（`text/event-stream`）**：响应必须带 `Cache-Control: no-cache, no-transform` 与 `X-Accel-Buffering: no`，避免 nginx / CDN 类中间层缓冲或转码事件流导致 token 一次性倾倒。SSE 永不压缩。
- **内嵌 UI 静态资源**：由 `sag-gateway/src/ui_routes/` 的 SPA fallback serve。UI 构建会为文本产物生成 `.br`（brotli q11）与 `.gz`（gzip 9）副本，handler 按 `Accept-Encoding` 直接下发预压缩件（`br` 优先于 `gzip`）并带 `Vary: Accept-Encoding`；无预压缩副本或客户端不支持时裸发兜底。运行期不压缩 UI。
  - **缓存分两档**：hash 化资产（`assets/**`）为 `public, max-age=31536000, immutable`；`index.html` 等根级文件为 `no-cache`，并带构建期 sha256 强 `ETag`（按**实际下发表示**取值，`If-None-Match` 命中返回空 body `304`）。
  - **缺失资源返 404**：`assets/**` 与末段扩展名属静态扩展名白名单（`js` / `css` / `map` / `svg` / `ico` / `woff2` 等 24 项）的子资源探针返回 `404` 且**不**回 SPA shell；`*.br` / `.gz` 不作为独立资源可访问（`404`）。带点但不是静态文件的客户端路由（如 `/providers/v1.2`）仍回 shell。
  - **方法限制**：静态面只接受 `GET` / `HEAD`，其它方法返回 `405` + `Allow: GET, HEAD`。
  - **API 命名空间**：**未注册**的 `/api/*`、`/v1/*` 返回 `404` JSON（`{"error":{"code":"NOT_FOUND",...}}`，已注册路由不经 fallback）；API 命名空间的未知路径永远先拿 `404`，不走 `405`。
  - **响应头**：所有响应（含 `304`）带 `X-Content-Type-Options: nosniff`。
- **API 响应**：由 `sag-router` 的 `CompressionLayer` 动态压缩，默认开启、无配置开关。predicate 为「响应体 ≥ 1 KiB（含 1024 字节整）且非 SSE / gRPC / 图片」；级别 `Fastest`（延迟敏感优先于压缩比）。

压缩层注册在 `http_log` 之外（更外层），使日志仍记录明文预览与 `content-length`。UI fallback 在 `build_router` 返回后才挂载，不经过中间件栈，因此 UI 的 `Content-Encoding` / `Vary` / `Cache-Control` 由 handler 自己设置。

### 模块隔离

- `sag-core` 不依赖任何其它子包
- `sag-config` 只依赖 `sag-core`
- `sag-providers` 依赖 `sag-core`
- `sag-router` 依赖 `sag-core` + `sag-config`
- `sag-dispatch` 依赖 `sag-core` + `sag-config` + `sag-providers`
- `sag-analytics` 只依赖 `sag-core`
- `sag-admin` 依赖 `sag-core` + `sag-config` + `sag-providers` + `sag-analytics`
- `sag-gateway` 依赖所有子包（组装层）

## 功能边界

### 包含
- Rust 2024 edition，Axum 0.8 HTTP 框架
- 共享状态通过 `Arc<RwLock<T>>` 在线程间传递
- 配置热重载通过 `notify` crate 文件监听实现
- 单个二进制部署（可选 embed-ui 特性嵌入前端 SPA）
- HTTP 响应压缩：内嵌 UI 走构建期预压缩，API 响应走动态压缩层，两者默认开启且无配置开关

### 不包含
- 分布式部署 / 多实例协调
- gRPC / WebSocket 协议（仅 HTTP/1.1）
- 插件系统（通过 Rust trait 实现扩展，非动态加载）

### 版本升级脚本
- `scripts/release.py` 升级版本时仅将实际发生变更且已被 Git 跟踪的文件传给自动提交。
- 被 `.gitignore` 忽略或未被 Git 跟踪的派生文件可以在工作区同步，但不得导致 `git commit` 因无效 pathspec 失败。
- 版本唯一源为根 `Cargo.toml` 的 `[workspace.package] version`；成员 crate 通过 `version.workspace = true` 继承，升级脚本不得改写成员 `Cargo.toml`。
- 一次升级写入并提交的文件固定为：根 `Cargo.toml`、`Cargo.lock`（成员条目由根版本推导）、`sag-ui/package.json`、`docs/package.json`。

## 与其它模块的交互
- 所有子包通过 `sag-core` 的类型定义通信
- `sag-gateway` 是唯一包含 `main()` 的 crate
- 前端 `sag-ui` 通过 HTTP 调用管理 API 和模型 API

## 变更记录

- [0001 初始化](changes/0001-initial.md)
- [0002 版本升级提交按实际变更文件过滤](changes/0002-version-bump-commit-files.md)
- [0003 HTTP 响应压缩：SSE 防护头、UI 构建期预压缩、动态压缩层](changes/0003-http-response-compression.md)
- [0004 内嵌 UI 静态服务：缓存分档、兜底收窄、零拷贝与 ETag](changes/0004-ui-static-cache-and-fallback.md)
