---
created: 2026-10-25
updated: 2026-10-25
---

# 隐私过滤 — 规格

## 关键文件

按职责分组。

### 检测与脱敏（`sag-privacy` crate）

纯算法 crate，只依赖 `regex` / `aho-corasick` / `sha2` / `serde_json` 与 `sag-config` 的类型，不持有状态、不读配置。

- `sag-privacy/src/placeholder.rs` — `make`（确定性占位符）、`placeholder_len`、`PlaceholderMap`（占位符 → 明文反向索引，`replace_in` 单遍最长优先替换）
- `sag-privacy/src/rules.rs` — `CompiledRule { kind, re, adj, check }`、`Adjacency { Digits, Wordish, None }`、`Checksum { Luhn, IdCardCn, Iban }`、`boundary_ok`、7 条内置 pattern 常量
- `sag-privacy/src/engine.rs` — `PrivacyEngine::compile`（内置 + 自定义正则 + Aho-Corasick 词表）、`scan`（候选收集 + allowlist + 贪心非重叠解析）、`mask_text`、`mask_request`（Chat 请求全字段）、`mask_ir`（IR 原地脱敏，遍历层 `scrub_blocks` / `scrub_json` / `scrub_map`）、`UNSCANNED_KEYS`、`Outcome`、`MaskReport`
- `sag-privacy/src/restore.rs` — `RestoreWriter`（SSE 逐帧还原）
- `sag-privacy/src/lib.rs` — 公开再导出

### 配置

- `sag-config/src/config.rs` — `PrivacyConfig`（含 `Default`：`enabled=false` / `mode=Mask` / `restore=true` / 空 salt）、`PrivacyMode`（严格枚举）、`PrivacyBuiltins`（默认全 true）、`PrivacyCustomRegex`、`PrivacyWordList`、`GatewayConfig::validate_privacy`
- `gateway.example.toml` — `[privacy]` 示例段（默认全注释）

### 分发挂点（`sag-dispatch`）

- `sag-dispatch/src/dispatch/chat/mod.rs` — 请求入口的隐私闸门（约 L176-243，含一次性 config 快照）、masked IR 优先（约 L259-268）、非流式出口 `privacy_restored_response`（5 处调用，约 L1146-1158）、`restore_value`、`render_hits`、`privacy_violation_response`、`privacy_unavailable_response`
- `sag-dispatch/src/dispatch/stream.rs` — `build_streaming_response` 末参 `Option<RestoreWriter>`（约 L917）、`make_stream_state` 的同名字段、unfold 循环内 `writer.push`（约 L1010-1025）与终点 `writer.finish`（约 L985-1002）
- `sag-dispatch/src/dispatch/chat/tests.rs` — 4 个隐私集成测试与 `build_state_with_privacy` fixture

## 核心设计

### 请求与响应链路

```mermaid
sequenceDiagram
    participant C as 客户端
    participant G as dispatch_chat
    participant E as PrivacyEngine
    participant P as provider
    participant R as 还原层

    C->>G: chat request（可能含明文）
    G->>G: state.config.read().await.privacy.clone()
    alt enabled = false
        G->>P: 原请求（零改写）
    else enabled = true
        G->>E: PrivacyEngine::compile(&cfg)
        alt 编译失败
            E-->>C: 503 SERVICE_UNAVAILABLE
        else 编译成功
            G->>E: mask_request(&mut candidate)
            alt Blocked
                E-->>C: 400 privacy_violation（kind=count）
            else Masked
                E-->>G: MaskReport{hits, map}
                G->>G: mask_ir(&mut ir.clone())（原地脱敏）
                G->>P: masked request + masked IR
                P-->>G: 响应（含占位符）
                G->>R: privacy_map（restore=true 时）
                R-->>C: 还原后的明文
            else Detected（warn）
                E-->>G: MaskReport{hits, map=∅}
                G->>P: 原请求
                P-->>C: 上游原文
            else Clean
                G->>P: 原请求
            end
        end
    end
```

### 检测流水线（`scan`）

```mermaid
flowchart TD
    A[text] --> B[每条 CompiledRule find_iter]
    B --> C{accepts?}
    C -->|否| X[丢弃候选]
    C -->|是| D[(候选集 start,end,kind,matched)]
    A --> E[AhoCorasick find_overlapping_iter]
    E --> D
    D --> F[allowlist 整段相等过滤]
    F --> G[stable sort: start 升序, end 降序]
    G --> H[游标贪心: span.start 小于 cursor 则跳过]
    H --> I[有序、互不重叠的 span 列表]
```

规则顺序固定（`email` → `phone` → `id_card_cn` → `bank_card` → `iban` → `api_key` → `private_key` → 自定义正则），`custom_words` 在最后追加。stable sort 让「同区间」的胜者退化为「配置在前的规则」，因此 `id_card_cn` 天然压过同区间的 `bank_card`。

