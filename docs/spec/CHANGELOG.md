---
created: 2026-06-03
updated: 2026-10-28
---

# 变更时间线

按时间倒序列出所有模块的变更记录。

- **2026-10-28** · **配置**：[0008 dev 轨 XDG 数据隔离（`.dev/xdg/`）](config/changes/0008-dev-track-xdg-isolation.md) — 开发轨（`just dev` / `just run` / `just run-release`）此前不注入 XDG，analytics 库、滚动日志、配置 fallback 全落在生产路径（`~/.local/share/sag`、`~/.local/state/sag`、`~/.config/sag`）；新增 `scripts/dev_env.py` 提供 `dev_env()`（纯函数覆盖三个 `XDG_*` 为项目内绝对路径 `.dev/xdg/`）与 `ensure_dev_dirs()`（预建 `config/sag`、`data/sag`、`state/sag/logs`，SQLite 不自建父目录）加 CLI `python3 scripts/dev_env.py -- <cmd...>`（建目录后 `execvpe`），`scripts/dev.py` 的后端（`watchexec → cargo run`）复用同一注入，`justfile` 的 `run` / `run-release` 改经 launcher 启动并新增 `dev-clean`（`rm -rf .dev`，`clean` 只清构建产物、不碰隔离数据）；dev 轨落点变为库 `.dev/xdg/data/sag/analytics.db` / 日志（仅非 TTY）`.dev/xdg/state/sag/logs/` / 配置 fallback `.dev/xdg/config/sag/gateway.toml`，配置优先级（显式参数 > `./gateway.toml` > XDG）不变，唯一有意 break 是此前依赖 `~/.config/sag/gateway.toml` fallback 者需复制到项目根或 `.dev/xdg/config/sag/`，逃生口为绕过 launcher 直接 `cargo run`（无注入，可再自行导出 `XDG_*` 指到任意位置；`just dev` / `just run` / `just run-release` 无条件覆盖这三个变量，预先导出无效）；零 Rust 改动（`paths.rs` 纯 XDG 逻辑不动，注入值必须绝对路径否则静默回退 `$HOME`），`.dev/` 与 `.test-env/` 互不感知；`AGENTS.md`（两轨表新增 dev 轨 SQLite / 日志 / 配置 / 清理行 + 双注入小节 + 边界小节，修正两处过时路径）与 `docs/guide/{configuration,deployment,analytics}.md` 同步

- **2026-10-27** · **调度**：[0008 SystemOne 决策接口接入（原样透传 + 深度 dispatch）](dispatch/changes/0008-systemone-ingress.md) — 新增 `POST /v1/systemone` 入口（protected 树，Bearer 同其它 `/v1/*`）：该协议（`{model, state, questions, assets?}` → `{id?, model, answers, usage}`）的 `state` / `questions` / `answers` 与逐题概率字段无法映射到 chat IR，故作为与 `image` 同构的「无 IR 分发物种」处理——`dispatch_systemone` 镜像 `dispatch_image` 的骨架（in-flight 计数 → 会话 fact → 非流式 `DispatchCancelGuard` → 一次性配置快照 → 逐目标熔断闸门 + `run_with_retries_outcome` → 成功 / 客户端 4xx / 全耗尽 502 三路终局），但请求与响应体**逐字透传**（wire body 由 `build_systemone_body` 从 carrier `ChatCompletionRequest.extra` 重建并只把 `model` 改写为 `real_model`），`usage.input_tokens/output_tokens` → analytics 的 `prompt/completion_tokens`（缺失留 NULL，不编造 0）；校验边界只到路由所需（非对象 body / `model` 非字符串 / `questions` 非对象 → 422，形状预检早于派发；JSON 语法错 400；`pipeline="image"` 路由 422），`question.type` / `criteria` 形状交给上游；能力闸门是 `apis.systemone` **presence 即能力**（`filter_systemone_targets` 丢弃无该节点的目标并记 `lacks capability systemone`，全丢走 `502` all-disabled 信封），`capability.rs` 中它不声明任何 chat 能力位且永不作 Translated lane 候选；错误统一 OpenAI 信封（上游 200 内嵌 `error` 按 rate-limit 特征升格 429 / 其余 502）；协议无 `stream` 字段故恒非流式；`sag-config` 的 `fallback` 需求判定与取值域收窄为 chat IR 三协议（`is_chat_ir_protocol`），显式 `fallback = "systemone"` 在校验期报错（静默回退会把决策协议当 chat 翻译目标）；`sag-core` 新增 `ApiKind::SystemOne` / `IngressProtocol::SystemOne`（协议 key 为单词 `systemone`，`PROTOCOLS` 长度 4）与 `api/systemone.rs` DTO（均带 `extra`、禁 `deny_unknown_fields`），`sag-providers` 新增 `LlmProvider::systemone` 原生扩展方法（默认 `BadRequest`）与 `base::systemone_inner`（`/systemone` 路径拼接、API 节点超时优先、body 三段覆盖、200 内嵌 error 升格），`sag-admin` 的 `minimal_request` 对该协议返回 `None`（探针不支持→400），`GET /v1/models` 的 `ingress_protocols` 自动包含它；测试 +8 入口集成（`sag-router/tests/systemone_ingress.rs`）+ 分发 / 解析 / 供应商 / 协议 / 配置单元
- **2026-10-26** · **管理 API**：[0003 per-route 在途计数与 `GET /api/inflight` SSE 端点](admin/changes/0003-per-route-in-flight-counters-sse.md) — `sag-state` 新增 `InFlightRegistry`（`std::sync::Mutex<HashMap<route_name, u64>>` + `watch::Sender<InFlightSnapshot>`，`seq` 与计数同锁递增）与 `InFlightGuard`（RAII，`Drop` 内减计数、归零删键、`send_replace` 发布、未知键只 `warn!` 不 panic）；`AppState.in_flight` 与 `DispatchRuntime.in_flight` 共享同一 `Arc`（`main.rs` 装配 + `with_shared_state` 增参），`dispatch_chat` / `dispatch_image` 入口 acquire（早于隐私过滤与重试调度，退避期间仍在途），流式路径把 guard 移交 `StreamState._in_flight`（三个协议出口）故客户端断连即释放；新端点 `GET /api/inflight` 事件名 `inflight`，载荷为全量快照 `{"seq","total","routes":[{"model_name","count"}]}`（`count` 降序、同数按名字升序、`total` = Σ`count`、零计数键删除），连接即发当前快照、此后每次变化推全量快照（幂等，丢帧不失步），空闲 15s `: ping` 心跳，响应带 `no-cache, no-transform` + `X-Accel-Buffering: no`（不加逐跳的 `Connection`）；鉴权同其它 `/api/*`（Bearer-only，`x-api-key` 不放行，集成测试钉住），端点自身不计数
- **2026-10-26** · **UI**：[0013 路由页实时在途请求徽章行（SSE）](ui/changes/0013-routes-live-inflight-badges.md) — 路由表格上方新增 `RouteActivityBar`：`◉ 处理中 N` + 标题内联 Info Tooltip，右侧仅列 `count > 0` 的 route 徽章（等宽 `model_name` 截断 + `×count` + 脉冲绿点，按 `count` 降序），全零时「空闲 — 无正在处理的请求」，首帧到达前整行不渲染，断线保留最后快照并标 `stale`（徽章置灰 + 「连接断开，重连中…」，**计数不清零**）；因 `EventSource` 带不了 auth 头，新增 `lib/sse.ts` 用 `fetch` + `ReadableStream` 自读 SSE（跨 chunk 分帧、`: ping` 注释丢弃、指数退避重连 1s→30s、`stop()` abort + `running` 闸保证零投递、非 2xx 抛 `SseHttpError` 且 401/403 走 `handle401()` 不重连；`Accept: text/event-stream` 必须写在 spread 之后，否则被 axios 默认 `application/json` 盖掉），`lib/inflight.ts` 容错解析 + 整体替换快照 + `activeRouteCounts` 过滤，`stores/inflight.ts` 生命周期绑 `RoutesPage` 的 mount/unmount；i18n 各 +6 key（607），单测覆盖分帧器与连接循环

