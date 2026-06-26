---
created: 2026-06-14
updated: 2026-06-14
---

# 熔断器 — 规格

## 关键文件

- [sag-dispatch/src/circuit_breaker.rs](https://github.com/rainbowatcher/sag/blob/main/sag-dispatch/src/circuit_breaker.rs) — 熔断器状态管理
- [sag-core/src/types.rs](https://github.com/rainbowatcher/sag/blob/main/sag-core/src/types.rs) — `CircuitHint` 枚举（错误分类）
- [sag-config/src/config.rs](https://github.com/rainbowatcher/sag/blob/main/sag-config/src/config.rs) — `CircuitBreakerConfig` 配置段

## 熔断器状态机

```mermaid
stateDiagram-v2
    [*] --> Closed: target 正常
    Closed --> Open: 重试耗尽 → 分类错误 → 记录 opened_at + cooldown
    Open --> Closed: SystemTime::now() >= opened_at + cooldown
```

## 关键决策

### 简单时间驱动 vs 半开探测

当前实现是最简单的时间驱动熔断器：到时间就恢复。没有 half-open 状态（发送探测请求验证恢复）。

**Trade-off**:
- ✅ 实现极其简单（几十行代码）
- ✅ 不会产生额外的"探测请求"消耗上游 quota
- ❌ 没有 half-open 意味着无法感知"上游是否真的恢复了"
- ❌ 冷却期内所有请求都跳过，即使上游在 1 秒后就恢复了

对于 LLM API 网关的场景，这个 trade-off 是合理的：
- 上游 429 通常有明确的 `Retry-After` 或固定的 cooldown
- 不会像微服务那样需要快速 failover
- 冷却时长保守设置（10s ~ 1h）已经足够

### `CircuitHint` 的分类粒度

```rust
pub enum CircuitHint {
    QuotaExhausted,  // 1h
    RateLimited,     // 10s
    Generic,         // 60s
}
```

**Trade-off**: 三种分类 vs 更多细分：
- 三种覆盖了主要的故障模式
- 更多分类（如区分 timeout 和 500）会增加配置复杂度，但效用有限

### 全局状态 vs 每 target 状态

熔断器状态存储在 `DispatchRuntime` 的 `HashMap<(provider, real_model), CircuitState>` 中。

**Trade-off**: 全局 HashMap vs 嵌入 RouteTarget：
- 全局：简单，热重载时只需清除对应 key
- 嵌入：与 target 生命周期绑定，但 target 可能在热重载时被删除

选择全局是因为热重载安全——即使 target 被删除又重建，状态可以通过 (provider, real_model) key 找到。

## 不变式

- 冷却期按 `opened_at + cooldown_secs` 计算（不依赖定时器）
- 同一个 target 的多个错误不叠加冷却（取最近的）
- 熔断器打开时记录的是当时的时间戳，不是 UTC 字符串

## 测试策略

- `sag-dispatch/src/circuit_breaker.rs` — 单元测试
- 关键场景：冷却期计算、时间穿越、错误分类

## 变更记录
- [0001 初始化](changes/0001-initial.md)
