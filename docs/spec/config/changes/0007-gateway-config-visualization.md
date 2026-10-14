---
created: 2026-10-11
updated: 2026-10-11
type: 新增
---

# 0007 — 顶级配置 Admin API 与 Settings 页可视化

## 动机

顶级 `GatewayConfig` 的 `server` / `logging` / `analytics` / `backoff` / `circuit_breaker` / 两个调度 TTL 标量 / 全局 `proxy_url` 此前没有任何 admin 端点：Settings 页只有 `Network`（全局代理）与 `Authentication` 两个 tab，查看或调整 backoff、熔断冷却、TTL 只能手工编辑 `gateway.toml`，既不直观也无法在 Web 面板内完成。

本次新增 `GET/PUT /api/config`，把内存生效的顶级配置以快照形式暴露出来，并为可热生效的运行时参数提供写入口；同时在 Settings 页补齐可编辑与只读可视化。

## 需求变更

- `docs/spec/config/requirements.md` 新增「顶级配置可视化（Admin Config API）」节：定义 `GET /api/config` 快照字段、`PUT /api/config` 部分更新语义、生效边界与 Settings 页 tab 规划。
- Settings 页由 `Network` / `Authentication` 2 个 tab 扩展为 7 个：新增 Backoff、Breaker、Timers（可编辑保存）与 Analytics、System（Server + Logging，只读 +「Requires restart」标注）。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-admin/src/admin_config.rs`](../../../../sag-admin/src/admin_config.rs) | 新增 `GET /api/config`（`ConfigSnapshot` 只读快照，clone 各 section 以免持有 `config.read()` guard）与 `PUT /api/config`（`UpdateConfigRequest` 部分更新）；`CircuitBreakerView` 单独视图暴露 4 字段（含 `probe_interval_secs`），`CircuitBreakerUpdate` 全字段必填 |
| [`sag-admin/src/admin.rs`](../../../../sag-admin/src/admin.rs) | 挂载 `/api/config` → `get(get_config).put(update_config)`，受 `gateway_auth` 中间件保护 |
| [`sag-admin/src/lib.rs`](../../../../sag-admin/src/lib.rs) | 注册 `admin_config` 模块 |
| [`sag-state/src/config_mutation.rs`](../../../../sag-state/src/config_mutation.rs) | 新增 `ConfigMutationCoordinator::update_runtime_config`：backoff / circuit_breaker / 两个 TTL 标量的统一 mutation 路径 |
| [`sag-ui/src/types/api.ts`](../../../../sag-ui/src/types/api.ts) | 新增 `GatewayConfigSnapshot` / `UpdateGatewayConfigPayload` 及 `BackoffConfig` / `CircuitBreakerConfigForm` / `AnalyticsConfig` / `ServerConfig` / `LoggingConfig` |
| [`sag-ui/src/stores/gatewayConfig.ts`](../../../../sag-ui/src/stores/gatewayConfig.ts) | 新增 gateway config store：`fetchGatewayConfig()` / `saveGatewayConfig()` |
| [`sag-ui/src/pages/SettingsPage.vue`](../../../../sag-ui/src/pages/SettingsPage.vue) | 扩展为 7 tab：新增 Backoff / Breaker / Timers 可编辑、Analytics / System 只读 |
| [`sag-ui/src/components/settings/`](../../../../sag-ui/src/components/settings/) | 新增 `BackoffForm` / `CircuitBreakerForm` / `StrategyTimersForm` / `SystemInfoCard` 组件 |

## wire 契约 / 设计决策

### `GET /api/config`

返回内存生效的顶级配置快照：

- `server`（host/port）、`logging`（level/format）、`analytics`（`null` 表示 `[analytics]` 段缺失）；
- `backoff`、`circuit_breaker`（含 `probe_interval_secs` 共 4 字段）、`shuffle_cache_ttl_secs`、`sticky_failover_reset_secs`、`proxy_url`（未配置时 `null`）；
- **不含** `providers` / `routes` / `gateway_auth`（各自有专属端点）。

### `PUT /api/config`

部分更新运行时可热生效的配置（字段缺省 = 保持不变）：

- `backoff` 与 `circuit_breaker` 为**全对象替换**：对象出现时其全部字段必填，包括 `probe_interval_secs`；
- `shuffle_cache_ttl_secs` / `sticky_failover_reset_secs` 为可选标量；
- `server` / `logging` / `analytics` / `proxy_url` / `providers` / `routes` **不可**经该端点修改——DTO 不声明这些字段，serde 静默忽略未知字段（网关前向兼容原则）；server / logging / analytics 需重启生效，`proxy_url` 由 `PUT /api/settings` 拥有。

### 统一 mutation 路径

写操作委托 `ConfigMutationCoordinator::update_runtime_config`，走与其它生产写一致的全流程：mutation mutex → 全配置校验 → 原子提交（含 `routes_version` bump）→ `toml_edit` 持久化 + 外部编辑冲突检测 → provider-registry 同步。PUT 成功后 `routes_version` 变化被熔断器 stale-probe 阈值 watcher 观察到，按新配置刷新阈值（与热重载同路径），handler 不直接写 `state.config`、不直接存盘、不直接写熔断器。

校验失败（非正冷却、`backoff.max_ms < base_ms` 等）由 coordinator 以 `AppError::Config` 返回并映射为 **400 BAD_REQUEST**（而非 500）；缺字段的 partial `circuit_breaker` 对象在反序列化阶段失败，返回 **422**。

### 生效边界

- `backoff`、`circuit_breaker`（含探测间隔）、两个 TTL 标量保存后对新请求**立即生效**（分发与开闸路径每次从内存配置读取）；
- `server`、`logging`、`analytics` 为只读展示，仅进程启动时生效（「Requires restart」标注）；
- `proxy_url` 由 `PUT /api/settings` 独占。

### 设计决策

- **单独 `CircuitBreakerView` / `CircuitBreakerUpdate` 而非直接序列化 `CircuitBreakerConfig`**：把公开契约显式固定为 4 个字段，避免内部字段变动泄漏到 API。
- **只读 section 不声明进 DTO**：靠 serde 忽略未知字段实现「写不进去」，符合网关「容忍未知字段而非报错拒绝」的前向兼容原则。
- **full-object replace**：breaker / backoff 出现即整体替换，避免半更新（缺字段直接 422），也避免 TTL 与探测间隔被静默重置。
- **复用统一 mutation 协议**：不新开写路径，持久化、冲突检测、registry 同步与版本 bump 全部与既有 admin mutation 一致。

## 测试

`sag-admin/src/admin_config.rs` 内联测试：

- `get_config_returns_readonly_snapshot` — 快照字段完整，`analytics` 缺失时为 `null`，breaker 4 字段含 `probe_interval_secs`
- `put_config_applies_all_runtime_sections` — backoff / breaker / 两个 TTL 全部落盘到内存配置，breaker 全对象替换生效
- `put_config_cannot_write_readonly_or_separately_owned_sections` — `server` / `logging` / `analytics` / `proxy_url` 请求体被忽略，`proxy_url` 保持原值
- `put_config_invalid_backoff_is_400_and_unchanged` — 非法 backoff 返回 400 且不提交
- `put_config_invalid_circuit_breaker_is_400_and_unchanged` — 非法 breaker 返回 400 且不提交
- `put_config_partial_circuit_breaker_object_is_rejected` — 缺字段的 breaker 对象返回 422
- `put_config_full_circuit_breaker_round_trips_probe_cadence` — 完整 breaker 写入后 `GET` 回读探测间隔一致

`sag-state/src/config_mutation.rs` 内联测试：

- `update_runtime_config_applies_all_sections` — coordinator 路径应用全部 section
- `update_runtime_config_validation_failure_leaves_config_unchanged` — 校验失败不改变配置
