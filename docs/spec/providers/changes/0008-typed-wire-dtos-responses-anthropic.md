---
created: 2026-10-26
updated: 2026-10-26
type: 修改
---

# 0008 — Responses / Anthropic 请求响应体 typed 化（无损透传）

## 动机

三个 codec 里只有 `codec/chat.rs` 吃 typed DTO（`ChatCompletionRequest` / `ChatCompletionResponse`），`codec/responses.rs` 与 `codec/anthropic.rs` 全程在 `serde_json::Value` 上工作。这条不对称带来三类成本：

1. **字段访问没有编译期约束**：`body.get("max_output_tokens").and_then(Value::as_u64)` 这类读法，键名拼错或类型判漏都不会报错，只能靠测试兜底。
2. **未建模字段的往返靠两处手工维护**：旧实现用「`KNOWN` 白名单 + 逐字段特判拷贝」组合决定哪些键进 `ir.raw`——白名单（Responses 15 项 / Anthropic 13 项）列出已建模字段，不进 `raw`；`parallel_tool_calls` / `text` / `reasoning` 等再单独写一段 `raw.insert(...)`。新增一个字段必须同时改两处，漏改任一处就是静默丢字段，而白名单本身不表达类型。
3. **多态联合只能手写 tag 分派**：Responses 的 `input` / `output` / `tools` / `tool_choice` 与 Anthropic 的 15 种 content block 都在 `Value` 上 `match` `type` 字符串，新增变体没有任何检查。

## 需求变更

[`docs/spec/providers/requirements.md`](../requirements.md)：

