---
created: 2026-06-26
updated: 2026-10-29
---

# UI 管理面板

## 目标

提供基于 Vue 3 + TypeScript + Tailwind CSS v4 的 SAG 网关管理面板，涵盖 Dashboard 概览、Provider/Route 配置管理、用量统计与请求日志查询。作为 SAG 的默认前端界面，通过 `rust-embed` 内嵌于 gateway 二进制分发。

## 概述

SAG UI（`sag-ui/`）是一个单页应用，通过 HTTP 调用 SAG 管理 API（`sag-admin/`）获取数据和控制网关。

| 层面 | 技术 |
|------|------|
| 页面路由 | Vue Router，6 个路由：Dashboard（`/`）、Providers（`/providers`）、Routes（`/routes`）、Analytics（`/analytics`）、Logs（`/logs`）、Connect（`/connect`） |
| 状态管理 | Pinia stores：`connection`、`health`、`providers`、`routes`、`analytics`、`sessionAffinity`、`inflight` |
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

- 列表页以 TanStack Table 展示：Name、Base URL、Models Endpoint、Type、Keys 数量、Actions（编辑/删除）；启用状态由 Actions 列的 Switch 直接切换，不单独占用 Status 列
- 筛选：按 model 名称（Combobox 模糊搜索）、provider type（OpenAI / Anthropic）、排序（Name A-Z / 默认）
- 添加：`ProviderAddForm` 组件，根据 provider_type 动态展示不同表单字段
- 编辑：`ProviderEditForm`，视口 ≥768px（`md`）为 Dialog、<768px 为底部抽屉（drawer），由 `useResponsiveModal` 统一提供
- 开关：Switch 组件直接切换 provider 启用/禁用，即时生效
- 删除：二次确认对话框，底部固定栏显示确认信息

### Route 管理

