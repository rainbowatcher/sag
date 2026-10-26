---
created: 2026-10-26
updated: 2026-10-26
type: 新增
---

# 0011 — 日志页会话信息卡

## 动机

日志页的 Session 列（0009）与应用后的会话过滤只暴露一个 32 位哈希：前 8 位可读、全量在 `title` 里，
但**哈希本身不携带任何人类可读信息**。运维按会话筛选出一串 502 之后，仍然不知道这段对话是谁、开头问的是什么。

后端为此新增了 `session_registry`（[analytics 0009](../../analytics/changes/0009-session-registry-and-logs-session-envelope.md)）
并让 `GET /api/logs?session=<key>` 的信封携带 `session` 记录。本次把这个记录在日志页上展示成一张卡片：
应用了会话过滤时，卡片出现在筛选栏与表格之间，给出会话键、来源、会话 ID、首条消息与首末出现时间。

## 需求变更

- `docs/spec/ui/requirements.md`「Logs」新增条目：应用会话过滤（`logsFilterSession` 非空）时，筛选栏与
  表格之间渲染会话信息卡，展示会话键（前 8 位 + 全量 `title`）、来源徽标、会话 ID、首条消息预览，
  首末出现时间放在来源徽标的 tooltip 里；无注册表记录时卡片不消失，只保留会话键并显示降级文案。
- `docs/spec/ui/spec.md` 关键组件新增 `SessionInfoCard.vue` 与 `lib/sessionInfo.ts`；人工核对项新增一条
  会话信息卡的三态核对（有记录 / 无记录 / 无会话过滤不渲染）。
- 无行为回退：卡片是纯增量展示，会话过滤、Session 列、分页与其它过滤器都不受影响。

## 展示口径（四项 + 首末时间）

卡片标题「会话信息」，`dl` 网格（1 列 / `sm:` 2 列 / `xl:` 4 列，仅在**有记录**时开 4 列）：

| 位置 | 内容 | 缺失/降级 |
|---|---|---|
| 会话键 | `shortSessionKey()` 前 8 位，`title` 为全量 32 位 | 恒有（来自当前过滤值，不依赖后端） |
| 来源 | `Badge variant="outline"`：`meta` / `top-level` / `prompt cache` / `content hash` 的 i18n 标签 | 未知来源回退显示原始 `session_source`，空则 `—` |
| 会话 ID | 原始 `conversation_id`，`font-mono` 截断 + 全量 `title` | 缺失（`pc` / `fb` 来源）显示 `—` 并用 `text-muted-foreground` |
| 首条消息 | `first_message_preview` 折叠空白后截断，全量在 `title` | 缺失显示 `—` 并弱化 |
| 首次 / 最近出现 | 不在网格里，作为**来源徽标的 tooltip**（`side="top"`）显示两行 | 时间缺失或不可解析时 `—` |

**降级形态**：`session === null`（键无注册表行、旧数据、已过保留期，或注册表查询失败降级）时，卡片仍然渲染，
但只剩会话键一格 + 一句 `logs.sessionCard.noRecord` 文案，`dl` 回到 1/2 列。卡片不因缺数据而消失——
用户刚点的那个哈希必须始终可见，否则「点了 Session 列却什么都没出现」看起来像页面坏了。

展示格式化集中在 `lib/sessionInfo.ts`（纯函数，可单测）：`SESSION_SOURCE_KINDS` /
`sessionSourceKind()`（trim + 小写后收窄，未知返回 `null`）、`sessionText()`（空值 → `—`）、
`sessionPreviewText()`（折叠空白 + 按 160 字符截断，后端已截断，这里是纵深防御）、
`formatSessionTime()`（`toLocaleString("en-CA", { hour12: false })`，与日志表的 `formatTs` 口径一致；
不可解析 → `—`）。`SESSION_PLACEHOLDER = "—"` 与 Session 列同一占位符。

## store 竞态处理（`logsSessionKey`）

`logsSession` 记录与 `logs` 明细页来自**同一个响应**，但两者在 UI 上的生命周期不同：明细页是「最后一次成功
fetch 的结果」，而卡片必须永远对应**当前**过滤键。若只存记录本身，快速切换会话键（点 Session 列 → 再点另一行）
时，旧键的记录会在新请求返回前显示在新哈希下面——把一段对话的首条消息挂在另一段对话的哈希上。