- 「预期行为」新增子节「上游协议请求/响应体 typed 化」，定义 8 条行为契约：DTO 只做数据建模不含映射逻辑、未建模字段经 `extra` 无损透传、多态联合未知 tag 逐字透传、`ir.raw` 仍双向承载无 IR 归宿的字段、语义校验先于 typed 解析（422 顺序与消息不变）、SSE 流式不 typed 化、上游响应降级无损（含 usage 按字段降级）、容错输入保留（`role` 缺失或非字符串按 `"user"` 兜底）。
- 末尾「变更记录」节追加 0008 条目。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-core/src/api/mod.rs`](../../../../sag-core/src/api/mod.rs) | 新增。`sag_core::api` 模块声明 `anthropic` / `responses` 两个子模块，模块文档固定三条约定（无 `deny_unknown_fields`、判别式用不校验的 `String`、每个联合有 `Unknown(Value)` 兜底） |
| [`sag-core/src/api/anthropic.rs`](../../../../sag-core/src/api/anthropic.rs) | 新增。Anthropic Messages API 的 75 个公开类型（57 struct / 17 enum / 1 type alias）：`AnthropicMessagesRequest` / `AnthropicMessagesResponse` / 15 种 `AnthropicContentBlock` 变体 / `tool_choice` 4 变体 / `thinking` 3 变体 / 嵌套 `usage`；`Extra` 类型别名 + `deserialize_tagged` 按 `type` 分派、未知 tag 与不可解析载荷落 `Unknown(Value)`；`temperature` / `top_p` / `top_k` 虽已被上游标记弃用仍保留 typed（附弃用注释，协议翻译链需要） |
| [`sag-core/src/api/responses.rs`](../../../../sag-core/src/api/responses.rs) | 新增。OpenAI Responses API 的 145 个公开类型（122 struct / 23 enum）：`ResponsesRequest`（`CreateResponse` 全字段）、`ResponsesResponse`（33 字段）、`InputItem` 30 变体、`OutputItem`、`ResponsesTool` 16 种、`ResponseUsage`；`type_tagged_union!` 宏生成按 `type` tag 精确分派的反序列化（纯 `untagged` 会被「全可选」变体吞掉），已知 tag 解析失败同样落 `Unknown`；已弃用字段（`truncation` / `user` / `prompt_cache_retention` / `reasoning.generate_summary` / `tools[].connector_id`）不建模，经 `extra` 透传 |
| [`sag-core/src/lib.rs`](../../../../sag-core/src/lib.rs) | 导出 `pub mod api;`（供 providers / dispatch / router 三处共享同一份类型） |
| [`sag-providers/src/codec/anthropic.rs`](../../../../sag-providers/src/codec/anthropic.rs) | `decode_request` / `encode_request(_for_model/_with_loss)` / `decode_response` / `encode_response` 签名切 typed（对齐 `codec/chat.rs`）；删除顶层键 `KNOWN` 白名单，decode 侧把 `req.extra` 起手复制进 `ir.raw` 并额外写入「typed 但无 IR 归宿」的字段，encode 侧先按 `ir.raw` 回填 typed 槽位、其余落 `body.extra`（`or_insert`，不覆盖已合成值）；`anthropic_usage` 改为**按字段降级**（typed 解析失败时逐字段直读 `input_tokens` / `output_tokens` / 两个 cache 计数 / `output_tokens_details.thinking_tokens`，一个坏子对象不再把整包 usage 归零）；四个 `*_from_value` 桥接（`content_block` / `tool_union` / `tool_choice` / `thinking_config`）统一走 `parse_faithful` round-trip 守护，`tool_use` / `server_tool_use` 的 `input` 用 `Option` 表达三态（缺 `input` 不长键、显式 `"input": {}` 逐字回写）；SSE 帧仍走 `Value` |
| [`sag-providers/src/codec/responses.rs`](../../../../sag-providers/src/codec/responses.rs) | 同上切 typed；`decode_request(_allow_stateful/_with_mode)` 以 `req.extra` 为 `ir.raw` 起点，`store` / `previous_response_id` 状态字段继续进 `ir.raw` 供 native lane 回写；`put_raw` / `absorb_request` 成对处理无 IR 归宿的 typed 字段（`text` / `reasoning` / `metadata` / `include` / `service_tier` / `prompt_cache_key` / `context_management` 等）；`tools` / `tool_choice` / `input` / `output` 用 typed union 的 `Unknown` 保留原对象，桥接统一走 `parse_faithful`（回序列化必须与原值逐字相同才收 typed）；`responses_usage` 同样**按字段降级**（typed 失败时直读 `input_tokens` / `output_tokens` / `total_tokens` / `input_tokens_details.cached_tokens` / `output_tokens_details.reasoning_tokens`，坏子对象不把计数归零）；input message 的补丁守卫放宽为「`role` 缺失**或非字符串**」均补 `"user"` 后重走 typed 分派（此前非字符串 role 会整条丢弃）；`reject_unsupported` / `reject_unsupported_mode` 仍接收 `&Value`，422 消息文本逐字不变 |
| [`sag-providers/src/codec/chat.rs`](../../../../sag-providers/src/codec/chat.rs) | 测试 fixture 适配：跨 codec 用例先把 `json!` 反序列化为 `AnthropicMessagesRequest` 再进 decode |
| [`sag-providers/src/anthropic.rs`](../../../../sag-providers/src/anthropic.rs) | legacy `chat` 路径适配：`encode_request_with_loss` 得到的 typed body 序列化为 `Value` 后再做 `cache_control` 注入；上游响应经 `parse_anthropic_response` 解析为 typed，解析失败时降级为「读字符串信封 + 逐块 `Unknown`」，且降级**无损**：`stop_details` / `container` 按 `from_value(...).ok()` 还原、`extra` 收下整包 body（未建模字段与未知顶层键一个不丢），不再把非规范上游体变成空消息；该分支与 dispatch 的同名实现互指 NOTE（重复是已知取舍，抽公共 helper 属另一变更） |
| [`sag-providers/tests/cross_codec_tools.rs`](../../../../sag-providers/tests/cross_codec_tools.rs) | 跨 codec 集成用例的 fixture 改为 typed 请求/响应后再 encode |
| [`sag-dispatch/src/dispatch/chat/mod.rs`](../../../../sag-dispatch/src/dispatch/chat/mod.rs) | 边界适配：`body_to_value<T: Serialize>` 把 codec 产出的 typed body 单向序列化给 `LlmProvider` 原生方法（序列化失败映射 `AppError::Internal`，不 panic）；`decode_anthropic_response` / `decode_responses_response` 提供「typed 解析成功走 typed decode，失败则 lenient 降级」的辅助函数，保持旧 `decode_response(&Value)` 永不失败、且把原始 JSON 留在 `ir.raw` 供 native 透传；两处降级与 providers 侧同样无损（`stop_details` / `container` 还原、`extra` 保留整包 body），并与 `sag-providers/src/anthropic.rs::parse_anthropic_response` 互指 NOTE |
| [`sag-dispatch/src/dispatch/chat/tests.rs`](../../../../sag-dispatch/src/dispatch/chat/tests.rs) | 新增 typed happy path、schema mismatch 的 lenient 降级、lenient 分支保留 `container` / `stop_details` / 未知顶层键、200 送达的上游错误体保留在 `raw` 等用例 |
| [`sag-router/src/routes.rs`](../../../../sag-router/src/routes.rs) | 两个 handler 接线：Anthropic handler 在 JSON 语法检查后 typed 解析（失败返回 Anthropic 信封的 422 `invalid_request_error`），Responses handler 先跑 raw `reject_unsupported_mode` 再 typed 解析（失败返回 OpenAI 信封 422），语义校验顺序与消息不变；抽出 `unprocessable_app_error` 复用 `BadRequest` 原文 |
| [`sag-router/tests/anthropic_messages_ingress.rs`](../../../../sag-router/tests/anthropic_messages_ingress.rs) | 新增 typed 形状不匹配返回 Anthropic 错误信封（而非 OpenAI 信封）的用例 |
| [`sag-router/tests/responses_ingress.rs`](../../../../sag-router/tests/responses_ingress.rs) | 新增 typed 形状不匹配 422、以及 stateful 预检查先于 typed 解析的顺序守护用例 |

## 设计决策

| 决策 | 内容 |
|------|------|
| DTO 放 `sag-core/src/api/` | `sag-router` 需要在 handler 边界反序列化请求体，而 router 不依赖 `sag-providers` 的 codec 内部类型；放 `sag-core` 后 providers / dispatch / router 共享同一份类型，避免为了一处解析而在 router 里复制结构 |
| 无 `deny_unknown_fields`，每结构体带 `#[serde(flatten)] extra` | 与网关「拒绝未知值是反模式」原则一致：未知**字段**必须透传，严格性只留给配置里的枚举值。`extra` 同时是「无 IR 归宿字段」的载体，取代旧的 `KNOWN` 白名单 + 特判拷贝两处维护 |
| 多态联合「untagged 序列化 + 按 `type` tag 反序列化 + `Unknown(Value)`」 | 变体都保留 `kind: String`（值不校验）且字段多为可选，纯 `untagged` 反序列化会按形状匹配、让「全可选」变体吞掉后续变体；改为先读原始 `type` 精确分派，未知 tag 与不可解析载荷一律 `Unknown`，双向无损 |
| 弃用字段不建模（例外：Anthropic 采样三件套） | `truncation` / `user` / `prompt_cache_retention` / `reasoning.generate_summary` / `tools[].connector_id` 不进 DTO，靠 `extra` 透传；`temperature` / `top_p` / `top_k` 是 `chat ingress → IR sampling → anthropic upstream` 的活跃输入，旧模型仍接受，故保留 typed 并加弃用注释 |
| 边界仍保留 `Value` | `LlmProvider` 的原生方法签名（`anthropic_messages` / `responses_native` 及其 stream）不变，转换收敛在 dispatch 的 `body_to_value`；本次范围只覆盖非流式请求/响应体，SSE 帧继续用 `Value` |
| 上游响应 lenient 解析（降级也必须无损） | 旧 `decode_response(&Value)` 永不失败，若 typed 解析失败就直接报错会把「上游 200 但字段不规范」变成空响应；因此 dispatch 与 legacy provider 各保留一个降级分支：读字符串信封 + 逐块/逐项 `Unknown`，并显式还原 `stop_details` / `container`、把整包 body 收进 `extra`——降级恰好是 body 携带 DTO 表达不了形状时触发的路径，它最不该是丢字段的那条。两处实现互指 NOTE 维持同步（重复是已知取舍，抽公共 helper 属另一变更） |
| usage 按字段降级，而非整包归零 | `responses_usage` / `anthropic_usage` 都服务流式帧（SSE 不 typed 化），typed 解析失败时退回逐字段直读：一个类型写错的子对象（`input_tokens_details.cached_tokens: "64"`、`usage.cache_creation`）只影响它自己，计数器照旧进 analytics；整包 `Usage::default()` 会把「坏一个字段」放大成「整次调用计数丢失」 |
| `parse_faithful` round-trip 守护（两个 codec 同源） | DTO 中 `#[serde(default)]` 字段会被朴素 `from_value` 凭空补出调用方没发的键（Responses 侧 `tool_choice.allowed_tools` / `file_search.vector_store_ids`）。解析后回序列化与原值比对，只有逐字往返才收 typed、否则保留 `Unknown(Value)`。Anthropic 侧 `ToolUseBlock.input` / `ServerToolUseBlock.input` 用 `Option<Map<String, Value>>`（`skip_serializing_if = "Option::is_none"`）表达三态：缺 `input` → `None` → 不长键；显式 `"input": {}` → `Some(空 map)` → 逐字回 `"input": {}`；非空 → 原样回。三态在 DTO 上就成立，因此 `decode_response` 的 Raw 臂（直接序列化 typed block、不经桥接）同样保真——上一版用 `skip_serializing_if = "Map::is_empty"` 时该臂会把显式空对象静默修剪，已修正。IR 没有「参数缺席」状态，故 `tool_use` 的 `input` 在 IR 侧物化为 `arguments = {}`、egress 恒回 `input: {}`（与 Chat 侧 `arguments: "{}"` 同源）。Responses 侧（`tool_choice` / `tools` / `input` / `output`）与 Anthropic 侧四个桥接共用同一判据，避免两侧防线不对称 |
| 容错输入：`role` 缺失或非字符串都按 `"user"` | typed `MessageItem.role` 是必填 `String`，而旧 `Value` 解码器用 `as_str().unwrap_or("user")` 容忍了非规范输入。补丁守卫因此放宽为「`role` 缺失**或非字符串**」，把对象补成 `role: "user"` 后重走 typed 分派；否则一条 `{"type":"message","role":123,"content":"hi"}` 会整条丢弃用户内容 |
| 422 顺序与消息不动 | Responses 的 stateful / background 预检查必须在 raw `Value` 上先跑（body 连 typed 都过不去时，报的仍应是「不支持该能力」而非「反序列化失败」）；typed 解析只在语义预检查之后，且 `reject_unsupported(_mode)` 保持 `&Value` 与原文消息 |

