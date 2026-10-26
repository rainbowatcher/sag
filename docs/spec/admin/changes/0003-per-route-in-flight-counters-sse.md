---
created: 2026-10-26
updated: 2026-10-26
type: 新增
---

# 0003 — per-route 在途计数与 `GET /api/inflight` SSE 端点

## 动机

路由页能列出「每条路由配了哪些 target、当前粘在谁身上」（sticky-targets 0001–0006），
但**看不见此刻有多少请求正在被处理**。运维在故障期间的第一个问题是「是不是全压在同一条
路由上」，此前只能从 `/api/logs` 的事后记录推断，或干脆上 `top`/日志。

本次补一个只读的实时观测面：dispatch 侧对每条路由维护一个在途计数（排队 + 上游耗时 +
流式传输全过程），管理面通过 `GET /api/inflight` 以 SSE 推送**全量快照**。计数本身不进
`call_logs`、不影响路由选择，是纯观测设施。

## 需求变更

- `docs/spec/admin/requirements.md`「预期行为」新增「在途请求观测」一节：
  - `GET /api/inflight` 返回 `text/event-stream`，事件名 `inflight`，载荷为全量快照
    `{"seq": u64, "total": usize, "routes": [{"model_name": string, "count": u64}]}`；
  - 连接即发当前快照，此后每次计数变化再推一份全量快照（幂等：消费方整体替换，不做增量合并）；
  - 空闲每 15s 发一行 `: ping` 注释心跳；端点自身**不计数**（连接/断开不影响任何计数）；
  - 鉴权与其它 `/api/*` 完全一致：只认 `Authorization: Bearer`，`x-api-key` 不放行
    （该头只解锁 `/v1/messages`）；
  - `routes` 按 `count` 降序（同数按 `model_name` 升序，帧内容确定）、`total` 恒为各 `count` 之和、
    `count` 归零的 route 从快照中删除（因此快照只列出真正有在途请求的路由）。
- 无行为回退：既有端点、dispatch 行为、`call_logs` schema 均不变；新增计数只在进程内内存中。

## 计数口径

- **键 = 路由的 `model_name`**（`DispatchCtx::route_name`），与路由列表里看到的身份一致；
  不是请求里的 model（通配路由下二者不同）。
- **计数区间 = 整条派发生命周期**：`dispatch_chat` / `dispatch_image` 入口 `acquire`，早于隐私
  过滤与重试调度——退避 sleep 期间请求确实还在途。
- **释放只走 `Drop`**：非流式 / 错误 / panic 靠栈帧释放；流式把 guard 移交给响应体状态
  （`StreamState._in_flight`，三个协议出口各一次移交），因此**客户端断连（body drop）同样释放**。
  `Drop` 内不得 `await`，用 `std::sync::Mutex` + `watch::Sender::send_replace`（永不阻塞、
  无订阅者也不 panic）。
- **零计数即删键**：`HashMap` 里永不保留 `0` 值，所以「只显示在途 > 0 的路由」对消费方是免费的，
  被删除/改名的路由也不会留下残影（不需要额外接 prune）。
- **释放未知键只告警不 panic**：`Drop` 里撞到不存在的键说明有双重释放的 bug，打
  `tracing::warn!` 后按 no-op 处理——析构里 panic 会毁掉整个进程。

## 快照通道语义

`InFlightRegistry` 用 `watch::Sender<InFlightSnapshot>` 而非广播每条增量：突发的一批
acquire/release 会被合并成「最新快照」，因此

- 订阅者（SSE 端点）**不需要从增量重建状态**，重连的客户端由下一帧自愈，不需要单独的
  「拉全量」请求；
