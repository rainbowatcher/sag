---
created: 2026-10-25
updated: 2026-10-25
type: 修复
---

# 0002 — 隐私过滤 Minor 项收敛

## 动机

0001 落地后的 round2 评审留下 6 条 Minor。它们都不是功能缺失，而是「边界条件下行为不合预期」，且多数在默认配置下就能被用户看见：

1. **IR-only 字段的占位符不还原**：Anthropic / Responses ingress 的 IR 是直接从原始 body 解码的，携带 Chat 请求副本没有的字段（未知顶层键、`Reasoning` 的 thinking 文本、`ToolResult` 内容）。`mask_ir` 在这些字段上生成的占位符只存在于它自己的 `MaskReport.map` 里，而 dispatcher 只保留了 `mask_request` 的 map——模型一回显这类占位符，客户端就看到裸的 `[[EMAIL_…]]` 标记。
2. **非 ASCII `kind` 的 `placeholder_len` 偏小**：长度按 `kind.len()` 算，而 `make` 渲染的是 `kind.to_uppercase()`。`ß` → `SS`、`工号`（6 字节）这类情况下实际占位符比预算长，容量估算与切片边界会失配。
3. **CRLF 上游的流式响应完全不还原**：`RestoreWriter` 只认 `\n\n`，而 SSE 规范允许 `\r\n` 行尾，帧终止符因此也可能是 `\r\n\r\n`；命中帧从不被识别，整条流原样透传给客户端。
4. **15 位旧式身份证只做边界校验**：任何 15 位数字串（订单号、时间戳）都能命中 `id_card_cn`；且同一 span 与 Luhn 合法的 15 位卡号撞车时，配置顺序让**没有**校验位的旧式身份证压过**有**校验位的卡号。
5. **流式尾帧晚于终态记账**：`finish()` 的残余字节与 `finish_stream_terminal` 在同一次 poll 里完成，客户端最后一个可见字节可能晚于 `call_logs` / analytics 落库——观测上「请求已结束」早于「响应已发完」。
6. 上述各点缺少回归测试。

## 需求变更

