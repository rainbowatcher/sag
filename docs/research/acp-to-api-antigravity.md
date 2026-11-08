# 把 agent 客户端协议（泛称「ACP」）转成 OpenAI 兼容 API：限制全景与 Antigravity 深挖

> **调研日期**：2026-11-08（北京时间 / UTC 同日）
> **面向**：SAG（Smart AI Gateway）接入决策
> **信息时效性说明**：本文涉及的协议端点、模型目录、配额档位、ToS 条款均处于高频变动状态。所引用一手材料中，最短的失效周期是 **71 分钟**（Google 论坛对 `agy -p` 的立场反转，2026-10-08），最长的是月度级的模型下线。**本文结论的有效期以「下一次上游客户端大版本发布」为上限，建议超过 30 天即视为过期，重新核对 ToS 与模型表。**
> **标注约定**：`[官方]` = 厂商一手文档/论坛官方回复；`[项目]` = 转换项目 README/issue/PR；`[社区]` = 用户帖（附可信度）；`未验证` = 无法交叉核实，不得当作确定事实使用。

---

## 1. 摘要（TL;DR）

把「agent 客户端协议」转成 OpenAI 兼容 API 这件事，**技术上的核心是协议翻译，商业上的核心是「用别人家的客户端身份去花别人家的订阅额度」**。前者是工程问题、可解且已被反复验证（CLIProxyAPI 一家覆盖 8 类上游，WindsurfAPI 覆盖 5 套出口协议，ACP 生态已有 4+ 个通用转换器）；后者是商业与风控问题，**不可解，只能缓解**。

对 SAG 的具体落地含义：**架构上几乎零成本，政策上不可承诺，运维上不收敛。** SAG 已有的 `apis` 协议 catalog + 任意 `base_url` + headers/body overrides 意味着「OpenAI 兼容的转换服务」可以直接作为一个普通 provider 节点接入，不需要新增 provider 类型；但这也意味着 SAG 会把一个**随时可能整体失效、且失效原因是封号而非 5xx** 的上游，包装成看起来很正常的 `/v1/chat/completions`。

核心结论：

- **「ACP」是三重撞名**。用户泛称的「ACP 私有客户端协议」在 Antigravity 上实际指 ②Cloud Code `v1internal`（私有上游 HTTP 接口）或 ③本地 language server Connect-RPC（Codeium 血统），**不是** ①Zed 的 Agent Client Protocol 公开标准——虽然后者 Google 官方自己就提供（`antigravity-acp`），但它在 `oauth-personal` 下被本地过滤器限制为**仅 Gemini 模型**。
- **政策是硬天花板，不是灰色地带**。ToS §6 书面禁止「用第三方软件/工具/服务访问服务」，FAQ 点名了 Claude Code / OpenClaw / OpenCode 这三个正是「转 API」最主流的客户端。2026-02 与 2026-10/11 各有一次封禁潮，且 2026-11-06 的封禁已细化到**服务级 + 凭证级**（3 个凭证中 2 个中招，第 3 个同条件 200）。
- **「429 = 限速」这个假设在该通道上是错的**。已复现：请求体 `systemInstruction` 里含特定字面文本（如 `<system-conventions>` + RFC 2119 列表、`You are Codex, an agent based on GPT-5.`）会触发**内容级 WAF**，返回**假的** `429 RESOURCE_EXHAUSTED`，而账号配额完好、官方 App 同账号同模型正常。
- **参数语义大面积失真，而且是「接受但静默失效」**。Gemini 3.6 Flash 及之后模型官方忽略 `temperature` / `topP` / `topK`；`frequencyPenalty` / `presencePenalty` 直接报错；`max_tokens` 有硬顶；上下文广告 1M、客户端实际 ~256K 就压缩。对网关而言，「静默丢弃用户参数」比报错更危险。
- **`thoughtSignature` 是最大的隐性状态，也是与 SAG 无状态定位的直接冲突**。OpenAI 的 `chat/completions` 协议里没有「思考签名」这个概念；要支持 thinking 模型的多轮工具调用，只有「网关内维护 per-conversation 签名存储」或「用哨兵绕过上游校验」两条路，各自有明确代价。
- **运维成本 ≈ 上游客户端发布频率，且失效是整体性的**。2026 年 2–11 月，UA 版本号 gate、字段白名单、端点身份绑定、模型下线这四类破坏性变更**各至少发生过一次**；一个 5.8 万星的 Go 项目仍挂着 824 个 open issue，其中大量是「上游变了」而非「代码有 bug」。
- **对 SAG 的建议**：把 Antigravity 通道定位为**可弃的实验通道**（Tier C），与合规通道（AI Studio / Vertex API key、官方 `crsr_` 类 API Key）做硬隔离，并对外声明其不可承诺 SLA。ACP（真正的 Agent Client Protocol）反倒是可以认真评估的一类「干净 provider」——但那是另一条路，不解决订阅额度问题。

---

## 2. 术语澄清（必须靠前，否则后续全部判断会错位）

中文生态里「ACP」至少有三个互不相干的所指。混淆它们会直接导致架构判断错误。

| # | 「ACP」实际所指 | 是什么 | 协议/传输 | 在 Antigravity 生态里的位置 |
|---|---|---|---|---|
| ① | **Agent Client Protocol**（Zed 发起，现 Zed + JetBrains 共同开发，Apache-2.0） | 编辑器/IDE ↔ coding agent 之间的通信标准，**公开规范**，v1 共 18 个方法/通知 | JSON-RPC 2.0，本地 stdio，远程 HTTP/WebSocket（远程仍标 work in progress） | Google **官方**提供 `antigravity-acp` 服务器（`agy acp`），在 ACP Registry 里是 v1.3.0，作者标 "Google LLC" |
| ② | **Cloud Code `v1internal`** | Google 私有上游 HTTP 接口，Gemini 形状 JSON/SSE | HTTPS + JSON + SSE（**不是** protobuf gRPC，见 §4.2 争议项） | 这是**所有** `antigravity2api` 类项目实际打的靶子 |
| ③ | **本地 language server Connect-RPC** | Antigravity 桌面/IDE 与 Codeium 血统的 Go `language_server` 之间的私有 RPC | Connect RPC v1（JSON over HTTP），自签 TLS，`127.0.0.1:随机端口`，需 `x-codeium-csrf-token` | 第一代架构的桥接层；①出现之前「驱动本机 LS」类项目的入口 |

**为什么必须区分**：

