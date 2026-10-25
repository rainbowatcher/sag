---
created: 2026-10-25
updated: 2026-10-25
type: 新增
---

# 0009 — UI 对接 session-affinity 观测与会话列/校验提示

## 动机

后端已有会话亲和（`session_sticky`）的完整观测面与清除面——`GET|DELETE /api/session-affinity`
（按 model / key 查询，按 key / model / all 清除），`call_logs.session_key` 记录会话哈希。
0008 只补齐了「能创建 session_sticky 路由」，运维仍然看不见这条策略实际在做什么：

- 列表的 Active Target 列显示当前粘住的目标，但**看不到该请求模型空间里有多少条会话 pin、
  各粘在谁身上、多久失效**，也没有清理入口——遇到「某会话一直粘在坏目标上」只能调 API；
- 请求日志表已经有会话过滤输入框与「已应用筛选」chip，却没有一列显示 `session_key`，
  排查某个会话的历史调用时必须先把 32 位哈希复制出来；
- 0008 把「通配 × session_sticky」「session_sticky + image 管线」两类组合完全留给后端拒绝，
  用户要提交一次、往返 400 才知道非法。

本次补上这三处：active target 列内的会话 pin 浮层（观测 + 两条清除路径）、Logs 的 Session 列
（点击即套用会话过滤）、以及两条与后端等价的表单前置校验。

## 需求变更

- 「预期行为 → Route 管理」新增条目：session-sticky 路由的 Active Target 单元格在现有
  target 按钮右侧挂一个「会话 pin」图标按钮（`KeyRound`），仅在 `strategy === "session_sticky"`
  时渲染；该路由没有 active target（显示 `-`）时按钮依然存在。
- 「预期行为 → Route 管理」新增条目：浮层打开时按该行的 `model_name` 拉
  `GET /api/session-affinity?model=<model>`，头部显示模型空间名与 pin 条数；每条列出
  `provider/real_model`、会话 key 前 8 位（`title` 保留全量 32 位）、来源 `source`、存活时长
  （`age_secs`）、剩余有效期（`expires_in_secs`，`ttl_secs = 0` 即 `null` 时显示「永不过期」）；
  加载中 / 空 / 错误三态各有提示，浮层内容区限高滚动。
- 「预期行为 → Route 管理」新增条目：逐条清除走 `DELETE /api/session-affinity {"key": …}`；
  「清空此模型全部」为两步确认（第一步按钮换成「清空 N 条？+ Cancel/清空」），确认后走
  `DELETE {"model": …}`；两种清除成功后都 toast（逐条带 key 前 8 位、按模型带 `deleted` 条数）、
  重新拉取该模型空间的 pin、并回调 RouteList 重拉 active targets（pin 被清后下一跳会变）。
- 「预期行为 → Route 管理」修改条目（0008「跨字段组合校验留给后端」的部分反案）：通配路由
  `model_name = "*"` × session-sticky、session-sticky × `pipeline = "image"` 两类组合改为
  表单前置校验，规则与后端 `sag-config/src/config.rs` 的 `validate_routes` 等价；错误分别挂在
  `strategy` 与 `pipeline` 字段上就地展示，不再等提交后看顶部错误条。其余组合规则（容量 `0`
  等）仍只由后端拒绝。
- 「预期行为 → Logs」新增条目：表格在 Client 列之后新增 Session 列，显示 `session_key` 前 8 位
  （`title` 全量），无会话记录时显示 `—`；点击该单元格把完整 key 写入会话筛选状态并立即重查，
  与筛选输入框、「已应用筛选」chip、清除按钮共用同一状态（`logsFilterSession`）。
- 「预期行为 → Logs」补充口径：本节历史描述里的「12 列」是 0001 时的列口径，此后 Client 列
  与本列均在其后追加，现共 15 列。
