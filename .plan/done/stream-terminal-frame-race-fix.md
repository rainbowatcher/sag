# 流式终点帧竞态误分类修复 — 执行计划

> status: done · created: 2026-10-12 · revised: 2026-10-22 · completed: 2026-10-22 · target: `.plan/done/stream-terminal-frame-race-fix.md`
>
> 已实施：`d3cf9b2`(fix) · `9151428`(docs) · `f6b8529`(Minor 收尾 fix) · `f1c6624`(docs 返工)，评审 ACCEPT、门禁全绿；变更记录见 `docs/spec/dispatch/changes/0007-stream-terminal-frame-race.md`。

## 1. 背景与生产证据

### 1.1 现象

2026-10-12 当天 5 起流式请求被 SAG 记为 499（`client disconnected before stream completion`），
但经与 pi 客户端 session 文件逐毫秒比对，**客户端侧全部正常完成**：

| SAG 时间 | provider | duration_ms | pi 侧实际结果 |
|---|---|---|---|
| 08:05:17 | volc/glm-5.3 | 4521 | `stopReason="toolUse"`，工具调用块完整 |
| 08:09:10 | volc/glm-5.3 | 4279 | `stopReason="toolUse"`，内容块完整 |
| 09:23:02 | codebuddy/deepseek-v4.1 | 4217 | `stopReason="toolUse"` |
| 09:33:05 | codebuddy/deepseek-v4.1 | 4996 | `stopReason="stop"`，最终答复完整送达 |

pi 侧 `durationMs` 与 SAG 侧逐毫秒吻合（4996↔5000、4573↔4573、4343↔4343），证明响应
**完整送达且被客户端消费**，SAG 却记为「客户端断连」。

历史上该模式高频出现（10-05: 74 条、10-06: 231 条、10-08~10-10 各 100~377 条 499 中相当比例为此形态），
显著污染成功率与 499 统计。

### 1.2 形态特征（可与真实断连区分）

- duration 落在 4.2~5.0s（≈ 模型完整生成时间），且当天同 route 的 200 与 499 交替出现
  （07:44:11 200 → 07:44:34 499 → 07:44:44 200）——典型竞态的非确定性表现；
- `call_logs` 行 `ttft_ms`/`usage` 全 NULL（Drop 路径不携带），而客户端实际收全了 usage 帧。

## 2. 竞态机制（修订版）

### 2.1 正常完成的事件序列

以 OpenAI-compatible 上游（native 透传与 translated lane 相同骨架）为例：

```
provider task (sag-providers/src/openai.rs + util.rs process_sse_frame)
  → StreamChunk::Data(内容帧...)        —— 转发客户端
  → StreamChunk::Usage(u)               —— unfold 内联合并（usage 帧，在 [DONE] 之前）
  → StreamChunk::Data(b"data: [DONE]\n\n")   ← process_sse_frame（util.rs:240-241，[DONE] 以 Raw Data 转发）
  → StreamChunk::Done                   ← openai.rs:264（EOF 后发送）→ IrStreamEvent::End
```

dispatch 侧 unfold 循环（`sag-dispatch/src/dispatch/stream.rs:845-880`）：

```rust
match state.stream.next().await {                                  // :845
    Some(IrStreamEvent::Usage(u)) => { merge_stream_usage(...); }        // 内联
    Some(IrStreamEvent::Error(msg)) => { record_stream_error(...); }     // 内联
    Some(IrStreamEvent::End) | None => { finish_stream_terminal(...); }  // 成功终点
    Some(ev) => { /* encode → yield 给 hyper */ }                        // 客户端可见帧
}
```

**关键点**：`End` 控制帧从不编码转发（`stream.rs:940` 注释「`End` is never encoded
by the loop」；codec 里 `End => message_stop` 的分支因此不可达）；客户端可见的终点帧
`data: [DONE]\n\n` 是 provider 层在 `End` **之前**作为 `Raw` 帧转发的。两者之间隔着
provider task 的「upstream EOF 检测」（通常 <1ms，但受上游 FIN 尾延迟影响可达数百 ms）。

### 2.2 竞态窗口（重写）

