---
created: 2026-11-09
updated: 2026-11-09
type: 修复
---

# 0011 — 原生流式车道保留 SSE 注释帧

## 动机

两条原生透传流的读循环（`OpenAiProvider::responses_stream_native_inner`、`AnthropicProvider::messages_stream_native`）各自把「看起来像 SSE」的帧原样转发，判据只有 `frame.starts_with(b"data:") || frame.starts_with(b"event:")`；其余帧一律按「裸数据」补 `data: ` 前缀。SSE 注释帧（`:<text>`——上游 keepalive / heartbeat，如 `: OPENAI_PROCESSING`、`: ping`）既非字段行也不是裸数据，于是被改写成：

```
data: : keepalive
```

客户端把它解析成一个 data 事件，payload 是 `: keepalive`（非 JSON），注释语义彻底丢失；某些 SSE 客户端还会因「data 事件却无法解析」而报错或中断。注释与字段行同属合法 SSE 帧，必须原样透传。

帧边界本身没有问题：`sag-providers/src/util.rs::find_sse_boundary` 同时识别 `\n\n` 与 `\r\n\r\n`，注释帧会被正确切出来，缺陷只发生在切帧之后的包装步骤。

两条车道的调用链均在 dispatch 的直通车道（`needs_ir_decode == false`）：`sag-dispatch/src/dispatch/chat/mod.rs` 在 Anthropic 入口走 `anthropic_messages_stream` → `messages_stream_native`，Responses 入口走 `responses_stream_native` → `responses_stream_native_inner`，帧由 provider 自行转发，不经过 codec / IR，因此修复必须落在这两个读循环里。

## 需求变更

[`docs/spec/providers/requirements.md`](../requirements.md) 的「预期行为」此前没有任何子节定义原生透传流如何区分「SSE 帧」与「裸数据」——该契约只存在于实现里。本次新增子节「原生流式透传（SSE 帧边界）」：

- 以 `data:` / `event:`（字段行）或 `:`（注释行）开头的**完整帧**按 SSE 原样转发，不再二次包装
- 仅当完整帧既非字段行也非注释时才按裸数据补 `data: ` 前缀（既有兼容语义不变）
- EOF 尾包是例外：不足一个完整帧的残留带裸 JSON 尾包也原样发出、只补 `\n\n`，不加 `data: ` 前缀（既有行为，本次未改）
- LF 与 CRLF 帧边界都由 `find_sse_boundary` 识别；转发的帧统一以 `\n\n` 收尾（既有行为，本次未改）

