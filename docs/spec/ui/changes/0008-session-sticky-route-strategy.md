---
created: 2026-10-25
updated: 2026-10-25
type: 新增
---

# 0008 — UI 补齐 `session_sticky` 路由策略

## 动机

后端自 v0.8.0 起已有第 4 个路由策略 `session_sticky`（`sag-config/src/config.rs` 的
`Strategy` 枚举、`/api/session-affinity` 读写会话 pin、`session_ttl_secs` /
`session_max_entries` 两个可调参数）。前端把策略枚举硬编码在 `types/api.ts` 的 `Strategy`
联合类型与两个表单的 Zod schema 里，只列了 `sticky_failover` / `fill_first` / `round_robin`
三值，于是：

- 通过配置或 API 建成 `session_sticky` 的路由，在列表里没有 badge 样式与文案（渲染成裸
  值或空标签），在编辑表单里过不了 Zod 枚举校验，改任何字段都无法保存；
- 新建表单无从选中该策略，两个调参字段在 UI 上完全不可达——用户只能改 `gateway.toml`。

本次把类型、表单、列表与文案补齐，使 UI 与后端策略面一致。

## 需求变更

- 「预期行为 → Route 管理」新增条目：策略选择（ToggleGroup）含四个值
  sticky-failover / fill-first / round-robin / session-sticky；选中 session-sticky 时展开
  会话 TTL（秒，整数 ≥ 0，`0` = 永不过期）与每请求模型会话容量（整数 ≥ 1）两个数字输入，
  留空 = 不发送该字段、使用后端默认值；两个输入仅在 session-sticky 下显示，也只有在该
  策略下参与校验（切回其它策略后残留的输入不会再挡住提交）。
- 「预期行为 → Route 管理」新增条目：这两个字段的 update 语义是「省略=保留」，编辑表单
  因此不提供清空交互（清空输入框等价于省略，即保持当前值）。
- 「预期行为 → Route 管理」新增条目：前端不做跨字段组合校验（通配路由 `*` ×
  `session_sticky`、`session_sticky` + `pipeline = "image"` 两类组合由后端 `validate_routes`
  拒绝），错误经表单顶部错误条以 400 消息原文展示，不翻译、不复述。
- 「预期行为 → Route 管理」列表的 Strategy 彩色标签新增 session-sticky 档（fuchsia 配色，
  与既有三色区分）。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-ui/src/types/api.ts`](../../../../sag-ui/src/types/api.ts) | `Strategy` 联合类型补 `"session_sticky"`；`RouteConfig` 新增可选 `session_ttl_secs` / `session_max_entries`（带语义注释：`0` = 永不过期、`0` 容量被后端拒绝、省略 = 后端默认）；`CreateRoutePayload` / `UpdateRoutePayload` 各补两个可选数字字段，update 侧注释明确「省略 = 保留当前值（后端无法清空）」；`ActiveTargetEntry.strategy` 的注释枚举同步补值 |
| [`sag-ui/src/components/routes/RouteAddForm.vue`](../../../../sag-ui/src/components/routes/RouteAddForm.vue) | Zod schema 的 `strategy` 枚举补 `session_sticky`，新增两个字符串形态的数字字段 + 两条 `.refine`（正则 `^\d+$` / `^[1-9]\d*$`，且仅当 `strategy === "session_sticky"` 时生效）；新增 `parseOptionalInt`（空串 → `undefined`，以此省略字段）；提交时仅在 session-sticky 下把两个字段并入 payload；ToggleGroup 补第 4 个选项 + 策略说明行；两个 `Input type="number"` 字段（`min` / placeholder / 提示文案 / `aria-invalid` + `FieldError`），按策略条件渲染 |
| [`sag-ui/src/components/routes/RouteEditForm.vue`](../../../../sag-ui/src/components/routes/RouteEditForm.vue) | 同上补枚举、字段、两条策略相关 `.refine`、`parseOptionalInt`、ToggleGroup 选项与说明行、两个条件渲染输入；初值由 `props.route.session_*` 序列化成字符串（`undefined` → 空串），提交时**仅在值有变化时**发送（值未变或留空 → `undefined` → 省略即保留），`api_endpoint` 的既有 `null` 清除语义不受影响 |
| [`sag-ui/src/components/routes/RouteList.vue`](../../../../sag-ui/src/components/routes/RouteList.vue) | `strategyStyles` 新增 `session_sticky` 的 badge 配色；`strategyLabels`（`computed`，随 locale 重建）新增 `session_sticky` 文案映射 |
| [`sag-ui/src/lib/i18n/en.ts`](../../../../sag-ui/src/lib/i18n/en.ts) / [`zh.ts`](../../../../sag-ui/src/lib/i18n/zh.ts) | 各新增 9 个 key：`routeList.strategy.sessionSticky`、`routes.form.strategySessionSticky` / `strategySessionStickyHint` / `sessionTtlLabel` / `sessionTtlHint` / `sessionTtlInvalid` / `sessionMaxEntriesLabel` / `sessionMaxEntriesHint` / `sessionMaxEntriesInvalid`（en 571 key，zh 编译期齐平） |

## 测试

- 无新增组件测试：仓库没有表单组件测试先例（表单逻辑依赖 TanStack Form 的 `Field` 渲染与
  shadcn `Input`，现有测试全部落在 `src/lib/*.test.ts` 的纯逻辑模块上）。本次新增的逻辑只有
  `parseOptionalInt` 与两条 Zod refine，均内联在 SFC 内，抽出成 lib 会为一次性用途引入
  新模块，故不加测试。
- 门禁：`just verify-ui`（`oxfmt --check` + `oxlint` + `vue-tsc --noEmit` + 前端测试 5 pass +
  i18n 齐平检查，en/zh 均 571 key）全绿。
- 人工核对：新建路由选 session-sticky → 两个输入出现且为必填可选（留空可提交，payload 不含
  两字段）；填非整数 / 负容量 → 就地报错且不发请求；编辑既有 session-sticky 路由 → 初值正确
  回填，不改这两项时 payload 中不出现它们；会话 TTL 留空保存后列表 badge 仍为 session-sticky；
  `model_name = "*"` 配 session-sticky 提交 → 顶部错误条显示后端 400 消息。
