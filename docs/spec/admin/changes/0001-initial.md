# 0001 — 管理 API 初始化

**日期**：2026-06-03
**类型**：新增

## 动机
需要 REST API 在运行时管理供应商和路由，支持 CRUD 和模型连接测试。

## 需求变更
- requirements.md 全部初始定义

## 代码变更
| 文件 | 变更 |
|------|------|
| [sag-admin/src/admin_routes.rs](https://github.com/rainbowatcher/sag/blob/main/sag-admin/src/admin_routes.rs) | 管理路由注册 |
| [sag-admin/src/admin_providers.rs](https://github.com/rainbowatcher/sag/blob/main/sag-admin/src/admin_providers.rs) | 供应商 CRUD handler |
| [sag-admin/src/admin.rs](https://github.com/rainbowatcher/sag/blob/main/sag-admin/src/admin.rs) | 路由 CRUD handler |
| [sag-admin/src/admin_logs.rs](https://github.com/rainbowatcher/sag/blob/main/sag-admin/src/admin_logs.rs) | 日志查询 handler |
| [sag-admin/src/test_utils.rs](https://github.com/rainbowatcher/sag/blob/main/sag-admin/src/test_utils.rs) | 配置一致性测试 |

## 影响的需求条目
- 全部（初始定义）