**Trade-off**：

- ✅ decode / encode 全链路有类型约束，新增字段只需在 DTO 上加一个字段，不再同步维护白名单与特判拷贝
- ✅ 未建模字段、未知联合变体都经 `extra` / `Unknown` 无损往返，native 透传链不再依赖「记得改白名单」
- ✅ 请求体的「形状错误」从静默 loss 变成 handler 边界的 422，与 chat 路径的 typed 拒绝语义一致
- ❌ DTO 体量大（Anthropic 75 / Responses 145 个公开类型），与官方 schema 同步是长期维护成本；官方新增字段要等下次补 DTO 才「typed」，但期间仍经 `extra` 透传
- ❌ 多态联合的序列化（`untagged`）与反序列化（宏按 tag 分派）不是同一套派生逻辑，读代码时需要对照宏与变体列表
- ❌ 上游响应多了一条 lenient 降级分支，属于「typed 化不该损失健壮性」的额外成本
- ❌ 该降级分支在 `sag-providers` 与 `sag-dispatch` 各存一份（靠互指 NOTE 维持同步），抽公共 helper 属另一变更

## 测试

- `sag-core/src/api/anthropic.rs`（21 例）与 `sag-core/src/api/responses.rs`（7 例）内联单元测试：DTO 的 `json!` 反序列化 / 序列化往返、未知字段经 `extra` 保留、未知联合 tag 落 `Unknown`、已知 tag 但载荷不匹配的行为、弃用字段（`truncation` 等）仍能透传、`temperature` / `top_p` / `top_k` 正常读写；`missing_or_empty_tool_input_is_not_fabricated_on_serialize` 双向钉住 `tool_use` / `server_tool_use` 的 `input` 三态：缺 `input` 时回序列化不长键、显式 `"input": {}` 原样回空对象；`every_documented_block_type_dispatches_to_a_typed_variant` 另含显式 `"input": {}` 的 strict round-trip 用例
- `sag-providers/src/codec/anthropic.rs`：`request_typed_fields_without_ir_home_roundtrip` / `response_typed_fields_without_ir_home_roundtrip` 钉住「typed 但无 IR 归宿的字段必须经 `decode → ir.raw → encode` 存活」，以及 typed 化后既有 wire 形状断言全部保持；`usage_degrades_per_field_when_subobject_is_malformed` / `usage_degraded_path_keeps_old_defaults` 钉住 usage 按字段降级（坏 `cache_creation` / `server_tool_use` 不归零计数，缺字段仍取旧默认：cache 计数在 `input_tokens > 0` 时算「已知 0」、legacy 根级 `thinking_tokens` 仍读）；`tool_use_block_without_input_keeps_wire_shape` / `response_raw_block_without_input_keeps_wire_shape` / `decode_response_typed_path_does_not_grow_server_tool_use_input` / `faithful_guard_keeps_well_formed_unions_typed` 钉住两侧都不凭空合成 `"input": {}`、显式 `"input": {}` 逐字回写（含 `decode_response` → `encode_response` 全链路，即绕过桥接的 Raw 臂），且不干扰形状良好的联合
- `sag-providers/src/codec/responses.rs`：`reject_unsupported_keeps_value_layer_messages`（422 消息与顺序逐字不变）、`request_typed_echo_fields_roundtrip_through_raw`、`input_message_typed_extras_roundtrip`、`tool_choice_allowed_tools_shape_preserved`（`allowed_tools: []` 不被凭空合成）、`response_echo_fields_and_incomplete_details_roundtrip`、`message_item_without_role_defaults_to_user` / `message_item_with_non_string_role_defaults_to_user`（typed 必填 `role` 时缺失与非字符串都恢复默认而非丢内容）、`malformed_values_are_parse_errors_but_unknown_fields_pass`（形状错误 → 解析错误，未知字段仍透传）、`usage_degrades_per_field_when_details_are_ill_typed` / `stream_completed_keeps_usage_when_details_are_ill_typed`（坏 `input_tokens_details` / `output_tokens_details` 不把流式帧计数归零）
- `sag-providers/src/anthropic.rs`：`from_anthropic_response_decodes_leniently_on_schema_mismatch`（`usage` 类型错误仍还原正文）、`parse_anthropic_response_lenient_path_keeps_unmodelled_fields`（降级分支保留 `container` / `stop_details` / 未知顶层键，且 encode 不重复输出已建模键）
- `sag-providers/tests/cross_codec_tools.rs`：既有跨 codec 工具保真用例改为 typed fixture 后全部保持
- `sag-dispatch/src/dispatch/chat/tests.rs`：typed happy path 字节保真、Anthropic / Responses 两处 schema mismatch 的 lenient 降级、`decode_anthropic_response_lenient_path_keeps_unmodelled_fields`（降级分支的 `ir.raw` 保留 `container` / `stop_details` / 未知顶层键）、200 送达的上游错误体保留在 `raw` 供 native 透传
- 返工新增 10 例回归测试（usage 降级 4 / `parse_faithful` 守护 3 / `role` 兜底 1 / lenient 无损 2）；`cargo test -p sag-providers --lib` 263 passed、`cargo test -p sag-dispatch` 282 passed
- `sag-router/tests/anthropic_messages_ingress.rs`：`messages_typed_shape_mismatch_returns_anthropic_error_envelope`
- `sag-router/tests/responses_ingress.rs`：`typed_shape_mismatch_rejected_422`、`stateful_reject_runs_before_typed_parse`
