---
created: 2026-10-13
updated: 2026-10-13
type: 修改
---

# 0006 — XDG Base Directory 标准路径布局

## 动机

配置文件、analytics 数据库、滚动日志此前全部相对进程 CWD 解析（项目根 `gateway.toml` / `analytics.db` / `logs/`），导致：

- 数据文件散落在仓库目录内，与源码混淆；
- 部署时必须依赖启动器把 CWD 摆对，且无法声明「用户数据在哪」的统一约定。

本次按 freedesktop.org XDG Base Directory Specification 重排三类持久化数据，并顺带修复被锚定暴露的 SQLite 建库缺陷。

## 需求变更

- `docs/spec/config/requirements.md`「加载」第 1 条：配置路径解析顺序改为 `--config` / 位置参数 > `./gateway.toml`（存在时，向后兼容）> `$XDG_CONFIG_HOME/sag/gateway.toml`
- `docs/spec/analytics/requirements.md`「配置 Schema」节：补充相对 `sqlite://` DSN 在启动时锚定到 `$XDG_DATA_HOME/sag` 的语义
- `docs/spec/logging/requirements.md`：日志目录从 `logs/` 改为 `$XDG_STATE_HOME/sag/logs`
- `AGENTS.md`：测试环境两轨对比表、Agent 工具接入要点同步新路径；新增「XDG Base Directory」小节
- `docs/guide/configuration.md` / `deployment.md` / `architecture.md`：用户文档同步

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-core/src/paths.rs`](../../../sag-core/src/paths.rs) | 新增 XDG 路径解析模块：`xdg_config_home` / `xdg_data_home` / `xdg_state_home`（env 值为空或相对路径时回退 `$HOME` 规范默认值）、`sag_*_dir` 应用子目录、`anchor_sqlite_dsn` 相对 DSN 锚定、`sqlite_dsn_file_path` 提取落盘路径 |
| [`sag-core/src/lib.rs`](../../../sag-core/src/lib.rs) | 注册 `paths` 模块并 re-export |
| [`Cargo.toml`](Cargo.toml) | workspace 新增 `home = "0.5"` 依赖（cargo/rustup 维护的 `$HOME` 读取） |
| [`sag-gateway/src/main.rs`](../../../sag-gateway/src/main.rs) | `resolve_config_path` 增加 CWD 回退与 XDG 默认；CLI 解析拆出纯函数 `parse_config_args`；新增 `resolve_analytics_dsn_with_parent`（锚定 DSN + 幂等创建父目录）；`cleanup_old_logs` 改扫 XDG 日志目录 |
| [`sag-gateway/src/tracing_init.rs`](../../../sag-gateway/src/tracing_init.rs) | `init()` 接收日志目录参数，轮转文件写入 `$XDG_STATE_HOME/sag/logs` |
| [`sag-analytics/src/lib.rs`](../../../sag-analytics/src/lib.rs) | `analytics_connect_options`：`sqlite:` URI 走 `from_str` 后补 `create_if_missing(true)`（除非 DSN 显式 `mode=`），修复「文件不存在即 `SQLITE_CANTOPEN (14)`」——旧 CWD 相对路径靠已存在文件掩盖了该缺陷 |
| [`scripts/test-env.py`](scripts/test-env.py) | 启动子进程时注入项目内 `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_STATE_HOME` 覆盖（`.test-env/xdg/`），DB 与日志物理隔离在项目内；日志 glob 与 docstring 同步 |
| [`scripts/dev.py`](scripts/dev.py) | `--config` 改为可选；不传时不带位置参数，由二进制按查找顺序自行解析 |
| [`justfile`](justfile) | `clean` 移除 `rm -f analytics.db`（DB 不再落项目内，clean 不触碰用户数据）；`setup` 注释补充 XDG 位置说明 |
| [`gateway.example.toml`](gateway.example.toml) | `[analytics]` 段注释说明相对 DSN 锚定语义 |
| [`gateway.test.example.toml`](gateway.test.example.toml) | 同上，并说明 test-env 的 XDG 覆盖隔离 |

## 设计决策

- **三变量统一规范回退**：`$XDG_*_HOME` 未设置、为空或为相对路径时一律视为未设置，回退 `$HOME/.config`、`$HOME/.local/share`、`$HOME/.local/state`；且三个默认值在所有平台一致（不做 macOS `Library/` 适配）——SAG 是面向开发者的本机服务，跨平台布局一致优先。
- **锚定发生在 gateway 边界**：`sag-config` 的默认 DSN 保持相对形态 `sqlite://analytics.db`，配置文件里写什么就存什么（save 回写不变）；运行时由 gateway 统一锚定并建父目录。analytics 层对 DSN 的解析语义不变，测试不受影响。
- **CWD 回退保留**：项目根已存在 `gateway.toml` 时优先命中（dev 流程零迁移），否则落 `$XDG_CONFIG_HOME/sag/gateway.toml`。
- **test-env 用 env 覆盖而非代码特判**：启动器向子进程注入 `.test-env/xdg/` 三变量，二进制保持纯 XDG 逻辑，无任何「测试模式」分支；整个隔离树随 `.test-env/` gitignore。

## 测试

- `sag-core/src/paths.rs` 内联测试：相对 DSN 锚定（含子目录、query 保留）、绝对 / `:memory:` / `file:` 透传、`sqlite_dsn_file_path` 提取与拒绝、`xdg_dir_or` env 策略（绝对命中 / 未设置 / 空 / 相对路径回退）
- `sag-gateway/src/main.rs` 内联测试：`parse_config_args` 显式优先、`--config=` 形式、未知 flag 丢弃、缺参报错
- `sag-analytics/src/lib.rs` 新增：`sqlite_dsn_creates_missing_database_file`（不存在的 `sqlite://` 文件可建库）、`sqlite_dsn_mode_rw_keeps_create_disabled`（`mode=rw` 保持不建）
- 端到端冒烟：`just build-test` + `just test-env start` 验证 DB 落 `.test-env/xdg/data/sag/`、日志落 `.test-env/xdg/state/sag/logs/`、用户级 `~/.local/share/sag` 与 `~/.local/state/sag` 不被触碰
