---
created: 2026-10-27
updated: 2026-10-27
type: 新增
---

# 0008 — `/v1/systemone` 决策接口接入（原样透传 + 深度 dispatch）

## 动机

SAG 此前只服务 chat IR 家族（`chat_completions` / `responses` / `anthropic_messages`）与图片生成（`image`）。TypeSafe System One 决策接口（`POST /v1/systemone`，被 LiteLLM / OpenRouter / Ollama 以同一形状兼容）是另一物种：请求 `{model, state, questions, assets?}`、响应 `{id?, model, answers, usage}`，其中 `state` / `questions` / `answers` 与逐题概率字段**无法映射到 chat IR**，任何 decode → encode 往返都会丢字段（尤其是各题型的概率分布与 `id`）。

若只做最小转发（一个 handler 直接打上游），这条路径会绕过重试退避、熔断、跨目标降级、`in_flight` 计数、`call_logs` / analytics 与会话注册表，行为与 chat / image 分裂，运维不可见。因此采用与 `dispatch_image` 同构的「无 IR 分发物種」：完整复用 resolve → retry → circuit → analytics 全套，但**请求与响应体逐字节语义不变**地透传。

## 需求变更

- **新增入口**：`POST /v1/systemone`（protected 树，Bearer 鉴权规则与其它 `/v1/*` 一致）。请求体 `{model, state, questions(noul/choice/score/box 等任意题型), assets?}`，响应体 `{id?, model, answers, usage}` 原样回给客户端。
- **新增行为约束（原样透传）**：`answers` / `id` / 各题型概率字段零解码、零改写；网关对 wire body 的唯一改动是把 `model` 改写成目标的 `real_model`（与 chat / image 一致）。`state` 的三种形态（字符串 / 对象 / 数组）与 `questions` 的动态键名原样保留。
- **新增行为约束（校验边界）**：网关只校验路由所需——JSON 语法错 → `400`；body 非对象、`model` 非字符串、`questions` 非对象 → `422 unprocessable`；`pipeline = "image"` 的路由 → `422`。**不校验** `question.type` 与 `criteria` 形状：非法值由上游拒绝（请求体容忍未知值，严格性只保留给类型化配置枚举）。
- **新增行为约束（presence 即能力）**：SystemOne 没有 chat IR、也没有 Translated lane，因此**目标 provider 必须声明 `apis.systemone`**。缺该节点的目标在解析阶段被 `filter_systemone_targets` 丢弃，`errors` 里记 `"{provider}/{model}: lacks capability systemone"`，全部被丢弃时沿用 all-disabled / fallback-exhausted 的 `502` 信封并原样带出该原因。
- **连带行为变化（chat 路由的 `apis` presence 预检）**：本改动放开了「provider 可以只有 `apis.systemone`、没有任何 chat-IR 节点」这个配置形状，破坏了 chat 侧「`used_protocol` 候集恒非空」的隐含不变量。`filter_capable_targets` 因此新增一道无条件 presence 检查：目标实际使用的协议必须是 provider 已声明的 `apis` 节点，缺则**在能力比对之前丢弃**并记 `"{provider}/{model}: lacks capability {used}"`。修复前这种目标会一路走到 `provider.chat()`，被 `catalog` 以 `BadRequest` 拒绝，再被 `is_client_caused_4xx` 误归因为客户端错误（400 终态、不 failover、不落 `target_attempts`）。未知 provider 名不在这一层丢弃（留给 `resolve_provider` 记 `not found`，保留 failover）。
- **新增行为约束（错误信封）**：统一走 SAG 的 `{ "error": { "code", "message" } }`（OpenAI 信封），不采用 TypeSafe 原生 `{detail}`。上游以 HTTP 200 投递的 `error` 对象按上游错误升格——rate-limit 特征 → `429`，其它 → `502`（与 chat / responses 的 `responses_native_inner` 惯例一致）。
- **新增行为约束（analytics）**：`usage.input_tokens/output_tokens` → `prompt_tokens/completion_tokens`；缺失或形状不合法一律留 NULL，**不编造 0**。每次请求照常计入 `in_flight` 计数、写一次会话注册表 fact、按目标产生 `TargetAttemptFact` 并落 `call_logs` 终态；成功 / 客户端 4xx / 全耗尽分别对应成功行、客户端错误行与 `502` 模型失败行。
- **新增行为约束（无流式）**：协议没有 `stream` 字段，恒为一次性 JSON 响应，取消检测只走非流式 `DispatchCancelGuard` 路径。
- **修改配置语义（`fallback` 只覆盖 chat IR）**：`fallback` 的取值域与「是否需要 fallback」的判定只统计 chat IR 三协议（`chat_completions` / `responses` / `anthropic_messages`）。`apis.systemone` 存在但只有它 + 单个 chat 协议时仍不需要 `fallback`；显式 `fallback = "systemone"` 在配置校验阶段**直接报错**（`fallback='systemone' is not supported`）——静默回退会把决策协议当成 chat 翻译目标，比报错更危险。
- **修改派生视图**：`GET /v1/models` 的 `ingress_protocols` 由 `ApiKind::protocols()` 派生，自动包含 `systemone`；`POST /api/providers/probe-protocols` 明确不支持该协议（`minimal_request` 返回 `None` → 调用方转 `400`），因为决策协议的请求形状与 chat 无关，无法用 chat 探针探测。
- **配置示例**：`gateway.example.toml` 增加 `[providers.<name>.apis.systemone]` 注释示例，说明 `base_url` 会拼 `/systemone`、该节点不计入 `fallback` 需求且不能作 `fallback` 值。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-core/src/protocol.rs`](../../../../sag-core/src/protocol.rs) | 新增 `ApiKind::SystemOne` 与 `IngressProtocol::SystemOne`（`#[serde(rename = "systemone")]`，协议 key 是单词而非 `snake_case`）；`PROTOCOLS` 扩到 4、`protocol_path` → `"systemone"`，同步枚举 / lane 矩阵测试 |
| [`sag-core/src/api/systemone.rs`](../../../../sag-core/src/api/systemone.rs) | 新增 `SystemOneRequest`（`state: Value` 三态、`questions: BTreeMap<String, Question>`、可选 `assets: BTreeMap<String, Value>`）与 `Question`（`type` + 可选 `instructions` / `criteria`，均为 `Value`），两者都带 `#[serde(flatten)] extra`、禁 `deny_unknown_fields`；`extract_usage` 提供 usage 读取 |
| [`sag-core/src/api/mod.rs`](../../../../sag-core/src/api/mod.rs) | 导出新 DTO 与 `extract_usage` |
| [`sag-providers/src/traits.rs`](../../../../sag-providers/src/traits.rs) | `LlmProvider` 新增 `systemone(body, target_extra, timeout)` 原生扩展方法，默认实现返回 `BadRequest`（不支持即拒绝，仿 `anthropic_messages`） |
| [`sag-providers/src/base.rs`](../../../../sag-providers/src/base.rs) | 新增共享 `systemone_inner`：`protocol_url(SystemOne)` + `effective_timeout` + `send_json`（鉴权、body overrides、strip fields、target extra 三级覆盖）+ 200 内嵌 `error` 对象升格 `429` / `502` |
| [`sag-providers/src/openai.rs`](../../../../sag-providers/src/openai.rs) | 覆盖 `systemone`，委派 `base.systemone_inner` |
| [`sag-providers/src/anthropic.rs`](../../../../sag-providers/src/anthropic.rs) | 同上 |
| [`sag-providers/src/catalog.rs`](../../../../sag-providers/src/catalog.rs) | 按 `base.supports(ApiKind::SystemOne)` 分发 `systemone`；未声明该节点时返回 `BadRequest` |
| [`sag-dispatch/src/dispatch/systemone.rs`](../../../../sag-dispatch/src/dispatch/systemone.rs) | 新增 `dispatch_systemone`：in-flight 守卫 → 会话 fact → 非流式 cancel guard → 配置一次性快照 → 逐目标（熔断闸门 + provider resolve + `run_with_retries_outcome`）→ 终局 analytics；`build_systemone_body` 从 carrier 的 `extra` 重建 wire body 并只改 `model`；`usage_from_body` 把 usage 映射到 analytics（缺失留 NULL） |
| [`sag-dispatch/src/dispatch/resolve.rs`](../../../../sag-dispatch/src/dispatch/resolve.rs) | `resolve_route_with_ingress` 接受 `IngressProtocol::SystemOne`；新增 `filter_systemone_targets`（`apis.systemone` presence 即能力，丢弃原因与 chat 预检同款措辞） |
| [`sag-dispatch/src/dispatch/capability.rs`](../../../../sag-dispatch/src/dispatch/capability.rs) | SystemOne 不声明任何 chat 能力位（streaming / reasoning / tools / images / documents 全 false），且永不作为 Translated lane 的 fallback 候选（`used_protocol` 过滤）；`filter_capable_targets` 新增无条件 presence 预检（缺 used protocol 的 `apis` 节点 → pre-drop `lacks capability {used}`） |
| [`sag-dispatch/src/dispatch/chat/empty.rs`](../../../../sag-dispatch/src/dispatch/chat/empty.rs) | `classify_raw_frame` 补 `ApiKind::SystemOne => RawVerdict::Content`（非流式，永不到达 chat 空响应检测） |
| [`sag-dispatch/src/dispatch/mod.rs`](../../../../sag-dispatch/src/dispatch/mod.rs) | 注册 `systemone` 模块并 re-export `dispatch_systemone` |
| [`sag-dispatch/src/lib.rs`](../../../../sag-dispatch/src/lib.rs) | 对外 re-export `dispatch_systemone` |
| [`sag-router/src/routes.rs`](../../../../sag-router/src/routes.rs) | protected 树新增 `POST /v1/systemone`；handler 做「形状预检先于 carrier 构造」（`model` 字符串 + `questions` 对象 → 422）、`pipeline == Image` → 422、以 `extra` 承载原始 body 的 carrier、`IngressProtocol::SystemOne` 路由、`ir: None` 与恒非流式 |
| [`sag-config/src/config.rs`](../../../../sag-config/src/config.rs) | `fallback` 需求判定与建议值只统计 chat IR 三协议（新增 `is_chat_ir_protocol`）；`fallback = "systemone"` 在校验期报错 |
| [`sag-admin/src/probe_request.rs`](../../../../sag-admin/src/probe_request.rs) | `minimal_request` 对 `ApiKind::SystemOne` 显式返回 `None`（探测不支持，调用方转 400，不 panic） |
| [`gateway.example.toml`](../../../../gateway.example.toml) | 新增 `apis.systemone` 注释示例，说明 `/systemone` 路径拼接与 `fallback` 语义 |
| [`sag-router/tests/systemone_ingress.rs`](../../../../sag-router/tests/systemone_ingress.rs) | 新增入口级集成测试（8 例） |

