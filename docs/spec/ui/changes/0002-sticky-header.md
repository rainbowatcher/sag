---
created: 2026-06-27
updated: 2026-06-27
type: 修改
---

# 0002 — 表格固定表头与玻璃拟态效果

## 动机

LogsPage、RouteList、ProviderList、AnalyticsPage 四个页面的表格在数据超出视口时，表头随内容一起滚动消失，用户无法看到列名。需要实现固定表头（sticky header），同时统一所有表格的滚动机制。

## 需求变更

- requirements.md：无变更（固定表头为实现细节，不涉及需求定义）

## 代码变更

| 文件 | 变更 |
|------|------|
| [sag-ui/src/pages/LogsPage.vue](https://github.com/rainbowatcher/sag/blob/main/sag-ui/src/pages/LogsPage.vue) | 外层 `rounded-lg border` div 移除 `overflow-auto`，添加 `[&>div]:h-full` 将高度传递给 Table 包裹 div；`<TableHeader>` 添加 `bg-background/90 sticky top-0 z-10 backdrop-blur-xs` 玻璃拟态固定表头；loading/empty 状态改为 `flex h-full items-center justify-center` |
| [sag-ui/src/components/routes/RouteList.vue](https://github.com/rainbowatcher/sag/blob/main/sag-ui/src/components/routes/RouteList.vue) | 同上：外层移除 `overflow-auto` + `[&>div]:h-full`；`<TableHeader>` 添加 `bg-background/90 sticky top-0 z-10 backdrop-blur-xs` |
| [sag-ui/src/components/providers/ProviderList.vue](https://github.com/rainbowatcher/sag/blob/main/sag-ui/src/components/providers/ProviderList.vue) | 外层移除 `overflow-hidden` + `[&>div]:h-full`；`<TableHeader>` 添加 `bg-background/90 sticky top-0 z-10 backdrop-blur-xs`；移除 `<TableBody>` 上的 `overflow-auto`（会使 `<tbody>` 变为 `display: block` 破坏列宽同步） |
| [sag-ui/src/pages/AnalyticsPage.vue](https://github.com/rainbowatcher/sag/blob/main/sag-ui/src/pages/AnalyticsPage.vue) | Stats Tab / Cache Tab 双表修复：外层 `min-h-0 flex-1` 移除 `overflow-auto`，`rounded-lg border` 添加 `h-full [&>div]:h-full`，`<TableHeader>` 改为 `bg-background/90 sticky top-0 z-10 backdrop-blur-xs`；loading/empty 状态改为 `flex h-full items-center justify-center` |
| [docs/spec/ui/spec.md](https://github.com/rainbowatcher/sag/blob/main/docs/spec/ui/spec.md) | 更新固定高度布局渲染数据流图（Analytics、Providers、Routes、Logs 节点）；新增 `[&>div]:h-full 高度传递` 不变式；更新 `sticky 表头` 和 `表格滚动容器层级` 不变式 |

## 测试

- `pnpm typecheck`：通过（vue-tsc，无错误）
- `pnpm lint`：通过（oxlint，0 warnings 0 errors）
- 手动验证场景：加载 Logs / Routes / Providers / Analytics 页面，数据超出视口时表头固定、内容在表格容器内滚动、侧边栏始终可交互
