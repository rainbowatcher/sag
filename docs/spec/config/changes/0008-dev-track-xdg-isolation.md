---
created: 2026-10-28
updated: 2026-10-28
type: 修改
---

# 0008 — dev 轨 XDG 数据隔离（`.dev/xdg/`）

## 动机

0006 把配置 / analytics 数据库 / 滚动日志按 XDG 标准落位到用户级目录，并让 `scripts/test-env.py` 通过注入 `XDG_*` 把测试轨关进 `.test-env/xdg/`。但**开发轨（`just dev` / `just run`）当时没有注入**，于是日常开发直接写生产路径：

- analytics 数据库落到 `~/.local/share/sag/analytics.db`（本地调试数据与真实调用记录混在同一张 `call_logs`）；
- 滚动日志落到 `~/.local/state/sag/logs/`，而 gateway 启动时按「7 天前」清理旧日志——本地反复重启会真的删掉生产排障用的历史日志；
- 配置 fallback 命中 `~/.config/sag/gateway.toml`，调试验证过的配置可能被当成生产配置读取。

本次给开发轨补上同一套注入，把 dev 的全部可变状态钉在仓库内 `.dev/xdg/`，与生产 XDG 目录、`.test-env/` 三者互不干扰。**零 Rust 改动**：隔离完全由 launcher 的环境注入实现，二进制保持纯 XDG 逻辑（与 0006 对 test-env 的做法同构）。

## 行为变化

- 开发轨数据落点整体平移（三个 `XDG_*` 都指向项目内绝对路径）：

  | 数据 | 之前（生产 XDG） | 现在（dev 轨） |
  |---|---|---|
  | analytics 数据库 | `~/.local/share/sag/analytics.db` | `.dev/xdg/data/sag/analytics.db` |
  | 滚动日志（仅非 TTY） | `~/.local/state/sag/logs/` | `.dev/xdg/state/sag/logs/` |
  | 配置 fallback | `~/.config/sag/gateway.toml` | `.dev/xdg/config/sag/gateway.toml` |

  「仅非 TTY」：`tracing_init` 只在 stdout 非 TTY 时挂 `RollingFileAppender` —— `just dev`（stdout=PIPE）写文件，TTY 交互（直接 `just run`）只输出终端、不产生文件。

- **配置查找优先级不变**：`--config` / 位置参数 > 项目根 `./gateway.toml`（存在时）> `$XDG_CONFIG_HOME/sag/gateway.toml`。只是 dev 轨下第三项如今指向 `.dev/xdg/config/sag/gateway.toml`。
- **迁移提示（唯一有意 break）**：此前依赖 `~/.config/sag/gateway.toml` fallback 的开发者，开发轨不再读它——把该文件复制为项目根 `gateway.toml`，或放到 `.dev/xdg/config/sag/gateway.toml`。
- **逃生口**：绕过 launcher 直接 `cargo run`（此时无注入，可再自行导出 `XDG_*` 指到任意位置）；`just dev` / `just run` / `just run-release` 会无条件覆盖这三个变量，预先导出无效。
- **`just run` / `just run-release` 也走同一注入**，不再只有 `just dev`：
  - `run` / `run-release` 改为 `python3 scripts/dev_env.py -- cargo run [--release] -- <config>`；
  - `scripts/dev.py` 拉起 `watchexec → cargo run` 时复用 `dev_env()` 注入同一组值。
- 新增 `just dev-clean` 显式删除 `.dev/`；`just clean` 仍然只清构建产物（`cargo clean` + `sag-ui/dist` + `node_modules`），不触碰 `.dev/`（与「clean 不动用户数据」的既有约定一致）。
- 文档同步：`AGENTS.md` 的两轨对比表 / XDG 小节 / 边界小节、`docs/guide/{configuration,deployment,analytics}.md` 中描述 dev 轨落点之处。

## 需求变更

- `docs/spec/config/requirements.md`「加载」第 1 条追加 dev 轨注入说明：第三级 fallback（`$XDG_CONFIG_HOME/sag/gateway.toml`）在 dev 轨下为 `.dev/xdg/config/sag/gateway.toml`，前两级（`--config` / 位置参数 > 项目根 `./gateway.toml`）优先级不变。
- `docs/spec/config/requirements.md` 末尾「变更记录」追加 0008 条目。
- `docs/spec/logging/requirements.md`：输出目标表按 TTY 直跑 / 非 TTY dev 轨 / 非 TTY 生产与 test-env 三行重写，轮转文件名订正为 `sag.log.YYYY-MM-DD`。
- `AGENTS.md`：两轨对比表、双注入表、逃生口措辞与日志 TTY 条件同步。
- `docs/guide/configuration.md` / `deployment.md`：日志落点补非 TTY 限定、日志文件名订正。