- **① 是公开标准，Google 自己支持它，但它被阉割了。** 官方 ACP 服务器在 `oauth-personal` 认证下**只暴露 Gemini 模型**，Claude / GPT-OSS 被本地过滤器丢弃。证据是 [agentclientprotocol/registry issue #554](https://github.com/agentclientprotocol/registry/issues/554)（2026-08-28）：作者读了分发包里的 Python 源码 `acp_server/model_selection.py`，其中 `if not ccpa_id.startswith("gemini"): continue`，随后 `server.py::_apply_session_model` 对非 Gemini ID 抛 `-32602`，但错误信息伪装成 "not available for the current authentication method"。Google 工作人员在论坛确认了同一约束："outside of first-party IDE integrations, **model selection in ACP clients is limited to the Gemini models included in your plan**" —— [discuss.ai.google.dev #192027 第 2 帖](https://discuss.ai.google.dev/t/tos-clarification-is-t3-code-official-antigravity-acp-agent-permitted-under-the-antigravity-terms-of-service/192027/2)（2026-10-26）。
- **②③ 是私有的、逆向得来的，且属于 ToS 禁止范围。** 它们形态与 ① 相似（都是「事件流 + 工具 + 会话」），但稳定性差一个量级：公开协议不会因为「你没跟上客户端版本」而失效，私有协议会（见 §5.4）。
- **① 的语义是 agent，不是 chat completions。** ACP 是 `session/prompt` + 工具授权 + 文件读写，agent 自己执行工具（"Agents run tool calls themselves"，[ACP v1 Tool Calls](https://agentclientprotocol.com/protocol/v1/tool-calls)）。把它「降维」成 `/v1/chat/completions` 必须丢掉工具执行权，或自己实现客户端侧的 `fs/*`、`terminal/*`、`session/request_permission`。

**命名漂移的实证**（说明为什么必须在内部术语表里钉死）：把 Cursor 自有协议称作 ACP（[AsterMux 介绍文](https://www.jxxy.net/ai/articles/astermux-cursor-acp-openai-anthropic-gateway/)，2026-09-07）；把 ACP 错误展开成 "Agent Collaboration Protocol"（[Eave Luo 博客](https://eaveluo.com/blog/ai/openclaw/acp-agent-protocol)，2026-03-13）；把 ACP 当「任何 agent 接入方式」的泛称（[fanlv.fun](https://www.fanlv.fun/2026/04/25/acp/)，2026-04-25）。

> **SAG 内部术语建议**：`acp`（Agent Client Protocol，公开标准，可对接、可承诺协议层稳定性）/ `cloudcode-v1internal`（Google 私有上游）/ `antigravity-ls-connect`（本地 LS 桥接）。三者不要共用同一个词。

---

## 3. 这类转换在做什么：三种架构路线

所有「agent 客户端协议 → OpenAI 兼容 API」的项目，按**凭证与请求由谁发出**可分为三条路线。这条分界线比「支持哪个协议」重要得多，因为它决定了风险特征。

| 路线 | 做法 | 上游看到什么 | 技术难度 | 政策风险 | 代表项目 |
|---|---|---|---|---|---|
| **A. 直连上游 + token 重放** | 自己持有 OAuth refresh token，直接调上游 HTTP 端点（`v1internal:streamGenerateContent`） | 一个「自称官方客户端」的伪造请求，指纹靠自己复刻 | 低（最轻） | **高**（明确的凭证盗用）；2026-11-06 起有凭证级封禁 | su-kaka/agy2api、liuw1535/antigravity2api-nodejs、CLIProxyAPI、AIClient2API |
| **B. 本地 LS 桥接 / MITM** | 驱动或劫持本机 `language_server` 二进制，代它发请求 | 真的本地客户端进程，但出口流量被改写 | 高（必须本机装 IDE + 进程/端口/token 发现 + 指纹复刻） | **中高**（仍属「用第三方工具访问服务」） | jackwener/open-antigravity、zhe-gu/zero-gravity、frieser/antigravity-proxy |
| **C. 官方 CLI / 官方 ACP 二进制驱动** | 起 `agy` / `agy_acp_server` 子进程，说它自己的协议 | **真客户端本身**，指纹问题消失 | 中（进程管理、并发、超时、状态机全部转移到网关侧） | **最低但仍受限**（FAQ 明确「Wrapping `agy` inside third-party coding agents … is not supported under consumer plans」） | shubzkothekar/antigravity-acp、tiezbro/paseo-agy-acp、CIVITAS-John/codex-app-server-to-proxy（Codex 侧同构） |

**跨路线的重要观察**（R3 跨项目验证）：`「重放 token」→「驱动官方二进制」` 的迁移是可复现的通用演进方向。Cursor 那条线是最完整的教科书案例：`cursor2api`（1,886★）逆向文档页免费对话接口，2026-04-01 上游关停后直接 archive；后继 `cursor2api-go`（1,050★）只剩 `gemini-3-flash` 且 README 自称「这次倒下啦」；最终 `NGLSG/Cursor2API`（79★，2026-08-14）转向官方 `crsr_…` API Key + `@cursor/sdk`——**从「凭证盗用」退到「协议适配」，才变得可持续**。

**但路线 C 也无法产出 OpenAI 语义**。官方 ACP 有两个硬约束：

1. **仅 Gemini 模型**（见 §2 ①）。
2. **交互式权限请求无法被纯 API 网关回答**。Antigravity 通过 `session/request_permission` 发固定选项的「用户提问」（tool-call id 以 `interaction_` 开头），选项是「答案」而不是「工具批准」，`--approve-all` 会直接取消 —— [openclaw/acpx `agents/Antigravity.md`](https://github.com/openclaw/acpx/blob/main/agents/Antigravity.md)。
3. 另有部署细节：ACP 服务器**不继承 `agy` CLI 的登录态**，需在 `~/.gemini/antigravity-acp/settings.json` 配 `{"auth":{"type":"oauth-personal"}}` 并做一次性浏览器登录 —— [pi-go 博客](https://pi-go.sh/blog/antigravity-acp/)（2026-08-29）。

---

## 4. 案例：Antigravity 协议与生态

### 4.1 时间线（2025-11 发布 → 2026-11 ACP 开放）

| 时间 | 事件 | 对转换层的含义 | 来源 |
|---|---|---|---|
| 2025-11-18 | 发布，定位「agent-first development platform」，与 Gemini 3 同日；首版是 VS Code 分支 IDE + 独立 Agent Manager 双界面 | 本地 LS + `v1internal` 时代开始 | [官方博客](https://antigravity.google/blog/introducing-google-antigravity)（"Nov 18, 2025"）；[The Verge](https://www.theverge.com/news/822833/google-antigravity-ide-coding-agent-gemini-3-pro) |
| 2026-02-20 | `gemini-3-pro` 下线，报 "Gemini 3 Pro is no longer available. Please switch to Gemini 3.1 Pro" | 所有硬编码 `gemini-3-pro` 的配置全失效 | [Antigravity-Manager #2051](https://github.com/lbjlaq/Antigravity-Manager/issues/2051) |
| 2026-05-19（I/O 2026） | **2.0**：去掉 IDE 的独立桌面应用 + `agy` CLI + SDK + API；配额改为「5h + 周上限」，Flash/Pro 限额合并 | 第二代协议腿（CLI/SDK）+ 额度模型变更 | [2.0 博客](https://antigravity.google/blog/introducing-google-antigravity-2/)（"May 19, 2026"）；[plans 变更](https://antigravity.google/blog/changes-to-antigravity-plans/) |
| 2026-06-18 | Gemini CLI 对个人账号停服，个人侧并入 Antigravity | 同源协议的另一条支线被上游关掉 | [CloudZero](https://www.cloudzero.com/blog/google-antigravity-pricing/)（2026-09-18）——**二手来源，未验证** |
| 2026-08-18~ | 官方独立 ACP 二进制 `agy_acp_server`（build `agy_acp_server_20260818_01_RC01`），进 ACP Registry | 出现「官方 ACP 路线」这一类项目 | [registry issue #554](https://github.com/agentclientprotocol/registry/issues/554)；[ACP Protocol Reference](https://cdn.jsdelivr.net/npm/@estebanforge/pi-antigravity-bridge@1.4.7/docs/ACP-PROTOCOL-REFERENCE.md)（"verified LIVE on 2026-09-03"） |
| 2026-11-04/05 | `agy acp` 才进入 CLI `--help`（1.5.0）；**1.5.1 才向所有 ACP 客户端开放账号下全部模型**，此前只有固定编辑器名单能拿到非 Gemini 模型 | 开放时间比二进制分发晚约 2.5 个月 | [CLI releases](https://github.com/google-antigravity/antigravity-cli/releases) |
| 2026-11-18（预告） | 移除 Claude Sonnet 4.6 / Opus 4.6 / GPT-OSS-120b；日期已从 11-02 改到 11-18 一次 | 免费/Plus 档彻底失去第三方模型 | [docs/models](https://antigravity.google/docs/models/) |

### 4.2 三条协议腿

```
【第一代】IDE / 桌面 App
  IDE(Electron/VS Code fork) ──Connect RPC v1(JSON over HTTP, 自签 TLS, 127.0.0.1:随机端口)──> language_server(Go, Codeium 血统)
  language_server ──HTTPS + JSON/SSE──> daily-cloudcode-pa.googleapis.com / cloudcode-pa.googleapis.com

【第二代】CLI / 2.0
  agy ──HTTPS + JSON/SSE──> 同一个 v1internal 后端
  agy acp ──JSON-RPC 2.0 over stdio──> 编辑器（官方 ACP 服务器）
```

**上游端点与方法**（`[项目]` 逆向 + 抓包一致）：

- `https://daily-cloudcode-pa.googleapis.com`（consumer 默认）、`https://cloudcode-pa.googleapis.com`（prod/enterprise）—— [aphrody-antigravity `endpoints.rs`](https://docs.rs/aphrody-antigravity/latest/src/aphrody_antigravity/endpoints.rs.html)（注释称 "reverse-engineered from the Antigravity 2.0.1 client and confirmed against its live OAuth flow"）。
- 方法：`POST /v1internal:loadCodeAssist`、`:fetchAvailableModels`、`:onboardUser`、`:generateContent`、`:streamGenerateContent?alt=sse`、`:countTokens`、`:retrieveUserQuotaSummary`。
- 请求信封：`{project, model, request:{contents, systemInstruction, generationConfig, tools}, userAgent, requestId}`；响应把 `GenerateContentResponse` 包在 `response` 字段下 —— [cortexkit/antigravity-auth `ANTIGRAVITY_API_SPEC.md`](https://github.com/cortexkit/antigravity-auth/blob/main/packages/opencode/docs/ANTIGRAVITY_API_SPEC.md)（v1.0，2025-12-13，"Verified by Direct API Testing"）。
- **端点身份绑定是最容易踩的坑**：`daily` 只对 consumer 授权，`prod` 只对 enterprise 授权。24 个 consumer 账号实测：`daily` 24/24 → 200，`prod` 24/24 → **429**。因此代理里的 "daily→prod 回退" 会把健康账号误判为额度耗尽 → 打进冷却 → 整池雪崩。CLIProxyAPI 后续 [commit adf0529](https://github.com/router-for-me/CLIProxyAPI/commit/adf052984f8b53bbc6ac8e4338297afee5b337f5)（2026-08-25）"remove cross-endpoint fallback" —— [CLIProxyAPI #4749](https://github.com/router-for-me/CLIProxyAPI/issues/4749)（2026-08-03）。
- **传输形态争议**：有第三方文档称上游是 "gRPC (HTTP/2, binary protobuf)"（[wikifita gRPC Protocol 页](https://wikifita.aleffita.dev/wiki/sdk-test/sdk-test-grpc-protocol)），但与多个独立抓包实现冲突，且该页描述的实际是本地 harness↔SDK 层。**「上游 = JSON over HTTPS + SSE」可信度更高；「上游 gRPC」标为存疑/未验证。**

**本地 LS 一跳**（路线 B 的入口）：服务名 `exa.language_server_pb.LanguageServerService`，Connect RPC v1，必须带 `Connect-Protocol-Version: 1` 与 `x-codeium-csrf-token`（CSRF token 从进程命令行 `--csrf_token` 读取）；**端口与 token 每次 IDE 重启都变**；关键方法 `GetUserStatus` / `GetCommandModelConfigs` / `GetCascadeTrajectory` / `SendUserCascadeMessage` —— [openburn `docs/providers/antigravity.md`](https://github.com/suasgn/openburn/blob/main/docs/providers/antigravity.md)；[jackwener/open-antigravity](https://github.com/jackwener/open-antigravity)（含 Connect envelope `[flags:1][len:4 BE][payload]` 实现见 [src/bridge/grpc.ts](https://github.com/jackwener/open-antigravity/blob/fab75833/src/bridge/grpc.ts)）。**会话单位是 Cascade**（有状态 agentic session），不是无状态 completion。

### 4.3 认证：OAuth PKCE + 内嵌 client_secret + UA 作为准入门槛

| 项 | 事实 | 来源 |
|---|---|---|
| 流程 | OAuth 2.0 **Authorization Code + PKCE**，浏览器授权，loopback 回调 `http://localhost:9109/oauth-callback` | [aphrody `endpoints.rs`](https://docs.rs/aphrody-antigravity/latest/src/aphrody_antigravity/endpoints.rs.html) |
| client_id | `1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com` | [aphrody `auth.rs`](https://docs.rs/aphrody-antigravity/latest/src/aphrody_antigravity/auth.rs.html)；[openburn](https://github.com/suasgn/openburn/blob/main/docs/providers/antigravity.md) |
| client_secret | **桌面二进制内嵌**（非机密，但缺失会报 `400 client_secret is missing`）：Google 拒绝 PKCE 换码与 refresh grant，除非出示桌面二进制里带的那个 secret | 同上 `auth.rs` |
| scopes | `cloud-platform`、`userinfo.email`、`userinfo.profile`、`cclog`、`experimentsandconfigs` | 同上 `ANTIGRAVITY_SCOPES` |
| 刷新 | `POST https://oauth2.googleapis.com/token`，`grant_type=refresh_token`，`expires_in≈3599` | 同上 `refresh()` |
| token 落地 | ①`~/.gemini/antigravity-cli/antigravity-oauth-token`（`{"token":{access,refresh,expiry},"auth_method":"consumer"}`）②Windows 凭据管理器 `gemini:antigravity` ③旧 IDE：`state.vscdb` 里 `antigravityUnifiedStateSync.oauthToken`（双层 base64 + sentinel 信封 + protobuf） | [aphrody `auth.rs`](https://docs.rs/aphrody-antigravity/latest/src/aphrody_antigravity/auth.rs.html)；[openburn](https://github.com/suasgn/openburn/blob/main/docs/providers/antigravity.md) |
| SSH / 无浏览器 | 打印授权 URL，人工粘贴到本地浏览器，浏览器回显**字母数字授权码**再粘回终端 | [Authenticate 文档](https://antigravity.google/docs/install/) |
| 是否 RFC 8628 device flow | 未见 `device_code`/`user_code` 端点证据；判为「类 device flow 的人工码流，非标准 RFC 8628」——**严格结论未验证** | 同上 |

**UA 是准入门槛，不是装饰**。`agy` 发 `antigravity/cli/<ver> (aidev_client; os_type=…; arch=…; auth_method=consumer)`；Cloud Code 后端把任意 UA 当作已退役的 "Gemini Code Assist for individuals" 客户端并以 `UNSUPPORTED_CLIENT` / 无合格 tier 拒绝；必须带 `{"metadata":{"ideType":"ANTIGRAVITY"}}` 才路由到 Antigravity tier —— [aphrody `endpoints.rs`](https://docs.rs/aphrody-antigravity/latest/src/aphrody_antigravity/endpoints.rs.html)（注释称 "Verified by capturing the live agy.exe 1.1.27 request (2026-09-07)"）。

**且 Google 按 UA 版本号 gate 模型可用性**（不只是身份）：同一 payload、同一 token、同一端点，仅改 UA —— `antigravity/hub/2.2.1 darwin/arm64` → **404 NOT_FOUND**（`gemini-3.7-flash-high`）；`antigravity/hub/2.9.1 darwin/arm64` → 200 / 429。即 3.7 需 ≥2.9.0，**不是后端下架而是 UA gate** —— [CLIProxyAPI #5175](https://github.com/router-for-me/CLIProxyAPI/issues/5175)（2026-08-22），修复 PR [#5177](https://github.com/router-for-me/CLIProxyAPI/pull/5177)。

### 4.4 免费政策与模型表（用户原说法需修正）

- 首发是 public preview 且免费："Google Antigravity for individuals at no charge"，"generous rate limits on Gemini 3 Pro usage"，限额**每五小时**刷新，且与「agent 完成的工作量」而非 prompt 数挂钩 —— [官方博客](https://antigravity.google/blog/introducing-google-antigravity)（2025-11-18，脚注 1）。
- 现在**不是** preview：免费 Individual 层为 GA，$0，含无限 Tab 补全与 Command 请求，但 **agent 配额按周**计 —— [Plans 文档](https://antigravity.google/docs/plans/)（读取于 2026-11-08）："Users not on Google AI Pro or Ultra plans receive … A meaningful quota, refreshed weekly"。
- **明确不支持 BYOK / 自带 endpoint 换额度**：同页 "There is currently no support for … Bring-your-own-key (BYOK) or bring-your-own-endpoint for additional rate limits"。**这条封死了「自带 key 走 Antigravity 通道」的合规化路径。**
- 2026-05-19 调价：新增 $100/月 Ultra(5x)，顶档 $250→$200(20x)，Gemini Flash / Pro 独立限额合并为单一共享池；非 Gemini 模型因容量限制**保留独立固定限额** —— [Changes to Antigravity plans](https://antigravity.google/blog/changes-to-antigravity-plans/)（2026-05-19）。价格数字来自 [CloudZero](https://www.cloudzero.com/blog/google-antigravity-pricing/)（2026-09-18），官方 [pricing 页](https://antigravity.google/pricing)只列层级不列数字。

**模型表（截至 2026-11-08）** —— [Choose a model](https://antigravity.google/docs/models/)：

| 模型 | Free / AI Plus | Pro | Ultra | Enterprise |
|---|---|---|---|---|
| Gemini 4 Argon | ❌ | ✅（仅付费 Pro，不含试用） | ✅ | ✅ |
| Gemini 3.8 Flash | ✅ | ✅ | ✅ | ✅ |
| Gemini 3.7 / 3.6 Flash、3.1 Pro | ❌ | ❌ | ❌ | ✅ |
| Claude Sonnet 5.5 / Opus 5.5 (thinking) | ❌ | ✅（仅非试用订阅） | ✅ | ✅ |
| Claude Sonnet 4.6 / Opus 4.6 (thinking) | ✅ | ✅ | ❌ | ❌ |
| GPT-OSS-120b | ✅ | ✅ | ✅ | ❌ |

脚注：`* Will be removed on November 18, 2026.`（Claude 4.6 两档 + GPT-OSS-120b）；`** Available on Google AI Pro for non-trial subscriptions only.`

> **对转换层的直接影响**：用户在「试用期 Pro」「学生 Pro」「Jio/合作档 Pro」上跑转换层时，模型目录会**静默少掉** Claude 5.5 与 Argon——这不是 bug，是档位限制。社区已有多起同现象报告：[Linux.do](https://linux.do/t/topic/2992939)（2026-10-08）、[Google 论坛 #190310](https://discuss.ai.google.dev/t/google-ai-pro-jio-partner-plan-losing-all-pro-third-party-models-on-nov-2-leaving-only-flash-3-8/190310)。

### 4.5 转换项目盘点（星数/最近 push 为 2026-11-08 GitHub REST API 实测）

| # | 项目 | 星 / 最近 push | 路线 | 架构要点与自述限制 |
|---|---|---|---|---|
| 1 | [su-kaka/agy2api](https://github.com/su-kaka/agy2api) | 5,318 / 2026-11-07 | A 直连 | Python；OpenAI + Claude + Gemini 三出口；凭证池以 email 为键，`CREDENTIAL_SELECT_MODE` 三档（`random`/`cache`/`sticky`，其中 `cache` 按「模型 + system 或首条 user」哈希绑定以命中上游 prompt cache）；模型级冷却；`AUTO_BAN` 遇 403 自动禁用；**无任何 release**；自述「仅供学习研究，禁止商业用途」。原名 `gcli2api`，已移除全部 GeminiCLI 代码 |
| 2 | [liuw1535/antigravity2api-nodejs](https://github.com/liuw1535/antigravity2api-nodejs) | 814 / 2026-11-08 | A 直连 | 最活跃；`/v1`+`/v1beta`+`/v1/messages`+`/sdapi/v1`+`/admin`；token 三档轮换（`round_robin`/`quota_exhausted`/`request_count` 默认 50）；**thoughtSignature 缓存 + 可选透传**（`passSignatureToClient`）；SSE 15s 心跳防 CDN 断连；图片输入必须 Base64；**不支持 `cache_control` / `propertyNames`**（[#118](https://github.com/liuw1535/antigravity2api-nodejs/issues/118)，2026-01-10，上游 400 `Unknown name`） |
| 3 | [router-for-me/CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) | 57,916 / 2026-11-08 | A + C | 多 provider（Antigravity / Codex / Claude Code / Grok / Devin）；executor + translator 插件模型，是「网关 provider 化」最接近的参考实现；824 open issues |
| 4 | [justlovemaki/AIClient2API](https://github.com/justlovemaki/AIClient2API) | 8,960 / 2026-11-08 | A 直连 | Antigravity 支持始于 2025-11-30；**独立 Go sidecar 模拟 JA3 TLS 指纹**绕 Cloudflare/403；自认「无真实账号凭据，未做上游实测」，其免费档模型策略只是保守猜测 |
| 5 | [lbjlaq/Antigravity-Manager](https://github.com/lbjlaq/Antigravity-Manager) | 32,885 / 2026-11-08 | 凭证工厂 | Rust + Tauri；定位账号管理器/切换器（配额可视化、批量刷新、`SignatureCache`、`sanitize_user_text_for_fingerprint`）；1,713 open issues；被生态当作「上游凭证工厂」 |
| 6 | [Draculabo/AntigravityManager](https://github.com/Draculabo/AntigravityManager) | 2,478 / 2026-11-07 | 凭证工厂 + 网关 | TS；账号管理 + 本地 API 网关；曾提 PR #224 做服务端会话历史，被维护者以「proxy 应保持无状态」拒绝 |
| 7 | [lbjlaq/Antigravity-Tools-LS](https://github.com/lbjlaq/Antigravity-Tools-LS) | 438 / 2026-03-27 后停更 | B 桥接 | 「完整接管原生 `ls_core` 进程的生命周期」，自述「早期实验阶段」 |
| 8 | [jackwener/open-antigravity](https://github.com/jackwener/open-antigravity) | 211 / 2026-03-21 | B 桥接 | 零运行时依赖 TS；`discovery.ts`(ps+lsof) → `statedb.ts`(读 SQLite 取 key) → `grpc.ts`；**把无状态 API 映射到有状态 Cascade 的具体手法**：首条 `system` 抽成独立参数、`x-workspace`/`x-conversation-id` 自定义头、非流式回 `_conversation_id`、SSE 内联错误避免断流；要求桌面应用运行且至少开一个 workspace |
| 9 | [zhe-gu/zero-gravity](https://github.com/zhe-gu/zero-gravity) | 26 / 2026-02-18 | B（最激进） | 伪造 Extension Server 骗 LS → iptables UID 级重定向进 MITM → 换入真实 prompt → **BoringSSL 复刻 Chrome JA3/JA4 + H2 指纹**；自述 "Early stage"、"tool call 部分仍不稳定" |
| 10 | [frieser/antigravity-proxy](https://github.com/frieser/antigravity-proxy) | 82 / 2026-06-19 / **archived** | B 桥接 | 翻译层完整（模型归一、历史映射、JSON Schema 清洗、跨轮 signature、SSE 回译）；**已归档 = 该路线维护成本高于收益的实证** |
| 11 | [GhostXia/agy2api](https://github.com/GhostXia/agy2api) | 25 / 2026-06-19 | C 包装 | `subprocess.run(["agy","--print",...])`，因 "agy stdout 在当次构建里是空的"，改读会话 SQLite 再自写 wire decoder 解 protobuf；**fake streaming**（心跳 + 整段切块） |
| 12 | [truongqv12/agy2api](https://github.com/truongqv12/agy2api) | 41 / 2026-08-09 | C 包装 | FastAPI，要求先装 `agy` CLI，明确警告「不要暴露到公网」 |
| 13 | [Kazuki-0147/Antigravity-Proxy](https://github.com/Kazuki-0147/Antigravity-Proxy) | 62 / 2026-02-27 | A 直连 | Go+JS；OpenAI+Anthropic+Gemini 三端；账号池 + 配额可视化 + 429 指数退避 |
| 14 | [Mag1cFall/Antigravity2API](https://github.com/Mag1cFall/Antigravity2API) | 14 / 2026-10-14 | A 直连 | 暴露面最广（含 `/v1/audio`、`/v1/files`）；**系统提示四档**（`full`/`medium`/`aggressive`/`custom`）；`/v1/audio`、`/v1/files` 是否真有上游支撑**未验证** |
| 15 | [link0518/Antigravity2api](https://github.com/link0518/Antigravity2api) | 115 / 2025-11-24 | A 直连 | 已停更；README 写得最直白：「**使用本项目可能违反 Google 服务条款，存在账号被封禁的风险**」 |
| 16 | [znlsl/Antigravity2Api](https://github.com/znlsl/Antigravity2Api) | 208 / 2026-04-03 | A 直连 | 主打 MCP XML bridge（`mcp__*` 工具）+ subagent + 交错思考签名校验 |

**官方 ACP 路线（C 类的特化，不做 Cloud Code 翻译）**：

| 项目 | 星 / 最近 push | 说明 |
|---|---|---|
| [yuting0624/antigravity-for-claude-code](https://github.com/yuting0624/antigravity-for-claude-code) | 477 / 2026-11-02 | Claude Code 插件，把 `agy` 当子 agent（**不是** API 网关） |
| [shubzkothekar/antigravity-acp](https://github.com/shubzkothekar/antigravity-acp) | 85 / 2026-09-27 | Bun 写 ACP server，包 `agy` CLI；issue [#3](https://github.com/shubzkothekar/antigravity-acp/issues/3) 要求「记录 ToS 违规风险与免责」 |
| [tiezbro/paseo-agy-acp](https://github.com/tiezbro/paseo-agy-acp) | 67 / 2026-11-01 | 唯一深挖官方内核缺陷的 |
| [zacbemis/pi-antigravity-acp-provider](https://github.com/zacbemis/pi-antigravity-acp-provider) | 67 / 2026-11-05 | Pi 的 ACP provider |
| [shindgew/agy-acp](https://github.com/shindgew/agy-acp) | 21 / 2026-08-19 | 自动下载并校验 `agy`；作者写了[构建记](https://shindgew.com/blocks/building-agy-acp/)（2026-08-17） |
| [pingdotgg/t3code PR #9348](https://github.com/pingdotgg/t3code/pull/9348) | merged 2026-09-03 | "Everything runs through Google's official ACP executable"——是 2026-10-26 官方「开口」的当事项目 |

> **注**：以上项目均为第三方。SAG 若评估接入，应把它们当作**协议实现的参考资料**，而非可依赖的组件——`cliproxy-rs`（CLIProxyAPI 的 Rust 移植，读同一份 `config.yaml` 与 auth 文件、加 `soonest-reset` 路由策略）同样继承全部上游风险。

---

## 5. 限制清单（报告主体）

### 5.1 政策与账号风险

#### 5.1.1 ToS 书面禁止 + FAQ 点名

`[官方]` [Google Antigravity - Terms of Service](https://antigravity.google/terms/)（查阅 2026-11-08；该条款在 2026-09 前后被改写以明确适用范围）：

> **§6.** "You must not abuse, harm, interfere with, or disrupt the Service. This includes, but is not limited to, **using the Service in connection with products not provided by us**. **Using third party software, tools, or services to access the Service (e.g. using OpenClaw with Antigravity OAuth) is a breach of this Agreement.** Such actions may be grounds for **suspension or termination of your Antigravity and/or Gemini CLI accounts**."

`[官方]` [FAQ | Antigravity Docs](https://www.antigravity.google/docs/faq/) 两条把边界写死：

1. "**Why can't I use third party software (e.g. Claude Code, OpenClaw, OpenCode) with my Antigravity login?** Using third party software, tools, or services to access Antigravity is a violation of our Terms of Service … If you would like to use a third party coding agent with Gemini, we recommend using a **Vertex or AI Studio API key**."
2. "**Can I use Antigravity CLI headless mode in third-party coding agents** or automate account switching?" → "Wrapping `agy` inside third-party coding agents or applications, or automating account rotation, **is not supported under consumer plans**."

> **第 2 条直接否掉了 §4.5 里几乎全部项目**——它们正是「wrap agy / 轮换账号」。第 1 条点名的三个例子（Claude Code / OpenClaw / OpenCode）又恰好是「把 Antigravity 当 OpenAI/Anthropic API 用」最主流的三个客户端。**这不是解释空间问题。**

`[官方]` 附带约束：第三方模型（如 Anthropic）适用该模型自己的条款（§8，"For Anthropic specifically, you agree to be bound by … anthropic.com/legal/commercial-terms"）。→ 通过 Antigravity 转出 Claude **同时**可能触及 Anthropic 的商业条款边界（**未验证** Anthropic 是否实际执法）。

#### 5.1.2 封禁时间线

| 时间 | 事件 | 来源 |
|---|---|---|
| 2026-02-23 | Antigravity 负责人公开称「massive increase in malicious usage of the Antigravity backend」，开始封禁 | [VentureBeat](https://venturebeat.com/orchestration/google-clamps-down-on-antigravity-malicious-usage-cutting-off-openclaw-users)（2026-02-23） |
| 2026-02-26~27 | 封禁**连带** Gemini CLI 与 Gemini Code Assist（因共用 abuse-prevention 后端层） | `[官方]` [gemini-cli Discussion #20632](https://github.com/google-gemini/gemini-cli/discussions/20632)（2026-02-27，Google 员工 jackwotherspoon 发帖） |
| 2026-02-27 | 官方宣布全量自动解封；改为「邮件 + Google Form 重新承诺」流程；**第二次违规永久封禁** | 同上 |
| 2026-03 | 社区实测解封需数天至数周，多数人未收到邮件 | [V2EX](https://www.v2ex.com/t/1193212)（2026-02-19）；[Antigravity-Manager #3009](https://github.com/lbjlaq/Antigravity-Manager/issues/3009)（2026-04-01） |
| 2026-09-03 | ToS 第 6 条重写并点名 OpenClaw；同日 HN 278 分事件与 12 小时内措辞收窄（从 "your account" 改为点名两个产品账号） | [Ground Truth 报道](https://groundtruth.day/news/googles-antigravity-terms-ban-third-party-clients-outright.html)（2026-09-03）；[Zentor 复盘](https://zentor.ai/blog/antigravity-account-suspended-third-party)（2026-09-04，更新至 2026-10-27） |
| 2026-09-16 ~ 10-08 | 论坛有 **10 次书面确认**「用 `agy -p` 作为本地子进程被 Codex/Claude Code 调用符合 §6」 | [Google 论坛 #198430](https://discuss.ai.google.dev/t/1m-advertised-context-vs-a-256k-compaction-clamp-in-the-shipped-binaries-plus-quota-and-agy-p-policy-questions-desktop-2-29-0-cli-1-5-1/198430)（2026-11-05，附 10 个帖号） |
| **2026-10-08** | 最后一帖后 **71 分钟**，另一帖推翻该立场并警告自动封号 | 原始反转帖 #187241（帖号，见上条 #198430 的附注） |
| 2026-10-21 | 用户 4 天内被封 3 个号，含「OAuth 认证被封」与「Antigravity Tools 反代被封」 | `[社区]` [Linux.do](https://linux.do/t/topic/3053739)（可信度高，具体案例） |
| 2026-10-26 | 唯一「开口」：Google 工作人员书面回复——用 T3 Code + 个人订阅**可以**，前提是跑 ACP Registry 里**未经修改的** `antigravity-acp` 二进制、走它自己的 `oauth-personal` 原生 OAuth；仍会封「**抽取或代理 OAuth token**（如 OpenClaw/OpenCode 桥）、把 `agy -p` 包进第三方 agent 循环、自动化多账号轮换」 | [discuss.ai.google.dev #192027](https://discuss.ai.google.dev/t/tos-clarification-is-t3-code-official-antigravity-acp-agent-permitted-under-the-antigravity-terms-of-service/192027/2)（2026-10-26）；并补「social media posts aren't official policy」 |
| 2026-10-30 | Blyck 开发者请求**书面确认**（同一路径：官方 ACP 二进制 + 用户自有个人套餐），**截至 2026-11-08 未获答复**；另有「Conflicting guidance on using T3 Code」质疑帖 | [discuss #196885](https://discuss.ai.google.dev/t/tos-clarification-desktop-app-blyck-using-the-official-antigravity-acp-binary-over-acp-with-the-users-own-personal-plan/196885)；[discuss #197131](https://discuss.ai.google.dev/t/conflicting-guidance-on-using-t3-code-with-personal-google-ai-subscriptions/197131) → **该「开口」的稳定性未验证** |
| 2026-11-01 | 官方论坛：用户本地多 agent 直接调用 `agy`，周额度用了不到 20%，无代理无共享，仍被封 | [discuss #197297](https://discuss.ai.google.dev/t/account-suspended-again-multi-agent-setup-calling-antigravity-cli/197297) |
| 2026-11-03 / 11-05 | 两波社区封号潮：「CLIProxyAPI 反代，就三个人用也不行」；「几乎全被杀完」，均 403，走 Appeal Form | `[社区]` [Linux.do](https://linux.do/t/topic/3112874)（11-03）、[Linux.do](https://linux.do/t/topic/3129568)（11-05），均可信度高、多人同时 |
| **2026-11-06** | `403 PERMISSION_DENIED / reason: TOS_VIOLATION`，错误体带 `appeal_url`。**关键差异：只封 Cloud Code 这一个服务**，账号登录、token 刷新、模型目录读取、订阅全部正常；3 个凭证中 2 个中招、第 3 个同条件 200 → 判定为**凭证级**而非 IP/机器/客户端签名级 | [CLIProxyAPI #7296](https://github.com/router-for-me/CLIProxyAPI/issues/7296)（2026-11-06，含中文摘要与逐条排除实验） |

> **这条最新证据的分量**：封堵已从「账号级」细化到「**服务级 + 凭证级**」，且申诉入口自动化。对网关的直接影响是——**失效不会以「账号掉线」这一可观测事件出现，而是一个凭证的某一个服务突然 403，其余一切正常**，如果 SAG 的健康检查只看 token 是否有效，会完全看不见这种故障。

#### 5.1.3 检测手段分层（逆向证据）

`[项目]` [9router issue #1138](https://github.com/decolua/9router/issues/1138)（2026-05-14，open）是对检测层最系统的公开梳理，逐层标注了代码出处（Antigravity-Manager commit `cc2f187`、Antigravity-Tools-LS commit `d312237`）：

| 检测层 | Google 校验什么 | 缺失时的表现 |
|---|---|---|
| TLS / JA3-JA4 指纹 | 与 `User-Agent` 关联 | 指纹不匹配 → 403 / Captcha；需 Chrome 123 模拟（`rquest` + BoringSSL） |
| OAuth TLS 分离 | `/token`、`/oauth2` 期望**原生** TLS 指纹，**不是** Chrome 模拟 | 混用会异常 |
| 会话头 | 要求 `x-machine-id`、`x-vscode-sessionid`、`x-goog-user-project` | 缺失 = 403 `SERVICE_DISABLED` |
| `requestId` 格式 | 严格 `agent/{timestamp_ms}/{hex8}` | 格式不符 |
| `enabledCreditTypes` | 需 `["GOOGLE_ONE_AI"]` 才被认作官方客户端 | 未注入 |
| 动态 `ideType` / `userAgent` | Enterprise/Workspace 用 `jetski`/`JETSKI`，普通用 `antigravity`/`ANTIGRAVITY` | 硬编码 `antigravity` 会失配 |
| `thought_signature` | Gemini 3+ 必需，且有最小长度 | 400 |
| `state.vscdb` / `serviceMachineId` 同步 | 上报身份与 SQLite 状态必须一致 | "Environment Changed" 弹窗 + 会话吊销 |
| `__cloudCodeMeta` trace 中继 | trace 链不能断 | 断链 = 被拦截信号 |
| 版本指纹 | 上报版本必须与 `ls_core` 二进制行为特征一致 | 版本不符 |
| gRPC CSRF | 每个 gRPC 请求要 `x-codeium-csrf-token` | 403 |
| **bootstrap 一致性** | OAuth 流程用数字枚举 `{"ideType": 9, "pluginType": 2}`，而 `loadCodeAssist` 若发字符串 `{"ideType": "VSCODE", "pluginType": "GEMINI"}`，**比对两次调用即可在第一个请求就标记账号** | 账号被标记 |

补充证据：**设备指纹绑定**的确存在——Antigravity-Manager [PR #523](https://github.com/lbjlaq/Antigravity-Manager/pull/523)（2026-01-10，merged）实现「一账号一设备」，绑定 `Machine ID / MAC / SQM ID`，切号时替换本地 `storage.json`。**如果不需要，不会有人花 1184 行代码去同步这些字段。**

> 注意：9router #1138 是**功能请求**而非实测结论，其「多层检测」的具体清单**未验证**。但 TLS 指纹已被实际实现：[AIClient2API 的 Go TLS sidecar](https://www.promptquorum.com/power-local-llm/aiclient2api-review)（2026-09-19）、[zero-gravity 的 BoringSSL JA3/JA4 复刻](https://github.com/zhe-gu/zero-gravity)。

#### 5.1.4 地域限制与档位耦合

- `[官方]` FAQ 列出 150+ 国家/地区白名单，**明确不含中国大陆、香港、澳门**；含台湾、日本、新加坡、美国。门槛是**Google 账号关联地区**，不只是 IP。
- `[项目]` 机房 IP 会得到 `400, User location is not supported for the API use.` —— [PaperPlane.cc 博客](https://paperplane.cc/post/antigravity-proxy)（无日期，作者实测腾讯云硅谷/多家云厂商均失败，改用 Cloudflare WARP 出口后恢复，可信度中）。
- `[项目]` 反例：账号地区被改回中国大陆 → 直接 `Agent terminated due to error`，用官方 IDE 退出重登可恢复 —— [Antigravity-Manager #1842](https://github.com/lbjlaq/Antigravity-Manager/issues/1842)（2026-02-11）。
- 档位耦合：家庭组子账号与主账号权限**独立**，主号 Pro 未掉而子号可能全被封；多账号池的「平均可用性」不可线性外推 —— `[社区]` [Linux.do](https://linux.do/t/topic/1618883)（2026-02-15，可信度中）。
- Enterprise（Gemini Enterprise）走 Google Cloud 条款，可能是唯一合规路径，但需企业合同（**未验证**是否开放 `v1internal` 给第三方客户端）。

### 5.2 配额与限速

#### 5.2.1 官方口径：只有相对描述，没有绝对数字

`[官方]` [Plans 文档](https://antigravity.google/docs/plans/)（查阅 2026-11-08）：

| 档位 | 官方措辞 | 刷新 |
|---|---|---|
| 无 Pro/Ultra | "a meaningful quota" + weekly rate limit | 每周 |
| Google AI Pro | "a high, generous quota" | 每 5 小时，直到触达周限额 |
| Google AI Ultra | "the highest, most generous quota" | 每 5 小时；"highest weekly rate limits" |

同页关键约束：

- 配额「primarily determined by available capacity and exist to prevent abuse」，且**按 agent 实际完成的工作量计**（"the amount of work done by the agent"）——同一句话在 agent 循环里会因工具轮次多寡而消耗不同，**无法用 prompt token 数线性预测**。
- **额度不结转**；超额可用 AI Credits，按 Gemini Enterprise 消费价计费。
- **Google 从未公布任何一档的绝对配额数字**。5x / 20x 只是相对 Pro 的倍数 —— [CloudZero](https://www.cloudzero.com/blog/google-antigravity-pricing/)（2026-09-18）。
- `[官方]` `/usage`（别名 `/quota`）显示两个模型组：Gemini（Flash+Pro 合并）与 Claude+GPT，各有 Weekly / Five Hour 两条进度条 —— [CLI usage 文档](https://antigravity.google/docs/cli/commands/usage/)。
- 合并口径：Gemini Flash 与 Pro **按 API 价格折算进同一池**（Flash 比 Pro 便宜 8 倍 → 同额度可跑 8 倍 Flash token）；非 Gemini 模型因容量限制**保持独立固定限额** —— [Changes to Antigravity plans](https://antigravity.google/blog/changes-to-antigravity-plans/)（2026-05-19）。

`[社区]` 实测口径（可信度中—低，均为自报、样本小，**不可用于容量规划**）：

| 断言 | 来源 | 日期 | 可信度 |
|---|---|---|---|
| Pro 账号周限额约 450–550M token（4 个号样本） | [Linux.do](https://linux.do/t/topic/2951333) | 2026-09-25 | 中（方法未公开） |
| 周限额 ≈ 6–10 个「5 小时满额」 | 同上 | 2026-09-25 | 中低 |
| 免费额度 7 天刷新，且**倒计时从首次使用才开始** | [V2EX](https://www.v2ex.com/t/1223155) | 2026-06-26 | 中（多人确认） |
| Pro 周额度：更新前约 4–5 个 5h 满额，更新后降到 1.5–2 个 | [V2EX](https://www.v2ex.com/t/1190155) | 2026-02-02 | 中（有对照） |
| Gemini 4 Argon 抽干配额速度约为 3.8 Flash 的 **3×** | [论坛 #198430](https://discuss.ai.google.dev/t/1m-advertised-context-vs-a-256k-compaction-clamp-in-the-shipped-binaries-plus-quota-and-agy-p-policy-questions-desktop-2-29-0-cli-1-5-1/198430)；[归档](https://afengblog.com/en/blog/antigravity-256k-context-discrepancy.html) | 2026-11-05 | 中（Google 员工回复：内部倍数不公开） |
| 免费账号不报 429，只走 7 天周限；老账号反而疯狂 429 | [Antigravity-Manager #1039](https://github.com/lbjlaq/Antigravity-Manager/issues/1039) | 2026-01-23 | 低 |

#### 5.2.2 四种 429 语义 + 假 429（本节最重要）

`[项目]` 代理层从 `error.details[0].reason` / `error.message` 能区分出至少四类 429，**处理方式完全不同** —— [Antigravity Manager: Fix 429, 503 & Pool Errors](https://antigravitymanager.com/help/limits/)（更新 2026-11-01）；同类分类亦见 [文档站](https://lzw.me/docs/opencodedocs/zh-tw/lbjlaq/Antigravity-Manager/faq/429-rotation/)：

| reason / 线索 | 真实含义 | 正确处置 |
|---|---|---|
| `QUOTA_EXHAUSTED` / "quota reset" / daily allowance | 配额真耗尽 | 等到 reset 时间 |
| `RATE_LIMIT_EXCEEDED` / "per minute" / "too many requests" | 短时速率限制（秒级） | 按 `Retry-After` 等 |
| `MODEL_CAPACITY_EXHAUSTED` | 模型容量不足（过载） | 换模型 / 等 |
| `RESOURCE_EXHAUSTED` 泛化消息 | 需看 details 里的 `retryDelay` | 先暂停再单发探测 |

`RetryInfo.retryDelay` 实测可达 30 分钟量级：规范示例 `"retryDelay": "3.957525076s"`（[ANTIGRAVITY_API_SPEC](https://github.com/cortexkit/antigravity-auth/blob/main/packages/opencode/docs/ANTIGRAVITY_API_SPEC.md)，2025-12-13）；实测 `Provider requested 1800000ms wait, exceeds retry.maxDelayMs (300000ms)`（[oh-my-pi #11689](https://github.com/can1357/oh-my-pi/issues/11689)，2026-09-11）。

**假 429（内容级 WAF）** —— `[项目]` [CLIProxyAPI #5751](https://github.com/router-for-me/CLIProxyAPI/issues/5751)（2026-09-11 提出，09-15 结帖，多方独立复现）：

- 现象：任何模型都返回 `429 RESOURCE_EXHAUSTED`，但**账号配额完好，官方 App 同账号同模型正常**。
- 二分定位（多用户独立复现）：
  - `systemInstruction` 里出现 `<system-conventions>` + `RFC 2119: MUST, REQUIRED, SHOULD, RECOMMENDED, MAY, OPTIONAL.` → 429；去掉 `<system-conventions>` 标签 → 200；把 RFC 列表截断到 `MAY` → 200；完整到 `OPTIONAL.` → 429。
  - 另一条独立指纹：`You are Codex, an agent based on GPT-5.` → 429；`You are Codex.` 或 `You are an agent based on GPT-5.` 单独出现 → 200。
  - **插入零宽字符即恢复 200** → 说明是**字面/词法级匹配**，不是语义模型。
- 项目方应对：`antigravity.sensitive-words` 配置项做零宽字符混淆（v7.2.118+），后续 PR #5790 加诊断告警。

> **这是架构级发现**：上游对「客户端身份文本」做了内容级封锁。任何把第三方 agent 的 system prompt 原样转发的网关，都会周期性撞上这类假 429，且错误信息会把人引向「配额/风控」的错误方向。这也解释了为什么「用官方 App 好好的，一套代理就 429」的报障占比极高。

#### 5.2.3 静默模型降级

CLI 会直接把不存在的模型换成别的，且只给一行 warning（`[官方论坛]` [「Gemini models are missing on Antigravity」第 8 楼](https://discuss.ai.google.dev/t/gemini-models-are-missing-on-antigravity/188723/8)，2026-10-13）：

```
⚠ Warning
⎿ "Gemini 3.1 Pro (High)" is no longer available. Using "Gemini 3.8 Flash (High)".
```

> 对 SAG 的含义：**上游可能在你请求 A 模型时静默给你 B 模型的输出**，而响应里 `modelVersion` 字段是否仍为 A —— **未验证**。网关必须记录并回显实际模型，否则用户侧无法察觉能力降级。

#### 5.2.4 403 与 429 的区别

| 语义 | 状态码 | 典型错误体 |
|---|---|---|
| 配额 / 限速 | **429** | `QUOTA_EXHAUSTED` / `RATE_LIMIT_EXCEEDED` / `MODEL_CAPACITY_EXHAUSTED` / 假 429 |
| 政策封禁 | **403** | `This service has been disabled in this account.` / `PERMISSION_DENIED` / `Verify your account to continue.` / `reason: TOS_VIOLATION` |

判别与处置差异见 [kRouter 文档](https://krouter.kodelyth.com/blog/antigravity-verify-account)（2026-04-22 发布，2026-10-08 更新）；`su-kaka/agy2api` 的 `AUTO_BAN` 默认触发码即 **403**。

**未验证**：是否存在「静默降权」（同账号响应变慢/质量下降但不报错）。Antigravity-Manager 有 QoS 概念（`Suspected Throttled` / `In QoS Cooldown` / `Probing Recovery`），但那是**代理侧**启发式判定，不是上游信号。

#### 5.2.5 并发

- 官方并发数字：**不存在 / 未验证**。Plans 页与 models 页均未给出上限。
- 代理侧两个并发 bug 说明「并发 + 429」的组合很难处理对：
  - [CLIProxyAPI #5265](https://github.com/router-for-me/CLIProxyAPI/issues/5265)（2026-08-26）：429 的 `Retry-After` 是亚秒级（如 `708.717057ms`），而遍历 20+ 账号池比 708ms 慢，导致「刚冷却就过期」→ 多轮重试风暴 → CPU/内存耗尽。
  - [CLIProxyAPI #6643](https://github.com/router-for-me/CLIProxyAPI/issues/6643)（2026-10-15）：并发中先发出的请求成功返回后会**清掉**刚刚因配额耗尽设置的冷却 → 已耗尽的凭证被反复选中。
- 重试放大：单凭证下 `request-retry: 3` 会把一个 227KB 请求重发 3 次（约 1M 输入 token 打进同一个配额桶），把瞬时 429 变成持续 429 —— 同上 #5751 中 arksdev 的观测（2026-09-11）。

### 5.3 功能失真

#### 5.3.1 上游真实形状：Gemini 风格，不是 OpenAI 也不是 Anthropic

`[项目]` [ANTIGRAVITY_API_SPEC.md](https://github.com/cortexkit/antigravity-auth/blob/main/packages/opencode/docs/ANTIGRAVITY_API_SPEC.md)（v1.0，2025-12-13，"Verified by Direct API Testing"）的 **"NOT Supported"** 表直接说明转换层必须做「有损翻译」：

| 传入字段 | 上游行为 |
|---|---|
| `messages` 数组 | Unknown field |
| `max_tokens` | Unknown field |
| `anthropic_version` | Unknown field |
| 字符串形式的 `systemInstruction` | Invalid value（必须 `{parts:[]}`） |
| JSON Schema `const` / `$ref` / `$defs` / `$schema` / `$id` / `default` / `examples` | 400，必须预处理 |
| 工具名含 `/`、以数字开头、或 >64 字符 | 400 |

`contents` 的角色只有 `user` / `model`（**不是** `assistant`）→ OpenAI 的 `assistant` / `tool` / `function` 角色都要重映射。

#### 5.3.2 参数静默失效（最严重的一类失真）

`[官方]` [Content generation parameters | Gemini Enterprise Agent Platform](https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/capabilities/content-generation-parameters)（Last updated 2026-11-07）：

> "Warning: For **Gemini 3.6 Flash and later** models, custom values for token sampling parameters (`temperature`, `topP`, and `topK`) **aren't supported and are ignored if set**, and custom values for penalization parameters (`frequencyPenalty` and `presencePenalty`) **return an error**."
> "For Gemini 3.5 Flash, `topK` is fixed at 64 and can't be modified."

Antigravity 当前主力是 Gemini 3.8 Flash / Gemini 4 Argon（均 3.6 之后），所以：

| OpenAI 参数 | 实际行为 |
|---|---|
| `temperature` / `top_p` / `top_k` | **静默无效**（200 OK，参数被丢弃） |
| `frequency_penalty` / `presence_penalty` | **应当报错**（若转换层不主动剥离） |
| `seed` | 上游有该字段，转换层是否透传**未验证** |

> 对 SAG：这是「语义谎言」级别的问题。用户设 `temperature=0` 期望确定性输出，实际得到模型默认采样。网关要么显式拒绝这些参数，要么在响应里标注 `x-sag-params-dropped`。

#### 5.3.3 输出上限：`max_tokens` 硬顶

| 事实 | 来源 | 日期 |
|---|---|---|
| Antigravity 上 Claude 模型 `max_completion_tokens` = **64000**；下游传 `max_tokens: 128000` → 上游 `400 INVALID_ARGUMENT`；CLIProxyAPI 加了 clamp | [CLIProxyAPI PR #2800](https://github.com/router-for-me/CLIProxyAPI/pull/2800) | 2026-04-15（merged） |
| `max_tokens` 必须**严格大于** `thinking.budget_tokens`，相等即 400 | [CLIProxyAPI #1781](https://github.com/router-for-me/CLIProxyAPI/issues/1781) | 2026-03-01 |
| 早期全模型输出硬顶 **16384**，Claude 思考预算硬编码 **1024**（API 最小值），且 Gemini 超限**静默截断不报错** | [keisksw/antigravity-output-analysis](https://github.com/keisksw/antigravity-output-analysis)（单方测量，Ultra 订阅者） | 2026-04-03 |

> 16384 与后来的 64000 冲突，说明**上限随版本变化**，且前者为单方逆向测量、**未验证**是否仍成立。但「Gemini 路径超限静默截断、Claude 路径报错」这个不对称在同一作者的 4 组对照实验（日/英双语）里有数据支撑，且与 `finishReason: MAX_TOKENS` 的转换缺失一致。**`finish_reason` 的可靠性不能假设**——必须把 `MAX_TOKENS` 正确映射到 `finish_reason: "length"`，否则 agent 会在截断后继续，产生级联错误。

#### 5.3.4 上下文：广告 1M，实际 ~256K

证据链（三层互相印证）—— `[官方论坛 + 项目]` [「1M advertised context vs. a 256K compaction clamp in the shipped binaries」](https://discuss.ai.google.dev/t/1m-advertised-context-vs-a-256k-compaction-clamp-in-the-shipped-binaries-plus-quota-and-agy-p-policy-questions-desktop-2-29-0-cli-1-5-1/198430)（2026-11-05），配套[归档（含反汇编、SQLite 审计、hash）](https://afengblog.com/en/blog/antigravity-256k-context-discrepancy.html)：

1. **二进制反汇编**：`cortex/utils/utils.GetCheckpointConfig` 含 `cmp $0x3e800,%edx; mov $0x3e800,%esi; cmovb %edx,%esi`，即 `min(calculated, 256000)`。CLI 1.5.1 / 1.5.0 / Desktop 2.29.0 各有一处。
2. **会话数据库**：209 个本地 CLI 1M 会话、15,575 次模型调用，全部 `max_context_tokens = 256000`，无单次超过 300K。社区独立审计（310 会话 / 65,613 调用）测得单次最大输入 **255,999** token。
3. **Google 员工口径**：论坛 #198191（2026-11-04）称 ~250K 阈值 "is a fixed default and isn't user-configurable yet"，按 feature request 处理。

`[项目]` 更早、更细的失败模式记录：[antigravity-cli issue #1245](https://github.com/google-antigravity/antigravity-cli/issues/1245)（2026-10-14，仍 open）：

- 94 轮 / 264 步自主运行中，在 276,110 与 249,867 token 处两次强制压缩。
- 压缩后**具体后果**：`view_file` 原文被替换成散文摘要 → 模型开始编造不存在的 catalog item ID；把 `1000..2000` 的数值尺度误当成 `0..10`，把存量值 clamp 了 200 倍；`localStorage` key 一处连字符一处下划线。
- 压缩 median 耗时 **70 秒**（35–224s，最长 602s），且**不上报 usage**；压缩后 45–70K token 的调用缓存全失效（`cache_read_tokens → 0`）。
- 关键区分（2026-10-15 补充）：**Compaction（摘要）** 与 **Pre-flight truncation（静默裁旧历史，无摘要无标记，`truncation_reason = 2`）** 是两套机制。**关掉 auto-compact 不是解决方案**——关掉摘要只是把「有痕迹的丢失」换成「无痕迹的丢失」。`AGY_CLI_AUTO_COMPACT_OVERRIDE` 在发布二进制里被编译掉了（`os.Getenv` 内联成空串），**完全无效**。

**这些结论对「转换层」的适用性需谨慎区分**（重要）：

- 上述 256K clamp 是**客户端侧**（`agy` / `language_server`）行为。一个**不经过官方客户端**、直接打 `v1internal` 的转换层，理论上不受此 clamp 约束。
- 但转换层会撞上另一层限制：`[项目]` [Antigravity-Manager #3325](https://github.com/lbjlaq/Antigravity-Manager/issues/3325)（2026-08-25，已 close）报告在 60K/200K/400K 等**不稳定位置**报 `The input token count exceeds the maximum number of tokens allowed 1048576`，且「频繁工具调用或含图片多模态数据时更容易触发」，后台不显示 token 数。→ 上游**确实**存在 1,048,576 硬上限，而转换层的 token 估算/拼接逻辑会在远低于上限时误触。**这是转换层自身引入的上下文丢失。**
- Claude 系另有独立上限：`[项目]` [opencode-antigravity-auth #512](https://github.com/NoeFabris/opencode-antigravity-auth/issues/512)（2026-02-28），维护者确认 Antigravity 路径上 Claude 按 **200K** 配置，1M long-context beta 未开放。

#### 5.3.5 `thoughtSignature` 签名链：最大的隐性状态

**(a) 上游要求签名必须原样回传**

`[项目]` [Antigravity-Manager #3529](https://github.com/lbjlaq/Antigravity-Manager/issues/3529)（2026-09-26，closed）上游 400：

> `Function call is missing a thought_signature in functionCall parts. This is required for tools to work correctly, and missing thought_signature may lead to degraded model performance.`

触发条件：单轮或首轮工具调用正常；客户端把 `tool_calls` + `role:"tool"` 发回做第二轮生成时，若历史里的 `functionCall` part 没带 `thoughtSignature`，直接 400。

**(b) Claude thinking 签名必须逐字节回传，任何改写都 400**

多条独立记录，模式完全一致：

- [Antigravity-Manager #608](https://github.com/lbjlaq/Antigravity-Manager/issues/608)（2026-01-13）：`messages.3.content.13: Invalid signature in thinking block`，第 3 条消息必现。
- [Antigravity-Manager #547](https://github.com/lbjlaq/Antigravity-Manager/issues/547)（2026-01-11）：`messages.1.content.0: Invalid 'signature' in 'thinking' block`。
- [CLIProxyAPI #1398](https://github.com/router-for-me/CLIProxyAPI/issues/1398)（2026-02-02）：根因是签名前缀与模型名比较逻辑写错，`"claude-opus-4-5-20251101" == "claude"` 恒 false。
- [CLIProxyAPI #1584](https://github.com/router-for-me/CLIProxyAPI/issues/1584)（2026-02-13）：**会话中途从 Gemini 切到 Claude 模型**即触发签名失效。
- [sub2api #935](https://github.com/Wei-Shaw/sub2api/issues/935)（2026-03-11）：Gemini 端点缺签名清理逻辑，`400 Corrupted thought signature`。

**(c) 转换层的三种应对，各有代价**

| 策略 | 做法 | 代价 |
|---|---|---|
| 签名缓存 + 回填 | 从响应抓 `thoughtSignature`，按 session 缓存，下一轮注入 | 需服务端状态；重启/回退/压缩即失效。Antigravity-Manager 的 `SignatureCache` 是内存态，服务重启后客户端仍持旧签名 → 400（[PR #1094](https://github.com/lbjlaq/Antigravity-Manager/pull/1094)，2026-01-24） |
| 哨兵值绕过 | 注入 `skip_thought_signature_validator` 让上游跳过校验 | [CLIProxyAPI PR #3470](https://github.com/router-for-me/CLIProxyAPI/pull/3470)（2026-05-18，merged）。**等于主动放弃上游的思考一致性校验**——上游错误信息自己承认这会 "lead to degraded model performance" |
| 清洗掉签名 | 剥离未签名 / 跨模型的 thinking block | [CLIProxyAPI PR #4562](https://github.com/router-for-me/CLIProxyAPI/pull/4562)（2026-07-25）。丢失历史思考内容 |

> **对 SAG 的判断**：OpenAI 的 `chat/completions` 协议里**没有**「思考签名」这个概念。要支持 thinking 模型的多轮工具调用，只有两条路：(1) 网关内维护 per-conversation 签名状态（引入状态、TTL、失效处理、跨账号迁移问题）；(2) 用哨兵绕过（牺牲上游校验）。**没有第三条干净的路。** 这与 SAG 作为「无状态 OpenAI 兼容网关」的定位直接冲突。

#### 5.3.6 会话状态性：协议层无状态，身份连续性有状态

**先纠正一个常见误解**：Antigravity 后端**不是**「必须传 conversation_id 的有状态 chat 后端」。线上 `v1internal:generateContent` 每次请求都携带完整 `contents[]`，**协议层是无状态的**（见 ANTIGRAVITY_API_SPEC 的完整请求示例）。

**但它是「agent / trajectory 为中心」的**，证据：

- `[官方]` CLI 把每个对话持久化为独立 SQLite（`~/.gemini/antigravity-cli/conversations/<id>.db`），一行一个 step，`step_payload` 是 protobuf；`agy --conversation=<id>` 从 steps 重建模型上下文，DB 缺失时**静默开新会话** —— 第三方逆向文档 [txcript crate](https://docs.rs/txcript/latest/txcript/harness/antigravity/index.html)。
- `[官方]` ACP 是显式会话制：`session/new`、`session/load`、`session/resume`（"restoring a session without replaying its history"），**每个 session 保持自己的模型与思考档位** —— [ACP 文档](https://antigravity.google/docs/cli/acp/)；[release 1.5.0](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.0)。
- `[官方]` SDK 的 `Conversation` 是 Layer 2 的「stateful session management」 —— [antigravity-sdk-python](https://github.com/google-antigravity/antigravity-sdk-python/blob/main/google/antigravity/conversation/README.md)。

**转换层如何伪造无状态 chat completions（四种已被实现的手法）**：

| 手法 | 实现 | 代表项目 | 副作用 |
|---|---|---|---|
| 会话指纹哈希 + 粘性账号 | 用 `model + system 指令（或首条 user 消息）`的哈希做 session key，绑定同一凭证 | su-kaka/agy2api `CREDENTIAL_SELECT_MODE=cache` | 首条消息相同即误绑；改 system 提示即换号 → 缓存全丢 |
| 会话指纹（清洗后）+ session→account 映射表 | `sanitize_user_text_for_fingerprint` 剥掉 `<env>`、`[System ...]` 等瞬态噪声，再 SHA256 出 `sid-*`，存 `DashMap` | [Antigravity-Manager DeepWiki 9.3](https://deepwiki.com/lbjlaq/Antigravity-Manager/9.3-session-management) | 内存态，重启即丢；绑定不均会导致「很多会话挤在一个号上」 |
| 服务端会话历史存储 | 客户端只发最新一轮，网关补全历史 | [Draculabo/AntigravityManager PR #224](https://github.com/Draculabo/AntigravityManager/pull/224)（2026-07-11） | **被维护者拒绝合并**（理由见下） |
| 完全无状态，每次全量重传 | 只做协议翻译，不做任何绑定 | 多数轻量项目 | 缓存命中率低、签名链断、多账号轮换时 400 |

`[项目]` PR #224 维护者的拒绝理由值得 SAG 直接引用（2026-07-15）：

> "The proxy should remain stateless and focus on compatibility, routing, account selection, and upstream reliability. Conversation state is application-level behavior and should be managed by clients such as Codex/OpenCode... Keeping it here introduces non-standard API extensions, duplicate state management, lifecycle/security concerns, and a maintenance surface that is outside the project's core responsibilities."

**副作用清单**：

1. **上下文丢失？会**，但来源不是「后端有状态」，而是三个叠加：客户端/网关侧的 compaction 与 pre-flight truncation（§5.3.4）；签名链断裂导致上游拒绝或降级（§5.3.5）；网关的上下文压缩逻辑主动丢弃内容（工具结果压缩直接丢 base64 图片、浏览器快照变摘要 —— [Antigravity-Manager release v4.3.3](https://github.com/lbjlaq/Antigravity-Manager/releases/tag/v4.3.3)，2026-07-06）。
2. **会话堆积？会**。每个新的 OpenAI 客户端「会话」（往往等于一次进程启动）都会在上游产生新的 trajectory/session 与新的缓存命名空间。粘性绑定策略下旧绑定不会被主动清理（Antigravity-Manager 需手动 `clear_proxy_session_bindings`）。上游侧会话堆积**未验证**是否有配额成本。
3. **计费按什么算？按 token**（含 cached read 与 thinking），按 API 价格加权折算进模型组池；**不按请求数、不按会话数**。依据：plans 页「correlated with the amount of work done by the agent」+ blog「drawn down according to API pricing」。具体权重倍数 Google 明确不公开。

#### 5.3.7 流式缺陷

上游是 Gemini 风格 SSE：`data: {"response": {"candidates":[{"content":{"parts":[{"text":"..."}]}}], "usageMetadata":{...}}, "traceId":"..."}`。转换层的已知流式缺陷（均为项目 issue，可交叉验证）：

| 缺陷 | 来源 | 日期 |
|---|---|---|
| thinking 内容在上游有、转换后丢失（流式分块处理过严） | [CLIProxyAPI #752](https://github.com/router-for-me/CLIProxyAPI/issues/752) | 2025-12-27 |
| `usage.completion_tokens` 不含 `thoughtsTokenCount`，违反 `total == prompt + completion` 不变式，差额恰等于 `reasoning_tokens` | [CLIProxyAPI #6463](https://github.com/router-for-me/CLIProxyAPI/issues/6463) | 2026-10-08（已修） |
| usage chunk 缺 `completion_tokens` 字段，Rust serde 严格客户端整轮失败（`missing field completion_tokens`） | [CLIProxyAPI #4498](https://github.com/router-for-me/CLIProxyAPI/issues/4498) | 2026-07-21 |
| Anthropic `message_start` 里 `input_tokens/output_tokens` 恒为 0，破坏 Vercel AI SDK / OpenCode 的实时计费 | [CLIProxyAPI #747](https://github.com/router-for-me/CLIProxyAPI/issues/747) | 2025-12-27 |
| 大上下文（100K–500K）+ high thinking 时，prefill 的空 SSE 帧导致 `Stream ended without finish_reason` | [9router PR #5272](https://github.com/decolua/9router/pull/5272) | 2026-10-27 |
| **假流式**：`agy --print` 的 stdout 为空，只能读会话 SQLite 再分块伪造 SSE | [GhostXia/agy2api](https://github.com/GhostXia/agy2api) | 2026-06-19 |

> `reasoning_tokens` 的语义在 OpenAI 侧是 `completion_tokens_details.reasoning_tokens`，Antigravity 侧是 `usageMetadata.thoughtsTokenCount`。**两者不是同一个量**（后者可能包含未计入输出的思考），映射关系**未验证**。

#### 5.3.8 工具调用：schema 子集 + 执行权丢失

- 支持 function calling，但有明确 schema 子集限制（见 §5.3.1 表）。
- `[项目]` `googleSearch` / `urlContext` **不能**与 `functionDeclarations` 同请求，必须拆成两次调用（ANTIGRAVITY_API_SPEC）。
- `[项目]` `functionResponse` 的 `response` 字段形状有讲究（`{result: string}` vs 直接对象），不同转换层做法不一，是 400 的高频来源之一（Codex-Launcher CHANGELOG v3.11.12，2026-05-26，第三方 changelog，可信度中）。
- **执行权冲突**是更深的问题：ACP/agent 模型里 agent 自己执行工具，`/v1/chat/completions` 的 `tools` 语义却是「客户端执行」。工程上三条路：透传 + 客户端执行 / prompt 模拟工具 / 让远端执行。**server-side 工具（`file_search`、`computer_use`、`code_execution`）必然被丢弃或必须显式报错。**
- **`tool_choice` 支持度普遍残缺**：多数实现只支持 `auto`/`none`，强制指定函数普遍不支持（跨项目矩阵见 §6.2）。

#### 5.3.9 多模态：可用但会衰减

| 项 | 状态 | 依据 |
|---|---|---|
| 图片输入 | 支持，走 base64 inline | [liuw1535/antigravity2api-nodejs](https://github.com/liuw1535/antigravity2api-nodejs) README |
| 图片 / PDF 跨协议 | 声称 OpenAI Chat / Responses / Claude Messages / Gemini 四协议互通 | [Mag1cFall/Antigravity2API](https://github.com/Mag1cFall/Antigravity2API)（2026-10-14 建库，**较新、未验证**） |
| 长会话中的保留 | **不可靠**：上下文压力下工具结果压缩会**直接丢弃 base64 图片**、把浏览器快照变成头+尾摘要 | [Antigravity-Manager 上下文压缩文档](https://lzw.me/docs/opencodedocs/zh/lbjlaq/Antigravity-Manager/advanced/context-compression/) |
| 音频 | 不是「多模态输入」，是「先转写再喂文本」（走目录里的转写模型如 `proactive-observer-v10`，耗时 ≈ 音频时长） | Mag1cFall README |
| 图像生成 | 可用（`gemini-3.1-flash-image` / Nano Banana 2），需独立模型 ID + `responseModalities: ["IMAGE"]` | [docs/models](https://antigravity.google/docs/models/) |

#### 5.3.10 prompt cache：依赖账号粘性

`[项目]` [Prompt Caching and Sessions | antigravity-claude-proxy 的 DeepWiki](https://deepwiki.com/badrisnarayanan/antigravity-claude-proxy/6.3-prompt-caching-and-sessions)：

1. **缓存作用域是「组织 + 会话」**（"operates at the organization and session scope"）→ 要命中缓存必须「稳定 sessionId + **粘性账号**」（同一 conversation 永远打到同一 Google 账号的同一 org）。
2. `cache_control` 字段（Claude Code 会注入 `{type:'ephemeral'}`）**被上游拒绝**：`Extra inputs are not permitted`，转换层必须剥离 → **OpenAI/Anthropic 的显式缓存标注在 Antigravity 通道上完全无效**，缓存退化为「靠请求前缀自然命中」的隐式行为。
3. 命中时上游返回 `cachedContentTokenCount`，映射为 `cache_read_input_tokens = cachedContentTokenCount`、`input_tokens = promptTokenCount - cachedContentTokenCount`。
4. **换号即缓存清零**（"When rate limits force an account rotation, cache continuity may temporarily reset"）。

量化代价：15 份 headless 日志、1,279 次调用中，**17 次调用输入 >100K 而 `cache_read_tokens = 0`**，合计 3,292,077 token，占这些运行全部未缓存输入的 **31%** —— [antigravity-cli #1245 评论](https://github.com/google-antigravity/antigravity-cli/issues/1245)（2026-10-19）。→ **缓存不命中直接放大配额消耗。**

`[社区]` 缓存读单价约为未缓存输入的 1/20 —— **未验证**是否为官方定价。

#### 5.3.11 system prompt 污染（容易被忽略）

`[项目]` [Mag1cFall/Antigravity2API](https://github.com/Mag1cFall/Antigravity2API) 的「系统提示档位」：每个生成请求的系统指令 = **官方客户端的系统提示** + 客户端自己的系统提示。转换层必须决定保留多少官方部分，共四档：

| 档位 | 官方系统提示 | 用户消息 |
|---|---|---|
| `full` | 全部段落 | 按官方格式包装，带本地时间与模型选择 |
| `medium` | identity + guidelines + communication_style 三段 | 带本地时间 |
| `aggressive` | 仅 identity | 原样发送 |
| `custom` | 自选段落 | 自定义包装 |

> **你在 OpenAI 里发的 system prompt 不是模型看到的全部**。无论哪一档，模型都额外看到一段 Google 写的 agent 人格与规则。这与 OpenAI「system 就是你发的那段」的语义直接冲突，且**会影响输出风格**（agent 化、倾向用工具）。对 SAG 是必须显式告知用户的行为差异。

#### 5.3.12 其它能力缺失速查

| 能力 | 状态 | 依据 |
|---|---|---|
| `n > 1`（多候选） | 上游 `candidateCount` 是 Preview 功能，且 `streamGenerateContent` 不支持 | [Google Cloud 参数文档](https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/capabilities/content-generation-parameters)（2026-11-07） |
| `logprobs` / `top_logprobs` | 上游无对应字段 | ANTIGRAVITY_API_SPEC |
| `response_format: json_schema` | 部分支持：`responseMimeType: application/json` + schema，但 schema 子集受限（无 `$ref`/`const`） | 同上 |
| `stop` / `stop_sequences` | 上游支持 `stopSequences`（string[]），可映射 | Google Cloud 参数文档 |
| 多轮 `tool` 角色 | 需重映射为 `role:"user"` + `functionResponse` part | ANTIGRAVITY_API_SPEC |
| WebSocket / Responses 原生 | 仅协议仿真；`/v1/responses` 的状态化语义（`previous_response_id`）无法真正实现 | Mag1cFall README 列了该端点但无 `previous_response_id` 说明 → **未验证** |

### 5.4 运维稳定性

#### 5.4.1 破坏性变更时间线（每月至少一次）

| 时间 | 变更 | 对转换层的影响 | 来源 |
|---|---|---|---|
| 2026-02-20 | `gemini-3-pro` 下线 | 硬编码该 ID 的配置全失效 | [Antigravity-Manager #2051](https://github.com/lbjlaq/Antigravity-Manager/issues/2051) |
| 2026-04-03 | 客户端升级后拒绝旧版本指纹，返回 "This version of Antigravity is no longer supported" 且**被当成成功响应**，破坏 failover | 版本 pin 策略必须重审 | [CLIProxyAPI #2523](https://github.com/router-for-me/CLIProxyAPI/issues/2523) |
| 2026-04-19 | Antigravity-Manager v4.1.32：重写 `requestId`（`agent/{timestamp}/{hex8}`）、注入 `__cloudCodeMeta`、同步 `serviceMachineId` 到 `state.vscdb` | 说明这些字段是**后来才被发现**需要对齐的 → 未来还会有 | [release notes](https://github.com/lbjlaq/Antigravity-Manager/releases/tag/v4.1.32) |
| 2026-05-19 | 2.0 + CLI + ACP；配额模型改「5h + 周上限」 | 额度语义变更 | [plans 变更](https://antigravity.google/blog/changes-to-antigravity-plans/) |
| 2026-06-12 | `gemini-3.1-pro-high` 被废弃 → 400；`gemini-3.1-pro-low`（thinkingBudget 1001）与 `gemini-pro-agent`（10001）成为活跃 ID | 别名表必须按 thinking 档位拆分 | [OmniRoute #3696](https://github.com/diegosouzapw/OmniRoute/issues/3696)；[antigravity-auth #1](https://github.com/cortexkit/antigravity-auth/issues/1)（MITM 实测，2026-06-14） |
| 2026-10 上旬 | Gemini 3.1 Pro / 3.7 Flash / 3.6 Flash 从个人档全面退役，Free/Plus/Pro 只剩 3.8 Flash | 模型目录缩水到 1 个 Gemini | `[社区]` [Linux.do](https://linux.do/t/topic/3027535)（2026-10-15，多人确认）；[Snow System 对照表](https://snowsystem.net/ai/antigravity-models-plan-comparison/)（2026-10-10） |
| 2026-10-14 | 需对齐 Hub 2.23.0 的 `User-Agent`（`aidev_client / cl`）与启动版本号 | UA 是硬编码在转换层里的**易碎常量** | [CLIProxyAPI #6619](https://github.com/router-for-me/CLIProxyAPI/issues/6619)（open） |
| 2026-11-04 | CLI 1.5.0 新增 `agy acp`、`--remote-agent`、local projects、`session/resume` | 新增能力，也新增需对齐的协议面 | [release 1.5.0](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.0) |
| 2026-11-18（预告） | 移除 Claude Sonnet/Opus 4.6 与 GPT-OSS-120b；日期已从 11-02 改到 11-18 一次 | 免费/Plus 档彻底失去第三方模型 | [docs/models](https://antigravity.google/docs/models/) |

**结论**：这是一个**每月至少一次破坏性变更**的协议面。任何「一次对齐、长期可用」的假设都不成立。

#### 5.4.2 issue 区高频故障模式（6 类，跨项目重复出现）

对 CLIProxyAPI（824 open issues）与 Antigravity-Manager（1,713 open issues）抽样后归类：

| # | 故障类 | 典型症状 | 代表 issue | 现状 |
|---|---|---|---|---|
| ① | **Thinking 签名 400**（最高频） | `Invalid signature in thinking block` / `Function call is missing a thought_signature` / `Corrupted thought signature` / `Expected thinking... but found tool_use/text`；触发于多轮工具调用、会话压缩后、服务重启后、跨模型切换、回退撤销 | Antigravity-Manager #608 / #547 / #3529 / #1575；CLIProxyAPI #1398 / #1584 / #4562 / #3470；sub2api #935；opencodex #2150 | 修复方式都是「打补丁 + 加缓存」，**没有根治** |
| ② | **OAuth token 失效（`invalid_grant`）** | 账号突然全部失效 | [Antigravity-Manager 文档 `proxy-invalid-grant.md`](https://github.com/lbjlaq/Antigravity-Manager/blob/main/docs/proxy-invalid-grant.md)（PR #203，2025-12-30）；[#3319](https://github.com/lbjlaq/Antigravity-Manager/issues/3319)（2026-08-22）；[CLIProxyAPI #833](https://github.com/router-for-me/CLIProxyAPI/issues/833)（2026-01-02，每 5 秒刷新死循环）；[sub2api #620](https://github.com/Wei-Shaw/sub2api/issues/620)（2026-02-24，P0） | 根因假设（**未验证**）：并发刷新时 token rotation 把后到的刷新判为重放，**永久吊销 refresh_token** |
| ③ | **429 分类与冷却逻辑 bug** | 亚秒级 `Retry-After` → 重试风暴（#5265）；并发成功清掉新冷却（#6643）；配额 API 显示 60–100% 但生成端点全 429（[#1015](https://github.com/router-for-me/CLIProxyAPI/issues/1015)，2026-01-14，17 个账号全中）；假 429（§5.2.2） | 同上 | 每修一处都会暴露新的对偶问题 |
| ④ | **模型下线 / 静默替换** | 见 §5.4.1 | — | `MODEL_REWRITE_RULES` / `MODEL_MAPPING` 变成常驻维护项 |
| ⑤ | **上下文估算与压缩** | `Prompt is too long` / `The input token count exceeds the maximum number of tokens allowed 1048576`；Claude Code 显示上下文 19–20%、实际已接近上限 → 自动 compact 不触发 → 突然报错 | [Antigravity-Manager PR #925](https://github.com/lbjlaq/Antigravity-Manager/pull/925)（2026-01-21，ASCII 4 字符/token、Unicode 1.5 字符/token + 15% 安全边际 + 从上游真实用量校准） | 估算永远只能逼近 |
| ⑥ | **流式语义破坏** | 见 §5.3.7 | — | — |
| ⑦ | **上游泛化错误 `Agent terminated due to error`** | 无具体原因，可能网络/MCP 冲突/输入过大/配额/服务端故障 | Antigravity-Manager #649 / #1427 / #1842 / #1900（均 open）；[排障文章](https://agentpedia.codes/blog/antigravity-agent-terminated-error)（2026-03 / 2026-10 更新） | **不是转换层引入的，官方客户端也会遇到** → 说明上游本身不稳 |

#### 5.4.3 维护成本的量化信号

| 指标 | CLIProxyAPI | Antigravity-Manager |
|---|---|---|
| Stars | 57,884 | 32,368 |
| **Open issues** | **824** | **1,713** |
| 主要语言 | Go | Rust（Tauri） |
| 首次创建 | 2025-07-01 | 2025-11-26 |
| 贡献者 | 10+ 核心 | 6+ 核心 |

（数据来自 2026-11-08 的 GitHub 仓库页快照）

> 一个 5.8 万星的 Go 项目仍挂着 824 个 open issue，其中大量是「上游变了」而非「代码有 bug」。这是**协议逆向类项目的结构性成本**：issue 数不收敛，因为它不是产品质量问题。

#### 5.4.4 失效是整体性的，不是渐进的

R3 跨项目交叉验证出的共同形态：**某次上游客户端升级后全线 4xx**，而不是可用性逐步下降。已记录的同类事件：

| 漂移事件 | 后果 | 来源 |
|---|---|---|
| Cursor 关闭文档页免费对话接口（2026-04-01） | 1,886★ 项目直接 archive | [7836246/cursor2api](https://github.com/7836246/cursor2api) |
| Gemini CLI 停服（2026-06-18）→ Antigravity CLI | ACP Registry 未收录，Zed 用户断档 | [zed Discussion #57221](https://github.com/zed-industries/zed/discussions/57221)（2026-05-20） |
| Antigravity 客户端升级后拒绝旧版本指纹（2026-04-03） | 全部请求返回 "no longer supported" 且被当成成功响应 | [CLIProxyAPI #2523](https://github.com/router-for-me/CLIProxyAPI/issues/2523) |
| Claude Code 自动更新越过 pin | 真实客户端被误判为第三方并改写请求 | [CLIProxyAPI #5443](https://github.com/router-for-me/CLIProxyAPI/issues/5443)（2026-09-03） |
| Codex app-server 协议小版本变更（0.159.3 → 0.163.0） | 新增必填 nullable 字段 `Turn.rootTurnId` 等，代理需重建 fixtures | [codex-app-server 兼容性注记](https://github.com/CIVITAS-John/codex-app-server-to-proxy/blob/main/docs/codex-app-server.md)（2026-10-21） |
| Claude Code 2.1.312 新增请求头 | 网关未转发，疑似风控相关（`未验证`因果） | [CLIProxyAPI #7066](https://github.com/router-for-me/CLIProxyAPI/issues/7066)（2026-10-28） |

---

## 6. 泛化：跨项目共性模式

### 6.1 九类共性限制（压缩为模式表）

| # | 模式 | 具体表现（跨 Claude Code / Codex / Cursor / Windsurf / Antigravity 一致） | 是否有解 |
|---|---|---|---|
| 1 | **refresh token 是单次使用的硬约束** | Codex 的 ChatGPT refresh token 单次使用，官方 CLI 与代理共享登录会互相作废；触发 `refresh_token_reused` / `expired` / `invalidated` 后只能重登。auth2api 直接把账号打进 **24 小时终态冷却** | **可解**（独占 auth-dir + 刷新并发锁 + 重登热重载） |
| 2 | **配额不是「限速」，是「分级配额 + 动态限频」** | Windsurf 周限模型每账号每周 5 次（31 个账号 ≈ 150 次即打满）；free tier 本地 RPM 10/min 但上游还有动态限频；Codex `usageLimitExceeded` → 429 且各档 reset 语义不同 | **不可解**（只能读配额、按配额路由、透出 reset 时间） |
| 3 | **有状态 agent 会话 ↔ 无状态 chat completions 的映射，四种方式都有代价** | transcript hash → sessionId（要求逐字回放，历史被编辑即退化）；显式会话头（**必须改客户端**）；自定义 `previous_response_id`（非标准扩展）；每次新建会话（agent 工具态每轮丢失） | **部分可解**，需显式建模会话键 |
| 4 | **工具执行权归属冲突** | agent 自己执行工具 vs `tools` 语义是「客户端执行」；三种对策（透传 / prompt 模拟 / 远端执行）。**server-side 工具必然丢弃** | **部分可解**；server-side 工具应显式报错而非静默丢弃 |
| 5 | **权限确认与 plan 模式在 OpenAI 协议里没有位置** | ACP 的 `session/request_permission` 是反向请求需人回答，无头网关只能「自动批准」或「改 agent 配置文件」；`plan` 事件只能塞进 `content` 污染正文或旁挂非标准字段；`mode` 甚至被当成模型 id 暴露在 `/v1/models` | **不可解**（「逐次人工审批」语义 100% 丢失，退化为部署期静态策略） |
| 6 | **agent 事件流 → 线性 SSE delta，必然丢信息** | 200 空流（需「先 priming 一个 chunk 再提交 200」）；usage 位置漂移（`finish_reason` 之前的 `choices: []` chunk）；推理内容与正文串台 | **可解**（工程细节） |
| 7 | **请求指纹与客户端伪装的军备竞赛** | 官方客户端会主动上报「我走了第三方网关」（`x-anthropic-billing-header` 的 `cc_gateway`，`未验证`）；签名字段逐版本对齐（`cc_version` = `SHA256(salt + 首条 user 第 4/7/20 字符 + 版本号)` 前 3 hex，`cch` = body 的 xxHash64）；**pin 版本 = 每次客户端自动更新就全员误判**（CLIProxyAPI 曾把真实 Claude Code 误判为第三方并改写其请求） | **只能缓解**（版本号当 floor 而非 pin，漂移告警一次） |
| 8 | **ToS 与封号是路线级风险，不是工程风险** | Anthropic 2026-01-09 技术性阻断伪装客户端；2026-02-19 法务页写明 OAuth token 用在其他产品「constitutes a violation」；2026-11-02 新规（11-12 生效）禁把 Pro/Max 额度包装成第三方 API 出售。OpenAI 2026-08-21 点名 sub2api「this type of usage gets flagged by our fraud-prevention systems」 | **不可解** |
| 9 | **协议版本漂移是所有这类项目的共同折旧曲线** | 见 §5.4.4。**维护成本 ≈ 上游客户端的发布频率，且失效是整体性的** | **不可解**（只能缩短发现时间） |

### 6.2 能力矩阵（保留最有代表性的行）

图例：✅ 普遍可用；⚠️ 部分可用/需降级；❌ 普遍丢失；? 未验证

| 能力 | Claude Code(OAuth 反代) | Codex(ChatGPT) | Cursor | Windsurf | ACP agent（公开协议） | **Antigravity（私有上游）** |
|---|---|---|---|---|---|---|
| 纯文本对话 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| SSE 流式 | ✅（多为强制上游流式，本地再聚合） | ✅ | ✅ | ✅ | ✅ | ⚠️ 有 6 类已知缺陷（§5.3.7） |
| 客户端侧 function calling | ⚠️ 透传或 prompt 模拟 | ⚠️ 需 Chat↔Responses 翻译 | ⚠️ 视实现 | ⚠️ 模型相关 | ⚠️ agent 自己执行，需区分 observational 工具 | ⚠️ schema 子集 + 签名链（§5.3.8） |
| 并行工具调用 | ⚠️ 上游常不支持，需 strip `parallel_tool_calls` | ⚠️ 需 override 模型目录 | ? | ⚠️ 需 native bridge 灰度 | ⚠️ 取决于 agent | ⚠️ 支持但文档薄 |
| `tool_choice` 强制指定函数 | ❌ | ❌ | ? | ⚠️ 有 `forced_tool_not_declared` 过滤 | ❌ 协议无此概念 | ❌ |
| 服务端工具（`file_search`/`computer_use`/`code_execution`） | ❌ 翻译层丢弃 | ❌ | ❌ | ❌ 翻译层丢弃 | ⚠️ 取决于 agent 内建工具 | ❌ `googleSearch`/`urlContext` 不能与 functionDeclarations 同请求 |
| `response_format=json_schema` | ⚠️ 部分 | ⚠️ 需转换 | ⚠️ | ⚠️ | ❌ | ⚠️ schema 子集（无 `$ref`/`const`） |
| 图片输入 | ⚠️ 视来源 | ⚠️ 多数只支持 text-only | ⚠️ | ✅ | ⚠️ 需 `promptCapabilities.image` | ⚠️ base64 inline；长会话中被压缩逻辑丢弃 |
| reasoning/thinking 暴露 | ⚠️ 需通道分离 | ⚠️ 非标准字段 | ⚠️ 派生 `-thinking` 模型 | ⚠️ 易与正文串台 | ✅ 但 OpenAI 无对应位 | ⚠️ `thoughtsTokenCount` 语义与 `reasoning_tokens` 不等价 |
| usage 精确计量 | ⚠️ 可能缺失或含隐藏 reasoning token | ⚠️ 缺失时**省略而非估算** | ❌ 常报 0 | ✅ 可从 Cascade metadata 抓 | ⚠️ `usage_update` 可选 | ⚠️ 记账缺口（§5.3.7） |
| 会话连续性 | ⚠️ 靠上游 conversation_id 或 transcript hash | ⚠️ thread resume / `previous_response_id` | ⚠️ | ⚠️ Cascade 复用 | ⚠️ `session/load`，需 hash 或显式头 | ⚠️ 需伪造身份连续性（§5.3.6） |
| 系统提示控制 | ⚠️ 上游可能重写 system | ⚠️ 被 lift 成 `instructions` | ⚠️ | ⚠️ | ⚠️ `systemPrompt` 能力协商（2026-11-06 才 stabilized） | ❌ 官方系统提示必然叠加（§5.3.11） |
| `temperature`/`top_p`/`max_tokens` | ❌ 基本被忽略 | ❌ `max_output_tokens` 被 strip | ❌ | ❌ `MAX_TOKENS` 为网关默认值 | ⚠️ 取决于 agent 是否暴露 config option | ❌ 3.6+ 静默忽略；`max_tokens` 硬顶 64000 |
| `stop` / `logprobs` / `n>1` | ❌ | ❌（最多 1 个 choice） | ❌ | ❌ | ❌ | ⚠️ 仅 `stop` 可映射 |
| `count_tokens` | ⚠️ 仅 Anthropic 原生路径 | ❌ 501 | ❌ 501 | ❌ | ❌ | ⚠️ `v1internal:countTokens` 存在但对有状态字段 400 |
| 错误语义保真 | ⚠️ 429/400 语义模糊 | ⚠️ 可映射 `usageLimitExceeded`→429 + reset | ⚠️ | ⚠️ 可给真实 `Retry-After` | ⚠️ JSON-RPC error → HTTP 需自定映射 | ⚠️ 四种 429 + 假 429 + 403 封禁（§5.2） |
| 模型列表真实性 | ⚠️ 静态表 + 逐账号探测 | ✅ 可 live 拉取（`model/list`、ETag 缓存） | ⚠️ 交集/别名 | ⚠️ 目录同步 + entitlement | ✅ `agentInfo` + modes | ⚠️ 档位静默少模型 + 静默降级 |
| 稳定性（月级） | ❌ 政策 + 技术双重打击 | ⚠️ 政策风险中，技术路径较稳 | ⚠️ 官方 API Key 路线较稳 | ⚠️ 依赖 LS 二进制版本 pin | ✅ 协议公开（但产品可能下线） | ❌ 每月破坏性变更 + 封禁潮 |

### 6.3 六类缓解手段（工程外资产）

| # | 手段 | 关键设计点 |
|---|---|---|
| 1 | **账号池 + 粘性会话 + 分级冷却** | sticky 的键必须是「**调用方 + 模型**」而不是「请求」（WindsurfAPI PR #162 的 `callerKey + modelKey → accountId`）；冷却要能区分「配额耗尽」与「传输层 429」的双链根因，否则「刚被 429 的账号几秒内被重选，一路撞到硬封」（WindsurfAPI PR #224）；客户端断连**不计入账号故障**（PR #225） |
| 2 | **请求整形 / 指纹对齐 / 节奏控制** | 版本号作为 **floor 而非 pin**（`candidate >= baseline` + major 上限），漂移告警一次并暴露 `GET /v0/management/<provider>-client-versions`；设备指纹**按上游身份固定**；补齐 billing header 派生字段与 session-id 头；**主动的请求卫生**（清除 `x-anthropic-billing-header`、剥离上游身份自称——WindsurfAPI 的 `neutralizeClientIdentity` a1–a7 规则集） |
| 3 | **客户端版本跟进机制** | 把「跟随最新模型/版本」做成运行时能力：每小时刷新 Codex 模型列表；动态拉 `/v1/models` 取多 Key 交集；生成式契约（`codex app-server generate-json-schema`）+ `Reviewed on <date> against <version>` 注记 |
| 4 | **出口 IP 策略** | 给账号绑定不同出口 IP；CLIProxyAPI README 的赞助商区长期挂着住宅代理服务（"one-IP-per-account"），**这本身就是一个信号：该生态把出口 IP 当作一等运维对象**。社区经验（`未验证`）：登录 IP 比使用 IP 更敏感；机房 IP 易触发 Cloudflare 质询；IP 频繁跳变是封号主因之一 |
| 5 | **降级策略（比硬失败更重要）** | 模型降级（周限模型 → daily 配额模型）；工具降级（native tool → prompt 模拟 → 纯文本）；多模态降级（本地 OCR 兜底、远程图片被拒时「只丢这张图，文本照发」）；**显式 `degraded` 状态**——把「工具假成功、`max_tokens` 未续写、模型自述写到一半」标成 degraded 并在日志里给原因。**承认降级比假装成功更可运维** |
| 6 | **合法性边界声明（工程外）** | WindsurfAPI 用整节写「BYO-account / 不提供账号 / 多账号用于同一部署者的故障切换而非公共池」；sub2api README 写「使用可能违反上游 ToS，风险自负，不授权商业运营」；star-cliproxy 写「Subscriptions and CLI usage remain subject to each provider's terms of service」 |

### 6.4 核心判断

**协议翻译可解，身份与配额不可解，只能缓解。**

- **可泛化的部分**：协议翻译层。CLIProxyAPI 一家覆盖 8 类上游，WindsurfAPI 一家覆盖 5 套出口协议，ACP 生态已有 4+ 个「ACP → OpenAI」通用转换器。**「translator + executor 插件化」是被反复验证的架构。**
- **不可泛化的部分**：凭证与配额。每家的 refresh 语义、配额窗口、风控信号、ToS 边界都不同，且都在**主动对抗**。缓解手段（账号池、指纹、IP、节奏）是**运营资产而不是代码资产**——它们无法随代码一起复用，且随时可能整体归零。
- **ACP 的出现没有改变这一点**。它标准化的是「编辑器 ↔ agent」的客户端侧，不标准化「agent ↔ 模型厂商」的授权侧。Zed 官方文档明确把责任推给 agent 提供方："Billing, legal terms, retention, and data handling are between you and the agent provider."（[Zed — External Agents](https://zed.dev/docs/ai/external-agents)）

---

## 7. SAG 接入评估（落点）

### 7.1 架构契合度：几乎零成本，这是问题所在

SAG 现有架构事实（来自本仓库代码核对）：

| SAG 侧能力 | 位置 | 对本次接入的含义 |
|---|---|---|
| `LlmProvider` trait（标准化 Chat 请求/响应 + SSE chunk 流，含 real_model、api_endpoint、target_extra、timeout） | `sag-providers/src/traits.rs:14-43` | 转换服务若暴露 OpenAI 兼容面，可直接复用现有 OpenAI 适配器 |
| `apis` 协议 catalog 工厂（`create_provider` → 按 catalog 创建单实例并委派适配器） | `sag-providers/src/catalog.rs:14-34`、`sag-providers/src/lib.rs:19-55` | **没有 `provider_type` 分派**，因此新增一个「转换服务上游」不需要新类型 |
| 任意 OpenAI 兼容端点：`apis.*.base_url` + headers/body overrides | `sag-config/src/config.rs:1297` 起 `ApiConfig`；`sag-config/src/loader/load.rs:195` 示例 | 只要转换服务给标准 `/v1/chat/completions`，加一个 provider 节点即可 |
| `provider_type` 已移除，迁移清理逻辑在 loader | `sag-config/src/loader/save.rs:1078-1088` | 不要再引入「按来源分派」的类型枚举 |
| Admin API 增删改查 provider + 模型列表 / fetch-models / probe | `sag-admin/src/admin_providers.rs:257,277,345,451`；`sag-admin/src/admin.rs:40-48` | 凭证生命周期、探活可复用现有通道 |
| 重试语义：TTFB 超时可重试；4xx 不重试 | `sag-config/src/config.rs` ProviderConfig TTFB 注释；`docs/spec/dispatch/changes/0004-upstream-4xx-non-retryable.md` | **`429` / `403` 的分类必须在这里做对**，否则「假 429」会被当成真限速冷却，`TOS_VIOLATION` 403 会被当成不可重试的客户端错误直接回给用户 |
| 目标池 / 会话粘性 / 断路器 | `sag-router/src/session.rs`、`docs/spec/circuit-breaker/spec.md` | 会话粘性已有落点；但 SAG 默认假设「上游稳定」，Anitgravity 通道需要更激进的断路器 |
| 文档结构：`docs/` 顶层只有 `guide/` 与 `spec/`，无 research 目录 | `docs/index.md:4-6`；`docs/spec/index.md` | 本文新建 `docs/research/`，是该目录的第一个文件 |

**结论**：技术上，「接一个 antigravity2api 类转换服务」在 SAG 侧的成本约等于「加一个 provider 配置节点 + 若干 override 头」。**正是这种低成本构成了主要危险**——它让一个不可承诺 SLA、随时可能因封号整体失效、且失效形态是 `403`/假 `429` 的上游，看起来和普通上游一样。

### 7.2 Tier A / B / C 分层建议

| 层 | 定义 | 具体来源 | SAG 策略 |
|---|---|---|---|
| **Tier A**（可承诺 SLA） | 官方 API Key / 公开协议 + 自带认证 | OpenAI / Anthropic / Google AI Studio / Vertex 官方 key；Cursor `crsr_…` API Key + `@cursor/sdk`；**ACP agent（协议公开、版本协商、能力协商、错误码规范）** | 可作为正式上游；在 `/v1/models` 里正常暴露；纳入容量规划 |
| **Tier B**（可提供但不承诺） | **官方 CLI / app-server 驱动型** | Codex `app-server`（ChatGPT 订阅）、`claude -p`、`agy acp`（仅 Gemini 模型） | 技术上稳（上游看到真客户端，指纹问题消失），但 ToS 风险仍在（Anthropic 明确禁止；OpenAI 要求合作或纯 OSS；Antigravity FAQ 明确 "not supported under consumer plans"）。**必须标注为实验通道，且不与其上层承诺 SLA** |
| **Tier C**（仅自用/研究） | **token 重放 + 指纹伪装型** | Antigravity Cloud Code `v1internal` 直连（agy2api / CLIProxyAPI 等）、Claude Code OAuth 反代、Windsurf LS 桥接 | **不适合作为对外网关的 provider**。2026-01 与 2026-11 两轮 Anthropic 政策收紧、2026-08 OpenAI 点名 sub2api、2026-11-06 Antigravity 凭证级 `TOS_VIOLATION`，已经把这条路线的政策风险从「灰」推到「明」 |

**Antigravity 的具体定位**：如果 SAG 要做，只能是 **Tier C 的「可弃实验通道」**：

- 独立 provider 命名（如 `antigravity-experimental`），**不要与合规通道放在同一个 `/v1/models` 列表而不加标注**；
- 配额与降级上与合规通道**硬隔离**（独立断路器、独立并发池、独立 token 账本）；
- 响应头显式标注通道来源与实验性；
- 文档里附官方推荐的替代方案（Vertex / AI Studio API key）——[FAQ](https://www.antigravity.google/docs/faq/) 自己就是这么建议的。这既是合规姿态，也是产品差异化。

### 7.3 若接入，需要哪些适配假设（合并 R2 的 8 条建议 + R3 的 10 条契约假设）

| # | 适配假设 | 具体要求 | 若不做会怎样 |
|---|---|---|---|
| 1 | **429 必须分五类，不能合并** | `QUOTA_EXHAUSTED`（等 reset）/ `RATE_LIMIT_EXCEEDED`（按 `Retry-After`）/ `MODEL_CAPACITY_EXHAUSTED`（换模型）/ `RESOURCE_EXHAUSTED` 泛化（探测）/ **假 429（内容级 WAF：改写或剥离指纹后重试，绝不冷却账号）** | 把 WAF 假 429 当限速 → 健康账号被打进冷却 → 整池雪崩；把真配额 429 当 WAF → 无限重试烧配额 |
| 2 | **403 与 429 语义分离，403 单独一类** | `TOS_VIOLATION` / `PERMISSION_DENIED` = 凭证级封禁 → 停用该凭证 + 告警，**不回给用户当普通 4xx**；`Verify your account` = 需人工介入 | 封禁被当成「上游 4xx 不重试」直接透传，运维看不见系统性封禁 |
| 3 | **参数契约要显式降级，不要静默吞掉** | `temperature`/`top_p`/`top_k` 在 3.6+ 无效；`frequency_penalty`/`presence_penalty` 会 400；`max_tokens` 需 clamp 到模型上限且 > thinking budget。响应里返回 `x-sag-params-dropped` / `x-sag-params-clamped` | 用户设 `temperature=0` 却拿到非确定性输出，且无从得知 |
| 4 | **thinking 签名必须做决定，并写进文档** | 二选一：网关内维护 per-conversation 签名存储（引入状态、TTL、重启失效语义）或 `skip_thought_signature_validator` 哨兵（放弃上游校验）。**OpenAI 协议里没有这个位置，这是路线级设计债** | 多轮工具调用随机 400，且无法归因 |
| 5 | **会话粘性要做成显式能力，不是隐式副作用** | 「conversation → account」绑定必须暴露为配置项（如 Cache First / Balance）+ 配套清理接口；session key 默认按 `api key + model` 隔离 | 「单账号被打爆 + 其他账号闲置」，跨用户上下文串台 |
| 6 | **请求不是权威的** | 上游可能重写 `system`、重命名工具、剥离 betas、强制 `stream: true`、strip `max_tokens`/`parallel_tool_calls`、改写 `metadata.user_id`。契约里要区分「客户端意图」与「上游实际收到的请求」，并保留可审计 request-log | 客户侧表现为「偶发的、无法解释的坏」，且无法复现 |
| 7 | **响应可能不完整但 HTTP 200** | 空流、半截 tool call、`response.output` 恒空、`usage` 缺失。需支持「priming 首 chunk 再提交 200」、`degraded` 状态位、「usage 缺失 = 省略而非 0」 | 客户端拿到 200 空响应；严格 serde 客户端（如 Rust）整轮失败 |
| 8 | **`model` 字段语义会被污染** | 需独立的 `model_alias → (provider, real_model, effort)` 映射层，不直传；记录并回显**实际模型**（防静默降级） | 用户以为在用 A，实际拿到 B |
| 9 | **工具语义要标注「谁执行」** | `observational`（上游已执行，客户端不得执行、不得回传 `tool_results`、永不产生 `finish_reason: "tool_calls"`）/ `client-side`（必须回传 `tool_result`）/ `server-side`（网关无法支持，**显式 400 而非静默丢弃**）。`tool_choice` 不支持的情况要显式降级 | 双重工具循环（agent 跑了一次 shell，客户端又执行一次） |
| 10 | **错误码必须双层** | HTTP 状态 + 上游原因（`usage_limit_exceeded` / `server_overloaded` / `insufficient_credits` / `model_not_entitled` / `refresh_token_reused`）+ 建议重试时间（`Retry-After` / `reset_at`） | 把所有上游错误压成 500 会直接摧毁客户端的退避逻辑 |
| 11 | **配额是外部状态，网关不能承诺** | 只能「读配额、按配额路由、把 reset 时间透出」；读失败时省略 reset 字段但保留类型化 429 | 网关承诺了它无法保证的东西 |
| 12 | **凭证生命周期是网关的责任** | 独占 auth-dir、刷新并发锁、单次 refresh token 的处理、重登后的热重载；契约上把「凭证失效」（部署事件）与「请求失败」（请求事件）分开 | refresh token 被并发刷新判为重放而永久吊销 |
| 13 | **模型目录必须实时拉取，禁止硬编码** | `/v1/models` 以上游目录为唯一真相（含档位 entitlement），缓存上次快照以便初始化期降级；`MODEL_REWRITE_RULES` 视为常驻维护项 | 每月一次的模型下线直接让硬编码配置全线 404 |
| 14 | **版本漂移必须可观测** | 暴露 `/v0/management/<provider>-client-versions`；上游版本 pin 当 floor 不当 pin；漂移时**告警一次**而不是静默改写请求 | 「pin 版本 = 每次客户端自动更新就全员误判」 |
| 15 | **可观测性要记录实际 token 账本** | 输入 / 缓存读 / 输出 / 思考分开记（存在静默替换与 `thoughtsTokenCount` 记账缺口） | 无法回答「这次请求花了多少配额」 |
| 16 | **合法性/可撤销性写进 provider 元数据** | 同一份 OpenAI 兼容契约下，「官方 API Key 来源」与「订阅凭证反代来源」的 SLA、ToS 风险、可撤销性完全不同 | 网关把不可承诺的东西承诺出去了 |

### 7.4 明确结论

1. **Antigravity 通道只能定位为「可弃的实验通道」，不是正式上游。** 决定因素不是技术可行性，而是「账号资产随时被单点清零」——而这个清零在 2026-11 已经细化到**凭证级 + 服务级**，连「账号还在」这个信号都不再可靠。
2. **「ACP → OpenAI」在 Antigravity 上有官方通道，但被阉割到只剩 Gemini**（registry #554 + Google 员工原话 + FAQ），且是 agent 语义、需要交互式回答 `session/request_permission`。把它做成 `/v1/chat/completions` 会**同时丢掉 Claude/GPT 模型和工具执行能力**——剩下的价值不足以支撑接入。
3. **「私有协议 → OpenAI」的全部价值都在 Cloud Code `v1internal`，而这条路径被官方书面判定违规**（ToS §6 + FAQ 两条 + 2026-11-06 `TOS_VIOLATION` + 2026-02 连带封禁 + 二次永久封禁）。它不是「技术可行性」问题，是「合规与账号资产」问题。
4. **协议脆弱性可量化**：UA 版本号 gate（#5175）、字段白名单（#118、`dependentRequired`）、端点身份绑定（#4749 的 consumer 只能打 `daily`）、模型下线日程（2026-11-18）——四类变更在 2026 年 2–11 月各至少发生过一次。任何 SAG 侧适配层都需要「**按 provider 版本号 + 模型目录 + 字段白名单三元组做灰度**」，而不是硬编码。
5. **如果 SAG 要长期支持真正的 ACP**，那是一条独立的价值线：协议公开、版本协商、能力协商、错误码规范，且 `systemPrompt` 能力在 2026-11-06 才 stabilized。但这条线解决的是「编辑器 ↔ agent」的互通，**不解决订阅额度转 API 的问题**——不要用同一个功能叙事把它们混在一起。

---

## 8. 未验证事项清单（合并 R1 / R2，已去重）

| # | 未验证事项 | 为什么重要 | 需要什么才能验证 |
|---|---|---|---|
| 1 | 「上游 Cloud Code 用 gRPC」 | 与多数抓包实现冲突，影响逆向工作量估计 | 一次 mitmproxy 抓包复核 |
| 2 | 「Gemini CLI 2026-06-18 停服」 | 仅二手媒体（CloudZero），无 Google 一手文档 | 官方 changelog 原文 |
| 3 | 「封禁会连带整个 Google 账号（Gmail 等）」 | ToS 文本只写 Antigravity / Gemini CLI 账号；Ground Truth 明确说该说法无法用一手来源证实 | 官方书面澄清 |
| 4 | 「Antigravity ACP 二进制在 39-bit 寻址 ARM（多数 Pixel/Android）上不可用」 | 影响移动端可用性判断 | 论坛单一用户报告（2026-11-01），需复现 |
| 5 | 「zero-gravity 的 Pro 账号 3 小时被封 vs Ultra 3 天无问题」 | 常被引作「Ultra 更安全」的证据 | 作者自述 n=1，需对照实验 |
| 6 | 各项目 README 声称的 `/v1/audio`、`/v1/files` 等端点是否真有上游支撑 | 影响「能力矩阵」的乐观程度 | 读实现或实测 |
| 7 | `agy_acp_server` 1.3.0（当前 registry 版本）是否已修掉 #554 的 Gemini-only 过滤 | 决定「官方 ACP 路线」能否拿到 Claude/GPT | registry #554 只覆盖 1.0.0/RC01，需实跑二进制 |
| 8 | Antigravity 是否就 Blyck（#196885）给出书面答复 | 决定 2026-10-26「开口」的稳定性 | 截至 2026-11-08 该帖无 Google 回复 |
| 9 | 2026-11-06 `TOS_VIOLATION` 浪潮在 11-08 之后的扩散面 | 决定封禁是「抽样」还是「全量」 | 需要新 issue 报告第三、第四个凭证被打 |
| 10 | 16384 输出上限在 2026-11 版本是否仍成立 | 与 64000 冲突，影响 `max_tokens` clamp 值 | 单方测量，需复测 |
| 11 | 上游是否对 cached token 在配额折算中打折 | 影响成本模型 | 社区称 1/20，无官方确认 |
| 12 | `seed` 是否透传 | 影响确定性输出承诺 | 读 translator 或实测 |
| 13 | `/v1/responses` 的 `previous_response_id` 是否被任何项目真正实现 | 影响「Responses 原生」的能力声明 | 读实现 |
| 14 | 是否存在客户端完整性校验（签名 / attestation），而非仅 TLS + header + 设备指纹 | 决定「指纹复刻」路线的天花板 | 9router #1138 只提「潜在信号」，无实证 |
| 15 | Anthropic 商业条款是否对「经 Google 转出的 Claude」实际执法 | 决定 Tier B/C 的双重合规风险 | 无公开案例 |
| 16 | Enterprise / Gemini Enterprise 通道是否允许第三方客户端访问 `v1internal` | **这可能是唯一合规路径** | 需企业合同与官方答复 |
| 17 | 静默降级时响应里的 `modelVersion` 字段是否也被改写 | 决定网关能否靠响应字段发现降级 | 需能复现模型下线的账号，或读 translator 代码 |
| 18 | 上游侧会话 / trajectory 堆积是否有配额或性能成本 | 影响长期运行的稳定性判断 | 无公开数据 |
| 19 | 是否存在「静默降权」（响应变慢/质量下降但不报错） | 影响 SLA 判定 | Antigravity-Manager 的 QoS 只是代理侧启发式 |
| 20 | 9router #1138 的「多层检测」具体清单 | 是功能请求而非实测结论 | 需逐项实测 |
| 21 | Claude Code 2.1.30x 起的 `cc_gateway` 风控头上报 | 社区逆向，无官方确认；影响 Tier C 可行性 | 官方确认或抓包复现 |
| 22 | 9router #1138 提到的 "Missing/invalid device attestation" 是否真是信号 | 影响检测层清单完整性 | 无实证 |

---

## 9. 来源清单（按主题精选）

> 正文中以行内链接引用的均为下表条目或其直接延伸（issue 号 / PR 号）。星数与 push 时间统一取自 GitHub REST API（`api.github.com/repos/<owner>/<repo>`）于 2026-11-08 的实测，而非搜索摘要。

### 9.1 Antigravity 官方文档与政策

- [Introducing Google Antigravity](https://antigravity.google/blog/introducing-google-antigravity)（2025-11-18）
- [Introducing Google Antigravity 2.0](https://antigravity.google/blog/introducing-google-antigravity-2/)（2026-05-19）
- [Introducing Google Antigravity CLI](https://antigravity.google/blog/introducing-google-antigravity-cli/)（2026-05-19）
- [Changes to Antigravity plans](https://antigravity.google/blog/changes-to-antigravity-plans/)（2026-05-19）
- [Google I/O 2026 developer highlights](https://blog.google/innovation-and-ai/technology/developers-tools/google-io-2026-developer-highlights/)（2026-05-19）
- [Plans | Google Antigravity Docs](https://antigravity.google/docs/plans/)（读取 2026-11-08）
- [Choose a model | Google Antigravity Docs](https://antigravity.google/docs/models/)（读取 2026-11-08）
- [Google Antigravity - Terms of Service](https://antigravity.google/terms/)（读取 2026-11-08，§6/§8）
- [FAQ | Google Antigravity Docs](https://www.antigravity.google/docs/faq/)（读取 2026-11-08）
- [antigravity.google/pricing](https://antigravity.google/pricing)（读取 2026-11-08）
- [Authenticate | Antigravity Docs](https://antigravity.google/docs/install/)
- [Agent Client Protocol (ACP) | Antigravity CLI Docs](https://antigravity.google/docs/cli/acp/)
- [Other editors (ACP) | Antigravity Docs](https://antigravity.google/docs/ide/extensions/acp/)
- [Model quotas (/usage) | Antigravity CLI Docs](https://antigravity.google/docs/cli/commands/usage/)
- [Releases · google-antigravity/antigravity-cli](https://github.com/google-antigravity/antigravity-cli/releases)（1.5.0 / 1.5.1 / 1.5.3 均在 2026-11-04~07）
- [antigravity-cli release 1.5.0](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.0)（2026-11-04）
- [antigravity-cli issue #1245](https://github.com/google-antigravity/antigravity-cli/issues/1245)（2026-10-14，1M 上下文与压缩的失败模式）
- [antigravity-sdk-python Conversation README](https://github.com/google-antigravity/antigravity-sdk-python/blob/main/google/antigravity/conversation/README.md)
- [Content generation parameters | Gemini Enterprise Agent Platform](https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/capabilities/content-generation-parameters)（2026-11-07）
- [使用 Google AI Ultra 福利 - Google One 帮助](https://support.google.com/googleone/answer/16286513?hl=zh-)

### 9.2 官方论坛与封禁一手记录

- [discuss #192027 — ToS clarification: is T3 Code / official Antigravity ACP agent permitted](https://discuss.ai.google.dev/t/tos-clarification-is-t3-code-official-antigravity-acp-agent-permitted-under-the-antigravity-terms-of-service/192027/2)（2026-10-26，Google 工作人员回复）
- [discuss #196885 — Blyck 请求书面确认](https://discuss.ai.google.dev/t/tos-clarification-desktop-app-blyck-using-the-official-antigravity-acp-binary-over-acp-with-the-users-own-personal-plan/196885)（2026-10-30，未获答复）
- [discuss #197131 — Conflicting guidance on using T3 Code](https://discuss.ai.google.dev/t/conflicting-guidance-on-using-t3-code-with-personal-google-ai-subscriptions/197131)
- [discuss #198430 — 1M advertised context vs 256K compaction clamp + `agy -p` 政策反转记录](https://discuss.ai.google.dev/t/1m-advertised-context-vs-a-256k-compaction-clamp-in-the-shipped-binaries-plus-quota-and-agy-p-policy-questions-desktop-2-29-0-cli-1-5-1/198430)（2026-11-05）
- [discuss #197297 — Account suspended again: multi-agent setup calling Antigravity CLI](https://discuss.ai.google.dev/t/account-suspended-again-multi-agent-setup-calling-antigravity-cli/197297)（2026-11-01）
- [discuss #188723 — Gemini models are missing on Antigravity](https://discuss.ai.google.dev/t/gemini-models-are-missing-on-antigravity/188723/8)（2026-10-13，静默降级 warning）
- [discuss #190310 — Google AI Pro (Jio/Partner plan) losing all Pro & Third-Party models](https://discuss.ai.google.dev/t/google-ai-pro-jio-partner-plan-losing-all-pro-third-party-models-on-nov-2-leaving-only-flash-3-8/190310)
- [gemini-cli Discussion #20632 — Addressing Antigravity Bans & Reinstating Access](https://github.com/google-gemini/gemini-cli/discussions/20632)（2026-02-27）
- [agentclientprotocol/registry issue #554](https://github.com/agentclientprotocol/registry/issues/554)（2026-08-28，Gemini-only 过滤器源码证据）
- [registry/antigravity-acp/agent.json](https://github.com/agentclientprotocol/registry/blob/main/antigravity-acp/agent.json)（读取 2026-11-08，v1.3.0）
- [Ground Truth — Google's Antigravity terms ban third-party clients outright](https://groundtruth.day/news/googles-antigravity-terms-ban-third-party-clients-outright.html)（2026-09-03）
- [Zentor — Antigravity account suspended](https://zentor.ai/blog/antigravity-account-suspended-third-party)（2026-09-04，更新至 2026-10-27）
- [VentureBeat — Google clamps down on Antigravity malicious usage](https://venturebeat.com/orchestration/google-clamps-down-on-antigravity-malicious-usage-cutting-off-openclaw-users)（2026-02-23）
- [The Verge — Google Antigravity IDE coding agent](https://www.theverge.com/news/822833/google-antigravity-ide-coding-agent-gemini-3-pro)（2025-11-18）
- [Simon Willison — Google Antigravity](https://simonwillison.net/2025/Nov/18/google-antigravity/)（2025-11-18）

### 9.3 Antigravity 协议逆向与转换项目

- [cortexkit/antigravity-auth — ANTIGRAVITY_API_SPEC.md](https://github.com/cortexkit/antigravity-auth/blob/main/packages/opencode/docs/ANTIGRAVITY_API_SPEC.md)（v1.0，2025-12-13，"Verified by Direct API Testing"）
- [aphrody-antigravity — endpoints.rs](https://docs.rs/aphrody-antigravity/latest/src/aphrody_antigravity/endpoints.rs.html) / [auth.rs](https://docs.rs/aphrody-antigravity/latest/src/aphrody_antigravity/auth.rs.html)
- [suasgn/openburn — docs/providers/antigravity.md](https://github.com/suasgn/openburn/blob/main/docs/providers/antigravity.md)
- [jackwener/open-antigravity](https://github.com/jackwener/open-antigravity) / [src/bridge/grpc.ts](https://github.com/jackwener/open-antigravity/blob/fab75833/src/bridge/grpc.ts) / [DeepWiki Glossary](https://deepwiki.com/jackwener/open-antigravity/7-glossary)
- [su-kaka/agy2api](https://github.com/su-kaka/agy2api)
- [liuw1535/antigravity2api-nodejs](https://github.com/liuw1535/antigravity2api-nodejs) / [API.md](https://github.com/liuw1535/antigravity2api-nodejs/blob/main/API.md) / [issue #118](https://github.com/liuw1535/antigravity2api-nodejs/issues/118) / [DeepWiki](https://deepwiki.com/liuw1535/antigravity2api-nodejs)
- [router-for-me/CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) —— 关键 issue/PR：#4749（端点身份绑定）、#5175 / #5177（UA 版本 gate）、#5751（假 429）、#7296（TOS_VIOLATION 凭证级）、#2523（旧版本指纹拒绝）、#5265 / #6643（并发与冷却）、#1015（配额显示与实际不符）、#1398 / #1584（签名）、#6463 / #4498 / #747 / #752（流式与 usage）、#1781 / PR #2800（max_tokens）、PR #3470 / PR #4562（签名策略）、#6619（UA 常量）、[commit adf0529](https://github.com/router-for-me/CLIProxyAPI/commit/adf052984f8b53bbc6ac8e4338297afee5b337f5)
- [justlovemaki/AIClient2API](https://github.com/justlovemaki/AIClient2API) / [docs/ANTIGRAVITY_FREE_TIER_MODELS.md](https://github.com/justlovemaki/AIClient2API/blob/main/docs/ANTIGRAVITY_FREE_TIER_MODELS.md) / [PromptQuorum 评测](https://www.promptquorum.com/power-local-llm/aiclient2api-review)（2026-09-19）
- [lbjlaq/Antigravity-Manager](https://github.com/lbjlaq/Antigravity-Manager) —— 关键 issue/PR：#3529 / #608 / #547 / #1575（签名）、#3325（1M 硬上限与估算误差）、#2051（模型下线）、#1842（地区）、PR #523（设备指纹）、PR #925（token 估算）、PR #1094（SignatureCache）、[docs/proxy-invalid-grant.md](https://github.com/lbjlaq/Antigravity-Manager/blob/main/docs/proxy-invalid-grant.md) / [release v4.1.32](https://github.com/lbjlaq/Antigravity-Manager/releases/tag/v4.1.32) / [release v4.3.3](https://github.com/lbjlaq/Antigravity-Manager/releases/tag/v4.3.3) / [DeepWiki 9.3 session management](https://deepwiki.com/lbjlaq/Antigravity-Manager/9.3-session-management)
- [Draculabo/AntigravityManager](https://github.com/Draculabo/AntigravityManager) / [PR #224](https://github.com/Draculabo/AntigravityManager/pull/224)
- [lbjlaq/Antigravity-Tools-LS](https://github.com/lbjlaq/Antigravity-Tools-LS)
- [zhe-gu/zero-gravity](https://github.com/zhe-gu/zero-gravity)
- [frieser/antigravity-proxy](https://github.com/frieser/antigravity-proxy) / [DeepWiki 翻译层](https://deepwiki.com/frieser/antigravity-proxy/3-protocol-translation-layer)
- [GhostXia/agy2api](https://github.com/GhostXia/agy2api) / [truongqv12/agy2api](https://github.com/truongqv12/agy2api)
- [Kazuki-0147/Antigravity-Proxy](https://github.com/Kazuki-0147/Antigravity-Proxy) / [Mag1cFall/Antigravity2API](https://github.com/Mag1cFall/Antigravity2API) / [link0518/Antigravity2api](https://github.com/link0518/Antigravity2api) / [znlsl/Antigravity2Api](https://github.com/znlsl/Antigravity2Api) / [anshulrules/Antigravity2Api](https://github.com/anshulrules/Antigravity2Api)
- [shubzkothekar/antigravity-acp](https://github.com/shubzkothekar/antigravity-acp) / [issue #3](https://github.com/shubzkothekar/antigravity-acp/issues/3)
- [shindgew/agy-acp](https://github.com/shindgew/agy-acp) / [构建记](https://shindgew.com/blocks/building-agy-acp/)（2026-08-17）
- [tiezbro/paseo-agy-acp](https://github.com/tiezbro/paseo-agy-acp) / [zacbemis/pi-antigravity-acp-provider](https://github.com/zacbemis/pi-antigravity-acp-provider) / [yuting0624/antigravity-for-claude-code](https://github.com/yuting0624/antigravity-for-claude-code) / [t3code PR #9348](https://github.com/pingdotgg/t3code/pull/9348)
- [openclaw/acpx — agents/Antigravity.md](https://github.com/openclaw/acpx/blob/main/agents/Antigravity.md)
- [pi-go — Antigravity ACP](https://pi-go.sh/blog/antigravity-acp/)（2026-08-29）
- [ACP Protocol Reference（pi-antigravity-bridge）](https://cdn.jsdelivr.net/npm/@estebanforge/pi-antigravity-bridge@1.4.7/docs/ACP-PROTOCOL-REFERENCE.md)（"verified LIVE on 2026-09-03"）
- [txcript — antigravity harness 逆向文档](https://docs.rs/txcript/latest/txcript/harness/antigravity/index.html)
- [badrisnarayanan/antigravity-claude-proxy — DeepWiki 6.3 Prompt Caching and Sessions](https://deepwiki.com/badrisnarayanan/antigravity-claude-proxy/6.3-prompt-caching-and-sessions) / [issue #292](https://github.com/badrisnarayanan/antigravity-claude-proxy/issues/292)
- [NoeFabris/opencode-antigravity-auth issue #512](https://github.com/NoeFabris/opencode-antigravity-auth/issues/512)
- [Wei-Shaw/sub2api](https://github.com/Wei-Shaw/sub2api) / [issue #935](https://github.com/Wei-Shaw/sub2api/issues/935) / [issue #620](https://github.com/Wei-Shaw/sub2api/issues/620) / [PR #3065](https://github.com/Wei-Shaw/sub2api/pull/3065)
- [diegosouzapw/OmniRoute issue #3696](https://github.com/diegosouzapw/OmniRoute/issues/3696) / [cortexkit/antigravity-auth issue #1](https://github.com/cortexkit/antigravity-auth/issues/1)
- [decolua/9router issue #1138](https://github.com/decolua/9router/issues/1138) / [PR #5272](https://github.com/decolua/9router/pull/5272)
- [can1357/oh-my-pi issue #11689](https://github.com/can1357/oh-my-pi/issues/11689)
- [keisksw/antigravity-output-analysis](https://github.com/keisksw/antigravity-output-analysis)
- [Antigravity Manager: Fix 429, 503 & Pool Errors](https://antigravitymanager.com/help/limits/)（更新 2026-11-01）
- [kRouter — Antigravity verify account 判别](https://krouter.kodelyth.com/blog/antigravity-verify-account)（2026-04-22 发布，2026-10-08 更新）
- [Antigravity-Manager 上下文压缩文档](https://lzw.me/docs/opencodedocs/zh/lbjlaq/Antigravity-Manager/advanced/context-compression/) / [429 轮换 FAQ](https://lzw.me/docs/opencodedocs/zh-tw/lbjlaq/Antigravity-Manager/faq/429-rotation/)
- [afengblog — Antigravity 256K context discrepancy（含反汇编归档）](https://afengblog.com/en/blog/antigravity-256k-context-discrepancy.html)
- [wikifita — gRPC Protocol 页（存疑来源）](https://wikifita.aleffita.dev/wiki/sdk-test/sdk-test-grpc-protocol) / [Protobuf Deep Analysis](https://wikifita.aleffita.dev/raw/sdk-test/sdk-test-protobuf.md)
- [PaperPlane.cc — Antigravity 反代与机房 IP](https://paperplane.cc/post/antigravity-proxy)
- [agentpedia — Agent terminated due to error 排障](https://agentpedia.codes/blog/antigravity-agent-terminated-error)
- [Snow System — Antigravity 模型对照](https://snowsystem.net/ai/antigravity-models-plan-comparison/)（2026-10-10）

### 9.4 跨项目（Claude Code / Codex / Cursor / Windsurf）与 ACP 标准

- [CLIProxyAPI issue #5443](https://github.com/router-for-me/CLIProxyAPI/issues/5443)（2026-09-03）/ [#7066](https://github.com/router-for-me/CLIProxyAPI/issues/7066)（2026-10-28）/ [PR #2489](https://github.com/router-for-me/CLIProxyAPI/pull/2489)（2026-04-02）/ [PR #2213](https://github.com/router-for-me/CLIProxyAPI/pull/2213)（2026-03-18）
- [AmazingAng/auth2api](https://github.com/AmazingAng/auth2api) / [1rgs/claude-code-proxy](https://github.com/1rgs/claude-code-proxy)
- [CIVITAS-John/codex-app-server-to-proxy](https://github.com/CIVITAS-John/codex-app-server-to-proxy) / [codex-app-server 兼容性注记](https://github.com/CIVITAS-John/codex-app-server-to-proxy/blob/main/docs/codex-app-server.md)（2026-10-21）
- [mehdic/codex-proxy](https://github.com/mehdic/codex-proxy) / [0oAstro/codex-openai-proxy](https://github.com/0oAstro/codex-openai-proxy) / [starhunt/star-cliproxy](https://github.com/starhunt/star-cliproxy) / [star-cliproxy Session Reuse](https://github.com/starhunt/star-cliproxy/blob/main/docs/client-integration-session-reuse.md) / [leeguooooo/agent-cli-to-api](https://github.com/leeguooooo/agent-cli-to-api)
- [7836246/cursor2api](https://github.com/7836246/cursor2api)（已 archive）/ [Xchat1/cursor2api-go](https://github.com/Xchat1/cursor2api-go) / [NGLSG/Cursor2API](https://github.com/NGLSG/Cursor2API)
- [dwgx/WindsurfAPI](https://github.com/dwgx/WindsurfAPI) / [guanxiaol/WindsurfPoolAPI](https://github.com/guanxiaol/WindsurfPoolAPI)
- [EvilFreelancer/acpbox — docs/api-mapping.md](https://github.com/EvilFreelancer/acpbox/blob/main/docs/api-mapping.md) / [vadim-vyb/acp-openai-gateway](https://github.com/vadim-vyb/acp-openai-gateway) / [pingu1m/acp-to-api](https://github.com/pingu1m/acp-to-api) / [chaojimct/cli-agent-gateway](https://github.com/chaojimct/cli-agent-gateway/blob/main/README.zh-CN.md) / [euri10/acp-llm-adapter](https://github.com/euri10/acp-llm-adapter) / [agentclientprotocol/claude-agent-acp](https://github.com/agentclientprotocol/claude-agent-acp) / [agentclientprotocol/codex-acp](https://github.com/agentclientprotocol/codex-acp)
- [Agent Client Protocol — Introduction](https://agentclientprotocol.com/get-started/introduction)（lastModified 2026-08-20）/ [Agents](https://agentclientprotocol.com/get-started/agents) / [Clients](https://agentclientprotocol.com/get-started/clients) / [Registry](https://agentclientprotocol.com/get-started/registry) / [Updates](https://agentclientprotocol.com/updates) / [v2 Draft 公告](https://agentclientprotocol.com/announcements/acp-v2-draft)（2026-07-20）
- [ACP v1 — Tool Calls](https://agentclientprotocol.com/protocol/v1/tool-calls) / [Agent Plan](https://agentclientprotocol.com/protocol/v1/agent-plan) / [Prompt Turn](https://agentclientprotocol.com/protocol/v1/prompt-turn) / [Session Modes](https://agentclientprotocol.com/protocol/v1/session-modes) / [Initialization](https://agentclientprotocol.com/protocol/v1/initialization) / [v2 Overview](https://agentclientprotocol.com/protocol/v2/overview)
- [ACP 协议详解](https://blog.cyeam.com/ai/2026/10/15/agent-client-protocol)（2026-10-15）
- [Zed — Bring Your Own Agent to Zed](https://zed.dev/blog/bring-your-own-agent-to-zed)（2025-08-27）/ [ACP Registry 发布](https://zed.dev/blog/acp-registry)（2026-01-28）/ [External Agents 文档](https://zed.dev/docs/ai/external-agents)
- [agentclientprotocol/agent-client-protocol](https://github.com/agentclientprotocol/agent-client-protocol)（4,596★，push 2026-11-07）
- [Gemini CLI — ACP Mode](https://geminicli.com/docs/cli/acp-mode/) / [PR #21171](https://github.com/google-gemini/gemini-cli/pull/21171)（2026-03-05）/ [zed Discussion #57221](https://github.com/zed-industries/zed/discussions/57221)（2026-05-20）
- [OpenAI — Codex App Server](https://learn.chatgpt.com/docs/app-server) / [Unlocking the Codex harness](https://openai.com/index/unlocking-the-codex-harness/)（2026-02-04）
- [Claude Code — Legal and compliance](https://code.claude.com/docs/en/legal-and-compliance) / [The Register](https://www.theregister.com/software/2026/02/20/anthropic-clarifies-ban-on-third-party-tool-access-to-claude/5014546)（2026-02-20）/ [VentureBeat](https://venturebeat.com/technology/anthropic-cracks-down-on-unauthorized-claude-usage-by-third-party-harnesses)（2026-01-09）/ [WinBuzzer](https://winbuzzer.com/2026/02/19/anthropic-bans-claude-subscription-oauth-in-third-party-apps-xcxwbn/)（2026-02-19）/ [ChainCatcher](https://www.chaincatcher.com/article/2300998)（2026-11-02，新规 11-12 生效）
- [OpenAI Tibo 表态转述](https://explainx.ai/blog/codex-usage-limits-sub2api-sign-in-chatgpt-august-2026)（2026-08-21）/ [ChainCatcher 转述](https://www.chaincatcher.com/en/article/2284385) / [第三方 Codex 客户端政策](https://aicoder.com/news/news-20260908-openai-codex-sign-in-policy)（2026-09-08）/ [多伙伴产品表态转述](https://manifest.build/blog/chatgpt-plus-tokens-third-party-harnesses/)（2026-09-29）/ [openai/codex issue #41664](https://github.com/openai/codex/issues/41664)
- [Cursor Terms of Service](https://cursor.com/terms-of-service)（2026-09-03）
- [CloudZero — Google Antigravity pricing 2026](https://www.cloudzero.com/blog/google-antigravity-pricing/)（2026-09-18）/ [TechRadar](https://www.techradar.com/pro/google-is-making-gemini-cli-users-switch-to-its-new-antigravity-2-0-so-what-will-it-mean-for-you)（2026-05-20）
- 社区（可信度已逐条标注，不作为唯一依据）：[Linux.do 2992939](https://linux.do/t/topic/2992939) / [2951333](https://linux.do/t/topic/2951333) / [1325597](https://linux.do/t/topic/1325597/19) / [3053739](https://linux.do/t/topic/3053739) / [3112874](https://linux.do/t/topic/3112874) / [3129568](https://linux.do/t/topic/3129568) / [3027535](https://linux.do/t/topic/3027535) / [1618883](https://linux.do/t/topic/1618883) / [1683105](https://linux.do/t/topic/1683105) / [1606203](https://linux.do/t/topic/1606203) / [2043354](https://linux.do/t/topic/2043354) / [3117531](https://linux.do/t/topic/3117531)；[V2EX 1223155](https://www.v2ex.com/t/1223155) / [1190155](https://www.v2ex.com/t/1190155) / [1193212](https://www.v2ex.com/t/1193212) / [1199224](https://www.v2ex.com/t/1199224) / [1203440](https://www.v2ex.com/t/1203440)

### 9.5 SAG 本仓库代码位置（本轮核对，非外部来源）

`docs/index.md:4-6`、`docs/spec/index.md`、`docs/spec/dispatch/spec.md:178`、`docs/spec/dispatch/changes/0004-upstream-4xx-non-retryable.md`、`docs/spec/circuit-breaker/spec.md`、`sag-providers/src/traits.rs:14-43`、`sag-providers/src/catalog.rs:14-34`、`sag-providers/src/lib.rs:19-55`、`sag-config/src/config.rs:1297` 起 `ApiConfig`、`sag-config/src/config.rs:982-1025`（`KeyStrategy` / `ProxyMode`）、`sag-config/src/config.rs:1351` 起 `ProviderConfig`、`sag-config/src/loader/load.rs:195`、`sag-config/src/loader/save.rs:1078-1088`、`sag-admin/src/admin_providers.rs:257,277,345,451`、`sag-admin/src/admin.rs:40-48`、`sag-router/src/routes.rs:205` 起、`sag-router/src/session.rs:11`。

---

## 附录：本文未覆盖的内容

- **Cursor `crsr_` 官方 API Key 的配额与 ToS 边界实证**（当前只有 ToS 条款文本，缺实际限流数据）。
- **Enterprise / Gemini Enterprise 通道是否允许第三方客户端访问 `v1internal`** —— 这是唯一可能合规的路径，值得单独立项调研。
- **假 429 的指纹全集**（CLIProxyAPI `sensitive-words` 默认列表 + PR #6280 / #7049 新增指纹），可作为内容过滤基线。
- **Antigravity-Manager `signature_cache.rs` / `thinking_store.rs` 的实际 TTL 与失效语义**（DeepWiki 9.1 只有概览，需读源码确认）。