- **2026-10-26** · **UI**：[0012 会话 pin 图标 hover 预览与活跃 pin 置顶/置灰](ui/changes/0012-session-pin-tooltip-and-active-pin-ordering.md) — 把「看」与「改」拆开：钥匙 icon hover 出只读 Tooltip 预览（`side="left"`，标题 + 条数 + 三态 + pin 列表，无任何清除入口，浮层打开时不显示 tooltip（受控 `:open`，不用 `disabled` 以避开 reka 的 hover 卡死）），点击仍进带清除动作的 Popover；两份列表共用 `sortedRows`，活跃 pin（`expires_in_secs === null || > 0`）顶置、过期残留置灰（Tooltip `opacity-70` / 浮层 `text-muted-foreground opacity-60`）并显示「已过期」（复用 `common.expired`，不新增 i18n key）；hover 拉取受 `HOVER_REFRESH_MS = 3000` 节流且本行 scope 有在途请求时短路、浮层打开总是重拉；session-sticky 行无 active target 时不再渲染 `-` 占位，只留钥匙 icon（其它策略仍为 `-`）；存活性判定、排序与按 scope 取错误 / 在途标志抽为 `lib/sessionAffinity.ts` 的 `isPinActive` / `sortPins` / `errorForModel` / `loadingForModel`（均 `Object.hasOwn` 防原型链键）并补单测，共享 store 的 `errors` / `loading` / `generation` 全部按 `params.model ?? ""` 记账（启动 / 重试不清旧错误，陈旧响应只跳过自己 scope，`clearError` 前置 `Object.hasOwn` 守卫），新增 store 行为测试与 `scripts/alias-loader.mjs` 测试基建（`test` 脚本加 `--import`）

