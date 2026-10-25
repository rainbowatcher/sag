---
created: 2026-10-24
updated: 2026-10-25
type: 新增
---

# 0006 — 路由 Target 改为浮窗搜索多选（provider+model 对为选择单位）

## 动机

旧交互是逐行加 target：点「加一行」得到一行空白行，先在行内 Combobox 选 provider，再选
模型。要配 10 个模型就得重复 10 次「加行 → 选 provider → 选模型」，且必须先选中 provider
才能看到该 provider 的模型，跨 provider 挑选时无法比较。目标是一次浏览所有 provider 的
模型、可搜索、一次勾选多个。

## 需求变更

- 「预期行为 → Route 管理」新增条目：新增 target 改为浮窗批量选择（按 provider 分组的
  模型列表 + 搜索 + 多选 + 确认差异预览），已存在的 target 预勾选，取消勾选即删除该行。
- 「预期行为 → 移动端弹框形态」的弹框清单新增「Route target 选择浮窗」——与其它模态
  一致走 `useResponsiveModal`（≥768px Dialog / <768px 底部抽屉）。
- 「预期行为 → Provider 管理 / Route 管理」不再假设 target 行由空白行起步：新建路由表单
  不预置空行。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-ui/src/lib/targetPicker.ts`](../../../../sag-ui/src/lib/targetPicker.ts) | 纯逻辑：`optionKey`（`provider\0model` 集合键）、`buildPickerGroups`（provider 分组 + 禁用标记 + 被 target 引用但已不在模型列表的 orphan 行；**空模型 provider 保留分组**，仅 `options` 为空）、`filterPickerGroups`（provider 名或模型名命中；空 query 保留空分组，有 query 时丢弃——空分组无法命中）、`flattenOptions`（列表/键盘顺序）、`diffSelection` / `applyPickerSelection`（勾选集合 ↔ target 行的差异与合并） |
| [`sag-ui/src/components/routes/TargetPickerDialog.vue`](../../../../sag-ui/src/components/routes/TargetPickerDialog.vue) | 浮窗本体：搜索框 + 选中计数 + 分组勾选列表 + 加载/错误/空态 + 差异化确认按钮；打开时并行加载所有 provider 模型、聚焦搜索框；↑/↓/Enter/空格/Esc 键盘操作（全部在搜索框的 `onKeydown` 上处理，Space 走 `preventDefault` 以免页面/列表滚动，Enter/Space 均切换当前高亮项）与 `aria-activedescendant` 维护；**空模型 provider 渲染分组 + 空态提示**（i18n key `routes.targetPicker.providerNoModels`，提示等 loading 结束才显示，避免被当成「已经没有模型」） |
| [`sag-ui/src/components/routes/TargetEditor.vue`](../../../../sag-ui/src/components/routes/TargetEditor.vue) | `addTarget()` 改为打开浮窗；确认后清空行内配置栏展开态与冻结排序，避免已删行的配置栏/顺序被复活；`onProviderChange` 的 `loadProviderModels` 调用补 `void` |
| [`sag-ui/src/components/routes/RouteAddForm.vue`](../../../../sag-ui/src/components/routes/RouteAddForm.vue) | 去掉预置的空白 target 行（浮窗批量添加，空白行会以「target provider required」挡住提交） |
| [`sag-ui/src/components/routes/RouteEditForm.vue`](../../../../sag-ui/src/components/routes/RouteEditForm.vue) | 补 `void` 前缀，消除浮动 Promise |
| [`sag-ui/src/stores/routes.ts`](../../../../sag-ui/src/stores/routes.ts) | `providerModelsLoading` 改为由在途探测计数 `providerModelsPending` 派生的 `computed`（浮窗并发探测所有 provider 时不会被兄弟请求提前清零）；新增 `providerModelsErrors`（按 provider 记录失败信息，随成功/清缓存移除）；`loadProviderModels` 改为 `async`。注意 `enabled_models` 为空时调用的 `/api/providers/{name}/models` **只回显配置里的 `enabled_models`，不向上游发起 `/models` 探测**，所以空列表是可达的正常状态而非失败（该语义已从注释与浮窗文案上去掉「上游探测」的说法） |
| [`sag-ui/src/lib/i18n/en.ts`](../../../../sag-ui/src/lib/i18n/en.ts) / [`zh.ts`](../../../../sag-ui/src/lib/i18n/zh.ts) | 新增 `routes.targetPicker.*` 文案（标题、描述、搜索、空态、加载、按 provider 汇总的加载失败、选中计数、Add/Remove/Both 确认、orphan、provider 禁用） |

## 测试

- 新增 [`sag-ui/src/lib/targetPicker.test.ts`](../../../../sag-ui/src/lib/targetPicker.test.ts)
  （node test runner，`pnpm -C sag-ui test`）覆盖：对身份（同一模型在两个 provider 是两个
  选项）、分组顺序 / 禁用标记 / orphan 行追加 / **空 provider 保留分组且 `options` 为空**
  （空 query 时分组仍在、有 query 时被丢弃）、预勾选去重与不可表示
  行跳过、搜索命中 provider 名与模型名、flatten 顺序、`diffSelection` 增删与 no-op、重复行
  与不可表示行、`applyPickerSelection` 保留原有 priority/retries/extra 与新行默认值、
  `nextId` 连续递增、清空全部
- 门禁：`just verify-ui`（`fmt-check` + `oxlint` + `vue-tsc` + 前端测试 + i18n 齐平检查）