上一版计划把窗口描述为「`End` 已在 channel 中却永远不被 poll」，这与 hyper 1.11.1 的
实际行为不符。`src/proto/h1/dispatch.rs` 的 `poll_write` 循环：

```
loop {
    match body.poll_frame() {            // 拿到一帧（例如 [DONE]）
        Ready(Some(frame)) => {
            self.conn.write_body(chunk); // 仅写入发送缓冲，客户端此刻尚未收到
            continue;                    // ← 同一次 task poll 内立即再 poll body
        }
        ...
    }
}
```

因为 `continue` 发生在同一次 poll 内，若 `End` 已经入队（`Ready`），unfold 的
`loop { state.stream.next().await }` 本来就会同步消费它并 claim 正常终点——**发生在
客户端收到 `[DONE]` 之前**。所以「`End` 已在 channel 中却不被 poll」的窗口不存在。

真实的唯一窗口是：**`End` 尚未入队**。`StreamChunk::Done` 由 provider 任务在探测到上游
EOF 之后才发送（openai.rs:264，`if !had_error { tx.send(StreamChunk::Done) }`），而上游
FIN 尾延迟可达数百 ms。此间客户端已收到 `[DONE]`（OpenAI 协议语义即流结束）并关闭连接，
hyper 在后续 `poll_read` 中探测到断连并 drop body → `TerminalGuard` 在 `End` 到达前触发
→ 误判 499。

```
provider:   ... ── send Raw("[DONE]") ──┬── (等待上游 EOF / FIN 尾延迟 10~500ms) ── send Done
                                        │                                          │
unfold:     yield "[DONE]" ─────────────┤  （此刻 channel 为空，next() 为 Pending） │
                                        │                                          │
hyper:      write_body("[DONE]") ── 客户端收到并关闭连接                            │
                                        │                                          │
hyper:      poll_read → 断连 → drop body                                           │
                                        ▼                                          ▼
TerminalGuard::drop：done=false, error=false, 终点帧标志=false → 499 ❌        （永远到不了）
```

结论：修复必须建立在「终点帧是否已经交给 hyper」这一**同步可判定的标志**上，
而不是靠调整 poll 顺序去抢在客户端前面消费 `End`。

### 2.3 同类问题的既有先例

`process_sse_frame`/`forward_error_frame`（`sag-providers/src/util.rs:279-301`）的注释
记录过**同构竞态**：「client disconnects immediately after receiving the `Data` frame,
the stream is dropped without polling the `Error` chunk」——当时的修法是调整发送顺序
（`Error` 先于 `Data` 入队）。正常完成路径没有等价的顺序处理可做（`End` 天然在 `[DONE]`
之后，且要等上游 EOF），需要从终点判定层面修复。

## 3. 修复设计

### 3.1 语义目标

重新定义 499 的判定边界：

| 场景 | 修复前 | 修复后 |
|---|---|---|
| 上游正常完成、终点帧已转发、客户端随后断开 | 499 ❌ | **200 Success**（usage/ttft 完整）|
| 上游正常完成、终点帧已转发、正常 EOF | 200 | 200（不变）|
| 流中途（终点帧之前）客户端断开 | 499 | 499（不变）|
| 流中途上游错误后客户端断开 | 502 | 502（不变）|

判定依据是**确定性的**：客户端可见终点帧（Chat `[DONE]` / Anthropic `message_stop` /
Responses `response.completed` 或 `response.incomplete`）是否已经转发给 hyper。断连只能
发生在帧写出之后，而标志位在帧 yield 前同步设置，因此不存在「断连先于标志」的窗口。

判定口径明确为：**终点帧已交给 hyper 即算成功**。TCP 没有应用层交付确认，hyper 的
`write_body` 只保证写入发送缓冲；我们接受这一口径——它比「客户端确认收到」更宽松，
但比现状（把完整送达的请求记成 499）严格得多，且是本地唯一可同步观测的信号。

### 3.2 Fix（唯一）：终点帧标志 + 共享态 + Drop 三路分类

**① 共享态扩展**（`StreamAnalytics`，`stream.rs:21-73`，供 unfold 与 Drop guard 共同读写）：