- **2026-10-26** · **隐私过滤**：[0003 隐私边界与 `session_registry` PII 例外](privacy/changes/0003-pii-carve-out-boundary.md) — 只补文档边界、不改行为：隐私掩码的承诺范围明确为**上游出口**（发往 provider 的请求内容 + 回客户端的响应还原），本地管理面的 `session_registry`（analytics 0009）是经用户裁定并文档化的唯一 PII 例外——`conversation_id`（≤128）与首条消息预览（≤160）存**掩码前**的原始值，仅供日志页会话信息卡经 `GET /api/logs?session=` 读取，独立保留期 `[analytics] session_retention_days`（默认 7 天、`0` = 永久），不参与 `call_logs` 轮转；明确开启 `mask` 不会把 `[[KIND_h12]]` 占位符写进注册表，原始值仍不得进入 `CallLog` / 会话亲和 map / admin 快照（`sag_core::session.rs` 不变式的唯一例外）；requirements 的「检测范围」补边界、「事件日志」补唯一例外、「不包含」补 carve-out 项、「与其它模块的交互 → 分析」补例外段落与交叉链接，spec 新增「掩码边界是上游出口」决策节与一条不变式
- **2026-10-26** · **分析**：[0009 会话注册表 session_registry 与 /api/logs 的 session 信封](analytics/changes/0009-session-registry-and-logs-session-envelope.md) — 新增独立表 `session_registry`（`session_key` 主键 + `session_source` / `conversation_id`≤128 / `first_message_preview`≤160 / `first_seen` / `last_seen`，`idx_session_registry_last_seen`）为哈希提供可读的会话身份：合并语义是**最早请求者胜**（`first_seen` 取 MIN、`last_seen` 取 MAX，opener 字段仅当新到 fact 的 `first_seen` 更早时覆盖，赢家由请求时间戳而非写入顺序决定，因为 collector 队列满时的直写兜底绕过 FIFO）；写入点 = chat 派发 / image 派发 / `__no_route__` / `__all_disabled__` 四处每请求一次（绝不进终态），`CallLogCollector::log_session` 与 `log` 共用同一 MPSC 且队列满/关时直写不丢弃；`GET /api/logs` 信封向后兼容地新增可选 `session` 三态字段（未带参数无键 / 未知键 `null` / 命中对象，查询失败降级 `null`）；`[analytics] session_retention_days` 默认 7、`0` = 永久保留、**超期直接删行不归档**、不参与 `call_logs` 轮转（挂在每轮维护末尾，天数按 3650 钳制，首轮与变更时输出 `session registry prune armed`）；隐私边界（用户裁定）：脱敏仅作用于发往上游的请求内容，注册表存原始 `conversation_id` 与原始（未脱敏）首条消息预览，是 `SessionKey` 「不离开即不哈希」不变式的唯一文档化例外
- **2026-10-26** · **UI**：[0011 日志页会话信息卡](ui/changes/0011-logs-session-info-card.md) — 应用会话过滤时在筛选栏与表格之间渲染 `SessionInfoCard`：会话键（前 8 位 + 全量 `title`）、来源徽标（首末出现时间在 tooltip）、会话 ID、首条消息预览四项，`session === null` 时降级为「只有会话键 + 降级文案」而不消失；新增 `lib/sessionInfo.ts` 展示纯函数（来源收窄 / `—` 占位 / preview 折叠截断 / 时间格式化）与单测，store 新增 `logsSession` + `logsSessionKey` 一对（只在 `logsSessionKey === logsFilterSession` 时展示记录，切换过滤不会把上一段对话的信息挂在新哈希下，失败分支清空），i18n 各 +12 key
- **2026-10-26** · **UI**：[0010 日志页会话键过滤项内联显示](ui/changes/0010-logs-session-filter-inline-control.md) — 删掉筛选栏下方独占一行的「已应用筛选」`Badge` chip，把会话键过滤收进筛选栏本身：`LogsPage` 用复刻 `SearchSelect` Trigger 样式的容器（`flex-1`、无 `xl:` 限定，与同栏 Provider / Model 同宽同高、全断点常显）承载空态内嵌输入框（回车应用）与有值态 `font-mono` 截断文本（`title` 全量，点击/聚焦切回输入态），清除按钮内联在控件右端（`v-if` 有值才渲染，清空后 `nextTick` 聚焦输入框）；Session 列点击仍写同一 `logsFilterSession`，表格高度不再被多压一行；删除 i18n `logs.filter.sessionChip`（en/zh 同步），`just verify-ui` 全绿
- **2026-10-26** · **供应商**：[0008 Responses / Anthropic 请求响应体 typed 化（无损透传）](providers/changes/0008-typed-wire-dtos-responses-anthropic.md) — 两套原生协议的 decode / encode 从 `serde_json::Value` 切到 typed DTO，与 chat 路径对齐：新增 `sag_core::api::{anthropic,responses}`（Anthropic 75 / Responses 145 个公开类型，按官方 schema 全变体建模：`input` 30 变体、`tools` 16 种、`Response` 33 字段、content block 15 种、`tool_choice` 4 变体、`thinking` 3 变体），每个结构体带 `#[serde(flatten)] extra`、无 `deny_unknown_fields`，多态联合用「untagged 序列化 + 按 `type` tag 分派反序列化 + `Unknown(Value)` 兜底」实现无损透传；已弃用字段（`truncation` / `user` / `prompt_cache_retention` / `reasoning.generate_summary` / `tools[].connector_id`）不建模、经 `extra` 透传，例外是 Anthropic `temperature` / `top_p` / `top_k`（协议翻译链的活跃采样输入，保留 typed + 弃用注释）；两个 codec 删除顶层 `KNOWN` 白名单（由 `extra` 承载），`ir.raw` 仍双向填充（含 Responses `previous_response_id` / `store` 状态透传），`reject_unsupported(_mode)` 保持 `&Value` 与 422 消息原文，SSE 流式不变；`sag-dispatch` 新增 `body_to_value` 边界序列化与两个 lenient decode helper（typed 解析失败时读信封 + 逐块/逐项 `Unknown`，且降级无损：`stop_details` / `container` 还原、整包 body 进 `extra`，不把非规范上游体变成空响应），usage 解析（`responses_usage` / `anthropic_usage`）改为按字段降级、单个坏子对象不归零计数，`parse_faithful` round-trip 守护在 responses / anthropic 两侧同源（`ToolUseBlock.input` / `ServerToolUseBlock.input` 用 `Option` 表达三态：缺 `input` 不长键、显式 `"input": {}` 逐字回写），input message 的 `role` 缺失或非字符串都按 `"user"` 兜底，`sag-router` 两个 handler 接线并保持「语义校验先于 typed 解析」的 422 顺序；测试 +27（`sag-core` DTO）+ `sag-providers` / `sag-dispatch` 透传与降级回归（含评审返工 10 例）+ `sag-router` 3 例 422 顺序守护

