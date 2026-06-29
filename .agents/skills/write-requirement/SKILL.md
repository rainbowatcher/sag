---
name: write-requirement
description: Writes requirements documents (requirements.md) for SAG project feature modules. Use when adding a new feature, documenting existing behavior, or the user asks about requirements, expected behavior, or scope definition.
---

# 写需求文档

输出位置：`docs/spec/<module>/requirements.md`。

## 流程

复制此清单追踪进度：

```
进度：
- [ ] Step 1: 界定范围
- [ ] Step 2: 通读代码
- [ ] Step 3: 写行为
- [ ] Step 4: 标边界
- [ ] Step 5: 验交互
- [ ] Step 6: 补变更记录
```

**Step 1: 界定范围** — 确认模块名与 `docs/spec/` 下目录一致。如果模块已有需求文档，判断是新建还是补充。

**Step 2: 通读代码** — 读取模块关键源文件。用 `read_files` 和 `code-searcher` 理解实际行为。不猜测。

**Step 3: 写行为** — 按功能域分组，每组一个 `###` 标题。每个功能域内按正常路径 → 边界情况 → 错误路径顺序，用列表描述关键场景的输入、处理、输出。不写实现细节（CSS 类名、DOM 结构、具体算法、文件路径等——这些放 spec.md）。

**Step 4: 标边界** — 列出「包含」和「不包含」。不包含项必须明确——防止 scope creep。

**Step 5: 验交互** — 列出该模块依赖哪些模块、被哪些模块依赖、交互方式。

**Step 6: 补变更记录** — 在文档末尾列出该模块所有变更文件，格式：`- [NNNN <标题>](changes/NNNN-<slug>.md)`。

全部勾选后，文件写入 `docs/spec/<module>/requirements.md`。

## 模板

从 [templates/requirements.md](templates/requirements.md) 复制模板填入。

## 约束

- 描述**实际行为**。代码与预期不符时标注 TODO，不写虚假需求。
- 行为描述可验证：读了就能判断实现是否正确。
- 不写实现细节。实现放 `spec.md`。
- 文档间引用用相对路径（`](/guide/...)`、`](/spec/...)`）。
- 文档末尾必须包含「变更记录」节，列出该模块所有 changefile。
- 中文书写，术语保留原文。

### 一级标题规范

- `requirements.md` 的一级标题（H1）使用纯中文，不带「需求」后缀，例如 `# 管理 API`、`# 配置`、`# 路由`
- 一级标题会被 `generateSpecSidebar()` 自动提取作为 VitePress 侧边栏的模块显示名，因此 H1 必须能独立作为模块标签
- sidebar 不再使用 `MODULE_LABELS` 硬编码映射，标签完全来自文件内容，请确保每个需求文件的 H1 唯一且有明确表义
