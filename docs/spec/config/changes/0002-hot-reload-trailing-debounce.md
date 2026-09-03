---
created: 2026-09-03
updated: 2026-09-03
type: 修复
---

# 0002 — 热重载 watcher 加固：尾部防抖、目录监听与瞬时失败重试

## 动机

原有热重载 watcher 存在三处可靠性问题：

1. **前缘防抖（leading debounce）会读到中间态**：旧逻辑在收到第一个相关事件后立即触发 reload，并在 500ms 内忽略后续事件。编辑器分段原子保存（temp-file + rename）或 SAG 自身的 `save_config_to_file()` 会产生一串连续写入，旧逻辑可能把中间的临时版本当作最终配置加载，且之后不会再重新读取，留下永久错误的运行态。
2. **inode 级监听在原子替换后失效**：旧 watcher 只监听配置文件本身，而原子保存会替换 inode，导致监听静默失效，后续外部编辑不再被感知（外部编辑与 UI 变更之间的协调会被破坏，见 `ConfigMutationCoordinator` 的 `accepted_file_revision` 校验）。
3. **瞬时读取失败无重试**：reload 若恰逢文件被截断/临时缺失（原子替换中段、编辑器就地写中途），一次失败就直接放弃，直到下一个文件事件才重试。

本次变更把 watcher 改为**尾部防抖 + 目录与文件双监听 + settle 窗口排空 + 有界重试**，保证永远加载突发写入的最终版本，并在原子替换后继续监听。

## 需求变更

- 热重载触发从「首个事件后立即加载、随后忽略」改为**尾部防抖**：仅当文件静默超过 `DEBOUNCE`（300ms）后加载，突发写入期间不断重置窗口，保证读取的是最终版本。
- 新增监听语义：监听配置文件**父目录** + 文件本身；原子 temp-file + rename 替换不会中断监听，事件按配置路径/文件名过滤，`Access` 类只读事件被忽略。
- 新增 settle 窗口（100ms）与边界事件排空：窗口内若出现新的相关事件则重新武装防抖，不加载可能的中间态；排空事件同时会重建直接文件监听（原子替换后旧 inode 监听失效）。
- 新增瞬时失败有界重试：reload 失败（读到截断/无效中间态、文件短暂缺失）最多再重试 `MAX_RELOAD_RETRIES`（2 次），之后等待下一个文件事件。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-config/src/loader/reload.rs`](../../../../sag-config/src/loader/reload.rs) | 前缘防抖改为尾部防抖（`last_event` + `DEBOUNCE_TICK` 轮询）；增加目录监听与直接文件监听，事件按配置路径过滤（`event_targets_config`），`Access` 事件忽略；增加 settle 窗口与边界事件排空/重建；增加 `MAX_RELOAD_RETRIES` 有界重试；settle-drain 分支不再静默吞掉 watcher error；新增事件过滤与目录解析单元测试。 |
| [`sag-state/tests/hot_reload_watcher.rs`](../../../../sag-state/tests/hot_reload_watcher.rs) | 新增 watcher ↔ `ConfigMutationCoordinator` 集成测试：外部编辑被重载后 UI 变更不再冲突；原子替换突发写入只加载最终版本；未重载的外部编辑仍被拒绝（不静默覆盖）；SAG 自身持久化被重载且不产生误报冲突。 |
| [`sag-state/Cargo.toml`](../../../../sag-state/Cargo.toml) | 增加 `async-trait`（workspace）依赖，供集成测试实现 `ConfigReloadHandler`。 |

## 测试

- `sag-config/src/loader/reload.rs` 单元测试：`event_targets_config` 精确路径/文件名匹配、`Create`/`Remove`/`Rename` 命中、`Access` 忽略、无关路径与 SAG 自身 temp 文件忽略；`watch_dir_for` 父目录/裸文件名/相对路径解析。
- `sag-state/tests/hot_reload_watcher.rs` 集成测试（polling-based，无固定 sleep）：
  - 外部编辑被 watcher 重载后，`accepted_file_revision` 同步，后续 UI 变更不冲突且持久化到磁盘；
  - 连续两次原子 temp-file + rename 保存（中间版 + 最终版）后，运行态最终版本正确（不声明不可观测的 reload 次数，仅断言最终版本落地）；
  - 未重载的外部编辑仍触发 `AppError::Conflict`，磁盘内容不被覆盖；
  - SAG 自身持久化被 watcher 无操作重载，二次 UI 变更不产生误报「config file changed outside SAG」。