- **2026-10-25** · **隐私过滤**：[0002 隐私过滤 Minor 项收敛](privacy/changes/0002-minor-fixes.md) — round2 评审 6 条 Minor 收敛：`dispatch_chat` 合并 `mask_request` 与 `mask_ir` 两份 `PlaceholderMap`（`PlaceholderMap::extend`），IR-only 字段（未知顶层键、`Reasoning` thinking、`ToolResult`）的占位符现在也能还原，不再把裸 `[[KIND_h12]]` 交给客户端；`placeholder_len` 改按 `kind.to_uppercase()` 的字节长度计算（非 ASCII kind 精确）；`RestoreWriter` 的帧终止符集扩为 `\n\n` / `\r\n\r\n` 且整体匹配（半截分隔符不提前切帧，CRLF 上游不再整条流不还原）；15 位旧式身份证新增出生日期校验（第 7-12 位按 `19YY` + 闰年规则）并把 span 撞车的排序键改为 `(start 升序, end desc, check_digit 降序, order 升序)`，使 Luhn 合法的 15 位卡号压过无校验位的旧式身份证；流式尾帧先下发、`call_logs` / analytics 终态记账后收（`terminal_pending` 延迟一拍），上游 `Error` 后无显式 `End` 的路径同样先 flush 尾缓冲；测试 57（`sag-privacy`，+8）/ 277（`sag-dispatch`，+3）
- **2026-10-25** · **隐私过滤**：[0001 隐私过滤初始化](privacy/changes/0001-initial.md) — 新增 `sag-privacy` crate（规则引擎：`email` / `phone` / `id_card_cn` / `bank_card` / `iban` / `api_key` / `private_key` 七个内置检测器 + `custom_regexes` / `custom_words` / `allowlist`，正则 → 邻接字符 → 校验和三级判定，候选按起点升序、同起点取最长贪心解析），`mask` 模式以 `[[KIND_12hex]]`（`sha256("{kind}:{text}:{salt}")` 前 12 hex）确定性脱敏并在响应侧还原（非流式递归字符串值、流式 `RestoreWriter` 按 SSE 帧还原 + 冲尾，未命中帧逐字节透传），`warn` / `block` 两模式与引擎编译失败 fail-closed `503`；`[privacy]` 配置段默认 `enabled = false`（关闭时零改写），自定义正则在配置校验阶段无条件编译；`sag-dispatch` 挂点位于 `dispatch_chat` 请求入口（含 masked IR 重建）与非流式 5 个出口 + 流式 writer；block / 503 早退发生在 analytics 之前，因此不写 `call_logs`
- **2026-10-25** · **UI**：[0009 UI 对接 session-affinity 观测与会话列/校验提示](ui/changes/0009-session-affinity-pins-and-log-session-column.md) — 补上 `session_sticky` 的运维面：RouteList 的 Active Target 单元格为 session-sticky 行额外挂 `SessionAffinityPopover`（按 `model` 拉 `GET /api/session-affinity`，展示 provider/real_model、key 前 8 位、source、age、剩余 TTL，`ttl_secs = 0` 显示「永不过期」；逐条 `DELETE {key}` + 按模型两步确认 `DELETE {model}`，成功后刷新浮层并回调列表重拉 active targets），新增 `stores/sessionAffinity.ts`（单调 `generation` 竞态 token，慢的旧请求不得写 entries/loading）与纯函数 `lib/sessionAffinity.ts`；LogsPage 在 Client 列后新增 Session 列（前 8 位 + 全量 `title`，点击复用 `logsFilterSession` 套用会话筛选，表格 14 → 15 列）；两个路由表单新增两条与后端 `validate_routes` 等价的 zod refine（通配 `*` × session_sticky → `strategy` 字段，session_sticky × `pipeline = "image"` → `pipeline` 字段），自 0008 的「组合规则全留给后端」反案两条但其余仍归后端；i18n 各 +18 key（589）；评审 0 Blocker / 0 Major / 4 Minor（共享 store 边角竞态被 reka-ui 关闭语义阻止、两表单错误渲染风格不一致、通配未 `trim`、按 key 删除后跨空间瞬时陈旧），均不阻塞、未修
- **2026-10-25** · **UI**：[0008 UI 补齐 `session_sticky` 路由策略](ui/changes/0008-session-sticky-route-strategy.md) — 后端自 v0.8.0 已有第 4 个策略 `session_sticky`，前端 `Strategy` 联合类型与两个表单的 Zod 枚举却硬编码三值，导致该策略的路由在列表无 badge、编辑过不了校验、新建也不可选；本次补齐 `types/api.ts` 的联合类型与 `RouteConfig` / `CreateRoutePayload` / `UpdateRoutePayload` 的 `session_ttl_secs` / `session_max_entries`，两个表单加第 4 个 ToggleGroup 选项 + 策略说明行 + 两个条件渲染的数字输入（TTL `^\d+$`、容量 `^[1-9]\d*$`，仅 session-sticky 下校验，留空 = 省略该字段），`RouteList` 补 badge 配色与文案，i18n 各 +9 key（571）；编辑遵循 update DTO「省略=保留」，因此不做清空交互（切走后旧值仍留在服务端）；通配路由 × session-sticky、session-sticky + image 管线两类组合不在前端复现，由后端 `validate_routes` 拒绝并以 400 原文展示（取舍已写入 spec 关键决策）
- **2026-10-25** · **routing**：[0006 拒绝 `session_sticky` + `pipeline = "image"`，admin 侧补齐 prune](routing/changes/0006-session-sticky-image-rejection-and-admin-prune.md) — image 管线丢弃 `session_writeback`，该组合永远不会记录 pin，改为在 `validate_routes` fail-fast（替代策略提示），避免静默退化为纯加权随机；admin `prune_stale_routing_state` 补 `session_affinity.retain_sticky_routes`，与 dispatch 侧同一套 keep 规则（策略切走 / 路由删除 / 精确路由遮蔽通配符 → 丢弃 pin），并新增 admin prune 集成测试；同时把 `RouteCounters` 从 `DispatchRuntime` 提升为 `AppState` 共享句柄（`sag-state` 新增 `RouteCounters` 类型与 `retain_resolving_route_keys` 唯一清理实现、`DispatchRuntime::with_shared_state` 增参、`sag-gateway` 装配同一 `Arc`、`sag-router` re-export），admin 与 dispatch 两侧的 `fill_first_orders` / `route_counters` 清理从此同源——删除 `round_robin` 路由后计数器立即被清，不再等下一次请求观察到 `routes_version` 变化。**评审返工又并入**：通配路由（`model_name = "*"`）× `session_sticky` 改为 validate 直接拒绝（space 键域随请求模型无界）；admin 路由 CRUD 补齐 `session_ttl_secs` / `session_max_entries` 字段表面（create 透传、update「不传=保留」、GET 未设时省略）；`DELETE /api/session-affinity {"all":true}` 的 `deleted` 计数改为单写锁原子
- **2026-10-25** · **routing**：[0005 会话墓碑集合上限与全局 epoch 递增](routing/changes/0005-session-affinity-tombstone-budget.md) — generation 墓碑此前只增不减（键域客户端可控 + LRU 驱逐每个键一条），改为每 space `max(session_max_entries, 1024)` 封顶，溢出时整批清空并在同一临界区递增全局 epoch（只清墓碑会因 ABA 放行陈旧写回）；`clear` / `clear_model` / `clear_all` / `retain_sticky_routes` / LRU 驱逐统一走 `bump_generation` 并回写局部 epoch
- **2026-10-24** · **UI**：[0006 路由 Target 改为浮窗搜索多选](ui/changes/0006-target-picker-dialog.md) — 新增 `TargetPickerDialog` + 纯逻辑 `lib/targetPicker.ts`：选择单位是 provider+model 对（同模型两 provider 是两个选项），已有行预勾选、取消勾选即删除、新勾选追加默认值行、未动过的行保留 priority/retries/extra；搜索同时匹配 provider 名与模型名，禁用 provider / orphan 行有标记，键盘 ↑↓/Enter/空格/Esc 与 `aria-activedescendant` 维护；store 的 `providerModelsLoading` 改为在途计数派生（并发探测不会被提前清零）并新增按 provider 的错误表，`loadProviderModels` 改 async；**评审返工定稿的实态**：空模型 provider 保留分组 + 空态提示（`routes.targetPicker.providerNoModels`）、Enter/空格均可切换勾选（焦点在搜索框），`/api/providers/{name}/models` 只回显配置不探上游
- **2026-10-23** · **架构**：[0004 内嵌 UI 静态服务（收尾）](architecture/changes/0004-ui-static-cache-and-fallback.md) — 子资源探针判定由「末段含点」收紧为 26 项扩展名白名单（`/providers/v1.2` 由 404 改回 shell，`/gone.js` 仍 404）、sourcemap 排除改用 fixture 非空转验证、release profile 开 `lto="thin"`/`codegen-units=1`/`strip="symbols"`（本机 25.7 MB → 21.3 MB；当时 `codegen-units=1` 被 `.cargo/config.toml` 的 `codegen-units=16` 覆盖，本地与 CI 均未生效，体积收益全部来自 `strip`，覆盖行已在返修中删除）、`chart.js` 拆出 `vendor-chart`（`AnalyticsPage` 258 kB → 95.5 kB）、删除 `/vite.svg` 死引用；e2e 矩阵按新产物复测（`index.html` 1783 B、新 ETag）。**评审返工又并入**：`looks_like_asset` 改取最后一个非空段（`GET /gone.js/` 由 shell 改 404）、`Accept-Encoding` 多行折叠（`get_all` join）、两个 500 兜底补 `nosniff`
- **2026-10-23** · **UI**：[0007 Provider 列表删除 Status 列](ui/changes/0007-provider-list-drop-status-column.md) — 只读的 enabled/disabled Badge 与 Actions 列真正生效的 Switch 表达同一状态，删掉 Status 列（启停能力不变，仍由 Switch 直接切换）
- **2026-10-23** · **UI**：[0005 构建侧收尾：`vendor-chart` 分包与删除 favicon 死引用](ui/changes/0005-build-chunk-splitting-and-favicon.md) — rolldown `codeSplitting.groups` 拆 `vendor-chart`（chart.js / vue-chartjs，`includeDependenciesRecursively: false`），首屏 `modulepreload` 仍 11 条；`index.html` 删 `/vite.svg` 死引用（1845 → 1783 B）

