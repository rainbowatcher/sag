---
created: 2026-06-14
updated: 2026-07-22
---

# 熔断器

## 目标

当上游持续返回错误时，暂时跳过问题 target，避免浪费重试资源，给上游恢复时间。

## 预期行为

### 错误分类

上游错误按 response body 关键词分为三类：

| 冷却类型 | 触发条件 | 默认冷却 |
|---------|---------|---------|
| 额度耗尽 | HTTP 429 **或 403** + body 含 quota/billing 关键词 | 3600s (1h) |
| 速率限制 | HTTP 429（非额度相关） | 10s |
| 通用故障 | 5xx / 网络超时 / 连接错误 / 上游超时 | 60s |

分类逻辑：对 `Upstream{status: 429|403}` 先精确匹配 `"rate limit"` → `RateLimited`，再匹配 quota/billing 关键词 → `QuotaExhausted`，其余 → `Generic`。403-非quota 不走熔断（归 provider 持续性，见 [4xx 归因策略](../failover-4xx-policy/spec.md)）。

### 冷却行为

- 目标重试耗尽后 → 按错误类型打开对应冷却时长的熔断器（`Open` 态）
- 冷却期内 → 目标在路由选择时被跳过（视为不可用），但**按 `probe_interval_secs` 周期性允许一次探测**
- 探测触发：冷却期内每当 `next_probe_at` 到期，下一个请求作为探测打一次上游（带 jitter 错峰）；冷却期结束（首次探测也到期）同样触发
- 探测成功 → 目标关闭熔断器（`Closed`），恢复正常
- 探测失败 → 仅将 `next_probe_at` 推后一个 `probe_interval_secs` 继续轮询（不重开整段冷却）
- 并发控制：半开期间**全局仅 1 个**请求作为探测打上游；其余并发请求仍按"电路打开"跳过，走下一 target 或 all_exhausted
- 探测槽自愈：探测请求被客户端取消（断连）导致 `probe_in_flight` 未正常归还时，下一次 `try_acquire_probe` 在超过 `STALE_PROBE_THRESHOLD`（3600s）后会兜底回收僵死槽位，目标不会被永久跳过
- 冷却期结束恢复路径：
  - StickyFailover：继续优先使用 sticky target
  - FillFirst：按优先级尝试首选
  - RoundRobin：回到轮转顺序

### 半开探测与提前感知

冷却时长（如 `quota_exhausted_secs`）是"最少等待下界"，**不是**恢复的唯一时机。目标在冷却窗口内即被周期探测，因此上游（如额度中途补充）一旦恢复，最多 `probe_interval_secs` 内即被发现并解除跳过——无需傻等满整个冷却窗口。

### 无请求时自动关闭

冷却期是时间驱动的，不需要请求触发。冷却时间到后目标自动可用（下一次探测到期即放行）。

## 配置 Schema

```toml
[circuit_breaker]
quota_exhausted_secs = 3600   # 默认 1h
rate_limited_secs = 10        # 默认 10s
generic_secs = 60             # 默认 60s
probe_interval_secs = 60      # 默认 60s，冷却期内探测间隔（带 jitter）
```

## 功能边界

### 包含

- 按错误类型分类
- 时间驱动的冷却
- 自动恢复
- 半开状态（half-open）探测
- 冷却期周期探测（提前感知恢复）

### 不包含

- 滑动窗口计数
- 基于成功率的熔断（仅时间驱动）
- 探测次数上限（真耗尽目标无限期按 interval 低频探测直到恢复）

## 与其它模块的交互

- **路由**：检查目标的熔断器状态
- **调度**：重试耗尽后打开熔断器；`resolve_provider` 失败时若已占用探测槽须释放
- **StickyFailover 粘性记忆**：手动关闭熔断器（`POST /api/circuit-breaker/close`）时同步清除该 model 的 sticky active target 记忆，使下一次请求回退到 fill-first 优先级顺序。语义与 `sticky_failover_reset_secs` 自然过期一致。

## 变更记录

- [0001 初始化](changes/0001-initial.md)
- [0002 熔断半开探测与冷却期周期探测](changes/0002-half-open-periodic-probe.md)
- [0003 探测失败隐瞒冷却下界与流式取消探测槽泄漏修复](changes/0003-probe-lifetime-fix.md)
- [0004 熔断状态可观测化](changes/0004-observability.md)
- [0005 手动关闭熔断时清除 StickyFailover 粘性记忆](changes/0005-manual-close-clears-sticky.md)
- [0006 熔断器逻辑概念内聚与探测槽生命周期治理（RAII）](changes/0006-concept-consolidation-and-probe-guard-lifecycle.md)（无行为变更：探测槽归还收敛为消费式 `ProbeGuard` / `apply_circuit_outcome`，`force_close_circuit` 下沉 `sag-state`）
