---
created: 2026-10-09
updated: 2026-10-09
type: 修复
---

# 0006 — Chat 入口容忍缺失 `content` 的消息（assistant tool_calls 形态）

## 动机

zcode 等客户端在多轮工具调用回放时，会发送只携带 `tool_calls`、完全省略 `content` 键的 assistant 消息。这在 OpenAI 规范中合法（`content` 为 "Required unless tool_calls or function_call is specified"），但网关 `/v1/chat/completions` 入口用强类型 `Json<ChatCompletionRequest>` 反序列化请求体，`ChatMessage.content: serde_json::Value` 无 `#[serde(default)]`，字段缺失直接被 serde 拒绝，客户端收到：

```
422 Unprocessable Entity: Failed to deserialize the JSON body into the target type:
messages[8]: missing field `content` at line 1 column 33909
```

下游其实早已兼容该形态：`codec/chat.rs` 的 `decode_content` 将 `Value::Null` 解码为空内容块，`tool_calls` 从 `extra` 扁平字段独立解码，反向 `encode_content` 把空块序列化回 `Value::Null`（正是 OpenAI 自家对 assistant tool_calls 消息的标准输出）。入口拦截纯属误伤，且违背网关「容忍未知/缺失形态而非报错拒绝」的设计原则。

## 需求变更

- [`sag-core/src/types.rs`](sag-core/src/types.rs) `ChatMessage.content` 注释新增 wire 契约说明：字段在 wire 上可选，缺失反序列化为 `Null`；显式 `null` 与缺失等价。

（`docs/spec/providers/requirements.md` 行为节无实质变化——codec 对 `content: null` 的既有兼容行为未改动，仅入口解析不再提前拒绝。）

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-core/src/types.rs`](sag-core/src/types.rs) | `ChatMessage.content` 加 `#[serde(default)]`（缺失 → `Value::Null`），补 wire 契约注释 |
| [`sag-router/tests/chat_bytes_regression.rs`](sag-router/tests/chat_bytes_regression.rs) | 新增两个回归测试 + `spawn_capturing_server` 捕获型 mock upstream |
| [`sag-dispatch/src/dispatch/image.rs`](sag-dispatch/src/dispatch/image.rs) | 顺带修复存量 clippy `-D warnings` 失败：`MockImageProvider::last_api_endpoint` 从未被使用，在成功路径测试补上 api_endpoint 透传断言 |

wire 兼容性说明：native OpenAI 透传路径 `serde_json::to_value(&upstream_request)` 重新序列化，缺失 `content` 的消息以上游收到 `"content": null` 转发——即 OpenAI 自身对 assistant tool_calls 消息的规范输出形态，不丢键也不改语义。

## 测试

- `sag-core/src/types.rs` 内联单元测试：
  - `chat_message_missing_content_deserializes_to_null` — assistant + `tool_calls` 无 `content` 键 → `Null`，`tool_calls` 保留在 `extra`
  - `chat_message_missing_content_reserializes_as_content_null` — 缺失 → 重新序列化为 `"content": null`
  - `chat_completion_request_tolerates_message_without_content` — 完整请求形态回归
- `sag-router/tests/chat_bytes_regression.rs` 集成测试（真实 handler 路径）：
  - `chat_assistant_tool_calls_without_content_passes_ingress` — 修复前 422 "missing field"，修复后通过入口进入路由解析（unknown model → 404 MODEL_NOT_FOUND 证明 gate 通过）
  - `chat_message_without_content_forwards_content_null_to_upstream` — 捕获 mock upstream 收到的请求体，断言 `messages[1].content == null` 且 `tool_calls` 数组完整透传
