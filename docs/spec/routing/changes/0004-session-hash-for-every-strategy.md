---
created: 2026-10-21
updated: 2026-10-25
type: 修改
---

# 0004 — call_logs 会话哈希不区分策略记录

## 动机

0003 把 `call_logs.session_key` 限定为 `session_sticky` 路由，NULL 混合了
「未识别出会话」与「有会话但路由不粘」两种含义。会话归属是请求的**事实**、
粘性是路由的**行为**，事实列不应依赖行为：排障时「把这个会话的所有调用拉出来」
不取决于路由策略，且给路由开 `session_sticky` 前需要会话分布数据评估收益。

## 需求变更

- 「预期行为 → 会话粘性路由」第 5 条改为：会话键以哈希形式存在于状态、
  管理端快照与 `call_logs.session_key`；**凡识别出会话即记录（不区分路由策略）**，
  NULL 仅表示「请求未携带可识别的会话标识」。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-dispatch/src/dispatch/chat/mod.rs`](../../../../sag-dispatch/src/dispatch/chat/mod.rs) | 去掉 `session_log_key` 的 strategy 闸门，所有策略记录哈希 |
| [`sag-dispatch/src/dispatch/image.rs`](../../../../sag-dispatch/src/dispatch/image.rs) | image pipeline 各终端行（成功/失败/熔断跳过/耗尽/取消）统一记录 |
| [`sag-dispatch/src/dispatch/resolve.rs`](../../../../sag-dispatch/src/dispatch/resolve.rs) | pre-resolve 失败行（`__no_route__` / `__all_disabled__`）统一记录 |

## 测试

- 新增 `call_log_records_session_hash_for_every_strategy`：StickyFailover 路由
  带会话请求的 call_log 行含哈希；无会话行为 NULL（NULL 仅剩此含义）
