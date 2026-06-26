---
name: write-spec
description: Writes technical specification documents (spec.md) for SAG project feature modules. Covers key files, data flow diagrams, design trade-offs, invariants, and test strategies. Use after requirements.md exists, or when documenting implementation details and design decisions.
---

# 写规格文档

输出位置：`docs/spec/<module>/spec.md`。需求定义「做什么」，规格定义「怎么做」。

## 流程

复制此清单追踪进度：

```
进度：
- [ ] Step 1: 定位文件
- [ ] Step 2: 画数据流
- [ ] Step 3: 找决策点
- [ ] Step 4: 列不变式
- [ ] Step 5: 标测试
```

**Step 1: 定位文件** — 找到模块所有关键源文件。路径用 GitHub 绝对链接（`https://github.com/rainbowatcher/sag/blob/main/...`），agent 可直接 `read_url` 打开。

**Step 2: 画数据流** — 选一种 Mermaid 图描述核心流程。选型规则：请求链路 → `sequenceDiagram`，决策分支 → `flowchart TD`，状态迁移 → `stateDiagram-v2`。一行文字能说清的不画图。

**Step 3: 找决策点** — 扫描代码中的分支（`if`/`else`、`match`、trait object），追问「为什么选这个不选那个」。每个决策写：选择的方案 + 被放弃的方案 + 放弃原因。不写「我们选择了 X」——代码已经选了。

**Step 4: 列不变式** — 提取必须恒真的条件（类型约束、顺序约束、唯一性约束）。每条不变式必须可直接 grep 验证。

**Step 5: 标测试** — 指出测试位置和覆盖的关键场景。至少列出最重要的 3 个测试用例。

全部勾选后，文件写入 `docs/spec/<module>/spec.md`。

## 模板

从 [templates/spec.md](templates/spec.md) 复制模板填入。

## 约束

- 文件路径用 GitHub 绝对链接。
- Trade-off 写**被放弃的方案**和**放弃原因**。代码已经做了选择，不要重复。
- 不变式可检查：读完就能 grep 验证。
- 不重复 requirements.md 已有内容。
- 不与需求文档矛盾。若发现矛盾，标注并告知用户。
- 中文书写，术语保留原文。
