---
created: 2026-06-03
updated: 2026-06-03
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

### 不包含
- 分布式部署 / 多实例协调
- gRPC / WebSocket 协议（仅 HTTP/1.1）
- 插件系统（通过 Rust trait 实现扩展，非动态加载）

### 版本升级脚本
- `scripts/bump-version.py` 升级版本时仅将实际发生变更且已被 Git 跟踪的文件传给自动提交。
- 被 `.gitignore` 忽略或未被 Git 跟踪的派生文件可以在工作区同步，但不得导致 `git commit` 因无效 pathspec 失败。

## 与其它模块的交互
- 所有子包通过 `sag-core` 的类型定义通信
- `sag-gateway` 是唯一包含 `main()` 的 crate
- 前端 `sag-ui` 通过 HTTP 调用管理 API 和模型 API

## 变更记录

- [0001 初始化](changes/0001-initial.md)
- [0002 版本升级提交按实际变更文件过滤](changes/0002-version-bump-commit-files.md)
