# SAG (Smart AI Gateway) 项目指南

## 关键命令

### 门禁 / 跨前后端（推荐入口）
- `just verify` — 全量验证：`fmt-check` + `lint` + `test`（前后端），等价于 pre-commit hook 两条轨之和
- `just verify-rust` / `just verify-ui` — 只跑单轨验证（hook 按暂存文件类型分别调用）
- `just fmt` — rustfmt + oxfmt 格式化（**会改写文件**）
- `just fmt-check` — 格式化检查（只读，不改写文件）
- `just lint` — `cargo clippy --all-targets -- -D warnings` + oxlint + vue-tsc
- `just test` — Rust 单元/集成测试（默认跳过 doc test）+ 前端测试；`just test --doc` 只跑 doc test

### 后端 (Rust)
- `just build` — 构建 release
- `just test-rust` — 仅 Rust 测试（无参数时默认 `--tests`，跳过 doc test）
- `just test-rust --doc` — 仅文档测试
- `just test-crate sag-config` — 单个 crate 的测试
- `just clippy` — 仅 clippy
- `just run` — 以 debug 模式运行网关
- `just dev` — 通过 `scripts/dev.py` 同时启动前后端

### 前端 (`sag-ui/`)
- `pnpm dev` — 启动 Vite 开发服务器
- `pnpm build` — 生产构建
- `just ui-lint` — oxlint 检查（`just lint` 的子步骤）
- `just ui-typecheck` — vue-tsc 类型检查（`just lint` 的子步骤）
- `just ui-fmt` — oxfmt 格式化（`just fmt` 的子步骤）
- `just ui-test` — 前端测试（`just test` 的子步骤）

## 代码规范

### Rust

- **错误处理**: 使用 `AppError` (thiserror) + `sag_core::Result<T>`，通过 `IntoResponse` 转为 JSON 错误响应
- **测试**: 使用 `#[cfg(test)] mod tests` + `#[tokio::test]`，优先内联测试
- **单元测试 vs 集成测试目录规范**:
  - **单元测试**: 放在 `src/` 文件中，通过 `#[cfg(test)] mod tests` 内联于被测模块底部。只测试当前模块的内部细节，不依赖外部资源（文件系统、网络、环境变量、config handle）。使用 `test_dispatch_runtime()` 或 `test_app_state()` 等内存 fixture
  - **集成测试**: 放在 crate 级 `tests/` 目录下（如 `sag-router/tests/auth_regression.rs`）。测试跨模块的公共 API 或完整请求生命周期，允许配置文件和端到端请求
  - **何时写哪个**: 逻辑正确性验证 → 单元测试（内联）；跨模块协作 / 完整流程 / admin API → 集成测试（`tests/` 目录）
- **验证 codebase**: 使用 `just test`（Rust 默认只跑单元/集成测试、跳过 doc test，并附带前端测试）；只跑 Rust 用 `just test-rust`，需要 doc test 时用 `just test-rust --doc`
- **当前仓库现状**: 无 doc test、无 examples、无 bin 测试
- **共享状态**: `AppState` 使用 `Arc<RwLock<...>>` + `ConfigHandle`
- **Provider 模式**: `#[async_trait] pub trait LlmProvider`，通过 `Arc<dyn LlmProvider>` 存储
- **导入顺序**: 标准库 → 外部 crate → 内部 crate, 空行分组
- **模块导出**: `lib.rs` 统一 `pub use ...` 重新导出公开类型
- **格式化**: 任何对 `.rs` 文件的修改完成后，必须运行 `just fmt`（rustfmt + oxfmt）确保代码符合 rustfmt / oxfmt 风格。pre-commit hook 会以 `just verify-rust` / `just verify-ui` 复查（只读，详见「重要规则 → Git」），未格式化的代码无法通过。

### 前端 (Vue/TypeScript)
- **组件**: `<script setup lang="ts">` Composition API
- **命名**: 组件 `PascalCase`, 文件 `kebab-case`, 变量/函数 `camelCase`
- **状态管理**: Pinia (composition API 风格)
- **样式**: Tailwind CSS utility 类, 不使用 scoped CSS
- **路径别名**: `@/` 映射到 `src/`
- **验证**: 改动 `.ts` / `.vue` 后跑 `just lint`（oxlint + vue-tsc）与 `just test`（含前端测试）；pre-commit hook 会自动执行 `just verify-ui`

## 重要规则

### UI 组件
`sag-ui/src/components/ui/` 目录由 shadcn-vue 自动生成，规则见该目录下的 [`AGENTS.md`](sag-ui/src/components/ui/AGENTS.md)。

