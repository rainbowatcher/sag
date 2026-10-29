---
created: 2026-10-25
updated: 2026-10-29
---

# 隐私过滤

## 目标

在请求离开网关之前识别其中的敏感值，按配置脱敏、告警或拒绝；在响应回到客户端时把脱敏值还原成原始明文，使客户端完全无感。默认关闭，关闭时行为与未引入该功能时完全一致。

## 预期行为

### 检测范围

网关检测请求中**所有会发往上游的字符串**，不限于消息正文——把 PII 从 `content` 挪到别的字段无法绕过脱敏：

> 本模块的承诺边界是**上游出口**：脱敏只作用于发往 provider 的请求内容（以及回到客户端的响应还原）。
> 网关本地管理面另有一处经过批准的 PII 例外（`session_registry` 表，见「与其它模块的交互 → 分析」），
> 它不属于本模块的检测范围。

- 消息 `content`：为字符串时整段检测；为内容块数组时检测其中每个字符串叶子（`{"type": "text"}` 的 `text`、未知块的 `body` 等）。
- `messages[].name`、`messages[].extra` 的每个字符串叶子（`tool_calls[].function.arguments`、`reasoning_content`、`refusal`、`annotations` …）。
- 请求顶层字段：`stop`、`metadata`、`tools`（`function.description`、`parameters` 里的字符串值）、`tool_choice`、`response_format`、`prediction`、`logit_bias`、`moderation`、`web_search_options`、`prompt_cache_key`、`safety_identifier` 等，以及 `extra` 兜底字段（`#[serde(flatten)]`，任意未知顶层字段）。
- 同一请求内 messages 内外一致：同一个明文 + 同一 salt 恒得同一占位符。

两类例外（逐字节保留，不检测）：

- **协议结构**：`model`（路由身份）、`role`、`type`、`id` / `tool_call_id` / `tool_use_id` / `call_id`（工具调用配对）。
- **不透明载荷**：`data` / `file_data`（base64 图片、音频、文档正文）、`url`（图片 URL / data URI）。这些内容不是可读文本，而 base64 里统计上必然出现 IBAN 形状的数字字母串（`[A-Z]{2}[0-9]{2}[A-Z0-9]{10,30}` 约每 25 KB 通过一次 mod-97 校验），扫描会把图片改坏。

对象**键名**不检测（只检测字符串值），因此 JSON schema 的属性名保持原样。

内置检测器（各自可单独开关，默认全开）：

| 类别（kind） | 识别对象 | 结构校验 |
|---|---|---|
| `email` | 常规邮箱地址（非 RFC 全量） | — |
| `phone` | 中国大陆手机号（`1[3-9]` + 9 位数字） | — |
| `id_card_cn` | 中国大陆居民身份证（18 位含校验位 / 15 位旧式） | GB 11643 加权校验位；15 位旧式无校验位，改为要求第 7-12 位 `YYMMDD` 是合法日期（年份读作 `19YY`，按月长与闰年规则校验） |
| `bank_card` | 13–19 位银行卡号 | Luhn |
| `iban` | IBAN 账号 | ISO 13616 mod-97 |
| `api_key` | 已知前缀（`sk-`、`sk-ant-`、`sk-proj-`、`ghp_`、`gho_`、`gsk_`、`xox[baprs]-`、`AKIA`、`AIza`）后接 ≥16 位密钥字符 | — |
| `private_key` | PEM 私钥整块（`-----BEGIN ... PRIVATE KEY-----` 到对应 `END`，含正文） | — |

另有两类自定义检测器：

- `custom_regexes`：正则 + 自定义 `kind` + 可选大小写不敏感。
- `custom_words`：字面词表（大小写敏感，正则元字符按字面处理），类别固定为 `custom_word`。

候选解析规则：

