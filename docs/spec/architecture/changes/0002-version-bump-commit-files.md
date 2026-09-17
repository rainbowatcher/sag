---
created: 2026-09-17
updated: 2026-09-17
type: 修复
---

# 0002 — 版本升级提交按实际变更文件过滤

## 动机

`scripts/bump-version.py` 会同步本地 `Cargo.lock`，但仓库通过 `.gitignore` 忽略该文件。脚本此前无条件把 `Cargo.lock` 传给 `git commit`，导致 Git 报 `pathspec 'Cargo.lock' did not match any file(s) known to git`，即使其它版本文件已经正确更新也无法完成自动提交。

参考 `bumpp` 的行为，提交文件列表应来自实际发生修改的文件，并且只应包含 Git 可识别的路径。

## 需求变更

- `architecture/requirements.md`「版本升级脚本」新增自动提交文件过滤规则：实际变更且已跟踪的文件才传给 `git commit`；忽略或未跟踪文件可同步到工作区，但不能导致提交失败。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`scripts/bump-version.py`](../../../../scripts/bump-version.py) | 让 Cargo.lock 的 dry-run 与实际同步都按真实替换计数，并在自动提交前通过 `git ls-files --cached` 过滤未跟踪/被忽略路径 |
| [`docs/spec/architecture/requirements.md`](../requirements.md) | 记录版本升级脚本的提交文件约束 |
| [`docs/spec/architecture/spec.md`](../spec.md) | 记录与 `bumpp` `updatedFiles` 对齐的实现和 Cargo.lock 行为 |

## 测试

- `python3 -m py_compile scripts/bump-version.py`
- 在临时 Git 仓库中验证：被 `.gitignore` 忽略的 `Cargo.lock` 会被本地同步，但不会传给 `git commit`，版本提交成功。
- `python3 scripts/bump-version.py patch --dry-run` 验证当前仓库不会把未跟踪的 `Cargo.lock` 计入提交文件。