### 流式还原状态机

```mermaid
flowchart TD
    A[push chunk] --> B[buf += chunk]
    B --> C{存在两个连续换行?}
    C -->|否| D[返回空字节<br/>半帧继续缓冲]
    C -->|是| E[切出一整帧]
    E --> F{data: 帧?}
    F -->|否| G[原样输出]
    F -->|是| H{payload 是 JSON?}
    H -->|否| G
    H -->|是| I[递归还原字符串值]
    I --> J{有命中?}
    J -->|否| G
    J -->|是| K[重新序列化 + 前缀 + \n\n]
    K --> L[输出]
    G --> L
    L --> C
    D --> M[End / None: finish 冲尾<br/>剩余字节原样下发]
```

## 关键决策

### 正则 + 邻接 + 校验和，而非 NER

内置检测器是「正则匹配 → 邻接字符校验 → 结构校验」三级，没有引入模型或词典。

**Trade-off**：放弃 NER 路线，因为网关要部署在用户自己的机器上：模型权重、推理延迟、误报后的可解释性都是新负担，且人名/地址类实体的误报会直接破坏 prompt 语义（比漏检更难排查）。代价是无固定形态的实体（人名、地址）不在覆盖范围，需求文档已明确列为边界，v2 通过 trait 扩展。

### 邻接与校验和守卫写在 Rust 侧，而非正则内

`Adjacency` 与 `Checksum` 是 `CompiledRule::accepts` 里的两次手工判定，pattern 本身不含断言。

**Trade-off**：Rust `regex` 不支持 lookaround，「两侧不紧邻数字」无法写进 pattern。放弃「换成支持 lookaround 的引擎（`fancy-regex`）」，因为那会引入回溯与性能不可控。代价是 18 位身份证、16 位卡号这类宽泛数字形态必须再叠一层校验和，否则订单号、时间戳会被大量误脱敏——误报比漏检更伤（会改坏正常对话内容）。

### 原地脱敏 IR，而非编解码往返

masked 请求需要同步到 IR（translated lane 的编码源），实现上是 `let mut masked_ir = original_ir.clone(); engine.mask_ir(&mut masked_ir)`：`mask_ir` 遍历 `IrRequest` 的每个字符串叶子（content 各 block 变体的 `text` / `ToolUse.name` / `ToolUse.arguments` / `ToolResult.content`、`IrMessage.name` / `raw` / `tool_calls[].arguments`、`tools[].description` / `input_schema` / `raw`、`tool_choice` / `response_format` / `metadata` / `stop` / `reasoning.thinking` / 顶层 `raw`），用与 `mask_request` **同一个** `scan` + `mask_text` + `PlaceholderMap` 替换，因此 messages 内外同文本得同一占位符。

**Trade-off**：放弃「用 `ChatCompletionsCodec::decode_request(&candidate)` 整包往返重建 IR」。`routes.rs` 对 Anthropic / Responses 请求体是直接解码的，用 Chat 编解码器整包往返会把 `IrContentBlock::Reasoning` 降级成 `Text`（Anthropic thinking 变成可见正文）、把 `ToolResult` 拆成独立 `role:"tool"` 消息、并让 `Raw` 块走 loss 路径——而且这些 loss 不会进 `ir.loss`（`..original.clone()` 保留原值），属静默结构丢弃。原地遍历保持 block 变体与 `ir.loss` 不变，代价是新增字段需要同步 `mask_ir` 的遍历（`mask_ir` 单测断言每个字段都被覆盖）。

### `map = None` 的零开销路径

非流式出口是 `privacy_restored_response(body, map)`：`map == None` 时直接 `(StatusCode::OK, Json(body))`，与引入隐私过滤之前的代码路径逐字节一致。

**Trade-off**：放弃「统一走还原管线再判断是否有命中」。默认 `enabled = false` 是最常见状态，任何额外序列化/反序列化都会给所有用户加成本；同理 `restore = false`、`warn`/`block` 模式（`map` 恒为空）也走这条路径。

### 每请求编译引擎

`PrivacyEngine::compile` 在每个启用隐私的请求里执行一次（引擎不可变、不持有状态，编译后只服务于该请求）。

**Trade-off**：放弃「编译结果缓存 + 按配置版本失效」。每请求编译换来热重载零缓存失效逻辑、无需在 `AppState` 增加第五份共享状态；代价是每请求重新编译 7 条内置正则与用户规则（未做专门基准）。隐私过滤是 opt-in，且引擎只持有不可变数据，无跨请求一致性风险。

### 占位符确定性（kind + 文本 + salt）

