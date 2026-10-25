"""release_notes.py 的 git 集成测试 + CLI 错误路径测试。

纯函数层（`parse_conventional` / `select_prev_tag` / `render_notes` ...）的测试在
`test_release_notes.py`；本文件只覆盖：

1. 临时 git 仓库端到端：真正建 commit / tag，调 `main()`，断言分组、release commit
   剔除、compare 链接、`--range` / `--prev-tag` 覆盖与首版 fallback。
2. CLI 错误矩阵：非法/不存在的 tag、缺失 tag、非法 `--date` / `--repo` /
   `--output` → `SystemExit`（fail-closed）。
   注意 `fail()` 用 `sys.exit("::error:: ...")`，`SystemExit.code` 是那条 annotation
   字符串；真正的「退出码 1 + stderr 上的 `::error::`」由解释器产生，因此每个错误
   路径同时用真实子进程断言一遍（`CompletedProcess.returncode == 1`）。
3. 浅克隆（`git clone --depth 1`）：tag 集不完整 → 必须 fail-closed，不得降级成
   「首版 + 空 changelog」并 exit 0（否则会静默发布空 release body）。

`scripts/release_notes.py` 的 git 封装（`git()` / `list_version_tags()` /
`log_range()` / `commit_date()`）**没有传 `cwd`**，subprocess 继承进程 cwd，
因此 `main()` 必须在临时仓库目录里执行 —— 见 `GitRepoTestCase._run_main()` 的
`os.chdir` + `finally` 还原。

两种运行方式均支持：
  python3 scripts/tests/test_release_notes_git.py -v
  python3 -m unittest discover -s scripts/tests -t scripts -p 'test_release_notes_git.py' -v
"""

from __future__ import annotations

import contextlib
import io
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from contextlib import contextmanager
from pathlib import Path
from typing import Iterator

_SCRIPTS_DIR = Path(__file__).resolve().parents[1]
if str(_SCRIPTS_DIR) not in sys.path:
    sys.path.insert(0, str(_SCRIPTS_DIR))

import release_notes  # noqa: E402  (需先修好 sys.path)

GIT = shutil.which("git")

# 宿主的 GITHUB_* 变量会从 `main()` 的默认值逻辑泄漏进来（未传 `--tag` 时为
# `$GITHUB_REF_NAME`、未传 `--repo` 时为 `$GITHUB_REPOSITORY`）。测试必须与宿主/CI
# 环境密闭，否则「无 --tag → fail」「无 --repo → 纯文本」这些语义在本地与 CI 上
# 表现不同（CI 的 test job 就带着这两个变量）。
_LEAKY_GITHUB_VARS = ("GITHUB_REF_NAME", "GITHUB_REPOSITORY", "GITHUB_ACTIONS")

# git 的仓库定位变量：从 hook / rebase / submodule 上下文里跑测试时它们会残留，
# 此时 `git -C <tmp>` 照样被重定向到外部仓库，fixture 会全线崩溃且报错难懂。
_LEAKY_GIT_VARS = ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE")

# 需要从子进程 env 与 os.environ 两侧对称摘除的全部变量。
_LEAKY_ENV_VARS = _LEAKY_GITHUB_VARS + _LEAKY_GIT_VARS

# CI / 开发机上不允许依赖任何用户级或系统级 git 配置，也不继承 GITHUB_* / GIT_* 变量。
_GIT_ENV = {
    key: value for key, value in os.environ.items() if key not in _LEAKY_ENV_VARS
}
_GIT_ENV.update(
    {
        "GIT_CONFIG_GLOBAL": os.devnull,
        "GIT_CONFIG_SYSTEM": os.devnull,
        "GIT_AUTHOR_NAME": "t",
        "GIT_AUTHOR_EMAIL": "t@t",
        "GIT_COMMITTER_NAME": "t",
        "GIT_COMMITTER_EMAIL": "t@t",
    }
)


@contextmanager
def _sanitized_github_env() -> Iterator[None]:
    """临时把 GITHUB_* 与 GIT_* 定位变量从 `os.environ` 里摘掉，模拟干净的本地环境。"""
    saved = {key: os.environ.pop(key) for key in _LEAKY_ENV_VARS if key in os.environ}
    try:
        yield
    finally:
        os.environ.update(saved)


