---
created: 2026-06-03
updated: 2026-10-26
---

# 管理 API

## 目标

提供 REST API 在运行时管理供应商和路由，支持 CRUD 操作和模型连接测试。

## 预期行为

### 供应商管理

- `GET /api/providers` — 列出所有供应商
- `POST /api/providers` — 添加供应商，返回 409 如果 name 已存在
- `PUT /api/providers` — 更新供应商，PATCH 语义（只更新传入字段）
- `DELETE /api/providers` — 删除供应商及其关联路由 target

### 路由管理

- `GET /api/routes` — 列出所有路由
- `POST /api/routes` — 创建路由，返回 409 如果 model_name 已存在
- `PUT /api/routes` — 更新路由（全量替换）
- `DELETE /api/routes` — 按 model_name 删除路由

### 模型测试

- `POST /api/model/test` — 发送最小消息到指定模型，验证连接和认证
- 支持多个 API key，逐一测试并返回每个 key 的结果；每次测试最多 16 个 key（超出返回 400 `TOO_MANY_KEYS`），模型级 override 仅两层且 `api.model_overrides[<model>]` 优先于 provider 级 `model_overrides[<model>]`
- 支持 `chat_completions` / `responses` / `anthropic_messages` 三种协议（`image` 返回 400 `UNSUPPORTED_API_KIND`）
- 上游请求经由 provider adapter 发送，与生产派发路径一致；因此上游返回 HTTP 200 但响应体携带 `error` 对象时同样判定为失败
- 明确不在覆盖范围内：路由 `target.extra` 合并、模型解析/映射、`enabled_models` 门控、provider 级 ttfb/chunk 超时；测试对所选协议使用非流式原生调用

### 协议探测

- `POST /api/providers/probe-protocols` — 对每个「协议 × base_url × key」发一条最小请求，判断上游是否支持该协议；只读，不写配置
- 三协议的最小请求体由 `probe_request::minimal_request` 统一构造（`chat_completions` 用 `max_completion_tokens`、`responses` 用 `input` + `max_output_tokens`、`anthropic_messages` 用 `max_tokens`）；探测地址由 `sag_core::protocol_path` 派生；`image` 不是协议，target 携带 `image` 返回 400
- 判定只看 HTTP 状态码：2xx 一律 `supported`，不做错误体嗅探（200 body 里夹带 `error` 对象仍是 `supported`）；2xx 的 `message` 给出上游 body 预览（折叠空白、截断 160 字符，body 为空时为 `OK`）
- 探测走原始 HTTP，**不**复用 provider adapter，也不读取任何已保存 provider 的配置（裸语义）；协议知识（path / body / 鉴权头）与模型测试共享

### 供应商模型查询

- `GET /api/providers/{name}/models` — 返回 provider 的 `enabled_models` 列表

### 日志查询

- `GET /api/logs` — 查询调用日志，支持分页和过滤
  - 查询参数：`provider` / `model` / `real_model` / `session` / `status_code` / `offset` / `limit`；缺省或空串均视为「未设置」而不是 400。`session` 对 `call_logs.session_key`（32 位哈希）做精确匹配。
  - 响应信封 `{"data": [CallLog...], "total": n}`，并在带 `?session=<key>` 时额外携带 `session` 字段，三态：
    - 未带 `session` 参数（或值为空串）—— **键不存在**，旧信封逐字保留；
    - 带 `session` 但 `session_registry` 无该行 —— `null`（旧数据 / 已过保留期）；
    - 带 `session` 且命中 —— 会话记录对象（`session_key` / `session_source` / `conversation_id` / `first_message_preview` / `first_seen` / `last_seen`）。
  - 注册表查询失败降级为 `session: null` 并记 `error!`：明细页是主载荷，不因装饰性会话卡片而 500。
  - 会话记录的字段语义、合并与保留期见 [分析 → 0009](../analytics/changes/0009-session-registry-and-logs-session-envelope.md)。

### 在途请求观测

- `GET /api/inflight` — 以 `text/event-stream` 推送各路由的实时在途请求数，事件名 `inflight`
  - 载荷为**全量快照** `{"seq": u64, "total": usize, "routes": [{"model_name": string, "count": u64}]}`：连接即发当前快照，此后每次计数变化再推一份；消费方整体替换，不做增量合并（丢帧不会失步，重连由下一帧自愈）。
  - `routes` 按 `count` 降序（同数按 `model_name` 升序，帧内容确定）；`total` 恒为各 `count` 之和；`count` 归零的路由从快照中删除，因此快照只列出真正有在途请求的路由。
  - 空闲时每 15s 发一行 `: ping` 注释心跳（过代理保活 + 给客户端存活信号）。
  - 端点自身**不计数**：连接与断开都不占用任何在途槽位。
  - 计数的键是路由的 `model_name`（不是请求里的 model）；计数区间覆盖整条派发生命周期（含重试退避与流式传输），流式请求由客户端断连（body drop）释放。
  - 计数是进程内内存态，不写 `call_logs`、不影响路由选择。

### 认证

所有管理端点使用与模型 API 相同的 Bearer token 认证。`x-api-key` 仅解锁 `/v1/messages`，不放行任何 `/api/*` 端点（含 `/api/inflight`）。

## 功能边界

### 包含
- 供应商 CRUD
- 路由 CRUD
- 模型连接测试
- 配置持久化（修改后自动保存到 TOML）

### 不包含
- 批量操作
- 配置回滚
- 操作审计日志

## 与其它模块的交互

- **配置**：修改后调用 `save_config_to_file()` 持久化 + 热重载
- **供应商**：调用 provider 进行模型测试
- **分析**：查询调用日志

## 变更记录

- [0003 per-route 在途计数与 `GET /api/inflight` SSE 端点](changes/0003-per-route-in-flight-counters-sse.md)
- [0009 会话注册表与 /api/logs 的 session 信封](../analytics/changes/0009-session-registry-and-logs-session-envelope.md)
- [0001 初始化](changes/0001-initial.md)
- [0002 探针与模型测试的协议对齐](changes/0002-probe-model-test-protocol-alignment.md)
