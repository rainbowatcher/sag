#!/usr/bin/env python3
"""bump-version.py — 递归升级 Rust + npm 项目文件版本号。

用法:
  python3 scripts/bump-version.py major|minor|patch [--dry-run] [--npm PATH ...]

升级对象:
  - Rust: 递归发现的所有 Cargo.toml（跳过 target/、node_modules/ 等），
    仅升级 [package] 与 [workspace.package] 段内的 `version = "x.y.z"`；
    同步更新 Cargo.lock 中对应 crate 的 version（workspace 成员依赖引用
    不带版本限定符，直接改 block 是安全的）。
  - npm: 默认 sag-ui/package.json 与 docs/package.json 的顶层 "version"，
    可通过 --npm 追加/替换。

语义: 按 semver 递增指定段并清零低位段，丢弃 pre-release / build 后缀。
"""

from __future__ import annotations

import argparse
import json
import re
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


def sync_cargo_lock(lock_path: Path, version_map: dict[str, tuple[str, str]]) -> int:
    """按 crate 名同步 Cargo.lock 中的 version，返回修改的 crate 数。"""
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
    if updated:
        lock_path.write_text("".join(lines), encoding="utf-8")
    return len(updated)


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
        description="递归升级 Rust + npm 项目文件版本号",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "示例:\n"
            "  python3 scripts/bump-version.py patch      # 所有 crate + npm 打 patch\n"
            "  python3 scripts/bump-version.py minor --dry-run\n"
            "  python3 scripts/bump-version.py major --npm sag-ui/package.json\n"
        ),
    )
    parser.add_argument(
        "kind",
        choices=("major", "minor", "patch"),
        help="升级方式: major / minor / patch",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="只打印将发生的变更，不写文件",
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

    root = args.root.resolve()
    if not root.is_dir():
        print(f"[ERR] root 不是目录: {root}", file=sys.stderr)
        return 1

    npm_files = [root / p for p in (args.npm or DEFAULT_NPM_FILES)]
    version_map: dict[str, tuple[str, str]] = {}
    changes = []

    # ── Rust ─────────────────────────────────────────────────────────
    for toml in find_cargo_tomls(root):
        if args.dry_run:
            # dry-run 只读：临时复制逻辑，不写文件
            lines = toml.read_text(encoding="utf-8").splitlines(keepends=True)
            section = None
            result = None
            for line in lines:
                m = SECTION_RE.match(line)
                if m:
                    section = m.group(1)
                    continue
                if section not in CARGO_VERSION_SECTIONS:
                    continue
                m = CARGO_VERSION_LINE_RE.match(line)
                if m:
                    v = parse_version(m.group(2))
                    if v:
                        result = (m.group(2), str(v.bump(args.kind)))
                        break
            if result:
                changes.append((toml.relative_to(root), result[0], result[1]))
                if toml.parent != root:
                    version_map.setdefault(toml.parent.name, result)
        else:
            result = bump_cargo_toml(toml, args.kind)
            if result:
                changes.append((toml.relative_to(root), result[0], result[1]))
                if toml.parent != root:
                    version_map.setdefault(toml.parent.name, result)

    # ── Cargo.lock ──────────────────────────────────────────────────
    lock = root / "Cargo.lock"
    lock_changed = 0
    if not args.dry_run:
        lock_changed = sync_cargo_lock(lock, version_map)
    else:
        lock_changed = len(version_map)

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
    return 0


if __name__ == "__main__":
    sys.exit(main())
