---
created: 2026-10-26
updated: 2026-10-26
type: 修改
---

# 0012 — 会话 pin 图标 hover 预览与活跃 pin 置顶/置灰

## 动机

0009 的会话 pin 观测面只有一个入口：点击钥匙 icon 打开浮层。这个入口同时承担「看」与「改」，
带来三处使用上的摩擦：

- **观测是只读动作，却与破坏性动作共用一个入口**。想确认某条 `session_sticky` 路由当前粘住了
  谁，必须先点开一个带删除按钮的浮层（多一次点击 + 多一次误触风险），再把浮层关掉；指针沿
  Active Target 列扫过一整排路由时，也无法「顺手看一眼」。
- **看不出哪条 pin 还在生效**。后端在 TTL 到期后不会立刻删除条目，`expires_in_secs <= 0` 的
  过期残留会和活跃 pin 混在同一个列表里、外观完全一致，运维无法判断某个会话「现在是否真的
  粘在这个目标上」。
- **`-` 占位符占了位置却没信息**。非 session_sticky 行没有 active target 时显示 `-` 是合理的
  （该列语义就是「当前目标」），但 session_sticky 行同样是 `-` + 钥匙 icon 并列，而 `-` 在这里
  只是噪声——这一行真正可看的东西是 pin 列表。

本次把「看」和「改」拆开：钥匙 icon hover 出**只读** Tooltip 预览（节流拉取），点击仍进带
清除动作的 Popover；两个列表共用的 pin 排序把活跃项顶置、过期残留置灰；active target 为空时
不再渲染 `-`。

## 需求变更

- 「预期行为 → Route 管理」修改条目（0009 的「会话 pin 观测」）：session_sticky 行在没有 active
  target 时**不再渲染 `-` 占位符**，单元格只显示钥匙 icon；其它策略仍为 `-`（该列语义仍是
  「当前目标」，只是不再为 session_sticky 行补一个无信息的占位）。
- 「预期行为 → Route 管理」新增条目：钥匙 icon **hover 即出只读 Tooltip 预览**（`side="left"`），
  内容与浮层同源——标题 + pin 条数 + 加载中 / 空 / 错误三态 + pin 列表（`provider/real_model`、
  key 前 8 位、`source`、剩余 TTL）；预览里没有任何清除入口，破坏性动作只在点击后的浮层里。
  浮层打开时不显示 tooltip（受控 `:open`，不再用 `disabled`），两者不同时出现。
- 「预期行为 → Route 管理」新增条目：pin 列表（Tooltip 与浮层共用同一份排序）按存活性分组，
  活跃 pin（`expires_in_secs === null` 即 `session_ttl_secs = 0`、或 `> 0`）全部置顶，过期残留
  （`<= 0`）排在后面并置灰；组内保持后端返回的相对顺序（stable sort）。剩余有效期文案新增
  第四态：`<= 0` 显示「已过期」（复用既有 key `common.expired`，本次**不新增** i18n key）。
- 「预期行为 → Route 管理」新增条目（拉取时机）：hover / 键盘聚焦（`@mouseenter` 与 `@focus`）触发的
  拉取有 **3 秒节流**——`store.loading` 中跳过、距上次拉取完成不足 `HOVER_REFRESH_MS`（3000ms）
  也跳过，因此指针扫过一整列路由不会一行发一个请求；点击打开浮层时**总是重新拉取**（不走节流），
  保证看到的是当下状态。
- 「预期行为 → Route 管理」补充口径：Tooltip 预览只读、浮层才可清除，因此 0009 的两条清除路径
  （逐条 `DELETE {"key": …}`、按模型两步确认 `DELETE {"model": …}`）行为不变。
- 无行为回退：`sessionAffinity` store 的 `generation` 竞态 token、`emit("changed")` 后 RouteList
  重拉 active targets、key 前 8 位展示 / 全量 key 删除的口径全部沿用 0009，未改动。

### 审查返工（同批修正，非初版行为）

初版评审发现共享 store 的写入口径有缺陷，返工三点如下，均已在本次提交内生效：