GIT_IDENT = (
    "-c",
    "user.email=t@t",
    "-c",
    "user.name=t",
    "-c",
    "commit.gpgsign=false",
    "-c",
    "tag.gpgsign=false",
)


@unittest.skipUnless(GIT, "git 不可用")
class GitRepoTestCase(unittest.TestCase):
    """在临时 git 仓库里端到端跑 `main()` 的基类。"""

    repo: Path
    tmp: Path

    def setUp(self) -> None:
        tmp = tempfile.TemporaryDirectory(prefix="sag-release-notes-")
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name).resolve()
        self.repo = self.tmp / "repo"
        self.repo.mkdir()
        self._git("init", "-q")
        self.last_stdout = ""
        self.last_stderr = ""

    # ── git 夹具 ─────────────────────────────────────────────────────

    def _git(self, *args: str) -> str:
        proc = subprocess.run(
            [GIT, "-C", str(self.repo), *GIT_IDENT, *args],
            capture_output=True,
            text=True,
            check=False,
            env=_GIT_ENV,
        )
        self.assertEqual(
            proc.returncode,
            0,
            msg=f"git {' '.join(args)} 失败: {proc.stderr or proc.stdout}",
        )
        return proc.stdout

    def _commit(
        self,
        subject: str,
        date: str,
        body: str | None = None,
        *,
        full_date: str | None = None,
    ) -> None:
        """建一个空提交。

        `date` 是 `YYYY-MM-DD`，默认展开成当天 `12:00:00 +0000`（UTC 正午，避开
        换日歧义）；需要钉住「committer 时区日历日 ≠ UTC 日」时用 `full_date`
        传完整时间戳（git 的 `GIT_*_DATE` 格式，如 `2024-01-07T02:00:00 +0800`）。
        """
        stamp = full_date or f"{date}T12:00:00 +0000"
        env = {
            **_GIT_ENV,
            "GIT_AUTHOR_DATE": stamp,
            "GIT_COMMITTER_DATE": stamp,
        }
        message_args = ["-m", subject]
        if body is not None:
            message_args += ["-m", body]
        proc = subprocess.run(
            [
                GIT,
                "-C",
                str(self.repo),
                *GIT_IDENT,
                "commit",
                "-q",
                "--allow-empty",
                *message_args,
            ],
            capture_output=True,
            text=True,
            check=False,
            env=env,
        )
        self.assertEqual(
            proc.returncode, 0, msg=f"git commit 失败: {proc.stderr or proc.stdout}"
        )

    def _build_base_history(self) -> None:
        """造 c1..c5，并把轻量 tag `v0.1.0` 打在 release commit 上。"""
        self._commit("feat(ui): add sidebar", "2024-01-01")
        self._commit("fix: correct typo", "2024-01-02")
        self._commit("chore: release v0.1.0", "2024-01-03")
        self._git("tag", "v0.1.0")
        self._commit("feat(api): add models endpoint", "2024-01-04")
        self._commit("docs: update readme", "2024-01-05")

    def _add_release_020(self) -> None:
        """补上 `chore: release v0.2.0` commit 与轻量 tag `v0.2.0`。"""
        self._commit("chore: release v0.2.0", "2024-01-06")
        self._git("tag", "v0.2.0")

    # ── main() 调用 ──────────────────────────────────────────────────

    def _run_main(self, *argv: str) -> int:
        """在临时仓库 cwd 下、以给定 argv 调 `main()`；捕获 stdout/stderr。

        `main()` 内部的 git 调用继承进程 cwd，所以必须 chdir 进临时仓库；
        `finally` 一定还原 cwd 与 sys.argv，避免污染其他用例。执行期间临时摘掉
        GITHUB_* 变量，保证结果不依赖宿主环境（见 `_sanitized_github_env`）。
        """
        out, err = io.StringIO(), io.StringIO()
        prev_argv = sys.argv
        prev_cwd = os.getcwd()
        sys.argv = ["release_notes.py", *argv]
        try:
            os.chdir(self.repo)
            with _sanitized_github_env():
                with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                    return release_notes.main()
        finally:
            os.chdir(prev_cwd)
            sys.argv = prev_argv
            self.last_stdout = out.getvalue()
            self.last_stderr = err.getvalue()

    def _run_cli(
        self, *argv: str, cwd: Path | None = None
    ) -> subprocess.CompletedProcess[str]:
        """以真实子进程跑 CLI（默认 cwd = 临时仓库），断言解释器层面的退出码。"""
        return subprocess.run(
            [sys.executable, str(_SCRIPTS_DIR / "release_notes.py"), *argv],
            capture_output=True,
            text=True,
            check=False,
            cwd=self.repo if cwd is None else cwd,
            env=_GIT_ENV,
        )


