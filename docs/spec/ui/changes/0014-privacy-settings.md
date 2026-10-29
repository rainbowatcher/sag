---
created: 2026-10-29
updated: 2026-10-29
type: 新增
---

# 0014 — 设置页隐私过滤配置

## 动机

后端已把隐私过滤配置接进管理面（`GET|PUT /api/config` 的 `privacy` 节），但管理面板里没有
任何入口：想开邮箱脱敏、想把处置模式从 `warn` 改成 `block`，只能手改 `gateway.toml` 再等热
重载。运维需要的是一个能看见「现在到底是什么状态、我改了什么还没生效」的设置表单。

本次在 Settings 页新增「隐私过滤」tab，把整节隐私配置收进一个显式保存的表单：总开关、处置
模式、响应还原、7 个内置检测器、自定义敏感词 / 放行名单 / 自定义正则、盐值替换，外加只读的
保护范围说明。UI 只做读写，不新增检测能力，也不改变既有网关语义。

## 需求变更

对应改动已落在 [`docs/spec/ui/requirements.md`](../requirements.md) 与
[`docs/spec/ui/spec.md`](../spec.md)：

- `requirements.md` 新增「隐私过滤设置」小节：入口与构成（`?tab=privacy`，位于「API 鉴权」
  之后、「重试退避」之前，侧边栏子菜单按同一顺序带出）、当前生效配置 / 草稿双摘要卡、基础
  设置（开关 / 三模式 / 响应还原）、内置检测规则（7 个，`已选 n / 7`）、自定义规则与例外
  （可折叠）、高级（盐值）、保存 / 撤销 / 失败、读取与不可用三态、沿用既有外壳。
  「功能边界 → 包含」补隐私 tab 与显式保存两项；「不包含」补「在线试跑 / 命中统计 / 审计报表
  / 演示开关」「盐值回显与清空控件」「客户端正则语法校验」「草稿跨会话持久化与路由离开拦截」
  「顶级导航入口 / 独立页面路由」；「与其它模块的交互」把管理 API 条目扩到 `GET|PUT
  /api/config`，并新增「隐私过滤」条目（单次原子更新、成功后下一个请求即生效、读取侧只有
  `salt_configured`）。
- `spec.md` 关键文件补 `PrivacyForm.vue`、`privacyConfig.ts`、`settingsTabs.ts`、
  `gatewayConfig.ts`、`SettingsPage.vue` 与 `types/api.ts` 的隐私类型；核心设计新增「隐私设置
  的草稿与保存」流程图；关键决策新增四条（显式保存 + 双份快照 vs 即时写、草稿只存组件内 vs
  全局 store、只判必填 vs 复刻 Rust regex、盐值只写不回显）；不变式新增七条（三态不降级、
  纯函数不改入参、未编辑词表保真、正则不 trim、空盐值不下发、保存中全局锁定、入口只经设置
  tab 表）；测试策略新增 `privacyConfig.test.ts` 的 6 条必钉场景。
- 两文件的「变更记录」均已追加 0014。隐私模块侧的端点与写路径由
  `docs/spec/privacy/changes/` 的对应变更记录承担，本文件只覆盖 UI 侧。

## 前端实现要点

**入口只有一处**：`privacy` 作为 `SETTINGS_TABS` 的一项插在 `auth` 与 `backoff` 之间，
`value` 即 `?tab=` 值，`key` 指向 `settings.tabs.privacy`。`SettingsPage` 与 `SidebarNav`
都渲染同一常量，因此没有「隐私」硬编码导航项，也没有新页面路由。

**布局沿用已认可的静态原型**：标题 + 描述 → 「当前生效配置 / 草稿」两张只读摘要卡（状态下
拉出启用状态、模式、内置检测器 `n / 7`、响应还原、盐值，草稿侧另有已同步 / 存在未保存更改
badge）→ 基础设置（总开关、三选一模式卡、仅 `mask` 可见的响应还原开关）→ 内置检测卡（7 行，
每行名称 + 等宽 `kind` + Info tooltip + Switch，右上计数）→ 两个原生 `<details>`：默认折叠的
「自定义规则与例外」（敏感词、放行名单、正则行编辑）与「高级」（盐值）→ 只读保护范围 4 条 →
`sticky bottom-0` 页脚（草稿 badge + 撤销 + 保存）。模式语义照需求原话呈现：`mask` 标推荐并
给占位符样例，`block` 说明「被拒请求不会到达上游」，`warn` 卡内醒目警示「原文仍会发送给上游」。