因此 store 存一对：

- `logsSession: SessionInfo | null` — 响应里的 `session ?? null`；
- `logsSessionKey: string | null` — 产生该记录的 `filter.session ?? null`。

`LogsPage` 只在 `store.logsSessionKey === logsFilterSession` 时把 `logsSession` 传给卡片，否则传 `null`
（即走降级形态）。写入点严格在 `logsGuard.isCurrent(token)` 的 generation 守卫内，与 `logs` / `logsTotal`
同一临界区；失败分支额外清空两者（失败不能把上一轮的记录留在界面上）。两个字段一并纳入守卫的写入清单。

## i18n

新增 12 个 key（en / zh 同步，`just ui-i18n-check` 要求完全对齐）：

`logs.sessionCard.title`、`logs.sessionCard.sessionKey`、`logs.sessionCard.source`、
`logs.sessionCard.source.meta`、`logs.sessionCard.source.top`、`logs.sessionCard.source.pc`、
`logs.sessionCard.source.fb`、`logs.sessionCard.conversationId`、`logs.sessionCard.preview`、
`logs.sessionCard.firstSeen`、`logs.sessionCard.lastSeen`、`logs.sessionCard.noRecord`。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-ui/src/components/SessionInfoCard.vue`](../../../../sag-ui/src/components/SessionInfoCard.vue) | 新增卡片：props `session: SessionInfo \| null` + `sessionKey: string`；四项网格（会话键 / 来源徽标 / 会话 ID / 首条消息），首末时间在来源徽标的 tooltip 内；无记录时降级为「只有会话键 + 降级文案」 |
| [`sag-ui/src/lib/sessionInfo.ts`](../../../../sag-ui/src/lib/sessionInfo.ts) | 新增展示纯函数与常量：`SESSION_SOURCE_KINDS` / `SessionSourceKind` / `SESSION_PLACEHOLDER` / `sessionSourceKind` / `sessionText` / `sessionPreviewText` / `formatSessionTime` |
| [`sag-ui/src/lib/sessionInfo.test.ts`](../../../../sag-ui/src/lib/sessionInfo.test.ts) | 新增：来源收窄（含大小写/空白/未知）、空值占位、preview 折叠与截断、时间格式化与不可解析回退 |
| [`sag-ui/src/stores/analytics.ts`](../../../../sag-ui/src/stores/analytics.ts) | 新增 `logsSession` / `logsSessionKey`，在 `fetchLogs` 成功分支的 generation 守卫内写入，失败分支清空；两者纳入 store 导出 |
| [`sag-ui/src/pages/LogsPage.vue`](../../../../sag-ui/src/pages/LogsPage.vue) | 筛选栏与表格之间插入 `<SessionInfoCard v-if="logsFilterSession">`，`session` 仅在 `store.logsSessionKey === logsFilterSession` 时传入，否则传 `null` |
| [`sag-ui/src/types/api.ts`](../../../../sag-ui/src/types/api.ts) | 新增 `SessionInfo`（`session_key` / `session_source` / `conversation_id?` / `first_message_preview?` / `first_seen` / `last_seen`）；`LogsResponse` 补可选 `session?: SessionInfo \| null` 并注明「未过滤时键省略、无记录时 `null`」 |
| [`sag-ui/src/lib/i18n/en.ts`](../../../../sag-ui/src/lib/i18n/en.ts) / [`zh.ts`](../../../../sag-ui/src/lib/i18n/zh.ts) | 新增 12 个 `logs.sessionCard.*` key（两侧同步） |
| [`docs/spec/ui/requirements.md`](../requirements.md) | 「Logs」补会话信息卡的行为条目 |
| [`docs/spec/ui/spec.md`](../spec.md) | 关键组件补两个文件、人工核对项补三态核对（第 14 条）、en 字典 key 总数 589 → 601 |

## 测试

- `pnpm -C sag-ui test`（`node --experimental-strip-types --test "src/**/*.test.ts"`）覆盖
  `lib/sessionInfo.ts` 的全部纯函数分支。
- 无组件级自动化测试：仓库仍无 Vue 组件测试先例，卡片的三态走 `docs/spec/ui/spec.md` 的人工核对项。
- 门禁：`just ui-lint` / `just ui-typecheck` / `just ui-i18n-check`。