`make` 是 `sha256("{kind_lower}:{text}:{salt}")` 前 12 hex，不做随机 nonce、不做递增序号。

**Trade-off**：放弃随机占位符，因为多轮对话里客户端每次重发明文，若占位符随机，上游看到的实体每次都是新标记，语义会漂移，prompt cache 也完全无法复用。代价是同一 salt 下同一明文产生同一摘要，等于给上游一个可跨请求关联的稳定假名——这是可接受的（摘要不可逆，salt 可配置成随机值）。

### 单遍最长优先的 span 解析

`scan` 收集**全部**候选后再做 stable sort + 游标贪心，而不是「按规则顺序，命中即屏蔽该区域」。

**Trade-off**：放弃短路式解析（先匹配的规则吃掉区域）。身份证必然内含手机号形状的数字，先到先得会把身份证切成「手机号 + 残渣」；`AhoCorasick::find_overlapping_iter` 同理不能提前去重，否则长词会被短词抢先。贪心 + 最长优先让「信息量更大的类别」胜出。

### 按 SSE 帧还原，而非缓冲整条流

`RestoreWriter` 只在凑齐 `\n\n` 时处理一帧，未凑齐的尾巴留在 `buf`。

**Trade-off**：放弃「缓冲整个响应体后统一替换」。那会破坏流式语义（首字节延迟、内存随响应长度无界增长）。代价是被劈到两个帧里的占位符无法还原（两半都不构成含完整占位符的 JSON 文档）——占位符是加盐摘要、不含明文，因此属显示瑕疵而非泄露。

### 还原走 JSON 值遍历，而非字节替换

非流式 `restore_value` 与流式 `replace_strings` 都是「解析成 `Value` → 递归处理字符串值 → 命中才重新序列化」。