- 「概述 → 状态管理」的 Pinia stores 枚举追加 `sessionAffinity`。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-ui/src/types/api.ts`](../../../../sag-ui/src/types/api.ts) | `CallLog` 补可选 `session_key`（32 位十六进制哈希，未打戳时缺省/null）；新增 `SessionAffinityEntry`（`key` / `model` / `source` / `provider` / `real_model` / `route_model` / `age_secs` / `ttl_secs` / `expires_in_secs`，逐字段注释语义）、`SessionAffinityListResponse`（`{ entries }`）、`SessionAffinityDeletedResult`（`{ deleted }`） |
| [`sag-ui/src/stores/sessionAffinity.ts`](../../../../sag-ui/src/stores/sessionAffinity.ts) | 新增 pinia store：`entries` / `loading` / `error` + `fetchEntries(query)`（`model` / `key` / `limit`，后端默认 200、上限 1000）+ `deleteByKey(key)` / `deleteByModel(model)`；查询带单调 `generation` token，只有最新在飞的请求可写 state 与清 `loading`；401 统一走 `connection.handle401()`；删除动作不自行 refetch（作用域由调用方决定） |
| [`sag-ui/src/components/routes/SessionAffinityPopover.vue`](../../../../sag-ui/src/components/routes/SessionAffinityPopover.vue) | 新增 popover 组件（props `model`，emit `changed`）：`watch(open)` 打开即拉取，`rows` 按 `model` 过滤共享 store 的 entries；逐条清除（按 key，禁用态 + spinner）、按模型两步确认清空（`confirmClearModel`，关闭浮层时复位）；toast 成功/失败，成功后 `refresh()` + `emit("changed")` |
| [`sag-ui/src/lib/sessionAffinity.ts`](../../../../sag-ui/src/lib/sessionAffinity.ts) | 新增纯函数 `shortSessionKey(key, length = 8)`：取前 N 位（负值视作 0），只用于展示，删除仍用全量 key |
| [`sag-ui/src/lib/sessionAffinity.test.ts`](../../../../sag-ui/src/lib/sessionAffinity.test.ts) | 新增单测：默认取 8 位、短键/空键不补齐、自定义长度（0 / 负数 / 超过键长）的行为 |
| [`sag-ui/src/components/routes/RouteList.vue`](../../../../sag-ui/src/components/routes/RouteList.vue) | active target 单元格改为 `flex` 容器：既有「状态点 + provider」按钮（tooltip 不变）与新的 `SessionAffinityPopover` 并排；popover 仅在 `route.strategy === "session_sticky"` 时以 `h(...)` 挂载，`onChanged` 触发 `activeTargetStore.fetchActiveTargets()`；无 active target 时 `-` 旁同样挂载 |
| [`sag-ui/src/pages/LogsPage.vue`](../../../../sag-ui/src/pages/LogsPage.vue) | 新增 `SESSION_KEY_SHOWN = 8` 常量与 `applySessionFilter(sessionKey)`（写 `logsFilterSession` + `refreshLogs()`）；表头在 Client 后加 Session 列（`min-w-[80px]`），单元格为按钮（`font-mono`、`title` 全量 key、hover 下划线、focus ring）或 `—` |
| [`sag-ui/src/components/routes/RouteAddForm.vue`](../../../../sag-ui/src/components/routes/RouteAddForm.vue) | schema 追加两条对象级 `.refine`（`path: ["strategy"]` / `["pipeline"]`）：通配判定取 `model_name || targets[0]?.real_model`，image 判定取 `pipeline`；strategy 与 pipeline 两个 `Field` 内补 `FieldError`，使挂在字段上的错误不再静默 |
| [`sag-ui/src/components/routes/RouteEditForm.vue`](../../../../sag-ui/src/components/routes/RouteEditForm.vue) | 同上两条 `.refine`，通配判定改为读 `props.route.model_name`（编辑时模型名不可改）；pipeline `Field` 内以 `<p class="text-destructive">` 渲染首条错误（与该表单既有错误渲染风格一致） |
| [`sag-ui/src/lib/i18n/en.ts`](../../../../sag-ui/src/lib/i18n/en.ts) / [`zh.ts`](../../../../sag-ui/src/lib/i18n/zh.ts) | 各新增 18 个 key：`sessionAffinity.*` 15 个（button / title / count / loading / empty / age / expiresIn / neverExpires / clearEntry / clearModel / confirmClearModel / clearing / 3 个 toast）、`routes.form.wildcardSessionStickyUnsupported`、`routes.form.imageSessionStickyUnsupported`、`logs.columns.session`（en 589 key，zh 编译期齐平） |

## 测试

- 新增 `sag-ui/src/lib/sessionAffinity.test.ts`（node test runner，`node:assert/strict`）：
  `shortSessionKey` 的默认截断长度、短键/空键不补齐、`length` 为 0 / 负数 / 超过键长的边界。
  该测试的意图是钉住「展示用截断」与「删除用全量 key」的边界——截断结果一旦被回传给
  `DELETE /api/session-affinity` 就是一次静默的误删。
- 无组件测试：仓库仍无 Vue 组件测试先例（无 @vue/test-utils / jsdom 依赖），浮层与列渲染
  逻辑内联在 SFC 中，只把可测的纯逻辑（`shortSessionKey`）抽成 `lib/` 模块。
- 门禁：`just verify-ui`（`oxfmt --check` + `oxlint` + `vue-tsc --noEmit` + 前端测试 +
  i18n 齐平检查）全绿，`check:i18n` 589 key 通过（en/zh 键与占位符齐平）。
- 人工核对：session-sticky 路由的 active target 单元格出现 pin 按钮、其它策略没有；
  打开浮层按 model 拉到 pin 并正确显示 `0` TTL 为「永不过期」；逐条清除后列表项消失且 active
  target 侧同步刷新；按模型清空需两步；Logs 点击 Session 单元格后筛选 chip 出现同一 key、
  清除后恢复；新建路由 `model_name = "*"` + session-sticky → strategy 字段就地报错且无请求，
  pipeline 切 image + session-sticky → pipeline 字段就地报错。
- 评审结论（记录未修项，均不阻塞）：0 Blocker / 0 Major / 4 Minor——(a) 多个 popover 共享同一
  store，理论上 cross-space 的 `rows` 过滤会被别的行的刷新影响，但 `v-if`/reka-ui 的关闭语义
  使浮层不会同时处于打开态；(b) 两个表单的错误渲染风格不一致（`FieldError` vs `<p>`）；
  (c) 通配判定未对 `model_name` 做 `trim()`；(d) 跨空间陈旧条目（按 key 删除后其它空间计数
  要等下次打开才更新）。