| 字段 | 类型 | 作用 |
|---|---|---|
| `terminal_frame_forwarded` | `AtomicBool` | 客户端终点帧已 yield 给 hyper |
| `usage` | `Mutex<Usage>` | usage 合并从 `StreamState` 迁入共享（当前 Drop 路径拿不到，这是 499 行 usage 全 NULL 的根因）|
| `first_byte_at` | `Mutex<Option<Instant>>` | TTFT 同理迁入共享 |
| `upstream_start` | `Instant` | 由 `StreamState` 迁入（Drop-as-success 要写 `upstream_duration_ms`）|

`StreamState`（`stream.rs:375-401`）中对应字段删除，unfold 改为读写共享态。

**② 判定位置：`encode` 之后、yield 之前的字节上**（`stream.rs:845-880` 的 `Some(ev)` 分支）：

```rust
Some(ev) => {
    let bytes = (state.encode)(&ev);        // 结构化帧在这里被合成为客户端协议形态
    if bytes.is_empty() {
        continue;                           // 0 字节不是首字节，也不是终点帧
    }
    if analytics.first_byte_at.lock().unwrap().is_none() {
        *analytics.first_byte_at.lock().unwrap() = Some(Instant::now());
    }
    if is_client_terminal_frame(&bytes) {   // ← 判定跑在编码后的字节上
        analytics.terminal_frame_forwarded.store(true, Ordering::SeqCst);
    }
    return Some((Ok(Bytes::from(bytes)), state));
}
```

判定必须跑在 `encode` 之后：Responses translated lane 的客户端终点帧是 `Finish` 结构化
事件经 `responses_stream_encoder` 合成的 `data: {"type":"response.completed",...}`
（`sag-providers/src/codec/responses.rs:1278-1293`，status 还可能是
incomplete/cancelled/failed），不是 `Raw` 帧。只看 `Raw` 会漏掉整条 translated lane。

**③ 终点帧判定 helper**（`sag-core`，与 `ApiKind` 无关，**三形态并集匹配**）：

```rust
/// 客户端可见 SSE 帧是否为流终点帧。与 ingress 无关：Chat ingress 也可能
/// 透传 Responses 的 `response.completed`（Responses upstream 的 Raw 透传），
/// 因此这里做三协议形态的并集匹配，不做 ingress 收窄。
/// - `data: [DONE]`                                （Chat 终点 / 桥接合成）
/// - `"type":"message_stop"`                       （Anthropic 终点）
/// - `event: response.completed` / `"type":"response.completed"`（Responses 终点，
///   含 translated lane 由 Finish 合成的形态）
/// - `"type":"response.incomplete"`                （Responses 流终点，usage 侧信道
///   已按 completed/incomplete 提取，见 openai.rs:431）
pub fn is_client_terminal_frame(frame: &[u8]) -> bool
```

字节子串匹配，O(1) 量级；不解析 JSON，避免在热路径上做反序列化。

> 与 `sag-dispatch/src/dispatch/chat/empty.rs` 的 `RawVerdict` 终点知识
> （`empty.rs:204` 枚举、`:299` 的 `message_stop`、`:307` 起的 `responses_raw_verdict`）
> 存在重复。本计划不强制大重构：**若复用同源成本低则复用，否则至少交叉引用注释，
> 并在两侧各用测试锚定**，避免两份知识静默漂移。

**④ `TerminalGuard::Drop` 三路分类**（`stream.rs:250` 起的 guard，Drop 在 `:254`）：

```text
done 已被正常终点 claim            → no-op（不变）
error=true                        → model_failure / 502（不变）
error=false && terminal_frame_forwarded=true
                                  → FinalStatus::Success：写共享 usage / ttft /
                                    upstream_duration（`upstream_start` 起算），
                                    call_logs 行 200，error_category 空，
                                    info 级日志 "client closed after terminal frame;
                                    classified as success"
error=false && terminal_frame_forwarded=false
                                  → client_cancelled / 499（不变，真中途断连）
```

`error` 的优先级高于 flag：上游在终点帧之后仍发 `Error`（或 `response.failed`）时，
依然记 502，不会被 flag 升级为成功。

不采用「last_forward_at 时间窗」启发式——持续流式输出中任意时刻断连都满足
「最近几 ms 内有转发」，无法区分中途断连与终点竞态；终点帧标志是唯一确定性判据。

