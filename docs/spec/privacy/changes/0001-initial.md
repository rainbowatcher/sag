---
created: 2026-10-25
updated: 2026-10-25
type: 新增
---

# 0001 — 隐私过滤初始化

## 动机

网关是明文凭据的必经之路：客户端会把邮箱、手机号、身份证、银行卡、API key、PEM 私钥写进 prompt，再被原样转发给上游 provider。此前没有任何出口侧的敏感值处置能力，用户只能在「自己逐个改写 prompt」和「把明文交给第三方」之间二选一。

本次引入独立的 `sag-privacy` crate 与一条默认关闭的请求闸门：在请求离开网关前识别敏感值，按 `mask` / `warn` / `block` 三种模式处置；`mask` 时把命中值替换成确定性占位符 `[[KIND_12hex]]`，并在响应回到客户端前（非流式 JSON 与流式 SSE 两条路径）把占位符还原成明文，使客户端完全无感。默认 `enabled = false`，关闭时行为与未引入该功能时逐字节一致。

## 需求变更

- `requirements.md` 全部初始定义：
  - 「检测范围」：检测请求中**所有会发往上游的字符串叶子**，把 PII 从 `content` 挪到别的字段无法绕过——`messages`（`content` 各内容块的字符串叶子、`name`、`extra`，含 `tool_calls[].function.arguments` / `reasoning_content` / `refusal` / `annotations`）、`metadata`、`stop`、`tools`（`name` / `description` / `input_schema` / `raw`）、`tool_choice`、`response_format`、`prediction` / `logit_bias` / `moderation` / `web_search_options` 等顶层字段、以及 `extra` 兜底字段（任意未知顶层字段）。两类逐字节保留的例外：**协议结构键**（`model` / `role` / `type` / `id` / `tool_call_id` / `tool_use_id` / `call_id`）与**不透明媒体载荷**（`data` / `file_data` / `url`——base64 里统计上必然出现 IBAN 形状串，脱敏会改坏图片）；对象**键名**不脱敏（只脱敏字符串值）。7 个内置检测器（`email` / `phone` / `id_card_cn` / `bank_card` / `iban` / `api_key` / `private_key`，各自可单独开关，默认全开）与两类自定义检测器（`custom_regexes` 正则、`custom_words` 字面词表）；邻接字符校验、重叠取最长、同区间取配置在前者、`allowlist` 整段精确豁免。
  - 「脱敏占位符」：`[[KIND_12位hex]]`，hex 取 `sha256("{kind 小写}:{text}:{salt}")` 前 12 字符，同文本 + 同 salt 恒得同一占位符。
  - 「三种模式」：`mask`（上游见占位符、客户端见明文，`info`）/ `warn`（不改写，`warn`）/ `block`（转发前 `400 privacy_violation`，message 只含 `kind=count`）。
  - 「响应侧还原」：`restore = true` 时非流式递归还原字符串值、流式按 SSE 帧还原（半帧缓冲、非 `data:` 帧与非 JSON 载荷逐字节透传、`finish` 冲尾）。
  - 「失败处理」：引擎初始化失败 fail-closed `503`（响应体不含正则与错误详情）；未启用时零改写。
  - 「配置与热重载」：`[privacy]` 段每请求从配置快照读取（与路由 / 重试等旋钮共用同一份快照，每请求只取一次），热重载后下一个请求生效；`custom_regexes` 无论 `enabled` 都在配置校验阶段编译。
  - 「事件日志与可观测性」：三类事件只记 `kind=count`，不记明文与占位符；不新增 analytics 记录。
  - 「功能边界」明确列出不包含项：NER / 自由文本语义检测、句末紧跟标点的邮箱、不透明载荷（`data` / `file_data` / `url`）与模型名 / `role` / `type` / 工具调用 id / 请求头 / 对象键名的检测、图片生成路径、`block` / `503` 早退的 analytics 记录、跨 SSE 帧被劈开的占位符、占位符可逆加密、反向映射跨请求持久化、admin API 隐私配置端点。
