---
created: 2026-10-25
updated: 2026-10-25
---

# 隐私过滤

## 目标

在请求离开网关之前识别其中的敏感值，按配置脱敏、告警或拒绝；在响应回到客户端时把脱敏值还原成原始明文，使客户端完全无感。默认关闭，关闭时行为与未引入该功能时完全一致。

## 预期行为

### 检测范围

网关检测请求中**所有会发往上游的字符串**，不限于消息正文——把 PII 从 `content` 挪到别的字段无法绕过脱敏：

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
- `allowlist` 条目按**整段精确相等**豁免：命中即不脱敏、不计数；不做子串或前缀匹配。

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

### 事件日志与可观测性

- 三类事件日志（masked / detected / blocked）只记录 `kind=count` 序列（如 `email=1,phone=2`），不记录明文，也不记录占位符。
- 隐私过滤不新增 analytics 记录，也不修改既有 `call_logs` 行的字段；被 `block` 拒绝或 `503` 早退的请求不写 `call_logs`（见「不包含」）。
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

字段定义参见 [配置模块](/spec/config/requirements)。

## 功能边界

### 包含

- 7 个内置检测器 + 自定义正则 + 自定义字面词表
- `allowlist` 整段精确豁免
- `mask` / `warn` / `block` 三种处置模式
- 确定性占位符（`[[KIND_12位hex]]`）
- 非流式与流式（SSE）响应还原
- 引擎初始化失败的 fail-closed `503`
- 配置校验（正则无条件编译）与热重载
- 事件日志（类别 + 计数，无明文）

### 不包含

- **NER / 自由文本语义检测**：人名、地址、组织名等无固定形态的实体不在范围内（v2 预留 trait）。当前只做正则 + 结构校验。
- **句尾紧跟标点的邮箱**：`Adjacency::Wordish` 把 `.` 也算邻接字符，`... alice@example.com.` 这类句末邮箱不会脱敏。这是为压住 URL / 文件名误报而接受的已知权衡（`a@b.com.png`、`logo@2x.png` 不该被当成邮箱）。
- 图片 / 音频等不透明载荷（`data` / `file_data` / `url`）、模型名、`role` / `type` / 工具调用 id、请求头、对象键名的检测。
- 图片生成路径（`/v1/images/*`）的隐私过滤。
- `block` / `503` 早退的 analytics 记录（v1.5 候选）。
- 跨 SSE 帧被劈开的占位符还原：两半都不构成含完整占位符的 JSON 文档，因此都不还原。占位符是加盐摘要、不含明文，属显示瑕疵而非泄露。
- 占位符加密或可逆加密：当前是加盐哈希 + 网关侧内存反向映射。
- 占位符反向映射的跨请求持久化：映射只存活于单次请求；多轮对话靠「客户端重发明文 → 同文本同占位符」保持一致。
- admin API / 管理面板的隐私配置读写端点：仅 TOML 配置 + 热重载。

## 与其它模块的交互

- **配置**：`PrivacyConfig` / `PrivacyMode` / `PrivacyBuiltins` / `PrivacyCustomRegex` / `PrivacyWordList` 定义 schema；`GatewayConfig::validate_privacy` 在加载与保存时校验自定义正则。
- **调度**：`dispatch_chat` 在解构 `DispatchCtx` 后、调用任何 provider 之前执行脱敏（Chat 请求副本 + IR 原地脱敏，不做编解码往返），并在非流式 / 流式出口执行还原。block 与 503 在分发循环之前早退。
- **供应商**：无直接交互。provider 只看到占位符，不感知隐私过滤存在。
- **分析**：隐私过滤不写入 analytics；block / 503 早退发生在 analytics 事件产生之前，因此不产生 `call_logs` 行。
- **日志**：通过 tracing 输出 `info` / `warn` / `error` 三类事件，遵循既有日志模块的滚动与级别约定。

## 变更记录

- [0001 隐私过滤初始化](changes/0001-initial.md)
- [0002 隐私过滤 Minor 项收敛](changes/0002-minor-fixes.md)