### 3.3 Anthropic translated 桥接补 `message_stop`

现状缺口：Anthropic ingress + chat/responses upstream 时，`chat/mod.rs` 的桥接只处理
反方向（`is_chat_ingress && use_anthropic` → 首个 `End` 改写为 `data: [DONE]`，
`chat/mod.rs:377-394`）。而 Anthropic ingress 下，`End` 从不被 encode
（`stream.rs:940`；`anthropic.rs:1073` 的 `End => message_stop` 分支不可达），
客户端**永远收不到 `message_stop`** —— 这是协议缺口，不是仅影响统计的问题。

修法：镜像既有桥接，对 `is_anthropic_ingress && !use_anthropic` 的流，把首个 `End`
改写为 `Raw(b"data: {\"type\":\"message_stop\"}\n\n")`，后续 `End` 正常终止循环：

```rust
let s = if is_anthropic_ingress && !use_anthropic {
    let mut stop_sent = false;
    s.map(move |ev| match ev {
        IrStreamEvent::End if !stop_sent => {
            stop_sent = true;
            IrStreamEvent::Raw(b"data: {\"type\":\"message_stop\"}\n\n".to_vec())
        }
        other => other,
    })
    .boxed()
} else {
    s
};
```

收益双重：① 修复「translated lane 的 Anthropic 客户端缺终点帧」的协议缺陷；② 合成帧
天然命中 §3.2 的终点帧判定（`"type":"message_stop"`），该路径的竞态 drop 也归为成功。
native anthropic 透传路径不受影响（Raw 字节恒等保留，上游自带 `message_stop`）。

### 3.4 为何不需要贪婪 drain

上一版的 Fix A（每帧 yield 后非阻塞排空 channel 中 `Ready` 事件）已删除，理由：

1. hyper 的 `poll_write` 在 `write_body` 后 `continue`，**同一次 task poll 内**会立即
   再 poll body，因此 `End` 只要在 channel 里（`Ready`），unfold 的 `loop { next().await }`
   必然同步消费它——发生在客户端收到 `[DONE]` 之前。drain 想抢的那段窗口本来就不存在。
2. 真实窗口是 `End` **尚未入队**（上游 EOF / FIN 尾延迟），drain 只排空 `Ready` 事件，
   对未入队事件零效果。
3. 控制流上也无法实现：`return Some((bytes, state))` 之后 closure 立即结束，把 drain
   放到下一次 closure 顶部与现状语义等价。

因此只保留 §3.2 的终点帧标志方案：它在帧 yield 前同步置位，天然没有竞态窗口。

### 3.5 非目标

- 不改变 pre-commit 的 `DispatchCancelGuard`（`dispatch/mod.rs:645`，Drop 在 `:766`）路径与 499 语义；
- 不改变 502 / `stream_error_after_commit` 分类（`forward_error_frame` 顺序保证保留）；
- 不引入配置开关——这是分类语义的 bug 修复，非行为可选特性；
- 不处理类型 B（volc 挂起 + 代理隧道死亡）问题，那属于上游健康度/熔断范畴；
- 不修改 `call_logs` schema（成功行复用既有字段，NULL 语义沿用 success 行约定）；
- `response.failed` 不算终点帧——它的 `Error` 帧已走 502 路径，`error` 优先于 flag。

## 4. 变更清单

| 文件 | 变更 |
|---|---|
| `sag-core/src/protocol.rs`（`ApiKind` 旁，:39） | 新增 `is_client_terminal_frame(frame: &[u8]) -> bool`（三形态并集匹配，与 `empty.rs` `RawVerdict` 同源或交叉引用）+ 三协议形态单元测试 |
| `sag-dispatch/src/dispatch/stream.rs` | ① `StreamAnalytics`（:21-73）增 `terminal_frame_forwarded` / `usage` / `first_byte_at` / `upstream_start`，`StreamState`（:375-401）相应迁移；② unfold（:845-880）encode 后、yield 前判定并置位；③ `TerminalGuard::Drop`（:254 起）三路分类；④ Drop-as-success 路径构造完整 `CallLog`/`terminal_event`（usage/ttft/upstream_duration 取共享值）|
| `sag-dispatch/src/dispatch/chat/mod.rs` | 新增 `is_anthropic_ingress && !use_anthropic` 的 `message_stop` 桥接（镜像 :377-394 的 `[DONE]` 桥接）|
| `sag-dispatch/src/dispatch/stream.rs` tests | §6 的 1~4 |
| `sag-dispatch/src/dispatch/chat/tests.rs` | §6 的 5~6（桥接回归）|

