---
created: 2026-10-19
updated: 2026-10-19
type: 修复
---

# 0004 — 响应体超限不再毁坏响应：有界收集 + 帧链重组

## 动机

`log_http` 为了记录响应预览，把响应 body 全量读进内存：

```rust
let bytes = axum::body::to_bytes(body, MAX_BODY_READ).await.unwrap_or_default();
...
Response::from_parts(parts, Body::from(bytes))
```

`MAX_BODY_READ` = `DEFAULT_MAX_REQUEST_BODY_BYTES`（10 MB）。`to_bytes` 在超过上限时返回 `Err(LengthLimitError)`，而 `unwrap_or_default()` 把它变成**空 body**，再配合原样回填的 `parts`（headers 未改）发回客户端。于是产生三个缺陷，且都属于「日志中间件改坏了业务响应」：

1. **响应被毁**：> 10 MB 的响应（大 `tools` 数组、长 prompt 回显、批量 JSON）到达客户端时 body 为空，但 `content-length` 仍声明全长 —— 客户端按 header 读取会挂起到超时或被截断，HTTP 语义已坏，且**没有任何错误信号**。
2. **content-length 与 body 不一致**：因为修复方向必须是「headers 不动、body 完整」，而不是「补一个正确的 content-length」——后者仍然丢数据。
3. **body 错误被吞**：流中途的 `Err`（上游断开、编码错误）同样被 `unwrap_or_default()` 折叠成空 body，日志里只有一行无 body 的 `response`，既没有 WARN 也没有 `body_error`，与「正常空响应」无法区分。

请求侧有孪生缺陷：`known_small` 路径（content-length ≤ 512 KiB）用 `to_bytes(body, REQUEST_PREVIEW_MAX).unwrap_or_default()`。客户端谎报 / 少报 `content-length` 时实际 body 超限，或流中途出错，handler 收到的是**空 body** —— 表现为莫名其妙的 400 / 422，而不是「请求体读取失败」，日志里同样没有痕迹。

## 方案对比

| 方案 | 结论 |
|------|------|
| 纯流式 tee（逐帧转发，同时累计前 512 B 作日志） | 能保证不改 body，但要改写 ≤ 10 MB 的既有路径（不再缓冲、不再回填 `Body::from(bytes)`），等于用一个新实现替换掉现在字节级正确的路径，回归面大；且 tee 本身也要处理「日志已满仍继续转发」，与选定方案的第二段逻辑等价，收益只是少一次重组 |
| 只修日志（超限时打 `truncated` 标记，body 依旧丢弃） | 解决的是「日志骗人」，不是「响应被毁」：客户端仍然拿到空 body + 全长 `content-length`。**不满足「中间件不得改变响应体」** |
| 提高 `MAX_BODY_READ` | 只是把阈值往后挪，任意大的响应仍会在某个点被毁；且内存占用随响应体线性增长，网关不能接受 |
| **有界收集 + 帧链重组（选定）** | 见下 |

选定方案的关键前提是：`to_bytes` 超限时**丢弃已收集数据**（返回 `Err` 而非「部分数据 + 标记」），且 body 被消费后不可重放。因此必须下沉到帧级：自己收集 `Body::into_data_stream()` 的帧，才能在超限时把「已收集的帧」与「尚未读取的剩余流」拼回去。

两段式行为：

- **未超限**：收集完所有帧 → `Body::from(collected)`。与 `to_bytes` 路径字节级一致，日志字段（`body_size` / `preview`）也完全不变。
- **超限**：停止收集（保留已收集帧）→ 用 `stream::iter(已收集帧).chain(剩余流)` 重组 `Body::from_stream(...)` 全量转发，headers 原样不动。日志只打 512 B preview + 截断标记，不再假装记录了完整 body。
- **body 错误**：`Err` 不再折叠成空 body，而是记 `warn!`（`body_error=true`）并把错误重新注入重组后的流，让客户端看到真实的传输失败。

## 需求变更

- requirements.md「HTTP 请求日志」：新增「响应体日志必须是有界收集：超过 10 MB 时转发完整 body（headers 不变），日志只记录 512 B 预览 + 截断标记」
- requirements.md「HTTP 请求日志」：新增「body 流中途错误不得被吞成空 body：必须记录 WARN 并把错误传播给客户端」
- requirements.md「HTTP 请求日志」：新增「日志中间件在任何情况下不得改变响应体字节」

## 代码变更

| 文件 | 变更 |
|------|------|
| [sag-router/src/middleware/http_log.rs](../../../../sag-router/src/middleware/http_log.rs) | 响应路径改为「帧级有界收集 + 帧链重组」：未超限回填 `Body::from(collected)`（字节级不变），超限时把已收集帧与剩余流 `chain` 后全量转发、headers 不动，日志打 `truncated=true` / `collected=<n>` |
| [sag-router/src/middleware/http_log.rs](../../../../sag-router/src/middleware/http_log.rs) | body 流 `Err` 不再 `unwrap_or_default()`：记 `warn!`（`body_error=true`）并把错误重新注入流传播给客户端，message 为 `"response body error"` |
| [sag-router/src/middleware/http_log.rs](../../../../sag-router/src/middleware/http_log.rs) | 请求侧孪生缺陷同样改为有界收集 + 帧链重组：`Err` 记 `warn!`（message `"request body error"`）并传播，不再给 handler 一个空 body |

## 日志字段契约

新增字段全部 **additive**（仅在对应条件成立时出现），既有 `≤ 10 MB` 路径的字段集合零变化：

| 字段 | 类型 | 出现条件 | 含义 |
|------|------|----------|------|
| `truncated` | `bool` | 响应体超过 `MAX_BODY_READ`（10 MB） | 日志预览不完整，body 本身已完整转发 |
| `collected` | `usize` | 同上 | 截断前实际收集到的字节数（用于确认阈值与响应体量级） |
| `body_error` | `bool` | body 流中途出错 | 该次请求的 body 传输失败 |

message 保持 `"response"` / `"request body error"` / `"response body error"`，便于既有 grep / 告警规则沿用。

## 不变式影响

- 新增：**中间件任何情况下不得改变或截断响应体；日志超限时只可省略日志内容，且必须打截断标记。**
- 既有「压缩层在 `log_http` 之外（日志看到压缩前明文 body）」的层序契约不变 —— 本修复只改 body 的收集方式，不动 `parts`/headers，也不调整 `routes.rs` 的 layer 顺序。

## 测试

- 响应体 ≤ 10 MB：断言回填字节与上游逐字节相等（含非 UTF-8 字节），日志仍带 `body_size` / `preview`，**不含** `truncated` / `collected`
- 响应体 > 10 MB：断言客户端收到的字节数等于上游总长、`content-length` header 未被改写；日志行含 `truncated=true`、`collected=10485760`、preview 以截断标记结尾，且**不含**完整 body
- 响应流中途返回 `Err`：断言错误传播到客户端（而非 200 + 空 body），日志出现 `response body error` + `body_error=true`
- 请求侧超限（content-length 谎报为 ≤ 512 KiB，实际 > 512 KiB）：断言 handler 收到完整 body，日志出现 `request body error` + `body_error=true`
- 回归：[sag-router/tests/http_log_large_body.rs](../../../../sag-router/tests/http_log_large_body.rs) 的大 body / SSE 响应行用例保持通过（SSE 分支不消费 body 的既有行为不变）

## 影响的需求条目

- 「HTTP 请求日志」（新增三处约束：响应体完整转发、截断标记、body 错误传播）