- **2026-10-22** · **架构**：[0004 内嵌 UI 静态服务：缓存分档、兜底收窄、零拷贝与 ETag](architecture/changes/0004-ui-static-cache-and-fallback.md) — `assets/**` 一年 `immutable`、`index.html` 保持 `no-cache` 并补 `ETag`/`304`；兜底按 `api/`→`v1/`→方法→`resolve`→`assets/`→`Accept`→末段带点的顺序收窄（未注册 API 路径 200 HTML → 404 JSON，静态面只接受 `GET`/`HEAD`，缺失资源 404 不回 shell）；`Body::from(file.data)` 去掉 `into_owned()` 的每次请求全量 memcpy；全部响应补 `nosniff`，`UiAssets` 排除 `*.map`

- **2026-10-22** · **调度**：[0007 流式终点帧竞态误分类修复](dispatch/changes/0007-stream-terminal-frame-race.md) — 终点帧转发后的客户端断连改记成功（499 下降 / 成功上升，统计口径一次性台阶）并携带完整 usage / TTFT / upstream 时长；终点帧判定覆盖 Responses 三个终态（含 `response.cancelled`），并补三条 translated 桥接（`[DONE]` / `message_stop` / `response.completed`，含不合规上游的块闭合与 `stop_reason`）

- **2026-10-21** · **routing**：[0004 call_logs 会话哈希不区分策略记录](routing/changes/0004-session-hash-for-every-strategy.md) — 会话归属是事实不是行为：所有策略/image/pre-resolve 失败行一律记录会话哈希
- **2026-10-21** · **routing**：[0003 会话粘性路由与 priority 加权随机基底](routing/changes/0003-session-affinity-weighted-random.md) — 新增 `session_sticky` 策略：会话按上次成功目标粘住，未命中按 priority 加权随机全排列

## 2026-10-19

- **供应商**：[0007 跨 codec 工具字段保真（统一 `IrTool.raw` 双层约定）](providers/changes/0007-tool-raw-cross-codec-fidelity.md) — chat 双层 / Responses、Anthropic 扁平的两套 `raw` 约定导致四个缺陷（Responses→Chat 丢 `tools[].type` 触发上游 pydantic 500、同路径丢 `strict`、Anthropic→Chat 丢 `type`、Chat→Responses/Anthropic 泄漏 `__tool_extra`/`__function_extra`）；新增 `codec/tool_raw.rs` 共享 helper（`__tool_extra` 键存在性作 chat 来源判别式 + 双轨读取 + 展开写入），chat 出站对外来 IR 走扁平 fallback 并合成 `"type": "function"`（chat↔chat 零合成不发明 `type`），Responses/Anthropic 出站展开双层键（`"function"` / `""` 按值省略，对所有来源一致），hosted `web_search*` 落 chat 目标改为丢弃 + 记 `LossReason::UnsupportedByTarget`
- **日志**：[0004 响应体超限不再毁坏响应](logging/changes/0004-response-body-truncation-fix.md) — `log_http` 不再用 `to_bytes(..).unwrap_or_default()` 把 > 10 MB 响应换成空 body：改为帧级有界收集 + 帧链重组（未超限字节级不变，超限时已收集帧与剩余流 `chain` 后全量转发、headers 不动），日志超限只打 512 B 预览 + `truncated=true` / `collected=<n>`；body 流错误不再被吞，记 `body_error=true` WARN 并传播给客户端；请求侧孪生缺陷（谎报 content-length 导致 handler 收到空 body）同步修复

## 2026-10-18

- **架构**：[0003 HTTP 响应压缩：SSE 防护头、UI 构建期预压缩、动态压缩层](architecture/changes/0003-http-response-compression.md) — SSE 响应补 `Cache-Control: no-cache, no-transform` + `X-Accel-Buffering: no` 防中间层缓冲；UI 构建新增 `postbuild-compress.mjs` 生成 `.br`/`.gz` 副本，`ui_routes` 按 `Accept-Encoding` 直接下发预压缩件（带 `Vary`，运行期零压缩）；`sag-router` 插入 `CompressionLayer`（`Fastest`，`SizeAbove(1024)` ∧ ¬SSE/GRPC/IMAGES，默认开启无配置）；`just clippy` / `just test-rust` 补 `--features embed-ui` 消除门控代码盲区

## 2026-10-17

- **analytics**：[0008 退役旧分析端点，固化两套口径契约分层](analytics/changes/0008-retire-legacy-analytics-endpoints.md) — 删除 `/v1/analytics/{cache,trend,distribution}`（零消费方 + 承诺无法兼现），保留 `/v1/analytics/stats` 与 `/api/logs` 作为热窗口诊断面（`/api/logs` 不迁移到事实表，dashboard 不展示统计），并将「诊断口径 vs 分析口径数字不可对比」写进契约
- **分析**：[0007 call_logs 保留窗口 + 表轮转（热窗口视图）](analytics/changes/0007-call-log-retention-rotation.md) — 整表轮转为冷表 `call_logs_arch_*`，`call_logs_hot`/`call_logs_all` 视图承接热窗口与全史查询。

