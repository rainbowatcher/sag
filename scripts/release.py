#!/usr/bin/env python3
"""release.py — 升级 Rust workspace + npm 项目版本号，提交并打 tag。

用法:
  python3 scripts/release.py major|minor|patch [--dry-run] [--no-commit] [--npm PATH ...]

升级对象:
  - Rust: 根 `Cargo.toml` 的 `[workspace.package] version` 是唯一版本源，
    各成员 crate 通过 `version.workspace = true` 继承，脚本不再改写成员
    `Cargo.toml`。随后同步 `Cargo.lock` 中所有 workspace 成员条目的
    version（从根版本 (旧, 新) 推导）；若 Cargo.lock 被 .gitignore 忽略，
    仍更新本地文件但不会把它传给 git commit。
  - npm: 默认 sag-ui/package.json 与 docs/package.json 的顶层 "version"，
    可通过 --npm 追加/替换。

语义: 按 semver 递增指定段并清零低位段，丢弃 pre-release / build 后缀。

提交与打 tag: 升级完成后自动执行 `git commit -m "chore: release vX.Y.Z"`，
只提交本次实际改动且已被 Git 跟踪的文件（不推送、不碰其他工作区变更）；
未跟踪或被忽略的文件仅保留工作区更新，不传给 git commit。
提交成功后，在该 release commit 上创建轻量 tag `vX.Y.Z`（不推送），
并打印可复制执行的 push 提示。若目标 tag 已存在于本地或 origin，
则在修改任何文件前报错退出。--dry-run / --no-commit 跳过提交与打 tag，
只打印相应提示。脚本永不 push。不传升级段则打印 help。
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

# 递归时跳过的目录（相对路径的任一段命中即跳过）
SKIP_DIRS = {
    ".git",
    ".kilo",
    ".mimocode",
    ".vitepress",
    "node_modules",
    "target",
    "__pycache__",
}

# 默认升级的 npm 项目文件（相对 --root）
DEFAULT_NPM_FILES = ["sag-ui/package.json", "docs/package.json"]

VERSION_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)(?:[-+].*)?$")
SECTION_RE = re.compile(r"^\s*\[([^\]]+)\]\s*$")
CARGO_VERSION_LINE_RE = re.compile(r'^(version\s*=\s*)"([^"]+)"')
CARGO_NAME_LINE_RE = re.compile(r'^(name\s*=\s*)"([^"]+)"')
# 允许任意缩进，但必须按值确认是顶层 version（见 bump_npm）
NPM_VERSION_LINE_RE = re.compile(r'^(\s*)"version"\s*:\s*"([^"]+)"')
LOCK_NAME_RE = re.compile(r'^name\s*=\s*"([^"]+)"')
LOCK_VERSION_LINE_RE = re.compile(r'^(version\s*=\s*)"([^"]+)"')

# 允许在其中升级 version 的 Cargo 段
CARGO_VERSION_SECTIONS = {"package", "workspace.package"}


class Version:
    def __init__(self, major: int, minor: int, patch: int):
        self.major = major
        self.minor = minor
        self.patch = patch

    def __str__(self) -> str:
        return f"{self.major}.{self.minor}.{self.patch}"

    def bump(self, kind: str) -> "Version":
        if kind == "major":
            return Version(self.major + 1, 0, 0)
        if kind == "minor":
            return Version(self.major, self.minor + 1, 0)
        return Version(self.major, self.minor, self.patch + 1)


def parse_version(text: str) -> Version | None:
    m = VERSION_RE.match(text.strip())
    if not m:
        return None
    return Version(int(m.group(1)), int(m.group(2)), int(m.group(3)))


def find_cargo_tomls(root: Path) -> list[Path]:
    files = []
    for path in root.rglob("Cargo.toml"):
        if any(part in SKIP_DIRS for part in path.relative_to(root).parts):
            continue
        files.append(path)
    return sorted(files)


def read_cargo_version(path: Path) -> str | None:
    """只读读取 [package] / [workspace.package] 段的 version；无则返回 None。"""
    lines = path.read_text(encoding="utf-8").splitlines()
    section = None
    for line in lines:
        m = SECTION_RE.match(line)
        if m:
            section = m.group(1)
            continue
        if section not in CARGO_VERSION_SECTIONS:
            continue
        m = CARGO_VERSION_LINE_RE.match(line)
        if m and parse_version(m.group(2)):
            return m.group(2)
    return None


def cargo_package_name(path: Path) -> str | None:
    """只读读取 Cargo.toml [package] 段的 name；无则返回 None。"""
    lines = path.read_text(encoding="utf-8").splitlines()
    section = None
    for line in lines:
        m = SECTION_RE.match(line)
        if m:
            section = m.group(1)
            continue
        if section != "package":
            continue
        m = CARGO_NAME_LINE_RE.match(line)
        if m:
            return m.group(2)
    return None


def bump_cargo_toml(path: Path, kind: str) -> tuple[str, str] | None:
    """升级单个 Cargo.toml，返回 (旧版本, 新版本)；无 version 可升返回 None。"""
    lines = path.read_text(encoding="utf-8").splitlines(keepends=True)
    section = None
    changed = False
    for i, line in enumerate(lines):
        m = SECTION_RE.match(line)
        if m:
            section = m.group(1)
            continue
        if section not in CARGO_VERSION_SECTIONS:
            continue
        m = CARGO_VERSION_LINE_RE.match(line)
        if not m:
            continue
        old = m.group(2)
        v = parse_version(old)
        if v is None:
            continue
        new = str(v.bump(kind))
        lines[i] = line.replace(f'"{old}"', f'"{new}"', 1)
        changed = True
        old_result = old
        new_result = new
        section = None  # 一个段内只处理第一个 version 行
    if not changed:
        return None
    path.write_text("".join(lines), encoding="utf-8")
    return (old_result, new_result)


def sync_cargo_lock(
    lock_path: Path,
    version_map: dict[str, tuple[str, str]],
    *,
    write: bool = True,
) -> int:
    """按 crate 名同步 Cargo.lock，返回实际修改的 crate 数。"""
    if not version_map or not lock_path.exists():
        return 0
    lines = lock_path.read_text(encoding="utf-8").splitlines(keepends=True)
    name = None
    updated = set()
    for i, line in enumerate(lines):
        m = LOCK_NAME_RE.match(line)
        if m:
            name = m.group(1)
            continue
        if name in version_map:
            m = LOCK_VERSION_LINE_RE.match(line)
            if m and m.group(2) == version_map[name][0]:
                lines[i] = line.replace(
                    f'"{version_map[name][0]}"', f'"{version_map[name][1]}"', 1
                )
                updated.add(name)
            name = None  # version 紧跟 name 之后，处理完即复位
    if updated and write:
        lock_path.write_text("".join(lines), encoding="utf-8")
    return len(updated)


def git_tracked_files(root: Path, paths: list[str]) -> list[str]:
    """返回 paths 中已被 Git 跟踪的文件，保留传入顺序。"""
    if not paths:
        return []
    proc = subprocess.run(
        ["git", "-C", str(root), "ls-files", "--cached", "--", *paths],
        capture_output=True,
        text=True,
        check=False,
    )
    tracked = set(proc.stdout.splitlines()) if proc.returncode == 0 else set()
    return [path for path in paths if path in tracked]


def tag_exists_local(root: Path, tag: str) -> bool:
    """检查本地是否已存在同名 tag。"""
    proc = subprocess.run(
        ["git", "-C", str(root), "rev-parse", "-q", "--verify", f"refs/tags/{tag}"],
        capture_output=True,
        text=True,
        check=False,
    )
    return proc.returncode == 0


def tag_exists_remote(root: Path, tag: str) -> bool:
    """检查 origin 是否已有同名 tag；查询失败（网络/远端不可用）时直接中止。"""
    proc = subprocess.run(
        ["git", "-C", str(root), "ls-remote", "--tags", "origin", tag],
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        sys.exit(
            f"error: 无法查询 origin 上的 tag {tag}"
            f"（git ls-remote rc={proc.returncode}）: {proc.stderr.strip()}"
        )
    return bool(proc.stdout.strip())


def current_branch(root: Path) -> str:
    """返回当前分支名；detached HEAD 时回退为 HEAD。"""
    proc = subprocess.run(
        ["git", "-C", str(root), "rev-parse", "--abbrev-ref", "HEAD"],
        capture_output=True,
        text=True,
        check=False,
    )
    return proc.stdout.strip() or "HEAD"


def read_target_version(root: Path, npm_files: list[Path], kind: str) -> str | None:
    """只读扫描版本源，返回本次升级后的目标版本号；无可升级版本返回 None。

    用于在修改任何文件前做 tag 冲突检查，因此不写入、不提交。
    """
    for toml in find_cargo_tomls(root):
        old = read_cargo_version(toml)
        if old is None:
            continue
        v = parse_version(old)
        if v:
            return str(v.bump(kind))
    for p in npm_files:
        if not p.exists():
            continue
        try:
            top_version = json.loads(p.read_text(encoding="utf-8")).get("version")
        except (json.JSONDecodeError, UnicodeDecodeError):
            continue
        if isinstance(top_version, str):
            v = parse_version(top_version)
            if v:
                return str(v.bump(kind))
    return None


def bump_npm(path: Path, kind: str) -> tuple[str, str] | None:
    """升级单个 package.json 的顶层 version，返回 (旧, 新)；无 version 返回 None。"""
    # 先用 json 解析出确切的顶层 version，避免误改嵌套的 "version" key
    #（如 scripts 里名为 version 的脚本、dependencies 里的同名包）
    try:
        top_version = json.loads(path.read_text(encoding="utf-8")).get("version")
    except (json.JSONDecodeError, UnicodeDecodeError):
        return None
    if not isinstance(top_version, str):
        return None
    v = parse_version(top_version)
    if v is None:
        return None
    new = str(v.bump(kind))
    lines = path.read_text(encoding="utf-8").splitlines(keepends=True)
    for i, line in enumerate(lines):
        m = NPM_VERSION_LINE_RE.match(line)
        # 只替换值等于顶层 version 的那一行
        if m and m.group(2) == top_version:
            lines[i] = line.replace(f'"{m.group(2)}"', f'"{new}"', 1)
            path.write_text("".join(lines), encoding="utf-8")
            return (top_version, new)
    return None


def main() -> int:
    parser = argparse.ArgumentParser(
        description="升级 Rust workspace + npm 项目版本号",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "示例:\n"
            "  python3 scripts/release.py patch      # workspace + npm 打 patch\n"
            "  python3 scripts/release.py minor --dry-run\n"
            "  python3 scripts/release.py major --npm sag-ui/package.json\n"
        ),
    )
    parser.add_argument(
        "kind",
        nargs="?",
        choices=("major", "minor", "patch"),
        help="升级方式: major / minor / patch（必传；不传则打印本帮助）",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="只打印将发生的变更，不写文件、不提交",
    )
    parser.add_argument(
        "--no-commit",
        action="store_true",
        help="升级后跳过自动 git commit（默认会提交 chore: release vX.Y.Z）",
    )
    parser.add_argument(
        "--npm",
        nargs="+",
        metavar="PATH",
        help="要升级的 package.json（相对 --root），默认: "
        + ", ".join(DEFAULT_NPM_FILES),
    )
    parser.add_argument(
        "--root",
        default=REPO_ROOT,
        type=Path,
        help=f"项目根目录（默认: {REPO_ROOT}）",
    )
    args = parser.parse_args()

    if args.kind is None:
        parser.print_help()
        return 0

    root = args.root.resolve()
    if not root.is_dir():
        print(f"[ERR] root 不是目录: {root}", file=sys.stderr)
        return 1

    npm_files = [root / p for p in (args.npm or DEFAULT_NPM_FILES)]

    # ── tag 冲突预检（必须在修改任何文件之前）──────────────────────
    target_version = read_target_version(root, npm_files, args.kind)
    if target_version is not None:
        target_tag = f"v{target_version}"
        if tag_exists_local(root, target_tag):
            print(
                f"[ERR] 本地已存在 tag {target_tag}，已中止（未修改任何文件）。",
                file=sys.stderr,
            )
            return 1
        if tag_exists_remote(root, target_tag):
            print(
                f"[ERR] origin 已存在 tag {target_tag}，已中止（未修改任何文件）。",
                file=sys.stderr,
            )
            return 1

    version_map: dict[str, tuple[str, str]] = {}
    changes = []

    # ── Rust ─────────────────────────────────────────────────────────
    # 版本唯一源是根 Cargo.toml 的 [workspace.package] version；成员 crate
    # 通过 version.workspace = true 继承，脚本不再改写它们的 Cargo.toml。
    root_toml = root / "Cargo.toml"
    root_bump: tuple[str, str] | None = None
    if root_toml.exists():
        if args.dry_run:
            # dry-run 只读：不写文件
            old = read_cargo_version(root_toml)
            if old:
                root_bump = (old, str(parse_version(old).bump(args.kind)))
        else:
            root_bump = bump_cargo_toml(root_toml, args.kind)
        if root_bump:
            changes.append((root_toml.relative_to(root), root_bump[0], root_bump[1]))

    # 成员 Cargo.toml 已无显式 version，Cargo.lock 里的成员条目需按根版本
    # 变化同步：用各成员 [package] name 建立 name -> (旧, 新) 映射。
    if root_bump:
        for toml in find_cargo_tomls(root):
            if toml == root_toml:
                continue
            name = cargo_package_name(toml)
            if name:
                version_map.setdefault(name, root_bump)

    # ── Cargo.lock ──────────────────────────────────────────────────
    lock = root / "Cargo.lock"
    # 与 bumpp 的 updateFiles() 一样，只把实际发生替换的文件视为 changed。
    # dry-run 也复用同一套检测逻辑，避免 Cargo.lock 不存在或已经同步时误报。
    lock_changed = sync_cargo_lock(lock, version_map, write=not args.dry_run)

    # ── npm ─────────────────────────────────────────────────────────
    for p in npm_files:
        if not p.exists():
            print(f"[WARN] 不存在，跳过: {p.relative_to(root)}", file=sys.stderr)
            continue
        if args.dry_run:
            try:
                top_version = json.loads(p.read_text(encoding="utf-8")).get("version")
            except (json.JSONDecodeError, UnicodeDecodeError):
                top_version = None
            if isinstance(top_version, str):
                v = parse_version(top_version)
                if v:
                    changes.append((p.relative_to(root), top_version, str(v.bump(args.kind))))
        else:
            result = bump_npm(p, args.kind)
            if result:
                changes.append((p.relative_to(root), result[0], result[1]))

    # ── 汇总 ────────────────────────────────────────────────────────
    if not changes and not lock_changed:
        print("未发现可升级的版本号。")
        return 0

    verb = "将升级" if args.dry_run else "已升级"
    print(f"{verb} {args.kind}:")
    for rel, old, new in changes:
        print(f"  {rel}: {old} -> {new}")
    if lock_changed:
        lock_rel = lock.relative_to(root)
        print(f"  {lock_rel}: 同步 {lock_changed} 个 crate 的 version")

    # ── 自动提交 ────────────────────────────────────────────────────
    release_version = changes[0][2]
    tag = f"v{release_version}"
    message = f"chore: release v{release_version}"
    changed_files = [str(rel) for rel, _, _ in changes]
    if lock_changed:
        changed_files.append(str(lock.relative_to(root)))

    # bumpp 只将 updateFile() 确认实际修改过的文件传给 git commit；
    # 这里再过滤 Git 未跟踪文件，避免被 .gitignore 忽略的 Cargo.lock
    # 触发 "pathspec ... did not match any file(s) known to git"。
    commit_files = git_tracked_files(root, changed_files)
    skipped_files = [path for path in changed_files if path not in commit_files]
    if skipped_files:
        print(
            "[INFO] 以下文件已更新但未被 Git 跟踪，跳过自动提交: "
            + " ".join(skipped_files)
        )

    if args.dry_run:
        print(f"将提交: {message}（{len(commit_files)} 个文件）")
        print(f"[INFO] --dry-run 指定，跳过 tag 创建: {tag}")
        return 0
    if args.no_commit:
        print(f"[INFO] --no-commit 指定，跳过自动提交: {message}")
        print(f"[INFO] --no-commit 指定，跳过 tag 创建: {tag}")
        return 0
    if not commit_files:
        print("[INFO] 没有可提交的已跟踪变更，跳过自动提交。")
        print(f"[INFO] 未创建 release commit，跳过 tag 创建: {tag}")
        return 0

    print(f"提交: {message}")
    print(f"  文件: {' '.join(commit_files)}")
    # `git commit -- <paths>` 只用 worktree 内容提交这些路径，
    # 不触碰其他暂存/未暂存变更；也不会推送。
    proc = subprocess.run(
        ["git", "-C", str(root), "commit", "-m", message, "--", *commit_files],
        capture_output=True,
        text=True,
    )
    if proc.stdout:
        print(proc.stdout.rstrip())
    if proc.returncode != 0:
        print(f"[ERR] git commit 失败:\n{proc.stderr.strip()}", file=sys.stderr)
        return 1

    # ── 打轻量 tag ──────────────────────────────────────────────────
    print(f"打 tag: {tag}")
    proc = subprocess.run(
        ["git", "-C", str(root), "tag", tag],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        print(f"[ERR] git tag 失败:\n{proc.stderr.strip()}", file=sys.stderr)
        return 1

    # 脚本永不 push，只打印可复制执行的推送提示。
    branch = current_branch(root)
    print(f"Release v{release_version} is ready. Push with:")
    print(f"  git push origin {branch} {tag}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
