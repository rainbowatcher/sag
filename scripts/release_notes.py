#!/usr/bin/env python3
"""release_notes.py — 生成 GitHub Release 说明（markdown body）。

用法:
  python3 scripts/release_notes.py [--tag TAG] [--prev-tag TAG] [--range A..B]
                                   [--output PATH] [--repo OWNER/NAME]
                                   [--date YYYY-MM-DD]

行为:
  1. 确定当前 tag（`--tag` > `$GITHUB_REF_NAME`），必须是 `vX.Y.Z` 形态。
  2. 确定 git log 范围，优先级: `--range` > `--prev-tag` > 自动发现。
     自动发现走 `git tag --merged <tag> --list 'v*'`，由纯函数
     `select_prev_tag()` 在祖先链上的版本 tag 里按 semver 取前驱，
     因此跳版（v0.2.0 → v0.5.0）与分支上的 hotfix tag 都不会算错。
  3. 解析 `<sha><US><subject><US><body><RS>` 记录流，按 conventional commit type
     分组，剔除 release commit（`chore: release vX.Y.Z`）后渲染 markdown。
     breaking 变更的两种声明方式等价：header 里的 `!`（`feat(api)!: ...`）与
     commit body 里的 `BREAKING CHANGE:` / `BREAKING-CHANGE:` footer，任一出现
     都会在渲染时补上 `!` 标记。
  4. 写到 `--output`（默认 stdout）。CI 传 `release-notes.md`，注意
     **不要写进 dist/**，否则会被 release 的 `files: dist/*` 当资产上传。

错误处理分层（详见 ERROR 段）:
  - tag / range 解析失败（含 `--prev-tag` 不小于 `--tag`）、git 命令失败、`--output` 不可写 → fail-closed，
    `sys.exit("::error:: ...")`（GitHub annotation + 退出码 1）。
  - 浅克隆（历史与 tag 集不完整）下无法确认前驱 tag → 同样 fail-closed，
    提示设置 `fetch-depth: 0` 或用 `--prev-tag` / `--range` 逃生舱。
  - 无前一个 tag（首版）、范围内 0 个提交、非 conventional commit、
    无 `--repo` → 降级渲染，只告警（`::warning::`，走 stderr），不阻塞发版。

模块分层（架构守则）:
  - 纯函数层（`parse_conventional` / `is_release_commit` / `has_breaking_footer` /
    `classify` / `parse_commits` / `select_prev_tag` / `render_notes` / `short_sha`）
    不 import subprocess、不碰文件系统，可脱离 git 仓库单测。
  - git 边界层（`git` / `is_shallow_repo` / `list_version_tags` / `log_range` /
    `commit_date`）只做 subprocess 薄封装，不做任何判定逻辑。

退出码: 0 成功；1 失败（fail-closed，不产出 body）。
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from datetime import datetime
from pathlib import Path
from typing import NamedTuple, NoReturn

# 版本 tag 形态：唯一认可的 tag 形态，其余（vnext、nightly 等）一律忽略。
TAG_RE = re.compile(r"^v(\d+)\.(\d+)\.(\d+)$")

# release commit 由 scripts/release.py 生成（`chore: release vX.Y.Z`），
# 一律从 changelog 剔除 —— 它是本次发版动作本身，不是用户可见变更。
RELEASE_COMMIT_RE = re.compile(r"^chore(?:\([^)]*\))?: release v\d+\.\d+\.\d+$")

# conventional commits: type(scope)!: subject（scope / `!` 均可选）
CONVENTIONAL_RE = re.compile(
    r"^(?P<type>[A-Za-z]+)(?:\((?P<scope>[^()]*)\))?(?P<breaking>!)?: (?P<subject>.+)$"
)

# breaking 变更的 footer 形式：`BREAKING CHANGE: <desc>` 或 `BREAKING-CHANGE: <desc>`。
# 按 conventional commits 规范，两个 token 必须全大写（小写不算），且位于行首。
BREAKING_FOOTER_RE = re.compile(r"^BREAKING[ -]CHANGE:", re.MULTILINE)

# git log 的字段 / 记录分隔符：`%x1f` 分隔 sha / subject / body，`%x1e` 把一条提交
# 框成一条记录 —— body 可含换行，所以不能再按行解析。
FIELD_SEP = "\x1f"
RECORD_SEP = "\x1e"

# 组顺序是渲染层唯一映射点：新增组时同时改这里与 TYPE_GROUPS。
GROUP_ORDER: tuple[tuple[str, str], ...] = (
    ("feature", "✨ 新增"),
    ("fix", "🐛 修复"),
    ("refactor", "♻️ 重构"),
    ("docs", "📚 文档"),
    ("misc", "🔧 杂项"),
    ("other", "📦 其他"),
)

# 显式列出的 type → 组；未列出的 conventional type 归 "misc"（容忍未知）。
TYPE_GROUPS: dict[str, str] = {
    "feat": "feature",
    "fix": "fix",
    "refactor": "refactor",
    "docs": "docs",
}

# markdown 需要转义的字符：反引号/方括号/尖括号可注入代码段、链接与 HTML 标签。
_ESCAPE_CHARS = "`[]<>"

FIRST_RELEASE_HINT = "> 首个发布版本，以下为全部历史提交。"
EMPTY_HINT = "本次发布无提交记录。"


# ── 纯函数层（无 subprocess / 无文件系统）─────────────────────────────


class Conventional(NamedTuple):
    """`parse_conventional()` 的解析结果。"""

    type: str
    scope: str | None
    subject: str
    breaking: bool


class Commit(NamedTuple):
    """`parse_commits()` 的解析结果（只保留渲染所需字段）。

    `body` 是 commit message 的正文（不含 subject 首行），仅用于识别
    `BREAKING CHANGE:` footer；纯函数单测里可以省略。
    """

    sha: str
    subject: str
    body: str = ""


def parse_conventional(subject: str) -> Conventional | None:
    """解析 conventional commit subject；不匹配返回 None（进「其他」组）。"""
    m = CONVENTIONAL_RE.match(subject.strip())
    if not m:
        return None
    text = m.group("subject").strip()
    if not text:
        return None
    scope = m.group("scope")
    return Conventional(
        type=m.group("type"),
        scope=scope.strip() if scope and scope.strip() else None,
        subject=text,
        breaking=bool(m.group("breaking")),
    )


def is_release_commit(subject: str) -> bool:
    """判断是否为 release commit（`chore: release vX.Y.Z` / 带 scope 的变体）。"""
    return bool(RELEASE_COMMIT_RE.match(subject.strip()))


def has_breaking_footer(body: str) -> bool:
    """commit body 是否声明了 `BREAKING CHANGE:` / `BREAKING-CHANGE:` footer。"""
    return bool(BREAKING_FOOTER_RE.search(body or ""))


def classify(type_: str | None) -> str:
    """把 conventional type 映射成组键；None（非 conventional）→ "other"。"""
    if type_ is None:
        return "other"
    return TYPE_GROUPS.get(type_.lower(), "misc")


def parse_commits(raw: str) -> list[Commit]:
    """解析 `git log --format=%H%x1f%s%x1f%b%x1e` 的记录流；坏记录跳过，顺序保持（新→旧）。

    git log 会在每条记录后再插一个换行，记录首尾空白不属于内容（`strip`）。
    body 之后的字段不存在（老格式只有两个字段）时按空 body 处理。
    """
    commits: list[Commit] = []
    for record in raw.split(RECORD_SEP):
        record = record.strip()
        if not record:
            continue
        sha, sep, rest = record.partition(FIELD_SEP)
        if not sep:
            continue
        subject, _, body = rest.partition(FIELD_SEP)
        sha = sha.strip()
        subject = subject.strip()
        if not sha or not subject:
            continue
        commits.append(Commit(sha=sha, subject=subject, body=body.strip()))
    return commits


def _version_key(tag: str) -> tuple[int, int, int] | None:
    """把版本 tag 解析成可排序的 (major, minor, patch)；非版本 tag → None。"""
    m = TAG_RE.match(tag.strip())
    if not m:
        return None
    return (int(m.group(1)), int(m.group(2)), int(m.group(3)))


def select_prev_tag(tags: list[str], current: str) -> str | None:
    """从 tags 里选出 current 的前驱版本 tag（严格小于 current 的最大版本）。

    只做纯版本序比较：ancestry 过滤已在 `list_version_tags()` 里由
    `git tag --merged` 完成。非 `vX.Y.Z` tag 与 current 自身被排除；
    没有更小的版本时返回 None（首版）。
    """
    cur = _version_key(current)
    if cur is None:
        return None
    candidates = [
        (key, tag)
        for tag in tags
        if tag.strip() != current.strip()
        for key in (_version_key(tag),)
        if key is not None and key < cur
    ]
    if not candidates:
        return None
    return max(candidates)[1]


def short_sha(sha: str) -> str:
    """7 位短 SHA，仅用于显示；链接始终用完整 SHA。"""
    return sha[:7]


def _escape(text: str) -> str:
    """最小 markdown 转义：防 subject 注入代码段 / 链接 / HTML 标签。"""
    out = text.replace("\\", "\\\\")
    for ch in _ESCAPE_CHARS:
        out = out.replace(ch, "\\" + ch)
    return out


def _commit_ref(sha: str, repo: str | None) -> str:
    """单条提交的引用：有 repo 时是显式短链，否则是纯 code span。"""
    short = short_sha(sha)
    if not repo:
        return f"`{short}`"
    return f"[`{short}`](https://github.com/{repo}/commit/{sha})"


def _render_line(commit: Commit, repo: str | None) -> str:
    """渲染单条提交行，保持 `- <type>(<scope>): <subject> (<ref>)` 形态。"""
    parsed = parse_conventional(commit.subject)
    if parsed is None:
        body = _escape(commit.subject)
    else:
        head = parsed.type if parsed.scope is None else f"{parsed.type}({parsed.scope})"
        # `!` 与 footer 声明等效：任一出现即标 breaking，两者并存也只标一次。
        if parsed.breaking or has_breaking_footer(commit.body):
            head += "!"
        body = f"{_escape(head)}: {_escape(parsed.subject)}"
    return f"- {body} ({_commit_ref(commit.sha, repo)})"


def _count_line(total: int, prev_tag: str | None, repo: str | None) -> str:
    """标题下方的提交计数行；有前一个 tag 时带上它的 release 链接。"""
    if prev_tag is None:
        return f"共 {total} 个提交。"
    if repo:
        ref = f"[`{prev_tag}`](https://github.com/{repo}/releases/tag/{prev_tag})"
    else:
        ref = f"`{prev_tag}`"
    return f"自 {ref} 以来共 {total} 个提交。"


def _compare_line(version: str, prev_tag: str, repo: str | None) -> str:
    """尾部「完整变更日志」compare 链接；无 repo 时降级为纯文本 code span。"""
    spec = f"{prev_tag}...{version}"
    if repo:
        return f"**完整变更日志**: [`{spec}`](https://github.com/{repo}/compare/{spec})"
    return f"**完整变更日志**: `{spec}`"


def render_notes(
    commits: list[Commit],
    *,
    version: str,
    prev_tag: str | None,
    date: str,
    repo: str | None = None,
) -> str:
    """拼装 markdown body（纯字符串操作，不碰 git / 文件系统）。

    `version` 是当前 tag（如 `v0.7.0`），`date` 是 tag 指向 commit 的
    committer 时区下的日历日（`git log --format=%cs`），不是 UTC；取 commit
    记录的日期而非 CI 当天，保证重跑 workflow 时 body 可复现。
    release commit 在此剔除；组序固定、空组整段省略；组内保持 git log 序。
    """
    kept = [c for c in commits if not is_release_commit(c.subject)]

    groups: dict[str, list[str]] = {key: [] for key, _ in GROUP_ORDER}
    for commit in kept:
        parsed = parse_conventional(commit.subject)
        groups[classify(parsed.type if parsed else None)].append(
            _render_line(commit, repo)
        )

    lines = [f"## sag {version} ({date})", ""]
    if prev_tag is None:
        lines += [FIRST_RELEASE_HINT, ""]

    if not kept:
        lines.append(EMPTY_HINT)
    else:
        lines.append(_count_line(len(kept), prev_tag, repo))
        for key, title in GROUP_ORDER:
            entries = groups[key]
            if not entries:
                continue
            lines += ["", f"### {title}", *entries]

    if prev_tag is not None:
        lines += ["", "---", "", _compare_line(version, prev_tag, repo)]

    return "\n".join(lines) + "\n"


# ── git 边界层（subprocess 薄封装，不做逻辑）──────────────────────────


class GitError(RuntimeError):
    """git 命令非零退出；调用方决定 fail-closed 还是降级。"""


def git(*args: str, cwd: Path | None = None) -> str:
    """执行只读 git 命令并返回 stdout；非零退出抛 GitError。"""
    cmd = ["git"]
    if cwd is not None:
        cmd += ["-C", str(cwd)]
    proc = subprocess.run(
        [*cmd, *args],
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        check=False,
    )
    if proc.returncode != 0:
        detail = proc.stderr.strip() or proc.stdout.strip() or f"rc={proc.returncode}"
        raise GitError(f"git {' '.join(args)} 失败: {detail}")
    return proc.stdout


def is_shallow_repo() -> bool:
    """是否浅克隆；浅克隆里 tag 集与历史都不完整，tag 发现不可信。

    `rev-parse --is-shallow-repository` 在无法判定时（GitError）按非浅克隆处理，
    让调用方走原有的降级路径。
    """
    try:
        return git("rev-parse", "--is-shallow-repository").strip() == "true"
    except GitError:
        return False


def list_version_tags(current_tag: str | None = None) -> list[str]:
    """列出当前 tag 祖先链上的 `v*` tag；`current_tag` 为 None 时列全部 `v*`。"""
    args = ["tag", "--list", "v*"]
    if current_tag:
        args = ["tag", "--merged", current_tag, "--list", "v*"]
    return [line.strip() for line in git(*args).splitlines() if line.strip()]


def log_range(range_spec: str) -> str:
    """按 range 取 `<sha><US><subject><US><body><RS>` 记录流（git log 输出序 = 新→旧）。

    body 也要取：`BREAKING CHANGE:` footer 只出现在 body 里，只读 `%s` 首行会漏掉
    body-only 的 breaking 变更。
    """
    return git("log", "--format=%H%x1f%s%x1f%b%x1e", range_spec)


def commit_date(tag: str) -> str:
    """取 tag 指向 commit 在 committer 时区下的日历日（`%cs`，非 UTC）。"""
    return git("log", "-1", "--format=%cs", tag).strip()


# ── CLI ──────────────────────────────────────────────────────────────


def fail(message: str) -> NoReturn:
    """fail-closed：GitHub annotation + 退出码 1（sys.exit(str) 写 stderr）。"""
    sys.exit(f"::error:: {message}")


def warn(message: str) -> None:
    """降级路径的告警：走 stderr，不污染 stdout 的 markdown body。"""
    print(f"::warning:: {message}", file=sys.stderr)


def _resolve_range(
    tag: str,
    range_spec: str | None,
    prev_tag: str | None,
) -> tuple[str, str | None]:
    """确定 git log 范围与用于渲染的 prev_tag，返回 (range_spec, prev_tag)。"""
    if range_spec is not None:
        spec = range_spec.strip()
        if not spec:
            fail("--range 不能为空（示例: v0.6.0..v0.7.0）")
        # `--range` 完全接管范围：静默丢弃 `--prev-tag` 会让正文自相矛盾（计数行 /
        # compare 链接指向 prev-tag，条目却来自 --range），误导读者，故 fail-closed。
        if prev_tag is not None:
            fail("--range 与 --prev-tag 不能同时使用（--range 已完全接管范围）")
        return spec, None

    if prev_tag is not None:
        if TAG_RE.match(prev_tag) is None:
            fail(f"--prev-tag 格式非法: {prev_tag}（应为 vX.Y.Z）")
        # 方向校验：两者都已通过 TAG_RE，_version_key 必不为 None。prev >= tag
        # 会让范围恒为空（`v0.2.0..v0.2.0`）或反向，静默产出空 changelog，故 fail-closed。
        prev_key = _version_key(prev_tag)
        cur_key = _version_key(tag)
        if prev_key is not None and cur_key is not None and prev_key >= cur_key:
            fail(f"--prev-tag 必须小于 --tag: {prev_tag} >= {tag}")
        return f"{prev_tag}..{tag}", prev_tag

    try:
        tags = list_version_tags(tag)
    except GitError as exc:
        fail(f"无法列出 tag: {exc}")

    discovered = select_prev_tag(tags, tag)
    if discovered is None:
        if is_shallow_repo():
            fail(
                f"仓库是浅克隆（祖先链上仅可见 {len(tags)} 个 tag），无法可靠确定 {tag} 的"
                "前一个版本 tag；请在 checkout 上设置 fetch-depth: 0，或显式传 "
                "--prev-tag/--range"
            )
        warn(f"未找到 {tag} 的前一个版本 tag，以全部可见历史生成")
        return tag, None
    return f"{discovered}..{tag}", discovered


def main() -> int:
    # stderr 也要加固：非 UTF-8 locale 下其编码可能是 ascii，直写中文告警/错误会抛
    # UnicodeEncodeError，把真正的失败原因埋进 traceback。可行则强制 utf-8，不可行
    # （如 StringIO / 不支持 reconfigure 的重定向）则静默退回原编码。
    try:
        sys.stderr.reconfigure(encoding="utf-8", errors="replace")
    except (AttributeError, OSError, ValueError):
        pass

    parser = argparse.ArgumentParser(
        description="生成 GitHub Release 说明（markdown body）",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "示例:\n"
            "  python3 scripts/release_notes.py --tag v0.7.0 --output release-notes.md \\\n"
            "      --repo rainbowatcher/sag\n"
            "  python3 scripts/release_notes.py --tag v0.7.0 --range v0.6.0..v0.7.0\n"
        ),
    )
    parser.add_argument(
        "--tag",
        help="当前 tag，必须匹配 ^v\\d+\\.\\d+\\.\\d+$（默认: $GITHUB_REF_NAME）",
    )
    parser.add_argument(
        "--prev-tag",
        metavar="TAG",
        help="显式指定前一个 tag，跳过自动发现（范围变成 <prev-tag>..<tag>）",
    )
    parser.add_argument(
        "--range",
        metavar="A..B",
        help="显式 git log 范围，完全接管 tag 发现（本地预览/兜底用；与 --prev-tag 互斥）",
    )
    parser.add_argument(
        "--output",
        default="-",
        metavar="PATH",
        help="markdown 输出路径，默认 '-'（stdout）；CI 传 release-notes.md",
    )
    parser.add_argument(
        "--repo",
        metavar="OWNER/NAME",
        help="commit 链接前缀（默认: $GITHUB_REPOSITORY；两者皆无则输出纯文本）",
    )
    parser.add_argument(
        "--date",
        metavar="YYYY-MM-DD",
        help=(
            "发布日期（默认: <tag> 指向 commit 在 committer 时区下的日历日，"
            "即 git log 的 %%cs 字段；非 UTC）"
        ),
    )
    args = parser.parse_args()

    tag = (args.tag or os.environ.get("GITHUB_REF_NAME") or "").strip()
    if not tag:
        fail("无法确定 tag：请传 --tag 或设置 GITHUB_REF_NAME")
    if TAG_RE.match(tag) is None:
        fail(f"tag 格式非法: {tag}（应为 vX.Y.Z）")

    repo = (args.repo or os.environ.get("GITHUB_REPOSITORY") or "").strip() or None
    if repo:
        repo = repo.strip("/")
        if "/" not in repo:
            fail(f"--repo 格式非法: {repo}（应为 OWNER/NAME）")

    range_spec, prev_tag = _resolve_range(tag, args.range, args.prev_tag)

    if args.date is not None:
        try:
            datetime.strptime(args.date.strip(), "%Y-%m-%d")
        except ValueError:
            fail(f"--date 格式非法: {args.date}（应为 YYYY-MM-DD）")
        date = args.date.strip()
    else:
        try:
            date = commit_date(tag)
        except GitError as exc:
            fail(f"无法解析 tag {tag}: {exc}")
        if not date:
            fail(f"无法取到 tag {tag} 的提交日期")

    try:
        raw = log_range(range_spec)
    except GitError as exc:
        fail(f"无法解析范围 {range_spec}: {exc}")

    commits = parse_commits(raw)
    body = render_notes(commits, version=tag, prev_tag=prev_tag, date=date, repo=repo)

    if args.output in (None, "", "-"):
        # 非 UTF-8 locale 下 stdout 编码可能是 ascii，直写中文 body 会抛
        # UnicodeEncodeError；可行则强制 utf-8，不可行（如已重定向的管道
        # 不支持 reconfigure）时退回原编码并替换不可编码字符。
        try:
            sys.stdout.reconfigure(encoding="utf-8", errors="replace")
        except (AttributeError, OSError, ValueError):
            pass
        sys.stdout.write(body)
        return 0

    out_path = Path(args.output)
    try:
        out_path.write_text(body, encoding="utf-8")
    except OSError as exc:
        fail(f"无法写入 {out_path}: {exc}")
    print(f"[INFO] release notes 已写入 {out_path}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