## 2026-10-15

- **UI**：[0004 UI 多语言支持（i18n，第一版 en/zh）](ui/changes/0004-ui-i18n-language-switching.md) — 新增无依赖的 DIY i18n 层（`t()` 命名插值 + `active → en → key` 三段回退、`useI18n()`、`installI18n()` 同步 `<html lang>`）；locale 经 `localStorage["sag-ui-lang"]` 持久化，默认 `zh` 且白名单校验脏值自愈；Settings 页新增 Language tab；539 个 key 全量迁移（`en.ts` canonical `as const`，`zh.ts: Record<keyof typeof en, string>` 编译期齐平），TanStack 表头改 `() => t(...)`、标签映射改 `computed`、表单 `validators` 改 getter 以免切换语言时冻结；新增 `check-i18n.mjs` 防回潮门禁（键/占位符齐平、花括号合法、CJK 字面量扫描、dead-key 报告）以 `ui-i18n-check` 接入 `verify-ui`

## 2026-10-13

- **配置**：[0006 XDG Base Directory 标准路径布局](config/changes/0006-xdg-base-directories.md) — 配置/数据库/日志按 XDG 标准落位（`$XDG_CONFIG_HOME` / `$XDG_DATA_HOME` / `$XDG_STATE_HOME` 下的 `sag/` 子目录）；配置查找顺序 `--config` > `./gateway.toml`（存在时）> XDG 默认；相对 `sqlite://` DSN 锚定数据目录并修复 from_str 不建库的 `SQLITE_CANTOPEN`；test-env 以项目内 XDG 覆盖保持两轨物理隔离

## 2026-10-11

- **配置**：[0007 顶级配置 Admin API 与 Settings 页可视化](config/changes/0007-gateway-config-visualization.md) — 新增 GET/PUT `/api/config` 暴露 server / logging / analytics / backoff / circuit_breaker（含 `probe_interval_secs`）与两个调度 TTL 标量；可热生效参数经 coordinator 统一 mutation 协议保存即时生效，需重启 section 只读标注；Settings 页扩展 7 个 tab
- **日志**：[0003 大请求响应日志与请求关联信息](logging/changes/0003-request-correlation-and-large-body-response-log.md) — 修复三个可观测性缺口：body > 512 KiB 的请求早退路径不再跳过响应日志（补 status / elapsed_ms，不消费 body）；provider 流读取任务改用 `spawn_in_current_span()` 继承 provider/model span（ERROR 可关联 trace_id）；流式响应 commit 后的客户端断开补一条 WARN


## 2026-10-09

- **供应商**：[0006 Chat 入口容忍缺失 `content` 的消息（assistant tool_calls 形态）](providers/changes/0006-chat-message-content-optional.md) — zcode 多轮工具调用回放发送仅含 `tool_calls`、省略 `content` 键的 assistant 消息（OpenAI 规范合法），入口强类型反序列化因 `content` 无 `#[serde(default)]` 而误拒 422；补上 default 后缺失 → `Null`，native 透传重新序列化为 `"content": null`（OpenAI 规范输出形态），codec 既有 `Null → 空内容块` 兼容无需变更

## 2026-10-06

- **UI**：[0003 移动端弹框统一为底部抽屉](ui/changes/0003-responsive-modal-drawer.md) — 新增 `useResponsiveModal()` 收敛 Dialog/Drawer 双轨映射与 768px 断点；`ProviderAddForm`/`ProviderEditForm` 的两份内联映射改为复用，`RoutesPage`、`RouteList`（编辑/删除/清除 Active Target 三处）、`ModelManager`（快捷建单确认）从纯 Dialog 迁到双轨

## 2026-09-30

- **管理 API**：[0002 探针与模型测试的协议对齐](admin/changes/0002-probe-model-test-protocol-alignment.md) — `model/test` 改走 provider adapter（合成单 lane `ProviderConfig` → `create_provider_with_proxy`），per-API headers 分层、`body_overrides`/`strip_fields`/`model_strip_fields`/`role_mappings`/`anthropic_cache_control` 与生产一致，模型级 override 经 `target_extra` 传入；支持三协议（`image` → 400 `UNSUPPORTED_API_KIND`），`api_keys` 上限 16、per-API 超时钳到 30s；协议知识收敛到 `probe_request::minimal_request` + `sag_core::protocol_path`（删除重复的 `provider_chat_endpoint` / `probe_body`）；probe 保持原始 HTTP 与裸语义（2xx 恒 `supported`），`message` 补 body 预览
- **供应商**：[0005 共享 HTTP client 连接池 idle 调优（降低 stale 连接复用）](providers/changes/0005-http-client-pool-idle-tuning.md) — `build_http_client` 写死 `POOL_IDLE_TIMEOUT=30s` / `POOL_MAX_IDLE_PER_HOST=4`，降低「复用已被对端关闭的连接 → 流中途 close_notify EOF」概率；只限 idle 池、不截断在途流；未加回 client 级 total `.timeout()`（保持 0006 流式超时边界）；不修已断的流（C′ 另议）

## 2026-09-28

- **供应商**：[0004 供应商 role_mappings 编辑入口](providers/changes/0004-role-mappings-editor.md) — Provider 新增/编辑表单 Advanced tab 新增 Role Mappings 键值编辑区；编辑表单以 `JSON.stringify` 脏检查实现 PUT「缺省字段保留旧值 / 传 `{}` 清空」语义，新增表单仅在非空时携带该字段

## 2026-09-17

- **架构**：[0002 版本升级提交按实际变更文件过滤与 workspace 版本继承](architecture/changes/0002-version-bump-commit-files.md) — 版本升级脚本参考 `bumpp` 的 `updatedFiles` 语义，仅提交实际变更且已被 Git 跟踪的文件，避免被忽略的 `Cargo.lock` 触发无效 pathspec；版本收敛到根 `Cargo.toml` 的 `[workspace.package] version`，成员 `Cargo.toml` 通过 `version.workspace = true` 继承，脚本不再改写成员文件

## 2026-09-15

