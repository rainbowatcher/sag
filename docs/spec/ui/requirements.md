---
created: 2026-06-26
updated: 2026-10-23
---

# UI 管理面板

## 目标

提供基于 Vue 3 + TypeScript + Tailwind CSS v4 的 SAG 网关管理面板，涵盖 Dashboard 概览、Provider/Route 配置管理、用量统计与请求日志查询。作为 SAG 的默认前端界面，通过 `rust-embed` 内嵌于 gateway 二进制分发。

## 概述

SAG UI（`sag-ui/`）是一个单页应用，通过 HTTP 调用 SAG 管理 API（`sag-admin/`）获取数据和控制网关。

| 层面 | 技术 |
|------|------|
| 页面路由 | Vue Router，6 个路由：Dashboard（`/`）、Providers（`/providers`）、Routes（`/routes`）、Analytics（`/analytics`）、Logs（`/logs`）、Connect（`/connect`） |
| 状态管理 | Pinia stores：`connection`、`health`、`providers`、`routes`、`analytics` |
| 组件库 | shadcn-vue + Tailwind CSS v4 |
| 图表 | Chart.js via vue-chartjs |
| 构建 | Vite 6，产物通过 `rust-embed` 内嵌到 gateway 二进制；`chart.js` / `vue-chartjs` 单独拆为 `vendor-chart` chunk，只随 `/analytics` 路由懒加载（不进首屏 preload） |

## 预期行为

### 连接管理

- 首次访问或未连接时重定向到 `/connect` 页面，输入 gateway URL 后连接
- 连接状态持久化到 localStorage
- 断开时清除连接状态，返回 `/connect`
- 侧边栏底部显示当前连接的 gateway URL

### 健康检查

- 心跳轮询 gateway 版本号和在线状态
- 侧边栏底部 Badge 实时显示连接状态：Connected（绿色）/ Offline（红色）
- 侧边栏顶部显示 gateway 版本号

### 主题切换

- 支持亮色（light）和暗色（dark）主题
- 主题状态持久化到 localStorage
- Chart.js 图表配色随主题自动切换（通过 `useChartTheme` composable）
- 侧边栏底部提供切换按钮

### 多语言支持（i18n）

- 支持英文（en）和中文（zh）两种界面语言，第一版不含第三种语言
- 默认语言为中文（zh）；用户未做过选择时界面以中文呈现
- 语言偏好持久化到 localStorage（键名 `sag-ui-lang`），跨会话、跨刷新保持
- 持久化的语言值必须经白名单校验（仅接受 `en` / `zh`）；遇到脏值（损坏、被篡改、旧格式）时静默回退到默认语言，不得抛错或渲染出空白文案
- 切换入口在 Settings 页的 Language tab（Select：English / 中文）；切换后当前页面所有文案即时翻转，无需刷新
- 文档根元素 `<html lang>` 跟随当前语言同步为 `zh-CN` / `en`
- 界面文案（导航、表头、toast、表单校验消息、对话框、tooltip、空态/错误态）不得以中文字面量硬编码在源文件中，统一收敛到字典；未翻译的 key 回退到英文，仍缺失时回显 key 本身并在开发环境打印警告
- 英文（en）为字典的 canonical 源：key 为稳定英文标识符，中文译文缺失时构建/类型检查即失败

### Dashboard

- 加载时并行获取 providers、routes、stats、health 数据
- 四张统计卡片：Providers 数量、Routes 数量、总调用数、错误数
- Token 趋势折线图，支持 1h / 24h / 7d 时间范围切换和 provider 筛选
- 调用次数趋势折线图，同上筛选条件
- 饼图展示各 provider/model 的调用分布
- 加载中显示 spinner，加载失败显示 Alert 错误提示

### Provider 管理

- 列表页以 TanStack Table 展示：Name、Base URL、Models Endpoint、Type、Keys 数量、Status（开关）、Actions（编辑/删除）
- 筛选：按 model 名称（Combobox 模糊搜索）、provider type（OpenAI / Anthropic）、排序（Name A-Z / 默认）
- 添加：`ProviderAddForm` 组件，根据 provider_type 动态展示不同表单字段
- 编辑：`ProviderEditForm`，视口 ≥768px（`md`）为 Dialog、<768px 为底部抽屉（drawer），由 `useResponsiveModal` 统一提供
- 开关：Switch 组件直接切换 provider 启用/禁用，即时生效
- 删除：二次确认对话框，底部固定栏显示确认信息

### Route 管理