**Trade-off**：放弃「直接在字节流上 `str::replace`」。明文可能含 `"`、`\n`、`\`，字节替换会产出非法 JSON；交给序列化器重新转义才是正确做法。代价是命中帧的键顺序可能被重排（对 JSON 消费者无害），未命中帧必须走原字节透传以免无谓重排。

### 错误体只含类别计数

`privacy_violation_response` 的 message 由 `render_hits` 生成（`kind=count` 用 `,` 连接）；`privacy_unavailable_response` 的 message 是固定串。

**Trade-off**：放弃「回显命中的原文或具体正则」。400 体若带明文，拒绝本身就变成泄露通道；503 体若带 pattern，会把用户配置里的正则回吐给任何能发请求的人。细节只进 tracing（`error!` / `warn!`）。

### block / 503 早退不写 analytics

两条早退路径都在分发循环之前 `return`，此时 analytics 的 terminal guard 尚未创建。

**Trade-off**：现状是「被拒请求在统计里不可见」。好处是拒绝不会污染成功/失败率与供应商维度指标（没有 provider 被调用）；代价是运营方无法从 dashboard 看到「有多少请求被隐私策略挡掉」——已列入 v1.5 候选（补一条独立事件而非复用 `call_logs`）。

## 不变式

- `PrivacyEngine::compile` **不读** `cfg.enabled`：是否构建引擎由调用方（`dispatch_chat`）判定
- 占位符格式恒为 `[[{KIND}_{12 hex}]]`，hex 为小写；`make` 对 `kind` 大小写不敏感（摘要用小写、渲染用大写）
- `make` 的摘要输入恒为 `"{kind_lower}:{text}:{salt}"`，截断长度恒为 `HASH_HEX_LEN = 12`
- `PlaceholderMap::replace_in` 单遍扫描、长 key 优先，替换进去的明文不会被再次扫描
- allowlist 是**整段精确相等**匹配（`allowed.as_str() == matched.as_str()`），不是子串/前缀
- `scan` 输出按 `start` 严格升序且区间互不重叠；同起点取最长；同区间取配置顺序在前的规则
- 内置规则注册顺序固定为 `email`、`phone`、`id_card_cn`、`bank_card`、`iban`、`api_key`、`private_key`，自定义正则追加在后
- `custom_words` 空串在编译期被丢弃（空 pattern 会在每个偏移命中）；`allowlist` 同样丢弃空串
- 只有 `PrivacyMode::Mask` 会改写请求；`warn` / `block` 的 `MaskReport.map` 恒为空
- `mask_request` 扫描请求的每个字符串叶子：`messages[].content`（字符串或内容块里的文本叶子）、`messages[].name`、`messages[].extra`、`stop`、`metadata`、`tools`、`tool_choice`、`response_format`、`logit_bias`、`prediction`、`moderation`、`web_search_options` 等顶层字段与 `extra` 兜底字段；除 `UNSCANNED_KEYS`（`model` / `role` / `type` / id 类键、`data` / `file_data` / `url` 不透明载荷）外，其余字段必须逐字节不变；对象键名恒不扫描
- `mask_ir` 原地遍历 `IrRequest` 全部字符串叶子，**不** 走任何编解码往返：block 变体（`Reasoning` / `ToolResult` / `Raw`）与 `ir.loss` 恒不变，`model` 与 `tool_call_id` / `id` 恒不改写
- `mask_request` 与 `mask_ir` 共用同一 `PlaceholderMap` 语义：同 `(kind, text, salt)` 恒得同一占位符
- `privacy_map` 仅在 `privacy_cfg.restore && !report.map.is_empty()` 时被填充
- `privacy_restored_response` 在 `map == None` 时走 `(StatusCode::OK, Json(body))`，不得插入任何序列化往返
- `RestoreWriter` 只改写 `data:` 帧里的 JSON **字符串值**（不含对象键）；非 `data:` 帧、非 JSON payload、无命中的帧一律原样返回
- `RestoreWriter::push` 返回空字节表示「尚无完整帧」，流式循环必须 `continue` 且不得据此 stamp TTFT（TTFT 以实际下发字节为准）
- 流式终点（`End` / `None`）必须调用 `finish()` 并下发残余字节，否则尾部帧会被丢弃
- `block` 的 400 与 `503` 的 body 不得包含明文、正则 pattern 或引擎错误详情

## 测试策略

`cargo test -p sag-privacy` — 49 个单元测试：

| 文件 | 数量 | 覆盖 |
|---|---|---|
| `sag-privacy/src/placeholder.rs` | 8 | 确定性 / salt 区分 / kind 大小写 / 12 hex 形态 / 已知摘要前缀 / 长度一致 / 多命中与相邻 / 不重扫明文 |
| `sag-privacy/src/rules.rs` | 7 | Luhn 正反例与长度窗口 / 身份证校验位与 15 位旧式 / IBAN mod-97 / `boundary_ok` 三档邻接 / 内置 pattern 形态 / `accepts` 组合 / 坏 pattern 报错 |
| `sag-privacy/src/engine.rs` | 21 | 7 个内置检测器正反例 / 关闭后不命中 / 重叠取最长 / 同区间取具体规则 / allowlist / 自定义词字面与大小写 / 自定义正则大小写 / 坏正则报错 / 占位符确定性复用 / `mask_request` 字段覆盖（含 name / extra / 顶层字段）/ `mask_ir` 全字段覆盖与 block 变体保持 / 四模式语义 |
| `sag-privacy/src/restore.rs` | 13 | 嵌套还原与转义 / 数组与对象 / 单帧与多帧 / 半帧缓冲 / 分隔符后立即下发 / `finish` 冲尾 / 非 JSON 与非 `data:` 透传 / 无命中字节透传 / 空 map / 跨帧占位符不还原 |

`cargo test -p sag-dispatch` — 4 个集成测试（`sag-dispatch/src/dispatch/chat/tests.rs`，`build_state_with_privacy` fixture）：

| 用例 | 断言 |
|---|---|
| `dispatch_chat_masks_request_and_restores_response` | provider 收到占位符且不含明文；客户端响应含明文、不含占位符 |
| `dispatch_chat_restores_placeholders_in_stream` | 流式 SSE 帧内占位符被还原，客户端看到明文 |
| `dispatch_chat_blocks_request_in_block_mode` | `400` + `error.code = "privacy_violation"` + message 含 `email=1` 且不含明文；`mock.chat_calls() == 0` |
| `dispatch_chat_masks_fields_outside_messages` | `metadata` / 顶层未知字段（`extra`）/ `messages[].name` 中的 PII 在 provider 侧已是占位符，序列化后的请求不含明文 |

`cargo test -p sag-config privacy` — 8 个配置测试（`config::tests`）：`empty_privacy_table_yields_defaults`、`missing_privacy_section_yields_defaults`、`full_privacy_section_round_trips`、`unknown_privacy_mode_is_rejected`、`unknown_privacy_fields_are_tolerated`、`invalid_custom_regex_is_rejected`、`empty_custom_regex_kind_is_rejected`、`valid_custom_regex_passes_validation`。

关键场景清单：

- 规则正反例（校验和失败、邻接字符失败、前缀长度不足）
- 占位符确定性（同文本同 salt → 同标记；换 salt 变化）
- 流式切分（一帧跨多 chunk、多帧在一个 chunk、尾部不完整帧）
- 端到端四条路径（mask 非流式还原 / mask 流式还原 / mask 覆盖 messages 之外的字段 / block 早退不调 provider）
- 配置校验（未知 `mode` 拒绝、未知字段容忍、坏正则与空 `kind` 在 `enabled=false` 下仍被拒）

## 变更记录

- [0001 隐私过滤初始化](changes/0001-initial.md)