## 测试

- 入口集成（`sag-router/tests/systemone_ingress.rs`，8 例）：请求 / 响应逐字透传（`answers` / `id` / 概率字段 / 未知字段零丢失，`model` 被改写成 `real_model`）、缺 Bearer → 401、形状错（非对象 body / `model` 非字符串 / `questions` 非对象）→ 422 且早于派发、JSON 语法错 → 400、未知模型 → 404、provider 无 `apis.systemone` 被过滤 → 502 且原因是 `lacks capability systemone`、image 路由 → 422、`GET /v1/models` 的 `ingress_protocols` 含 `systemone`。
- 分发单元（`sag-dispatch/src/dispatch/systemone.rs`）：成功透传且 usage 缺失留 NULL、失败后降级到下一目标、全耗尽 → 502 模型失败信封、熔断打开的目标被跳过、`build_systemone_body` 保留 `extra` 逐字并只改写 `model`、provider 解析失败时半开探针槽同步释放（同 `chat/tests.rs` 的同名回归场景）。
- 能力过滤单元（`sag-dispatch/src/dispatch/capability.rs`）：presence 预检——只有 `apis.systemone` 的 provider 作为 chat 目标被 pre-drop（`lacks capability chat_completions`），未知 provider 不被这一层丢弃。
- 解析单元（`sag-dispatch/src/dispatch/resolve.rs`）：`filter_systemone_targets` 只保留声明了该 API 节点的 provider，其余记入 `errors`。
- 供应商单元（`sag-providers`）：`protocol_url` 拼接 `/systemone`、缺 API 节点 → `BadRequest`、API 节点超时优先于调用方超时、原始 JSON 原样返回、200 内嵌 `error` → 502、200 内嵌 rate-limit → 429、body 覆盖顺序（API 节点 overrides → target extra）；`catalog` 侧有 / 无节点两条路径。
- 协议与配置单元：`ApiKind` / `IngressProtocol` 的 serde 往返（`"systemone"`，`"system_one"` 非法）、`protocol_path` 与 protocols 矩阵；`chat_completions + systemone` 不需要 `fallback`、多 chat 协议 + systemone 仍要求 `fallback` 且建议值不取 systemone、`fallback = "systemone"` 被拒；`minimal_request(SystemOne)` 为 `None`。
