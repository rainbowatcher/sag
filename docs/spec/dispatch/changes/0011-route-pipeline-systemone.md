---
created: 2026-11-09
updated: 2026-11-09
type: 新增
---

# 0011 — route 级 `pipeline = "systemone"`

## 动机

0008 把 `POST /v1/systemone` 接了进来，但当时唯一可用的路由形状是「`pipeline = "chat"` 的 route 接住 decision 请求」：靠 `filter_systemone_targets` 的 `apis.systemone` presence 闸门，把不懂该协议的 provider 从目标里摘掉。这条兼容路径能用，却表达不了「这条 route 只服务决策协议」：

1. **无法按协议拆分流量**。同一个 `model_name` 必须同时接受 chat ingress 与 systemone ingress，运维没有办法把 chat 流量与 decision 流量分到不同上游、不同重试策略。
2. **派生视图说错话**。`GET /v1/models` 的 `ingress_protocols` 是 route 各目标 provider `apis` 的并集，一条只想服务 systemone 的 route 仍会被通告成 chat 协议，客户端据此发 chat 请求只会拿到 404。

本变更给 `[[routes]]` 增加显式的 `pipeline = "systemone"`：这样的 route **只在** `/v1/systemone` 上存在，其它 chat 族 ingress 对它一律按「无此模型」拒绝，不回退到 chat / image 分发。

```toml
[[routes]]
model_name = "sys-one-model"
pipeline   = "systemone"

  [[routes.targets]]
  provider   = "my-systemone"
  real_model = "sys-one-v1"
```

## 需求变更

### 配置形状

`pipeline` 取值域从 `chat` / `image` 扩为 `chat` / `image` / `systemone`（serde 名 `systemone`，不带分隔符；`system_one`、`SystemOne`、`systemone2` 均非法）。省略 `pipeline` 时仍是 `chat`——默认值、落盘时的省略规则都不变。

### 路由匹配语义

route 先按 `model_name` 精确匹配、再回退 `"*"` 通配；两种 pipeline 共用这一个匹配逻辑，守卫落在「route 已选中、target 过滤之前」：

| ingress | 选中 route 的 pipeline | 结果 |
|---|---|---|
| `/v1/systemone` | `systemone` | 接受；enabled 过滤 + `apis.systemone` presence 过滤；走 SystemOne dispatch |
| `/v1/systemone` | `chat` | 与改动前完全一致（presence 过滤 + SystemOne dispatch）|
| `/v1/systemone` | `image` | 与改动前完全一致（先 presence 过滤；存活则 handler 拒绝 Image）|
| `/v1/chat/completions` | `systemone` | no-route，不 fall through 到 chat/image dispatch |
| `/v1/chat/completions` | `chat` | 不变 |
| `/v1/chat/completions` | `image` | 不变 |
| `/v1/messages`、`/v1/responses` | `systemone` | no-route，保留各自错误信封 |

「no-route」指沿用既有的「无此模型」失败路径与错误信封，**不**新增错误类型：站在这些 ingress 的角度，该模型确实不存在。守卫位于 route 选中之后、`filter_enabled_targets` 之前，因此不会消耗目标、不会打上游、不产生 attempt 行。

### route 身份只有 `model_name`

route 的身份**仅仅**是 `model_name`：

- 落盘侧按 `model_name` 匹配 `[[routes]]`（`sync_array_of_tables` 的 key 取 `route.model_name`）；
- admin 创建路径拒绝重名（`route '<name>' already exists`），编辑 / 删除同样按 `model_name` 定位；
- 解析侧先精确匹配、再回退 `"*"`。

因此**同一 `model_name` 不能并存一条 chat route 与一条 systemone route**：后写入的那条要么被拒（同名创建），要么在保存时与既有行合并成同一段 TOML。想让同一个上游模型同时服务 chat 与 decision，只有两条路——一条 `pipeline = "chat"` 的 route 走 0008 的兼容路径（它会接受 `/v1/systemone`），或给 systemone 用另一个 `model_name`。

