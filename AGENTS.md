# SAG (Smart AI Gateway) 项目指南

## 关键命令

### 后端 (Rust)
- `just build` — 构建 release
- `just test` — 运行 Rust 测试（可追加参数，如 `just test --tests` 仅运行单元/集成测试）
- `just test --tests` — 仅运行单元测试和集成测试（跳过 doc test），验证 codebase 时优先使用
- `just test --doc` — 仅运行文档测试
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

## 代码规范

### Rust

- **错误处理**: 使用 `AppError` (thiserror) + `sag_core::Result<T>`，通过 `IntoResponse` 转为 JSON 错误响应
- **测试**: 使用 `#[cfg(test)] mod tests` + `#[tokio::test]`，优先内联测试
- **验证 codebase**: 使用 `just test --tests`（仅运行单元/集成测试，不包含 doc test）；需要 doc test 时请用 `just test --doc`
- **当前仓库现状**: 无 doc test、无 examples、无 bin 测试
- **共享状态**: `AppState` 使用 `Arc<RwLock<...>>` + `ConfigHandle`
- **Provider 模式**: `#[async_trait] pub trait LlmProvider`，通过 `Arc<dyn LlmProvider>` 存储
- **导入顺序**: 标准库 → 外部 crate → 内部 crate, 空行分组
- **模块导出**: `lib.rs` 统一 `pub use ...` 重新导出公开类型
- **格式化**: 任何对 `.rs` 文件的修改完成后，必须运行 `cargo fmt`（或 `just fmt`）确保代码符合 rustfmt 风格。提交前 `pre-commit` hook 会检查 `cargo fmt --check`，未格式化的代码无法通过。

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
- 提交前 `pre-commit` hook 会根据暂存文件类型自动执行对应检查：
  - `*.rs` → `cargo fmt --check`
  - `sag-ui/**/*.{ts,vue}` → `lint-staged` → `oxfmt` + `oxlint`
- 跳过 git hooks：`SKIP_SIMPLE_GIT_HOOKS=1 git commit`
- `cargo clippy` 和 `cargo check` 请通过 `just lint` / `just test` 手动执行
- `gateway.toml` 已 gitignored，不要提交
- 不要修改已存在的 commit，除非用户明确要求

### API 设计
- 遵循 OpenAI `/v1/chat/completions` 接口格式
- 错误响应格式: `{ "error": { "code": "...", "message": "..." } }`
- 避免使用路径参数

### 文档优先原则
- 遇到框架/库相关的使用问题、API 语法、配置选项或版本迁移时，**优先通过 `find-docs` skill 查阅最新官方文档**，不要依赖训练数据中的知识。
- **排查问题时尤其要遵循此原则**——先查官方文档确认 API 签名、配置项、版本差异，再下结论，避免凭记忆或过时经验猜测。

## 测试环境

`scripts/test-env.py` 提供与开发环境物理隔离的 release 路径，agent 工具与运维脚本均通过 `just test-env <子命令>` 操作。

### 两轨对比

| 维度 | 开发轨 (`just dev`) | 测试轨 (`just test-env`) |
|---|---|---|
| Rust 二进制 | `cargo run` 的 debug 实例 | `./target/release/sag`（事先 `just build-test`） |
| 前端 | Vite dev server 独立 HMR | emit 后的产物通过 `rust-embed` 嵌入二进制（`--features embed-ui`） |
| 默认端口 | 37210（读 `gateway.toml`） | 37211（读 `gateway.test.toml`） |
| 状态目录 | n/a | `.test-env/{pid,lock,build-info.json}` |
| 日志 | stdout（watchexec 接管） | `logs/test-env.log`（append，grep 友好） |
| 配置 | `gateway.toml` | `gateway.test.toml`（从 `gateway.test.example.toml` 复制） |
| SQLite | `analytics.db` | `analytics-test.db` |

### 启动顺序

```bash
just build-test                # 一次性：pnpm build UI → cargo --features embed-ui
just test-env start            # 后台拉起，写入 .test-env/{pid,lock,build-info.json}
just test-env status           # 查看 pid / 启动时间 / uptime / binary mtime / config mtime / log tail
just test-env restart          # 原子 stop+start（中途失败回滚状态文件）
just test-env stop             # SIGTERM → 5s grace → SIGKILL
```

### 默认子命令：status（不是 start）

不带任何子命令直接调用 `just test-env` 等价于 `just test-env status`。这是 `scripts/test-env.py` 中 argparse `args.cmd` 为 `None` 时 fallback 到 `cmd_status()` 的有意设计 —— status 是 read-only、副作用为零，作默认安全；start 会 fork + 写 pid + 占 37211 端口，作为默认会让 `<tab>` 补全误起实例。

```bash
just test-env          # → status（不是 start）
just test-env status   # 显式声明，与上面等价
just test-env start    # 真正启动 gateway；同上必须显式写
```

误输 `just test-env` 看到 `[ERR] no test-env state (`.test-env/pid` missing)` 不是错，是状态为空 —— 跑 `just test-env start` 即可拉起。启动路径上的 4 类常见 bail 见下一小节。

State（`.test-env/`）、配置（`gateway.test.toml`）、分析库（`analytics-test.db`）、日志（`logs/test-env.log`）均列入 `.gitignore`，不会污染版本控制。

### Agent 工具接入要点

- **工作面是 `just test-env <subcommand>`**，所有子命令与 `--binary` / `--config` 参数透传给 `python3 scripts/test-env.py`。
- **start 的 fail-fast 路径**：缺 `./target/release/sag` → exit 1 + `[ERR] binary not found ... did you run \`just build-test\`?` ；缺 `gateway.test.toml` → exit 1 + `[ERR] config not found`。Agent 拿到 exit code 后追查即可，无须自己预先验证。
- **`_acquire_lock()`** 走 `O_CREAT | O_EXCL` 原子获取，并发 `start` 互斥；agent 调 `restart` 时无须自己加锁。
- **PID 文件是 canonical pointer**：status 路径先看 `.test-env/pid`，再 best-effort 读 `build-info.json`（缺失时打 `(build-info.json unavailable — partial state)` 而不是硬退）。无状态目录 → exit 1 + `[ERR] no test-env state`，agent 可作为运行探针使用。
- **日志路径不变**：`logs/test-env.log` append 模式；SAG 进程不会自己重开该文件，reader agent 可放心 `tail -f` 或 `grep`。
- **隐式前置依赖**：改 `sag-ui/**.{ts,vue}` 之后必须重跑 `just build-test`，不然 `./target/release/sag` 仍是上一版 UI；改 `gateway.test.toml` 不需重建二进制（文件级 notify 热重载）。

## 开发环境与测试环境的边界

`just dev` 与 `just test-env` 共用 `.gitignore` 的 categorical 排除（`*.db` / `target/` / `node_modules/`），但两者各自**不**感知对方的状态。开发轨禁用 test-env launcher，测试轨禁用 watchexec —— 否则 watch 触发 cargo 重建会让测试实例的 PID 失效，但 launcher 的状态目录不会自动清理，留下幽灵 pid 文件。