- 列表页以 TanStack Table 展示：Model Name、Endpoint、Strategy（彩色标签）、Targets 数量、Actions
- 新增 target：浮窗批量选择（`TargetPickerDialog`）——按 provider 分组的模型列表 + 搜索（同时匹配 provider 名与模型名）+ 多选；选择单位是 provider+model 对（同一模型在两个 provider 是两个选项），已有 target 预勾选，取消勾选即删除该行，新勾选追加为新行并带默认值（priority/retries 为 0），未动过的行保留原有 priority/retries/extra；确认按钮预览差异（Add N / Remove N / Add A remove R），无差异时仅关闭
- 浮窗选项列表：被禁用 provider 的组带 Disabled 标记；provider 已被删除（或模型列表加载失败）但被现有 target 引用时，补一个尾部组并以 orphan 标记展示，这些行不可勾选但也不会消失；provider 模型列表加载失败按 provider 汇总为一条错误提示，不影响其他组
- 浮窗键盘操作：↑/↓ 移动高亮、Enter/空格切换勾选（IME 组合输入时不响应）、Esc 关闭且不应用；过滤后高亮行消失时重锚到首个可见项，`aria-activedescendant` 不指向未渲染的行
- 新建路由表单不预置空白 target 行（空白行会以「target provider required」挡住提交）
- 策略选择含四个值：sticky-failover、fill-first、round-robin、session-sticky；选中 session-sticky 时展开「会话 TTL（秒，整数 ≥ 0，`0` = 永不过期）」与「每请求模型会话容量（整数 ≥ 1）」两个数字输入，留空 = 不发送该字段、使用后端默认值；两个输入仅在 session-sticky 下显示，也只有在该策略下参与校验（切回其它策略后残留输入不再挡住提交）
- 会话参数（`session_ttl_secs` / `session_max_entries`）的 update 语义为「省略 = 保留」，编辑表单因此不提供清空交互（清空输入框等价于省略，保持当前值）
- 跨字段组合校验留给后端：通配路由（`model_name = "*"`）× session-sticky、session-sticky + `pipeline = "image"` 两类组合由 `validate_routes` 拒绝，表单顶部错误条直接展示 400 消息原文（前端不复述、不翻译）（**此口径已由 0009 部分反案**：这两类组合改为表单前置校验，见下文「表单前置校验」条目；其它组合规则仍由后端拒绝）
- 标记引用了已禁用 provider 的 target：行变灰 + Targets 列显示琥珀色警告
- 支持列排序（点击表头）
- 编辑：弹框内嵌 `RouteEditForm`，可修改 targets、strategy 等；视口 ≥768px 为 Dialog、<768px 为底部抽屉
- 删除：二次确认弹框，同上双轨形态
- 清除 Active Target / 重置 Fill-First 顺序：点击 Active Target 单元格触发二次确认，同上双轨形态
- 会话 pin 观测：`strategy = session_sticky` 的路由在 Active Target 单元格的 target 按钮右侧另挂一个图标按钮（`KeyRound`），点击打开会话 pin 浮层；其它策略不渲染该按钮，该路由没有 active target 时按钮仍然存在（**此口径已由 0012 修改**：session-sticky 行没有 active target 时单元格**只渲染钥匙 icon，不再渲染 `-` 占位**；其它策略该列仍为 `-`，列语义仍是「当前目标」，只是不再为 session-sticky 行补一个无信息的占位）
- 会话 pin 浮层按该行的 `model_name` 拉 `GET /api/session-affinity?model=<model>`：头部显示模型空间名与 pin 条数，每条列出 `provider/real_model`、会话 key 前 8 位（`title` 保留全量 32 位）、来源 `source`、存活时长（`age_secs`）、剩余有效期（`expires_in_secs`；`session_ttl_secs = 0` 即 `expires_in_secs = null` 时显示「永不过期」）；加载中 / 无 pin / 请求失败各有提示（loading 与错误均按该行 model 作用域取值，他行的请求在飞或失败不会让本行转圈、也不会把错误文案渲染到本行），列表区限高滚动
- 会话 pin 清除有两条路径：逐条清除走 `DELETE /api/session-affinity {"key": …}`；「清空此模型全部」为两步确认（第一步按钮替换为「清空 N 条？」+ Cancel/清空），确认后走 `DELETE {"model": …}`。两条路径成功后均 toast（逐条显示 key 前 8 位，按模型显示 `deleted` 条数）、重新拉取该模型空间的 pin，并回调列表重拉 active targets（pin 被清后下一跳目标会变）；失败 toast 展示后端错误消息
- 路由表格上方常显一行实时活动徽章（数据来自 `GET /api/inflight` 的 SSE 流）：左侧 `◉ 处理中 N`（N = 快照 `total`）+ 标题内联 Info Tooltip，右侧为各活跃 route 的徽章（`model_name` 等宽字体、超长截断、Tooltip 显示全量，后跟 `×count`，带一个脉冲绿点）；**仅列出 `count > 0` 的 route**，按 `count` 降序
- 快照确认全零（`routes` 为空且 `total === 0`）时显示「空闲 — 无正在处理的请求」占位；首帧到达前整行不渲染（没有快照时不声称 0）
- 实时流的断开态：保留最后一份快照并标 `stale`——徽章置灰、绿点变灰、右侧显示「连接断开，重连中…」（Tooltip 说明计数可能过期），**计数不清零**；恢复后自动回到实时态。401/403 走共享 `handle401()` 并停止重连（不留在「重连中」无限重试）
- 会话 pin hover 预览（0012）：钥匙 icon **hover 即出只读 Tooltip**（`side="left"`），内容与浮层同源——标题 + pin 条数 + 加载中 / 无 pin / 请求失败三态 + pin 列表（`provider/real_model`、key 前 8 位、`source`、剩余 TTL）；预览里**没有任何清除入口**，破坏性动作只在点击后的浮层里；浮层打开时不显示 tooltip（受控 `:open`，不再用 `disabled`），两者不同时出现；预览只对鼠标（`pointerType === "touch"` 被忽略）与键盘聚焦（仅 `:focus-visible`）生效，且即时出现（跳过 reka 的 delay）；指针移进 tooltip 内容即关闭（受控后无 grace area，长列表请用浮层）
- 会话 pin 排序与置灰（0012）：Tooltip 与浮层共用同一份排序，按存活性分组——活跃 pin（`expires_in_secs === null` 即 `session_ttl_secs = 0`、或 `> 0`）全部置顶，过期残留（`<= 0`）排在后面并置灰（Tooltip 行 `opacity-70`、浮层行 `text-muted-foreground opacity-60`）；组内保持后端返回的相对顺序（stable sort）。剩余有效期文案新增第四态：`<= 0` 显示「已过期」（复用既有 key `common.expired`，不新增 i18n key）
- 会话 pin 预览的拉取节流与在途去重（0012）：hover / 键盘聚焦触发的拉取需**同时**满足两个条件——距上次拉取完成满 `HOVER_REFRESH_MS`（3000ms）**且**本行（该 `model` 作用域）没有在途请求；任一不满足即跳过，因此指针扫过一整列路由不会一行发一个请求，慢请求未结算时反复 hover / 聚焦也不会排入重复请求。判定按作用域：他行的在途请求不影响本行（不得拿整份 loading 映射当全局闸）。点击打开浮层时**总是重新拉取**（不走这两道闸），保证看到的是当下状态
- 表单前置校验（0008 的「组合规则全留给后端」就此两类反案）：通配路由（`model_name = "*"`）× session-sticky、session-sticky × `pipeline = "image"` 由表单对象级 refine 直接拒绝，规则与后端 `sag-config` 的 `validate_routes` 等价；错误分别挂在 `strategy` 与 `pipeline` 字段上就地展示（新建表单的通配判定取 `model_name` 或首个 target 的 `real_model`，编辑表单取路由自身的 `model_name`），不再等提交后看顶部错误条。其余组合规则（容量为 `0` 等）仍只由后端拒绝
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
- Session 列位于 Client 列之后：显示 `session_key` 前 8 位（`title` 保留全量 32 位哈希），该行没有会话记录时显示 `—`
- 点击 Session 单元格把完整 `session_key` 写入会话筛选并立即重查——与筛选栏内的会话过滤控件、清除按钮共用同一状态（`logsFilterSession`），点击后控件内联显示同一 key，清除即回到无会话筛选；会话过滤控件与其它过滤器同宽同高，值以截断文本（`title` 保留全量）内联显示在控件内，不额外占用筛选栏下方空间
- 列数口径：本节上文的「12 列」是 0001 时的列口径，此后 Client 列与本列均在其后追加，现共 15 列
- 会话信息卡：应用会话过滤（`logsFilterSession` 非空）时，在筛选栏与表格之间渲染卡片（无过滤时不渲染）：会话键（前 8 位 + 全量 `title`）、来源徽标（`meta` / `top-level` / `prompt cache` / `content hash`）、会话 ID（`font-mono` 截断 + 全量 `title`）、首条消息预览；首次 / 最近出现时间放在来源徽标的 tooltip 内
- 会话信息卡的降级形态：当前会话键在 `session_registry` 无记录（旧数据、已过保留期，或注册表查询失败）时卡片仍渲染，但只剩会话键一格 + 降级文案；卡片只在「记录的过滤键 === 当前过滤键」时展示记录，切换过滤时不会把上一段对话的信息挂在新哈希下

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
- 属于「弹框」并适用该规则：Provider 新增/编辑、Route 新增/编辑/删除确认、清除 Active Target 与重置 Fill-First 确认、Models 列表的快捷建单确认、Route target 选择浮窗
- 不属于「弹框」：Popover 类浮层（`Combobox`、`SearchSelect`、`DateRangePicker`）、Tooltip、移动端侧边栏（已是 Sheet）、`ProviderList` 的行内删除确认条
- 布局约定：抽屉内没有默认内边距（`DrawerContent` 无 padding），正文区自行加 `px-4 sm:px-0`——对话框一侧已有 `DialogContent` 的 `p-6`，`sm:px-0` 避免叠加成双重内边距

