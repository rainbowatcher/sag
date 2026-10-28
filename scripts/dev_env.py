#!/usr/bin/env python3
"""dev 轨 XDG 隔离根 — 把 `just dev` 的可变状态钉在项目内 `.dev/xdg/`.

`scripts/dev.py` 用 `dev_env()` 给后端子进程（watchexec → cargo run → sag）
注入 `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_STATE_HOME`，于是配置 fallback、
analytics DB 与滚动日志全都落在仓库内，不会碰到：

  - `~/.config/sag/gateway.toml`（生产配置）
  - `~/.local/share/sag/analytics.db`（生产 analytics 数据）
  - `~/.local/state/sag/logs/`（生产日志，滚动日志的 7 天清理会误删它们）

机制与 `scripts/test-env.py` 的 `child_env()` 相同，只是根目录换成 `.dev/`，
与 `.test-env/` 并列且同样被 `.gitignore` 排除。

注入值必须是**绝对路径**：`sag-core/src/paths.rs` 对空值 / 相对路径的
`XDG_*` 会静默回退到 `$HOME` 默认值，隔离会无声失效。

也可当启动器用（先建好目录，再 exec 目标命令）:

    python3 scripts/dev_env.py -- <cmd...>
"""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parent.parent
# XDG_* override root injected into the dev backend process: keeps config /
# data / state physically inside the project.
DEV_XDG_ROOT = PROJECT_ROOT / ".dev" / "xdg"

# 需要预先存在的叶子目录 —— SQLite 不会替我们创建父目录。
_LEAF_DIRS = ("config/sag", "data/sag", "state/sag/logs")


def dev_env(base: dict | None = None) -> dict:
    """返回注入了 dev 轨 XDG_* 覆盖的环境字典.

    纯函数：只拷贝 `base`（缺省 `os.environ`）并覆盖三个 XDG 键，不做任何 I/O，
    也不修改调用方的字典 / `os.environ`。
    """
    env = dict(os.environ if base is None else base)
    env["XDG_CONFIG_HOME"] = str(DEV_XDG_ROOT / "config")
    env["XDG_DATA_HOME"] = str(DEV_XDG_ROOT / "data")
    env["XDG_STATE_HOME"] = str(DEV_XDG_ROOT / "state")
    return env


def ensure_dev_dirs() -> None:
    """创建隔离树下的叶子目录（analytics DB 与日志目录不会自建父目录）."""
    for leaf in _LEAF_DIRS:
        (DEV_XDG_ROOT / leaf).mkdir(parents=True, exist_ok=True)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="在 dev 轨 XDG 隔离环境下执行命令（`--` 之后为命令）",
        # `--` 是必需的：REMAINDER 不豁免前导 option，省略 `--` 时
        # `-x foo` 会被 argparse 当成自己的参数而报 unrecognized arguments。
        epilog="用法: python3 scripts/dev_env.py -- <命令...>（`--` 不能省略）",
    )
    parser.add_argument(
        "command",
        nargs=argparse.REMAINDER,
        help="要执行的命令，例如: -- cargo run",
    )
    args = parser.parse_args(argv)

    cmd = args.command
    # `python3 dev_env.py -- cmd...` → argparse 把分隔符留在 REMAINDER 里。
    if cmd and cmd[0] == "--":
        cmd = cmd[1:]
    if not cmd:
        parser.error("缺少命令：用法 `python3 scripts/dev_env.py -- <cmd...>`")

    ensure_dev_dirs()
    # execvpe 按 PATH 解析 cmd[0]，并替换掉当前进程。
    # 命令不存在时 execvpe 抛 FileNotFoundError —— 用 parser.error 转成
    # 与其它 CLI 用法错误一致的「usage + 明确原因 + exit 2」，而不是裸 traceback。
    try:
        os.execvpe(cmd[0], cmd, dev_env())
    except FileNotFoundError:
        parser.error(f"命令不存在: {cmd[0]}")
    return 0  # 不可达（execvpe 成功即替换进程；失败已由 parser.error 退出）


if __name__ == "__main__":
    sys.exit(main())