**显式保存 + 双份快照**：`saved` 是最近一次服务器确认的快照（`JSON.parse(JSON.stringify())`
深拷贝，绕开 reactive proxy 无法 `structuredClone` 的问题），`draft` 一律由
`createPrivacyDraft(saved)` 生成；`draft !== null` 后不再因 store 更新而重建，因此其它表单保存
或后台刷新都不会覆盖未保存编辑。dirty 由 `getPrivacyChanges(draft, saved).length > 0` 判定，
组件内不重写比较逻辑。

**保存与锁定**：`locked = localSaving || store.saving`，期间开关 / 模式 / 输入 / 增删行 / 撤销
全部 `disabled`，两个 handler 各自再 guard 一次，双击不会产生第二次 `PUT`。payload 在 `await`
之前由 `buildPrivacyPayload` 生成（整节 7 字段全量、无 UI 行 id、空盐值不带 `salt` 键）。
成功用响应里的 `privacy` 重建 `saved` + `draft`（盐值输入随重建清空）并 toast
`settings.toast.privacySaved`；失败保留 `draft` 与 `newSalt`，显示
`saveFailed{message}`（后端原文直出）+ `saveUnconfirmedHint`，不宣称「服务器一定没变」。

**盐值只写不回显**：界面只读 `salt_configured` 渲染「已配置 / 未配置」，`newSalt` 只存在于组件
局部状态（不进 store / localStorage / URL / 日志），输入框 `type="password"` +
`autocomplete="new-password"`；留空即不下发 `salt`（保持不变），填写任意非空值按字面替换，
草稿摘要显示「待替换」，不提供清空控件。

**校验分工**：前端 `validatePrivacyDraft` 只判 `kind` / `pattern` 必填（按行 id 定位，类别标识
允许重复，不做语法校验），错误就地渲染并 focus 首错；正则合法性最终由网关按 Rust regex 方言
编译校验（无 lookaround / 反向引用），后端 `privacy.custom_regexes[i]` 报错用正则取出 0 基下标
→ 对应行显示 1 基序号 + 后端消息原文。

**三态不降级**：组件自取 `gatewayConfig` store，并自行处理「首次加载中」/「`GET` 失败」/
「快照缺 `privacy`（旧网关）」三种情况，分别渲染 loading、`loadError` + `common.retry`、
`unavailable` + 同一 retry，均**不**伪造一份 `enabled=false` 的可编辑表单；因此挂载处不再套
页面级 `v-if loading / v-else-if error` 外壳（否则会闪两次占位）。

