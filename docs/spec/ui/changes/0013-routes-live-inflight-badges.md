---
created: 2026-10-26
updated: 2026-10-26
type: 新增
---

# 0013 — 路由页实时在途请求徽章行（SSE）

## 动机

后端新增了 per-route 在途计数与 `GET /api/inflight` SSE 端点
（[admin 0003](../../admin/changes/0003-per-route-in-flight-counters-sse.md)），但管理面板里
没有任何地方消费它。路由页的表格回答的是「配了什么」，运维在故障时想问的是「现在压在哪条
路由上、压了多少」。

本次在路由表格**上方**加一行实时活动徽章：总数 + 每条有流量的路由徽章（`model_name ×n` +
脉冲绿点），数据来自 SSE 长连接。UI 只做展示，不新增任何写入能力。

## 需求变更

- `docs/spec/ui/requirements.md`「Route 管理」新增条目：
  - 路由表格上方常显一行活动徽章：左侧 `◉ 处理中 N`（N = 快照 `total`）+ 标题内联 Info
    Tooltip；右侧为该路由列表，仅列出 `count > 0` 的 route，每项是 `model_name`（等宽字体、
    超长截断、`title`/Tooltip 显示全量）+ `×count` + 一个脉冲绿点徽章；
  - 快照确认全零（`routes` 为空且 `total === 0`）时显示「空闲 — 无正在处理的请求」占位；
  - 首帧到达前整行不渲染（没有快照时说「0」是在撒谎）；
  - 流断开时**保留最后一份快照**并标 `stale`：徽章置灰、绿点变灰、右侧显示
    「连接断开，重连中…」（带 Tooltip 说明计数可能过期），不清零；
  - 401/403 走共享 `handle401()` 并**停止重连**，不留在「重连中」里无限重试。
- 「概述 → 状态管理」的 Pinia stores 枚举追加 `inflight`；「功能边界 → 包含」补 SSE 实时在途计数。
- 无行为回退：表格、筛选、CRUD、active target 列均不变；徽章行只是表格上方的增量一行。

## 展示口径

| 状态 | 渲染 |
|---|---|
| 尚无快照 | 整行不渲染 |
| 有快照且 `total > 0` | `◉ 处理中 N` + `·` + 每个 `count > 0` 的 route 徽章（`model_name ×n`，脉冲绿点），按 `count` 降序 |
| 有快照且全零 | `◉ 处理中 0` + 「空闲 — 无正在处理的请求」 |
| 断开且持有快照（`stale`） | 徽章整体 `opacity-60`、绿点变灰、附「连接断开，重连中…」；**计数保留不清零** |

「空闲」需要 `routes` 与 `total` **同时**为零才判定——若某条 route 条目被解析丢弃（畸形帧），
两个数字会不一致，此时宁可显示「处理中 N」也不能自相矛盾地喊空闲。

## 前端实现要点

- **不用 `EventSource`**：它无法附加请求头，而 `/api/*` 需要 `Authorization: Bearer`。
  `lib/sse.ts` 用 `fetch` + `ReadableStream` 自读字节流：`createSseParser` 做**跨 chunk 安全**的
  分帧（CRLF 可能被切在两个 chunk 之间、尾随孤立 CR 在 flush 时丢弃以免改写 `event:` 行）、
  `: ping` 注释帧丢弃、多行 `data:` 用 `\n` 拼接、无 `data` 的帧不派发。
- **`Accept` 必须写在 spread 之后**：请求头来自 `api.defaults.headers.common`，其中已有
  `Accept: application/json`，写成 `{ Accept: "text/event-stream", ...headers }` 会被覆盖掉
  SSE 握手头。`headersFromDefaults()` 只复制字符串值（axios 的默认头里混有非字符串项）。
- **重连**：指数退避 `nextRetryDelay(attempt) = min(maxMs, base * factor^attempt)`
  （默认 1s 起、30s 封顶），连接成功即重置 `attempt`；`stop()` 同时 abort 在飞请求与待触发的
  重试定时器，卸载后不留僵尸连接。
- **stop 之后零投递**：`consume()` 里每个回调前都过 `running` 闸——一个在 `stop()` 之后才
  settle 的 `read()` 不得把已关闭的面板重新点亮（会把 `stale` 清掉）。
- **认证失败契约**：非 2xx 抛 `SseHttpError.status`，`isAuthFailure()` 判 401/403 → 调
  `useConnectionStore().handle401()` 并 `client.stop()`，不重连。
- **状态替换而非合并**：后端每帧都是全量快照，`applySnapshot` 直接整体替换，`stale` **只**在这
  里清除（新快照是「界面与网关重新一致」的唯一证据）。`markDisconnected` 保留快照并置
  `stale = snapshot !== null`。
- **畸形帧不得清空面板**：`parseInflightSnapshot` 在 JSON 非法、非对象、`seq` 不可用时返回
  `null`（整帧丢弃）；`routes` 里的坏条目逐条跳过；未知字段忽略（网关设计原则）；
  `total` 缺失时用各 `count` 求和兜底；本地再按 `count` 降序排一次，徽章顺序不依赖后端的
  sort 稳定性。
