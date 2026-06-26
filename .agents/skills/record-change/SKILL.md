---
name: record-change
description: Records a completed feature change as a numbered changefile in docs/spec/<module>/changes/. Use after implementing a new feature, modifying existing behavior, or when the user asks to document a change.
---

# 记录变更

每次功能变更完成后，在 `docs/spec/<module>/changes/` 下创建一个编号变更文件。

## 流程

复制此清单：

```
进度：
- [ ] Step 1: 确定编号
- [ ] Step 2: 写 changefile
- [ ] Step 3: 更新 requirements.md
- [ ] Step 4: 更新 spec.md
```

**Step 1: 确定编号** — 查看 `changes/` 目录下最大编号，+1。用四位数字 `NNNN`。

**Step 2: 写 changefile** — 从 [templates/change.md](templates/change.md) 复制模板填入。关键：
- 「代码变更」表用 GitHub 绝对链接指向文件
- 「需求变更」记录 requirements.md 的具体段落变化
- 「测试」列出新增或修改的测试场景
- 不引用 commit hash 或 PR 号

**Step 3: 更新 requirements.md** — 如果变更影响了行为定义，更新对应节。追加新行为，不删除旧描述（旧描述留在 changefile 中）。

**Step 4: 更新 spec.md** — 如果变更影响了实现（新文件、新决策、新不变式），更新对应节。同时在末尾「变更记录」节追加新条目。

**Step 5: 更新 CHANGELOG.md** — 在 `docs/spec/CHANGELOG.md` 顶部按日期插入新条目。格式：
```markdown
- **<模块>**：[NNNN <标题>](<模块>/changes/NNNN-<slug>.md) — <一句话>
```

全部勾选后，确认 `just build-test` 或 `pnpm -C docs build` 通过。

## 模板

从 [templates/change.md](templates/change.md) 复制填入。

## 约束

- changefile 自包含，不依赖外部平台（不引用 commit / PR）
- 代码变更表每行一个文件 + 一句话变更描述
- 日期用 YYYY-MM-DD 格式
- 文件路径用 GitHub 绝对链接
- 中文书写，术语保留原文
