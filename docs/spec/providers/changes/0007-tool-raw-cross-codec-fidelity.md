---
created: 2026-10-19
updated: 2026-10-19
type: 修复
---

# 0007 — 跨 codec 工具字段保真（统一 `IrTool.raw` 双层约定）

## 动机

`sag-providers` 的三个 codec 对 `IrTool.raw` 各自约定了一套布局，且没有任何共享代码约束：

| codec | `raw` 布局 |
|---|---|
| `codec/chat.rs` | 双层结构 `{"__tool_extra": {...}, "__function_extra": {...}}`（工具级字段 / `function` 级字段分开放，编码时各回原位） |
| `codec/responses.rs` | 扁平平铺（工具对象除 `name` 外的字段原样留在 `raw`） |
| `codec/anthropic.rs` | 扁平平铺（工具对象除 `name` / `description` / `input_schema` 外的字段原样留在 `raw`） |

约定分裂由同一个根因产生四个缺陷：

1. **Responses 入站 → Chat 出站丢 `tools[].type`**：chat `encode_tools` 只从 `__tool_extra` 还原顶层字段，扁平 `raw` 里的 `type` 读不到，出站工具变成无 `type` 的裸 `function` 对象；上游按 pydantic 校验直接 500（`tools[0].type Field required`）。
2. **同路径丢 `tools[].strict`**：Responses 把 `strict` 放在工具根部，chat 编码只把 `__function_extra` 写进 `function`，扁平 `raw` 的 `strict` 无处落位，静默消失。
3. **Anthropic 入站 → Chat 出站丢 `type`**：Anthropic 工具对象没有 `type` 字段，而 chat `encode_tools` 又只从 `__tool_extra` 取 `type`，于是出站工具同样缺 `type`（与缺陷 1 同一条上游 500 路径）；hosted `web_search*` 的 `raw.type` 也只能在扁平平铺上看到
4. **Chat → Responses / Chat → Anthropic 出站泄漏伪字段**：两个 codec 的 `encode_tools` 把 `raw` 的**所有**键平铺写出，`__tool_extra` / `__function_extra` 这两个内部结构标记作为字面键进入上游 JSON。

此外 hosted `web_search*` 工具落到 chat 目标时被压成「`type: web_search` + 假 `function` 对象」的混合形状，既不是 OpenAI 合法工具，也没有任何 loss 记录。

## 需求变更

[`docs/spec/providers/requirements.md`](../requirements.md)：

