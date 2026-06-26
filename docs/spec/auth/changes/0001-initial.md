# 0001 — 认证系统初始化

**日期**：2026-06-03
**类型**：新增

## 动机
网关需要可选的 Bearer token 认证保护模型 API 和管理 API。

## 需求变更
- requirements.md 全部初始定义

## 代码变更
| 文件 | 变更 |
|------|------|
| [sag-router/src/middleware/auth.rs](https://github.com/rainbowatcher/sag/blob/main/sag-router/src/middleware/auth.rs) | 认证中间件（提取 Bearer token、比对 api_keys） |
| [sag-config/src/config.rs](https://github.com/rainbowatcher/sag/blob/main/sag-config/src/config.rs) | GatewayAuthConfig |
| [sag-ui/src/stores/connection.ts](https://github.com/rainbowatcher/sag/blob/main/sag-ui/src/stores/connection.ts) | 前端 localStorage 持久化 |

## 影响的需求条目
- 全部（初始定义）