- `spec.md` 全部初始定义（关键文件、请求/响应链路、检测流水线、流式还原状态机、10 条关键决策、不变式清单、测试策略）。
- 本模块「变更记录」节初始条目。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-privacy/Cargo.toml`](../../../../sag-privacy/Cargo.toml) | 新 crate：仅依赖 `sha2` / `serde_json` / `regex` / `aho-corasick` 与 `sag-config` / `sag-core`，不持状态、不读配置 |
| [`sag-privacy/src/lib.rs`](../../../../sag-privacy/src/lib.rs) | 公开再导出 `PrivacyEngine` / `Outcome` / `MaskReport` / `PlaceholderMap` / `RestoreWriter` / `make` / `placeholder_len` / `HASH_HEX_LEN` |
| [`sag-privacy/src/placeholder.rs`](../../../../sag-privacy/src/placeholder.rs) | `make` 确定性占位符（`sha256("{kind_lower}:{text}:{salt}")` 前 12 hex）、`placeholder_len`、`PlaceholderMap::replace_in`（单遍、长 key 优先、替换结果不再扫描） |
| [`sag-privacy/src/rules.rs`](../../../../sag-privacy/src/rules.rs) | `CompiledRule { kind, re, adj, check }`、`Adjacency { Digits, Wordish, None }`、`Checksum { Luhn, IdCardCn, Iban }`、`boundary_ok`、7 条内置 pattern 常量；邻接与校验和守卫写在 Rust 侧（`regex` 无 lookaround） |
| [`sag-privacy/src/engine.rs`](../../../../sag-privacy/src/engine.rs) | `PrivacyEngine::compile`（内置 + 自定义正则 + Aho-Corasick 词表，空串丢弃）、`scan`（候选收集 → allowlist → stable sort → 游标贪心取最长）、`mask_text`、`mask_request`（Chat 请求全字段：`content` / `name` / `extra` / 顶层字段 / `extra` 兜底）、`mask_ir`（IR **原地**脱敏，遍历层 `scrub_blocks` / `scrub_json` / `scrub_map`）、`UNSCANNED_KEYS`（协议结构键与不透明载荷豁免表）、`Outcome` / `MaskReport` |
| [`sag-privacy/src/restore.rs`](../../../../sag-privacy/src/restore.rs) | `RestoreWriter`：按 `\n\n` 切 SSE 帧、只还原 `data:` 帧内 JSON 字符串值、半帧缓冲、`finish` 冲尾；命中才重新序列化，未命中逐字节透传 |
| [`sag-config/src/config.rs`](../../../../sag-config/src/config.rs) | 新增 `PrivacyConfig`（`enabled=false` / `mode=Mask` / `restore=true` / 空 salt 的 `Default`）、严格枚举 `PrivacyMode`、`PrivacyBuiltins`（默认全 true）、`PrivacyCustomRegex`、`PrivacyWordList`；`GatewayConfig` 增 `privacy` 字段并在 `validate()` 挂 `validate_privacy`（自定义正则与 `kind` 无条件校验，错误信息含条目索引与 pattern） |
| [`sag-config/src/loader/handle.rs`](../../../../sag-config/src/loader/handle.rs) | 测试 fixture 的全字段字面量补 `privacy: PrivacyConfig::default()` |
| [`sag-config/Cargo.toml`](../../../../sag-config/Cargo.toml) | 新增 `regex` 依赖（`validate_privacy` 编译用户正则） |
| [`sag-dispatch/src/dispatch/chat/mod.rs`](../../../../sag-dispatch/src/dispatch/chat/mod.rs) | 每请求只取一次配置快照（路由 / 重试 / 隐私共用）；请求侧隐私闸门（解构 `DispatchCtx` 后、调用 provider 前）：`enabled` 判定 → `compile` 失败 `503` → `mask_request` 按 `Outcome` 分派（`Blocked` 400 / `Masked` 记 map、以 masked 请求转发、并把脱敏同步到 IR / `Detected` 仅告警 / `Clean` 透传）。IR 同步走 `let mut masked_ir = original_ir.clone(); engine.mask_ir(&mut masked_ir)`——**原地**脱敏，保持 `IrContentBlock` 变体（`Reasoning` 不降级为 `Text`、`ToolResult` 不拆成 `role:"tool"` 消息）与 `ir.loss` 不变；非流式 5 个出口统一走 `privacy_restored_response`（`map == None` 时零开销直出）；新增 `restore_value` / `render_hits` / `privacy_violation_response` / `privacy_unavailable_response` |
| [`sag-dispatch/src/dispatch/stream.rs`](../../../../sag-dispatch/src/dispatch/stream.rs) | `build_streaming_response` 与流式状态新增 `Option<RestoreWriter>`：unfold 循环内 `push` 逐帧还原（空字节表示无完整帧，不据此 stamp TTFT），终点 `End` / `None` 调 `finish` 下发残余字节 |
| [`sag-dispatch/src/dispatch/chat/tests.rs`](../../../../sag-dispatch/src/dispatch/chat/tests.rs) | 新增 `build_state_with_privacy` fixture 与 4 条端到端隐私测试 |
| [`sag-dispatch/Cargo.toml`](../../../../sag-dispatch/Cargo.toml) | 新增 `sag-privacy` 依赖 |
| [`Cargo.toml`](../../../../Cargo.toml) | workspace members 增 `sag-privacy`；workspace 依赖增 `regex` / `aho-corasick` |
| [`gateway.example.toml`](../../../../gateway.example.toml) | 新增全注释的 `[privacy]` 示例段（含 `builtins` / `custom_regexes` / `custom_words` / `allowlist`） |
| [`gateway.test.example.toml`](../../../../gateway.test.example.toml) | 同上（测试轨示例配置同步） |
| [`sag-gateway/src/main.rs`](../../../../sag-gateway/src/main.rs) | 测试 fixture 的全字段字面量补 `privacy: PrivacyConfig::default()` |

## 测试

`cargo test -p sag-privacy` — 49 个单元测试：

- `sag-privacy/src/placeholder.rs`（8）— 确定性、salt 区分、`kind` 大小写、12 hex 形态、已知摘要前缀、长度一致、多命中与相邻、不重扫已替换明文。
- `sag-privacy/src/rules.rs`（7）— Luhn 正反例与长度窗口、身份证校验位与 15 位旧式、IBAN mod-97、`boundary_ok` 三档邻接、内置 pattern 形态、`accepts` 组合、坏 pattern 报错。
- `sag-privacy/src/engine.rs`（21）— 7 个内置检测器正反例、单独关闭后不命中、重叠取最长、同区间取具体规则、`allowlist` 精确豁免、自定义词字面与大小写、自定义正则大小写、坏正则报错、占位符跨次复用、`mask_request` 字段覆盖范围、`mask_request_covers_name_extra_and_top_level_fields`（`messages[].name` / `extra` / 顶层字段）、`mask_ir_covers_every_field_and_keeps_block_variants`（全字段覆盖 + block 变体保持）、四模式语义。
- `sag-privacy/src/restore.rs`（13）— 嵌套还原与转义、数组与对象、单帧与多帧、半帧缓冲、分隔符后立即下发、`finish` 冲尾、非 JSON 与非 `data:` 帧透传、无命中字节透传、空 map、跨帧占位符不还原。
- `sag-dispatch/src/dispatch/chat/tests.rs`（4）— `dispatch_chat_masks_request_and_restores_response`（provider 侧只见占位符、客户端侧只见明文）、`dispatch_chat_restores_placeholders_in_stream`（SSE 帧内还原）、`dispatch_chat_blocks_request_in_block_mode`（`400` + `privacy_violation` + message 含 `email=1` 不含明文 + `mock.chat_calls() == 0`）、`dispatch_chat_masks_fields_outside_messages`（`metadata` / 顶层未知字段 / `messages[].name` 的 PII 在 provider 侧已是占位符）。
- `sag-config/src/config.rs` `mod privacy_tests`（8）— `empty_privacy_table_yields_defaults`、`missing_privacy_section_yields_defaults`、`full_privacy_section_round_trips`、`unknown_privacy_mode_is_rejected`、`unknown_privacy_fields_are_tolerated`、`invalid_custom_regex_is_rejected`、`empty_custom_regex_kind_is_rejected`、`valid_custom_regex_passes_validation`。
