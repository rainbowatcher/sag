---
created: 2026-10-15
updated: 2026-10-15
type: 新增
---

# 0004 — UI 多语言支持（i18n，第一版 en/zh）

## 动机

`sag-ui` 此前是纯中文界面：所有文案（导航、表头、toast、表单校验、对话框、分析页、日志页）都以中文字面量硬编码在 `.vue` / `.ts` 源文件中。这带来两个问题：

1. **面向非中文用户不可用**——SAG 是通用 LLM 反向代理网关，界面语言固定为中文会挡住相当一部分使用者。
2. **文案无法统一维护**——同一概念（如 `Enabled`、`Save`）在几十个文件里各自重复一份字面量，改一处措辞要全仓搜索。

本次变更引入第一版 UI 多语言支持（en / zh），把全部文案收敛到字典，并提供运行时切换入口。

## 需求变更

- requirements.md：「预期行为」新增「多语言支持（i18n）」小节——定义语言偏好项、默认语言、持久化、`<html lang>` 同步与切换入口
- requirements.md：「功能边界 → 包含」新增「多语言支持（en / zh）：语言偏好可切换并持久化」
- requirements.md：「功能边界 → 不包含」移除「国际化（i18n）」——该条与本次实现不符
- requirements.md：末尾「变更记录」新增 0004 条目

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-ui/src/lib/i18n/index.ts`](../../../../sag-ui/src/lib/i18n/index.ts) | 新增：DIY i18n 核心。导出 `t(key, params?)`（`{param}` 命名插值，解析顺序 active → `en` → key 本身，缺 key 时 dev 警告并回显 key）、`setLocale()`、`useI18n()`（`{ t, locale, setLocale }`）、`installI18n(app)`（注册 `globalProperties.$t` + `watchEffect` 同步 `<html lang>` 为 `zh-CN` / `en`）；`LOCALE_STORAGE_KEY = "sag-ui-lang"`、`DEFAULT_LOCALE = "zh"`；locale ref 经 `useLocalStorage` 懒建（模块在无 `window` 的 node 环境可加载），读取时经 `normalizeLocale` 白名单校验，脏值自愈回默认 |
| [`sag-ui/src/lib/i18n/en.ts`](../../../../sag-ui/src/lib/i18n/en.ts) | 新增：canonical 英文字典（`as const`），539 个扁平点分 key；key 为稳定英文标识符（如 `common.save`、`providerList.columns.apis`） |
| [`sag-ui/src/lib/i18n/zh.ts`](../../../../sag-ui/src/lib/i18n/zh.ts) | 新增：中文译文，类型为 `Record<keyof typeof en, string>`——缺译即编译错，中英 key 集合编译期齐平 |
| [`sag-ui/src/types/i18n.d.ts`](../../../../sag-ui/src/types/i18n.d.ts) | 新增：声明 `ComponentCustomProperties.$t: typeof t`，使模板里的 `$t()` 有类型 |
| [`sag-ui/src/main.ts`](../../../../sag-ui/src/main.ts) | `installI18n(app)` 在 `app.mount()` 前调用 |
| [`sag-ui/src/pages/SettingsPage.vue`](../../../../sag-ui/src/pages/SettingsPage.vue) | 新增 Language tab（Select：English / 中文），绑定 `useI18n()` 的 `locale` + `setLocale`；页面其余文案同步迁移 |
| [`sag-ui/src/components/SidebarNav.vue`](../../../../sag-ui/src/components/SidebarNav.vue) | 导航项与分组标题迁移为 `t()` |
| [`sag-ui/src/components/providers/ProviderList.vue`](../../../../sag-ui/src/components/providers/ProviderList.vue) | TanStack 列定义、筛选与行内操作文案迁移；表头一律 `() => t(...)` 函数式 |
| [`sag-ui/src/components/providers/ProviderAddForm.vue`](../../../../sag-ui/src/components/providers/ProviderAddForm.vue) | 表单标签、Zod 校验消息迁移；`validators` 改为 getter 模式 |
| [`sag-ui/src/components/routes/RouteList.vue`](../../../../sag-ui/src/components/routes/RouteList.vue) | 列定义、删除/清除确认对话框文案迁移；`strategyLabels` 改为 `computed` 标签映射 |
| [`sag-ui/src/components/analytics/AnalyticsV2Panel.vue`](../../../../sag-ui/src/components/analytics/AnalyticsV2Panel.vue) | 分析页筛选、表格、空态文案迁移 |
| [`sag-ui/src/pages/LogsPage.vue`](../../../../sag-ui/src/pages/LogsPage.vue) | 日志页筛选、12 列表头、分页与 tooltip 文案迁移 |
| [`sag-ui/scripts/check-i18n.mjs`](../../../../sag-ui/scripts/check-i18n.mjs) | 新增：防回潮门禁（无依赖，`node --experimental-strip-types` 运行）。规则 a–d 失败即 exit 1：a) en/zh 键集合完全一致；b) 每个 key 的 `{param}` 占位符集合一致；c) 花括号合法（平衡且每个 `{` 开启合法 `{name}`）；d) `src/**/*.{ts,vue}` 中无 CJK 字面量（`src/lib/i18n/` 整目录跳过、注释行跳过）。规则 e 仅报告不失败：dead key（`en` 中定义但 `src/**` 无 `t("key")` 引用）计数与列表 |
| [`justfile`](../../../../justfile) | 新增 `ui-i18n-check` recipe（`pnpm -C sag-ui run check:i18n`），并接入 `verify-ui` 依赖链 |
| [`sag-ui/package.json`](../../../../sag-ui/package.json) | 新增 `check:i18n` script |

除上表代表性文件外，另迁移 31 个 `.vue` / `.ts` 源文件（全量迁移共 34 个 `.vue`、40 个 `src/` 文件）的文案，覆盖 `App.vue`、`AppLayout.vue`、`ConnectPage.vue`、`DashboardPage.vue`、`RoutesPage.vue`、`ModelManager.vue`、`RouteAddForm.vue`、`RouteEditForm.vue`、`TargetEditor.vue`、`ProviderEditForm.vue`、`ProviderAdvancedTab.vue`、`ProviderBasicFields.vue`、`ProviderModelsTab.vue`、`ProtocolProbePanel.vue`、`ApisEditor.vue`、`ApiKeyEditor.vue`、`KvEditor.vue`、`Combobox.vue`、`SearchSelect.vue`、`RefreshButton.vue`、`analytics/DateRangePicker.vue`、`analytics/V2TrendChart.vue`、`analytics/v2.ts`、`circuit/CircuitBreakerPanel.vue`、`settings/BackoffForm.vue`、`settings/CircuitBreakerForm.vue`、`settings/StrategyTimersForm.vue`、`settings/SystemInfoCard.vue`、`lib/utils.ts`，以及 `composables/useProviderProbe.ts`、`lib/provider-probe.ts`。

`sag-ui/src/components/ui/**`（shadcn-vue CLI 生成物）未改动。

## 测试

新增两个 node test runner 测试（`pnpm -C sag-ui test`，即 `node --experimental-strip-types --test "src/**/*.test.ts"`）：

- [`sag-ui/src/lib/i18n/i18n.test.ts`](../../../../sag-ui/src/lib/i18n/i18n.test.ts)：
  - 默认值：`DEFAULT_LOCALE === "zh"`、`LOCALE_STORAGE_KEY === "sag-ui-lang"`、无 `localStorage` 的新进程 `locale.value === "zh"`
  - 插值：命名 `{param}` 替换、缺 param 保留字面占位符、无 param 时模板原样返回、无占位符模板忽略 params
  - 回退顺序：active → `en` → key 本身（构造一个 `zh` 缺失的 key 验证回退到 `en`）
  - dev 警告探针：用 `module.registerHooks` 注入 `import.meta.env = { DEV: true }` 后加载模块，断言 `t(missingKey)` 回显 key 且产生 `missing key` 警告（真实走 dev 分支而非打桩）
  - 键齐平：`Object.keys(zh).sort()` 与 `Object.keys(en).sort()` 全等，且值均为非空字符串
  - `setLocale` 驱动响应式：切换后 `locale.value` 与 `t()` 同步翻转并可切回
  - `installI18n`：注册 `$t`，且在无 `document` 环境下 `<html lang>` watcher 不抛错
  - 脏 locale 子进程回归：在子进程中预置 `localStorage` 为 `"fr"` / `""` / `"FR"` / `"zh-CN"` / `null` / `'{"a":1}'`，断言全部自愈为 `zh` 且 `t()` 不抛错；`"en"` / `"zh"` 合法值原样保留
- [`sag-ui/src/lib/i18n/i18n-form.test.ts`](../../../../sag-ui/src/lib/i18n/i18n-form.test.ts)：对真实 `FormApi`（`@tanstack/vue-form`）复现生产表单的 `get validators()` getter 形态，断言 `setLocale("zh")` / `setLocale("en")` 后 `form.validate("submit")` 产生的校验消息随之翻转（zh → en → zh）

门禁与既有验证：

- `pnpm -C sag-ui run check:i18n`（`just ui-i18n-check`）：键齐平 / 占位符齐平 / 花括号合法 / CJK 字面量扫描全部通过，dead-key 报告为 0（或仅报告不阻塞）
- `pnpm -C sag-ui run typecheck`（vue-tsc）、`pnpm -C sag-ui run lint`（oxlint）、`pnpm -C sag-ui run test`、`pnpm -C sag-ui run build`（Vite）：通过
- `just verify-ui`：`fmt-check` + `lint` + `typecheck` + `test` + `i18n-check` 全链通过

浏览器实测（Vite dev server + 测试实例）：Settings → Language 切换 English / 中文后，导航、表头、toast、表单校验消息即时翻转；刷新后语言保持；`<html lang>` 同步为 `en` / `zh-CN`。