class GitIntegrationTest(GitRepoTestCase):
    def setUp(self) -> None:
        super().setUp()
        self._build_base_history()

    def test_auto_discovered_prev_tag_groups_and_compare_link(self) -> None:
        """默认自动发现前驱 tag：分组正确、release commit 剔除、compare 链接就位。"""
        self._add_release_020()
        out_path = self.tmp / "notes.md"

        code = self._run_main(
            "--tag",
            "v0.2.0",
            "--output",
            str(out_path),
            "--repo",
            "rainbowatcher/sag",
        )

        self.assertEqual(code, 0, msg=self.last_stderr)
        self.assertTrue(out_path.exists(), "release notes 未写盘")
        body = out_path.read_text(encoding="utf-8")
        self.assertIn("[INFO] release notes 已写入", self.last_stderr)

        # 标题用 tag 原文 + tag 指向 commit 的 committer date（committer 时区日历日，非 UTC）
        self.assertTrue(body.startswith("## sag v0.2.0 (2024-01-06)\n"), msg=body)

        # 范围是 v0.1.0..v0.2.0：只有 feat(api) 与 docs 两条，release commit 被剔除
        self.assertIn(
            "自 [`v0.1.0`](https://github.com/rainbowatcher/sag/releases/tag/v0.1.0)"
            " 以来共 2 个提交。",
            body,
        )
        self.assertIn("### ✨ 新增", body)
        self.assertIn("feat(api): add models endpoint", body)
        self.assertIn("### 📚 文档", body)
        self.assertIn("docs: update readme", body)
        self.assertNotIn("chore: release", body)
        self.assertNotIn("feat(ui): add sidebar", body, "v0.1.0 之前的提交不应出现")
        self.assertNotIn("### 🐛 修复", body)

        self.assertIn(
            "**完整变更日志**: [`v0.1.0...v0.2.0`]"
            "(https://github.com/rainbowatcher/sag/compare/v0.1.0...v0.2.0)",
            body,
        )
        self.assertNotIn(release_notes.FIRST_RELEASE_HINT, body)

    def test_breaking_change_footer_is_marked_breaking(self) -> None:
        """body 里的 `BREAKING CHANGE:` footer 与 `!` 等效（端到端验 `%b` 取到了）。"""
        self._commit(
            "feat(api): drop v1",
            "2024-01-06",
            body="legacy clients break\n\nBREAKING CHANGE: drop v1 endpoints",
        )
        self._add_release_020()

        code = self._run_main("--tag", "v0.2.0")

        self.assertEqual(code, 0, msg=self.last_stderr)
        body = self.last_stdout
        self.assertIn("- feat(api)!: drop v1", body)
        self.assertNotIn("- feat(api): drop v1", body)
        # 同一次渲染里的普通提交不得被误标
        self.assertIn("feat(api): add models endpoint", body)
        self.assertNotIn("feat(ui)!:", body)
        self.assertNotIn("models endpoint!:", body)

    def test_commit_body_without_footer_is_not_marked_breaking(self) -> None:
        """普通 body（无 footer）不应把提交标成 breaking。"""
        self._commit(
            "feat(api): add models endpoint",
            "2024-01-06",
            body="Refs: #12\n\nnotes for reviewers",
        )
        self._add_release_020()

        code = self._run_main("--tag", "v0.2.0")

        self.assertEqual(code, 0, msg=self.last_stderr)
        self.assertIn("feat(api): add models endpoint", self.last_stdout)
        self.assertNotIn("feat(api)!:", self.last_stdout)

    def test_title_date_uses_committer_timezone_not_utc(self) -> None:
        """标题日期取 committer 时区下的日历日，不是 UTC 日（可复现性不变量）。

        commit 的 `+0800` 本地时间是 2024-01-07T02:00，对应的 UTC 日历日是
        2024-01-06T18:00（前一天）；标题必须是 2024-01-07 —— 任何「转 UTC 再
        取日期」或「用 CI 当天日期」的实现都会在这里翻车。
        """
        self._commit(
            "feat(api): midnight commit",
            "2024-01-07",
            full_date="2024-01-07T02:00:00 +0800",
        )
        self._git("tag", "v0.2.0")

        # 用例前提：该 commit 的 UTC 日历日确实与 committer 日历日不同
        iso = self._git("log", "-1", "--format=%cI", "v0.2.0").strip()
        self.assertEqual(iso, "2024-01-07T02:00:00+08:00")

        code = self._run_main("--tag", "v0.2.0")

        self.assertEqual(code, 0, msg=self.last_stderr)
        body = self.last_stdout
        self.assertTrue(body.startswith("## sag v0.2.0 (2024-01-07)\n"), msg=body)
        self.assertNotIn("2024-01-06", body.split("\n")[0])
        self.assertIn("feat(api): midnight commit", body)

    def test_explicit_range_overrides_tag_discovery(self) -> None:
        """`--range` 完全接管范围：全历史（4 条）而非自动发现的 2 条。"""
        self._add_release_020()

        code = self._run_main("--tag", "v0.2.0", "--range", "v0.2.0")

        self.assertEqual(code, 0, msg=self.last_stderr)
        body = self.last_stdout
        self.assertTrue(body.startswith("## sag v0.2.0 (2024-01-06)\n"), msg=body)
        # 全历史：feat(ui) + fix + feat(api) + docs（两条 release commit 剔除）
        self.assertIn("共 4 个提交。", body)
        self.assertIn("feat(ui): add sidebar", body)
        self.assertIn("### 🐛 修复", body)
        self.assertIn("fix: correct typo", body)
        self.assertIn("feat(api): add models endpoint", body)
        self.assertNotIn("chore: release", body)
        # --range 单独使用时 prev_tag 为 None → 无 compare 链接
        self.assertNotIn("完整变更日志", body)

    def test_explicit_prev_tag_narrows_range(self) -> None:
        """`--prev-tag` 接管自动发现：范围收缩成 v0.1.0..v0.2.0。"""
        self._add_release_020()

        code = self._run_main("--tag", "v0.2.0", "--prev-tag", "v0.1.0")

        self.assertEqual(code, 0, msg=self.last_stderr)
        body = self.last_stdout
        self.assertTrue(body.startswith("## sag v0.2.0 (2024-01-06)\n"), msg=body)
        # 范围 = v0.1.0..v0.2.0：feat(api) + docs，release commit 剔除
        self.assertIn("自 `v0.1.0` 以来共 2 个提交。", body)
        self.assertIn("feat(api): add models endpoint", body)
        self.assertIn("docs: update readme", body)
        self.assertNotIn("feat(ui): add sidebar", body, "v0.1.0 之前的提交不应出现")
        self.assertNotIn("fix: correct typo", body, "v0.1.0 之前的提交不应出现")
        self.assertNotIn("chore: release", body)
        # prev_tag 参与渲染：compare 链接指向显式给定的前驱 tag
        self.assertIn("**完整变更日志**: `v0.1.0...v0.2.0`", body)
        self.assertNotIn(release_notes.FIRST_RELEASE_HINT, body)
        self.assertEqual(self.last_stderr, "", "显式 --prev-tag 走正常路径，不应告警")

        proc = self._run_cli("--tag", "v0.2.0", "--prev-tag", "v0.1.0")
        self.assertEqual(proc.returncode, 0, msg=proc.stderr)
        self.assertEqual(proc.stdout, body)
        self.assertIn("feat(api): add models endpoint", proc.stdout)

    def test_no_repo_and_no_env_renders_plain_text(self) -> None:
        """无 `--repo` 且环境无 `GITHUB_REPOSITORY` → 全篇纯文本，不含任何链接。"""
        self._add_release_020()

        code = self._run_main("--tag", "v0.2.0")

        self.assertEqual(code, 0, msg=self.last_stderr)
        body = self.last_stdout
        self.assertNotIn("https://github.com", body)
        self.assertNotIn("](", body, "无 repo 时不应出现任何 markdown 链接")
        # 计数行 / compare 行 / 提交引用全部退化为 code span
        self.assertIn("自 `v0.1.0` 以来共 2 个提交。", body)
        self.assertIn("**完整变更日志**: `v0.1.0...v0.2.0`", body)
        self.assertIn("(`", body)

        proc = self._run_cli("--tag", "v0.2.0")
        self.assertEqual(proc.returncode, 0, msg=proc.stderr)
        self.assertEqual(proc.stdout, body)

    def test_first_release_uses_full_history_and_hint(self) -> None:
        """只有 v0.1.0 且取它作 tag：无前驱版本 → fallback 到全历史 + 首版提示。"""
        code = self._run_main("--tag", "v0.1.0")

        self.assertEqual(code, 0, msg=self.last_stderr)
        body = self.last_stdout
        self.assertIn(release_notes.FIRST_RELEASE_HINT, body)
        self.assertIn("首个发布版本", body)
        self.assertIn("## sag v0.1.0 (2024-01-03)", body)
        self.assertIn("共 2 个提交。", body)
        self.assertIn("feat(ui): add sidebar", body)
        self.assertIn("### 🐛 修复", body)
        self.assertNotIn("chore: release", body)
        self.assertNotIn("完整变更日志", body)
        self.assertIn("::warning::", self.last_stderr)


