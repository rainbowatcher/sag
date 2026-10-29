---
created: 2026-10-29
updated: 2026-10-29
type: 新增
---

# 0004 — 隐私设置管理面读写

## 动机

隐私过滤此前只有 TOML 配置 + 热重载一条入口：想改一个检测器开关，得手动编辑配置文件；`docs/spec/privacy/requirements.md`
也曾把「管理面读写端点」明确列为不包含。设置页因此没有隐私入口，运营者无法在管理面板里完成「看当前策略、改一处、
确认生效」的闭环。

本次把隐私配置接入既有管理面：设置页新增隐私 tab，经既有 `GET /api/config` / `PUT /api/config` 读取与显式保存。
不新增端点、不新增依赖、不改数据面：`privacy` 只是既有配置快照 / 更新体的又一段，与 backoff、breaker、TTL 共用
同一次 coordinator 事务（apply → validate → 冲突检测 → 原子落盘 → commit）。

三条安全约束决定了实现形状：

- **盐值只写不回读**：任何响应都不含 `salt`（连掩码回显也没有），只暴露 `salt_configured` 布尔；留空提交保持原值，
  非空按原始字节替换，不提供清空入口。
- **落盘只写差异**：保存不整节覆盖 `[privacy]`，未变化的字段、未知键、未知子表与注释一律保留；privacy 未变化时
  连该节都不创建或改写（普通 provider / auth 保存不应物化隐私节）。
- **失败不谎报**：前端只对「确定未提交」的 4xx 明说服务器仍是旧配置；5xx 与网络中断只说「保存未确认」并保留草稿，
  因为既有事务在 commit 之后还有 analytics 身份同步，可能报错但新配置已生效。

## 需求变更

`docs/spec/privacy/requirements.md`：

- **「配置与热重载」**：新增管理面保存走同一条校验与热更新路径（非法正则 / 空 `kind` / 未知 `mode` 在提交阶段拒绝，
  不留半生效配置）；明确配置校验对象与 Schema 只有一份，设置页读写的是同一份 `[privacy]`。
- **新增「预期行为 → 管理面配置」**：读取字段清单（7 个非 salt 字段 + `salt` 是否已配置）；读取失败显示错误与重试、
  不渲染「已关闭」表单；显式保存与页面内草稿（首次读取不 dirty，草稿不进全局状态 / URL / 日志）；保存期间锁定编辑与
  撤销、重复提交被拒；成功后以服务器快照重建表单并清空待替换盐值；失败 / 未确认保留草稿且不自动重读覆盖；
  自定义正则逐行编辑、`kind` / `pattern` 为空拒绝并定位；`warn` 原文出站且不还原、`restore` 只作用于本请求、
  只有 `mask` 依赖 `salt`。
- **新增「盐值」约束**：`salt` 只以「已配置 / 未配置」可见；留空表示保持原值（不改变、不清空），非空按字面替换、
  不 trim，纯空白也算非空；管理面没有清空控件，清空只能手动改配置文件。
- **新增「与 TOML 热载共存」**：管理面保存持久化到配置文件并对后续请求立即生效；手改配置文件仍热重载；两者读写同一份
  配置、无独立副本或固定优先级，以最后一次保存 / 重载为准；保存只改动变化部分，未知键与注释保留。
- **「配置 Schema」**：说明该 schema 同时是配置文件与设置页读写的唯一对象；管理面把 `salt` 折叠为状态。
- **「功能边界 → 包含」**：新增管理面读写、草稿与保存状态、`salt` 状态语义、与 TOML 热载共存四项。
- **「功能边界 → 不包含」**：删除「admin API / 管理面板的隐私配置读写端点：仅 TOML 配置 + 热重载」这条已失效边界；
  新增 `salt` 回显 / 查看 / 复制 / 清空入口、命中统计与审计、规则在线试跑、未保存草稿离开拦截 / 自动保存 / 逐项即时写、
  管理面私有副本或独立优先级。
- **「与其它模块的交互」**：新增「管理 API」条目（快照携带隐私段、`salt` 仅状态；PUT 与既有运行时字段同一次原子更新并
  持久化；提交前拒绝未知 `mode`、非法正则、空 `kind`）与「UI」条目（设置页 tab 读存快照、不缓存 `salt`、草稿与保存
  状态留在页面内）。
- 本模块「变更记录」追加 0004 条目。

## 规格变更

`docs/spec/privacy/spec.md`：

