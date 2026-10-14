---
created: 2026-09-30
updated: 2026-09-30
type: 修复
---

# 0005 — 共享 HTTP client 连接池 idle 调优（降低 stale 连接复用）

## 动机

`build_http_client`（`sag-core/src/http_client.rs`，provider + admin 共享）此前沿用 reqwest 默认连接池参数（idle timeout 90s、每 host 无限 idle 连接）。长连接在空闲期被上游 / 中间设备单方面关闭后仍留在池中，下一次复用即拿到已死连接，表现为流中途 `peer closed connection without sending TLS close_notify` / EOF。

本次在模块级收紧 idle 池，降低「复用已被对端关闭的连接」的概率。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-core/src/http_client.rs`](../../../../sag-core/src/http_client.rs) | 模块级写死 `POOL_IDLE_TIMEOUT = 30s` / `POOL_MAX_IDLE_PER_HOST = 4`，并在 proxy 与 direct 两个分支都应用到 `ClientBuilder` |

## 设计决策

常量取值（对比 reqwest 默认）：

| 常量 | 取值 | reqwest 默认 |
|------|------|--------------|
| `POOL_IDLE_TIMEOUT` | `30s` | `90s` |
| `POOL_MAX_IDLE_PER_HOST` | `4` | `usize::MAX` |

| 决策 | 内容 |
|------|------|
| 写死而非配置化 | 连接层需快速实测迭代；上游 idle 差异按部署而变，配置化收益低、误配风险高 |
| 只调 idle 池 | 两个常量只约束空闲池中可被复用的连接，**不截断在途流**——与 client 级 total timeout 截断长流有本质区别 |
| 禁回退 total timeout | 成功流式响应不受 client 级总 deadline 约束（非流式 request-level `timeout`，流式 TTFB + 可选 chunk idle）；给 `build_http_client` 加回 `.timeout(...)` 会重新掐断健康长流 |
| 调参顺序 | 若 TLS 握手率升高，优先上调 `POOL_MAX_IDLE_PER_HOST`，不优先降低 idle；若上游 / proxy idle 更短，再下调 idle 并保留 10–15s 余量 |

## Trade-off

- ✅ 改动面小、见效快，不触碰 failover / 流式超时语义
- ✅ 与 0006 流式超时边界兼容（0006 负责「已开始的流不被总超时掐断」；本变更负责「减少拿到已死连接」）
- ❌ 只能降低 stale 复用概率，**不修**「已断的流」（那是方案 C′ 的范围）
- ❌ 取值写死，极端上游 idle 更短的部署需实测后手工调整
- ❌ admin 临时测试客户端经共享 builder 获得相同池默认（短探测请求可接受）

## 测试

`sag-core/src/http_client.rs` 内联单元测试：

- `pool_connection_constants_match_plan` — 钉住 `POOL_IDLE_TIMEOUT` / `POOL_MAX_IDLE_PER_HOST` 的取值，静默改值会失败
- `pool_options_coexist_with_proxy_and_timeout` — pool 选项与 proxy / timeout 构造共存