- **分析**：[0006 200 状态内上游错误识别 + 取消流写 call_logs](analytics/changes/0006-upstream-error-in-200-and-cancelled-call-logs.md) — 识别 HTTP 200 内交付的上游错误（SSE data frame / 200 body 顶层 `error` 对象）：流式进入 `model_failure` / `stream_error_after_commit` 失败路径，非流式记为失败尝试并可 failover、限流特征映射 `rate_limited`、`call_logs` 记 502（限流 429）+ 上游消息；被取消的流式请求在 `TerminalGuard` 之外补写 `call_logs` 行（499 / `client_cancelled`），保证管理面板日志列表可见
- **Sticky Targets**：[0006 target_index 改为优先级降序秩](sticky-targets-observability/changes/0006-target-index-priority-rank.md) — 前端用 `target_index === 0` 判定主目标，原始数组位置在配置 priority 时会指错主目标；`target_index` 改为优先级降序秩（stable sort by priority 降序、同优先级保持数组顺序、rank 0 = 最高优先级 target），响应 targets 数组保持 config 顺序，全默认优先级时行为不变

## 2026-09-11

- **分析**：[0005 共享 SQLite 连接池 + WAL / BEGIN IMMEDIATE 锁修复](analytics/changes/0005-shared-pool-lock-fix.md) — 旧 CallLogCollector 改为共享正式分析连接池（`new_shared` / `from_pool`，store 不关闭共享 pool）；file DSN 统一 WAL + `synchronous=NORMAL` + busy_timeout 连接选项（`:memory:` 跳过 WAL）；EventWriter / RollupEngine / 身份 sync / 旧 store 整批插入的写事务全部改 `BEGIN IMMEDIATE`，消除双 pool 锁竞争与 deferred-BEGIN 升级竞态

## 2026-09-08

- **配置**：[0005 出站代理配置：顶层 proxy_url + provider proxy_mode 三态](config/changes/0005-outbound-proxy-configuration.md) — 顶层 `proxy_url` 定义全局 HTTP/HTTPS 代理，provider 三态 `proxy_mode`（inherit/enabled/disabled）控制继承/强制/直连；SAG 显式屏蔽系统代理环境变量，`ProxyConfig` 仅存 scheme/host/port 拒绝 userinfo 防泄漏；启动/热重载/mutation/临时测试统一经 `effective_proxy_for` 解析，hot reload 按 effective proxy 差异重建受影响 provider；新增 GET/PUT `/api/settings` 与 Settings 页
- **Sticky Targets**：[0005 target_index 权威首项判定与策略切换残留清理](sticky-targets-observability/changes/0005-target-index-and-strategy-switch-cleanup.md) — DTO 携带 0-based `target_index` 作首项权威来源（取代对象身份）；GET 只对 StickyFailover 展示 pin（切换后残留报 idle）；DELETE 对每种策略都清 active_targets、FillFirst 额外清 order cache；route CRUD 与 gateway hot-reload（含 fallback retry）成功后立即 prune
- **Sticky Targets**：[0004 expired 圆点改 muted 灰绿/灰橙](sticky-targets-observability/changes/0004-expired-dot-muted-hue.md) — expired 不再统一灰色：命中 route.targets 首项 `bg-green-500/40`、非首项/无法确认 `bg-orange-500/40`（expired 表示过期 pin 的配置位置，非当前流量）；pinned 与 fill_first 行为不变

## 2026-09-07

- **Sticky Targets**：[0003 Routes 页 Active Target 状态圆点颜色](sticky-targets-observability/changes/0003-routes-active-target-status-colors.md) — 圆点改四态语义：fill_first 保持 emerald、sticky expired 改灰、pinned 命中 route.targets 原顺序首项改绿、非首项/无法确认改橙；helper 抽出到 `sag-ui/src/lib/activeTarget.ts` 并新增边界测试

## 2026-09-04

- **分析**：[0004 Token 命中率分母改为全部成功样本](analytics/changes/0004-token-cache-hit-rate-all-success.md) — `token_cache_hit_rate` 改为全部成功样本的 ΣCOALESCE(cached_tokens,0)/Σprompt_tokens，明细与 Rollup 混合路径口径一致

## 2026-09-03

- **配置**：[0004 配置未知字段告警重构：serde_ignored 统一 typed 反序列化](config/changes/0004-warn-unknown-config-fields.md) — 移除 raw TOML + 手写白名单，改用 `serde_ignored` 配合 `toml::Deserializer` 统一 typed 反序列化，未知字段仅 warn 不拒绝，load/reload 共用，动态 HashMap key 不误报
- **配置**：[0003 修复 macOS 文件句柄耗尽与回退白名单遗漏](config/changes/0003-use-native-fsevent-and-backoff-whitelist.md) — 移除 `macos_kqueue` 切换回原生 FSEvents，杜绝全项目 WalkDir 导致的句柄爆炸，并将 `retry_on_empty_response` 纳入配置白名单消除虚警
- **配置**：[0002 热重载 watcher 加固：尾部防抖、目录监听与瞬时失败重试](config/changes/0002-hot-reload-trailing-debounce.md) — 热重载改为尾部防抖（只加载突发写入的最终版本），同时监听父目录以在原子 temp-file + rename 后不失效，并增加 settle 边界排空与瞬时失败有界重试

## 2026-08-26

- **调度**：[0006 流式请求超时边界修复](dispatch/changes/0006-stream-request-timeout-boundary.md) — 成功流式响应不再受共享 reqwest client 总 timeout 截断，非流式、TTFB 和 chunk idle timeout 分阶段生效

## 2026-08-25

- **供应商**：[0003 供应商模型下拉自动拉取与缺失标记](providers/changes/0003-combobox-auto-fetch-and-missing-indicator.md) — Combobox 点击展开自动请求 models 接口并在下拉内展示 loading；比对已启用模型并在已选列表和下拉项中标记上游不存在的模型
- **路由**：[0002 目标模型有效性校验与 Provider 变更联动感知](routing/changes/0002-target-model-validity-and-provider-sync.md) — 运行时 `filter_enabled_targets` 与 `/v1/models` 增加 `enabled_models` 白名单过滤；前端 Provider 变更联动清理模型缓存，Route 列表与 Target 编辑器增强失效目标诊断与高亮提示

## 2026-08-18

- **熔断器**：[0006 熔断器逻辑概念内聚与探测槽生命周期治理（RAII）](circuit-breaker/changes/0006-concept-consolidation-and-probe-guard-lifecycle.md) — `CircuitPermit`/`ProbeGuard` RAII 守卫收敛探测槽归还（`is_probe` 布尔与手工释放分支消失），`force_close_circuit` 下沉 `sag-state`，移除 shim 并统一 `DispatchRuntime.circuit_breaker` 命名；行为零回归