- **关键文件**：新增「管理面」（`admin_config.rs` / `config_mutation.rs`）、「持久化写回」（`save.rs` +
  `privacy_persistence.rs`）、「设置页」（`types/api.ts`、`lib/privacyConfig.ts`、`PrivacyForm.vue`、`SettingsPage.vue`、
  `settingsTabs.ts`、`gatewayConfig.ts`、i18n）三组。
- **核心设计**：新增「管理面读写链路」`sequenceDiagram`（GET → 草稿 / dirty / 必填校验 → PUT 单次
  `update_runtime_config` → validate → 冲突检测 → 差异写回 → commit → 200 / 4xx / 5xx 三分支）、
  「privacy 差异写回」`flowchart`、「表单状态机」`stateDiagram-v2`。
- **关键决策**：新增 10 条，均写明被放弃的方案与原因——复用 `/api/config` + 第五参数（放弃独立 `/api/privacy` 与通用
  `RuntimeConfigUpdate`）；快照显式投影（放弃序列化 `PrivacyConfig`、放弃掩码回显）；`salt` 只在非空时替换（放弃
  `Some("")` = 清空与独立 `clear_salt` 标志）；显式保存 + pending 锁定（放弃逐项即时写 / 自动保存）；失败区分
  「确定未提交」与「保存未确认」；差异字段级写回（放弃整节覆盖）；语义相等不碰该节（放弃每次补默认键）；正则行按完整
  三元组复用、不引入规则 ID、不按 `kind` 唯一（放弃加 `id` 字段与 kind 去重）；校验由 Rust `validate_privacy` 唯一裁决
  （放弃 JS `RegExp` 预校验）；草稿只在初次读取 / 撤销 / 保存成功时由快照重建（放弃 watch store 自动覆盖）。
- **不变式**：新增 13 条可 grep 项，覆盖快照字段集与「`salt` 只以 `salt_configured` 出现」、DTO 不嵌套 `PrivacyConfig`、
  七字段必填与 `salt: Option<String>`、空串 / `None` 保持旧盐、单次 mutate、`providers_rebuilt` 为空、无差异不写、
  只写变化字段、`custom_regexes` 不以 `kind` 唯一、`items` 按位置 round-trip、空 salt 不发 / `pattern` 不 trim、
  行 id 与 `salt_configured` 不进 payload。
