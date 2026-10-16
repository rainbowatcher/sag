---
created: 2026-09-17
updated: 2026-10-16
type: 修改
---

# 0002 — 版本升级提交按实际变更文件过滤与 workspace 版本继承

## 动机

### 提交文件按实际变更过滤

`scripts/release.py` 会同步本地 `Cargo.lock`，但仓库通过 `.gitignore` 忽略该文件。脚本此前无条件把 `Cargo.lock` 传给 `git commit`，导致 Git 报 `pathspec 'Cargo.lock' did not match any file(s) known to git`，即使其它版本文件已经正确更新也无法完成自动提交。

参考 `bumpp` 的行为，提交文件列表应来自实际发生修改的文件，并且只应包含 Git 可识别的路径。

### workspace 版本单一源

此前每个成员 crate 的 `Cargo.toml` 各自维护 `version = "x.y.z"`，`release.py` 递归改写所有 `Cargo.toml`。版本号分散在 9 个成员文件里，任何一次漏改都会造成版本漂移。将版本收敛到根 `Cargo.toml` 的 `[workspace.package] version`，成员通过 `version.workspace = true` 继承后，只有一处需要维护。

## 需求变更

- `architecture/requirements.md`「版本升级脚本」新增自动提交文件过滤规则：实际变更且已跟踪的文件才传给 `git commit`；忽略或未跟踪文件可同步到工作区，但不能导致提交失败。
- `architecture/requirements.md`「版本升级脚本」新增版本单一源约束：唯一版本源为根 `Cargo.toml` 的 `[workspace.package] version`，成员 `Cargo.toml` 由脚本继承更新，`Cargo.lock` 成员条目从根版本推导。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`scripts/release.py`](../../../../scripts/release.py) | Cargo.lock 的 dry-run 与实际同步都按真实替换计数；自动提交前通过 `git ls-files --cached` 过滤未跟踪/被忽略路径；Rust 升级只改写根 `Cargo.toml` 的 `[workspace.package] version`，成员 `Cargo.lock` 条目按各成员 `[package] name` 从根版本 (旧, 新) 推导同步 |
| [`Cargo.toml`](../../../../Cargo.toml) | `[workspace.package]` 新增 `version = "0.4.1"`，成为唯一版本源 |
| 9 个成员 `Cargo.toml`（`sag-core` … `sag-gateway`） | `version = "0.4.1"` 改为 `version.workspace = true`，继承 workspace 版本 |
| [`.github/workflows/release.yml`](../../../../.github/workflows/release.yml) | 两个 job 的版本校验从根 `Cargo.toml` 的 `[workspace.package] version` 读取，维持单一版本源 |
| [`docs/spec/architecture/requirements.md`](../requirements.md) | 记录版本单一源与固定提交文件集 |
| [`docs/spec/architecture/spec.md`](../spec.md) | 记录 workspace 版本继承的实现、Cargo.lock 推导与 `bumpp` `updatedFiles` 对齐行为 |

### 升级写入并提交的文件集

一次 `major|minor|patch` 升级只写入并提交以下四类文件：

1. 根 `Cargo.toml`（`[workspace.package] version`）
2. `Cargo.lock`（所有 workspace 成员条目，按根版本 (旧, 新) 推导）
3. `sag-ui/package.json`
4. `docs/package.json`

成员 crate 的 `Cargo.toml` 不再被写入或提交（继承根版本）。

## 测试

- `python3 -m py_compile scripts/release.py`
- 在临时 Git 仓库中验证：被 `.gitignore` 忽略的 `Cargo.lock` 会被本地同步，但不会传给 `git commit`，版本提交成功。
- `python3 scripts/release.py patch --dry-run` 验证当前仓库不会把未跟踪的 `Cargo.lock` 计入提交文件。
- 临时 Git 仓库集成测试（根 `Cargo.toml` `[workspace.package] version` + 一个 `version.workspace = true` 成员 + 已跟踪 `Cargo.lock` + 两个 `package.json`）：`patch` 后根、Cargo.lock 成员条目与两个 `package.json` 同步升级，外部依赖条目不动，生成单个 `chore: release vX.Y.Z` commit 与 tag 并打印 push 提示；`--dry-run` / `--no-commit` 不写文件或不打 tag；本地/远端已存在目标 tag 时在改动任何文件前中止。
