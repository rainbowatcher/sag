# Antigravity 的合规边界与合规路径限制：官方 ACP 路线与官方 API key 替代

> **调研日期**：2026-11-08（北京时间 / UTC 同日）
> **面向**：SAG（Smart AI Gateway）接入决策
> **本版说明**：v2 合规重构。本次把探索范围收缩为**合规途径**，主线从「协议转 API 的技术可行性」改为**「合规边界地图 + 合规路径自身的限制」**。**不合规路线的技术细节已剔除**（直连私有端点的手法、请求信封与认证细节、UA gate 绕过、检测手段分层、指纹对齐、账号池与出口 IP 等缓解手段均不在本报告范围）；两条路线（官方 ACP / 裸 API）的完整逐维度对比见续篇 [antigravity-acp-vs-bare-api.md](./antigravity-acp-vs-bare-api.md)。
> **信息时效性说明**：本文涉及的 ToS、FAQ、模型目录、二进制版本与官方论坛表态均处于日更级变动状态。`agy` CLI 在 2026-11-04 ~ 11-07 三天内发了 1.5.0 / 1.5.1 / 1.5.3 三个版本；政策文本与 release note 之间已出现**互相矛盾**（见 §3.3）。**本文结论的有效期不超过 30 天**，任何「合规边界」「模型范围」判断在使用前必须重读一手页面。
> **标注约定**：`[官方]` = Google 一手文档 / release note / 论坛官方回复；`[协议]` = agentclientprotocol.com 规范原文；`[项目]` = 转换层或客户端项目自述 / issue / PR；`[社区]` = 用户帖（附可信度）；`未验证` = 无法交叉核实，不得当作确定事实使用。

---

## 1. 摘要

**本报告的目的不是「怎么把 Antigravity 转成 API」，而是先回答「哪些做法在合规边界内、这些合规做法各自被限制到什么程度」。** 边界事实本身（官方明文允许什么、明文禁止什么、什么悬而未决）是本文要摸清的对象。

**边界三区速览**：

| 区域 | 内容 | 依据 |
|---|---|---|
| **明文禁止** | 用第三方软件 / 工具 / 服务访问 Antigravity（含抽取或代理 OAuth 凭据、把 `agy -p` 包进第三方 agent、自动化账号轮换） | `[官方]` ToS §6、FAQ 两条、2026-11-02 论坛答复 |
| **明文允许** | 在 ACP 兼容客户端里跑**未修改的**官方 `antigravity-acp` 二进制 + 原生 `oauth-personal` 认证，用于**单用户交互式开发** | `[官方]` 2026-11-02 论坛答复 |
| **悬而未决** | ① 允许范围内**模型是否只限 Gemini**（1.5.1 与 11-02 答复冲突）；② 非编辑器类 ACP client 是否有普遍许可（Blyck 提问无答复）；③ 官方文档「第三方模型仅限固定编辑器名单」与 release note 的冲突 | `[官方]` 见 §3.3 |

**合规路径一句话结论**：目前只有两条明确合规的路——**① 官方 API key（Vertex / AI Studio）**，可承诺 SLA 但与 Antigravity 订阅额度无关；**② 官方 ACP 服务器**，有官方书面许可，但被政策口径与协议语义双重限制（模型范围、agent 语义、usage 缺失、每会话固定成本），只适合「单用户 + Gemini 文本 + 不需计费」的极窄场景。**边界外路线（直连私有端点、token 重放、`agy -p` 包装、账号轮换）被官方明文禁止，不进入本报告的技术评估。**

---

## 2. 术语澄清（必须靠前，否则后续全部判断会错位）

中文生态里「ACP」至少有三个互不相干的所指。混淆它们会直接导致架构判断错误。

| # | 「ACP」实际所指 | 是什么 | 协议 / 传输 | 在 Antigravity 生态里的位置 |
|---|---|---|---|---|
| ① | **Agent Client Protocol**（Zed 发起，现 Zed + JetBrains 共同开发，Apache-2.0） | 编辑器 / IDE ↔ coding agent 之间的通信标准，**公开规范** | JSON-RPC 2.0，本地 stdio，远程 HTTP/WebSocket（远程仍标 work in progress） | Google **官方**提供 `antigravity-acp` 服务器（`agy acp`），ACP Registry 里是 v1.4.5（2026-11-08 实测 `agent.json`），作者标 "Google LLC" |
| ② | **Cloud Code `v1internal`** | Google 私有上游 HTTP 接口，Gemini 形状 JSON/SSE | HTTPS + JSON + SSE | **不合规路线**实际打的靶子（见 §7） |
| ③ | **本地 language server Connect-RPC** | Antigravity 桌面 / IDE 与 Codeium 血统的 `language_server` 之间的私有 RPC | Connect RPC v1，自签 TLS，`127.0.0.1:随机端口` | 第一代架构的桥接层，同属**不合规路线** |

**为什么必须区分**：