- **测试策略**：新增管理面（DTO / HTTP 集成 / file-backed 事务）、持久化、设置页三张表，并补关键场景清单。
- `docs/guide/admin-api.md`：为既有 `GET/PUT /api/config` 章节补 `privacy` 字段说明与安全示例（不新增端点）。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-state/src/config_mutation.rs`](../../../../sag-state/src/config_mutation.rs) | 新增 `PrivacyConfigUpdate`（纯领域命令，无 serde、不 derive `Debug`）；`update_runtime_config` 追加第五参数 `Option<PrivacyConfigUpdate>`，在既有 `mutate` 闭包内把七个非 salt 字段整体替换到 candidate，`salt` 仅在非空时按原字节替换；仍走 apply → validate → 冲突检测 → 原子落盘 → commit |
| [`sag-state/src/lib.rs`](../../../../sag-state/src/lib.rs) | 从 `config_mutation` 重新导出 `PrivacyConfigUpdate` |
| [`sag-admin/src/admin_config.rs`](../../../../sag-admin/src/admin_config.rs) | 新增 `PrivacySnapshot`（七字段 + `salt_configured`，`From<&PrivacyConfig>` 逐字段映射，绝不序列化 `salt`）与 `UpdatePrivacyRequest`（七字段必填、`salt: Option<String>`，无 `Debug`）；`ConfigSnapshot.privacy` 必有、`UpdateConfigRequest.privacy` 可选；`update_config` 映射成命令后只调用一次 coordinator，`AppError::Config` → 400 |
| [`sag-config/src/loader/save.rs`](../../../../sag-config/src/loader/save.rs) | 新增 `PrivacyProjection` / `privacy_on_disk` 与 `write_privacy_section`：语义相等则整节零写入；有差异时按字段写回标量、7 个 builtins、两个词表 `items` 与 `custom_regexes`；支持 `Table` / `InlineTable` 两形态，数组按位置保留顺序与重复项、清空写 `items = []`，regex 行按完整三元组复用其余按位置回填，未知键 / 未知子表 / 注释保持；写回仍只经既有 `atomic_write` |
| [`sag-config/tests/privacy_persistence.rs`](../../../../sag-config/tests/privacy_persistence.rs) | 新增 7 个真实文件 + `load_config` 重载的持久化集成测试 |
| [`sag-admin/tests/privacy_config.rs`](../../../../sag-admin/tests/privacy_config.rs) | 新增 11 个 HTTP 集成测试（GET / PUT / 失败回滚 / 盐值不回显，真实 handler → coordinator → commit 链路） |
| [`sag-state/tests/privacy_config_mutation.rs`](../../../../sag-state/tests/privacy_config_mutation.rs) | 新增 7 个 file-backed 事务测试（落盘 + reload、盐值三态、无 provider 重建、拒绝路径不改状态、外部编辑冲突、写前 IO 失败） |
| [`sag-ui/src/types/api.ts`](../../../../sag-ui/src/types/api.ts) | 新增 `PrivacyMode` / `PrivacyBuiltins` / `PrivacyCustomRegex` / `PrivacyWordList` / `PrivacyConfigFields` / `PrivacyConfigSnapshot`（+`salt_configured`）/ `UpdatePrivacyConfigPayload`（+`salt?`）；`GatewayConfigSnapshot.privacy` 必有，`UpdateGatewayConfigPayload.privacy` 可选 |
| [`sag-ui/src/lib/privacyConfig.ts`](../../../../sag-ui/src/lib/privacyConfig.ts) | 新增纯逻辑模块：`PrivacyDraft` 与 `createPrivacyDraft` / `getPrivacyChanges` / `validatePrivacyDraft` / `buildPrivacyPayload`；词表未编辑时按原 `items` 保真、编辑后才按行 trim 去空，`kind` / `pattern` 比较与发送保留原始文本，空 salt 不 dirty 且不发键 |
| [`sag-ui/src/lib/privacyConfig.test.ts`](../../../../sag-ui/src/lib/privacyConfig.test.ts) | 新增 Node 原生 runner 测试：首次读取 clean、深拷贝隔离、逐字段 dirty、词表保真与编辑归一化、regex 空格保真、salt 发送规则、payload 键集与纯函数性 |
| [`sag-ui/src/components/settings/PrivacyForm.vue`](../../../../sag-ui/src/components/settings/PrivacyForm.vue) | 新增设置页表单：自行处理 loading / GET 失败 / 快照缺 `privacy` 三态；草稿与 `newSalt` 只存在于组件内存；保存中锁定全部编辑与撤销、双击被拒；4xx 定位后端 `privacy.custom_regexes[i]`，5xx / 网络中断保留草稿并提示「保存未确认」；成功用响应快照重建并清空盐输入 |
| [`sag-ui/src/pages/SettingsPage.vue`](../../../../sag-ui/src/pages/SettingsPage.vue) | 在 auth 与 backoff 之间挂载 privacy tab 的 `TabsTrigger` / `TabsContent`，直接渲染 `PrivacyForm`，不复制页面级加载 / 错误外壳 |
| [`sag-ui/src/lib/settingsTabs.ts`](../../../../sag-ui/src/lib/settingsTabs.ts) | `SETTINGS_TABS` 在 `auth` 后、`backoff` 前插入 `privacy`（`?tab=privacy`），默认 tab 仍为 `network`；侧边子菜单复用同一常量自动生效 |
| [`sag-ui/src/lib/settingsTabs.test.ts`](../../../../sag-ui/src/lib/settingsTabs.test.ts) | tab 顺序断言加入 `privacy`，并覆盖 `isSettingsTab("privacy")` / `resolveSettingsTab("privacy")` |
| [`sag-ui/src/lib/i18n/en.ts`](../../../../sag-ui/src/lib/i18n/en.ts) | 新增 97 个隐私词条（`settings.tabs.privacy`、`settings.toast.privacySaved`、`settings.privacy.*`），与 zh 成对同键同占位符 |
| [`sag-ui/src/lib/i18n/zh.ts`](../../../../sag-ui/src/lib/i18n/zh.ts) | 补齐上述 97 个词条的中文值，key / 占位符不变 |

## 测试

后端（focused，实际执行）：

- `cargo test -p sag-state --lib` → 90 passed；其中 4 个纯内存 privacy 用例覆盖七字段落盘、`salt: None` / `Some("")`
  保持与带空格盐原字节替换、privacy 为 `None` 时 runtime-only 更新不动该节且无 provider 重建、非法正则返回
  `AppError::Config`（消息含 `privacy.custom_regexes[0]`）且配置不变。
- `cargo test -p sag-state --test privacy_config_mutation` → 7 passed（`--test-threads=4` 复跑同样通过）：完整七字段
  落盘并 reload 等值、重复词项与重复 `kind` 保序、盐值保持 / 替换、`enabled=false` 下非法正则仍拒、空 `kind` 拒、
  backoff + 非法 privacy 混合不提交、外部编辑返回 `Conflict` 且外部文件保留、写前 IO 失败不改内存 / 版本 / 文件。
- `just test-crate sag-config` → 单元 266 passed、集成 `privacy_persistence` 7 passed：全字段 round-trip、清空词表 /
  规则后无旧项、重复 `kind` 与顺序保真、inline 形态就地更新、table 形态保留注释与未知字段、无关保存不物化 /
  不改写已有 privacy。
- `cargo test -p sag-admin --lib` → 182 passed；其中 7 个 DTO 用例覆盖快照无 `salt` 键与无盐子串、`salt_configured`
  仅对非空盐为 true（纯空白算非空）、七字段缺失即反序列化失败、salt 缺省 / `null` / `""` / 带空格原样透传、
  未知字段容忍而未知 `mode` 拒绝、`privacy` 缺省 / `null` 时不动该节、逐字段映射到命令。
- `cargo test -p sag-admin --test privacy_config` → 11 passed（含分支共 19 次执行）：GET 默认 / 非默认字段全量且无
  `salt` 键；PUT 返回最新快照且不含新旧盐；salt omit / `null` / `""` 保持、非空带空格精确替换；runtime-only PUT
  不动 privacy 与盐；未知字段两级容忍；未知 `mode`、partial privacy → 422；非法正则 / 空 `kind` → 400（`enabled`
  为 true / false 均校验）；混合 backoff + 非法 privacy 整体不提交；上述失败均校验内存与 `routes_version` 未变。
- 局部 `cargo fmt -p <crate> -- --check` 与 `cargo clippy -p <crate> --all-targets -- -D warnings` 在
  `sag-state` / `sag-config` / `sag-admin` 通过。

前端（focused，实际执行）：

- `node --experimental-strip-types --import ./scripts/alias-loader.mjs --test "src/lib/privacyConfig.test.ts"`
  → 1 个测试文件、60 条断言通过：首次读取 clean、双向深拷贝隔离、每字段独立 dirty 与稳定顺序、未编辑词表原样
  round-trip（含重复、空项、含换行项）、编辑后按行 trim 去空、regex 前后空白不算编辑也不被 trim、同 `kind` 多行
  按行 id 定位、空 salt 不 dirty 且 payload 无 `salt` 键、payload 只含七个可写字段、helper 不改入参。
- `settingsTabs.test.ts` → 断言 tab 顺序为 `network, auth, privacy, backoff, breaker, timers, analytics, system,
  appearance`，`privacy` 可解析，默认仍为 `network`。
- `node --experimental-strip-types scripts/check-i18n.mjs` → 704 keys，en / zh 键集与占位符齐平，无 CJK 字面量越界；
  `npx vue-tsc --noEmit` → rc 0。
- `npx oxfmt --check` / `npx oxlint` 对本次新增或修改的 UI 文件通过。

未执行（留给统一收尾）：全仓 `just verify` / `just lint` / `just fmt`、`pnpm -C docs run build`，以及真实浏览器验收
（浅 / 深主题、375 / 768 视口、保存中锁定与失败保留草稿等）。设置页组件没有组件级单测，其行为契约由
`privacyConfig.test.ts` 与上述后端测试覆盖，浏览器验收仍是独立门禁。

## 未涉及

- **另一并发会话的 `save.rs` 加固不计入本变更**：`atomic_write` 保留目标文件权限位、写前读文件 / 解析 / rename 的
  IO 错误统一归 `AppError::Internal`（500），以及相应的权限与错误分类测试，均由另一会话完成。本变更只新增 privacy
  差异写入逻辑，写回仍经既有 `atomic_write` 单一入口，未修改其语义。
- 检测与脱敏行为、占位符形态、三种模式、响应侧还原、fail-closed、`[privacy]` 的 schema 与默认值：均未改动。
- 未新增 `/api/privacy` 端点、隐私专用配置副本、命中统计 / 审计 / 规则在线试跑、`salt` 清空或回显入口、未保存草稿的
  离开拦截。
- `docs/spec/CHANGELOG.md` 与 `docs/spec/ui` 文档不在本次 changefile 范围内（由各自 owner 同步）。
- 既有本地 PII 边界不变：`session_registry` 的明文 carve-out 仍只覆盖该表，管理面快照不新增明文面；设置页操作的是
  配置元数据，不接触消息正文或命中样本，`salt` 明文仍只存在于配置文件（既有配置能力），管理面没有新增读取或存储路径。