### 隐私过滤设置

**入口与构成**

- Settings 页新增「隐私过滤」tab（`?tab=privacy`），位置在「API 鉴权」之后、「重试退避」之前；侧边栏设置子菜单按同一顺序自动带出该入口，不新增顶级导航项，也不新增页面路由
- 该 tab 只在已认证连接下可读写；鉴权失败沿用既有 401 处理
- 一个 tab 承载全部隐私设置：基础设置（总开关 / 处置模式 / 响应还原）、内置检测规则、自定义规则与例外、高级（盐值），以及只读的保护范围说明
- 保护范围的只读说明精简为四条：仅处理聊天接口的请求文本（`/v1/chat/completions`、`/v1/messages`、`/v1/responses`）；不扫描图片、附件与 URL；只还原本次请求产生的占位符，不检测模型新写出的敏感内容；本地会话预览仍可能含原文（日志只记录命中类别与数量）
- 不提供规则在线试跑、命中统计/审计报表或演示开关

**当前生效配置与草稿**

- 顶部固定展示两张只读摘要卡：「当前生效配置」= 最近一次从服务器确认的配置快照，「草稿」= 尚未保存的编辑内容；两卡各自列出启用状态、处置模式、内置检测器（`n / 7`）、响应还原、盐值状态
- 草稿卡标明是否同步：一致时提示已与已保存配置同步，有编辑时为「存在未保存的更改」
- 首次成功读取后草稿与当前生效配置一致，不产生未保存状态；此后任何编辑只改草稿，摘要卡随草稿即时更新
- 切换模式、勾选检测器、改词表 / 正则、输入新盐值都不会自动提交

