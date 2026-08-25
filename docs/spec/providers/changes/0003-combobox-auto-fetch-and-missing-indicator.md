---
created: 2026-08-25
updated: 2026-08-25
type: 改进
---

# 0003 — 供应商模型下拉自动拉取与缺失标记

## 动机
在 Provider 编辑/新增弹窗中，过去需要用户手动点击「Refresh Models」按钮才能获取上游模型列表，且 Combobox 在拉取过程中被 `disabled` 导致无法交互。同时，如果当前已配置的模型在上游返回列表中不存在，界面无法直观区分哪些模型有效、哪些模型可能已失效或不存在于上游。

## 需求变更
- Combobox 展开时自动请求 `models` 接口拉取上游模型列表。
- 加载状态下 Combobox 保持可交互并在下拉面板内展示 loading 动画，不禁用组件。
- 标记上游不存在的模型：在已选模型列表（Accordion）、Combobox 多选标签及下拉候选项中高亮标注「Not in upstream」警示。

## 代码变更
| 文件 | 变更 |
|------|------|
| [sag-ui/src/components/Combobox.vue](../../../../sag-ui/src/components/Combobox.vue) | 增加 `open` 事件与 `warning` 选项支持，优化 loading 态（不禁用 trigger，下拉区显示 loading 动画） |
| [sag-ui/src/components/ModelManager.vue](../../../../sag-ui/src/components/ModelManager.vue) | 监听 Combobox 展开自动拉取模型列表，对比上游模型并在 Accordion 与 Combobox 中标记不存在的模型 |
| [sag-ui/src/components/providers/ProviderEditForm.vue](../../../../sag-ui/src/components/providers/ProviderEditForm.vue) | 弹窗打开时重置上游模型缓存 |
| [sag-ui/src/components/providers/ProviderAddForm.vue](../../../../sag-ui/src/components/providers/ProviderAddForm.vue) | 弹窗打开时重置上游模型缓存 |