预计 diff ~150-200 行（含测试），不含任何 drain 逻辑，全部集中在两个 crate。

## 5. 不变式

1. **单一终点**：`done` 原子 claim 语义不变——正常终点、Drop guard 至多一方生效；
2. **failure 优先于 cancel**：`error=true` 恒为 `model_failure`（现行为，保留），且优先于
   终点帧标志；
3. **499 语义收紧**：仅「终点帧转发前」的客户端断连记 499；转发后的断连是正常完成
   （客户端收到 `[DONE]` 后关闭连接是 OpenAI 协议的标准行为，非异常）；
4. **字节恒等**：native 透传路径的客户端可见字节恒等；唯一例外是 translated 桥接新增的
   `message_stop` 合成帧（与既有 `[DONE]` 桥接同例，属协议合规修复）；
5. **success 行数据完整性**：Drop-as-success 行的 usage/ttft/duration 必须与非竞态
   成功行同等完整（取共享态最终值）；
6. **只读判定**：终点判定只读 `encode` 后的字节，不修改、不重排任何帧的转发顺序。

## 6. 测试计划

单元测试（`stream.rs` 内联 `mod tests`，复用 `build_state_with_event_pool` fixture）：

1. `race_client_drop_after_done_classifies_success`：
   流 = `[Raw(delta), Usage(u), Raw("data: [DONE]\n\n")]`，`End` **延迟到达**
   （可控时序 fixture：用 `poll_fn` 控制相位，或手动向 mpsc 送事件——`stream::iter`
   全 Ready 不够，那会让 `End` 在客户端 drop 前就被消费，测不到窗口）→ 在 `[DONE]`
   yield 后 drop body → call_logs 行 200、usage=u、ttft / upstream_duration 非 NULL；
2. `midstream_disconnect_still_499`：流 = `[Raw(delta), Raw(delta), ...]` 中途 drop →
   499 保持，usage/ttft NULL；
3. `upstream_error_then_drop_still_502`：`Error` 帧后 drop → `model_failure`/502 保持；
4. `terminal_frame_detection_forms`：三协议 Raw 形态命中（`data: [DONE]` /
   `data: {"type":"message_stop"}` / `event: response.completed` +
   `data: {"type":"response.completed",...}`）；Responses translated 合成形态
   （`Finish` 经 `responses_stream_encoder` 的编码产物）命中；`response.incomplete` 命中；
   普通 delta 帧、`"stop_reason":null` 的 `message_delta` 不命中。同时断言判定作用于
   **encode 后**字节（对 `Finish` 事件走 `encode` 再判定）；
5. `anthropic_bridge_done_synthesis_race`（`chat/tests.rs`）：anthropic 上游桥接 Chat
   ingress，模拟 2.2 窗口 → 200 成功；
6. `anthropic_translated_message_stop_bridge`（`chat/tests.rs`，新增）：anthropic ingress +
   chat upstream → 首个 `End` 合成 `data: {"type":"message_stop"}`，后续 `End` 正常终止；
   模拟竞态 drop → 200；
7. 原 `greedy_drain_consumes_ready_end_inline` 测试**删除**：Fix A 已移除，且该用例在
   现状（无 drain）下同样通过，没有区分度。

回归验证：

```bash
just test-rust                             # Rust 单元 + 集成（跳过 doc test）
just lint                                  # clippy + 前端 lint
just build-test && just test-env restart    # release 轨验证
```

线上验证（test-env）：跑一轮真实 pi 会话（会产生终点帧竞态形态的请求），确认
`call_logs` 中不再出现 duration≈4~5s、ttft/usage 全 NULL 的 499，且该批请求呈 200。

## 7. 风险与权衡

