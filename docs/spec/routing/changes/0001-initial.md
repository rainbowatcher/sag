# 0001 — 路由系统初始化

**日期**：2026-06-03
**类型**：新增

## 动机
网关核心功能：将客户端模型名映射到供应商和实际模型，支持优先级故障转移。

## 需求变更
- requirements.md 全部初始定义

## 代码变更
| 文件 | 变更 |
|------|------|
| [sag-config/src/config.rs](https://github.com/rainbowatcher/sag/blob/main/sag-config/src/config.rs) | RouteConfig、RouteTarget、Strategy 枚举 |
| [sag-dispatch/src/dispatch/resolve.rs](https://github.com/rainbowatcher/sag/blob/main/sag-dispatch/src/dispatch/resolve.rs) | 路由匹配 + 目标选择 |
| [sag-dispatch/src/order.rs](https://github.com/rainbowatcher/sag/blob/main/sag-dispatch/src/order.rs) | 目标排序 + shuffle 缓存 |
| [sag-dispatch/src/state.rs](https://github.com/rainbowatcher/sag/blob/main/sag-dispatch/src/state.rs) | DispatchRuntime（持有缓存状态） |

## 影响的需求条目
- 全部（初始定义）
