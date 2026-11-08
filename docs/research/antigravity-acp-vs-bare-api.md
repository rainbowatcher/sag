# 官方 ACP 路线 vs 裸 API 路线：Antigravity 双路线深入对比

> **调研日期**：2026-11-08（北京时间 / UTC 同日）
> **面向**：SAG（Smart AI Gateway）接入决策
> **续篇**：承接 [Antigravity 的合规边界与合规路径限制：官方 ACP 路线与官方 API key 替代](./acp-to-api-antigravity.md)（下称「首份报告」，v2 合规重构版）。本文只做**两条具体路线**的对比，不重复首份报告已展开的通用限制；凡标「见首份报告 ……」者不再复述。
> **时效性说明**：本文涉及的对象处于**日更级**变动状态。`agy` CLI 在 2026-11-04 ~ 11-07 三天内发了 1.5.0 / 1.5.1 / 1.5.3 三个版本；政策文本与 release note 之间已出现**互相矛盾**（见 §3.1、§7 第 1 条）。**本文结论的有效期不超过 30 天**，任何「模型范围」「政策口径」判断在使用前必须重读一手页面。
> **标注约定**：`[官方]` = Google 一手文档 / release note / 论坛官方回复；`[协议]` = agentclientprotocol.com 规范原文；`[项目]` = 转换层或客户端项目自述 / issue / PR；`[社区]` = 用户帖（附可信度）；`未验证` = 无法交叉核实，不得当作确定事实使用。

---

## 1. 摘要

**两条路线各一句话**：

- **A 官方 ACP 路线** = 把 Google 官方 ACP 服务器（CLI 内置 `agy acp`，或 ACP Registry 的独立二进制 `antigravity-acp`）当子进程驱动，用 **Agent Client Protocol**（JSON-RPC 2.0 over stdio）说 **agent 语义**。它是**唯一有官方书面许可**的路线，但语义方向与 OpenAI 相反、能力被政策与二进制双重阉割、单位经济性最差（~330 MB RSS + 5–6 s 冷启 + 1.5 GB 首次下载）。
- **B 裸 API 路线** = 自持 OAuth refresh token 直连 Cloud Code `v1internal`，用 **Gemini 形状的 JSON / SSE** 说 **模型语义**。它是**功能最全、成本最低、语义最接近 `/v1/chat/completions`** 的路线，但被 ToS §6 **明令禁止**，且 2026-11-06 已出现**凭证级 + 服务级**的 `TOS_VIOLATION` 封禁。

**一句话结论**：两条路线都不是「正经 provider」。A 合规但被阉割且翻译伤筋动骨（等于在 SAG 里内嵌一个 ACP 客户端 + MCP 桥）；B 功能全但政策上是死路。

### 1.1 对比总表（15 个维度）

| # | 维度 | A 官方 ACP 路线 | B 裸 API 路线 |
|---|---|---|---|
| 1 | **合规定性** | 「permitted」（2026-11-02 官方答复），前提：未修改的官方二进制 + 原生 `oauth-personal` + 单用户交互式开发 | **明文禁止**（ToS §6 + FAQ 两条），已有 4 轮封禁潮与凭证级 `TOS_VIOLATION` |
| 2 | **上游看到的身份** | 真客户端本身（指纹问题消失） | 伪造的「官方客户端」，UA + TLS + 会话头 + 设备指纹全靠复刻 |
| 3 | **模型范围** | 政策文本：仅套餐内 Gemini；技术现状：1.5.1 起 `agy acp` 对全部 ACP 客户端开放账号下全部模型（两口径直接冲突，见 §3.1） | 账号下全部模型（Gemini / Claude / GPT-OSS），但受档位 entitlement 与 UA 版本 gate 双重裁剪 |
| 4 | **认证** | 5 种原生 auth method（`oauth-personal` / `oauth-business` / `gemini-api-key` / `agent-platform` / `terminal-login`），token 刷新由 runtime 自管 | 桌面二进制内嵌 client_secret 的 OAuth PKCE，refresh token 由网关自己保管与刷新 |
| 5 | **工具语义** | **方向相反**：agent 自己执行工具，客户端只收到 `tool_call_update` 展示流；唯一注入通道是 MCP（实测有坑） | 原生 `functionDeclarations`，语义与 OpenAI `tools` 一致（但有 schema 子集 + 签名链约束） |
| 6 | **多模态** | 图片上下文可用；媒体类工具结果在 1.4.5 上退化成文本（registry #741） | base64 inline 可用；长会话中会被压缩逻辑直接丢弃 |
| 7 | **usage / 计量** | **硬缺失**（#1045 open，三种独立实测一致）→ 只能估算或填 0 | 部分有（`usageMetadata` + `cachedContentTokenCount`），但 `thoughtsTokenCount` 与 `reasoning_tokens` 不等价 |
| 8 | **流式语义** | 逐 chunk 的 `session/update` 通知，**无 `finish_reason` 中间态**，只有终局 `stopReason` | Gemini 风格 SSE，可映射（裸 API 侧 6 类转换缺陷明细已移出首份报告，见其「边界外的路线」） |
| 9 | **会话所有权** | **服务端持有** `sessionId` + 历史（`session/load` 重放、`session/resume` 不重放）；与客户端 `messages[]` **双写** | 协议层无状态（每请求全量 `contents[]`），但需伪造「身份连续性」与签名链 |
| 10 | **取消 / 中断** | RC01 上 `session/cancel` 返回 `-32601`，只能 teardown + `session/load`；1.5.1 才修 `cancelled` 语义 | 断连接即可 |
| 11 | **审批语义** | `session/request_permission` 是**反向请求**，需人回答；Antigravity 还有 `interaction_` 提问滥用 → 无头网关**不可解** | 协议里**没有**这个位置（无审批概念） |
| 12 | **运行成本** | ~1.5 GB 首次下载 + 冷启 5–6 s（Windows ~16 s）+ 常驻 ~327 MB RSS/进程 + stdio 单管道 | 一次 HTTPS 往返（网关自身开销仅数十 MB） |
| 13 | **并发** | `session/prompt` 是长事务，业界实现普遍串行化（`one prompt per provider at a time`）；并发 = 多进程 = 内存线性增长 | 每请求独立，可水平扩展（受配额与账号粘性限制） |
| 14 | **失效形态** | 二进制缺陷（SIGILL / 分块截断卡死 / stdin EOF 不退）+ 策略收紧 | 协议漂移（每月至少一次破坏性变更）+ 封号（凭证级 403） |
| 15 | **对 SAG 的接入形态** | 子进程生命周期管理 + ACP 客户端实现 + MCP 桥 + 会话映射表 → **新增一类 provider 运行时** | 一个 OpenAI 兼容 `base_url` + headers/body overrides → **零新类型**（见首份报告「架构契合度」） |

> 第 15 行本身就是结论的一半：**低成本接入的是不能接的那条，能接的那条要新造一整套运行时。**

---

## 2. 两条路线是什么

### 2.1 A：官方 ACP 子进程（两条腿，同源不同分发）

