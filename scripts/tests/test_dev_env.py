#!/usr/bin/env python3
"""`scripts/dev_env.py` 单测：XDG 隔离的纯函数层 + CLI 冒烟。

覆盖范围（不启动 watchexec / cargo，不碰用户的 XDG 目录）:
  dev_env / ensure_dev_dirs / main 的 `--` 命令透传

两种跑法都要通:
  python3 scripts/tests/test_dev_env.py -v
  python3 -m unittest discover -s scripts/tests -t scripts -p 'test_dev_env.py' -v
"""

from __future__ import annotations

import contextlib
import io
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

# 直跑（sys.path[0] = scripts/tests）与 discover（顶层 = scripts）都要能
# `import dev_env`，统一把 scripts/ 目录塞进 sys.path。
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

import dev_env  # noqa: E402

XDG_KEYS = ("XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME")


class DevEnvDictTest(unittest.TestCase):
    def test_defaults_point_into_project_dev_tree(self) -> None:
        env = dev_env.dev_env({})
        self.assertEqual(env["XDG_CONFIG_HOME"], str(dev_env.DEV_XDG_ROOT / "config"))
        self.assertEqual(env["XDG_DATA_HOME"], str(dev_env.DEV_XDG_ROOT / "data"))
        self.assertEqual(env["XDG_STATE_HOME"], str(dev_env.DEV_XDG_ROOT / "state"))
        # 相对路径会被 sag-core/src/paths.rs 静默回退到 $HOME，必须绝对。
        for key in XDG_KEYS:
            self.assertTrue(Path(env[key]).is_absolute(), key)

    def test_overrides_xdg_but_keeps_unrelated_keys(self) -> None:
        env = dev_env.dev_env({"XDG_DATA_HOME": "/prod", "FOO": "bar"})
        self.assertEqual(env["FOO"], "bar")
        self.assertNotEqual(env["XDG_DATA_HOME"], "/prod")
        self.assertEqual(env["XDG_DATA_HOME"], str(dev_env.DEV_XDG_ROOT / "data"))

    def test_does_not_mutate_base_or_process_environ(self) -> None:
        base = {"XDG_DATA_HOME": "/prod"}
        before_env = dict(os.environ)
        before_base = dict(base)
        dev_env.dev_env(base)
        self.assertEqual(base, before_base)
        self.assertEqual(dict(os.environ), before_env)

    def test_dev_env_has_no_filesystem_side_effects(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "xdg"
            with mock.patch.object(dev_env, "DEV_XDG_ROOT", root):
                dev_env.dev_env({})
                self.assertFalse(root.exists())

    def test_default_base_does_not_mutate_process_environ(self) -> None:
        """`base=None` 走 `dict(os.environ)` 拷贝路径，不能回写 `os.environ`."""
        before = dict(os.environ)
        env = dev_env.dev_env()
        self.assertEqual(dict(os.environ), before)
        # 返回值是独立副本：改它不会泄漏回进程环境。
        self.assertIsNot(env, os.environ)
        env["XDG_DATA_HOME"] = "/tampered"
        self.assertEqual(dict(os.environ), before)


class EnsureDevDirsTest(unittest.TestCase):
    def test_creates_leaf_directories(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "xdg"
            with mock.patch.object(dev_env, "DEV_XDG_ROOT", root):
                dev_env.ensure_dev_dirs()
                for leaf in ("config/sag", "data/sag", "state/sag/logs"):
                    self.assertTrue((root / leaf).is_dir(), leaf)


class CliSmokeTest(unittest.TestCase):
    def test_execs_command_with_absolute_xdg_data_home(self) -> None:
        script = dev_env.PROJECT_ROOT / "scripts" / "dev_env.py"
        proc = subprocess.run(
            [
                sys.executable,
                str(script),
                "--",
                "sh",
                "-c",
                "printf %s $XDG_DATA_HOME",
            ],
            cwd=str(dev_env.PROJECT_ROOT),
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(proc.stdout, str(dev_env.DEV_XDG_ROOT / "data"))
        self.assertTrue(Path(proc.stdout).is_absolute())

    def test_empty_command_exits_2(self) -> None:
        """`--` 后无命令 → parser.error（exit 2），不会 exec 空 argv."""
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            with self.assertRaises(SystemExit) as ctx:
                dev_env.main(["--"])
        self.assertEqual(ctx.exception.code, 2)
        self.assertIn("-- <cmd...>", err.getvalue())

    def test_missing_command_reports_clean_error(self) -> None:
        """命令不存在 → 清晰的 `命令不存在` 报错 + exit 2，而不是裸 traceback."""
        err = io.StringIO()
        with mock.patch.object(dev_env, "ensure_dev_dirs"):
            with contextlib.redirect_stderr(err):
                with self.assertRaises(SystemExit) as ctx:
                    dev_env.main(["--", "sag-definitely-not-a-command"])
        self.assertEqual(ctx.exception.code, 2)
        stderr = err.getvalue()
        self.assertIn("命令不存在: sag-definitely-not-a-command", stderr)
        self.assertNotIn("Traceback", stderr)


if __name__ == "__main__":
    unittest.main()
