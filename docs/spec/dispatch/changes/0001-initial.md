# 0001 — 调度系统初始化

**日期**：2026-06-03
**类型**：新增

## 动机
协调请求分发、重试退避和流式处理。解耦路由选择和供应商调用。

## 需求变更
- requirements.md 全部初始定义

## 代码变更
| 文件 | 变更 |
|------|------|
| [sag-dispatch/src/dispatch/chat.rs](https://github.com/rainbowatcher/sag/blob/main/sag-dispatch/src/dispatch/chat.rs) | 聊天补全分发（非流式 + 流式） |
| [sag-dispatch/src/dispatch/image.rs](https://github.com/rainbowatcher/sag/blob/main/sag-dispatch/src/dispatch/image.rs) | 图片生成分发 |
| [sag-dispatch/src/dispatch/stream.rs](https://github.com/rainbowatcher/sag/blob/main/sag-dispatch/src/dispatch/stream.rs) | SSE 流式处理 |
| [sag-dispatch/src/dispatch/retry.rs](https://github.com/rainbowatcher/sag/blob/main/sag-dispatch/src/dispatch/retry.rs) | 指数退避重试 |
| [sag-dispatch/src/state.rs](https://github.com/rainbowatcher/sag/blob/main/sag-dispatch/src/state.rs) | DispatchRuntime |

## 影响的需求条目
- 全部（初始定义）