class CliErrorPathTest(GitRepoTestCase):
    def setUp(self) -> None:
        super().setUp()
        self._build_base_history()
        self._add_release_020()

    def test_invalid_tag_fails_closed(self) -> None:
        """非法 tag → fail-closed（`::error::` annotation，退出码 1）。"""
        with self.assertRaises(SystemExit) as ctx:
            self._run_main("--tag", "foo")

        # sys.exit(str) 的 SystemExit.code 就是该 annotation 字符串；
        # 解释器随后把它写到 stderr 并以退出码 1 结束进程（见下方子进程断言）。
        self.assertEqual(
            ctx.exception.code, "::error:: tag 格式非法: foo（应为 vX.Y.Z）"
        )
        self.assertEqual(self.last_stdout, "", "失败路径不应产出 markdown body")
        self.assertEqual(self.last_stderr, "")

        proc = self._run_cli("--tag", "foo")
        self.assertEqual(proc.returncode, 1, msg=proc.stderr)
        self.assertIn("::error::", proc.stderr)
        self.assertIn("tag 格式非法", proc.stderr)
        self.assertEqual(proc.stdout, "")

    def test_nonexistent_tag_fails_closed(self) -> None:
        """格式合法但仓库里不存在的 tag → fail-closed（`git tag --merged` 报错）。"""
        with self.assertRaises(SystemExit) as ctx:
            self._run_main("--tag", "v9.9.9")

        code = ctx.exception.code
        self.assertIsInstance(code, str, msg="sys.exit 应携带 ::error:: annotation")
        self.assertIn("::error::", code)
        self.assertIn("无法列出 tag", code)
        self.assertEqual(self.last_stdout, "", "失败路径不应产出 markdown body")
        self.assertEqual(self.last_stderr, "")

        proc = self._run_cli("--tag", "v9.9.9")
        self.assertEqual(proc.returncode, 1, msg=proc.stderr)
        self.assertIn("::error::", proc.stderr)
        self.assertIn("无法列出 tag", proc.stderr)
        self.assertEqual(proc.stdout, "")

    def test_missing_tag_and_no_env_fails_closed(self) -> None:
        """未传 `--tag` 且环境无 `GITHUB_REF_NAME` → fail-closed（不猜 tag）。"""
        with self.assertRaises(SystemExit) as ctx:
            self._run_main()

        code = ctx.exception.code
        self.assertIsInstance(code, str, msg="sys.exit 应携带 ::error:: annotation")
        self.assertIn("::error::", code)
        self.assertIn("无法确定 tag", code)
        self.assertEqual(self.last_stdout, "")

        proc = self._run_cli()
        self.assertEqual(proc.returncode, 1, msg=proc.stderr)
        self.assertIn("::error::", proc.stderr)
        self.assertIn("无法确定 tag", proc.stderr)
        self.assertEqual(proc.stdout, "")

    def test_invalid_date_fails_closed(self) -> None:
        """`--date 2024-13-99`（格式对、日期非法）→ fail-closed。"""
        with self.assertRaises(SystemExit) as ctx:
            self._run_main("--tag", "v0.2.0", "--date", "2024-13-99")

        code = ctx.exception.code
        self.assertIsInstance(code, str, msg="sys.exit 应携带 ::error:: annotation")
        self.assertIn("::error::", code)
        self.assertIn("--date 格式非法", code)
        self.assertEqual(self.last_stdout, "")

        proc = self._run_cli("--tag", "v0.2.0", "--date", "2024-13-99")
        self.assertEqual(proc.returncode, 1, msg=proc.stderr)
        self.assertIn("::error::", proc.stderr)
        self.assertIn("--date 格式非法", proc.stderr)
        self.assertEqual(proc.stdout, "")

    def test_repo_without_slash_fails_closed(self) -> None:
        """`--repo foo`（缺 OWNER/NAME 的斜杠）→ fail-closed。"""
        with self.assertRaises(SystemExit) as ctx:
            self._run_main("--tag", "v0.2.0", "--repo", "foo")

        code = ctx.exception.code
        self.assertIsInstance(code, str, msg="sys.exit 应携带 ::error:: annotation")
        self.assertIn("::error::", code)
        self.assertIn("--repo 格式非法", code)
        self.assertEqual(self.last_stdout, "")

        proc = self._run_cli("--tag", "v0.2.0", "--repo", "foo")
        self.assertEqual(proc.returncode, 1, msg=proc.stderr)
        self.assertIn("::error::", proc.stderr)
        self.assertIn("--repo 格式非法", proc.stderr)
        self.assertEqual(proc.stdout, "")

    def test_prev_tag_not_older_than_tag_fails_closed(self) -> None:
        """`--prev-tag` 版本 >= `--tag`（方向反了）→ fail-closed。"""
        with self.assertRaises(SystemExit) as ctx:
            self._run_main("--tag", "v0.2.0", "--prev-tag", "v0.3.0")

        code = ctx.exception.code
        self.assertIsInstance(code, str, msg="sys.exit 应携带 ::error:: annotation")
        self.assertIn("::error::", code)
        self.assertIn("--prev-tag 必须小于", code)
        self.assertEqual(self.last_stdout, "", "失败路径不应产出 markdown body")

        # 版本相等也算方向错：范围 v0.2.0..v0.2.0 恒为空
        with self.assertRaises(SystemExit) as ctx_eq:
            self._run_main("--tag", "v0.2.0", "--prev-tag", "v0.2.0")
        self.assertIn("--prev-tag 必须小于", str(ctx_eq.exception.code))

        proc = self._run_cli("--tag", "v0.2.0", "--prev-tag", "v0.3.0")
        self.assertEqual(proc.returncode, 1, msg=proc.stderr)
        self.assertIn("::error::", proc.stderr)
        self.assertIn("--prev-tag 必须小于", proc.stderr)
        self.assertEqual(proc.stdout, "")

    def test_range_and_prev_tag_together_fail_closed(self) -> None:
        """`--range` 与 `--prev-tag` 同时给出 → fail-closed（不静默丢弃其一）。"""
        with self.assertRaises(SystemExit) as ctx:
            self._run_main(
                "--tag",
                "v0.2.0",
                "--range",
                "v0.1.0..v0.2.0",
                "--prev-tag",
                "v0.1.0",
            )

        code = ctx.exception.code
        self.assertIsInstance(code, str, msg="sys.exit 应携带 ::error:: annotation")
        self.assertIn("::error::", code)
        self.assertIn("--range 与 --prev-tag 不能同时使用", code)
        self.assertEqual(self.last_stdout, "", "失败路径不应产出 markdown body")

        proc = self._run_cli(
            "--tag",
            "v0.2.0",
            "--range",
            "v0.1.0..v0.2.0",
            "--prev-tag",
            "v0.1.0",
        )
        self.assertEqual(proc.returncode, 1, msg=proc.stderr)
        self.assertIn("::error::", proc.stderr)
        self.assertIn("--range 与 --prev-tag 不能同时使用", proc.stderr)
        self.assertEqual(proc.stdout, "")

    def test_empty_range_fails_closed(self) -> None:
        """`--range ""`（空串）→ fail-closed，不退回自动发现。"""
        with self.assertRaises(SystemExit) as ctx:
            self._run_main("--tag", "v0.2.0", "--range", "")

        code = ctx.exception.code
        self.assertIsInstance(code, str, msg="sys.exit 应携带 ::error:: annotation")
        self.assertIn("::error::", code)
        self.assertIn("--range 不能为空", code)
        self.assertEqual(self.last_stdout, "", "失败路径不应产出 markdown body")

        proc = self._run_cli("--tag", "v0.2.0", "--range", "")
        self.assertEqual(proc.returncode, 1, msg=proc.stderr)
        self.assertIn("::error::", proc.stderr)
        self.assertIn("--range 不能为空", proc.stderr)
        self.assertEqual(proc.stdout, "")

    def test_output_into_missing_directory_fails_closed(self) -> None:
        """`--output` 指向不存在的目录 → fail-closed，且不留下半成品文件。"""
        out_path = self.tmp / "nope" / "x.md"

        with self.assertRaises(SystemExit) as ctx:
            self._run_main("--tag", "v0.2.0", "--output", str(out_path))

        code = ctx.exception.code
        self.assertIsInstance(code, str, msg="sys.exit 应携带 ::error:: annotation")
        self.assertIn("::error::", code)
        self.assertIn("无法写入", code)
        self.assertEqual(self.last_stdout, "")
        self.assertFalse(out_path.exists(), "失败路径不应写盘")

        proc = self._run_cli("--tag", "v0.2.0", "--output", str(out_path))
        self.assertEqual(proc.returncode, 1, msg=proc.stderr)
        self.assertIn("::error::", proc.stderr)
        self.assertIn("无法写入", proc.stderr)
        self.assertFalse(out_path.exists())