### Git
- **完成工作后必须提交**：任何代码改动在收尾时都要 commit，不允许留下未提交的改动（未完成、被阻塞或用户明确要求不提交时除外，需在回复中说明原因）
- **提交门禁由 `scripts/hooks/pre-commit` 自动执行**，不需要在提交前手动跑一遍验证；hook 按暂存文件类型分轨，纯文档改动不会拖起 `cargo test`：

  | 暂存文件 | 执行的 recipe | 具体动作（全部只读） |
  |---|---|---|
  | `*.rs` / `Cargo.toml` / `Cargo.lock` / `justfile` | `just verify-rust` | `cargo fmt --all --check` → `cargo clippy --all-targets -- -D warnings` → `cargo test --no-fail-fast --tests` |
  | `sag-ui/**` / `justfile` | `just verify-ui` | `oxfmt --check` → `oxlint` → `vue-tsc --noEmit` → 前端测试 |

  - 两条轨相加 = `just verify`（前后端全量），本地想提前自查就手动跑它
  - hook **只检查不改写**（一律 `--check` / `--noEmit`），不会静默修改你暂存的内容；格式报错请跑 `just fmt` 修好再提交
  - 任一步失败即中止提交，失败信息里会打印修复命令与绕过方式
  - hook 由 `just setup` 安装（`git config core.hooksPath scripts/hooks`）；GUI git 客户端不加载 shell rc，找不到 `just`/`cargo`/`pnpm` 时会以 `[ERROR] 找不到 just` 报错，去终端提交即可
- 跳过 git hooks：`SKIP_SIMPLE_GIT_HOOKS=1 git commit`（应急用；hook 失败信息里也会提示）
- `cargo clippy` 和 `cargo check` 请通过 `just lint` / `just test` 手动执行
- `gateway.toml` 已 gitignored，不要提交
- 不要修改已存在的 commit，除非用户明确要求

### API 设计
- 遵循 OpenAI `/v1/chat/completions` 接口格式
- 错误响应格式: `{ "error": { "code": "...", "message": "..." } }`
- 避免使用路径参数

### 网关设计原则
- **对网关而言，拒绝未知值是反模式**：解析配置或请求体时应容忍未知**字段**（额外字段），采取「忽略 / 透传」而非「报错拒绝」的策略，以保证前向兼容与多 provider 适配。
- **边界——类型化枚举值除外**：配置中带语义的枚举（`strategy` / `key_strategy` / `provider_type` / `proxy_mode` 等）遵循 loader 的严格校验，未知枚举值直接报错（请求体则 422）。静默回退到默认值可能改变行为（如 `proxy_mode = "enabled"` 意外回退为 `inherit` 导致直连），比报错更危险。新增枚举时应沿用此模式。

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
| 状态目录 | n/a | `.test-env/{pid,lock,build-info.json,stderr.log}` |
| 日志 | `logs/sag.log.YYYY-MM-DD`（每日滚动，tracing-appender 写入；dev.py 将子进程 stdout 以 `[BE]` 标签回显到终端） | `logs/sag.log.YYYY-MM-DD`（每日滚动，grep 友好；stdout 重定向到 DEVNULL，**stderr 捕获到 `.test-env/stderr.log`**） |
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

State（`.test-env/`）、配置（`gateway.test.toml`）、分析库（`analytics-test.db`）、日志（`logs/sag.*.log`）均列入 `.gitignore`，不会污染版本控制。

### Agent 工具接入要点

- **工作面是 `just test-env <subcommand>`**，所有子命令与 `--binary` / `--config` 参数透传给 `python3 scripts/test-env.py`。
- **start 的 fail-fast 路径**：缺 `./target/release/sag` → exit 1 + `[ERR] binary not found ... did you run \`just build-test\`?` ；缺 `gateway.test.toml` → exit 1 + `[ERR] config not found`。Agent 拿到 exit code 后追查即可，无须自己预先验证。
- **`_acquire_lock()`** 走 `O_CREAT | O_EXCL` 原子获取，并发 `start` 互斥；agent 调 `restart` 时无须自己加锁。
- **PID 文件是 canonical pointer**：status 路径先看 `.test-env/pid`，再 best-effort 读 `build-info.json`（缺失时打 `(build-info.json unavailable — partial state)` 而不是硬退）。无状态目录 → exit 1 + `[ERR] no test-env state`，agent 可作为运行探针使用。
- **日志路径**：`logs/sag.log.YYYY-MM-DD` 每日滚动（tracing-appender 管理）；SAG 进程不会自己重开该文件，reader agent 可放心 `tail -f` 或 `grep`。非 tracing 的 stderr（panic、`Error:`）落到 `.test-env/stderr.log`，排查静默失败时优先看它。
- **隐式前置依赖**：改 `sag-ui/**.{ts,vue}` 之后必须重跑 `just build-test`，不然 `./target/release/sag` 仍是上一版 UI；改 `gateway.test.toml` 不需重建二进制（文件级 notify 热重载）。

## 开发环境与测试环境的边界

`just dev` 与 `just test-env` 共用 `.gitignore` 的 categorical 排除（`*.db` / `target/` / `node_modules/`），但两者各自**不**感知对方的状态。开发轨禁用 test-env launcher，测试轨禁用 watchexec —— 否则 watch 触发 cargo 重建会让测试实例的 PID 失效，但 launcher 的状态目录不会自动清理，留下幽灵 pid 文件。