- 丢帧不会让消费方失步（每帧都是完整状态）；
- `seq` 在**同一把锁内**与 `counts` 一起递增，因此已发布快照的 `seq` 永不倒退。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-state/src/inflight.rs`](../../../../sag-state/src/inflight.rs) | 新增 `InFlightRegistry`（`std::sync::Mutex<InFlightInner>` + `watch::Sender<InFlightSnapshot>`）、`InFlightSnapshot` / `InFlightRouteCount`（`serde::Serialize`，字段名即载荷契约）、`InFlightGuard`（RAII，`Drop` 内 `saturating_sub` + 归零删键 + 发布 + 未知键 `warn!`）、`InFlight`（`Arc` 句柄别名）、`acquire` / `snapshot` / `subscribe`；`Mutex` 中毒时 `into_inner` 恢复而不是让后续请求全 panic |
| [`sag-state/src/lib.rs`](../../../../sag-state/src/lib.rs) | 新增 `pub mod inflight` 与 `pub use`（`InFlight` / `InFlightGuard` / `InFlightRegistry` / `InFlightRouteCount` / `InFlightSnapshot`）；`AppState` 新增 `in_flight: InFlight` 字段；`testing::test_app_state()` 补默认值 |
| [`sag-dispatch/src/state.rs`](../../../../sag-dispatch/src/state.rs) | `DispatchRuntime` 新增 `in_flight`；`with_shared_state` 增参，让 `AppState` 与 `DispatchRuntime` 持有**同一个 `Arc`**（两个实例会得到两份计数） |
| [`sag-dispatch/src/dispatch/chat/mod.rs`](../../../../sag-dispatch/src/dispatch/chat/mod.rs) | `dispatch_chat` 入口 `runtime.in_flight.acquire(route_name)`；三个流式出口（Anthropic / Responses / Chat 编码器）把 guard 移交给 `build_streaming_response` |
| [`sag-dispatch/src/dispatch/stream.rs`](../../../../sag-dispatch/src/dispatch/stream.rs) | `StreamState` 新增 `_in_flight: InFlightGuard`（声明在最后，释放晚于终态记账）；`make_stream_state` / `build_streaming_response` 增参接收并移交；`Drop` 自动释放，**不得手工释放** |
| [`sag-dispatch/src/dispatch/image.rs`](../../../../sag-dispatch/src/dispatch/image.rs) | `dispatch_image` 入口 `acquire`（image 恒非流式，guard 留在栈帧上） |
| [`sag-gateway/src/main.rs`](../../../../sag-gateway/src/main.rs) | 装配单一 `sag_state::InFlight`，同时注入 `AppState.in_flight` 与 `DispatchRuntime::with_shared_state` |
| [`sag-admin/src/admin_inflight.rs`](../../../../sag-admin/src/admin_inflight.rs) | 新增 `GET /api/inflight`：`stream_inflight`（`Sse` + `stream::unfold` 状态机 `Initial` / `Watching`，`INFLIGHT_EVENT = "inflight"`、`HEARTBEAT_INTERVAL = 15s`）、`inflight_event`（序列化失败降级为 comment 而不是 panic）、`inflight_stream` / `inflight_stream_with_heartbeat`；响应头补 `Cache-Control: no-cache, no-transform` 与 `X-Accel-Buffering: no`（不加 `Connection`，HTTP/2 禁止该逐跳头） |
| [`sag-admin/src/admin.rs`](../../../../sag-admin/src/admin.rs) / [`lib.rs`](../../../../sag-admin/src/lib.rs) | `build_admin_routes` 注册 `/inflight`（与其它 `/api/*` 一样由父路由的 `gateway_auth` 保护）；模块文档补条目与 `pub mod admin_inflight` |
| [`sag-admin/Cargo.toml`](../../../../sag-admin/Cargo.toml) | 新增 `futures-util` 依赖；dev-dependency 加 `sag-router`（集成测试要用真实 `gateway_auth` 包住 admin router） |
| [`sag-admin/tests/inflight_sse.rs`](../../../../sag-admin/tests/inflight_sse.rs) | 新增集成测试：鉴权（缺 Bearer → 401 / `x-api-key` 不放行）、连接即发快照、acquire/release 逐帧推进 `seq`、端点自身不计数、无限流不提前结束 |
| [`sag-dispatch/src/dispatch/chat/tests.rs`](../../../../sag-dispatch/src/dispatch/chat/tests.rs) | 新增 4 例：流式在 body 存活期间计数、`drop(body)` 即释放；非流式返回时已释放；错误路径不泄漏；多路由快照的排序与求和 |
| [`sag-router/src/routes.rs`](../../../../sag-router/src/routes.rs) / [`handlers/analytics_v2.rs`](../../../../sag-router/src/handlers/analytics_v2.rs) | 测试 fixture 的 `AppState` 补 `in_flight: Arc::default()` |
| [`docs/spec/admin/requirements.md`](../requirements.md) | 新增「在途请求观测」一节与变更记录条目 |
| [`docs/spec/admin/spec.md`](../spec.md) | 关键文件、关键决策（`watch` 全量快照 vs 增量事件）、不变式、测试策略、变更记录 |

## 测试

- `sag-state/src/inflight.rs`（内联）：acquire/drop 增减、同路由嵌套累积、归零删键只影响该路由、
  按 `count` 降序 + 同数按名字升序、`total` 为各 `count` 之和、每次发布 `seq` 递增、
  4 线程 × 250 次并发 acquire 的计数守恒（任何时刻不丢不泄漏）、`subscribe` 能收到每一次发布。
- `sag-dispatch/src/dispatch/chat/tests.rs`：见上表（4 例，含断连释放与错误路径不泄漏）。
- `sag-admin/tests/inflight_sse.rs`（集成，真实中间件栈）：401 契约与 `x-api-key` 不放行、
  连接帧是当前状态而非默认空快照、acquire/release 各推一帧且 `seq` 严格递增、
  body drop 不影响计数、流是无限流。
- `sag-admin/src/admin_inflight.rs`（内联）：帧编码（`event: inflight` + JSON）、
  首帧 = 连接时快照、空闲 20ms 心跳出 `: ping`、registry 被 drop 时流正常结束。
- 门禁：`just test-rust`（clippy + 单元/集成测试）。