- **store 生命周期**：`RoutesPage` `onMounted` 启动、`onUnmounted` 停止；快照留在 store 里，
  重新进入页面立刻重绘上一份（不必等首帧）。SSE client 是资源而非数据，存在闭包里**不进**
  reactive state（否则会被 Vue proxy 包一层）。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-ui/src/lib/sse.ts`](../../../../sag-ui/src/lib/sse.ts) | 新增通用 SSE 客户端：`createSseParser`（跨 chunk 分帧 / 心跳注释 / 多行 data / flush 尾帧）、`nextRetryDelay`（指数退避 + 封顶）、`headersFromDefaults`（只取字符串头）、`SseHttpError` + `isAuthFailure`（401/403）、`createSseClient`（单飞循环 + `stop()` abort + `running` 闸） |
| [`sag-ui/src/lib/inflight.ts`](../../../../sag-ui/src/lib/inflight.ts) | 新增纯函数：`emptyInflightState` / `parseInflightSnapshot`（容错解析 + 本地降序）/ `applySnapshot`（整体替换、清 `stale`）/ `markConnected` / `markDisconnected`（保留快照标 `stale`）/ `activeRouteCounts`（只留 `count > 0`）/ `resolveSseUrl`（由 axios `baseURL` 拼流地址） |
| [`sag-ui/src/stores/inflight.ts`](../../../../sag-ui/src/stores/inflight.ts) | 新增 pinia store：`snapshot` / `connected` / `stale` + `activeRoutes` / `total` / `hasTraffic`；`start()`（组装头 + 只认 `event === "inflight"` + 401 走 `handle401` 并停止重连）、`stop()`、`reset()`；请求头 = axios 默认头 + `connection.apiKey`（`x-api-key`） |
| [`sag-ui/src/components/routes/RouteActivityBar.vue`](../../../../sag-ui/src/components/routes/RouteActivityBar.vue) | 新增徽章行组件：`Activity` 图标 + `routeActivity.processing`（标题内联 Info Tooltip）、逐 route 徽章（脉冲绿点 + 等宽 `model_name` 截断 + `×count` + Tooltip）、`stale` 时置灰 + `WifiOff` +「连接断开，重连中…」、全零时「空闲」占位、无快照时整行不渲染 |
| [`sag-ui/src/pages/RoutesPage.vue`](../../../../sag-ui/src/pages/RoutesPage.vue) | 表格上方插入 `<RouteActivityBar />`（`mb-3 shrink-0`，属固定区域）；`onMounted(() => inflightStore.start())` / `onUnmounted(() => inflightStore.stop())` |
| [`sag-ui/src/types/api.ts`](../../../../sag-ui/src/types/api.ts) | 新增 `InflightRouteCount`（`model_name` / `count`）与 `InflightSnapshot`（`seq` / `total` / `routes`），逐字段注释语义 |
| [`sag-ui/src/lib/i18n/en.ts`](../../../../sag-ui/src/lib/i18n/en.ts) / [`zh.ts`](../../../../sag-ui/src/lib/i18n/zh.ts) | 新增 6 个 `routeActivity.*` key（processing / processingTitle / idle / reconnecting / staleHint / routeTitle），两侧同步（en 607 key） |
| [`sag-ui/src/lib/inflight.test.ts`](../../../../sag-ui/src/lib/inflight.test.ts) | 新增单测：帧解析（正常 / 重排序 / 非法 JSON / 非对象 / 缺 `seq` / 坏条目跳过 / 未知字段忽略 / `total` 兜底）、快照替换、open/drop 转换、`count > 0` 过滤、流 URL 拼接 |
| [`sag-ui/src/lib/sse.test.ts`](../../../../sag-ui/src/lib/sse.test.ts) | 新增单测：单帧 / 跨 chunk / 一 chunk 多帧 / 心跳注释 / CRLF / 多行 data 与默认事件名 / `id` 与无 data 不派发 / flush 尾帧与尾随孤立 CR / 字段值细节 / 退避表 / 头转发；连接循环用 mock `fetch` + `ReadableStream` 验证头转发（含 `Accept` 覆盖）、断线重连、`stop()` 中止与零投递 |
| [`docs/spec/ui/requirements.md`](../requirements.md) | 「Route 管理」补徽章行行为条目、stores 枚举补 `inflight`、「包含」补 SSE 观测、变更记录 |
| [`docs/spec/ui/spec.md`](../spec.md) | 关键文件补 4 个新文件、新增关键决策（`fetch` + `ReadableStream` vs `EventSource`；全量快照替换 vs 增量合并）、新增 4 条不变式（见下）、测试策略与关键验证场景补一条、变更记录 |

## 测试

- `pnpm -C sag-ui test`（`node --experimental-strip-types --test "src/**/*.test.ts"`）覆盖
  `lib/sse.ts` 与 `lib/inflight.ts` 的全部纯函数与连接循环分支；连接循环用 mock `fetch` 驱动
  真实 `ReadableStream`，因此「跨 chunk 分帧」「`stop()` 后零投递」「401 不重连」都是可回归的。
- 无组件级自动化测试：仓库仍无 Vue 组件测试先例，徽章行的四态走 `docs/spec/ui/spec.md` 的
  人工核对项（第 15 条）。
- 门禁：`just ui-lint` / `just ui-typecheck` / `just ui-i18n-check` / `just verify-ui`。
- 人工核对：打开路由页 → 徽章行出现且首帧前不渲染；发一个流式请求 → 对应 route 徽章出现、
  绿点脉冲、`×1`，请求结束后徽章消失并回到「空闲」；杀掉 gateway 或断网 → 徽章置灰 +
  「连接断开，重连中…」且计数**不归零**，恢复后自动回到实时态；把 API key 改错后重连 →
  跳转 `/connect` 而不是无限「重连中」。
