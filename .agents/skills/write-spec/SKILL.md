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
- [ ] Step 2: 画核心设计
- [ ] Step 3: 找决策点
- [ ] Step 4: 列不变式
- [ ] Step 5: 标测试
- [ ] Step 6: 补变更记录
```

**Step 1: 定位文件** — 找到模块所有关键源文件，按职责分组（如入口、状态管理、页面、组件、工具）。每组一个 `###` 标题。路径用仓库根目录相对路径（如 `sag-ui/src/main.ts`）。

**Step 2: 画核心设计** — 根据模块类型从以下子章节中选取，不强制全部出现。无合适内容时可整节省略。一行文字能说清的不画图。
- **数据流**：请求链路 → `sequenceDiagram`，决策分支 → `flowchart TD`
- **状态机**：状态迁移 → `stateDiagram-v2`
- **布局结构**：UI 模块的组件嵌套或布局分区 → `flowchart TD`
- **类型关系**：核心 trait/struct/enum 关系 → `classDiagram` 或文字描述

**Step 3: 找决策点** — 扫描代码中的分支（`if`/`else`、`match`、trait object），追问「为什么选这个不选那个」。按关注点分组（如状态管理、布局、数据流），每组一个 `###` 标题。每个决策写 1-2 句背景，然后：选择的方案 + 被放弃的方案 + 放弃原因。不写「我们选择了 X」——代码已经选了。

**Step 4: 列不变式** — 提取必须恒真的条件（类型约束、顺序约束、唯一性约束）。按关注点分组，每组一个 `###` 标题。每条不变式必须可直接 grep 验证。

**Step 5: 标测试** — 指出测试位置和覆盖的关键场景。格式根据模块类型调整：Rust 模块用 crate 路径，前端模块用表格。至少列出最重要的 3 个测试用例。

**Step 6: 补变更记录** — 在文档末尾列出该模块所有变更文件，格式：`- [NNNN <标题>](changes/NNNN-<slug>.md)`。

全部勾选后，文件写入 `docs/spec/<module>/spec.md`。

## 模板

从 [templates/spec.md](templates/spec.md) 复制模板填入。

## 约束

- 文件路径用仓库根目录相对路径。
- 关键文件按职责分组，每组一个 `###` 标题。
- 核心设计按模块类型选取子章节，无合适内容时可整节省略。
- 决策和不变式按关注点分组，每组一个 `###` 标题。
- Trade-off 写**被放弃的方案**和**放弃原因**。代码已经做了选择，不要重复。
- 不变式可检查：读完就能 grep 验证。
- 不重复 requirements.md 已有内容。
- 不与需求文档矛盾。若发现矛盾，标注并告知用户。
- 文档末尾必须包含「变更记录」节，列出该模块所有 changefile。
- 中文书写，术语保留原文。