- 「请求翻译」节新增子节「工具定义跨 codec 保真」，明确 7 条行为契约（两种布局、chat 来源判别式、同协议零合成、跨协议默认值合成、`strict` 的协议位置、出站禁止伪字段、hosted 丢弃必须记 loss）。
- 末尾「变更记录」节追加 0007 条目。
- 该子节第 4 条（跨协议默认值合成）经裁决收敛为**按值映射**：Anthropic 出站对所有来源统一省略 `"function"` / `""`，chat 出站对 `type` 缺失或为 `""` 统一合成 `"function"`。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-providers/src/codec/tool_raw.rs`](../../../../sag-providers/src/codec/tool_raw.rs) | 新增。`IrTool.raw` 双轨约定的唯一实现点：`TOOL_EXTRA_KEY` / `FUNCTION_EXTRA_KEY` 常量；`is_dual_layer` 以 `__tool_extra` 键存在性作判别式；`tool_extras` 双轨读取（双层 → 两个存储对象原样返回；扁平 → `raw` 副本作工具级字段，并把 `strict` 提升到 function 级）；`tool_raw_type` 双轨读 `type`（供 hosted 判定）；`merge_extras` 把两层展开到工具根部（`or_insert`：调用方已写字段优先），`is_reserved_key` 过滤内部伪字段 |
| [`sag-providers/src/codec/mod.rs`](../../../../sag-providers/src/codec/mod.rs) | 注册 `mod tool_raw;`（crate 内部共享，`pub(crate)`，不对外 re-export） |
| [`sag-providers/src/codec/chat.rs`](../../../../sag-providers/src/codec/chat.rs) | `encode_tools` 改签名为 `(tools, &mut LossInventory)` 并按 `is_dual_layer` 分两支：**chat 来源路径逐字不变**（仍从 `__tool_extra` / `__function_extra` 各回原位，绝不发明 `type`，源里 `type` 为 `""` 时也逐字往返）；外来扁平 IR 走 `tool_extras` + 补默认 `"type": "function"`（`or_insert`，源里已有非空 `type` 优先；`type` 缺失或为 `""` 均合成 `"function"`）；hosted `web_search*` 丢弃并记 `LossReason::UnsupportedByTarget`（不再压成假 function 形状），loss 经既有 `encode_losses` 通道进 `CallLog` |
| [`sag-providers/src/codec/responses.rs`](../../../../sag-providers/src/codec/responses.rs) | `encode_tools` 改用 `tool_raw_type` / `merge_extras`：双层键展开到工具根部（`strict` 落根部），`is_reserved_key` 过滤伪字段（hosted 分支同样过滤）；无 `type` 时仍补 Responses 默认 `"function"` |
| [`sag-providers/src/codec/anthropic.rs`](../../../../sag-providers/src/codec/anthropic.rs) | `encode_tools` 改用 `tool_raw_type` / `merge_extras`（Anthropic 工具对象无 `function` 嵌套，两层字段均展开到根部），`is_reserved_key` 过滤伪字段（hosted 分支同样过滤）；**按值映射，对所有来源一致**：`type` 值为 `"function"` 或 `""` 时一律省略（Anthropic tool-type 枚举无 `"function"`，自定义工具本身即 function 语义）；`"custom"`、`web_search*` 等其余值逐字保留；缺失维持缺失 |

## 设计决策

| 决策 | 内容 |
|------|------|
| 判别式 | 「chat 来源」的唯一标记是 `raw` 中 `__tool_extra` 键的**存在性**（`is_dual_layer`）。它是结构标记而非业务字段，任何非 chat codec 都不会写入，因此可安全当作布局判别式；空对象 `{}` 也算双层 |
| 修复放在 encode 侧（D′） | 不改 Responses / Anthropic 的 decode 布局去迁就双层约定，而是让 encode 侧兼容读取两种布局。decode 布局被既有 pinned 测试与 hosted `web_search*` 形状断言钉住，改布局会波及三个 codec 的 round-trip 断言，收益仅是「布局更整齐」 |
| 同协议零合成 | chat↔chat 往返走原路径：`type` 只从 `__tool_extra` 还原，源里没有 `type` 就继续没有。既有不变量「绝不凭空发明 `type`」保持不变；源里 `type` 为 `""` 时逐字往返为 `""`，不升级为 `"function"`（红线不变量） |
| 跨协议允许合成默认值 | 跨协议编码边界允许补齐目标协议必填项。Responses / Anthropic 的 function 工具在 wire 上不带 `type`，而 OpenAI 上游把 `type` 当必填，故 chat 出站对外来扁平 IR 在 `type` 缺失或为 `""` 时合成 `"type": "function"`——这是补协议默认值，不是发明字段。反方向：Anthropic 出站**按值**处理 `type`（对所有来源一致），`"function"` 与 `""` 一律省略（其 tool-type 枚举无 `function`，自定义工具本身即 function 语义），其余值逐字保留 |
| `strict` 位置是协议语义 | 同一份 `strict` 在 chat 里属 `function.strict`，在 Responses 里属工具根部。helper 的展开写入保证跨协议时落到目标协议的正确位置，而不是随源布局漂移 |
| 丢弃必须记 loss | hosted `web_search*` 无法表达为 chat 工具，只能丢弃；丢弃走 `LossReason::UnsupportedByTarget` 并进 `CallLog`，禁止静默丢弃（网关既有「禁止静默丢失」契约） |
| 共享 helper 而非三份复制 | 布局约定收敛到 `tool_raw.rs` 一处，后续新增 codec 只需接 helper，避免再次漂移出第三、第四套约定 |

**Trade-off**:

- ✅ 四个缺陷同根因同修，且 chat↔chat 路径字节不变，回归面收敛在跨协议编码
- ✅ 出站 JSON 不再含 `__` 前缀内部伪字段，上游不会收到网关内部结构
- ✅ hosted 工具丢弃可观测（loss → `CallLog`），不再静默压成非法形状
- ❌ IR 仍存在两种 `raw` 布局，未做一次性迁移，helper 必须长期维护双轨读取分支
- ❌ chat 出站对外来 IR 合成 `"type": "function"`，属跨协议边界的补偿语义，读代码时需靠 helper 注释区分两条路径
- ❌ Anthropic 没有 `strict` 的对应语义，展开后该键仍以根部字段透传（不静默丢弃），由上游判定是否接受

## 测试

- `sag-providers/src/codec/tool_raw.rs` 内联单元测试：
  - `dual_layer_extras_are_read_per_level` — 双层布局：工具级 / `function` 级字段分别从 `__tool_extra` / `__function_extra` 读出
  - `flat_extras_move_strict_to_function_level` / `flat_extras_without_strict_leave_function_level_empty` — 扁平布局：`raw` 自身即工具级字段，`strict` 提升到 function 级
  - `empty_tool_extra_is_still_dual_layer` — 空对象 `{}` 也算双层，判别式只看键存在性
  - `merge_extras_flattens_dual_layer_to_root` / `merge_extras_flattens_flat_raw_to_root` / `merge_extras_keeps_caller_fields` — 展开写入：两层均落到工具根部，调用方已写字段优先
  - `merge_extras_never_emits_reserved_keys` — 伪字段永不出现在出站 JSON（含扁平 `raw` 携带孤立伪键的防御路径）
  - `tool_raw_type_reads_both_conventions` / `is_dual_layer_uses_key_presence_only` — 双轨 `type` 读取与判别式
- `sag-providers/src/codec/chat.rs`：
  - Responses 形状 IR（扁平 `raw`，含 `type` / `strict`）→ 出站工具带 `"type": "function"`，`strict` 落在 `function.strict`
  - Anthropic 形状 IR（扁平 `raw`，无 `type`）→ 出站工具补出 `"type": "function"`
  - chat 来源 IR（源工具无 `type`）→ 出站仍无 `type`（同协议零合成不变量回归）
  - hosted `web_search*` IR → 工具从出站 `tools` 中消失，且 loss 含 `tools[i]` + `unsupported_by_target`
- `sag-providers/src/codec/responses.rs`：chat 来源 IR → 出站工具无 `__tool_extra` / `__function_extra` 键，`strict` 位于工具根部
- `sag-providers/src/codec/anthropic.rs`：chat 来源 IR → 出站工具无伪字段，`type: "function"` 被省略；扁平来源的 `"function"` / `""` 同样省略，`"custom"` 等其余值逐字保留，缺失维持缺失
- **既有测试断言调整（本次唯一一处）**：`function_tool_named_web_search_is_not_hosted` 原先断言扁平 `raw` 出站 `type == "function"`，现改为断言 `type` 缺失——测试原意「名为 `web_search` 的 function 工具不得被误映射为 hosted `web_search_20250305`」保持不变，仅期望值随「按值映射」裁决更新
- `sag-dispatch/src/dispatch/chat/tests.rs`：hosted 工具走 chat 目标的 encode loss 汇总进入 `CallLog` loss 摘要（验证「禁止静默」端到端可达）