## 不变量

- **不动 Rust**：`sag-core/src/paths.rs` 的 XDG 解析、gateway 的 DSN 锚定与日志目录逻辑一行未改。
- **注入值必须是绝对路径**：`paths.rs` 把空值 / 相对路径的 `XDG_*` 视为未设置并静默回退 `$HOME` 默认值，隔离会无声失效；`dev_env.py` 用 `PROJECT_ROOT` 拼绝对路径，并在启动前 `mkdir -p` 三个叶子目录（`config/sag`、`data/sag`、`state/sag/logs`；SQLite 不会自建父目录）。
- **`--config` 优先级不变**：显式参数永远最高，注入只影响 fallback。
- **`.dev/` 与 `.test-env/` 互不感知**：两个根并列、各自 gitignore，各自的清理命令（`just dev-clean` / `just test-env stop` 后删目录）不越界。
- **二进制无「开发/测试模式」分支**：隔离只是环境变量，行为差异全部来自 XDG 解析。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`scripts/dev_env.py`](../../../../scripts/dev_env.py) | 新增 dev 轨 XDG 注入模块：`dev_env()` 纯函数覆盖三个 `XDG_*`（指向 `PROJECT_ROOT/.dev/xdg/`）、`ensure_dev_dirs()` 预建叶子目录、CLI `python3 scripts/dev_env.py -- <cmd...>` 建目录后 `execvpe` 目标命令 |
| [`scripts/dev.py`](../../../../scripts/dev.py) | 后端子进程改用 `dev_env()` 作为环境，`watchexec → cargo run` 继承 `.dev/xdg/`；模块 docstring 与 `--config` 说明同步 |
| [`scripts/tests/test_dev_env.py`](../../../../scripts/tests/test_dev_env.py) | 新增单元测试：三变量覆盖为绝对路径、不修改入参 / `os.environ`、叶子目录创建幂等 |
| [`justfile`](../../../../justfile) | `run` / `run-release` 改经 `scripts/dev_env.py` 启动；新增 `dev-clean`（`rm -rf .dev`）；`clean` / `setup` / `dev` 注释同步 |
| [`.gitignore`](../../../../.gitignore) | 新增 `.dev/`（dev 轨隔离根） |
| [`AGENTS.md`](../../../../AGENTS.md) | 两轨对比表补 dev 轨 SQLite / 日志 / 配置 / 清理行与「双注入」小节；修正 dev 轨日志与 SQLite 的过时路径；边界小节说明 `.dev/` 与 `.test-env/` 互不感知 |
| [`docs/guide/configuration.md`](../../../guide/configuration.md) | 配置查找顺序与 analytics / 日志段落补 dev 轨 `.dev/xdg/` 落点；`[analytics]` 示例注释同步 |
| [`docs/guide/deployment.md`](../../../guide/deployment.md) | 测试环境与日志轮转小节说明开发轨同样注入 `.dev/xdg/` |
| [`docs/guide/analytics.md`](../../../guide/analytics.md) | `VACUUM` 操作说明补开发轨库路径 `.dev/xdg/data/sag/analytics.db` |

## 测试

- `scripts/tests/test_dev_env.py` —— `dev_env()` 覆盖三个 `XDG_*` 且均为绝对路径、只拷贝不修改入参、`ensure_dev_dirs()` 可重复调用；`just test-scripts` 执行
- 冒烟：`just dev`（或 `just run 2>&1 | cat` 等非 TTY 方式）启动后 analytics 库落在 `.dev/xdg/data/sag/analytics.db`、日志落在 `.dev/xdg/state/sag/logs/`；TTY 交互下（直接 `just run`）日志只输出终端、不产生文件。两种情况下 `~/.local/share/sag` / `~/.local/state/sag` 的 mtime 均不变；`just dev-clean` 后 `.dev/` 消失而 `just clean` 不影响 `.dev/`
- `just --list` 校验 justfile 语法与 `dev-clean` 注册