- 列表页以 TanStack Table 展示：Model Name、Endpoint、Strategy（彩色标签）、Targets 数量、Actions
- 标记引用了已禁用 provider 的 target：行变灰 + Targets 列显示琥珀色警告
- 支持列排序（点击表头）
- 编辑：弹框内嵌 `RouteEditForm`，可修改 targets、strategy 等；视口 ≥768px 为 Dialog、<768px 为底部抽屉
- 删除：二次确认弹框，同上双轨形态
- 清除 Active Target / 重置 Fill-First 顺序：点击 Active Target 单元格触发二次确认，同上双轨形态
- Models 列表中的「Create route」快捷建单：确认弹框，也使用同一双轨形态（移动端会出现抽屉套抽屉）

### Analytics

- Stats 面板：按 provider / model / real_model 聚合的调用次数、Token 总量、平均延迟、成功率
- Cache 面板：按相同维度聚合的缓存命中统计
- 顶部统计卡片：Total Calls、Total Tokens、Avg Latency、Success Rate
- 筛选：按 provider 和 model 过滤，支持回车键触发查询
- 加载中 / 空数据 / 错误状态分别处理

### Logs

- 12 列请求日志表格：Time、Provider、Model、Status、Prompt Tokens、Completion Tokens、Reasoning、Cached、Hit Rate、Total、Duration、Mode
- 筛选：provider、model、real_model（文本输入）+ status_code（数字输入）
- Status Badge 颜色：5xx → destructive、4xx → secondary、2xx → default
- 错误详情通过 Tooltip 展示
- Duration 列显示上游耗时、TTFT、tokens/sec 子行
- Model 列在 `real_model ≠ model` 时显示子行
- 分页：Prev / Next 按钮，显示当前页码和行数
- 客户端对 status_code 输入做 NaN 校验，避免向后端发送无效值

### 固定高度布局与容器内滚动

页面整体固定为视口高度（`h-screen`），侧边栏和主内容区均不整体滚动。仅数据容器（表格、图表区域）内部滚动，页面头部、筛选栏、统计卡片保持固定可见。

**根布局**：

- `<main>` 从 `overflow-y-auto` 改为 `overflow-hidden`，主内容区不再整体滚动
- 各页面需自行管理固定区域和滚动区域的分割

**各页面滚动策略**：

| 页面 | 固定区域（不滚动） | 滚动区域 |
|------|-------------------|---------|
| Dashboard | 页面标题 + 4 张统计卡片 + 时间范围切换 + provider 筛选 | 图表网格（饼图、Token 趋势、调用次数趋势） |
| Providers | 页面标题 + Add/Refresh 按钮 + 筛选栏（Model、Type、Sort） | Provider 表格 |
| Routes | 页面标题 + Add Route 按钮 | Route 表格 |
| Analytics | 页面标题 + Tabs 标签栏 + 4 张统计卡片 + 筛选栏 | Stats 表格 / Cache 表格 |
| Logs | 页面标题 + 筛选栏（provider、model、real_model、status） | 日志表格 |
| Connect | 无滚动（表单适配视口） | — |

**Logs 分页栏**：分页控件固定在表格下方可见，不作为滚动内容的一部分。分页栏属于固定区域，表格为滚动区域。

**加载/空/错误状态**：加载中 spinner、空数据提示、错误 Alert 与表格处于同一滚动容器内，居中显示。

**约束**：

- 侧边栏导航区已有 `flex-1`，可自然利用剩余高度；底部连接状态栏固定可见
- 不使用 JavaScript 计算高度——所有高度分配通过 Tailwind 的 `flex flex-col h-full` + `flex-1` + `shrink-0` 完成
- 表格外层的 `overflow-x-auto` 配合 `overflow-y-auto` 实现双向独立滚动（横向按需，纵向由容器控制）

### 大屏幕响应式优化

- **内容宽度**：`max-w-6xl`（1152px）→ `xl:max-w-7xl`（1280px）→ `2xl:max-w-[90rem]`（1440px）
- **内边距**：`p-6` → `xl:p-8` → `2xl:p-10`
- **侧边栏宽度**：`w-60`（240px）→ `xl:w-64`（256px）→ `2xl:w-72`（288px）
- **Dashboard 图表网格**：`lg:grid-cols-2` → `xl:grid-cols-3`，调用次数图移除 `lg:col-span-2`
- **饼图尺寸**：`max-w-xs` → `xl:max-w-sm`
- **Dashboard 统计卡片**：数值 `xl:text-2xl`，间距 `xl:gap-5`
- **LogsPage 表格**：`xl:` 下 Time / Provider / Model 列加宽；`overflow-x-auto` + `2xl:overflow-visible`
- **ProviderList 表格**：`xl:` 下 Base URL / Models Endpoint 列从 `min-w-[200px]` 扩展为 `xl:min-w-[280px]`
- **RouteList 表格**：`xl:` 下 Targets 列扩展，`2xl:` 下策略标签展示更多细节
- **筛选输入框**：LogsPage `w-40 xl:w-52`、AnalyticsPage `w-44 xl:w-56`、Dashboard `w-40 xl:w-52`
- **Analytics 统计卡片**：数值 `xl:text-xl`，间距 `xl:gap-4`
- 仅新增 `xl:`（1280px）和 `2xl:`（1536px）断点，`lg:` 及以下行为不变
- 所有变更均为纯 Tailwind CSS 工具类，无 JavaScript 媒体查询