- **共享 store 改为按 model 合并**：`fetchEntries` 成功路径不再整体覆盖 `entries`，改走新增的
  `mergeSessionEntries(prev, incoming, model?)`——`model` 未指定（全量拉取）整体替换，给定则只替换
  该 model 的桶、保留其他行的数据。初版是整体覆盖，导致 hover 行 A → 行 B → 行 A 时 A 的条目被
  行 B 的响应刷掉：A 假「无 pin」，已打开的浮层被清空且不自愈。
- **失败路径不再清空**：`fetchEntries` 的 catch 只写 `error`，保留上一次成功拉到的 `entries`。
  失败 ≠ 空列表，清空会让整列 pin 突然变成「无 pin」。
- **取数触发点补 `@focus`**：除 `@mouseenter` 外钥匙 icon 也绑 `@focus="ensureFresh"`，键盘 Tab
  聚焦同样会拉取（节流窗口不变）。初版只覆盖指针，键盘用户无法从预览态拿到数据。

### 审查返工（第二轮，同批修正）

第一轮返工修好了 `entries` 的写入口径（按 model 换桶、失败不清空），但 `error` 仍是全局单槽，
且渲染链上 error 优先于 rows。第二轮把错误面也按作用域收窄，闭环同一类失效：

- **`error` 作用域化**：store 的 `error` 改为 `{ scope?: string; message: string } | null`，`scope`
  记该次 fetch 的 `params.model`（全量拉取为 `undefined`）。`fetchEntries` 启动 / 成功只清除**同 scope**
  的错误，失败只写自己 scope。初版任一行失败会把所有行一起置错。
- **rows 优先渲染**：`SessionAffinityPopover` 的 Tooltip 与 Popover 两处在 `rows.length > 0` 时**始终
  渲染列表**，同 scope 的错误降级为列表下方非阻塞小字提示；只有 `rows` 为空时才走 loading →
  同 scope error → 空态。初版 error 优先于 rows：任一行 fetch 失败就用错误文案盖住所有行保留下来的
  pin 数据——Tooltip 显示的是**他行**的错误文案且条数与列表自相矛盾，打开中的浮层也不自愈。
  这与第一轮的「整体覆盖 entries」属同一失效类别（共享槽位 + 渲染优先级错配），本轮一并闭环。
- **`errorForModel(error, model)` 纯函数 + 单测**：scope 匹配取值抽进 `lib/sessionAffinity.ts`，
  由 node:test 覆盖同 scope 命中 / 他行 scope 不命中 / 全量拉取的 `undefined` scope 不命中单行 /
  `error` 为 `null`（第三轮已改为 `errors: Record<string, string>` + `Object.hasOwn`，见下）。

### Minor 跟进（第三轮，同批修正）

第三轮把共享 store 的记账面彻底按作用域铺平，并修掉 hover 预览的 reka-ui 受控缺陷：

- **`errors` / `loading` 改为按 scope 的映射**：store 的 `error: { scope, message } | null` 与 `loading: boolean`
  换成 `errors: Record<string, string>` / `loading: Record<string, boolean>`（键 = `params.model ?? ""`，`""` 为全量拉取），
  `generation` token 也改为 `Map<scope, number>`。单槽 error/loading 在共享 store 里会跨行互相覆盖：行 B 的失败
  顶掉行 A 的错误、行 B 的在飞让行 A 转圈。组件改按 `errorForModel(store.errors, props.model)` 与
  `store.loading[props.model] ?? false` 取值（第四轮改为 `loadingForModel(store.loading, props.model)`，见下）。
- **启动不清 error**：`fetchEntries` 启动只置 `loading[scope]`，不再清 `errors[scope]`；成功才清本 scope，失败
  写本 scope。初版启动即清，重试期间会闪一次 loading / 空态。内部 `clearError(scope)` 用解构剔除，只删匹配键。
- **陈旧响应只跳过自己 scope**：token 按 scope 记后，慢的旧请求不再连带丢弃其他 scope 的在途状态（初版单一
  token + `loading` 短路，会让 hover 到某行时瞬时假「无 pin」）。