- 相邻字符校验：手机号 / 身份证 / 银行卡 / IBAN 两侧不得紧邻 ASCII 数字；邮箱与 API key 两侧不得紧邻 `[A-Za-z0-9_.+-]`。长数字串中间的「手机号形状」不算命中。
- 重叠候选只保留一个：起点升序，同起点取最长。身份证内含的手机号形状数字只报 `id_card_cn`；同起点的自定义正则取更长的那个。
- 完全相同的区间先看「是否通过了真正的校验位」：通过校验位的规则优先于只过结构过滤的规则，再退回配置顺序。因此 Luhn 合法的 15 位卡号压过 15 位旧式身份证（后者没有校验位）；两者都没有校验位时（如 18 位身份证 vs `bank_card`）仍取配置顺序在前者，内置顺序固定为 `id_card_cn` 先于 `bank_card`。
- `allowlist` 条目按**整段精确相等**（大小写敏感）豁免：命中即不脱敏、不计数；不做子串或前缀匹配。
- 检测对象仅限客户端请求中的可读文本；模型在响应中新生成的内容不检测、不脱敏（只还原本请求产生的占位符，见「响应侧还原」）。

### 脱敏占位符

`mask` 模式下每个命中值替换为 `[[KIND_12位hex]]`：

- `KIND` 为类别大写（`EMAIL`、`PHONE`，自定义 `kind` 原样大写）。
- 12 位 hex 取 `sha256("{kind 小写}:{text}:{salt}")` 的前 12 个字符。**同文本 + 同 salt 恒得同一占位符**，因此同一条消息里重复出现的同一实体只产生一个标记，多轮对话中客户端重发明文时上游看到的仍是同一个标记。
- `salt` 为空时是未加盐哈希；配置 salt 后无法用预计算表反查。
- 占位符本身不含明文。

### 三种模式

| 模式 | 上游收到 | 客户端收到 | 日志级别 |
|---|---|---|---|
| `mask`（默认） | 占位符 | 还原后的原文 | `info` |
| `warn` | 原文（不改写） | 上游原文 | `warn` |
| `block` | 不转发 | `400`（见下） | `warn` |

- `block` 在调用任何 provider 之前返回 `400`：

  ```json
  {"error":{"code":"privacy_violation","message":"request rejected by the privacy filter: sensitive values detected (email=1,...)"}}
  ```

  message 只含「类别=计数」序列，不含明文。
- `warn` 不产生占位符，因此也不做响应还原。
- 未命中任何检测器时三种模式都不产生副作用（无日志、无改写）。

### 响应侧还原

`restore = true`（默认）且本次请求确实产生了占位符时：

- **还原 map 覆盖两次脱敏扫描**：请求侧既脱敏 Chat 请求副本，也原地脱敏 IR。Anthropic / Responses ingress 的 IR 从原始 body 解码，携带 Chat 副本里没有的字段（未知顶层键、`Reasoning` 的 thinking 文本、`ToolResult` 内容），这些字段上的占位符同样在响应里还原——还原 map 是两次扫描结果的合并。
- **非流式**：响应 JSON 中所有字符串值（数组元素、对象字段值；不含对象键）里的占位符递归还原为原文。命中才重新序列化，未命中时响应字节不变。
- **流式**：SSE 逐帧还原。只处理 `data:` 帧，帧内 JSON 的字符串值递归还原；帧边界可跨 chunk——半帧先缓冲，等后续 chunk 补齐后整帧下发。帧终止符按 SSE 规范接受 `\n\n` 与 `\r\n\r\n` 两种，且分隔符本身被 chunk 劈开时（`\r\n\r` \| `\n`）不会提前切帧。未命中的帧（注释行、`event:` / `id:` 行、`data: [DONE]`、非 JSON 载荷、无占位符的 JSON）逐字节透传。
- 流结束时缓冲区里剩余的不完整帧按原样下发，不做还原；这批尾字节先于终态记账下发（见「事件日志与可观测性」）。
- 帧内还原后的明文按 JSON 规则重新转义，含引号 / 换行的原文不会破坏帧结构。
- `restore = false` 时占位符原样透传给客户端。