未新增 `id:` / `retry:` 字段行的透传——它们不在本次缺陷范围内，留待需要时单独定义。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-providers/src/openai.rs`](../../../../sag-providers/src/openai.rs) | `responses_stream_native_inner` 的帧包装判据移到 `wrap_responses_native_frame`：增加 `frame.starts_with(b":")`，注释帧原样转发并补 `\n\n`，裸数据兼容分支不变；切帧 + 包装从读循环抽到同文件的 `drain_responses_native_frames`（纯字节 buffer → 帧，读循环只负责 append） |
| [`sag-providers/src/anthropic.rs`](../../../../sag-providers/src/anthropic.rs) | `messages_stream_native` 同一处判据同步为 `wrap_anthropic_native_frame` / `drain_anthropic_native_frames`，注释帧与字段行同等处理 |
| [`sag-router/tests/protocol_matrix.rs`](../../../../sag-router/tests/protocol_matrix.rs) | 新增两条客户端可见响应体回归：`anthropic_native_passthrough_preserves_sse_comments` / `responses_native_passthrough_preserves_sse_comments` |

两处改动是同一判据的对称修改，没有抽跨协议的公共 helper：两条车道的旁路提取闭包（`extract_side_channel` / `extract_usage`）与 trace 文案不同，各自留一份 `drain_*` / `wrap_*` 反而更小。抽出的 drain 是测试缝：读循环只做 append，因此「逐片喂字节」与真实读循环的切分行为等价。

### 未改动的相邻行为

| 位置 | 现状 | 说明 |
|------|------|------|
| 两个读循环的 EOF 尾包 | `buffer` 非空时原样补 `\n\n` 后发出，**不加** `data: ` 前缀 | 尾包天然不会二次包装注释；裸 JSON 尾包缺 `data: ` 前缀是既有行为，本次未触碰 |
| `process_sse_frame`（Chat 车道） | 只转发 `data:` 行，注释被丢弃（不会被改写） | 不在本缺陷范围；Chat 车道的注释策略如需定义，属另一项变更 |
| 帧内多行 | `data:` / `event:` 开头的多行帧本就整帧透传 | 注释在前、`data:` 在后的多行帧现在也整帧透传（旧实现会包装整帧并破坏它） |
| side-channel 提取 | `extract_side_channel` / `extract_usage` 只看 `data:` 行 | 注释帧没有 `data:` 行，仍走「无副作用」路径，不会被误判为错误帧或 usage 帧 |

## 测试

每条车道两例（layer 内的 provider 单测）+ 一条 router 集成例：

1. `openai::tests::responses_stream_native_preserves_sse_comment_frames` / `anthropic::tests::messages_stream_native_preserves_sse_comment_frames` — 完整provider 路径（`spawn_server` 一次写入整段 fixture）：注释帧、CRLF 注释帧、CRLF 事件帧、无空行结尾的 `: tail`（走 EOF 尾包路径）。断言 provider 转发出的 `StreamChunk::Data` 字节拼接逐字等于「`: keepalive\n\n` + 原事件 + `: ping\n\n` + 原事件 + `: tail\n\n`」，不含 `data: :`，且 EOF 仍发 `Done`。这一层只能看到 provider 的转发字节（`StreamChunk::Data`），看不到客户端 body——客户端字节的断言在第 3 条 router 用例。一次写入意味着断言与 socket 切分无关（确定性），但因此**不**能证明跨 chunk 行为。
2. `openai::tests::responses_native_frame_drain_ignores_chunk_boundaries` / `anthropic::tests::messages_native_frame_drain_ignores_chunk_boundaries`（普通 `#[test]`，无网络、无 sleep）— 确定性地掰开分片：逐片喂 `: keep` + `alive\n` + `\n`、`data: {…` + `}\n` + `\n`、`: pi` + `ng\r\n\r\n` 等，逐片断言「发出哪些帧」（分隔符未到 → 一个字节也不发）。TCP flush + sleep 无法保证 hyper 不把两次写入合并成一个 chunk，所以跨 chunk 行为只能在这层钉。
3. `protocol_matrix.rs::{anthropic,responses}_native_passthrough_preserves_sse_comments` — 客户端可见响应体：mock 上游发注释 + 正常事件，断言 gateway 返回的整条 body 与 fixture 逐字节相等，且注释各出现一次、无 `data: :`。provider 单测只看到 `StreamChunk::Data`，这条把 IR 桥与响应体写出链路一起钉住。

**覆盖边界（两条 router 用例共同）**：用例的 gateway state 用 `sag_config::gateway_config_base!()`，其中 `privacy` 为 `PrivacyConfig::default()`（`enabled = false`）。dispatch 只在 `privacy.enabled` 为真时才生成 `privacy_map` / `RestoreWriter`，故这两条用例的 `privacy_restore` 是 `None`——它们**不**覆盖 privacy restore 路径（restore 自身对注释帧的透传由 `sag-privacy` 的既有用例覆盖），也正因如此，privacy 侧的重包装不会让它们失败。

**Anthropic fixture 的 `output_tokens: 1`**：`message_start` 里写 `output_tokens: 1` 是为了让这条流通过既有的 empty-peek 判定——`validate_stream_first_chunk` 对 `Usage` 事件按 `completion_tokens == 0 && prompt_tokens > 0` 判空，而 native Anthropic 的 `message_start` 会被 provider 的 usage 侧通道先转成 `Usage` 事件，上游首帧若报 `output_tokens: 0` 就会命中该判空（提交前 502）。这个判空是**既有**行为，既不属于本次 SSE 注释修复的范围，本次也**未**修复它；本用例只覆盖「绕过该判空之后」注释字节的保真，真实 `output_tokens: 0` 起始流的边界需要另立用例与变更记录。

修复前两例（1、3）都会失败（实际输出 `data: : keepalive` / `data: : ping`）；既有用例无一翻转（`cargo test -p sag-providers --lib` 314 例、`cargo test -p sag-router --test protocol_matrix` 26 例全绿）。