## 2026-08-17

- **调度**：[0005 聊天分发单文件复杂度解耦与模块化](dispatch/changes/0005-chat-dispatch-modularization.md) — 将 3900+ 行 `chat.rs` 解耦拆分为 `chat/` 模块目录（`mod.rs` 主调度、`attempt.rs` 事实构造、`empty.rs` 空响应校验、`tests.rs` 集成测试）
- **分析**：[0003 UUID v4/v7 版本分层决策](analytics/changes/0003-uuid-version-layering.md) — 记录 request_id(v4) 与身份 ID(v7) 的版本分工理据，明确共存是有意分层非技术债

## 2026-08-11

- **分析**：[0002 双层事实与 Rollup](analytics/changes/0002-two-level-facts.md) — 缓存命中率 Rollup 可合并计数（known/hit/prompt_known）、actual_model 语义、failure_reason 映射表、target 三值枚举（+cancelled）、flowchart 补 emit 边、FK RESTRICT 等设计修订

## 2026-08-04

- **Failover 4xx 策略**：[0001 4xx Failover 归因策略](failover-4xx-policy/changes/0001-4xx-attribution-policy.md) — 4xx 三分归因（客户端返客户端不 failover / provider 持续性 failover+挪 pin / 瞬时熔断）；403-quota 重分类为可重试+熔断
- **Sticky Targets**：[0002 端点重命名 /api/active-targets](sticky-targets-observability/changes/0002-rename-active-targets.md) — 端点路径与全链路标识符（handler/DTO/store/组件）从 sticky-targets 改名 active-targets，硬切换不留旧路径

- **Sticky Targets**：[0001 FillFirst 手动重置与可观测性](sticky-targets-observability/changes/0001-fill-first.md) — `CachedOrder` 下沉 `sag-state` 并注入 `AppState`；GET 对 FillFirst 路由报缓存顺序位置/age/TTL，DELETE 按 strategy 分发（FillFirst 清缓存重新洗牌）

## 2026-07-22

- **熔断器**：[0005 手动关闭熔断时清除 StickyFailover 粘性记忆](circuit-breaker/changes/0005-manual-close-clears-sticky.md) — `active_targets` 提升为 `AppState` 共享 Arc，`close` handler 无条件清除 sticky 记忆
- **调度**：[0004 上游 4xx 不再触发重试与熔断](dispatch/changes/0004-upstream-4xx-non-retryable.md) — `is_retryable_error` 对 `Upstream` 按 status 判定，4xx（除 408/429）不可重试、不熔断、failover
- **Body Override**：[0002 per-provider strip_fields 字段剥离](body-override/changes/0002-strip-fields.md) — 新增 provider 级字段剥离，在三层 override 之后硬性移除指定字段

## 2026-07-19

- **供应商**：[0002 Anthropic reasoning_effort 翻译为 extended thinking](providers/changes/0002-anthropic-reasoning-effort.md) — 7 档映射表 + 递归降级；`reasoning_effort` 入口归一化；`thinking_tokens` 捕获
- **调度**：[0003 失败路径补全 reasoning_effort 记录](dispatch/changes/0003-reasoning-effort-failure-paths.md) — `failure_log` 及三个 wrapper 签名补 `reasoning_effort` 参数，6 个调用点补传；4 个 round-trip 测试

## 2026-07-17

- **熔断器**：[0003 探测失败隐瞒冷却下界与流式取消探测槽泄漏修复](circuit-breaker/changes/0003-probe-lifetime-fix.md) — `cooldown_until` 提升至 `CircuitEntry` 跨状态保留（修复探测失败覆盖硬冷却）；`try_acquire_probe` 兜底回收僵死探测槽（修复流式取消导致永久跳过）；`prune_expired` 统一按 `cooldown_until` 清理 Open/HalfOpen

## 2026-07-16

- **熔断器**：[0002 熔断半开探测与冷却期周期探测](circuit-breaker/changes/0002-half-open-periodic-probe.md) — 三态机 + 冷却期内按 `probe_interval_secs` 周期探测，上游恢复最多一个 interval 内被发现（提前感知）；修复 `resolve_provider` 失败路径探测槽泄漏

## 2026-07-06

- **调度**：[0002 路由删除后缓存泄漏修复](dispatch/changes/0002-cache-leak-fix.md) — `resolve_route()` 开头惰性清理已删除路由的 `fill_first_orders`、`route_counters`、`active_targets` 缓存条目

## 2026-06-27

- **UI**：[0002 表格固定表头与玻璃拟态效果](ui/changes/0002-sticky-header.md) — 四个表格页面 sticky 表头、`[&>div]:h-full` 高度传递模式、玻璃拟态表头

## 2026-06-26

- **UI**：[0001 初始化与大屏幕响应式优化](ui/changes/0001-initial.md) — 管理面板规格文档、`xl:`/`2xl:` 断点响应式布局
- **日志**：[0002 日志轮转与清理](logging/changes/0002-rotation-and-cleanup.md) — TTY 自适应输出、每日轮转、启动时清理 7 天前日志

## 2026-06-14

- **熔断器**：[0001 初始化](circuit-breaker/changes/0001-initial.md) — 按错误类型分类冷却

## 2026-06-03

- **架构**：[0001 初始化](architecture/changes/0001-initial.md) — 8 子包 workspace、启动编排
- **配置**：[0001 初始化](config/changes/0001-initial.md) — TOML 加载、热重载、原位保存
- **路由**：[0001 初始化](routing/changes/0001-initial.md) — 路由匹配、目标选择、shuffle 缓存
- **调度**：[0001 初始化](dispatch/changes/0001-initial.md) — 分发、重试退避、流式处理
- **供应商**：[0001 初始化](providers/changes/0001-initial.md) — OpenAI/Anthropic 适配器
- **认证**：[0001 初始化](auth/changes/0001-initial.md) — Bearer token 可选认证
- **分析**：[0001 初始化](analytics/changes/0001-initial.md) — SQLite 异步调用日志
- **日志**：[0001 初始化](logging/changes/0001-initial.md) — tracing、HTTP 日志、trace ID
- **管理 API**：[0001 初始化](admin/changes/0001-initial.md) — 供应商/路由 CRUD