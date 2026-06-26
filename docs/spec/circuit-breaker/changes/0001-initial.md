---
created: 2026-06-14
updated: 2026-06-14
type: 新增
---

# 0001 — 熔断器初始化

## 动机
上游频繁返回错误时跳过问题 target，避免浪费重试，给上游恢复时间。

## 需求变更
- requirements.md 全部初始定义

## 代码变更
| 文件 | 变更 |
|------|------|
| [sag-dispatch/src/circuit_breaker.rs](https://github.com/rainbowatcher/sag/blob/main/sag-dispatch/src/circuit_breaker.rs) | 熔断器状态管理（HashMap + 时间驱动冷却） |
| [sag-core/src/types.rs](https://github.com/rainbowatcher/sag/blob/main/sag-core/src/types.rs) | CircuitHint 枚举（QuotaExhausted / RateLimited / Generic） |
| [sag-config/src/config.rs](https://github.com/rainbowatcher/sag/blob/main/sag-config/src/config.rs) | CircuitBreakerConfig（三个冷却时长） |

## 影响的需求条目
- 全部（初始定义）