**形态**：编辑器（或网关）把官方 ACP 服务器作为**子进程**拉起，通过 **stdio 上的 NDJSON JSON-RPC 2.0** 通信，客户端发起 `initialize` / `session/new` / `session/prompt`，服务端用 `session/update` 通知回流内容 —— `[官方]` [Agent Client Protocol (ACP) | Antigravity CLI Docs](https://antigravity.google/docs/cli/acp/)、[Other editors (ACP)](https://antigravity.google/docs/ide/extensions/acp/)。

**腿 1：CLI 内置 `agy acp`（活跃腿）**
- `acp` 子命令直到 **1.5.0（2026-11-04）** 才进 `agy --help`；1.5.1（11-05）、1.5.3（11-07）连续跟进 —— `[官方]` [antigravity-cli release 1.5.0](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.0)、[release 1.5.1](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.1)、[releases 列表](https://github.com/google-antigravity/antigravity-cli/releases)。
- 文档确认的可调面：Model = 「The models available to your account」；Thinking level = `Low`/`Medium`/`High`/`Extra`/`Max`；Modes（5.0 起）= `default`/`accept-edits`/`plan`；flags 含 `--add-dir`、`--agent`、`--sandbox`、`--dangerously-skip-permissions`、`--process-per-session`、`--ide-mcp-port`、`--login`、`--log-file`、`--disable-telemetry` —— `[官方]` [Other editors (ACP)](https://antigravity.google/docs/ide/extensions/acp/)（查阅 2026-11-08）。

**腿 2：ACP Registry 独立二进制 `antigravity-acp`（慢腿）**
- `[官方]` [registry/antigravity-acp/agent.json](https://raw.githubusercontent.com/agentclientprotocol/registry/main/antigravity-acp/agent.json)（2026-11-08 拉取原文）：`"version": "1.4.5"`，`"authors": ["Google LLC"]`，`"license": "proprietary"`；构件从 `https://dl.google.com/agy-extensions/releases/{macos,linux,windows}/agy-acp-server-1.4.5-*.zip` 分发；Linux 两个架构带 `"args": ["--uid="]`，cmd 为 `./agy_acp_server.par`（Windows 为 `./agy_acp_server.exe`）。
- 版本时间线（registry 提交历史可见区）：1.0.0（#542，2026-08-20）→ 1.1.1（#567，09-03）→ 1.2.1（#627，09-23）→ 1.3.0（#647，10-02）→ 1.4.0（#684，10-14）；**1.4.5 的提交不在可见区，发布时间未验证**（但 11-01 的 registry issue #741 已在用 1.4.5 darwin-arm64）—— `[项目]` [agent.json 提交历史](https://github.com/agentclientprotocol/registry/commits/main/antigravity-acp/agent.json)。
- **没有公开 CHANGELOG**（构件是 proprietary `.par`），可核实的版本差异只能从 issue 反推。

**关键差异（决定选哪条腿）**：`agy acp` **继承** `agy` CLI 已缓存的登录态（文档原文：uses the credentials you cached with an interactive `agy` session），registry 二进制**不继承**（acpx：a separate download and sign-in from the Antigravity IDE and CLI）—— `[官方]` 同上文档；`[项目]` [openclaw/acpx `agents/Antigravity.md`](https://github.com/openclaw/acpx/blob/main/agents/Antigravity.md)。而 1.5.x 平均 1–2 天一发、registry 从 1.0.0 到 1.4.0 跨 8 周 —— **若评估 A 路线，应优先 `agy acp`**。两者是否共享同一份 `model_selection.py` / 同一 CCPA 代理层：**未验证**。

### 2.2 B：裸 API 直连 `v1internal`（token 重放，全部 2api 项目走的路）

**形态**：网关自己完成 OAuth（Authorization Code + PKCE，桌面二进制内嵌 client_secret 不可省），把 refresh token 存下来，然后**冒充官方客户端**直接对 Google 上游发 HTTPS 请求 —— 这与 `language_server` / `agy` 自己发出的请求走同一条后端。

- **端点**：`https://daily-cloudcode-pa.googleapis.com`（consumer 默认）、`https://cloudcode-pa.googleapis.com`（prod/enterprise）—— `[项目]` [aphrody-antigravity `endpoints.rs`](https://docs.rs/aphrody-antigravity/latest/src/aphrody_antigravity/endpoints.rs.html)（注释称 reverse-engineered from the Antigravity 2.0.1 client and confirmed against its live OAuth flow）。
- **方法**：`POST /v1internal:loadCodeAssist`、`:fetchAvailableModels`、`:onboardUser`、`:generateContent`、`:streamGenerateContent?alt=sse`、`:countTokens`、`:retrieveUserQuotaSummary`。
- **信封**：`{project, model, request:{contents, systemInstruction, generationConfig, tools}, userAgent, requestId}`，响应把 `GenerateContentResponse` 包在 `response` 字段下 —— `[项目]` [cortexkit/antigravity-auth `ANTIGRAVITY_API_SPEC.md`](https://github.com/cortexkit/antigravity-auth/blob/main/packages/opencode/docs/ANTIGRAVITY_API_SPEC.md)（v1.0，2025-12-13，Verified by Direct API Testing）。
- **认证**：client_id `1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com`，scopes `cloud-platform`/`userinfo.email`/`userinfo.profile`/`cclog`/`experimentsandconfigs`，`POST https://oauth2.googleapis.com/token` 刷新（`expires_in≈3599`）—— `[项目]` [aphrody `auth.rs`](https://docs.rs/aphrody-antigravity/latest/src/aphrody_antigravity/auth.rs.html)。
- **UA 是准入门槛不是装饰**：必须带 `{"metadata":{"ideType":"ANTIGRAVITY"}}` 才路由到 Antigravity tier，且上游按 UA **版本号** gate 模型可用性（同一 token 同一 payload，`2.2.1` → 404，`2.9.1` → 200）—— 见首份报告「边界外的路线」（该路线已按合规范围移出）。

**生态事实**：同类项目的公开盘点里，**16 个项目中的 8 个**是这条路线（其余为 4 个本地 LS 桥接、2 个 CLI 包装、2 个凭证工厂）——该盘点已按合规范围移出首份报告（见其「边界外的路线」）；`[项目]` [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI)（57,916★）与 [su-kaka/agy2api](https://github.com/su-kaka/agy2api)（5,318★）是其中的工程标杆。它们全部把这层翻译做完了——**这正是 B 路线「功能全」的来源：它是在逆向一个完整的 Gemini 协议面。**

---

## 3. 官方 ACP 路线的限制（深挖主体）

### 3.1 模型可用性：三方口径冲突，且根因是 `clientInfo.name` 白名单

这一格是**本文最重要、也最不稳定的一格**。三个一手来源在同一周内给出三种口径：

| 来源 | 日期 | 说法 | 出处 |
|---|---|---|---|
| **CLI release note 1.5.1** | 2026-11-05 | "Changed `agy acp` to offer **every model available to your account to every ACP client**, where previously only a fixed list of editors were offered models other than Gemini." | `[官方]` [release 1.5.1](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.1) |
| **官方文档（ACP 页）** | 查阅 2026-11-08 | "Gemini models are available in every client. **Third-party models are only offered to JetBrains IDEs (including JetBrains Air), Android Studio, Android Performance Analyzer, Zed, and Xcode.**" | `[官方]` [Other editors (ACP)](https://antigravity.google/docs/ide/extensions/acp/) |
| **论坛官方答复** | 2026-11-02 | "in general ACP clients outside of first-party IDE integrations, **model selection through `antigravity-acp` is scoped only to the Gemini models included in your plan**." | `[官方]` [discuss #197131](https://discuss.ai.google.dev/t/conflicting-guidance-on-using-t3-code-with-personal-google-ai-subscriptions/197131) |

三种解释，**未验证**哪一种成立：(a) 文档滞后于 1.5.1；(b) 文档那句在讲 registry 独立二进制（但该页正文讲的是 `agy acp`）；(c) Google 有意保留两套口径（技术放松 / 政策不放）。

**根因（源码级确诊）**：限制不在鉴权层，而在 **client 名字白名单**。

`[项目]` CLI issue [#1318](https://github.com/google-antigravity/antigravity-cli/issues/1318)（2026-10-22，open）给出源码原文：

```python
def supports_third_party_models(client: client_info_lib.ClientInfo) -> bool:
    """...only offered to clients confirmed to render them correctly..."""
    return (client.is_jetbrains or client.is_android_studio or client.is_zed or client.is_xcode)
```

同 issue 的对照实验（1.3.0，同账号 / 同二进制 / 同协议版本 / 同 capabilities / 同空工作区，**只改 `clientInfo.name`**）：

| `clientInfo.name` | 广告模型数 | 选 Claude 的结果 |
|---|---|---|
| `t3-code` | 11（仅 Gemini） | `-32602` 拒绝 |
| `zed` | 18（+6 个 Claude 5.5 档位 + `gpt-oss-120b`） | 接受，`claude-sonnet-5-5-low` 完成了一轮 prompt |

`[项目]` CLI issue [#1168](https://github.com/google-antigravity/antigravity-cli/issues/1168)（2026-10-05 创建，labels `ACP`/`1.4.4`/`ACP-1.3.0`/`ACP-1.4.1`，open）独立复现：`oauth-personal` 下 `session/new` 只广告 11 个 Gemini；`set_config_option{configId:"model", value:"claude-opus-5-5-high"}` → `-32602`，stderr 为 `Rejecting model … not available under the active auth method.`；而**同进程同 OAuth** 下 `fetchAvailableModels` 返回 Claude/GPT-OSS 且 `remainingFraction: 1`，`agy` CLI 同账号能跑 `claude-opus-5-5-high`。确诊评论（RomuloDrumond，2026-10-22）："The cause is the client name, not the auth method."

**两个可穿透手法，代价完全不同**：

1. **`clientInfo.name: "zed"` 伪装**：协议层**完全合法**（`clientInfo` 只是元数据），1.3.0 / 1.4.1 都验过，11 个模型变 18 个。但这是**对政策口径的直接违背**（11-02 答复的「仅 Gemini」正是指「通用 ACP 客户端」）。**协议可行 / 政策高危。**
2. **`customModels` 旁路**：`[项目]` t3code [#15378](https://github.com/pingdotgg/t3code/issues/15378)（2026-10-03 → 10-21 closed，label `upstream`）里，用户用 registry 实例（1.4.1）+ 手动 `customModels` 塞入 `claude-opus-5-5-{low,medium,high}` 等，**Opus 5.5 High / Sonnet 5.5 High 完成轮次并跑通了 shell/write/edit 工具**；GPT-OSS 报 `No capacity available`。即过滤主要发生在「目录广告」层，强制 `set_config_option` 对**已授权**模型有时能穿透。1.4.5 上是否仍可穿透：**未验证**。

**还有两条硬约束**：

- **模型目录与二进制版本硬绑定**：`[项目]` t3code [#19150](https://github.com/pingdotgg/t3code/issues/19150)（2026-10-13 → 11-01 closed）标题即结论——"Antigravity provider never lists Gemini 4 Argon (bundled ACP runtime 1.3.0 predates it)"。**registry 落后 = 新模型看不见**，而 registry 的更新节奏是周级。
- **1.0.0 即使解开滤镜也跑不了 Claude**：`[项目]` registry [#554](https://github.com/agentclientprotocol/registry/issues/554)（2026-08-28，**仍 open，Google 零回应**）作者读分发包内 Python 源，证明去过滤后还需 **5 处 provider 兼容改造**（`maxOutputTokens` 按 CCPA 广告上限裁剪；`parametersJsonSchema` → `parameters` 并递归删 `$schema`；给 Gemini `functionCall`/`functionResponse` 补配对 ID 以便 Anthropic 侧生成 `tool_use`/`tool_result`；GPT-OSS 去掉不支持的 generation 字段；保留 32768 输出上限）。结论句："RC01 is missing both catalog exposure and provider-specific request compatibility."

> **对首份报告的修正点**：首份报告把「仅 Gemini」归因于 RC01 时代的 `if not ccpa_id.startswith("gemini"): continue` 过滤器；**现行（已验证上限 1.3.0 / 1.4.1，1.4.5 未验证）根因是 `supports_third_party_models()` 的 `clientInfo.name` 白名单**。见 §7 第 1 条的未验证边界。

### 3.2 协议语义硬约束（对「转 OpenAI」而言）

本节是本文的技术核心：**ACP 不是「另一种 chat 格式」，而是另一套执行模型。** 规范原文均取自 `[协议]` https://agentclientprotocol.com/protocol/v1/ （查阅 2026-11-08）。

#### 3.2.1 session 生命周期：请求-响应 + 服务端持有历史

- `session/new` **必须**带绝对路径 `cwd` 与 `mcpServers`，返回服务端生成的 `sessionId`（Antigravity 是 UUID）—— `[协议]` [Session Setup](https://agentclientprotocol.com/protocol/v1/session-setup)。→ OpenAI 侧没有 cwd、没有 MCP、没有 sessionId；SAG 必须自造「请求 → 哪个 ACP 会话」的映射键。
- `session/load` **重放整段历史**（`user_message_chunk` / `agent_message_chunk` 成对、全文一次性重发而非原始 delta）；`session/resume` 不重放（1.5.0 新增）。→ 若网关把 OpenAI 的 `messages[]` 当权威历史，就与 ACP 侧历史 **双写**；两者不一致（客户端截断、编辑历史、多客户端并发）时只能二选一。
- `session/close` 幂等；`session/prompt` 的**响应体只有一个 `stopReason`**（`end_turn` / `max_tokens` / `max_turn_requests` / `refusal` / `cancelled`）—— `[协议]` [Prompt Turn](https://agentclientprotocol.com/protocol/v1/prompt-turn)。
- **steering**：ACP 允许在一轮进行中继续发 prompt，语义是「插话修正该轮」而不是「开新轮」；Antigravity 文档明确如此。→ OpenAI 客户端并发发两条消息时期望两条独立 completion，这里会变成插话。
- **cancel**：协议有 `session/cancel` 通知，但 RC01 实测 **`-32601 Method not found`** —— `[项目]` [ACP Protocol Reference](https://cdn.jsdelivr.net/npm/@estebanforge/pi-antigravity-bridge@1.4.7/docs/ACP-PROTOCOL-REFERENCE.md)（verified LIVE on 2026-09-03）。1.5.1 才修了「关闭/重载/恢复会话时正在跑的 prompt 以 `cancelled` 结束而不是挂住」—— `[官方]` [release 1.5.1](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.1)。→ 「用户点停止」在 1.4.x 上要么杀进程重建、要么等它跑完。

#### 3.2.2 14 种 `session/update` 的 OpenAI 映射表

现行 schema 共 **14** 种（口径已从「13 种」更新）；`[协议]` [Prompt Turn](https://agentclientprotocol.com/protocol/v1/prompt-turn)。

| `sessionUpdate` | 含义 | OpenAI `chat.completion.chunk` 对应位 | 结论 |
|---|---|---|---|
| `user_message_chunk` | 用户消息流 / 回放 | 无（`role:user` 只在请求侧） | 丢弃或塞扩展字段 |
| `agent_message_chunk` | 助手文本增量 | `choices[0].delta.content` | ✅ **唯一无损映射** |
| `agent_thought_chunk` | 推理增量 | 无（`reasoning_content` 是 DeepSeek 系方言） | 需自定义字段；RC01 上很稀疏，多数推理混在正文里 |
| `tool_call` | 新工具调用 | `delta.tool_calls[]`（部分） | ⚠️ 语义反了（见 3.2.4） |
| `tool_call_update` | 工具状态/结果变化 | 无 | 仅展示；塞 `acp` 扩展对象 |
| `plan` | 执行计划 | 无 | 丢弃 / 扩展 |
| `available_commands_update` | 斜杠命令集 | 无 | 丢弃 |
| `current_mode_update` | 模式变化 | 无 | 丢弃（但影响后续行为） |
| `config_option_update` | 会话配置变化 | 无 | 丢弃 |
| `session_info_update` | 标题 / 时间 | 无 | 丢弃 |
| `usage_update` | 上下文占用 + 累计成本 | 只有终局 `usage`，**字段结构不同** | ❌ 见 3.2.6 |
| `notice` | 需 client 声明 `session.notices` | 无 | Antigravity 未使用（未验证） |
| `compaction_update` | 压缩实体（需 `session.compaction` 能力） | 无 | ❌ |
| `compaction_summary_chunk` | 压缩摘要增量 | 无 | ❌ |

> **文档滞后是已知坑**：`[项目]` agent-client-protocol [#1694](https://github.com/agentclientprotocol/agent-client-protocol/issues/1694)（2026-07-16 → 09-23 closed）标题即 "Docs: prompt-turn page lists 5 SessionUpdate variants, schema v1 defines 11"。实现时**以 schema 为准，不以散文文档为准**。

#### 3.2.3 `request_permission`：反向请求 + Antigravity 特有的提问滥用

- `[协议]` [Tool Calls §Requesting permission](https://agentclientprotocol.com/protocol/v1/tool-calls)：结构为 `{sessionId, toolCall: ToolCallUpdate, options: PermissionOption[]}`，`option.kind` ∈ `allow_once` / `allow_always` / `reject_once` / `reject_always`；client 回 `{"outcome":{"outcome":"selected","optionId":…}}` 或 `{"outcome":{"outcome":"cancelled"}}`。
- **协议不定义超时**——谁等谁由实现决定。Antigravity 1.5.1 的修复项反证阻塞真实存在："permission requests and question forms now close when they are answered elsewhere or their prompt ends, instead of blocking later prompts or receiving their answers."（`[官方]` release 1.5.1）
- **Antigravity 特有：`interaction_` 前缀的「固定选项提问」**—— `[项目]` [openclaw/acpx `agents/Antigravity.md`](https://github.com/openclaw/acpx/blob/main/agents/Antigravity.md)：Antigravity sends fixed-choice user questions through `session/request_permission` with an `interaction_` tool-call ID. **Their options represent answers, not tool approvals.** acpx 即使开 `--approve-all` 也一律取消并上报「需要用户回答」。
  → **这是协议滥用**。无头网关若按「自动批准」处理，会把第一个选项当成用户的答案（语义完全错误）；若按「拒绝」处理，会话会挂住。`[项目]` 同款处理见 [t3code PR #9348](https://github.com/pingdotgg/t3code/pull/9348)（2026-09-03 merged）：routes the agent's `interaction_` permission requests to T3's user input questions.

#### 3.2.4 工具执行模型方向相反（最大的架构性不兼容）

`[协议]` [Tool Calls](https://agentclientprotocol.com/protocol/v1/tool-calls) 开头即写："Tool calls are actions the language model asks the **Agent** to perform… **The Agent executes them on the model's behalf.** Agents run tool calls themselves."

三层能力方向：

1. **agent → 自己执行**：读文件、跑命令、编辑（Antigravity 默认，`kind: read/edit/execute`，`content` 带 `diff`）。
2. **agent → client 反向请求**：`fs/read_text_file`、`fs/write_text_file`、`terminal/create|output|wait_for_exit|kill|release`（需 client 在 `initialize` 里声明 `clientCapabilities.fs` / `terminal`）。
3. **client → agent 注入工具**：**只有 MCP**（`session/new.mcpServers`），没有「OpenAI 式 tools」。

冲突是语义级的：OpenAI 的模型输出 `tool_calls` 后**客户端执行**并回填 `role:"tool"`；ACP 的模型输出的是 agent 内部工具，**agent 自己执行完**，client 只收到展示流——**客户端拿不到「模型想调你的函数」这个信号**。

而且**关掉 client 能力并不能让 agent 停手**：`[项目]` 实测（RC01，`fs:false, terminal:false`）"the agent self-executes its own tools and asks via `session/request_permission`"（ACP Protocol Reference）。

**唯一合法注入通道是 MCP，且实测有坑**：`session/new` 的 `mcpServers` 接受 `http`/`sse`/stdio（RC01 实测接受 `{"name","type":"http","url","headers":[]}`；`headers` 是**列表**；**死 URL 也接受**，连接是懒的）。要暴露 OpenAI `tools`，网关只能侧起一个 MCP server，把 tools 包成 MCP tools，再靠 `_meta.mcp.tool` 反查。已知坑：

- MCP 工具的 `rawInput` 多一层 `{arguments:{...}}` 信封；
- `title` 是 `_`；
- `rawOutput` 常是**展示字符串而不是结果**；
- MCP 结果「out-of-band」到达模型，client 侧看不到（同上 ACP-PROTOCOL-REFERENCE phase-2）。

已知转换项目对这条约束的三种应对，无一优雅：

| 项目 | 做法 | 代价 |
|---|---|---|
| `[项目]` [EvilFreelancer/acpbox](https://github.com/EvilFreelancer/acpbox/blob/main/docs/api-mapping.md) | 把非文本 update 作为**非标准 `acp` 字段**塞进 chunk | 标准客户端看到「没有 tool_calls 的纯文本」，工具能力全丢 |
| `[项目]` [chaojimct/cli-agent-gateway](https://github.com/chaojimct/cli-agent-gateway/blob/main/README.zh-CN.md) | ACP `session/update` → 统一 IR → SSE；「Client tools — OpenCode-style clients: ACP permission → OpenAI `tool_calls`」 | 只在**特定 agent 的 permission 语义**上近似；对「agent 自执行」仍不可见，且未适配 Antigravity 特例 |
| `[项目]` openclaw/acpx、t3code | 不转 OpenAI，只做 ACP 客户端（把 fs 能力反代给 agent） | 不解决 SAG 的问题 |

#### 3.2.5 modes / thinking levels 被烘进模型 slug

- `[协议]` [Session Modes](https://agentclientprotocol.com/protocol/v1/session-modes)：两套并行机制——旧 `modes`（`session/set_mode` + `current_mode_update`）与新 `configOptions`（`session/set_config_option`，`type:"select"`）；文档明确 "Dedicated session mode methods will be removed in a future version"。
- Antigravity RC01 的 `session/new` 同时给两套：`modes.availableModes = default | auto_edit | yolo`；`configOptions = [{id:"model"}, {id:"mode"}]`（ACP-PROTOCOL-REFERENCE）。
- **thinking level 不是独立字段，而是烘进模型 slug**：`gemini-3.8-flash-low|medium|high`、`claude-opus-5-5-high`、`gpt-oss-120b-medium`。→ OpenAI 可以用一个 `model` 字符串承载，代价是**SAG 的模型名与官方 slug 绑死，Google 改名即断**（#19150 是活例）。
- 1.5.x 把 thinking level 单列为会话选项（`Low/Medium/High/Extra/Max`），且「每个 session 保留自己的 model 与 thinking level」（`[官方]` release 1.5.0）——与 RC01 的 slug 方案**不同**，说明这块接口在 1.4 → 1.5 期间变过；**1.5.x 的确切 payload 未验证**。
- **plan 模式在 RC01 的 ACP 上不存在**：`[项目]` 实测 "ACP has NO review-only mode. The three modes (default/auto_edit/yolo) are permission modes only; the CLI's `--mode plan` has no ACP equivalent."，且 `/plan` 斜杠命令会被 server 拦截并**直接写文件**（`create_file` + `edit_file` 进 `~/.gemini/antigravity-acp/brain/`）。1.5.0 号称加了 `plan` 会话模式，**行为是否变成只读未验证**。
- **模型校验发生在服务端且在构造 agent 之前**（#554），所以网关不能「先发请求再失败」，必须从 `session/new` 的目录里取模型 ID。

#### 3.2.6 usage 硬缺失

- `[协议]` `usage_update` 载荷是 `{used, size, cost?{amount,currency}}` —— **上下文窗口占用 + 累计成本**，不是单轮 prompt/completion token；2026-06-05 起稳定（`[协议]` [Session usage stabilized](https://agentclientprotocol.com/announcements/session-usage-stabilized)）。单轮 token（`PromptResponse.usage`）是**另一个尚未合并的 RFD**。
- **Antigravity 两者都不发**，三种独立实测一致：
  - `[官方]` CLI [#1045](https://github.com/google-antigravity/antigravity-cli/issues/1045)（2026-09-18，open，点名 1.1.1）："`session/prompt` returns only `stopReason`. There is no `usage` in the response and no `usage_update` notification during the session." 配额也拿不到，只能旁路跑 `agy --print /usage --output-format json` 或读 server 的 SQLite（"which are not an interface"）。
  - `[项目]` 独立实测（RC01，2026-09-03）："NO usage/token fields appeared anywhere in `initialize`, `session/new`, `set_config_option`, `agent_message_chunk`, prompt results, or tool flows. **Gate B FINAL: ABSENT on RC01.**"
  - 1.4 / 1.5 是否补上：**未验证**（#1045 仍 open，1.5.x release notes 无此项）。
- 转换项目的现实做法：acpbox「`usage` is zero if ACP does not provide token counts」；`[项目]` [pingu1m/acp-to-api](https://raw.githubusercontent.com/pingu1m/acp-to-api/main/README.md)「Token usage in responses is estimated from text length」；pi-antigravity-bridge「token display shows client-side ESTIMATES」。
  → **SAG 若在 A 路线上做计费/配额统计，数据源只能是估算或另行旁路**。这与 B 路线「至少还有 `usageMetadata`」形成对比（见 §5 第 7 行）。

### 3.3 认证与运行细节

| 项 | 事实 | 来源 |
|---|---|---|
| auth method | `oauth-personal` / `oauth-business` / `gemini-api-key` / `agent-platform`（前身 vertex-ai，仍接受）/ `terminal-login` | `[官方]` [Other editors (ACP)](https://antigravity.google/docs/ide/extensions/acp/) |
| settings.json | `<GEMINI_HOME>/antigravity-acp/settings.json`，形如 `{"auth":{"type":"oauth-personal"}}`；**启动时读一次**，运行中改无效（实测：错误持续到重启） | `[项目]` ACP-PROTOCOL-REFERENCE；[acpx](https://github.com/openclaw/acpx/blob/main/agents/Antigravity.md) |
| 未认证错误 | `session/new` → `-32000 "Authentication required"`，文案直接列出 4 种 method 与 settings.json 路径 | `[项目]` 同上 |
| GEMINI_HOME | 账号 profile 根目录，**默认 `~/.gemini`，不是 `$HOME`**；且「只设 `GEMINI_HOME` 不实现拒绝浏览器登录的策略」，策略要自己实现 | `[项目]` acpx |
| 凭据存储 | `AGY_ACP_FORCE_FILE_STORAGE=1` 强制 profile 内文件存储；默认可能走 keychain（#1045 称 token 在 macOS keychain，只有 `agy_acp_server.par` 能读） | `[项目]` acpx；`[官方]` #1045 |
| token 文件 | `~/.gemini/antigravity-acp/acp_token.json`（0600，约 510 B）；**跨进程重启有效**（新进程 `session/load` 无需再 authenticate） | `[项目]` ACP-PROTOCOL-REFERENCE run 6 |
| 是否继承 `agy` 登录态 | registry 二进制：**否**；`agy acp`：**是** | `[项目]` acpx；`[官方]` 文档 |
| `oauth-personal` 流程 | 起 `127.0.0.1:<随机端口>` loopback + 组 OAuth URL + 尝试开浏览器；client_id `1071006060591-…`，scope `cloud-platform + userinfo.email + aicode`，`access_type=offline`、`prompt=consent`、PKCE S256 | `[项目]` ACP-PROTOCOL-REFERENCE |
| 无头环境 | 1.0.0：**不打印 URL**，只能靠 `BROWSER` 脚本或假 `xdg-open` 截获；1.4+：T3 称 The official agent prints its OAuth URL as one plain stdout line。**分界点未验证** | `[项目]` 两处说法冲突 |
| `--uid=` | registry `agent.json` 给 linux-x86_64 / linux-aarch64 的 `args: ["--uid="]` | `[官方]` agent.json |
| token 刷新 | **runtime 自己管**（有 `access_type=offline` 的 refresh token）；凭据过期时可能**直接触发交互式登录**（acpx 警告 "Expired credentials may otherwise start interactive sign-in"）→ 网关**无正当接口介入刷新** | `[项目]` acpx；ACP-PROTOCOL-REFERENCE |
| 配置不持久 | 重启后 `session/load` 回来的 `currentModelId` 是默认值，model/effort/mode **必须由驱动重新 set** | `[项目]` ACP-PROTOCOL-REFERENCE run 6 |
| 服务端持久化 | `~/.gemini/antigravity-acp/conversations/<sessionId>.db`（SQLite WAL，7 表，`steps.step_payload` 是不透明 blob）；"Never poll these DBs" | `[项目]` 同上 |

**一条容易被误引的边界**：FAQ 的「Can I use Antigravity CLI headless mode in third-party coding agents」→「not supported under consumer plans」（`[官方]` [FAQ](https://www.antigravity.google/docs/faq/)）。**`agy acp` 不是 headless 模式**（它是 ACP server），这条 FAQ 不直接命中 A 路线，但它是 `-p` 包装路线的红线。**两者在合规上不等价——这是 A 路线唯一的政策优势，也是它存在的全部意义。**

### 3.4 运行成本：硬数据

| 维度 | 数据 | 来源 |
|---|---|---|
| 首次获取 | **~1.5 GB** `.par`（原文："a ~1.5 GB server binary downloaded from Google"）；registry zip 另含 `localharness_external` | `[项目]` [pi-antigravity-bridge ENGINES.md](https://cdn.jsdelivr.net/npm/@estebanforge/pi-antigravity-bridge@1.6.2/docs/ENGINES.md) |
| 冷启动（Linux） | spawn → `initialize` **5–6 s**（页缓存热，三次一致） | `[项目]` ACP-PROTOCOL-REFERENCE |
| 冷启动（Windows） | 1.1.1 上 **~16 s / 每次 spawn**（帖标题即结论） | `[社区]` [discuss #183427](https://discuss.ai.google.dev/t/acp-server-1-1-1-official-agy-acp-server-cold-starts-in-16s-on-every-windows-spawn/183427) |
| 常驻内存 | 稳态 **RSS ~327 MB**（5 min 混合负载；VSZ 5.3 GB 是 TCMalloc 预留） | `[项目]` ACP-PROTOCOL-REFERENCE |
| 进程模型 | 默认**一进程 N 会话**；隔离用 `--process-per-session`（每会话独立 worker，**HTTP MCP 必需**） | `[官方]` [Other editors (ACP)](https://antigravity.google/docs/ide/extensions/acp/) |
| 通信 | stdio 上的 **NDJSON JSON-RPC 2.0，单管道**（请求-响应 + 通知混流；1.5.1 才修「迟到 update」） | `[项目]` + `[官方]` release 1.5.1 |
| 并发 | `session/prompt` 是**长事务**；`acp-to-api` 直接写 "Only one prompt runs per provider at a time (serialized via prompt lock)"；并发上限 = 内存/CPU，且**共用同一 CCPA 身份** | `[项目]` acp-to-api README |
| 升级 | 二进制替换（1.5.x 每 1–2 天一发）；运行中进程需持租约（t3code 实现） | `[官方]` releases；`[项目]` t3code |

**二进制缺陷（不是配置问题）**：

| 缺陷 | 现象 | 来源 |
|---|---|---|
| 启动 SIGILL | 1.4.1 / 1.4.5 上 `colossus::Feature::Feature+2171` 的 absl `DieBecauseNull`，非 SIMD；**与 CPU 亲和性相关**，12 次交错 A/B 可复现（单一项目记录，无第二方复现） | `[项目]` [tiezbro/paseo-agy-acp](https://github.com/tiezbro/paseo-agy-acp)（唯一深挖官方内核缺陷的项目） |
| 分块响应截断卡死 | 1.4.5 在 `session/new` 读 `fetchAvailableModels` 时因分块响应截断而卡死（Windows 桌面完全不可用） | `[项目]` 同上 |
| stdin EOF 不退出 | Windows 1.4.0：stdin EOF 后进程继续运行 | `[社区]` [discuss #189880](https://discuss.ai.google.dev/t/windows-acp-server-1-4-0-remains-running-after-stdin-eof/189880) |
| 会话永久卡死 | ACP 会话报 502 "model unreachable" 后**永久卡死**，新会话正常 | `[社区]` [discuss #196254](https://discuss.ai.google.dev/t/acp-sessions-become-permanently-wedged-with-502-the-model-is-currently-unreachable-while-fresh-sessions-succeed/196254) |
| ARM64 崩溃 | 39-bit VA 的 TCMalloc 崩溃，**官方承认是已知问题、正在修**（2026-11-02 答复） | `[官方]` [discuss #197131](https://discuss.ai.google.dev/t/conflicting-guidance-on-using-t3-code-with-personal-google-ai-subscriptions/197131) |

> 这五条里没有一条能靠「改配置 / 加重试」解决。对网关的含义：**A 路线的失败模式在进程与二进制层，不在 HTTP 状态码层**——SAG 现有的「4xx 不重试 / TTFB 超时可重试」语义完全覆盖不到它。

### 3.5 合规现状：唯一「permitted」，但边界模糊

| 时间 | 事件 | 来源 |
|---|---|---|
| 2026-10-26 | 首次「开口」：跑 ACP Registry 里**未经修改的** `antigravity-acp` 二进制 + 原生 `oauth-personal` 可以；仍封「抽取或代理 OAuth token」「把 `agy -p` 包进第三方 agent 循环」「自动化多账号轮换」 | `[官方]` [discuss #192027](https://discuss.ai.google.dev/t/tos-clarification-is-t3-code-official-antigravity-acp-agent-permitted-under-the-antigravity-terms-of-service/192027/2) |
| 2026-10-30 | Blyck 请求对「非编辑器类 ACP client」的书面确认（设计极保守：不改二进制、SHA-256 + Google LLC 签名校验、不读不存不转 token、只请求 Gemini）——**截至 2026-11-08 无任何答复** | `[官方]` [discuss #196885](https://discuss.ai.google.dev/t/tos-clarification-desktop-app-blyck-using-the-official-antigravity-acp-binary-over-acp-with-the-users-own-personal-plan/196885) |
| **2026-11-02** | 把 10-26 与 10-30 两条矛盾表态合并：**「is supported and permitted under our Terms of Service」**，但**模型范围限 Gemini**；封号范围是「your Antigravity and/or Gemini CLI service access rather than disabling your entire Google Account」 | `[官方]` [discuss #197131](https://discuss.ai.google.dev/t/conflicting-guidance-on-using-t3-code-with-personal-google-ai-subscriptions/197131) |
| 2026-11-05 | **1.5.1 对全部 ACP 客户端开放全部模型**（晚于上述答复 3 天，**未附任何政策说明**） | `[官方]` [release 1.5.1](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.1) |

**关键对照**：本次检索**未发现任何「仅跑官方 ACP 二进制被封」的案例**；现存封号案例全部集中在 **token bridge / `-p` 包装**型（如 `[官方]` [discuss #198478](https://discuss.ai.google.dev/t/appeal-antigravity-agy-cli-disabled-for-tos-403-requesting-manual-review/198478)、[#142192](https://discuss.ai.google.dev/t/appeal-for-account-disabling-unexpected-tos-violation-via-3rd-party-pi-dev-agent-oauth-bridge/142192)）。

> **这就是 A/B 两条路线最大的差异**：B 路线是**明文 breach + 有执行案例**；A 路线是**明文 permitted + 无执行案例，但模型范围写死在 Gemini**。风险从「账号资产」转移到「功能边界与口径漂移」。

---

## 4. 裸 API 路线的限制（精炼复用）

本节只列「与 A 路线形成对照」的骨架；**裸 API 侧的技术细节已按合规范围移出首份报告（其只保留「边界外的路线」范围声明），本节不再复述**。

1. **ToS 明文禁止 + 点名工具**：§6 "Using third party software, tools, or services to access the Service (e.g. using OpenClaw with Antigravity OAuth) is a breach of this Agreement."；FAQ 点名 Claude Code / OpenClaw / OpenCode，并建议改用 Vertex 或 AI Studio API key —— `[官方]` [Terms](https://antigravity.google/terms/)、[FAQ](https://www.antigravity.google/docs/faq/)。**见首份报告「合规边界地图」§3.1 明文禁止区。**
2. **封禁已到凭证级 + 服务级**：2026-11-06 `403 PERMISSION_DENIED / reason: TOS_VIOLATION`（带 `appeal_url`），**只封 Cloud Code 一个服务**，账号登录/token 刷新/模型目录/订阅全部正常；3 个凭证 2 个中招 —— `[项目]` [CLIProxyAPI #7296](https://github.com/router-for-me/CLIProxyAPI/issues/7296)。**见首份报告「合规边界地图」§3.1 明文禁止区（执法记录）。**
3. **多层检测**：TLS / JA3-JA4、会话头（`x-machine-id`、`x-vscode-sessionid`）、`requestId` 格式、`enabledCreditTypes`、`thought_signature`、设备指纹、**bootstrap 一致性**（OAuth 与 `loadCodeAssist` 的枚举/字符串不一致即可标记账号）—— `[项目]` [9router #1138](https://github.com/decolua/9router/issues/1138)（功能请求，清单**未验证**）。**见首份报告「边界外的路线」（检测层明细已移出合规范围）。**
4. **假 429（内容级 WAF）**：`systemInstruction` 里含特定字面文本（`<system-conventions>` + RFC 2119 列表、`You are Codex, an agent based on GPT-5.`）→ 返回**假的** `429 RESOURCE_EXHAUSTED`，而配额完好、官方 App 同账号正常；插零宽字符即恢复 200 → 字面/词法级匹配 —— `[项目]` [CLIProxyAPI #5751](https://github.com/router-for-me/CLIProxyAPI/issues/5751)。**见首份报告「边界外的路线」（该类指纹明细已移出合规范围）。**
5. **`thoughtSignature` 签名链**：多轮工具调用必须原样回传，任何改写即 400；只有「网关内维护签名存储」或「哨兵绕过」两条路 —— `[项目]` [Antigravity-Manager #3529](https://github.com/lbjlaq/Antigravity-Manager/issues/3529)、[CLIProxyAPI PR #3470](https://github.com/router-for-me/CLIProxyAPI/pull/3470)。**见首份报告「边界外的路线」（签名链明细已移出合规范围）。**
6. **参数失真与静默降级**：Gemini 3.6 Flash 及之后 `temperature`/`topP`/`topK` **静默失效**、penalty 系列报错；`max_tokens` 硬顶 64000；模型被静默替换只给一行 warning —— `[官方]` [Content generation parameters](https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/capabilities/content-generation-parameters)（2026-11-07）。**见首份报告「边界外的路线」（参数失真明细已移出合规范围）。**
7. **每月破坏性漂移**：UA 版本 gate（#5175）、字段白名单（#118）、端点身份绑定（#4749：consumer 只能打 `daily`）、模型下线日程（2026-11-18）—— 四类在 2026 年 2–11 月各至少发生一次。**见首份报告「边界外的路线」（漂移明细已移出合规范围）。**

**一句话**：B 路线的每一条限制**都能在代码里绕过去**（代价是持续工程投入），但**没有一条能在政策上被容忍**。

---

## 5. 逐维度深入对比

### 5.1 合规风险：**这是两条路线最大的、也是唯一不可调和的差异**

| 项 | A 官方 ACP | B 裸 API |
|---|---|---|
| 官方文本 | "is supported and permitted under our Terms of Service"（附条件） | "is a breach of this Agreement"（§6） |
| 附条件 | 未修改的官方二进制 + 原生 `oauth-personal` + 单用户交互式开发 + **仅 Gemini** | 无任何可满足条件（FAQ 建议改用 Vertex / AI Studio key） |
| 已有执行案例 | **未发现**（检索到 2026-11-08） | 四轮：2026-02 / 10-21 / 11-03 / 11-05 / **11-06 凭证级** |
| 封禁粒度 | 未观测；官方称「你的 Antigravity / Gemini CLI 服务访问」 | 服务级 + 凭证级（同账号其余服务正常） |
| 风险形态 | **口径漂移**（1.5.1 与 11-02 文本冲突） | **资产清零** |

论述：A 路线的合规定性**不是「灰」，是「有条件的白」**——但条件里包含「仅 Gemini」，而技术上 1.5.1 已把门打开。这意味着 **SAG 走 A 路线时，最大的合规风险不是「被封」，而是「暴露了不该暴露的模型」**。若 SAG 在 A 路线上暴露 `claude-*`，就同时进入「1.5.1 技术上允许 / 11-02 政策上不允许」的夹缝——**这个夹缝目前无法用任何技术手段消解，只能靠产品策略回避（只暴露 Gemini）**。

### 5.2 模型范围

| 项 | A 官方 ACP | B 裸 API |
|---|---|---|
| 政策文本 | Gemini-only（11-02 答复 + 官方文档） | 全部模型在技术上可取；政策上整条路都不许 |
| 技术现状 | 1.5.1 起 `agy acp` 全模型；registry 1.4.5（二进制）是否仍有 `clientInfo.name` 白名单：**未验证**（已验证上限为 1.4.1，#1168/#1318） | 账号 entitlement 全量，但受 UA 版本 gate 与档位裁剪 |
| 拿到 Claude 的代价 | 伪装 `clientInfo.name: "zed"`（政策高危）或 `customModels` 旁路（1.4.5 未验证） | 无需伪装（本来就在伪装整条链） |
| 新模型滞后 | **二进制版本绑定**（#19150：1.3.0 看不到 Gemini 4 Argon） | UA 版本 gate（#5175：3.7 需 ≥2.9.0） |

论述：两条路线**都会「少模型」，但少的原因不同**——A 少在「目录被白名单过滤 + 二进制太旧」，B 少在「UA 版本不够 + 档位 entitlement」。区别是：A 的缺失**在政策上是被要求的**，B 的缺失**是逆向没跟上**。前者不能修，后者能修但修了也违规。

### 5.3 功能面（tools / 多模态 / usage / 流式）

| 能力 | A 官方 ACP | B 裸 API | 差异性质 |
|---|---|---|---|
| 客户端侧 function calling | ❌ 语义不存在（agent 自执行）；只能经 MCP 注入 | ⚠️ 原生 `functionDeclarations`（schema 子集 + 签名链） | **A 需要新造 MCP 桥；B 只需翻译** |
| 并行工具调用 | 取决于 agent 自身 | 支持但文档薄 | B 更近 |
| `tool_choice` 强制指定 | 协议无此概念 | ❌ 普遍不支持 | 都缺 |
| 服务端工具 | agent 内建（可视为有） | ❌ `googleSearch`/`urlContext` 不能与 `functionDeclarations` 同请求 | A 反而占优 |
| 图片输入 | ✅ 上下文图片可用；媒体工具结果在 1.4.5 退化成文本（registry #741） | ✅ base64 inline；长会话被压缩丢弃 | 平手偏 A |
| reasoning 暴露 | `agent_thought_chunk`（无 OpenAI 对应位；RC01 稀疏） | `thoughtsTokenCount`（含未计入输出的思考） | 都是近似 |
| usage | **全缺**（#1045） | 部分有（有记账缺口） | **B 明显占优** |
| 流式语义 | `session/update` 逐 chunk，无 `finish_reason` 中间态 | SSE，可映射（6 类已知缺陷） | B 明显占优 |
| 参数控制（temperature 等） | 协议层只有 model / thinking level / mode，**无采样参数** | 有字段但 3.6+ 静默失效 | 都不可承诺 |
| `stop` / `logprobs` / `n>1` | 协议无 | `stop` 可映射，其余缺 | B 略优 |

论述：**A 路线在「能力表」上看起来还行，是因为很多能力被 agent 内建了**——但那不是 OpenAI 语义。真正落地到 `/v1/chat/completions` 时，A 需要额外造一个 MCP server（相当于在网关内再跑一个服务），而 B 只需要写 translator。「A 的多模态占优」也是假象：那是 agent 自己的文件读取，不是模型接收图片。

### 5.4 会话语义：谁持有历史

| 项 | A 官方 ACP | B 裸 API |
|---|---|---|
| 历史权威方 | **服务端**（`sessionId` + SQLite `conversations/<id>.db`）；`session/load` 重放全文 | **协议无状态**：每次全量 `contents[]` |
| 双写问题 | **有**：客户端 `messages[]` 与 ACP 历史并存，不一致即二选一 | **无协议级双写**，但有「身份连续性伪造」问题（session key + 粘性账号） |
| 上下文丢失来源 | 客户端侧 compaction / pre-flight truncation（见首份报告「边界外的路线」）；ACP 侧 compaction 事件 OpenAI 无对应位 | 网关侧压缩逻辑 + 签名链断裂 + 上游 1,048,576 硬上限 |
| 恢复能力 | `session/resume` 不重放历史（1.5.0+），比 `load` 更适合无头 | 完全靠客户端全量重传 |
| 会话堆积 | server 侧 SQLite 会话文件持续增长（"Never poll these DBs"） | 上游 trajectory 堆积（**未验证**是否有配额成本） |

论述：**两条路线都有「谁持有历史」的问题，但方向相反**。A 的问题是「服务端说它有历史，客户端也说它有一份」；B 的问题是「谁都没有历史，网关必须伪造连续性的**假象**」。对 SAG，前者意味着**必须实现有状态会话管理**（与「无状态兼容网关」定位冲突），后者意味着**必须实现粘性路由 + 签名存储**（同样引入状态，但至少状态是网关自己的，可清理）。

### 5.5 运维成本与稳定性

| 项 | A 官方 ACP | B 裸 API |
|---|---|---|
| 部署依赖 | ~1.5 GB 二进制 / 每机器；`--process-per-session` 时内存线性增长 | 0 依赖（纯 HTTPS 客户端） |
| 单会话固定成本 | ~327 MB RSS + 5–6 s 冷启 | ≈ 0 |
| 扩容方式 | 加进程（受内存限制） | 加并发（受配额与账号粘性限制） |
| 失效形态 | 二进制缺陷（SIGILL / 卡死 / 不退）+ 策略收紧 | 协议漂移（月级）+ 封号（凭证级 403） |
| 失效可观测性 | 进程退出码 / 挂住（需要 watchdog） | HTTP 403 / 假 429（需要分类器） |
| 修复方式 | 等 Google 发版（5–6 天一发，但缺陷修复慢：SIGILL 与 #1045 都跨版本存在） | 自己打补丁（每月至少一次） |

论述：**A 的成本是「每会话固定成本」，B 的成本是「持续军备投入」**。前者可预算但不可压缩，后者不可预算且随时可能整体归零。同类项目维护负担的量化信号（CLIProxyAPI 824 open issues / Antigravity-Manager 1,713 open issues；见首份报告「边界外的路线」）说明 B 的军备成本是结构性的、不收敛的；A 的成本则是**一台机器能跑几个会话**这种朴素算术。

### 5.6 可观测性

| 项 | A 官方 ACP | B 裸 API |
|---|---|---|
| token 账本 | **拿不到**（#1045，三种实测）；只能估算或旁路读 SQLite / `/usage` | 有 `usageMetadata`（prompt / candidates / thoughts / cachedContent），但 `total ≠ prompt + completion` 的记账缺口需自己修 |
| 真实模型回显 | 未知（1.5.x 是否回显实际模型未验证） | 静默降级时 `modelVersion` 是否被改写**未验证** |
| 请求可审计 | 只能看自己发的 JSON-RPC + server 日志（`--log-file`） | 自己的 HTTP 请求完全可见，但上游可能重写 |
| 健康检查 | 需 watchdog 探进程 + `initialize` 往返 | 可探配额端点 / 模型目录端点 |

论述：**这是 A 路线最被低估的短板**。SAG 的核心能力之一是「按 token 记账 / 按配额路由」（首份报告「SAG 合规接入评估」的适配假设），而 A 路线在这个能力上是**结构性缺失**：没有 usage 就没有账本，没有账本就没有配额路由。**这不是加个字段能补的，是上游没发。**

### 5.7 对 SAG 的接入形态差异

| 项 | A 官方 ACP | B 裸 API |
|---|---|---|
| 需要新增的东西 | 子进程管理器（下载/校验/租约/重启/watchdog）、ACP client（14 种 update 的解码）、会话映射表、MCP server（为了暴露 `tools`）、权限/提问应答策略 | 一个 provider 节点（`base_url` + headers/body overrides），复用现有 OpenAI 适配器 |
| 与现有 `LlmProvider` trait 的契合 | **不契合**：ACP 是「一轮长事务 + 通知流」，不是「请求 → chunk 流」 | 契合：本质就是 SSE chunk 流 |
| 4xx 语义映射 | JSON-RPC error → HTTP 需自定映射（`-32602` / `-32000` / `-32601`） | 上游 429/403 直接可得（但需五类分拣） |
| 状态引入 | 会话（会话 ID 映射 + 历史双写）、进程（生命周期） | 会话粘性（账号绑定）+ 签名存储 |
| 与首份报告 Tier 分层的对应 | 首份报告把 `agy acp`（仅 Gemini）放在 **Tier B**，对应本次的 A 路线 | 首份报告把 `v1internal` 直连放在 **Tier C**（v2 已从合规评估中剔除），对应本次的 B 路线 |

论述：**接入成本的差异与风险方向相反**——B 路线「零成本接入」恰恰是危险（它让一个随时 403 的上游看起来像正常上游，见首份报告「架构契合度」）；A 路线「高成本接入」反而构成一道天然门槛，但门槛后面是一个**功能不够用**的东西。

---

## 6. 对 SAG 的结论

### 6.1 两条路线都不是「正经 provider」

- **A 官方 ACP 路线**：合规，但**功能阉割 + 成本高 + 语义翻译伤筋动骨**。要接它，等于在 SAG 里内嵌「一个 ACP 客户端 + 一个 MCP 桥 + 一套会话状态机 + 一个进程看护器」，而换来的能力是「Gemini 文本 + agent 自执行工具（SAG 看不见）+ 无 usage」。**它更像「把 Antigravity 这个 agent 产品代理出来」，而不是「接一个模型 provider」。**
- **B 裸 API 路线**：功能全、成本低、语义最近，但**政策上是死路**（§6 + FAQ + 凭证级封禁 + 每月漂移）。技术可行性从来不缺——同类项目的公开盘点早已证明这一点（明细见首份报告「边界外的路线」）——缺的是「可以承诺」。

### 6.2 若必须选：场景化建议

| 场景 | 建议 | 理由 |
|---|---|---|
| **个人自用 + 只要 Gemini 文本** | **A 路线可行**（`agy acp` 腿，只暴露套餐内 Gemini，不冒充 `clientInfo.name`） | 官方明文 permitted；不需要 usage 精确；单机单会话成本可接受；避开模型范围的政策夹缝 |
| **个人自用 + 想要 Claude/GPT** | **不建议走任何一条** | A 需伪装 client 名（政策高危且 1.4.5 未验证）；B 是明文 breach 且已有凭证级封禁案例。**此时正确做法是买 Claude / OpenAI 官方 key** |
| **团队内部工具 / 多人共享一个额度** | **不建议 A**：单用户交互式开发是许可前提，多人共享直接越界；**B 明确违规** | 11-02 答复的条件是 "single-user interactive development" |
| **任何对外服务 / 需要 SLA / 需要计费** | **两条都别接**，走 **Vertex AI / AI Studio API key**（`gemini-api-key` auth method 也存在于 A 路线的合法选项里） | 只有 key 路线能承诺 SLA、能给准确 usage、不怕封号。这也是 Google 自己在 FAQ 里给的唯一建议 |
| **研究/实验通道（自建、不对外）** | B 路线可作 **Tier C 可弃实验通道**，与合规通道硬隔离 | 首份报告「合规分层」的分层框架仍可参照（其中 Tier C 已按合规范围移出） |

### 6.3 与首份报告 Tier 分层的衔接

| 首份报告 Tier | 本报告路线 | 本次补充的判据 |
|---|---|---|
| **Tier A**（可承诺 SLA） | 不涉及 | 唯一出口：Vertex / AI Studio key；或用 A 路线的 `gemini-api-key` / `agent-platform` auth method（此时不再是「订阅额度转换」，而是正当的 key 通道） |
| **Tier B**（可提供但不承诺） | **A 官方 ACP** | 新增判据：①usage 全缺 → 无法计费；②工具语义反 → 无法承诺 `tools`；③进程级失败模式 → 需 watchdog；④模型范围政策限 Gemini。**结论：只适合「单用户 + Gemini 文本 + 不需计费」这一极窄场景。** |
| **Tier C**（仅自用/研究） | **B 裸 API** | 判据不变（首份报告「合规分层」）。新增证据：2026-11-06 凭证级 `TOS_VIOLATION` 已把「账号还在」这个健康信号打掉。 |

### 6.4 三条不可妥协的判断

1. **不要在 A 路线上冒充 `clientInfo.name`。** 技术上可行（#1318 已验），但那是把「官方明文 permitted」换成「官方明文限 Gemini 之外的越界」，把唯一一条合规路线的合规性自己拆掉。
2. **不要在 B 路线上承诺任何 SLA 或计费。** 凭证级封禁 + 假 429 + 静默降级三者叠加，使得「失败」与「成功但降级」都不可观测。
3. **不要把两条路线混在同一个 `/v1/models` 列表里。** 它们的 SLA、ToS、可撤销性、能力面完全不同（首份报告「SAG 合规接入评估」的适配假设第 10 条）。若要同池，必须按「通道来源」打标并暴露在响应头。

---

## 7. 未验证事项

R4 的 7 条 + 本轮新增 5 条。**特别地，第 1 条是本报告最重要的未验证点**。

| # | 未验证事项 | 为什么重要 | 需要什么才能验证 |
|---|---|---|---|
| **1** | **「1.5.1 全模型开放」与「11-02 政策文本仅 Gemini」谁是真口径** | **决定 A 路线能否暴露 Claude/GPT，也决定「合规」这个最大卖点是否成立。** release note（11-05）晚于政策答复（11-02）3 天且无任何政策说明 | 官方对 #1168 / registry #554 的书面答复，或一次官方文档改版 |
| 2 | 1.4.5（registry 二进制）是否仍有 `clientInfo.name` 白名单 | 决定 A 路线两条腿是否行为一致 | 无源码、无 changelog、无 1.4.5 实测帖；只能实跑二进制 |
| 3 | `agy acp` 与 registry `.par` 是否共享 `model_selection` / CCPA 代理层 | 决定 1.5.1 的开放是否也覆盖 registry 腿 | 只能从「都是 Python + 都有 `--uid=`」推断 |
| 4 | 1.5.x 是否补了 usage / `session/cancel` / provider 形态请求 | 决定 A 路线能否计费、能否中断 | #1045 / #554 都仍 open，无实测 |
| 5 | `plan` 会话模式在 1.5.x 上是否只读 | RC01 的 `/plan` 会直接写文件；1.5.0 号称加了 `plan` | 实跑 1.5.3 观察是否有文件写入 |
| 6 | Blyck #196885 是否在别处获答；Antigravity Additional ToS §6 是否有版本变更 | 决定「官方开口」是否覆盖「非编辑器类 ACP client」 | 该帖截至 2026-11-08 无回复；ToS 版本号本轮未核对 |
| 7 | `agy acp` 的并发上限 / 多账号支持 | 决定 A 路线的多租户可行性 | 无官方或实测数据；`--process-per-session` 的存在暗示默认共享进程 |
| 8 | `customModels` 旁路在 1.4.5 / `agy acp` 1.5.3 上是否仍成立 | 若不成立，则唯一的「不伪装也能拿 Claude」手段消失 | 需实跑（#15378 只覆盖 1.4.1） |
| 9 | 1.5.x 的 thinking level 与 mode 的确切 payload | 决定 SAG 的 model alias 映射是否可行 | 1.4（slug 内嵌）与 1.5（独立选项）口径不同，需实测 |
| 10 | 1.4.5 上 SIGILL 的触发条件（CPU 亲和性 vs 具体 CPU 型号） | 决定 A 路线能否在生产机群稳定运行 | 单方 12 次 A/B 可复现，缺第二方复现 |
| 11 | 「仅跑官方 ACP 二进制被封」是否真的零案例 | 这是 A 路线合规判断的关键反证；「未发现」不等于「不存在」 | 需要更大范围的检索或官方统计 |
| 12 | A 路线上「真实模型回显」是否存在（静默替换在 ACP 侧的表现） | 决定网关能否发现降级 | 无实测 |

---

## 8. 来源清单（分组精选）

> 星数 / push 时间取 GitHub REST API 于 2026-11-08 的实测；`agent.json` 版本取 raw 原文。正文中行内链接均出自本表或其直接延伸（issue 号 / PR 号）。

### 8.1 A 路线：官方 ACP 文档与二进制

- `[官方]` [Agent Client Protocol (ACP) | Antigravity CLI Docs](https://antigravity.google/docs/cli/acp/)（查阅 2026-11-08）
- `[官方]` [Other editors (ACP) | Antigravity Docs](https://antigravity.google/docs/ide/extensions/acp/)（查阅 2026-11-08；第三方模型名单、thinking level、modes、flags、auth methods）
- `[官方]` [FAQ | Antigravity Docs](https://www.antigravity.google/docs/faq/)（查阅 2026-11-08）
- `[官方]` [Google Antigravity - Terms of Service](https://antigravity.google/terms/)（§6，查阅 2026-11-08）
- `[官方]` [antigravity-cli release 1.5.0](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.0)（2026-11-04，`agy acp` 进 `--help`、`session/resume`、`--process-per-session` 族）
- `[官方]` [antigravity-cli release 1.5.1](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.1)（2026-11-05，「every model to every ACP client」+ 一批会话 bug 修复）
- `[官方]` [antigravity-cli releases 列表](https://github.com/google-antigravity/antigravity-cli/releases)（1.5.0 / 1.5.1 / 1.5.3 均在 2026-11-04~07）
- `[官方]` [registry/antigravity-acp/agent.json（raw）](https://raw.githubusercontent.com/agentclientprotocol/registry/main/antigravity-acp/agent.json)（2026-11-08 拉取：`version 1.4.5`，Google LLC，proprietary，`--uid=`）
- `[项目]` [agent.json 提交历史](https://github.com/agentclientprotocol/registry/commits/main/antigravity-acp/agent.json)（1.0.0 → 1.4.0 可见区）

### 8.2 A 路线：模型可用性与协议实测

- `[项目]` [registry issue #554](https://github.com/agentclientprotocol/registry/issues/554)（2026-08-28，open，Google 零回应；去过滤后仍需 5 处兼容改造）
- `[项目]` [antigravity-cli issue #1168](https://github.com/google-antigravity/antigravity-cli/issues/1168)（2026-10-05，open；`clientInfo.name` 根因确诊）
- `[项目]` [antigravity-cli issue #1318](https://github.com/google-antigravity/antigravity-cli/issues/1318)（2026-10-22，open；`supports_third_party_models()` 源码 + 11→18 对照实验）
- `[官方]` [antigravity-cli issue #1045](https://github.com/google-antigravity/antigravity-cli/issues/1045)（2026-09-18，open；usage 全缺）
- `[项目]` [ACP Protocol Reference — antigravity-acp（verified live 2026-09-03）](https://cdn.jsdelivr.net/npm/@estebanforge/pi-antigravity-bridge@1.4.7/docs/ACP-PROTOCOL-REFERENCE.md)（14 种 update、`-32601` cancel、无 usage、settings.json/GEMINI_HOME/token 文件、MCP 实测坑）
- `[项目]` [pi-antigravity-bridge — ENGINES.md](https://cdn.jsdelivr.net/npm/@estebanforge/pi-antigravity-bridge@1.6.2/docs/ENGINES.md)（~1.5 GB 二进制）
- `[项目]` [registry issue #741](https://github.com/agentclientprotocol/registry/issues/741)（2026-11-01，open；1.4.5 darwin-arm64 媒体文件被当文本）
- `[项目]` [agent-client-protocol issue #1694](https://github.com/agentclientprotocol/agent-client-protocol/issues/1694)（2026-07-16 → 09-23 closed；文档与 schema 不一致）

### 8.3 A 路线：生态项目与工程对比

- `[项目]` [openclaw/acpx — agents/Antigravity.md](https://github.com/openclaw/acpx/blob/main/agents/Antigravity.md)（`interaction_` 提问、`--approve-all` 取消、GEMINI_HOME、凭据过期弹登录）；[acpx 权限档位](https://acpx.sh/permissions.html)
- `[项目]` [pingdotgg/t3code PR #9348](https://github.com/pingdotgg/t3code/pull/9348)（2026-09-03 merged；`interaction_` → 用户输入问题；安装/认证/租约实现）
- `[项目]` [t3code issue #15378](https://github.com/pingdotgg/t3code/issues/15378)（2026-10-03 → 10-21 closed；`customModels` 旁路实测）
- `[项目]` [t3code issue #19150](https://github.com/pingdotgg/t3code/issues/19150)（2026-10-13 → 11-01 closed；模型目录与二进制版本硬绑定）
- `[项目]` [chaojimct/cli-agent-gateway README](https://github.com/chaojimct/cli-agent-gateway/blob/main/README.zh-CN.md)（ACP → 统一 IR → SSE；只对 permission 语义近似）
- `[项目]` [EvilFreelancer/acpbox — api-mapping.md](https://github.com/EvilFreelancer/acpbox/blob/main/docs/api-mapping.md)（`acp` 扩展字段、usage 恒 0、每请求新建 session、把 mode 当模型）
- `[项目]` [pingu1m/acp-to-api README](https://raw.githubusercontent.com/pingu1m/acp-to-api/main/README.md)（one prompt per provider at a time；usage 估算；provider 列表无 Antigravity）
- `[项目]` [vadim-vyb/acp-openai-gateway](https://github.com/vadim-vyb/acp-openai-gateway/blob/main/README.md)（只解决 HTTP ACP → OpenAI）
- `[项目]` [tiezbro/paseo-agy-acp](https://github.com/tiezbro/paseo-agy-acp)（★67 / 2026-11-01，唯一深挖官方 ACP 内核缺陷的项目：SIGILL、分块响应截断卡死）
- `[项目]` [shubzkothekar/antigravity-acp issue #3](https://github.com/shubzkothekar/antigravity-acp/issues/3)（要求记录 ToS 风险）

### 8.4 ACP 协议规范

- `[协议]` [Prompt Turn](https://agentclientprotocol.com/protocol/v1/prompt-turn)（`session/update` 全表、`stopReason`）
- `[协议]` [Session Setup](https://agentclientprotocol.com/protocol/v1/session-setup)（`session/new` 的 cwd/mcpServers、`load` vs `resume`）
- `[协议]` [Tool Calls](https://agentclientprotocol.com/protocol/v1/tool-calls)（agent 自执行、`request_permission` 结构）
- `[协议]` [Session Modes](https://agentclientprotocol.com/protocol/v1/session-modes)（modes 与 configOptions 双轨）
- `[协议]` [Session usage stabilized](https://agentclientprotocol.com/announcements/session-usage-stabilized)（2026-06-05）

### 8.5 A 路线：合规一手记录与二进制缺陷

- `[官方]` [discuss #192027](https://discuss.ai.google.dev/t/tos-clarification-is-t3-code-official-antigravity-acp-agent-permitted-under-the-antigravity-terms-of-service/192027/2)（2026-10-26，首次开口 + 「仅 Gemini」）
- `[官方]` [discuss #197131](https://discuss.ai.google.dev/t/conflicting-guidance-on-using-t3-code-with-personal-google-ai-subscriptions/197131)（2026-11-02，permitted + Gemini-only + 封号范围 + ARM64 已知问题）
- `[官方]` [discuss #196885](https://discuss.ai.google.dev/t/tos-clarification-desktop-app-blyck-using-the-official-antigravity-acp-binary-over-acp-with-the-users-own-personal-plan/196885)（2026-10-30，无答复）
- `[官方]` [discuss #198478](https://discuss.ai.google.dev/t/appeal-antigravity-agy-cli-disabled-for-tos-403-requesting-manual-review/198478) / [#142192](https://discuss.ai.google.dev/t/appeal-for-account-disabling-unexpected-tos-violation-via-3rd-party-pi-dev-agent-oauth-bridge/142192)（封号申诉，均为 bridge / `-p` 型）
- `[社区]` [discuss #183427](https://discuss.ai.google.dev/t/acp-server-1-1-1-official-agy-acp-server-cold-starts-in-16s-on-every-windows-spawn/183427)（Windows 冷启 ~16 s）
- `[社区]` [discuss #189880](https://discuss.ai.google.dev/t/windows-acp-server-1-4-0-remains-running-after-stdin-eof/189880)（stdin EOF 不退出）
- `[社区]` [discuss #196254](https://discuss.ai.google.dev/t/acp-sessions-become-permanently-wedged-with-502-the-model-is-currently-unreachable-while-fresh-sessions-succeed/196254)（502 后会话永久卡死）

### 8.6 B 路线（细节见首份报告 §10 来源清单与「边界外的路线」）

- `[项目]` [cortexkit/antigravity-auth — ANTIGRAVITY_API_SPEC.md](https://github.com/cortexkit/antigravity-auth/blob/main/packages/opencode/docs/ANTIGRAVITY_API_SPEC.md)（v1.0，2025-12-13）
- `[项目]` [aphrody-antigravity — endpoints.rs](https://docs.rs/aphrody-antigravity/latest/src/aphrody_antigravity/endpoints.rs.html) / [auth.rs](https://docs.rs/aphrody-antigravity/latest/src/aphrody_antigravity/auth.rs.html)
- `[项目]` [router-for-me/CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) —— #4749（端点身份绑定）、#5175/#5177（UA 版本 gate）、#5751（假 429）、#7296（凭证级 TOS_VIOLATION）、PR #3470（哨兵签名）
- `[项目]` [decolua/9router issue #1138](https://github.com/decolua/9router/issues/1138)（多层检测清单，功能请求，未验证）
- `[项目]` [lbjlaq/Antigravity-Manager issue #3529](https://github.com/lbjlaq/Antigravity-Manager/issues/3529)（thoughtSignature 必需）
- `[官方]` [Content generation parameters | Gemini Enterprise Agent Platform](https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/capabilities/content-generation-parameters)（2026-11-07，采样参数静默失效）
- 政策：`[官方]` [Terms](https://antigravity.google/terms/) / [FAQ](https://www.antigravity.google/docs/faq/)；`[官方]` [gemini-cli Discussion #20632](https://github.com/google-gemini/gemini-cli/discussions/20632)（2026-02-27，连带封禁与全量解封）

### 8.7 SAG 本仓库（前序报告，非外部来源）

- [docs/research/acp-to-api-antigravity.md](./acp-to-api-antigravity.md) —— 首份报告（v2 合规重构），§2 术语澄清、§3 合规边界地图、§4 合规路径 A、§8 SAG 合规接入评估

---

## 附录：本报告与首份报告的分工

| 主题 | 首份报告 | 本报告 |
|---|---|---|
| 「ACP」三重撞名 | §2 术语澄清 | 引用（§2.1 / §2.2） |
| 通用限制（ToS、429、签名、参数、漂移） | §3 合规边界地图 + 裸 API 侧明细已移出（见「边界外的路线」） | §4 精炼 + 交叉引用 |
| 官方 ACP 路线的内部构造 | §4 合规路径 A（模型范围、协议语义、认证运行、成本、二进制缺陷） | **§3 深挖主体**（模型白名单、14 种 update、工具执行方向、usage、认证运行、成本、二进制缺陷、合规现状） |
| 双路线逐维度对比 | 无 | **§5** |
| Tier 分层 | §8.2「合规分层」提出 | §6.3 衔接并补充判据 |