### 移动端弹框形态

- 断点：`md`（768px），与 `AppLayout.vue` 的 `isMobile` 和 `SidebarProvider` 内部断点一致——整站只保留一个「移动端」定义
- 视口 ≥768px：弹框使用 Dialog（居中卡片）；<768px：使用底部抽屉（vaul Drawer，`direction=bottom`，可下拉关闭）
- 形态切换由 `useResponsiveModal()` 统一提供，组件不得自行写 `useMediaQuery` + 条件组件映射
- 属于「弹框」并适用该规则：Provider 新增/编辑、Route 新增/编辑/删除确认、清除 Active Target 与重置 Fill-First 确认、Models 列表的快捷建单确认
- 不属于「弹框」：Popover 类浮层（`Combobox`、`SearchSelect`、`DateRangePicker`）、Tooltip、移动端侧边栏（已是 Sheet）、`ProviderList` 的行内删除确认条
- 布局约定：抽屉内没有默认内边距（`DrawerContent` 无 padding），正文区自行加 `px-4 sm:px-0`——对话框一侧已有 `DialogContent` 的 `p-6`，`sm:px-0` 避免叠加成双重内边距

## 配置

UI 本身无独立配置文件。构建时通过 Vite 环境变量注入 API base URL，运行时管理 API 地址由用户手动输入并存储在 localStorage。

## 功能边界

### 包含

- 6 个页面路由（Dashboard、Providers、Routes、Analytics、Logs）+ Connect 入口
- Pinia 状态管理（connection、health、providers、routes、analytics）
- shadcn-vue 组件库集成
- Chart.js 图表（折线图、饼图）
- 亮/暗主题切换
- 多语言支持（en / zh）：语言偏好可切换、可持久化，文案统一收敛到字典
- 与 SAG 管理 API 的 HTTP 通信
- 固定高度布局：页面整体固定为视口高度，仅表格和图表容器内部滚动，页面头部和筛选栏保持可见
- 大屏幕（`xl:`、`2xl:` 断点）响应式优化
- 移动端（<768px）弹框以底部抽屉呈现，桌面端保持 Dialog

### 不包含

- 服务端渲染（SSR）
- 第三种及更多语言（第一版仅 en / zh）
- ICU MessageFormat 复数/性别等复杂插值、日期/数字的 locale 感知格式化（第一版仅 `{param}` 命名插值）
- 端到端测试
- 离线支持（PWA）
- 图表 / 数据导出
- `3xl:` 及以上超宽屏断点（1440px 最大内容宽度已考虑行长可读性）

## 与其它模块的交互

- **管理 API**（`sag-admin/`）：UI 通过 HTTP REST 调用所有 CRUD 端点（providers、routes）和查询端点（stats、logs、cache、health）
- **Gateway 二进制**：UI 产物通过 `rust-embed` 内嵌，单一二进制分发，无需额外静态文件服务
- **配置**（`sag-config/`）：管理 API 修改配置后触发热重载，UI 的列表/详情在刷新同步
- **分析**（`sag-analytics/`）：Analytics 和 Logs 页面从 SQLite 读取聚合统计数据
- **Tailwind CSS v4**：提供响应式断点和工具类系统
- **TanStack Table**：ProviderList 和 RouteList 使用 TanStack Table 进行列定义、排序和渲染

## 变更记录

- [0005 构建侧收尾：`vendor-chart` 分包与删除 favicon 死引用](changes/0005-build-chunk-splitting-and-favicon.md)
- [0001 初始化](changes/0001-initial.md)
- [0002 表格固定表头与玻璃拟态效果](changes/0002-sticky-header.md)
- [0004 UI 多语言支持（i18n，第一版 en/zh）](changes/0004-ui-i18n-language-switching.md)
- [0003 移动端弹框统一为底部抽屉](changes/0003-responsive-modal-drawer.md)
- [0005 构建侧收尾：`vendor-chart` 分包与删除 favicon 死引用](changes/0005-build-chunk-splitting-and-favicon.md)