- **`errorForModel` 改用 `Object.hasOwn` 判定**：`errors` 是普通对象，裸 `errors[model]` 会走原型链——模型名
  恰好是 `constructor` / `toString` 时会把 `Function` 当错误文案渲染。
- **Tooltip 受控化（不再传 `disabled`）**：组件自管 `hovering`（`pointerenter` 过滤 `pointerType === "touch"`、
  `pointerleave` 复位）与 `focused`（`@focus` 仅 `:focus-visible` 置真、`@blur` 复位），
  `:open="(hovering || focused) && !open"`。根因：reka-ui 的 `disabled` 会清空 trigger 的监听对象，而
  `hasPointerMoveOpened` 只在 `pointerleave` 复位——popover 关闭后指针原地不动再 hover 永不重现预览。
- **过期行置灰加强**：Tooltip 行 `opacity-70`、浮层行 `text-muted-foreground opacity-60`（原为各 `opacity-50`，
  在两种底色上都偏淡）。
- **store 测试基建落地**：新增 `sag-ui/scripts/alias-loader.mjs`（`module.register` + `data:` URL 的 resolve hook，
  映射 `@/` → `src/` 并补 `.ts` / `/index.ts`），`package.json` 的 `test` 脚本加 `--import`；新增
  `sag-ui/src/stores/sessionAffinity.test.ts`，把 `api.get` 换成 deferred 队列（网络层是 axios 非 `fetch`）以控制
  结算顺序，覆盖 8 组行为（全量拉取、按 model 换桶、作用域失败、重试保留旧错误、`clearError` 只清匹配键、陈旧
  成功、陈旧失败、并发 scope 互不写入）。既有测试不受影响。
- **第四轮收尾（分次演进，不是推翻第三轮）**：第三轮写下的「去掉 `store.loading` 闸、只看时间窗口」是针对
  「拿整份 `loading` 映射当布尔 → 恒为真 → 永不刷新」的修法，目的在去掉**全局**闸而非放弃在途去重；本轮把在途判定
  **按 scope** 补回：`ensureFresh()` 的取数条件 = 「距上次取数 ≥ `HOVER_REFRESH_MS`」**且**「本行 scope 无在途请求」，
  慢请求未结算时反复 hover / focus 不再并发排入相同请求。同批收尾五项：① `lib/sessionAffinity.ts` 新增与
  `errorForModel` 对称的 `loadingForModel(loading, model)`（`Object.hasOwn` 防原型链键），组件 loading 取值改用它
  （此前裸 `store.loading[props.model] ?? false` 会让 `constructor` / `toString` 类模型名 loading 恒真、永远转圈）；
  ② store 的 `clearError` 前置判断改 `Object.hasOwn`（原型链键不得被当成待清错误）；③ 在途去重恢复（上段）；
  ④ 清理验证期的临时脚手架，测试基建只保留 `scripts/alias-loader.mjs`；⑤ store 测试补两条——他行在途时本行 `loading`
  为 `false`、陈旧响应不触碰 `loading` / `errors`。

两个已知权衡（受控 Tooltip 的代价）：

- **grace area 失效**：受控 `:open` 下没有 reka 的「指针从 trigger 移到内容仍保持打开」延迟区，指针移进 tooltip
  内容即关闭；长列表请走浮层。