**基础设置**

- 「启用隐私过滤」开关初始值取自服务器（新配置默认关闭）；关闭时其余各项仍可编辑，保存后生效
- 关闭时明确提示：当前处于关闭状态，可先预配置模式与规则，保存并开启后才会作用于请求
- 所有开关与输入都需要显式保存才生效，不即时写服务器（与 Provider 启停的即时生效不同）
- 处置模式为三选一卡片，说明必须准确：
  - 「脱敏后发送」（`mask`，默认，标注推荐）：命中项替换为占位符后再发往上游，例如 `[[EMAIL_1a2b3c4d5e6f]]`，上游看不到原文
  - 「阻止请求」（`block`）：任一命中即拒绝本次请求、返回网关错误、不向上游发送任何内容；页面提示被拒绝的请求不会到达上游，客户端会收到网关错误
  - 「仅告警」（`warn`）：仅记录命中类别统计，请求照常转发；必须显式警示**「原文仍会发送给上游」**，并说明日志只记录命中类别与数量（如 `email=1`），不记录命中原文，也不记录占位符
- 切换模式保留响应还原设置（不因切模式重置）
- 「响应还原」开关只对 `mask` 生效：当前为 `block` / `warn` 时开关保留原值，并提示「响应还原仅在该模式下生效，当前设置为开启 / 关闭，切回后继续沿用」
- 响应还原只承诺「把本次请求生成的占位符替换回原文」，不检测模型新生成的敏感内容

**内置检测规则**

- 七个内置检测器各一个独立开关，与处置模式共同决定处置结果：邮箱 `email`、中国大陆手机号 `phone`、中国居民身份证 `id_card_cn`、银行卡号 `bank_card`、IBAN 国际账号 `iban`、常见 API 密钥 `api_key`、PEM 私钥 `private_key`
- 卡片头部显示已启用数量（「已选 n / 7」）

**自定义规则与例外**

- 该区块默认可折叠，为可选内容，不展开不影响既有配置
- 自定义敏感词：每行一项，空行忽略；命中按当前处置模式处理
- 放行名单：每行一项；对候选命中的完整值做**大小写敏感**的整段精确匹配，不做子串、前缀或单词边界匹配
- 自定义正则：可增删条目，每条包含类别标识、表达式与「忽略大小写」（默认关闭）；类别标识允许重复
- 正则的客户端校验只判必填：类别标识或表达式为空 / 全空白时就地报错并阻止提交，不判断正则语法；合法性最终由网关按 Rust regex 方言编译校验（不支持前后行断言与反向引用），保存被拒时错误指向具体条目（含条目序号与后端消息原文）
- 正则表达式按输入原样提交，首尾空白只在必填判断时忽略；空列表显示空态提示，不预置空行
- 未编辑的词表不会被改写；编辑后按行拆分，去掉每行首尾空白并忽略空行，其余条目与顺序按草稿保留

