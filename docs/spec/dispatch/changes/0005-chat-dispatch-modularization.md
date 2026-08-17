---
created: 2026-08-17
updated: 2026-08-17
type: 重构
---

# 0005 — 聊天分发单文件复杂度解耦与模块化

## 动机

`sag-dispatch/src/dispatch/chat.rs` 随网关演进膨胀至近 4000 行（158KB），同时承载了：
1. 调度主循环与重试退避；
2. 熔断探测与并发槽位释放；
3. 上下文与取消中断守卫（`DispatchCancelGuard`）；
4. 请求 Payload 与 Model 覆写合并；
5. 流式与非流式响应转换；
6. 庞大的空响应检测规则与内联单元测试（占 70%+ 行数）。

单文件超载导致可读性与维护成本增高。需在保持所有对外公共 API 签名、行为、不变式及 100% 测试覆盖完全不变的前提下，对 `chat` 模块进行细粒度拆分。

## 需求变更

- 行为与功能无变更（纯重构）。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-dispatch/src/dispatch/chat/mod.rs`](../../../sag-dispatch/src/dispatch/chat/mod.rs) | 模块主入口，专注核心调度循环 `dispatch_chat` 与子模块 re-export |
| [`sag-dispatch/src/dispatch/chat/attempt.rs`](../../../sag-dispatch/src/dispatch/chat/attempt.rs) | `ChatAttempt` 枚举与 `build_target_attempt_fact` 事实构造辅助函数 |
| [`sag-dispatch/src/dispatch/chat/empty.rs`](../../../sag-dispatch/src/dispatch/chat/empty.rs) | 空响应检测（`is_empty_response_data`、`is_empty_non_stream_response` 等）及首 chunk 校验与对应单元测试 |
| [`sag-dispatch/src/dispatch/chat/tests.rs`](../../../sag-dispatch/src/dispatch/chat/tests.rs) | 全量内联调度集成场景测试 |
| [`docs/spec/dispatch/spec.md`](../spec.md) | 更新关键文件清单 |

## 测试

- `sag-dispatch` 相关单元测试与集成测试全部通过（`cargo test -p sag-dispatch --tests`）。
