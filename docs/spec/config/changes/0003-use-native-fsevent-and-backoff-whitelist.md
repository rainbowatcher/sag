---
created: 2026-09-03
updated: 2026-09-03
type: 修复
---

# 0003 — 修复 macOS 文件句柄耗尽与回退白名单遗漏

## 动机

1. **`macos_kqueue` 引发文件句柄爆炸（`Too many open files`）**：
   在 0002 引入父目录监听后，若配置文件位于仓库根目录，watcher 需监听根目录。而 `Cargo.toml` 中启用了 `notify = { features = ["macos_kqueue"] }`。在 macOS 的 kqueue 后端中，目录收到 `Vnode::Link` 变动事件时内部硬编码以递归方式重新添加 watch，递归遍历整树（包括 `target/` 和 `node_modules/`），打开了超过 24 万个文件描述符，导致系统句柄耗尽（`os error 24: Too many open files`），进而引发网络连接无法创建、SQLite 报错、Axum 无法 accept 新连接的系统级崩溃。
2. **`[backoff].retry_on_empty_response` 虚警**：
   `retry_on_empty_response` 特性已在核心配置、调度与持久化逻辑中支持，但 `sag-config/src/loader/load.rs` 中的 `warn_unknown_fields` 检查遗漏了该键名，导致每次加载均输出未知字段警告。

## 需求变更

- 移除 `Cargo.toml` 中的 `macos_kqueue` 特性，恢复 macOS 平台原生推荐的 FSEvents（`notify` 默认后端），彻底解决文件描述符泄漏风险。
- `reload.rs` 仅监听配置文件所在的父目录；移除针对 kqueue 平台专用的文件直接监听与 `rewatch_config_file`，使代码在各平台（Linux inotify、macOS FSEvents、Windows）行为一致且精简。
- `sag-config/src/loader/load.rs` 将 `retry_on_empty_response` 纳入 `[backoff]` 白名单，消除误报警告。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`Cargo.toml`](../../../../Cargo.toml) | 移除 `notify` 的 `features = ["macos_kqueue"]`，改用默认配置（macOS 上启用 `macos_fsevent`）。 |
| [`sag-config/src/loader/reload.rs`](../../../../sag-config/src/loader/reload.rs) | 移除对单文件的冗余直接监听以及 `rewatch_config_file` 补偿逻辑，规范父目录监听语义。 |
| [`sag-config/src/loader/load.rs`](../../../../sag-config/src/loader/load.rs) | `warn_unknown_fields` 白名单中为 `[backoff]` 添加 `"retry_on_empty_response"`；增加已知 backoff 键单元测试。 |
| [`sag-state/tests/hot_reload_watcher.rs`](../../../../sag-state/tests/hot_reload_watcher.rs) | `unique_temp_path` 规范化 canonicalize 临时目录，保证跨平台与 macOS 符号链接路径一致性。 |
| [`docs/spec/config/spec.md`](../../../../docs/spec/config/spec.md) | 更新不变式说明（macOS 使用原生 FSEvents）。 |

## 测试

- `sag-config/src/loader/load.rs` 单元测试：新增 `warn_unknown_fields_ignores_known_backoff_keys`，验证含 `retry_on_empty_response` 的配置不触发告警。
- `sag-state/tests/hot_reload_watcher.rs` 集成测试：
  - 4 个 watcher ↔ coordinator 集成测试在原生 FSEvents 下全部通过。
- 全库 `just test --tests` 与 `just lint` 验证通过。