### 失败处理

- 启用后引擎初始化失败（典型原因：`custom_regexes` 中无法编译的正则）→ **fail-closed**：不转发请求，返回

  ```json
  {"error":{"code":"SERVICE_UNAVAILABLE","message":"privacy engine initialization failed; the request was not forwarded"}}
  ```

  响应体刻意通用，不含正则内容；诊断细节只进日志（`error` 级）。
- 未启用（`enabled = false`，默认）时：不编译引擎、不扫描、不改写响应，行为与未引入隐私过滤时完全一致。

### 配置与热重载

- 整段 `[privacy]` 在每次请求开始时从当前配置快照读取，因此热重载后下一个请求即按新配置生效，无需重启。
- `custom_regexes` 的正则**无论 `enabled` 为何都在配置校验阶段编译**：只在打开开关后才会炸的配置在加载时就被拒绝。错误信息含条目索引与 pattern 原文，`kind` 为空同样报错。
- 管理面保存走同一条校验与热更新路径，因此非法正则 / 空 `kind` / 未知 `mode` 在提交阶段即被拒绝，不会留下半生效的配置；合法保存后下一个请求按新配置运行。
- 配置校验对象与配置 Schema 只有一份：设置页读写的是同一份 `[privacy]`，不存在管理面专属副本。

### 管理面配置

设置页的隐私 tab 经管理 API 的配置快照 / 更新端点读写全局隐私设置，与该端点既有的运行时字段共用同一次读取与同一次保存。

- **读取**：快照包含 `enabled`、`mode`、`restore`、7 个内置检测器开关、自定义正则、自定义词表、`allowlist`，以及 `salt` 的「是否已配置」状态；不包含 `salt` 值本身。
- **读取未成功**：显示错误与重试入口，不渲染「已关闭」的默认表单，也不把读取失败当作隐私过滤已关闭；只有快照确实标记 `enabled = false` 时才显示关闭态。
- **显式保存**：所有改动只存在页面内草稿，用户点击保存后一次提交，不逐项即时写。首次读取不产生未保存改动；草稿不写入全局状态、URL 或日志。
- **保存期间锁定**：保存进行中锁定全部编辑与撤销，重复提交被拒绝；成功或失败后解锁。
- **成功**：新配置对下一个请求生效，无需重启；表单以服务器返回的快照重新初始化，草稿与待替换的盐值一并清空。
- **未确认 / 失败**：保留草稿与用户输入并提示「保存未确认」，不把失败当作已保存，也不自动重读覆盖草稿；用户可显式重试保存或重新读取。网络中断或提交后失败不保证服务端旧配置未变，需重新读取确认。
- **高级设置**：7 个内置检测器可逐项开关（对应上表 `kind`）；自定义正则以「类别 + 正则 + 大小写不敏感」逐行编辑，类别或正则为空时拒绝提交并定位到该行，服务端编译失败同样拒绝；自定义词表按字面匹配，`allowlist` 按整段精确相等（大小写敏感）豁免，二者均逐行编辑。
- **处置与还原语义**：`mode = warn` 原文出站、只记类别计数，不产生占位符也不做响应还原；`restore` 只决定本请求产生的占位符是否还原，关闭时占位符原样透传；只有 `mask` 依赖 `salt`。
- 请求体容忍未知字段；未知 `mode` 被拒绝。

**盐值**：`salt` 只以「已配置 / 未配置」状态可见，任何响应都不回显其值。

- 留空提交表示保持原值（不改变、也不清空）；非空值按字面替换，不 trim、不规范化，纯空白字符也算非空替换值。
- 不提供清空盐值的控件，管理面不会静默清空 `salt`；确需清空时只能手动编辑配置文件。

**与 TOML 热载共存**：管理面保存会持久化到配置文件并立即对后续请求生效，手动编辑配置文件仍按热重载生效。两者读写同一份配置，没有独立副本或固定优先级——以最后一次保存 / 重载为准；保存只改动发生变化的部分，未涉及的字段、未知键与注释保留。