**高级（盐值）**

- 只显示「已配置 / 未配置」状态，任何界面与响应都不回显盐值内容，也不提供查看、复制入口
- 「新盐值」为遮蔽输入：留空表示保持不变（不提交该字段）；填写任意非空内容即按所填字面值替换，不去空白、不做规范化。草稿摘要显示「待替换」
- 保存成功后输入框清空、状态转为已配置；撤销清空输入；保存失败保留已输入内容以便重试
- 不提供「清空盐值」控件，保存也不会因留空而静默清空既有盐值

**保存、撤销与失败**

- 底部固定操作区只有「保存更改」与「撤销」；保存期间按钮显示「保存中…」，全部编辑控件与撤销被锁定（重复点击不会二次提交），并提示保存中不可编辑
- 撤销把整份草稿回退到当前生效配置（含清空新盐值输入），未保存状态消失
- 保存成功：以服务器返回的配置替换「当前生效配置」并重建草稿（无未保存状态），toast 提示已保存
- 保存失败：草稿与已输入的新盐值原样保留，「当前生效配置」不更新，可修改后重试；toast 以「保存未确认：<后端错误消息>」呈现，后端消息原文直出（不复述、不翻译），并提示草稿已保留、需要时可重新加载以确认网关实际配置
- 失败文案不承诺「服务器一定未变更」：网络中断、响应丢失或提交后的副作用失败时，界面只表态「保存未确认 / 失败」，展示最后一次确认的配置，不自动重试、不自动丢弃草稿，也不由前端宣称服务器仍是旧配置
- 客户端必填校验未通过时不发起保存

**读取与不可用**

- 打开该 tab 时读取一次当前配置；读取中显示加载提示
- 读取失败（网络、401、5xx）显示错误提示与重试入口，不渲染可编辑表单，也不把各项呈现为「关闭 / 默认值」；重试由用户显式触发
- 后端快照不含隐私字段（旧版本网关）时显示不可用提示（未返回隐私设置，请重新加载或升级网关），同样**不降级**为一份关闭状态的表单
- 401 走既有统一鉴权处理

**沿用既有外壳**

- 英文与中文均完整覆盖本页文案，切换语言即时生效；不新增第三种语言
- 亮 / 暗主题与窄屏（<768px）布局收缩沿用现有机制，本页不出现横向滚动；不引入新主题、侧栏或导航外壳
- 不做路由离开拦截：离开该页不弹确认，未保存草稿不写入 localStorage、URL 或日志；在 Settings 内切换到其它 tab 再切回时草稿仍在，只有离开 `/settings` 路由或刷新页面才丢弃未保存草稿，且不留恢复入口

## 配置

UI 本身无独立配置文件。构建时通过 Vite 环境变量注入 API base URL，运行时管理 API 地址由用户手动输入并存储在 localStorage。

## 功能边界

### 包含

- 6 个页面路由（Dashboard、Providers、Routes、Analytics、Logs）+ Connect 入口
- Pinia 状态管理（connection、health、providers、routes、analytics、sessionAffinity、inflight）
- shadcn-vue 组件库集成
- Chart.js 图表（折线图、饼图）
- 亮/暗主题切换
- 多语言支持（en / zh）：语言偏好可切换、可持久化，文案统一收敛到字典
- 与 SAG 管理 API 的 HTTP 通信（含会话亲和观测/清除：`GET|DELETE /api/session-affinity`）
- 实时在途计数：路由页经 `GET /api/inflight` 的 SSE 长连接展示各路由在途请求数（只读，不写任何状态）
- 固定高度布局：页面整体固定为视口高度，仅表格和图表容器内部滚动，页面头部和筛选栏保持可见
- 大屏幕（`xl:`、`2xl:` 断点）响应式优化
- 移动端（<768px）弹框以底部抽屉呈现，桌面端保持 Dialog
- Settings 页「隐私过滤」tab：经管理 API 读写隐私配置（总开关、三种处置模式、响应还原、7 个内置检测器、自定义敏感词 / 放行名单 / 自定义正则、盐值状态与替换）
- 隐私设置的显式保存：当前生效配置与草稿双摘要、撤销、保存中锁定编辑、失败保留草稿