**不做的事**：不提供规则在线试跑、命中统计 / 审计报表、演示开关，也不新增主题、侧栏、顶级
导航或路由离开拦截；未保存草稿不写 localStorage。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-ui/src/lib/settingsTabs.ts`](../../../../sag-ui/src/lib/settingsTabs.ts) | `SETTINGS_TABS` 在 `auth` 与 `backoff` 之间插入 `{ value: "privacy", key: "settings.tabs.privacy" }`，`DEFAULT_SETTINGS_TAB` 与两个守卫函数不变 |
| [`sag-ui/src/lib/settingsTabs.test.ts`](../../../../sag-ui/src/lib/settingsTabs.test.ts) | 顺序断言加入 `privacy`，并新增 `isSettingsTab("privacy")` / `resolveSettingsTab("privacy")` 两例 |
| [`sag-ui/src/types/api.ts`](../../../../sag-ui/src/types/api.ts) | 新增 `PrivacyMode` / `PrivacyBuiltins` / `PrivacyCustomRegex` / `PrivacyWordList` / `PrivacyConfigFields` / `PrivacyConfigSnapshot`（只带 `salt_configured`）/ `UpdatePrivacyConfigPayload`（`salt` 可选），并给 `GatewayConfigSnapshot` / `UpdateGatewayConfigPayload` 加 `privacy` |
| [`sag-ui/src/lib/privacyConfig.ts`](../../../../sag-ui/src/lib/privacyConfig.ts) | 新增纯逻辑模块：`PrivacyDraft` + `createPrivacyDraft`（深拷贝、`newSalt` 恒空、行 id `saved-<i>`）/ `getPrivacyChanges`（逐字段 dirty，`salt` 恒最后）/ `validatePrivacyDraft`（只判必填，按行 id）/ `buildPrivacyPayload`（整节 payload，去行 id 与 `salt_configured`，空盐值不写键） |
| [`sag-ui/src/lib/privacyConfig.test.ts`](../../../../sag-ui/src/lib/privacyConfig.test.ts) | 新增 node test runner 直接断言：首次 clean、双向深隔离、逐字段 dirty、未编辑词表保真、编辑归一化、正则必填与空格保真、同 kind 多行按行 id 定位、盐值空不发 / 空格原样、payload 只含协议字段、三个 helper 不改入参（10 块 / 60 条 `assert.*`） |
| [`sag-ui/src/components/settings/PrivacyForm.vue`](../../../../sag-ui/src/components/settings/PrivacyForm.vue) | 新增隐私设置表单：已确认 / 草稿双摘要、三模式卡、7 个内置开关、两个折叠区块（词表 / 正则 / 盐值）、只读保护范围、`sticky` 撤销 + 保存条，自带 loading / 读取失败 / 缺 `privacy` 三态与保存中全局锁定 |
| [`sag-ui/src/pages/SettingsPage.vue`](../../../../sag-ui/src/pages/SettingsPage.vue) | 在 `auth` 之后插入 `TabsTrigger value="privacy"`，并在同位置加 `TabsContent value="privacy" class="mt-0 data-[state=inactive]:hidden" force-mount` 挂载 `<PrivacyForm />`（不套页面级 loading / error 外壳）；`force-mount` 让面板切到其它 tab 时仍挂载、仅靠 `data-[state=inactive]:hidden` 隐藏，所以切 tab 前后 `draft` / `newSalt` 都保持，只有离开设置页（组件销毁）才丢弃 |
| [`sag-ui/src/lib/i18n/en.ts`](../../../../sag-ui/src/lib/i18n/en.ts) | 新增 97 个 key（95 个 `settings.privacy.*` + `settings.tabs.privacy` + `settings.toast.privacySaved`），总计 607 → 704 |
| [`sag-ui/src/lib/i18n/zh.ts`](../../../../sag-ui/src/lib/i18n/zh.ts) | 同步同一批 97 个 key 的中文译文，键集与占位符与 en 对齐 |
| [`docs/spec/ui/requirements.md`](../requirements.md) | 新增「隐私过滤设置」小节，更新「功能边界」包含 / 不包含与「与其它模块的交互」，变更记录追加 0014 |
| [`docs/spec/ui/spec.md`](../spec.md) | 关键文件、核心设计流程图、四条关键决策、七条不变式、测试策略与变更记录同步 |

## 测试

focused 结果（本次变更内实际执行，均在 `sag-ui/` 下）：

- `node --experimental-strip-types --import ./scripts/alias-loader.mjs --test
  "src/lib/privacyConfig.test.ts" "src/lib/settingsTabs.test.ts"` → `tests 2 / pass 2 / fail 0`
  （`privacyConfig.test.ts` 内 60 条 `assert.*`，`settingsTabs.test.ts` 输出
  `all assertions passed`）。helper 覆盖 dirty 顺序、词表保真、正则原串、空盐值不发键与纯函数
  不改入参；tab 测试钉住 `privacy` 在 `auth` 之后、`backoff` 之前。
- `npx oxfmt --check` / `npx oxlint`（本次新增与改动的 `lib/privacyConfig*`、`lib/settingsTabs*`、
  `components/settings/PrivacyForm.vue`、`types/api.ts`）→ 格式化通过、lint rc 0。
- `npx vue-tsc --noEmit`（`just ui-typecheck`）→ rc 0；期间曾因 i18n key 尚未落地报过
  `settings.tabs.privacy` 不在 `TranslationKey`，key 补齐后消失。
- `node --experimental-strip-types scripts/check-i18n.mjs`（`just ui-i18n-check`）→
  `✓ 704 keys`，en/zh 键集与占位符齐平，`src/` 无 CJK 字面量。
- `node ... --test "src/**/*.test.ts"`（`just ui-test`）→ 11 个测试文件全通过。

尚未验证（不在本次变更内，留给集成阶段）：

- **真实浏览器核对**：亮 / 暗主题、375 / 768 / 1024 下长文案换行与 `sticky` 页脚、radio 键盘
  方向键、三态与失败提示的实际呈现，均未在浏览器中走查。
- **全量门禁**：`just verify-ui` / `just fmt` 未在并行期间运行；`SettingsPage.vue` 的挂载只做过
  文件级核对，未经类型检查 / 测试复核。