- **Esc / 焦点语义**：Esc 关浮层后 reka 把焦点交还 trigger（`:focus-visible` 为真），预览按焦点语义继续显示，
  移开指针不隐藏、失焦才隐藏；tooltip 也因此即时出现（跳过 reka 的 delay）。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-ui/src/lib/sessionAffinity.ts`](../../../../sag-ui/src/lib/sessionAffinity.ts) | 新增四个纯函数：`isPinActive(entry)`（`expires_in_secs === null \|\| > 0` 为活跃）、`sortPins(entries)`（活跃组在前、组内 stable，返回新数组不改入参）、`mergeSessionEntries(prev, incoming, model?)`（`model` 省略整体替换、给定只换该 model 的桶，返回新数组不改入参）、`errorForModel(errors, model)`（审查返工第二轮补、第三轮改签名：`Object.hasOwn(errors, model)` 为真时返回 `errors[model]`，否则 `null`；`""` 为全量拉取的作用域键）；第四轮补对称的 `loadingForModel(loading, model)`（同样 `Object.hasOwn` 守卫，防 `constructor` / `toString` 类模型名 loading 恒真） |
| [`sag-ui/src/stores/sessionAffinity.ts`](../../../../sag-ui/src/stores/sessionAffinity.ts) | `fetchEntries` 成功路径改用 `mergeSessionEntries(entries, data.entries, params.model)`；catch 不清空 `entries`，只写 `errors[scope]`（第三轮：`loading` / `errors` 均为 `Record<scope, …>`、`generation` 为 `Map<scope, number>`，`scope` = `params.model ?? ""`；启动 / 重试不清 error，成功只清本 scope；删除路径不变；第四轮 `clearError` 前置判断改 `Object.hasOwn`） |
| [`sag-ui/src/lib/sessionAffinity.test.ts`](../../../../sag-ui/src/lib/sessionAffinity.test.ts) | 新增 `isPinActive` 边界（`null` / 正数 / `1` / `0` / 负数）、`sortPins`（活跃顶置 + 组内原序、不改入参、返回新数组、空列表、全过期 / 全活跃退化输入）与 `mergeSessionEntries`（按 model 换桶保留其他行、同一 model 重拉不残留旧条目、空桶只清该 model、省略 `model` 整体替换、不改入参）单测；第二轮补 `errorForModel` 作用域用例，第三轮改为映射签名并补原型链用例（同 scope 命中 / 他行 scope 不命中 / `""` 只被全量视图读取 / 空映射 / `constructor`・`toString` 等继承键不误命中、同名 own 键是真失败） |
| [`sag-ui/src/stores/sessionAffinity.test.ts`](../../../../sag-ui/src/stores/sessionAffinity.test.ts) | 新增 store 行为测试（第三轮）：`api.get` stub 为 deferred 队列（网络层是 axios 非 `fetch`），每例 `setActivePinia(createPinia())` 重建 store，覆盖全量拉取、按 model 换桶、作用域失败、重试保留旧错误、`clearError` 只清匹配键、陈旧成功 / 陈旧失败被丢弃、并发 scope 互不写入 |
| [`sag-ui/scripts/alias-loader.mjs`](../../../../sag-ui/scripts/alias-loader.mjs) | 新增测试用 ESM loader（第三轮）：`module.register` + `data:` URL 的 resolve hook，把 `@/` 映射到 `src/` 并给 `src/` 内无扩展名说明符补 `.ts` / `.tsx` / `/index.ts`（`node_modules` 解析不受影响） |
| [`sag-ui/package.json`](../../../../sag-ui/package.json) | `test` 脚本加 `--import ./scripts/alias-loader.mjs`（第三轮），使经别名取数的 store 模块可在无打包器环境下直测；既有测试文件不改 |
| [`sag-ui/src/components/routes/SessionAffinityPopover.vue`](../../../../sag-ui/src/components/routes/SessionAffinityPopover.vue) | 钥匙 icon 用 `Tooltip` 包 `PopoverTrigger`（第三轮改为受控 `:open="(hovering \|\| focused) && !open"`、不传 `disabled`；自管 `hovering`（`pointerenter` 过滤 touch、`pointerleave` 复位）/ `focused`（`:focus-visible` 守卫、`blur` 复位），按钮移除原生 `title`（避免与自绘 Tooltip 双显）、保留 `aria-label`）；新增只读 `TooltipContent`（`side="left"`、`w-64`）；两份列表改用 `sortedRows`，错误 / loading 按本行 scope 取值（`refreshError = errorForModel(store.errors, props.model)`、`loading = loadingForModel(store.loading, props.model)`）并在 `rows.length > 0` 时把错误降级为列表下方非阻塞小字提示（rows 优先渲染，只有 `rows` 为空才走 loading → 同 scope error → 空态）；`expiryLabel` 增加 `<= 0` → `common.expired` 分支；`pinRowClass` / `tooltipRowClass` 分别置灰（浮层 `text-muted-foreground opacity-60`，Tooltip 反色下只用 `opacity-70` 保住对比度）；`HOVER_REFRESH_MS = 3000` + `lastFetchedAt`（在 `refresh()` 的 `finally` 里打点）+ `ensureFresh()`（第三轮去掉全局 `store.loading` 闸、只看时间窗口；第四轮按 scope 补回在途去重，见下） |
| [`sag-ui/src/components/routes/RouteList.vue`](../../../../sag-ui/src/components/routes/RouteList.vue) | active target 单元格：无 active target 且 `route.strategy === "session_sticky"` 时不再渲染 `-` 占位，直接返回 `affinityPins`（仅钥匙 icon）；非 session_sticky 行无 active target 时仍渲染 `-`（审查返工恢复此行为），其 `affinityPins` 为 `null`；有 active target 时的「状态点 + provider」按钮与 Tooltip 不变 |

## 测试

- 新增 `sag-ui/src/lib/sessionAffinity.test.ts` 中五组用例（node test runner，`node:assert/strict`，
  与被测模块同风格的顶层块）：
  - `isPinActive`：`null`（TTL 0，永不过期）→ 活跃；`3600` / `1` → 活跃；`0` / `-30` → 非活跃。
    这条边界钉住「倒计时为 0 的 pin 不得显示为活跃」——把 `> 0` 写成 `>= 0` 会让刚过期、
    下一跳已经变了的 pin 看起来还在生效。
  - `sortPins`：`[expired-a(-5), active-a(100), never(null), expired-b(0), active-b(1)]` 排成
    `[active-a, never, active-b, expired-a, expired-b]`（活跃顶置 + 组内 stable）；同时断言入参顺序
    未变、返回值不是同一个数组引用；退化输入（空列表、全过期、全活跃）保持原序。
  - `mergeSessionEntries`（审查返工补）：`model-a` 桶重拉只换该桶、`model-b` 行数据保留；同一 model
    重拉不残留旧条目；空响应只清该 model 的桶；省略 `model`（含显式 `undefined`）整体替换；两组入参
    均未被修改。这几条钉住「hover 相邻行不得互相覆盖」与「清空某 model 空间不得复活旧快照」。
  - `errorForModel`（审查返工第二轮补、第三轮改签名）：`errors = { "model-a": "boom" }` 时
    `errorForModel(errors, "model-a")` 返回文案、`errorForModel(errors, "model-b")` 返回 `null`（他行失败
    不得盖住本行）；`""` 只被全量视图读取、单行视图读不到全量失败；空映射恒为 `null`；`constructor` /
    `toString` 等继承键不得命中（裸 `errors[model]` 会把 `Function` 当文案），同名 own 键则是真失败。
  - `loadingForModel`（第四轮补）：同 scope 有键且在飞 → `true`；显式 `false` 保持 `false`；他行在途时本行（无该键）
    为 `false`；空映射恒为 `false`；`constructor` / `toString` 等继承键不得误判为在飞，同名 own 键才是真在飞。
    这几条钉住「行 B 的在飞不得让行 A 转圈」与组件取 loading 不再因原型链键恒真。
- 新增 `sag-ui/src/stores/sessionAffinity.test.ts`（审查返工第三轮）：`api.get` 换成 deferred 队列
  （网络层是 axios，不是全局 `fetch`），每例 `setActivePinia(createPinia())` 重建 store，覆盖全量拉取 /
  按 model 换桶 / 作用域失败（只写本 scope 且保留列表）/ 重试保留旧错误 / `clearError` 只清匹配键 /
  陈旧成功与陈旧失败被丢弃 / 并发 scope 互不写入（第四轮补两条：他行在途时本行 `loading` 为 `false`、陈旧响应不触碰 `loading` / `errors`）。支持它的基建是 `sag-ui/scripts/alias-loader.mjs`
  （`module.register` + `data:` URL 的 resolve hook，映射 `@/` → `src/` 并补 `.ts` / `/index.ts`）与
  `package.json` 的 `test` 脚本上的 `--import`；既有测试文件未改。
- 无组件测试：仓库仍无 Vue 组件测试先例（无 @vue/test-utils / jsdom），Tooltip 渲染、节流与
  置灰样式都内联在 SFC 中，只把可单测的存活性判定、排序、换桶合并与作用域错误 / 在途取值抽进
  `lib/sessionAffinity.ts`，store 的网络写入口径则由 `stores/sessionAffinity.test.ts` 覆盖。
- 门禁：`pnpm -C sag-ui test` 全绿（含本次新增用例）；i18n 未新增 key，en/zh 齐平检查不受影响。
- 人工核对：session_sticky 行无 active target 时单元格只剩钥匙 icon（无 `-`）；非 session_sticky
  行仍为 `-`；hover 钥匙 icon 出现左侧预览且不含清除按钮，指针在相邻行之间扫动时不会每行发请求
  （3 秒内复用快照），按 A → B → A 扫过后 A 的预览仍显示自己的 pin（不被 B 的响应刷成空）；键盘
  Tab 聚焦钥匙 icon 也会触发拉取；点击打开浮层时 Tooltip 不再出现且数据重新拉取；断开后端造一次
  `GET /api/session-affinity` 失败 → 预览与浮层仍显示上一次的 pin 列表且浮层置错误态（不被当成空
  列表）；只让行 B 的 `GET /api/session-affinity?model=B` 失败 → 行 A 的预览与浮层仍显示 A 自己的
  pin 与条数（不被 B 的错误文案盖住），B 自己的视图保留上一次列表、错误只在列表下方以非阻塞小字
  提示（审查返工第二轮）；含过期残留的模型空间里活跃 pin 在上、过期项在下且置灰、TTL 显示「已过期」；两条清除路径
  与清除后 active targets 刷新行为与 0009 一致。第三轮补充核对：关闭浮层后指针原地不动再 hover（或 Tab
  移出再移入）预览能重新出现；触屏点按与鼠标点击带来的焦点都不弹预览；行 B 请求挂起时行 A 不转圈；行 A
  刷新失败后再次 hover 触发重试时旧错误不消失、成功后消失；过期行在 Tooltip 为 `opacity-70`、在浮层为
  `text-muted-foreground opacity-60`。

## 文档同步

- `docs/spec/ui/requirements.md`「预期行为 → Route 管理」：修改 0009 的「会话 pin 观测」条目（占位符口径），新增 hover 只读预览、pin 排序与置灰、hover 拉取节流（第四轮补本行在途去重口径）三条；「变更记录」已追加本文件链接。
- `docs/spec/ui/spec.md`：关键文件节补 `SessionAffinityPopover.vue`（受控 Tooltip / `sortedRows` / `HOVER_REFRESH_MS` / `:focus-visible`）、`RouteList.vue`（session_sticky 行无 `-` 占位，非 session_sticky 行仍为 `-`）、`lib/sessionAffinity.ts`（`isPinActive` / `sortPins` / `mergeSessionEntries` / `errorForModel` / `loadingForModel`）、`stores/sessionAffinity.ts`（按 model 换桶、失败保留数据、`errors` / `loading` 按 scope 记账）与 `scripts/alias-loader.mjs`（测试用 ESM resolve hook）；不变式新增「只读预览不得出现清除入口」「活跃判定 `null || > 0`」「hover 节流 + 本行在途去重 + 浮层重拉」「单 model 拉取不得清空其他 model 的 pin 数据、失败不清空」「错误与 loading 按 fetch 作用域记账、他行失败 / 在飞不得影响本行（rows 优先渲染）」「Tooltip 必须受控、不得传 `disabled`」「`errorForModel` / `loadingForModel` 必须用 `Object.hasOwn`」；关键决策「会话 pin 浮层」改写为换桶 + 失败保留 + 作用域记账 + 受控预览的取舍；测试策略（store 测试与 alias loader 基建）与验证场景 12 补 hover / 聚焦预览、置灰项、「B 失败 / 在飞不影响 A」、预览重现与重试保留错误项；「变更记录」已追加本文件链接。
- `docs/spec/CHANGELOG.md` 顶部已插入本条目。