@unittest.skipUnless(GIT, "git 不可用")
class ShallowCloneTest(GitRepoTestCase):
    """浅克隆（`--depth 1`）下 tag 集与历史都不完整，必须 fail-closed。"""

    def _make_shallow_clone(self) -> Path:
        """建好 v0.1.0/v0.2.0 历史后克隆一份 `--depth 1 --branch v0.2.0` 的浅仓库。"""
        self._build_base_history()
        self._add_release_020()

        shallow = self.tmp / "shallow"
        # 必须用 file:// URL（`as_uri()`）：本地路径走硬链接快路径，`--depth`
        # 会被 git 静默忽略，克隆出来是完整历史，用例就变成假绿。
        subprocess.run(
            [
                GIT,
                "clone",
                "-q",
                "--depth",
                "1",
                "--branch",
                "v0.2.0",
                self.repo.as_uri(),
                str(shallow),
            ],
            capture_output=True,
            text=True,
            check=True,
            env=_GIT_ENV,
        )

        # 用例前提：克隆真的是浅的（否则下面的断言毫无意义）。
        probe = subprocess.run(
            [GIT, "-C", str(shallow), "rev-parse", "--is-shallow-repository"],
            capture_output=True,
            text=True,
            check=True,
            env=_GIT_ENV,
        )
        self.assertEqual(probe.stdout.strip(), "true", "克隆未走 --depth，用例无效")
        return shallow

    def test_shallow_clone_fails_closed(self) -> None:
        """浅克隆里只看得到一个 tag → 不得降级成「首版 + 空 changelog」。"""
        shallow = self._make_shallow_clone()

        out_path = self.tmp / "shallow-notes.md"
        proc = self._run_cli(
            "--tag", "v0.2.0", "--output", str(out_path), cwd=shallow
        )

        self.assertEqual(proc.returncode, 1, msg=proc.stderr)
        self.assertIn("::error::", proc.stderr)
        self.assertIn("浅克隆", proc.stderr)
        # 必须走 fail-closed 而非「首版降级」的告警路径
        self.assertNotIn("::warning::", proc.stderr)
        self.assertEqual(proc.stdout, "")
        self.assertFalse(out_path.exists(), "浅克隆失败路径不应写盘")

    def test_shallow_clone_range_escape_hatch_still_works(self) -> None:
        """浅克隆守卫只作用于自动发现分支：显式 `--range` 必须照常放行。

        钉住守卫的位置 —— 若后续把它上移到 `_resolve_range()` 开头，逃生舱会
        一起失效，而 fail-closed 用例仍全绿。
        """
        shallow = self._make_shallow_clone()

        proc = self._run_cli("--tag", "v0.2.0", "--range", "HEAD", cwd=shallow)

        self.assertEqual(proc.returncode, 0, msg=proc.stderr)
        self.assertNotIn("::error::", proc.stderr)
        self.assertIn("## sag v0.2.0", proc.stdout)


if __name__ == "__main__":
    unittest.main()
