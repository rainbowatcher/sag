---
created: 2026-09-03
updated: 2026-09-03
type: 修改
---

# 0004 — 配置未知字段告警重构：serde_ignored 统一 typed 反序列化

## 动机

原有的未知字段告警基于 **raw TOML + 手写白名单**（`warn_unknown_fields` / `check_unknown_keys`）：先 `toml::from_str::<toml::Value>` 解析原始文档，再按 top level / `[server]` / `[analytics]` / `[gateway_auth]` / `[backoff]` / `[circuit_breaker]` / `[logging]` / `[providers.*]` / `[[routes]]` 及 route target 手工枚举已知键名。该方案存在多个结构性缺陷：

1. **白名单必然滞后于 Schema**：每新增一个配置字段都要同步维护 `load.rs` 中的白名单数组，遗漏即产生误报警告——0003 修复的 `retry_on_empty_response` 虚警正是此类问题。
2. **手写白名单覆盖面依赖人工维护**：漏列的新键不会被识别，对动态 HashMap key 的处理也不一致。
3. **load / reload 各写一份告警逻辑**：重复且易漂移，初始加载与热重载的告警行为可能不一致。

本次重构移除 raw TOML 解析与手写白名单，改用 `serde_ignored` 配合 `toml::Deserializer`，在 **serde typed 反序列化过程中**收集类型不识别的字段点分路径并逐条 `tracing::warn!`，与 `GatewayConfig` 结构体本身保持同步——结构体加字段即自动识别，不再有独立白名单。`load_config`（初始加载）与 `reload_config`（热重载）共用同一 `deserialize_config` 路径，行为天然一致。动态 HashMap key（provider 名、header 名、body override key、`RouteTarget.extra` key 等）被 map 类型正常消费，**不**计入未知字段。

## 需求变更

- `requirements.md`「加载」节：完整反序列化改为统一 typed 反序列化（`toml::Deserializer` + `serde_ignored`）；未知字段仅 `warn!`、绝不拒绝；load / reload 共用同一告警路径。
- `requirements.md`「加载」节新增未知字段处理说明：动态 HashMap key（provider 名、header 名、body override key、`RouteTarget.extra` key 等）被 map 类型正常消费，不视为未知字段。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`Cargo.toml`](../../../../Cargo.toml) | workspace 依赖新增 `serde_ignored = "0.1"`。 |
| [`sag-config/Cargo.toml`](../../../../sag-config/Cargo.toml) | 新增 `serde_ignored = { workspace = true }`。 |
| [`sag-config/src/loader/load.rs`](../../../../sag-config/src/loader/load.rs) | 移除 raw TOML + 手写白名单 `warn_unknown_fields` / `check_unknown_keys`；新增 `deserialize_config_collecting`（`serde_ignored` 收集未知字段路径）与 `deserialize_config`（逐条 `warn!`）；`load_config` / `reload_config` 共用该路径；新增 serde_ignored 系列单元测试。 |

## 测试

- `sag-config/src/loader/load.rs` 单元测试（serde_ignored 版本替代原白名单测试）：
  - `unknown_field_detects_*` 系列：top-level 拼写、`[server]`、provider、route、route target 的未知键均被检测，返回 `server.bind`、`providers.openai.max_tokens`、`routes.0.endpoint`、`routes.0.targets.0.model` 等点分路径。
  - `unknown_field_ignores_known_keys` / `unknown_field_ignores_known_backoff_keys`：已知键（含 `retry_on_empty_response`）不告警。
  - `unknown_field_flags_top_level_retries`：迁移到 `[backoff]` 后残留的 top-level `retries` 仍被标记。
  - `unknown_field_ignores_dynamic_map_keys`：provider 名、`headers.*`、`body_overrides.*`、`model_overrides.*`、`role_mappings`、`model_strip_fields`、`RouteTarget.extra.*` 等动态 key 全部不告警。
  - `load_config_does_not_deny_unknown_fields`：含未知字段的配置仍成功加载（warn 不 deny）。
  - `unknown_fields_emit_warn`：手写 `RecordingSubscriber` 断言 `server.bind` 与 `providers.openai.max_tokens` 发出 `warn!`（`deserialize_config` 与 `load_config` 双路径）。
  - `reload_config_handles_unknown_fields`：热重载路径同样容忍未知字段，与初始加载行为一致。
- 全库 `just test --tests` 验证通过。