`"*"` 通配 route 也共用这条身份规则：通配只有一条，若把它声明成 `pipeline = "systemone"`，所有未被精确 route 命中的 chat 模型都会一起变成 no-route。校验层没有为此单独加规则（通配本身仍是合法的 `model_name`），这是运维选择，不是被禁止的组合——但文档提醒它等于放弃 chat 兜底。

### 校验规则

`GatewayConfig::validate` 对 `pipeline = "systemone"` 的 route 追加/沿用以下判定（admin API 的创建 / 更新走同一份校验，违规即 400）：

- **`api_endpoint` 非法**：仅 `image` 允许该字段。systemone 的上游路径由协议派生（`protocol_path`），声明 `api_endpoint` 是死配置——显式拒绝（含空串），不静默忽略。
- **`session_sticky` 不支持**：`dispatch_systemone` 丢弃 session write-back（与 `dispatch_image` 同理），pin 永远写不进去，route 会静默退化成加权随机。首选策略必须是 `sticky_failover` / `fill_first` / `round_robin` 之一。
- **target 缺 `apis.systemone` 只是警告**：该目标在 resolve 阶段被跳过（与 unhealthy target 同一条诊断路径），不是硬错误——这样一条 route 可以把「还没配好」的 provider 放在低优先级兜底位而不阻塞启动。
- **target 引用的 provider 不存在仍是硬错误**：`target.provider` 不在 `[[providers]]` 名字集合内时，配置校验 push `references unknown provider '<name>'`，启动失败 / admin CRUD 400。这条规则**不因** `pipeline = "systemone"` 而放宽，与上一条（provider 存在但未声明 `apis.systemone` 只记 warning）是两条不同规则，不要混淆：**配置校验期**的未知 provider 名是配置写错（名字根本不存在）；**运行时**的 `apis.systemone` 缺失是该 target 被 pre-drop 并留诊断。
- **四条 strategy 全部适用**：`sticky_failover` / `fill_first` / `round_robin` 均可；`session_sticky` 除外。
- **`capabilities` 保留既有校验**，但 chat 的能力预检（`filter_capable_targets`）对 SystemOne **不适用**：SystemOne 的 `streaming` / `reasoning` / `tools` / `images` / `documents` 能力位全为 false，presence 即能力。
- **`fallback` 与本次无关**：`fallback` 的取值域与需求判定仍只统计 chat IR 三协议，`fallback = "systemone"` 继续在配置校验期报错。systemone 仍不可作 fallback。

### `/v1/models`

显式的 systemone route 只在「至少一个 eligible target 的 provider 声明了 `apis.systemone`」时通告 `systemone` 协议，且**只通告 `systemone`**——不因这些 provider 同时声明了 chat 节点而通告 chat 协议（chat 请求对它只会 no-route）。旧 route（`pipeline = "chat"` / `"image"`）的聚合行为逐字不变，包括其既有怪癖。

### 兼容性

