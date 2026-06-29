---
created: 2026-06-26
updated: 2026-06-26
type: 新增
---

# 0001 — UI 管理面板初始化与大屏幕响应式优化

## 动机

SAG UI 此前无技术规格文档。本次同时完成两件事：(1) 建立完整的 UI 需求文档和技术规格文档；(2) 实施大屏幕响应式优化——SAG UI 当前内容宽度限制在 `max-w-6xl`（1152px），在 1440p+ 和超宽屏幕上留有大量空白。

## 需求变更

- requirements.md 全部初始定义（连接管理、健康检查、主题切换、Dashboard、Provider/Routes 管理、Analytics、Logs）
- requirements.md「大屏幕响应式优化」节：新增 `xl:` 和 `2xl:` 断点下内容宽度、侧边栏宽度、图表网格、表格列宽、筛选输入框、统计卡片的响应式行为定义

## 代码变更

| 文件 | 变更 |
|------|------|
| [sag-ui/src/components/AppLayout.vue](../../../../sag-ui/src/components/AppLayout.vue) | 侧边栏 `w-60 xl:w-64 2xl:w-72`；内容区 `max-w-6xl xl:max-w-7xl 2xl:max-w-[90rem]` + 自适应内边距 `p-6 xl:p-8 2xl:p-10` |
| [sag-ui/src/pages/DashboardPage.vue](../../../../sag-ui/src/pages/DashboardPage.vue) | 图表网格 `xl:grid-cols-3`；调用次数图 `lg:col-span-2 xl:col-span-1`；饼图 `xl:max-w-sm`；统计卡片值 `xl:text-2xl`、间距 `xl:gap-5`；筛选框 `xl:w-52` |
| [sag-ui/src/pages/LogsPage.vue](../../../../sag-ui/src/pages/LogsPage.vue) | 筛选框 `w-40 xl:w-52`；表格包装 `overflow-x-auto 2xl:overflow-visible`；Time/Provider/Model 列 `xl:min-w-[…]` |
| [sag-ui/src/components/providers/ProviderList.vue](../../../../sag-ui/src/components/providers/ProviderList.vue) | Base URL、Models Endpoint 列 meta `min-w-[200px] xl:min-w-[280px]` |
| [sag-ui/src/components/routes/RouteList.vue](../../../../sag-ui/src/components/routes/RouteList.vue) | Strategy 列 `2xl:min-w-[140px]`；Targets 列 `xl:min-w-[100px]` |
| [sag-ui/src/pages/AnalyticsPage.vue](../../../../sag-ui/src/pages/AnalyticsPage.vue) | 4 个筛选框 `xl:w-56`；统计卡片值 `xl:text-xl`、间距 `xl:gap-4` |

## 测试

- `pnpm typecheck`：通过（vue-tsc）
- `pnpm lint`：通过（oxlint，0 warnings 0 errors）