### 事件日志与可观测性

- 三类事件日志（masked / detected / blocked）只记录 `kind=count` 序列（如 `email=1,phone=2`），不记录明文，也不记录占位符。
- 隐私过滤不改写既有 `call_logs` 行的字段；被 `block` 拒绝或 `503` 早退的请求不写 `call_logs`（见「不包含」）。
- **明文落盘的唯一例外**是 `session_registry` 表：`conversation_id`（≤128）与首条消息预览（≤160）存**掩码前**的原始值，仅供本地管理面的日志页会话信息卡读取，有独立保留期（`[analytics] session_retention_days`，默认 7 天，`0` = 永久）。开启脱敏**不会**让占位符进入该表，也不会让原始值进入 `call_logs` / 会话亲和 map / admin 快照。细节见「与其它模块的交互 → 分析」。
- 流式响应的最后一批字节（含隐私尾缓冲的残余帧）先下发给客户端，`call_logs` / analytics 的终态记账随后：客户端看到最后一个字节时，该请求的统计尚未落库。这条顺序是有意的——「请求已结束」不应早于「响应已发完」。

## 配置 Schema

```toml
[privacy]
enabled = false             # 总开关；默认 false
mode = "mask"               # "mask" | "warn" | "block"；默认 "mask"
restore = true              # 响应侧还原；默认 true
salt = "change-me"          # 占位符哈希的盐；空 = 未加盐

# 内置检测器（默认全 true；置 false 单独关闭）
[privacy.builtins]
email = true
phone = true
id_card_cn = true
bank_card = true
iban = true
api_key = true
private_key = true

# 自定义正则检测器；pattern 必须可编译（即使 enabled = false 也在加载时校验）
[[privacy.custom_regexes]]
pattern = 'EMP-\d{6}'
kind = "employee_id"
case_insensitive = false    # 默认 false

# 字面词表与豁免表（默认均为空）
[privacy.custom_words]
items = ["alice@example.com"]
[privacy.allowlist]
items = ["public@example.com"]
```

该 schema 同时是配置文件与设置页（管理面）读写的唯一对象；管理面读取时把 `salt` 折叠为「是否已配置」，不接受对 `salt` 的清空指令。字段定义参见 [配置模块](/spec/config/requirements)。

## 功能边界

### 包含

- 7 个内置检测器 + 自定义正则 + 自定义字面词表
- `allowlist` 整段精确豁免（大小写敏感）
- `mask` / `warn` / `block` 三种处置模式
- 确定性占位符（`[[KIND_12位hex]]`）
- 非流式与流式（SSE）响应还原
- 引擎初始化失败的 fail-closed `503`
- 配置校验（正则无条件编译）与热重载
- 事件日志（类别 + 计数，无明文）
- 设置页 / 管理 API 对全局隐私设置的读取与显式保存：7 个内置检测器开关、自定义正则、自定义词表、`allowlist`、`mode`、`restore`、`enabled`
- 草稿与保存状态：显式保存、保存期间锁定、成功后生效并重置表单、未确认错误保留草稿
- `salt` 只暴露「是否已配置」状态：留空保持原值、非空按字面替换、不提供清空
- 管理面保存与 TOML 热载共存（同一份配置、同一套校验）

### 不包含