- **统计口径跳变**：修复后 499 计数下降、成功计数上升（历史上每天几十~几百条），
  dashboard 曲线会有一次性台阶。需在 record-change 文档中注明，供运营对照；
- **Drop-as-success 的 usage 精度**：依赖 Usage 事件在终点帧之前入队的 provider 顺序
  （OpenAI/Anthropic/Responses 均满足：usage 帧在 [DONE]/message_stop/
  response.completed 之前）。极端乱序 provider 理论上可产生 usage=0 的成功行，
  与现状（499 行 usage=NULL）相比仍是改进；
- **终点帧匹配的脆弱性**：采用三形态并集匹配 + 合成形态覆盖（translated lane 的
  `response.completed` 由 codec 合成，形态由 codec 代码定义），由 `sag-core` 单测与
  codec/stream 测试双重锚定；provider 侧终点形态变化会先在 codec 测试暴露。
  残余风险是上游自定义终点帧（如非标准 `event:` 名）不被识别 → 退化为 499，与现状一致，
  不会产生错误成功；
- **anthropic 桥接新增 `message_stop` 是客户端可见字节变化**（translated lane 多一帧）：
  对 anthropic-protocol 客户端而言这是**协议合规修复**（此前缺终点帧）；native 透传路径
  不受影响（Raw 字节恒等）。需在 record-change 注明；
- **成功判定口径**：以「终点帧已交给 hyper」为准，而非「客户端确认收到」。TCP 无应用层
  确认，这是本地唯一可同步观测的信号；接受该口径的误差（客户端在收到后立刻断网仍记 200）。

## 8. 后续（超出本计划）

- 类型 B（上游 TTFB 挂起 + 本地代理隧道死亡）：熔断信号纳入「长时间无首字节且客户端
  已消失」，及 `no_proxy` 指引（运维侧）；
- `terminal_frame_forwarded` 可进一步导出为 analytics 维度（`client_closed_after_done`），
  便于持续观测该形态占比——独立小任务。

## 9. 修订记录（2026-10-22 评审）

评审对照 hyper 1.11.1 源码（`src/proto/h1/dispatch.rs` 的 `poll_write`）与当前代码，
对本计划做了如下修订：

| # | 修订点 | 结论 |
|---|---|---|
| 1 | **删除 Fix A（贪婪 drain）** | hyper 在 `write_body` 后 `continue` 同 poll 重查 body，`Ready` 的 `End` 必被同步消费；真实窗口是 `End` 未入队（上游 EOF/FIN 尾延迟）；drain 只排空 `Ready`，对未入队事件无效；unfold 在 `return Some(...)` 后无法执行代码 |
| 2 | **判定覆盖面修正** | 原设计只查 `Raw` 帧，会漏 Responses translated lane 的 `Finish`→`response.completed` 合成帧；改为跑在 `encode` 之后的字节上 |
| 3 | **匹配形态修正** | 原设计按 ingress 分派，Chat ingress + Responses upstream（Raw 透传 `event: response.completed`）不命中；改为三形态并集匹配、不做 ingress 收窄，并把 `response.incomplete` 纳入终点 |
| 4 | **共享态补 `upstream_start`** | Drop-as-success 要写 `upstream_duration_ms`，故 `upstream_start` 也从 `StreamState` 迁入共享 `StreamAnalytics` |
| 5 | **测试修正** | 原 test 2（`greedy_drain_consumes_ready_end_inline`）无区分度（无 drain 也通过）→ 删除；终点判定测试扩展为覆盖合成形态与反例；竞态用例改为可控时序 fixture（`stream::iter` 全 Ready 测不到窗口） |
| 6 | **行号校正** | unfold 循环 `stream.rs:845-880`、`End` 不编码注释 `stream.rs:940`、`[DONE]` 桥接 `chat/mod.rs:377-394` |

用户决策：

- **删除 Fix A**，只做终点帧标志 + Drop 三路分类（判定在 yield 前同步置位，无竞态窗口）；
- **补 anthropic translated 桥接的 `message_stop`**：对齐既有 `[DONE]` 桥接先例，同时修复
  协议缺口（translated lane 的 Anthropic 客户端此前永远收不到终点帧）并提供终点判定依据。
