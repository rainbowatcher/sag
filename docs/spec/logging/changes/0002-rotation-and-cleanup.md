# 0002 — 日志轮转与清理

**日期**：2026-06-26
**类型**：新增

## 动机
测试环境之前通过 Python 启动器重定向 stdout 到固定文件，无轮转、依赖启动器进程、buffering 延迟。生产环境需要每日轮转和自动清理。

## 需求变更
- requirements.md「输出目标」节：TTY → stdout，非 TTY → 每日轮转文件
- requirements.md「每日轮转」节：新增（tracing_appender Rotation::DAILY）
- requirements.md「启动时清理」节：新增（7 天保留期）
- 功能边界「包含」：TTY 检测 + 自适应输出、每日轮转、启动清理

## 代码变更
| 文件 | 变更 |
|------|------|
| [sag-gateway/src/tracing_init.rs](https://github.com/rainbowatcher/sag/blob/main/sag-gateway/src/tracing_init.rs) | 非 TTY 模式改用 RollingFileAppender + non_blocking，返回 WorkerGuard |
| [sag-gateway/src/main.rs](https://github.com/rainbowatcher/sag/blob/main/sag-gateway/src/main.rs) | 新增 cleanup_old_logs()（7 天保留），_log_guard 生命周期管理 |
| [Cargo.toml](https://github.com/rainbowatcher/sag/blob/main/Cargo.toml) | 新增 tracing-appender 工作区依赖 |
| [sag-gateway/Cargo.toml](https://github.com/rainbowatcher/sag/blob/main/sag-gateway/Cargo.toml) | 新增 tracing-appender 依赖 |
| [scripts/test-env.py](https://github.com/rainbowatcher/sag/blob/main/scripts/test-env.py) | 移除 stdout/stderr 重定向，status 改为 glob 最新轮转文件 |

## 测试
- 手动验证：`just test-env start && just test-env status` 确认 `logs/sag.2026-06-26.log` 生成
- TTY 回退验证：`just dev` 日志仍输出到终端

## 影响的需求条目
- 「输出目标」「每日轮转」「启动时清理」（全部新增）
- 「功能边界」：补充轮转和清理
