---
created: 2026-10-26
updated: 2026-10-26
type: 新增
---

# 0003 — 隐私边界与 `session_registry` PII 例外

## 动机

分析模块新增了 `session_registry` 表（[analytics 0009](../../analytics/changes/0009-session-registry-and-logs-session-envelope.md)），
为日志页会话信息卡保存**可读的**会话身份：原始 `conversation_id`（≤128 字符）与原始（未脱敏）首条用户消息
预览（≤160 字符），默认保留 7 天。

这是一处经用户裁定的隐私边界：**脱敏（`[privacy]`）只作用于发往上游大模型的请求内容**；
`session_registry` 是本地管理面可见数据，因此存原始值。该裁定已经写在
`sag-core/src/session.rs` 的「Approved PII exception」注释、`sag-core/src/session_preview.rs`、
analytics 0009 changefile 与 `analytics/spec.md`，但**本模块的 requirements / spec 一个字都没有**。

后果是口径裂缝，且落在安全边界上：审计者或运维要回答「网关开了 `[privacy]` 之后，原始 PII 还会不会落在某个表里？」
时，权威口径是本模块的 requirements / spec；而那里此前的表述——「检测请求中所有会发往上游的字符串」、
「事件日志不记录明文」、「隐私过滤不写入 analytics」——会让人得出「不落盘」的结论，与事实相反。

本次只补**边界与例外**，不改动检测、脱敏、还原的**行为**描述。

## 需求变更

- 「预期行为 → 检测范围」开头新增边界说明：本模块的承诺边界是**上游出口**（发往 provider 的请求内容 +
  回客户端的响应还原）；本地管理面的 `session_registry` 是已批准的例外，不属于本模块的检测范围。
- 「预期行为 → 事件日志与可观测性」：「隐私过滤不新增 analytics 记录」改为「不改写既有 `call_logs` 行的字段」，
  并新增一条明文落盘唯一例外：`session_registry` 存掩码前的原始值、仅供管理面板读取、有独立保留期
  （`session_retention_days`，默认 7 天，`0` = 永久）；明确开启 `mask` 不会把占位符写进该表，也不会让原始值
  进入 `call_logs` / 会话亲和 map / admin 快照。
- 「功能边界 → 不包含」新增一条：本地管理面持久化的原始会话身份与首条消息预览（PII carve-out，不在掩码覆盖范围内）。
- 「与其它模块的交互 → 分析」补齐例外段落 + 指向 analytics 0009 changefile 与 `sag-core/src/session.rs` 的链接，
  并说明采集点在掩码之前（`chat/mod.rs` 的 `registry_request` / `registry_ir`）。
- 本模块「变更记录」节追加 0003 条目。

## 规格变更

- 「关键决策」新增「掩码边界是上游出口，`session_registry` 是已批准的例外」：写入点为什么在掩码之前、
  保留期为什么独立、范围为什么只覆盖一张表，以及两条 Trade-off（放弃「注册表也存占位符」、
  放弃「不存首条预览」）。
- 「不变式」新增一条：掩码边界恒为「上游出口」；本地持久化面取掩码**前**的请求与 IR，不得写入占位符。

## 代码变更

无。本次只补文档边界（代码侧的例外在 analytics 0009 中已落地：`sag-core/src/session_preview.rs`、
`sag-analytics/src/session_registry.rs`、`sag-dispatch` 的四处 `log_session` 写入点）。

## 未涉及

- 检测范围、占位符形态、三种模式、响应侧还原、fail-closed、配置校验：行为与描述均未改动。
- 隐私过滤是否会记录「有多少请求被 `block` 挡掉」：仍是 v1.5 候选（见 spec 的 `block / 503 早退不写 analytics`）。
