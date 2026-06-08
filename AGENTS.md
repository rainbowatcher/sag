# SAG (Smart AI Gateway) 项目指南

## 项目概述

SAG 是一个 LLM API 反向代理网关，接收 OpenAI 兼容的 `/v1/chat/completions` 请求，按优先级自动故障转移到多个上游 LLM 提供商，并附带用量统计和管理面板。

## 技术栈

| 层面 | 技术 |
|---|---|
| **后端** | Rust 2024 edition, Axum 0.8, Tokio, Serde, SQLx (SQLite), Reqwest |
| **前端** | Vue 3.5 (Composition API + `<script setup>`), TypeScript 5.7, Vite 6.4, Pinia, Tailwind CSS v4, shadcn-vue |
| **工具** | Cargo workspace, just, pnpm, prek (pre-commit), oxlint/oxfmt, mdBook |

## 关键命令

### 后端 (Rust)
- `just build` — 构建 release
- `just test` — 运行所有测试 (`cargo test --no-fail-fast`)
- `just lint` — clippy + 前端 lint
- `just fmt` — rustfmt 格式化
- `just run` — 以 debug 模式运行网关
- `just dev` — 通过 `scripts/dev.py` 同时启动前后端

### 前端 (`sag-ui/`)
- `pnpm dev` — 启动 Vite 开发服务器
- `pnpm build` — 生产构建
- `pnpm lint` — oxlint 检查
- `pnpm format` — oxfmt 格式化
- `pnpm typecheck` — vue-tsc 类型检查

## 目录结构

```
sag/
├── sag-core/           # 共享类型、错误枚举、工具函数
├── sag-config/         # TOML 配置定义 + 热重载
├── sag-providers/      # LLM 提供商适配器 (OpenAI, Anthropic)
├── sag-router/         # Axum 路由、中间件、Handler
├── sag-analytics/      # SQLite 用量统计
├── sag-gateway/        # 二进制入口 main.rs
├── sag-ui/             # Vue 3 管理面板
├── docs/               # mdBook 文档 (中文)
├── scripts/            # 开发工具脚本
└── gateway.toml        # 网关配置 (gitignored)
```

## 代码规范

### Rust

- **错误处理**: 使用 `AppError` (thiserror) + `sag_core::Result<T>`，通过 `IntoResponse` 转为 JSON 错误响应
- **测试**: 使用 `#[cfg(test)] mod tests` + `#[tokio::test]`，优先内联测试
- **共享状态**: `AppState` 使用 `Arc<RwLock<...>>` + `ConfigHandle`
- **Provider 模式**: `#[async_trait] pub trait LlmProvider`，通过 `Arc<dyn LlmProvider>` 存储
- **导入顺序**: 标准库 → 外部 crate → 内部 crate, 空行分组
- **模块导出**: `lib.rs` 统一 `pub use ...` 重新导出公开类型

### 前端 (Vue/TypeScript)
- **组件**: `<script setup lang="ts">` Composition API
- **命名**: 组件 `PascalCase`, 文件 `kebab-case`, 变量/函数 `camelCase`
- **状态管理**: Pinia (composition API 风格)
- **样式**: Tailwind CSS utility 类, 不使用 scoped CSS
- **路径别名**: `@/` 映射到 `src/`

## 重要规则

### UI 组件
`sag-ui/src/components/ui/` 目录由 shadcn-vue 自动生成，规则见该目录下的 [`AGENTS.md`](sag-ui/src/components/ui/AGENTS.md)。

### Git
- 提交前 `prek` 会自动运行 `lint-staged`，对 `sag-ui/` 下的暂存文件执行 `oxfmt` + `oxlint`
- `gateway.toml` 已 gitignored，不要提交
- 不要修改已存在的 commit，除非用户明确要求

### API 设计
- 遵循 OpenAI `/v1/chat/completions` 接口格式
- 错误响应格式: `{ "error": { "code": "...", "message": "..." } }`

## 开发流程

1. 确保安装了 `mise.toml` 中列出的工具 (just, node, pnpm, prek 等)
2. `just init-config` 初始化本地配置
3. `just dev` 或 `python3 scripts/dev.py` 启动开发环境
4. 提交前运行 `just test` + `just lint` 确保代码质量
