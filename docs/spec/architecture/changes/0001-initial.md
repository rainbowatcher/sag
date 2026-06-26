---
created: 2026-06-03
updated: 2026-06-03
type: 新增
---

# 0001 — 项目初始化与核心架构

## 动机
SAG 项目启动。需要一个模块化、可扩展的 LLM API 反向代理网关。

## 需求变更
- requirements.md 全部初始定义

## 代码变更
| 文件 | 变更 |
|------|------|
| [Cargo.toml](https://github.com/rainbowatcher/sag/blob/main/Cargo.toml) | workspace 定义，8 个子包 |
| [sag-core/src/lib.rs](https://github.com/rainbowatcher/sag/blob/main/sag-core/src/lib.rs) | 共享类型、AppError、CircuitHint |
| [sag-core/src/types.rs](https://github.com/rainbowatcher/sag/blob/main/sag-core/src/types.rs) | ChatCompletionRequest/Response、ImageGenerationRequest |
| [sag-gateway/src/main.rs](https://github.com/rainbowatcher/sag/blob/main/sag-gateway/src/main.rs) | 启动编排、配置加载、依赖注入、优雅关闭 |
| [sag-gateway/src/tracing_init.rs](https://github.com/rainbowatcher/sag/blob/main/sag-gateway/src/tracing_init.rs) | tracing-subscriber 初始化 |

## 影响的需求条目
- 全部（初始定义）