- **NER / 自由文本语义检测**：人名、地址、组织名等无固定形态的实体不在范围内（v2 预留 trait）。当前只做正则 + 结构校验。
- **句尾紧跟标点的邮箱**：`Adjacency::Wordish` 把 `.` 也算邻接字符，`... alice@example.com.` 这类句末邮箱不会脱敏。这是为压住 URL / 文件名误报而接受的已知权衡（`a@b.com.png`、`logo@2x.png` 不该被当成邮箱）。
- 图片 / 音频等不透明载荷（`data` / `file_data` / `url`）、模型名、`role` / `type` / 工具调用 id、请求头、对象键名的检测。
- 图片生成路径（`/v1/images/*`）的隐私过滤。
- `block` / `503` 早退的 analytics 记录（v1.5 候选）。
- **本地管理面持久化的原始会话身份与首条消息预览**（`session_registry`，见下节）：这是经批准的 PII carve-out，不在本模块的掩码覆盖范围内，日志页的会话预览因此可能显示原文。
- 模型在响应中新生成的敏感文本：只检测客户端请求里的文本，响应侧仅还原本请求产生的占位符。
- 跨 SSE 帧被劈开的占位符还原：两半都不构成含完整占位符的 JSON 文档，因此都不还原。占位符是加盐摘要、不含明文，属显示瑕疵而非泄露。
- 占位符加密或可逆加密：当前是加盐哈希 + 网关侧内存反向映射。
- 占位符反向映射的跨请求持久化：映射只存活于单次请求；多轮对话靠「客户端重发明文 → 同文本同占位符」保持一致。
- `salt` 明文回显、查看、复制或清空入口（只显示状态；清空需手动改配置文件）。
- 命中统计、审计记录、规则在线试跑 / 预览或命中样本回放。
- 未保存草稿的离开拦截、关闭提醒、自动保存或逐项即时保存。
- 管理面专属的私有隐私配置副本或独立优先级规则。

## 与其它模块的交互

- **配置**：`PrivacyConfig` / `PrivacyMode` / `PrivacyBuiltins` / `PrivacyCustomRegex` / `PrivacyWordList` 定义 schema；`GatewayConfig::validate_privacy` 在加载与保存时校验自定义正则。
- **管理 API**：`GET /api/config` 的配置快照携带隐私段（`salt` 仅状态），`PUT /api/config` 的隐私段与既有运行时字段在同一次原子更新中提交并持久化；未知字段透传，未知 `mode`、非法正则、空 `kind` 在提交前拒绝。
- **UI**：设置页的隐私 tab 读取 / 保存上述快照，不缓存 `salt` 值；草稿、保存中锁定与失败提示都留在页面内，不进入全局 store 或 URL。
- **调度**：`dispatch_chat` 在解构 `DispatchCtx` 后、调用任何 provider 之前执行脱敏（Chat 请求副本 + IR 原地脱敏，不做编解码往返），并在非流式 / 流式出口执行还原。block 与 503 在分发循环之前早退。
- **供应商**：无直接交互。provider 只看到占位符，不感知隐私过滤存在。
- **分析**：隐私过滤本身不写入 analytics；block / 503 早退发生在 analytics 事件产生之前，因此不产生 `call_logs` 行。
  唯一例外是 `session_registry`（[analytics 0009](../analytics/changes/0009-session-registry-and-logs-session-envelope.md)，文档化 PII carve-out）：
  `conversation_id` 与 `first_message_preview` 存**掩码前**的原始值（≤128 / ≤160 字符），仅供本地管理面的
  日志页会话信息卡经 `GET /api/logs?session=` 读取，独立保留期（`session_retention_days`，默认 7 天，`0` = 永久），
  不参与 `call_logs` 轮转。采集点在掩码之前（`chat/mod.rs` 的 `registry_request` / `registry_ir`），因此开启 `mask`
  也不会把 `[[KIND_h12]]` 占位符写进注册表。这是 `sag-core/src/session.rs` 中「No PII ever leaves this type unhashed」
  不变式的**唯一**文档化例外；脱敏的承诺范围仍是「发往上游的内容」。
- **日志**：通过 tracing 输出 `info` / `warn` / `error` 三类事件，遵循既有日志模块的滚动与级别约定。

## 变更记录

- [0001 隐私过滤初始化](changes/0001-initial.md)
- [0002 隐私过滤 Minor 项收敛](changes/0002-minor-fixes.md)
- [0003 隐私边界与 `session_registry` PII 例外](changes/0003-pii-carve-out-boundary.md)
- [0004 隐私设置管理面读写](changes/0004-admin-settings.md)