### 不包含

- 服务端渲染（SSR）
- 第三种及更多语言（第一版仅 en / zh）
- ICU MessageFormat 复数/性别等复杂插值、日期/数字的 locale 感知格式化（第一版仅 `{param}` 命名插值）
- 端到端测试
- 离线支持（PWA）
- 图表 / 数据导出
- `3xl:` 及以上超宽屏断点（1440px 最大内容宽度已考虑行长可读性）
- 隐私规则的在线试跑、命中统计 / 审计报表、演示开关
- 盐值的回显、查看、复制与「清空盐值」控件（只支持留空保持或按字面替换）
- 客户端正则语法校验（只判必填，最终由网关按 Rust regex 方言编译校验）
- 未保存隐私草稿的跨会话持久化与路由离开拦截
- 隐私设置的顶级导航入口、独立页面路由或专属主题（它只是 Settings 内的一个 tab）

## 与其它模块的交互

- **管理 API**（`sag-admin/`）：UI 通过 HTTP REST 调用所有 CRUD 端点（providers、routes）和查询端点（stats、logs、cache、health），以及配置读写端点（`GET|PUT /api/config`，含运行时参数与隐私设置）
- **Gateway 二进制**：UI 产物通过 `rust-embed` 内嵌，单一二进制分发，无需额外静态文件服务
- **配置**（`sag-config/`）：管理 API 修改配置后触发热重载，UI 的列表/详情在刷新同步
- **分析**（`sag-analytics/`）：Analytics 和 Logs 页面从 SQLite 读取聚合统计数据
- **Tailwind CSS v4**：提供响应式断点和工具类系统
- **TanStack Table**：ProviderList 和 RouteList 使用 TanStack Table 进行列定义、排序和渲染
- **隐私过滤**（`sag-privacy/` + 配置 / 管理 API）：隐私 tab 经 `GET /api/config` 读取、`PUT /api/config` 提交整份隐私设置；保存为单次原子配置更新，成功后下一个请求即按新配置生效，无需重启；读取侧只有「盐值是否已配置」布尔，绝不含盐值内容

## 变更记录

- [0014 设置页隐私过滤配置](changes/0014-privacy-settings.md)
- [0013 路由页实时在途请求徽章行（SSE）](changes/0013-routes-live-inflight-badges.md)
- [0012 会话 pin 图标 hover 预览与活跃 pin 置顶/置灰](changes/0012-session-pin-tooltip-and-active-pin-ordering.md)
- [0011 日志页会话信息卡](changes/0011-logs-session-info-card.md)
- [0010 日志页会话键过滤项内联显示](changes/0010-logs-session-filter-inline-control.md)
- [0009 UI 对接 session-affinity 观测与会话列/校验提示](changes/0009-session-affinity-pins-and-log-session-column.md)
- [0008 UI 补齐 `session_sticky` 路由策略](changes/0008-session-sticky-route-strategy.md)
- [0007 Provider 列表删除 Status 列](changes/0007-provider-list-drop-status-column.md)
- [0006 路由 Target 改为浮窗搜索多选](changes/0006-target-picker-dialog.md)
- [0005 构建侧收尾：`vendor-chart` 分包与删除 favicon 死引用](changes/0005-build-chunk-splitting-and-favicon.md)
- [0001 初始化](changes/0001-initial.md)
- [0002 表格固定表头与玻璃拟态效果](changes/0002-sticky-header.md)
- [0004 UI 多语言支持（i18n，第一版 en/zh）](changes/0004-ui-i18n-language-switching.md)
- [0003 移动端弹框统一为底部抽屉](changes/0003-responsive-modal-drawer.md)