- `requirements.md`「检测范围 → 内置检测器」：`id_card_cn` 的结构校验由「18 位 GB 11643 校验位；15 位旧式仅做边界校验」改为「18 位 GB 11643 校验位；15 位旧式要求第 7-12 位 `YYMMDD` 是合法日期（年份读作 `19YY`）」。
- `requirements.md`「检测范围 → 候选解析规则」：完全相同区间的胜者由「配置顺序在前者」改为「先看是否通过了真正的校验位，再退回配置顺序」；并明确 18 位身份证仍优先于 `bank_card`、Luhn 合法的 15 位卡号优先于 15 位旧式身份证。
- `requirements.md`「响应侧还原」：新增「IR 协议专属字段（未知顶层键、`Reasoning` thinking、`ToolResult`）的占位符同样还原」——脱敏覆盖两次扫描（Chat 请求副本 + IR），还原 map 是两者的合并。
- `requirements.md`「响应侧还原 → 流式」：帧分隔符由 `\n\n` 扩为 `\n\n` 与 `\r\n\r\n`；半截分隔符（`\r\n\r` | `\n`）跨 chunk 时不提前切帧。
- `requirements.md`「事件日志与可观测性」：新增「流式响应的最后一批字节（含隐私尾缓冲）先下发，`call_logs` / analytics 终态记账随后」的顺序约定。
- `requirements.md`「功能边界 → 不包含」：**无删除项**——「IR-only 占位符不还原」从未写进需求边界（只存在于评审记录，0001 的 changefile 也只描述了 IR 原地脱敏）。本次改为以新增行为描述落定，并在 `spec.md` 的「原地脱敏 IR」决策里补记 map 合并。
- `spec.md`：「关键文件」行号与职责更新（map 合并、`terminal_pending`、`FRAME_SEPS`、`verifies_check_digit`）；「检测流水线」排序键改为 `(start 升序, end 降序, check_digit 降序, order 升序)`；「流式还原状态机」分隔符判定改为「存在 `\n\n` 或 `\r\n\r\n`」；「关键决策」补 4 条（15 位身份证以出生日期替代校验位、同 span 校验位优先、两份 map 合并、尾帧先于终态），并改写「按 SSE 帧还原」的分隔符描述；「不变式」补 6 条；「测试策略」计数更新为 57 / 277。
- 本模块「变更记录」节与 `docs/spec/CHANGELOG.md` 追加 0002 条目。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-privacy/src/placeholder.rs`](../../../../sag-privacy/src/placeholder.rs) | `placeholder_len` 改按 `kind.to_uppercase().len()` 计算（渲染长度而非输入长度）；新增 `PlaceholderMap::extend`——逐条 `or_insert`，冲突保留既有条目（占位符确定性使冲突必为同一明文），合并顺序无关 |
| [`sag-privacy/src/rules.rs`](../../../../sag-privacy/src/rules.rs) | 新增 `Checksum::verifies_check_digit`（`Luhn` / `Iban` 恒 true，`IdCardCn` 仅 18 位为 true）；`id_card_cn_ok` 的 15 位分支改走 `legacy_birth_date_ok`（第 7-12 位按 `19YY` + `days_in_month` 校验，含闰年规则） |
| [`sag-privacy/src/engine.rs`](../../../../sag-privacy/src/engine.rs) | `Candidate` 增 `check_digit` 字段；`scan` 的排序键改为 `(start 升序, end 降序, check_digit 降序, order 升序)`，让「通过校验位」的规则压过「只过结构过滤」的规则 |
| [`sag-privacy/src/restore.rs`](../../../../sag-privacy/src/restore.rs) | `FRAME_SEPS = [b"\n\n", b"\r\n\r\n"]`；`find_sep` 取最早出现者并整体匹配（分隔符被 chunk 劈成 `\r\n\r` \| `\n` 时不提前切帧）；还原帧用**原**分隔符收尾 |
| [`sag-dispatch/src/dispatch/chat/mod.rs`](../../../../sag-dispatch/src/dispatch/chat/mod.rs) | `Masked` 分支合并两份 map（约 L224-243）：`let mut restore_map = report.map; restore_map.extend(&ir_report.map);`；`hits` 仍只取 `mask_request` 的计数，避免两份扫描对共享文本重复计数 |
| [`sag-dispatch/src/dispatch/stream.rs`](../../../../sag-dispatch/src/dispatch/stream.rs) | 流式状态新增 `terminal_pending`：终点先 yield 隐私尾缓冲并置位，下一次 poll 才 `finish_stream_terminal`；`Error` 后紧跟流结束（无显式 `End`）的路径汇入同一出口，同样先 flush 尾缓冲 |

## 测试

新增 11 个用例（`sag-privacy` +8、`sag-dispatch` +3）：

| 用例 | 位置 | 断言 |
|---|---|---|
| `placeholder_len_counts_bytes_of_the_uppercased_kind` | `sag-privacy/src/placeholder.rs` | `工号` / `straße` / `ﬁle` / `ÉMAIL` / `π` 等非 ASCII kind 下 `make(..).len() == placeholder_len(kind)`；`工号` 精确为 23 |
| `extend_merges_maps_and_keeps_existing_entries` | `sag-privacy/src/placeholder.rs` | 合并两份 map 后两个占位符都能还原；冲突不覆盖既有条目；空 map 两个方向都无副作用 |
| `legacy_id_needs_a_plausible_birth_date` | `sag-privacy/src/rules.rs` | 月 13 / 月 00 / 日 32 / 日 00 / 2 月 30 日 / `900229`（1900 非闰年）全拒；`920229` / `900228` / `901231` 接受；18 位不重新校验日期 |
| `verifies_check_digit_separates_check_digits_from_filters` | `sag-privacy/src/rules.rs` | `Luhn` / `Iban` 恒 true；`IdCardCn` 18 位 true、15 位 false |
| `luhn_verified_fifteen_digits_beat_the_legacy_id_rule` | `sag-privacy/src/engine.rs` | 同一 15 位 span 日期合法且 Luhn 合法 → `bank_card`；只开 `id_card_cn` 时仍报身份证；日期非法 + Luhn 合法 → 只有 `bank_card`；日期合法 + Luhn 失败 → `id_card_cn` |
| `restores_frames_separated_by_crlf` | `sag-privacy/src/restore.rs` | `\r\n\r\n` 帧被还原且保留自己的终止符；无命中的 CRLF 帧逐字节透传 |
| `restores_mixed_lf_and_crlf_frames` | `sag-privacy/src/restore.rs` | 单个 chunk 内 LF / CRLF 混排 3 帧全部还原 |
| `separator_split_across_chunks_waits_for_the_rest` | `sag-privacy/src/restore.rs` | `\r\n\r` \| `\n`、`\r\n` \| `\r\n`、`\n` \| `\n` 三种半截分隔符都不提前切帧 |
| `privacy_tail_flushes_before_terminal_bookkeeping` | `sag-dispatch/src/dispatch/stream.rs` | 完整帧先还原下发 → 尾缓冲原样下发 → 此时 `call_logs == 0` → 下一次 poll body 结束且 `call_logs == 1` |
| `privacy_tail_flushes_when_upstream_errors_out` | `sag-dispatch/src/dispatch/stream.rs` | `Raw` + `Error`（无显式 `End`）也先 flush 尾缓冲，再走终态记账 |
| `dispatch_chat_restores_ir_only_placeholders` | `sag-dispatch/src/dispatch/chat/tests.rs` | Anthropic ingress 的 `x-trace-note` 与 `thinking` 块在 provider 侧已是占位符、`type` 仍为 `thinking`（`Reasoning` 不降级为 `Text`）；三个邮箱在客户端全部还原（其中两个的占位符只有 `mask_ir` 记录过，证明 map 已合并） |

全量计数：`cargo test -p sag-privacy` 57 个单测（原 49），`cargo test -p sag-dispatch` 277 个（原 274）。