- **① 是公开标准，Google 自己支持它**，这是本报告唯一进入合规评估的自研协议路线（§4）。它真正限制的不是「能不能连」，而是**模型范围**与**执行语义**。
- **②③ 是私有、逆向得来的，被 ToS 明文禁止**（§3.1）。它们形态与 ① 相似（都是「事件流 + 工具 + 会话」），但既不合规、稳定性也差一个量级。本报告不探索其技术细节。
- **① 的语义是 agent，不是 chat completions。** ACP 是 `session/prompt` + 工具授权 + 文件读写，agent 自己执行工具（"Agents run tool calls themselves"，[ACP v1 Tool Calls](https://agentclientprotocol.com/protocol/v1/tool-calls)）。把它「降维」成 `/v1/chat/completions` 的代价见 §4.2。

**命名漂移的实证**：把 Cursor 自有协议称作 ACP（[AsterMux 介绍文](https://www.jxxy.net/ai/articles/astermux-cursor-acp-openai-anthropic-gateway/)，2026-09-07）；把 ACP 错误展开成 "Agent Collaboration Protocol"（[Eave Luo 博客](https://eaveluo.com/blog/ai/openclaw/acp-agent-protocol)，2026-03-13）。

> **SAG 内部术语建议**：`acp`（Agent Client Protocol，公开标准，可对接）/ `cloudcode-v1internal`（Google 私有上游，不合规，不接）/ `antigravity-ls-connect`（本地 LS 桥接，不合规，不接）。三者不要共用同一个词。

---

## 3. 合规边界地图（本文核心）

**本节是重构后的主体**：把「官方明文允许什么 / 明文禁止什么 / 什么悬而未决」逐条钉死，并给出执法记录。合规路径的选择与限制都由这一节推导。

### 3.1 明文禁止区

#### 3.1.1 一手政策文本

`[官方]` **[Google Antigravity - Terms of Service](https://antigravity.google/terms/)**（查阅 2026-11-08；该条款在 2026-09 前后被改写以明确适用范围）：

> **§6.** "You must not abuse, harm, interfere with, or disrupt the Service. This includes, but is not limited to, **using the Service in connection with products not provided by us**. **Using third party software, tools, or services to access the Service (e.g. using OpenClaw with Antigravity OAuth) is a breach of this Agreement.** Such actions may be grounds for **suspension or termination of your Antigravity and/or Gemini CLI accounts**."

`[官方]` **[FAQ | Antigravity Docs](https://www.antigravity.google/docs/faq/)** 两条把边界写死（原文引用）：

1. "**Why can't I use third party software (e.g. Claude Code, OpenClaw, OpenCode) with my Antigravity login?** Using third party software, tools, or services to access Antigravity is a violation of our Terms of Service … If you would like to use a third party coding agent with Gemini, we recommend using a **Vertex or AI Studio API key**."
2. "**Can I use Antigravity CLI headless mode in third-party coding agents** or automate account switching?" → "Wrapping `agy` inside third-party coding agents or applications, or automating account rotation, **is not supported under consumer plans**."

**第 2 条直接否掉了「起 `agy -p` 子进程包装」这一类做法**；第 1 条点名的三个例子（Claude Code / OpenClaw / OpenCode）又恰好是「把 Antigravity 当 OpenAI/Anthropic API 用」最主流的三个客户端。**这不是解释空间问题，是书面禁止。**

#### 3.1.2 2026-11-02 官方答复里的禁止清单

`[官方]` **[Conflicting guidance on using T3 Code with personal Google AI subscriptions](https://discuss.ai.google.dev/t/conflicting-guidance-on-using-t3-code-with-personal-google-ai-subscriptions/197131)**（提问 2026-10-31，答复 2026-11-02，答复人 Rebbanpalli_Naveen）在被允许项之外，明确点出仍会封的几类：

- **抽取或复用 OAuth 凭据**的第三方工具 / 代理 / harness（点名 OpenClaw、OpenCode、Claude Code）；
- **把 `agy -p` 包进第三方 agent**（consumer 套餐）；
- **自动化账号轮换**；
- 使用官方 runtime 之外的第三方集成时「必须改用 **Gemini Enterprise 配置**或 **Google AI Studio API key**（`GEMINI_API_KEY`）」。

同帖还确认 **linux-arm64 39-bit VA 的 TCMalloc 崩溃是已知问题、正在修**，并明确「**别用 `agy -p` 包装或 token bridge 绕过**」。

#### 3.1.3 执法记录（封禁时间线）

边界不是纸上规则——它有执行记录。以下几条**定义边界**，故保留：

| 时间 | 事件 | 来源 |
|---|---|---|
| 2026-02-23 | Antigravity 负责人公开称「massive increase in malicious usage of the Antigravity backend」，开始封禁 | [VentureBeat](https://venturebeat.com/orchestration/google-clamps-down-on-antigravity-malicious-usage-cutting-off-openclaw-users)（2026-02-23） |
| 2026-02-26~27 | 封禁**连带** Gemini CLI 与 Gemini Code Assist（因共用 abuse-prevention 后端层） | `[官方]` [gemini-cli Discussion #20632](https://github.com/google-gemini/gemini-cli/discussions/20632)（2026-02-27，Google 员工 jackwotherspoon 发帖） |
| 2026-02-27 | 官方宣布全量自动解封；改为「邮件 + Google Form 重新承诺」流程；**第二次违规永久封禁** | 同上 |
| 2026-09-03 | ToS §6 重写并点名 OpenClaw；12 小时内措辞收窄（从 "your account" 改为点名两个产品账号） | [Ground Truth 报道](https://groundtruth.day/news/googles-antigravity-terms-ban-third-party-clients-outright.html)（2026-09-03） |
| 2026-09-16 ~ 10-08 | 论坛有 10 次书面确认「用 `agy -p` 作为本地子进程被 Codex/Claude Code 调用符合 §6」；**10-08 最后一帖后 71 分钟，另一帖推翻该立场并警告自动封号** | [论坛 #198430](https://discuss.ai.google.dev/t/1m-advertised-context-vs-a-256k-compaction-clamp-in-the-shipped-binaries-plus-quota-and-agy-p-policy-questions-desktop-2-29-0-cli-1-5-1/198430)（2026-11-05，附 10 个帖号） |
| 2026-10-21 / 11-01 / 11-03 / 11-05 | 多轮社区封号潮，对象均为 token bridge / `agy -p` 包装 / 多账号型 | `[社区]` [Linux.do](https://linux.do/t/topic/3053739)（10-21）、[discuss #197297](https://discuss.ai.google.dev/t/account-suspended-again-multi-agent-setup-calling-antigravity-cli/197297)（11-01）、[Linux.do](https://linux.do/t/topic/3112874)（11-03）、[Linux.do](https://linux.do/t/topic/3129568)（11-05） |
| **2026-11-06** | `403 PERMISSION_DENIED / reason: TOS_VIOLATION`，错误体带 `appeal_url`。**关键差异：只封 Cloud Code 这一个服务**，账号登录、token 刷新、模型目录读取、订阅全部正常；3 个凭证中 2 个中招、第 3 个同条件 200 → 判定为**凭证级**而非 IP / 机器 / 客户端签名级 | [CLIProxyAPI #7296](https://github.com/router-for-me/CLIProxyAPI/issues/7296)（2026-11-06） |

**这条最新证据的分量**：封堵已从「账号级」细化到「**服务级 + 凭证级**」。对网关的含义是——失效不会以「账号掉线」这一可观测事件出现，而是一个凭证的某一个服务突然 403，其余一切正常。

#### 3.1.4 地域限制

- `[官方]` FAQ 列出 150+ 国家 / 地区白名单，**明确不含中国大陆、香港、澳门**；含台湾、日本、新加坡、美国。门槛是 **Google 账号关联地区**，不只是 IP。
- `[项目]` 反例证据：账号地区被改回中国大陆 → 直接 `Agent terminated due to error`，用官方 IDE 退出重登可恢复 —— [Antigravity-Manager #1842](https://github.com/lbjlaq/Antigravity-Manager/issues/1842)（2026-02-11）。
- 另有约 30 个地区**无 Google One AI 套餐**（无法购买 Pro / Ultra，只能用免费档）。

#### 3.1.5 第三方模型条款的附带约束

`[官方]` ToS §8：第三方模型（如 Anthropic）适用该模型**自己的条款**（"For Anthropic specifically, you agree to be bound by … anthropic.com/legal/commercial-terms"）。→ 即便走的是合规的官方 ACP 路线，**通过 Antigravity 转出 Claude 仍可能同时触及 Anthropic 的商业条款**（**未验证** Anthropic 是否实际执法）。这条与 §3.3 的模型范围冲突叠加，构成「想用 Claude 就不合规」的双重信号。

### 3.2 明文允许区

`[官方]` **2026-11-02 答复**（[discuss #197131](https://discuss.ai.google.dev/t/conflicting-guidance-on-using-t3-code-with-personal-google-ai-subscriptions/197131)）是迄今唯一把允许条件写清楚的一手文本，逐条引用：

1. "launching that official, unmodified `antigravity-acp` binary inside an ACP-compatible client (such as T3 Code or Zed) and signing in through its native authentication flow (`"auth": { "type": "oauth-personal" }`) for single-user interactive development **is supported and permitted** under our Terms of Service."
2. **封号范围**："enforcement applies to your Antigravity and/or Gemini CLI service access rather than disabling your entire Google Account across other Google services."

叠加 `[官方]` **2026-10-26 的首次「开口」**（[discuss #192027 第 2 帖](https://discuss.ai.google.dev/t/tos-clarification-is-t3-code-official-antigravity-acp-agent-permitted-under-the-antigravity-terms-of-service/192027/2)）：用 ACP 兼容客户端 + 个人订阅**可以**，前提是跑 ACP Registry 里**未经修改的** `antigravity-acp` 二进制、走它自己的 `oauth-personal` 原生 OAuth。

**允许区的三个限定词（缺一不可）**：

- **未修改的官方二进制**（不是自建的 ACP server，也不是改过的包）；
- **原生 `oauth-personal` 认证流**（不是抽出来的 token / 网关代持的凭据）；
- **单用户交互式开发**（不是团队共享、不是对外服务）。

**合规性的现状**：本次检索**未发现任何「仅跑官方 ACP 二进制被封」的案例**；现存封号案例全部集中在 **token bridge / `agy -p` 包装**型（如 `[官方]` [discuss #198478](https://discuss.ai.google.dev/t/appeal-antigravity-agy-cli-disabled-for-tos-403-requesting-manual-review/198478)、[#142192](https://discuss.ai.google.dev/t/appeal-for-account-disabling-unexpected-tos-violation-via-3rd-party-pi-dev-agent-oauth-bridge/142192)）。→ 官方 ACP 路线是**有条件的白**，不是灰。

### 3.3 悬而未决区（本报告最需要被记住的一节）

#### 3.3.1 模型范围冲突：1.5.1 release note vs 11-02 政策答复（**最高优先级**）

三个一手来源在同一周内给出**互相矛盾**的口径：

| 来源 | 日期 | 说法 | 出处 |
|---|---|---|---|
| **CLI release note 1.5.1** | 2026-11-05 | "Changed `agy acp` to offer **every model available to your account to every ACP client**, where previously only a fixed list of editors were offered models other than Gemini." | `[官方]` [release 1.5.1](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.1) |
| **官方文档（ACP 页）** | 查阅 2026-11-08 | "Gemini models are available in every client. **Third-party models are only offered to JetBrains IDEs (including JetBrains Air), Android Studio, Android Performance Analyzer, Zed, and Xcode.**" | `[官方]` [Other editors (ACP)](https://antigravity.google/docs/ide/extensions/acp/) |
| **论坛官方答复** | 2026-11-02 | "in general ACP clients outside of first-party IDE integrations, **model selection through `antigravity-acp` is scoped only to the Gemini models included in your plan**." | `[官方]` [discuss #197131](https://discuss.ai.google.dev/t/conflicting-guidance-on-using-t3-code-with-personal-google-ai-subscriptions/197131) |

**冲突点明确**：1.5.1（11-05）说「全模型对全部 ACP 客户端开放」，晚于 11-02 的政策答复 3 天，且**未附任何政策说明**；而 11-02 答复明确「通用 ACP 客户端仅限 Gemini」。

**这就是 SAG 走 ACP 路线时最大的单点风险**：技术上能拿到 Claude，政策文本没跟上（或反过来）。**未验证**哪一方是最终口径。三种可能解释，均**未验证**：(a) 文档滞后于 1.5.1；(b) 文档那句在讲 registry 独立二进制而非 `agy acp`（但该页正文讲的是 `agy acp`）；(c) Google 有意保留两套口径（技术放松 / 政策不放）。

**这条冲突的不可消解性**：若在 ACP 路线上暴露 `claude-*`，就同时进入「1.5.1 技术上允许 / 11-02 政策上不允许」的夹缝——**夹缝目前无法用任何技术手段消解，只能靠产品策略回避（只暴露 Gemini）**。

#### 3.3.2 Blyck 提问仍无答复：非编辑器类 ACP client 无口径

`[官方]` **[ToS clarification: desktop app (Blyck) using the official antigravity-acp binary over ACP with the user's own personal plan](https://discuss.ai.google.dev/t/tos-clarification-desktop-app-blyck-using-the-official-antigravity-acp-binary-over-acp-with-the-users-own-personal-plan/196885)**（2026-10-30 发帖）：

- 请求方设计得极保守：只做 ACP client、跑 `dl.google.com` 的未修改二进制、SHA-256 + Google LLC 签名校验、**不读不存不转 token**、只请求「binary 为他的套餐提供的 Gemini 模型」。
- 四个问题：个人套餐是否允许（"We saw the Oct 26 reply in the T3 Code thread, but we'd like confirmation that applies to an app like ours, which is not on the IDE extensions list"）；可否代用户下载；是否需要注册 / 命名 / 限流 / 遥测；若不行，换 `gemini-api-key` 是否可接受。

**截至本次查阅（2026-11-08）页面无任何答复**（显示 "9d"，仅原帖）。→ 说明：① 官方对「非编辑器类 ACP client」仍无明确口径；② 11-02 的答复只覆盖「T3 Code / Zed 这类」举例，**不构成对新 app 的普遍许可**。**未验证** Blyck 后续是否在别处获答。

#### 3.3.3 同一句话被官方重新解释过的先例（引用必须带日期）

上轮基线里的 2026-10-30 表态（"personal Antigravity sign-in can't be used with third-party coding agents such as T3 Code"）来自 forum #189889/6；**11-02 答复把它解释为「在讲 Gemini+Claude 双模型可得性，不是讲能不能连」**。→ 即**同一句话被官方自己重新解释过**。凡是引用论坛表态，必须带日期与帖号，否则可能引到已被推翻的版本。

#### 3.3.4 官方文档与 release note 冲突（同 3.3.1）

「第三方模型仅限 JetBrains / Android Studio / Zed / Xcode」这句文档与 1.5.1 release note 的「every model to every ACP client」直接冲突；且 registry 二进制（1.4.5）与 `agy acp`（1.5.x）两条腿是否同源也**未验证**（见 §9）。

### 3.4 官方推荐替代

`[官方]` FAQ 给出唯一明确的替代建议：**"If you would like to use a third party coding agent with Gemini, we recommend using a Vertex or AI Studio API key."**

- 这是官方自己指出的**合规出口**，也是本报告 §5 的主体。
- **重要边界**：Antigravity 本体的 Plans 页明确 **不支持 BYOK / 自带 endpoint 换额度**（"There is currently no support for … Bring-your-own-key (BYOK) or bring-your-own-endpoint for additional rate limits"，[Plans 文档](https://antigravity.google/docs/plans/)）。→ 「自带 key 走 Antigravity 通道」这条路**不存在**；官方 key 路线的含义是「改用 Gemini 官方 API，而不是继续借 Antigravity 的订阅额度」。
- **Enterprise 路径**：Enterprise（Gemini Enterprise）走 **Google Cloud 条款**，可能是另一条合规路径（且不被「consumer 套餐」限制），但**未验证**是否向第三方客户端开放 `v1internal` 后端 —— [Plans 文档](https://antigravity.google/docs/plans/)。

---

## 4. 合规路径 A：官方 ACP 路线及其限制

**定义**：把 Google 官方 ACP 服务器当子进程驱动，用 **Agent Client Protocol**（JSON-RPC 2.0 over stdio）说 **agent 语义**。两条腿：CLI 内置 `agy acp`（活跃腿，1.5.x 每 1–2 天一发）与 ACP Registry 的独立二进制 `antigravity-acp`（1.4.5，慢腿，`"license": "proprietary"`）。

`[官方]` 依据：[Agent Client Protocol (ACP) | Antigravity CLI Docs](https://antigravity.google/docs/cli/acp/)、[Other editors (ACP)](https://antigravity.google/docs/ide/extensions/acp/)、[registry/antigravity-acp/agent.json](https://raw.githubusercontent.com/agentclientprotocol/registry/main/antigravity-acp/agent.json)（2026-11-08 拉取）。

### 4.1 模型可用性限制

#### 4.1.1 政策文本：通用 ACP 客户端仅限 Gemini

- 11-02 答复：通用 ACP 客户端"model selection through `antigravity-acp` is scoped only to the Gemini models included in your plan"（§3.3.1）。
- 官方文档：第三方模型只给固定编辑器名单（JetBrains / Android Studio / Android Performance Analyzer / Zed / Xcode）。
- **对照**：1.5.1 release note 声称对全部 ACP 客户端开放全部模型（§3.3.1）。**两者冲突未解。**

#### 4.1.2 限制的根因是「client 名字白名单」（事实保留，操作手法不在本报告范围）

`[项目]` CLI issue [#1318](https://github.com/google-antigravity/antigravity-cli/issues/1318)（2026-10-22，open）给出源码级确诊：非 Gemini 模型的可见性由客户端名字白名单判定（`supports_third_party_models()`，只认 JetBrains / Android Studio / Zed / Xcode 四类 client）；`[项目]` CLI issue [#1168](https://github.com/google-antigravity/antigravity-cli/issues/1168)（2026-10-05，open）独立复现：`oauth-personal` 下 `session/new` 只广告 11 个 Gemini，而**同进程同 OAuth** 下 `fetchAvailableModels` 却返回 Claude/GPT-OSS。

**这是一条合规边界事实**：限制不在鉴权层，而在「客户端身份是否属于第一方 IDE 名单」。**如何利用该白名单（伪装 client 名 / 旁路注入）属于对政策口径的规避，操作手法与对照实验不在本报告范围。**

#### 4.1.3 模型目录与二进制版本硬绑定

`[项目]` t3code [#19150](https://github.com/pingdotgg/t3code/issues/19150)（2026-10-13 → 11-01 closed）标题即结论——"Antigravity provider never lists Gemini 4 Argon (bundled ACP runtime 1.3.0 predates it)"。**registry 落后 = 新模型看不见**，而 registry 的更新节奏是周级。→ 即使政策口径放开，模型可用性仍受二进制版本限制。

`[项目]` 另有 registry [#741](https://github.com/agentclientprotocol/registry/issues/741)（2026-11-01，open）：1.4.5 darwin-arm64 上 `view_file` / `client_view_file` 把 mp4 等媒体当文本返回（多模态工具结果在 ACP 路径上退化）。

#### 4.1.4 档位与模型撤下（合规路线的可用性边界）

`[官方]` [Choose a model](https://antigravity.google/docs/models/)：Claude Sonnet/Opus 4.6 与 GPT-OSS-120b 标注 **"Will be removed on November 18, 2026"**；Claude 5.5 / Gemini 4 Argon 仅对「非试用 Pro」或 Ultra 开放。→ 即便模型范围争议按宽松口径解决，**免费 / Plus 档在 2026-11-18 之后也只剩 Gemini 3.8 Flash**。

### 4.2 协议语义对「转 OpenAI」的硬约束

**这是 ACP 路线真正的技术上限**：ACP 不是「另一种 chat 格式」，而是**另一套执行模型**。规范原文均取自 `[协议]` https://agentclientprotocol.com/protocol/v1/ （查阅 2026-11-08）。

#### 4.2.1 session 生命周期：请求-响应 + 服务端持有历史

- `session/new` **必须**带绝对路径 `cwd` 与 `mcpServers`，返回服务端生成的 `sessionId`（Antigravity 是 UUID）—— `[协议]` [Session Setup](https://agentclientprotocol.com/protocol/v1/session-setup)。→ OpenAI 侧没有 cwd、没有 MCP、没有 sessionId；SAG 必须自造「请求 → 哪个 ACP 会话」的映射键。
- `session/load` **重放整段历史**（`user_message_chunk` / `agent_message_chunk` 成对、全文一次性重发而非原始 delta）；`session/resume` 不重放（1.5.0 新增）。→ 若网关把 OpenAI 的 `messages[]` 当权威历史，就与 ACP 侧历史 **双写**；两者不一致时只能二选一。
- `session/close` 幂等；`session/prompt` 的**响应体只有一个 `stopReason`**（`end_turn` / `max_tokens` / `max_turn_requests` / `refusal` / `cancelled`）—— `[协议]` [Prompt Turn](https://agentclientprotocol.com/protocol/v1/prompt-turn)。→ OpenAI 的 `finish_reason: "tool_calls"` 在 ACP 里不存在。
- **steering**：ACP 允许在一轮进行中继续发 prompt，语义是「插话修正该轮」而不是「开新轮」。→ OpenAI 客户端并发发两条消息时期望两条独立 completion，这里会变成插话。
- **cancel**：协议有 `session/cancel`，但 RC01 实测 **`-32601 Method not found`** —— `[项目]` [ACP Protocol Reference](https://cdn.jsdelivr.net/npm/@estebanforge/pi-antigravity-bridge@1.4.7/docs/ACP-PROTOCOL-REFERENCE.md)（verified LIVE on 2026-09-03）。1.5.1 才修了「关闭 / 重载 / 恢复会话时正在跑的 prompt 以 `cancelled` 结束」—— `[官方]` [release 1.5.1](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.1)。

**结论**：OpenAI 的「无状态请求」模型与 ACP 的「有状态会话」模型冲突，且冲突点在网关无法假装——要么丢弃 `messages[]` 只透传最后一条 user 文本，要么在网关侧维护会话映射。

#### 4.2.2 14 种 `session/update` 的 OpenAI 映射表

现行 schema 共 **14** 种；`[协议]` [Prompt Turn](https://agentclientprotocol.com/protocol/v1/prompt-turn)。

| `sessionUpdate` | 含义 | OpenAI `chat.completion.chunk` 对应位 | 结论 |
|---|---|---|---|
| `user_message_chunk` | 用户消息流 / 回放 | 无（`role:user` 只在请求侧） | 丢弃或塞扩展字段 |
| `agent_message_chunk` | 助手文本增量 | `choices[0].delta.content` | ✅ **唯一无损映射** |
| `agent_thought_chunk` | 推理增量 | 无（`reasoning_content` 是 DeepSeek 系方言） | 需自定义字段；RC01 上很稀疏，多数推理混在正文里 |
| `tool_call` | 新工具调用 | `delta.tool_calls[]`（部分） | ⚠️ 语义反了（见 4.2.4） |
| `tool_call_update` | 工具状态 / 结果变化 | 无 | 仅展示；塞 `acp` 扩展对象 |
| `plan` | 执行计划 | 无 | 丢弃 / 扩展 |
| `available_commands_update` | 斜杠命令集 | 无 | 丢弃 |
| `current_mode_update` | 模式变化 | 无 | 丢弃（但影响后续行为） |
| `config_option_update` | 会话配置变化 | 无 | 丢弃 |
| `session_info_update` | 标题 / 时间 | 无 | 丢弃 |
| `usage_update` | 上下文占用 + 累计成本 | 只有终局 `usage`，**字段结构不同** | ❌ 见 4.2.6 |
| `notice` | 需 client 声明 `session.notices` | 无 | Antigravity 未使用（未验证） |
| `compaction_update` | 压缩实体（需 `session.compaction` 能力） | 无 | ❌ |
| `compaction_summary_chunk` | 压缩摘要增量 | 无 | ❌ |

> **文档滞后是已知坑**：`[项目]` agent-client-protocol [#1694](https://github.com/agentclientprotocol/agent-client-protocol/issues/1694)（2026-07-16 → 09-23 closed）标题即 "Docs: prompt-turn page lists 5 SessionUpdate variants, schema v1 defines 11"。实现时**以 schema 为准，不以散文文档为准**。

#### 4.2.3 `request_permission`：反向请求 + Antigravity 特有的提问滥用

- `[协议]` [Tool Calls §Requesting permission](https://agentclientprotocol.com/protocol/v1/tool-calls)：结构为 `{sessionId, toolCall, options}`，`option.kind` ∈ `allow_once` / `allow_always` / `reject_once` / `reject_always`；client 回 `selected` 或 `cancelled`。
- **协议不定义超时**——谁等谁由实现决定。Antigravity 1.5.1 的修复项反证阻塞真实存在："permission requests and question forms now close when they are answered elsewhere or their prompt ends, instead of blocking later prompts"（`[官方]` release 1.5.1）。
- **Antigravity 特有：`interaction_` 前缀的「固定选项提问」**—— `[项目]` [openclaw/acpx `agents/Antigravity.md`](https://github.com/openclaw/acpx/blob/main/agents/Antigravity.md)：Antigravity 通过 `session/request_permission` 发固定选项的用户提问，**选项代表「答案」而不是「工具批准」**。**这是协议滥用**：无头网关若按「自动批准」处理，会把第一个选项当成用户的答案（语义完全错误）；若按「拒绝」处理，会话会挂住。`[项目]` 同款处理见 [t3code PR #9348](https://github.com/pingdotgg/t3code/pull/9348)（2026-09-03 merged）：把 `interaction_` 权限请求路由到 T3 的用户输入提问。

#### 4.2.4 工具执行模型方向相反（最大的架构性不兼容）

`[协议]` [Tool Calls](https://agentclientprotocol.com/protocol/v1/tool-calls) 开头即写："Tool calls are actions the language model asks the **Agent** to perform… **The Agent executes them on the model's behalf.** Agents run tool calls themselves."

三层能力方向：

1. **agent → 自己执行**：读文件、跑命令、编辑（Antigravity 默认，`kind: read/edit/execute`，`content` 带 `diff`）。
2. **agent → client 反向请求**：`fs/read_text_file`、`fs/write_text_file`、`terminal/create|output|wait_for_exit|kill|release`（需 client 声明 `clientCapabilities.fs` / `terminal`）。
3. **client → agent 注入工具**：**只有 MCP**（`session/new.mcpServers`），没有「OpenAI 式 tools」。

冲突是语义级的：OpenAI 的模型输出 `tool_calls` 后**客户端执行**并回填 `role:"tool"`；ACP 的模型输出的是 agent 内部工具，**agent 自己执行完**，client 只收到展示流——**客户端拿不到「模型想调你的函数」这个信号**。

**关掉 client 能力并不能让 agent 停手**：`[项目]` 实测（RC01，`fs:false, terminal:false`）"the agent self-executes its own tools and asks via `session/request_permission`"。

**唯一合法注入通道是 MCP，且实测有坑**（同上 ACP-PROTOCOL-REFERENCE phase-2）：`session/new` 的 `mcpServers` 接受 `http` / `sse` / stdio（实测 `headers` 是**列表**、死 URL 也接受，连接是懒的）；MCP 工具 `rawInput` 多一层信封、`rawOutput` 常是展示字符串而非结果、MCP 结果 out-of-band 到达模型而 client 侧看不到。

#### 4.2.5 modes / thinking levels 被烘进模型 slug

- `[协议]` [Session Modes](https://agentclientprotocol.com/protocol/v1/session-modes)：两套并行机制——旧 `modes` 与新 `configOptions`（`session/set_config_option`，`type:"select"`）；文档明确 "Dedicated session mode methods will be removed in a future version"。
- Antigravity RC01 的 `session/new` 同时给两套：`modes.availableModes = default | auto_edit | yolo`；`configOptions = [{id:"model"}, {id:"mode"}]`。
- **thinking level 不是独立字段，而是烘进模型 slug**：`gemini-3.8-flash-low|medium|high`、`claude-opus-5-5-high`、`gpt-oss-120b-medium`。→ SAG 的模型名会与官方 slug 绑死，**Google 改名即断**（#19150 是活例）。
- 1.5.x 把 thinking level 单列为会话选项（`Low`/`Medium`/`High`/`Extra`/`Max`，[Other editors (ACP)](https://antigravity.google/docs/ide/extensions/acp/)），与 RC01 的 slug 方案不同；**1.5.x 的确切 payload 未验证**。
- **模型校验发生在服务端且在构造 agent 之前**，网关不能「先发请求再失败」，必须从 `session/new` 的目录里取模型 ID。

#### 4.2.6 usage 硬缺失（合规路线的结构性短板）

- `[协议]` `usage_update` 载荷是 `{used, size, cost?}` —— **上下文窗口占用 + 累计成本**，不是单轮 prompt/completion token；2026-06-05 起稳定（`[协议]` [Session usage stabilized](https://agentclientprotocol.com/announcements/session-usage-stabilized)）。单轮 token（`PromptResponse.usage`）是**另一个尚未合并的 RFD**。
- **Antigravity 两者都不发**，三种独立实测一致：
  - `[官方]` CLI [#1045](https://github.com/google-antigravity/antigravity-cli/issues/1045)（2026-09-18，open，点名 1.1.1）："`session/prompt` returns only `stopReason`. There is no `usage` in the response and no `usage_update` notification during the session."
  - `[项目]` 独立实测（RC01，2026-09-03）："NO usage/token fields appeared anywhere … Gate B FINAL: ABSENT on RC01."
  - 1.4 / 1.5 是否补上：**未验证**（#1045 仍 open，1.5.x release notes 无此项）。
- 转换项目的现实做法：acpbox「`usage` is zero if ACP does not provide token counts」；`[项目]` [pingu1m/acp-to-api](https://raw.githubusercontent.com/pingu1m/acp-to-api/main/README.md)「Token usage in responses is estimated from text length」。
  → **SAG 若在 ACP 路线上做计费 / 配额统计，数据源只能是估算或另行旁路。没有 usage 就没有账本，没有账本就没有配额路由**——这不是加字段能补的，是上游没发。

### 4.3 认证与运行限制

| 项 | 事实 | 来源 |
|---|---|---|
| auth method | `oauth-personal` / `oauth-business` / `gemini-api-key` / `agent-platform`（前身 vertex-ai）/ `terminal-login` | `[官方]` [Other editors (ACP)](https://antigravity.google/docs/ide/extensions/acp/) |
| settings.json | `<GEMINI_HOME>/antigravity-acp/settings.json`，形如 `{"auth":{"type":"oauth-personal"}}`；**启动时读一次**，运行中改无效 | `[项目]` ACP-PROTOCOL-REFERENCE；[acpx](https://github.com/openclaw/acpx/blob/main/agents/Antigravity.md) |
| 未认证错误 | `session/new` → `-32000 "Authentication required"`，文案直接列出 4 种 method 与 settings.json 路径 | `[项目]` 同上 |
| GEMINI_HOME | 账号 profile 根目录，**默认 `~/.gemini`，不是 `$HOME`**；「只设 `GEMINI_HOME` 不实现拒绝浏览器登录的策略」，策略要自己实现 | `[项目]` acpx |
| 凭据存储 | `AGY_ACP_FORCE_FILE_STORAGE=1` 强制 profile 内文件存储；默认可能走 keychain（#1045 称 token 在 macOS keychain，只有 `agy_acp_server.par` 能读） | `[项目]` acpx；`[官方]` #1045 |
| token 文件 | `~/.gemini/antigravity-acp/acp_token.json`（0600，约 510 B）；**跨进程重启有效** | `[项目]` ACP-PROTOCOL-REFERENCE run 6 |
| 是否继承 `agy` 登录态 | registry 二进制：**否**（"a separate download and sign-in"）；`agy acp`：**是**（uses the credentials you cached with an interactive `agy` session） | `[项目]` acpx；`[官方]` 文档 |
| token 刷新 | **runtime 自己管**（有 `access_type=offline` 的 refresh token）；凭据过期时可能**直接触发交互式登录**（acpx 警告 "Expired credentials may otherwise start interactive sign-in"）→ 网关**无正当接口介入刷新** | `[项目]` acpx；ACP-PROTOCOL-REFERENCE |
| 配置不持久 | 重启后 `session/load` 回来的 `currentModelId` 是默认值，model / effort / mode **必须由驱动重新 set** | `[项目]` ACP-PROTOCOL-REFERENCE run 6 |
| 服务端持久化 | `~/.gemini/antigravity-acp/conversations/<sessionId>.db`（SQLite WAL，`steps.step_payload` 是不透明 blob）；"Never poll these DBs" | `[项目]` 同上 |

**一条容易被误引的边界**：FAQ 的「Can I use Antigravity CLI headless mode in third-party coding agents」→「not supported under consumer plans」（`[官方]` [FAQ](https://www.antigravity.google/docs/faq/)）。**`agy acp` 不是 headless 模式**（它是 ACP server），这条 FAQ 不直接命中 ACP 路线，但它是 `-p` 包装路线的红线。**两者在合规上不等价——这是 ACP 路线唯一的政策优势，也是它存在的全部意义。**

### 4.4 运行成本（硬数据）

| 维度 | 数据 | 来源 |
|---|---|---|
| 首次获取 | **~1.5 GB** `.par`（原文："a ~1.5 GB server binary downloaded from Google"） | `[项目]` [pi-antigravity-bridge ENGINES.md](https://cdn.jsdelivr.net/npm/@estebanforge/pi-antigravity-bridge@1.6.2/docs/ENGINES.md) |
| 冷启动（Linux） | spawn → `initialize` **5–6 s**（页缓存热，三次一致） | `[项目]` ACP-PROTOCOL-REFERENCE |
| 冷启动（Windows） | 1.1.1 上 **~16 s / 每次 spawn** | `[社区]` [discuss #183427](https://discuss.ai.google.dev/t/acp-server-1-1-1-official-agy-acp-server-cold-starts-in-16s-on-every-windows-spawn/183427) |
| 常驻内存 | 稳态 **RSS ~327 MB**（5 min 混合负载；VSZ 5.3 GB 是 TCMalloc 预留） | `[项目]` ACP-PROTOCOL-REFERENCE |
| 进程模型 | 默认**一进程 N 会话**；隔离用 `--process-per-session`（每会话独立 worker，**HTTP MCP 必需**） | `[官方]` [Other editors (ACP)](https://antigravity.google/docs/ide/extensions/acp/) |
| 通信 | stdio 上的 **NDJSON JSON-RPC 2.0，单管道**（请求-响应 + 通知混流） | `[项目]` + `[官方]` release 1.5.1 |
| 并发 | `session/prompt` 是**长事务**；`acp-to-api` 直接写 "Only one prompt runs per provider at a time (serialized via prompt lock)"；并发上限 = 内存 / CPU | `[项目]` acp-to-api README |
| 升级 | 二进制替换（1.5.x 每 1–2 天一发）；运行中进程需持租约（t3code 实现） | `[官方]` releases；`[项目]` t3code |

**二进制缺陷（不是配置问题，SAG 现有的「4xx 不重试 / TTFB 超时可重试」语义覆盖不到）**：

| 缺陷 | 现象 | 来源 |
|---|---|---|
| 启动 SIGILL | 1.4.1 / 1.4.5 上 absl `DieBecauseNull`，非 SIMD；**与 CPU 亲和性相关**，12 次交错 A/B 可复现（单一项目记录，无第二方复现） | `[项目]` [tiezbro/paseo-agy-acp](https://github.com/tiezbro/paseo-agy-acp) |
| 分块响应截断卡死 | 1.4.5 在 `session/new` 读 `fetchAvailableModels` 时因分块响应截断而卡死 | `[项目]` 同上 |
| stdin EOF 不退出 | Windows 1.4.0：stdin EOF 后进程继续运行 | `[社区]` [discuss #189880](https://discuss.ai.google.dev/t/windows-acp-server-1-4-0-remains-running-after-stdin-eof/189880) |
| 会话永久卡死 | 会话报 502 "model unreachable" 后**永久卡死**，新会话正常 | `[社区]` [discuss #196254](https://discuss.ai.google.dev/t/acp-sessions-become-permanently-wedged-with-502-the-model-is-currently-unreachable-while-fresh-sessions-succeed/196254) |
| ARM64 崩溃 | 39-bit VA 的 TCMalloc 崩溃，**官方承认是已知问题、正在修** | `[官方]` [discuss #197131](https://discuss.ai.google.dev/t/conflicting-guidance-on-using-t3-code-with-personal-google-ai-subscriptions/197131) |

**成本结论（一句话）**：ACP 路线把「每 token 的边际成本」换成了「每会话的固定成本」——**~330 MB RSS + 5–6 s 冷启 + 一次性 1.5 GB 下载 + 无 usage 计量**。若目标是「多租户 OpenAI 兼容面」，这条路的单位经济性差一个量级；但它是唯一有官方书面许可的自研协议路线。

### 4.5 已知无人做对过「Antigravity ACP → OpenAI」

**协议层面可行，产品层面无人做对。** 公开的通用「ACP → OpenAI」项目（`[项目]` [EvilFreelancer/acpbox](https://github.com/EvilFreelancer/acpbox/blob/main/docs/api-mapping.md)、[chaojimct/cli-agent-gateway](https://github.com/chaojimct/cli-agent-gateway/blob/main/README.zh-CN.md)、[pingu1m/acp-to-api](https://raw.githubusercontent.com/pingu1m/acp-to-api/main/README.md)、[vadim-vyb/acp-openai-gateway](https://github.com/vadim-vyb/acp-openai-gateway)）无一支持 Antigravity 特例：

- `acp-to-api` 的 provider 列表里**没有 Antigravity**；
- `cli-agent-gateway` 只在**特定 agent 的 permission 语义**上近似，未适配 Antigravity 的 `interaction_` 提问、slug 内嵌 effort、`--uid=`；
- `acpbox` 干脆放弃工具语义（非文本 update 塞进非标准 `acp` 字段）、`usage` 恒 0，并把 `modes` 当模型暴露。

**结构性信号**：`acp-to-api` 的「one prompt per provider at a time」说明 `session/prompt` 是长事务，串行化几乎是默认选择 → **并发 = 多进程 = 内存线性增长**。所有项目都在同一处妥协：**把「agent 自执行」伪装成「客户端执行」**，且没有优雅解。因此「Antigravity ACP → OpenAI chat completions」至今**没有被做对过**；最接近的都不以 OpenAI 面为终点。

---

## 5. 合规路径 B：官方 API key 替代

**这是 FAQ 唯一明确推荐的合规出口**（§3.4），也是唯一能承诺 SLA 的路线。

| 路径 | 内容 | 边界与注意 |
|---|---|---|
| **Google AI Studio API key** | `GEMINI_API_KEY`；FAQ 明确推荐用于「第三方 coding agent + Gemini」 | 与 Antigravity 订阅额度**无关**；按 API 计费；不受 Antigravity 的「单用户交互式开发」限制 |
| **Vertex AI** | Google Cloud 条款下的企业 / 项目级路径 | 计费与配额走 GCP；合规性由 Google Cloud 条款定义 |
| **ACP server 内置 key auth** | ACP server 的 auth methods 里本就有 **`gemini-api-key`** 与 **`agent-platform`**（前身 vertex-ai） | 走这两个 method 时已**不是**「订阅额度转换」，而是正当的 key 通道——这是 §4 ACP 路线内的合规出口 |
| **Gemini Enterprise** | Enterprise 走 **Google Cloud 条款**（[Plans 文档](https://antigravity.google/docs/plans/)） | 可能是唯一「用官方后端 + 合规条款」的路径，但**未验证**是否向第三方客户端开放 `v1internal`（见 §9） |

**关键取舍**：官方 key 路线**牺牲的是「用订阅额度」这件事本身**——它根本不使用 Antigravity 的个人订阅配额。所以它不是「把 Antigravity 转成 API」，而是「用 Google 官方 API 替代 Antigravity」。对 SAG 而言，这是 Tier A（可承诺）的唯一来源。

**明确的边界（避免误读）**：

- Antigravity 本体**不支持 BYOK / BYOE**（§3.4），所以「自带 key 走 Antigravity 通道」**不存在**。
- 反向引用 2026-02 封禁的教训：**连带封禁不会发生**在纯官方 key 路线上（那不在 Antigravity 服务范围内）。

---

## 6. 合规路径 C：开放 ACP 生态

**定位**：Agent Client Protocol 是一条**独立于订阅额度**的开放标准线。

- `[协议]` 公开规范，Apache-2.0，v1 stable（2026-11-02 的 Rust crate v1.15.1 / schema v1.29.1）与 v2 Draft（2026-07-20 发布）；由 Zed 发起、现 Zed + JetBrains 共同开发。仓库 [agentclientprotocol/agent-client-protocol](https://github.com/agentclientprotocol/agent-client-protocol)（4,596★，push 2026-11-07）。
- **责任边界由官方自己划定**：Zed 文档明确把计费与法律条款推给 agent 提供方——"Billing, legal terms, retention, and data handling are between you and the agent provider."（[Zed — External Agents](https://zed.dev/docs/ai/external-agents)）。→ **ACP 既不授权也不禁止把订阅额度转成 API**。
- **前例**：Gemini CLI 是 ACP 的首个参考实现（[Zed 博客 2025-08-27](https://zed.dev/blog/bring-your-own-agent-to-zed)），但 Gemini CLI 已于 **2026-06-18** 停止为个人 / Pro / Ultra 层服务（**二手来源**，[CloudZero](https://www.cloudzero.com/blog/google-antigravity-pricing/)，2026-09-18，**未验证**），用户被迁往 Antigravity CLI。→ **协议稳定 ≠ 上游产品稳定**。
- **通用 ACP → OpenAI 转换器确实存在**，但现状见 §4.5（无一支持 Antigravity）。生态的可承诺性来自「协议层公开」，不来自「上游额度」。

**对 SAG 的含义**：ACP 可以作为「一个干净的 provider 类型」（协议公开、版本协商、能力协商、错误码规范），用于**编辑器 ↔ agent 互通**；但它**不解决「把 Antigravity 订阅额度转成 API」的问题**——那取决于 Antigravity 的政策口径（§3），而不是协议本身。

---

## 7. 边界外的路线（范围声明）

有一类做法被官方**明文禁止**，本报告不探索其技术细节：**① 直连 Google 私有上游端点**（Cloud Code `v1internal`）；**② 抽取或重放 OAuth 凭据**（含以第三方工具 / 代理 / harness 桥的形式复用凭据）；**③ 把 `agy -p` 包进第三方 agent 循环**；**④ 自动化多账号轮换**。

依据：`[官方]` ToS §6（"Using third party software, tools, or services to access the Service … is a breach of this Agreement"）+ FAQ 两条（点名 Claude Code / OpenClaw / OpenCode，并明确 wrap `agy` 与账号轮换 "not supported under consumer plans"）+ 2026-11-02 答复的禁止清单（§3.1）；且已有执法记录（2026-02 连带封禁、**2026-11-06 凭证级 `TOS_VIOLATION`**，§3.1.3）。

**范围声明**：上述路线的端点 / 请求信封 / 认证流程 / 客户端身份对齐 / 检测手段 / 账号池与出口 IP 等**技术细节不在本报告范围内**（合规重构已剔除）。两条路线（官方 ACP vs 裸 API）的逐维度对比、裸 API 侧限制骨架与场景化建议，见续篇 [antigravity-acp-vs-bare-api.md](./antigravity-acp-vs-bare-api.md)。

---

## 8. SAG 合规接入评估

### 8.1 架构契合度

SAG 现有架构事实（来自本仓库代码核对）：

| SAG 侧能力 | 位置 | 对本次接入的含义 |
|---|---|---|
| `LlmProvider` trait（标准化 Chat 请求 / 响应 + SSE chunk 流） | `sag-providers/src/traits.rs:14-43` | 官方 API key 上游可直接复用现有适配器 |
| `apis` 协议 catalog 工厂（`create_provider` 按 catalog 创建单实例并委派适配器） | `sag-providers/src/catalog.rs:14-34`、`sag-providers/src/lib.rs:19-55` | **没有 `provider_type` 分派**，新增官方 key provider 不需要新类型 |
| 任意 OpenAI 兼容端点：`apis.*.base_url` + headers / body overrides | `sag-config/src/config.rs:1297` 起 `ApiConfig`；`sag-config/src/loader/load.rs:195` 示例 | 官方 key 上游（如 AI Studio 的 OpenAI 兼容面）加一个 provider 节点即可 |
| `provider_type` 已移除，迁移清理逻辑在 loader | `sag-config/src/loader/save.rs:1078-1088` | 不要再引入「按来源分派」的类型枚举 |
| Admin API 增删改查 provider + 模型列表 / fetch-models / probe | `sag-admin/src/admin_providers.rs:258,279,347,435,465`；`sag-admin/src/admin.rs:40-48` | 凭证生命周期、探活可复用现有通道 |
| 重试语义：TTFB 超时可重试；4xx 不重试 | `sag-config/src/config.rs` ProviderConfig TTFB 注释；`docs/spec/dispatch/changes/0004-upstream-4xx-non-retryable.md` | 官方 key 上游的 429 分类仍要正确；ACP 路线的失败在**进程层**，现有语义覆盖不到 |
| 目标池 / 会话粘性 / 断路器 | `sag-router/src/session.rs`、`docs/spec/circuit-breaker/spec.md` | 会话粘性已有落点 |

**结论（与旧版相反）**：**官方 API key 路线在 SAG 侧几乎零成本**——这正是它作为 Tier A 可承诺通道的价值。反观 ACP 路线**不契合**现有 trait：ACP 是「一轮长事务 + 通知流」，不是「请求 → chunk 流」，需要新造一整套运行时（§8.3）。

### 8.2 合规分层（Tier 重定义）

| 层 | 定义 | 具体来源 | SAG 策略 |
|---|---|---|---|
| **Tier A**（可承诺 SLA） | 官方 API Key / 公开协议 + 自带认证 | **Google AI Studio / Vertex 官方 key**；ACP server 的 `gemini-api-key` / `agent-platform` auth method；通用 ACP agent（协议公开、版本协商、能力协商） | 可作为正式上游；在 `/v1/models` 正常暴露；纳入容量规划 |
| **Tier B**（可提供但不承诺） | **官方 ACP 服务器驱动型** | `agy acp` / registry `antigravity-acp`，**仅 Gemini**、单用户交互式、不代管凭据 | 合规但功能 / 成本受限；定位**自用或实验**，不与其上层承诺 SLA。判据：① usage 全缺 → 无法计费；② 工具语义反 → 无法承诺 `tools`；③ 进程级失败模式 → 需 watchdog；④ 模型范围政策限 Gemini |
| **~~Tier C~~**（不合规，**从评估中剔除**） | ~~token 重放 + 指纹伪装型~~ | ~~直连 `v1internal` / LS 桥接 / `agy -p` 包装 / 账号轮换~~ | **不在本报告评估范围**（ToS §6 明文禁止，已有凭证级封禁案例，见 §3.1、§7）。任何 SAG 的对外 provider 都不应建立在此类来源上 |

**Antigravity 相关定位的具体建议**：

- 若 SAG 要正式支持 Antigravity 生态，**首选官方 key 通道**（Tier A），并在文档里指引用户改用 Vertex / AI Studio —— 这正是 FAQ 自己给的唯一建议（§3.4），既是合规姿态也是产品差异化。
- 官方 ACP 路线若接入，**必须定位为 Tier B 实验通道**：独立命名、独立断路器 / 并发池 / 账本，**不与 Tier A 通道放在同一个 `/v1/models` 列表而不加标注**；响应头显式标注通道来源与实验性；**只暴露 Gemini**（回避 §3.3.1 的模型范围夹缝）。
- **`gemini-api-key` 是 ACP 路线内的合规出口**：走它时不再是「订阅额度转换」，而是正当 key 通道（§5）。

### 8.3 若走 ACP 路线需要的适配假设（协议语义相关）

裸 API 专属的适配假设（429 五分类、假 429、thoughtSignature、凭据刷新锁、指纹对齐等）已随不合规路线剔除；下表只保留**与 ACP 协议语义相关**的部分：

| # | 适配假设 | 具体要求 | 若不做会怎样 |
|---|---|---|---|
| 1 | **会话是显式能力，不是隐式副作用** | 必须自造「请求 → ACP sessionId」映射键；决定网关历史是否权威（与 ACP 侧历史双写时二选一）；`session/load` 会重放全文、`session/resume` 不会 | 上下文不一致，多客户端并发时串台或丢历史 |
| 2 | **工具语义要标注「谁执行」** | `observational`（agent 已执行，客户端不得执行、不得回传 tool_results、永不产生 `finish_reason: "tool_calls"`）/ `client-side`（必须回传 tool_result）/ `server-side`（网关无法支持，**显式 400 而非静默丢弃**） | 双重工具循环（agent 跑了一次，客户端又执行一次） |
| 3 | **`interaction_` 提问必须单独识别** | 它是「固定选项的问答」而非工具批准；自动批准会把第一个选项当成用户答案，自动拒绝会挂住会话 | 语义完全错误的回答，或会话阻塞 |
| 4 | **权限策略是部署期静态决策** | ACP 里「批准」是 client 的义务；协议不定义超时（Antigravity 1.5.1 才修「阻塞后续 prompt」）；无头网关只能自动批准或改 agent 配置 | 「逐次人工审批」语义 100% 丢失 |
| 5 | **usage 缺失要显式降级，不要填 0** | ACP server 不发 `usage_update` / `PromptResponse.usage`；网关只能估算或另行旁路；响应里给出 `degraded` / `usage_estimated` 标记 | 计费 / 配额路由失去数据源；用户侧无法分辨真实与估算 |
| 6 | **模型别名必须显式映射** | thinking level 烘在 slug 里 / 1.5.x 单列，口径不一；Google 改名即断（#19150）；`session/new` 目录是唯一真相 | `model` 字段与官方 slug 绑死，改名即 404 |
| 7 | **响应可能不完整但 HTTP 200** | 空流、半截 tool call、`stopReason` 之外的中间态不存在；需支持「priming 首 chunk 再提交 200」与 `degraded` 状态位 | 客户端拿到 200 空响应 |
| 8 | **错误码双层映射** | JSON-RPC error（`-32602` / `-32000` / `-32601`）→ HTTP + 上游原因 + 建议重试时间；把进程失败与请求失败分开 | 把所有错误压成 500 会摧毁客户端退避逻辑 |
| 9 | **版本漂移必须可观测** | 模型目录与二进制版本硬绑定；registry 落后于 `agy acp`；暴露「客户端 / 二进制版本」端点，漂移时告警一次 | 「一次对齐、长期可用」的假设不成立 |
| 10 | **合法性 / 可撤销性写进 provider 元数据** | 同一份 OpenAI 兼容契约下，Tier A（官方 key）与 Tier B（官方 ACP）的 SLA、ToS 风险、可撤销性不同 | 网关把不可承诺的东西承诺出去了 |

---

## 9. 未验证事项（合规相关子集）

> 本表只保留与**合规边界、合规路径限制**相关、且无法交叉核实的条目。裸 API 侧的技术未验证项（指纹全集、设备 attestation、`modelVersion` 改写等）已随不合规路线剔除。

| # | 未验证事项 | 为什么重要 | 需要什么才能验证 |
|---|---|---|---|
| **1** | **「1.5.1 全模型开放」与「11-02 政策文本仅 Gemini」谁是真口径** | **决定 ACP 路线能否暴露 Claude/GPT，也决定「合规」这个最大卖点是否成立。** release note（11-05）晚于政策答复（11-02）3 天且无任何政策说明 | 官方对 #1168 / registry #554 的书面答复，或一次官方文档改版 |
| 2 | Blyck 提问（#196885）是否在别处获答 | 决定「官方开口」是否覆盖「非编辑器类 ACP client」 | 该帖截至 2026-11-08 无回复 |
| 3 | 1.4.5（registry 二进制）是否仍有 `clientInfo.name` 白名单 | 决定 ACP 路线两条腿是否行为一致 | 无源码、无 changelog、无 1.4.5 实测帖；只能实跑二进制 |
| 4 | `agy acp` 与 registry `.par` 是否共享 `model_selection` / CCPA 代理层 | 决定 1.5.1 的开放是否也覆盖 registry 腿 | 只能从「都是 Python + 都有 `--uid=`」推断 |
| 5 | 1.5.x 是否补了 usage / `session/cancel` | 决定 ACP 路线能否计费、能否中断 | #1045 仍 open，无实测 |
| 6 | `plan` 会话模式在 1.5.x 上是否只读 | RC01 的 `/plan` 会直接写文件；1.5.0 号称加了 `plan` | 实跑 1.5.3 观察是否有文件写入 |
| 7 | 官方文档「第三方模型仅限固定编辑器名单」与 release note 的冲突是否会收敛 | 决定 §3.3.1 的边界何时能被引用为稳定事实 | 官方文档改版或书面澄清 |
| 8 | 1.5.x 的 thinking level 与 mode 的确切 payload | 决定 SAG 的 model alias 映射是否可行 | 1.4（slug 内嵌）与 1.5（独立选项）口径不同，需实测 |
| 9 | 1.4.5 上 SIGILL 的触发条件（CPU 亲和性 vs 具体 CPU 型号） | 决定 ACP 路线能否在生产机群稳定运行 | 单方 12 次 A/B 可复现，缺第二方复现 |
| 10 | 「仅跑官方 ACP 二进制被封」是否真的零案例 | 这是 ACP 路线合规判断的关键反证；「未发现」不等于「不存在」 | 需要更大范围的检索或官方统计 |
| 11 | ACP 路线上「真实模型回显」是否存在（静默替换在 ACP 侧的表现） | 决定网关能否发现降级 | 无实测 |
| 12 | Enterprise / Gemini Enterprise 通道是否允许第三方客户端访问 `v1internal` | **这可能是唯一「官方后端 + 合规条款」的路径** | 需企业合同与官方答复 |
| 13 | Anthropic 商业条款是否对「经 Google 转出的 Claude」实际执法 | 决定 ACP 路线暴露 Claude 时的双重合规风险 | 无公开案例 |
| 14 | Antigravity Additional ToS §6 是否有版本变更 | 决定 §3.1 引用文本的有效期 | 本次未核对 ToS 版本号 / 修订日期 |
| 15 | Gemini CLI 2026-06-18 停服 | 仅二手媒体（CloudZero），影响 ACP 生态的时间线判断 | 官方 changelog 原文 |

---

## 10. 来源清单（精选合规边界相关）

> 正文中以行内链接引用的均为下表条目或其直接延伸（issue 号 / PR 号）。星数 / push 时间取 GitHub REST API 于 2026-11-08 的实测；`agent.json` 版本取 raw 原文。

### 10.1 政策与合规一手文本

- `[官方]` [Google Antigravity - Terms of Service](https://antigravity.google/terms/)（§6 / §8，查阅 2026-11-08）
- `[官方]` [FAQ | Google Antigravity Docs](https://www.antigravity.google/docs/faq/)（第三方软件 / headless wrap / 地域白名单，查阅 2026-11-08）
- `[官方]` [discuss #192027 — ToS clarification: is T3 Code / official Antigravity ACP agent permitted](https://discuss.ai.google.dev/t/tos-clarification-is-t3-code-official-antigravity-acp-agent-permitted-under-the-antigravity-terms-of-service/192027/2)（2026-10-26，首次「开口」）
- `[官方]` [discuss #197131 — Conflicting guidance on using T3 Code](https://discuss.ai.google.dev/t/conflicting-guidance-on-using-t3-code-with-personal-google-ai-subscriptions/197131)（2026-11-02，permitted + Gemini-only + 封号范围 + ARM64 已知问题）
- `[官方]` [discuss #196885 — Blyck 请求书面确认（无答复）](https://discuss.ai.google.dev/t/tos-clarification-desktop-app-blyck-using-the-official-antigravity-acp-binary-over-acp-with-the-users-own-personal-plan/196885)（2026-10-30）
- `[官方]` [discuss #198430 — 政策反转记录 + 256K compaction clamp](https://discuss.ai.google.dev/t/1m-advertised-context-vs-a-256k-compaction-clamp-in-the-shipped-binaries-plus-quota-and-agy-p-policy-questions-desktop-2-29-0-cli-1-5-1/198430)（2026-11-05）
- `[官方]` [gemini-cli Discussion #20632 — Addressing Antigravity Bans & Reinstating Access](https://github.com/google-gemini/gemini-cli/discussions/20632)（2026-02-27，连带封禁 + 全量解封 + 二次永久）
- `[官方]` [discuss #198478](https://discuss.ai.google.dev/t/appeal-antigravity-agy-cli-disabled-for-tos-403-requesting-manual-review/198478) / [#142192](https://discuss.ai.google.dev/t/appeal-for-account-disabling-unexpected-tos-violation-via-3rd-party-pi-dev-agent-oauth-bridge/142192)（封号申诉，均为 bridge / `-p` 型）
- `[项目]` [CLIProxyAPI #7296 — 2026-11-06 凭证级 `TOS_VIOLATION`](https://github.com/router-for-me/CLIProxyAPI/issues/7296)
- `[社区]` [VentureBeat — Google clamps down on Antigravity malicious usage](https://venturebeat.com/orchestration/google-clamps-down-on-antigravity-malicious-usage-cutting-off-openclaw-users)（2026-02-23）
- `[社区]` [Ground Truth — Google's Antigravity terms ban third-party clients outright](https://groundtruth.day/news/googles-antigravity-terms-ban-third-party-clients-outright.html)（2026-09-03）
- `[社区]` [Zentor — Antigravity account suspended](https://zentor.ai/blog/antigravity-account-suspended-third-party)（2026-09-04，更新至 2026-10-27）
- `[社区]` 封号潮个案：[Linux.do 3053739](https://linux.do/t/topic/3053739)（10-21）/ [discuss #197297](https://discuss.ai.google.dev/t/account-suspended-again-multi-agent-setup-calling-antigravity-cli/197297)（11-01）/ [Linux.do 3112874](https://linux.do/t/topic/3112874)（11-03）/ [Linux.do 3129568](https://linux.do/t/topic/3129568)（11-05）
- `[官方]` [Plans | Google Antigravity Docs](https://antigravity.google/docs/plans/)（BYOK 不支持、Enterprise 走 Google Cloud 条款，查阅 2026-11-08）
- `[官方]` [Choose a model | Google Antigravity Docs](https://antigravity.google/docs/models/)（2026-11-18 撤下，查阅 2026-11-08）

### 10.2 官方 ACP 文档与二进制

- `[官方]` [Agent Client Protocol (ACP) | Antigravity CLI Docs](https://antigravity.google/docs/cli/acp/)（查阅 2026-11-08）
- `[官方]` [Other editors (ACP) | Antigravity Docs](https://antigravity.google/docs/ide/extensions/acp/)（第三方模型名单、thinking level、modes、flags、auth methods）
- `[官方]` [antigravity-cli release 1.5.0](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.0)（2026-11-04，`agy acp` 进 `--help`、`session/resume`、`--process-per-session`）
- `[官方]` [antigravity-cli release 1.5.1](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.1)（2026-11-05，「every model to every ACP client」+ 会话 bug 修复）
- `[官方]` [antigravity-cli releases 列表](https://github.com/google-antigravity/antigravity-cli/releases)（1.5.0 / 1.5.1 / 1.5.3 均在 2026-11-04~07）
- `[官方]` [registry/antigravity-acp/agent.json（raw）](https://raw.githubusercontent.com/agentclientprotocol/registry/main/antigravity-acp/agent.json)（2026-11-08 拉取：`version 1.4.5`，Google LLC，proprietary）
- `[项目]` [agent.json 提交历史](https://github.com/agentclientprotocol/registry/commits/main/antigravity-acp/agent.json)（1.0.0 → 1.4.0 可见区）

### 10.3 官方 ACP 路线的限制证据

- `[项目]` [antigravity-cli issue #1168](https://github.com/google-antigravity/antigravity-cli/issues/1168)（2026-10-05，open；`clientInfo.name` 白名单根因复现）
- `[项目]` [antigravity-cli issue #1318](https://github.com/google-antigravity/antigravity-cli/issues/1318)（2026-10-22，open；`supports_third_party_models()` 源码）
- `[项目]` [registry issue #554](https://github.com/agentclientprotocol/registry/issues/554)（2026-08-28，open，Google 零回应；去过滤后仍需 5 处 provider 兼容改造）
- `[官方]` [antigravity-cli issue #1045](https://github.com/google-antigravity/antigravity-cli/issues/1045)（2026-09-18，open；usage 全缺）
- `[项目]` [registry issue #741](https://github.com/agentclientprotocol/registry/issues/741)（2026-11-01，open；1.4.5 媒体文件被当文本）
- `[项目]` [t3code issue #19150](https://github.com/pingdotgg/t3code/issues/19150)（2026-10-13 → 11-01 closed；模型目录与二进制版本硬绑定）
- `[项目]` [t3code issue #15378](https://github.com/pingdotgg/t3code/issues/15378)（2026-10-03 → 10-21 closed；`customModels` 旁路实验）
- `[项目]` [ACP Protocol Reference — antigravity-acp（verified live 2026-09-03）](https://cdn.jsdelivr.net/npm/@estebanforge/pi-antigravity-bridge@1.4.7/docs/ACP-PROTOCOL-REFERENCE.md)（14 种 update、`-32601` cancel、无 usage、settings.json / GEMINI_HOME / token 文件、MCP 实测坑）
- `[项目]` [pi-antigravity-bridge — ENGINES.md](https://cdn.jsdelivr.net/npm/@estebanforge/pi-antigravity-bridge@1.6.2/docs/ENGINES.md)（~1.5 GB 二进制）
- `[项目]` [openclaw/acpx — agents/Antigravity.md](https://github.com/openclaw/acpx/blob/main/agents/Antigravity.md)（`interaction_` 提问、GEMINI_HOME、凭据过期弹登录）；[acpx 权限档位](https://acpx.sh/permissions.html)
- `[项目]` [pingdotgg/t3code PR #9348](https://github.com/pingdotgg/t3code/pull/9348)（2026-09-03 merged；`interaction_` → 用户输入问题）
- `[项目]` [tiezbro/paseo-agy-acp](https://github.com/tiezbro/paseo-agy-acp)（唯一深挖官方 ACP 内核缺陷：SIGILL、分块响应截断卡死）
- `[项目]` ACP → OpenAI 通用转换器（均不支持 Antigravity）：[EvilFreelancer/acpbox](https://github.com/EvilFreelancer/acpbox/blob/main/docs/api-mapping.md)、[chaojimct/cli-agent-gateway](https://github.com/chaojimct/cli-agent-gateway/blob/main/README.zh-CN.md)、[pingu1m/acp-to-api](https://raw.githubusercontent.com/pingu1m/acp-to-api/main/README.md)、[vadim-vyb/acp-openai-gateway](https://github.com/vadim-vyb/acp-openai-gateway)
- `[社区]` [discuss #183427](https://discuss.ai.google.dev/t/acp-server-1-1-1-official-agy-acp-server-cold-starts-in-16s-on-every-windows-spawn/183427)（Windows 冷启 ~16 s）/ [#189880](https://discuss.ai.google.dev/t/windows-acp-server-1-4-0-remains-running-after-stdin-eof/189880)（stdin EOF 不退出）/ [#196254](https://discuss.ai.google.dev/t/acp-sessions-become-permanently-wedged-with-502-the-model-is-currently-unreachable-while-fresh-sessions-succeed/196254)（502 后永久卡死）

### 10.4 ACP 协议规范与开放生态

- `[协议]` [Prompt Turn](https://agentclientprotocol.com/protocol/v1/prompt-turn)（`session/update` 全表、`stopReason`）
- `[协议]` [Session Setup](https://agentclientprotocol.com/protocol/v1/session-setup)（`session/new` 的 cwd / mcpServers、`load` vs `resume`）
- `[协议]` [Tool Calls](https://agentclientprotocol.com/protocol/v1/tool-calls)（agent 自执行、`request_permission` 结构）
- `[协议]` [Session Modes](https://agentclientprotocol.com/protocol/v1/session-modes)（modes 与 configOptions 双轨）
- `[协议]` [Session usage stabilized](https://agentclientprotocol.com/announcements/session-usage-stabilized)（2026-06-05）
- `[项目]` [agent-client-protocol issue #1694](https://github.com/agentclientprotocol/agent-client-protocol/issues/1694)（文档与 schema 不一致，2026-07-16 → 09-23 closed）
- `[协议]` [Zed — External Agents 文档](https://zed.dev/docs/ai/external-agents)（责任边界）/ [Bring Your Own Agent](https://zed.dev/blog/bring-your-own-agent-to-zed)（2025-08-27）/ [ACP Registry 发布](https://zed.dev/blog/acp-registry)（2026-01-28）
- `[项目]` [agentclientprotocol/agent-client-protocol](https://github.com/agentclientprotocol/agent-client-protocol)（4,596★，push 2026-11-07）
- `[项目]` [Zed Discussion #57221](https://github.com/zed-industries/zed/discussions/57221)（Gemini CLI 停服 / Antigravity，2026-05-20）
- `[项目]` [CloudZero — Google Antigravity pricing 2026](https://www.cloudzero.com/blog/google-antigravity-pricing/)（2026-09-18，Gemini CLI 6/18 停服二手来源）

### 10.5 SAG 本仓库代码位置（本轮核对，非外部来源）

`docs/index.md:4-6`、`docs/spec/index.md`、`docs/spec/dispatch/changes/0004-upstream-4xx-non-retryable.md`、`docs/spec/circuit-breaker/spec.md`、`sag-providers/src/traits.rs:14-43`、`sag-providers/src/catalog.rs:14-34`、`sag-providers/src/lib.rs:19-55`、`sag-config/src/config.rs:1297` 起 `ApiConfig`、`sag-config/src/loader/load.rs:195`、`sag-config/src/loader/save.rs:1078-1088`、`sag-admin/src/admin_providers.rs:258,279,347,435,465`、`sag-admin/src/admin.rs:40-48`、`sag-router/src/session.rs:11`。