纯增量：仓库里不存在任何 `pipeline = "systemone"` 时，配置解析、路由匹配、`/v1/models` 通告、admin CRUD 的行为与改动前完全一致。旧 route 被 `/v1/systemone` 命中（`pipeline = "chat"`）时走的仍是 0008 的 presence 过滤 + SystemOne dispatch，本轮只在其前面加了一个「route 自己不是 systemone pipeline」的判断，对该组合恒为真。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`gateway.example.toml`](../../../../gateway.example.toml) | 新增 `pipeline = "systemone"` 注释示例 |
| [`sag-config/src/config.rs`](../../../../sag-config/src/config.rs) | `Pipeline` 新增 `SystemOne`（`#[serde(rename = "systemone")]` 覆盖 `rename_all` 的产物）与 `as_str()` 对应分支；`validate_routes` 拒绝 systemone × `api_endpoint`、systemone × `session_sticky`，并对缺 `apis.systemone` 的 target 记 warning；`resolve_model_route` 的精确 / 通配两级匹配不变 |
| [`sag-config/src/loader/save.rs`](../../../../sag-config/src/loader/save.rs) | 保存路径经 `Pipeline::as_str()` 落盘，非默认 pipeline 写出 `pipeline = "..."`（systemone 因此无需特判）；`[[routes]]` 仍按 `model_name` 作为唯一 key 合并 |
| [`sag-dispatch/src/dispatch/resolve.rs`](../../../../sag-dispatch/src/dispatch/resolve.rs) | `resolve_route_with_ingress` 在 route 选中之后、`filter_enabled_targets` 之前加 pipeline × ingress 守卫：`pipeline == SystemOne && ingress != SystemOne` → 走既有 no-route 失败路径；`filter_systemone_targets` 与 enabled 过滤保持原职责 |
| [`sag-router/src/handlers/mod.rs`](../../../../sag-router/src/handlers/mod.rs) | `list_models` 对显式 systemone route 单独派生 `ingress_protocols`（有 eligible 且声明 `apis.systemone` 的目标才通告 `systemone`，不通告 chat 协议）；旧 pipeline 分支保持原聚合 |
| [`sag-admin/src/admin_routes.rs`](../../../../sag-admin/src/admin_routes.rs) | 路由 CRUD 复用 `Option<Pipeline>` 透传第三值（创建 / 更新 / 读回均原样）；`pipeline = "systemone"` 的 `api_endpoint` / `session_sticky` 组合由统一校验转 400，不落盘 |
| [`sag-router/tests/systemone_ingress.rs`](../../../../sag-router/tests/systemone_ingress.rs) | 新增显式 systemone route 的入口级用例：chat 族 ingress 拒绝且不触达上游、`/v1/systemone` 正常透传、`/v1/models` 只通告 systemone |
| [`docs/spec/dispatch/requirements.md`](../requirements.md) | 记录本变更的需求与实现细节 |
| [`docs/spec/dispatch/spec.md`](../spec.md) | 记录本变更的需求与实现细节 |
| [`docs/spec/CHANGELOG.md`](../../../CHANGELOG.md) | 登记 Dispatch 0011 条目 |

## 测试

- 配置解析（`sag-config/src/config.rs`）：`pipeline = "systemone"` 往返合法；`system_one` / `SystemOne` / `systemone2` 被拒；systemone × `api_endpoint`（含空串）报 `only valid when pipeline = "image"`；systemone × `session_sticky` 报不支持；缺 `apis.systemone` 的 target 只产生 warning、不阻断启动。
- 解析守卫（`sag-dispatch/src/dispatch/resolve.rs`）：`build_state_with_pipeline(Pipeline::SystemOne)` 下 `/v1/systemone` 解析成功且 `resolved.pipeline == SystemOne`；chat 族 ingress 走 no-route，不产生目标、不产生 attempt。
- 入口集成（`sag-router/tests/systemone_ingress.rs`）：`/v1/chat/completions`、`/v1/responses`、`/v1/messages` 打显式 systemone route 全部返回客户端 4xx（`MODEL_NOT_FOUND` / `BAD_REQUEST`）且 mock 上游**从未被连接**；`/v1/systemone` 命中时上游路径为 `/systemone`、`model` 被改写为 `real_model`、响应逐字回给客户端；`/v1/models` 的 `ingress_protocols` 恰为 `["systemone"]`。
- admin CRUD（`sag-admin/src/admin_routes.rs`）：`POST /api/routes` 带 `pipeline = "systemone"` 创建成功且 `api_endpoint` 为 `null`，`GET /api/routes` 原样回报；带 `api_endpoint` 或 `session_sticky` 的同一 route 返回 400 且不落盘。
- 兼容性：既有 `pipeline = "chat"` 的 systemone ingress 用例（0008 引入的 8 例）全部保持通过，无需改动 fixture。
