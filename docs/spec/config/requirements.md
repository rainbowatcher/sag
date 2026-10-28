---
created: 2026-06-03
updated: 2026-10-28
---

# 配置

## 目标

提供 TOML 格式的配置文件，支持完整字段验证、环境变量插值和文件级热重载。

## 预期行为

### 加载

1. 解析命令行参数，确定配置文件路径（`--config <path>` > 位置参数 > `./gateway.toml`（存在时，向后兼容）> `$XDG_CONFIG_HOME/sag/gateway.toml`）。开发轨（`just dev` / `just run`）由 `scripts/dev_env.py` 注入 `XDG_*` 覆盖（`.dev/xdg/`），因此第三项为 `.dev/xdg/config/sag/gateway.toml`；前两项优先级不受注入影响
2. Peek 配置文件中的 `[logging]` 段（在完整加载前初始化 tracing）
3. 统一 typed 反序列化 TOML → `GatewayConfig` 结构体（`toml::Deserializer` + `serde_ignored`，与结构体 Schema 自动同步）
4. 运行 `validate()` → 收集所有错误（不 fail-fast）
5. 通过 `notify` crate 监听文件变化

### 未知字段处理

反序列化时用 `serde_ignored` 收集类型不识别的字段点分路径（如 `server.bind`、`providers.openai.max_tokens`、`routes.0.targets.0.model`），逐条 `tracing::warn!`：
- 未知字段仅告警、**绝不拒绝**（向前兼容契约），含未知字段的配置仍正常加载；
- 初始加载（`load_config`）与热重载（`reload_config`）共用同一告警路径，行为一致；
- 动态 HashMap key（provider 名、`headers.*`、`body_overrides.*`、`model_overrides.*`、`role_mappings`、`RouteTarget.extra.*` 等）被 map 类型正常消费，**不**视为未知字段。

### 环境变量插值

配置中的 `${VAR_NAME}` 在反序列化后替换为环境变量值：
- `api_keys = ["${OPENAI_API_KEY}"]` → 读取 `OPENAI_API_KEY`
- `real_model = "${MODEL_NAME}"` → 读取 `MODEL_NAME`
- 不存在的环境变量 → 保留原字符串 `"${VAR_NAME}"`（不报错）

### 热重载

文件变化时触发 `notify` 事件：
1. 重新反序列化 TOML
2. 重新验证
3. 替换 `AppState` 中的配置（通过 `Arc<RwLock<GatewayConfig>>`）
4. 清除路由的 shuffle 缓存和 sticky active targets（因为 targets 可能变了）

**热重载边界**：`[server].host` 和 `[server].port` 修改不触发 listen socket 重新 bind——需要重启生效。`[gateway_auth]` 同样需要重启。

**监听与防抖**（[0002](changes/0002-hot-reload-trailing-debounce.md) 起）：
- 同时监听配置文件**父目录**与文件本身；原子 temp-file + rename 保存不会中断监听（目录监听不受 inode 替换影响），事件按配置路径/文件名过滤，`Access` 类只读事件忽略。
- 使用**尾部防抖**（trailing debounce）：仅当文件静默超过 300ms 后才加载，突发写入期间不断重置窗口，保证读取的是**最终版本**而非中间态。
- 防抖窗口后增加 100ms settle 窗口并排空边界事件，若有新相关事件则重新武装防抖；排空时同步重建直接文件监听。
- 加载失败（读到截断/无效中间态、文件短暂缺失）最多重试 2 次，之后等待下一个文件事件。

### 验证规则

`validate()` 在加载时和保存时执行，收集所有错误后一次性返回：
- 至少一个 provider
- `server.port > 0`
- `server.host` 非空
- `api_keys` 非空
- `base_url` 包含 `://`
- `timeout_secs > 0`（provider、route、target 三级）
- `chunk_timeout_secs > 0`（如果设置）
- `backoff.base_ms > 0`
- `backoff.max_ms >= base_ms`
- `api_endpoint` 以 `/` 开头
- 路由 target 引用存在的 provider
- 同一路由内 `(provider, real_model)` 不重复
- `models_endpoint` 不含 scheme / query / fragment / 空白
- `proxy_url` 为合法 `http://` / `https://` URL，且不含 userinfo（空白/纯空白视为未配置，不报错）
- `proxy_mode='enabled'` 的 provider 要求存在可用的顶层 `proxy_url`，否则报错
- 单 key + RoundRobin → warning（非 error）

### 保存

管理 API 修改配置后，`save_config_to_file()` 将内存中的 `GatewayConfig` 序列化回 TOML 文件：
- 保留原有 TOML 格式（通过 `toml_edit` 做原位编辑，不用 toml crate 重新序列化整个文件）
- 保留注释和空白
- 默认值跳过序列化（`skip_serializing_if`）
- `extra` / `body_overrides` / `headers` 使用原地编辑，保留 key 顺序
- 顶层 `proxy_url` 与 provider `proxy_mode` 同样原地编辑：清除全局代理时移除 key（不残留空值），`proxy_mode` 为默认 `inherit` 时跳过

### 出站代理

SAG 的出站代理完全由配置显式控制，**不**依赖系统代理环境变量（[0005](changes/0005-outbound-proxy-configuration.md) 起）：

- 顶层 `proxy_url: Option<String>` 定义全局 HTTP/HTTPS 代理；`None` / 空 / 纯空白值表示未配置（直连）。
- SAG 默认显式屏蔽 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` 及大小写变体环境代理：没有显式代理时所有 outbound client 使用 `no_proxy` 直连，绝不静默走系统代理。
- provider 三态 `proxy_mode`：
  - `inherit`（默认）— 继承全局 `proxy_url`；无全局代理时直连。
  - `enabled` — 强制使用顶层 `proxy_url`；未配置顶层代理时，配置加载/保存/接口请求失败。
  - `disabled` — 强制直连，忽略全局代理。
- provider client、gateway 启动、state upsert、mutation / hot reload、admin settings、provider CRUD、fetch-models / model-test 均遵循有效代理策略；Web UI provider add/edit 的临时测试使用当前 dialog 内存 `proxy_mode` 与当前内存全局 `proxy_url`，不要求先保存。
- 仅支持 HTTP/HTTPS，不支持 SOCKS；代理 URL 拒绝 userinfo（`user:pass@`），避免凭据泄漏。
- hot reload 按 effective proxy（全局 `proxy_url` + provider `proxy_mode` 解析结果）差异重建受影响 provider；`disabled` provider 不因全局代理变化重建；在途请求不强行中断。

### 顶级配置可视化（Admin Config API）

`GET /api/config` 返回内存生效的顶级配置快照（[0007](changes/0007-gateway-config-visualization.md) 起）：`server`（host/port）、`logging`（level/format）、`analytics`（`null` 表示 `[analytics]` 段缺失）、`backoff`、`circuit_breaker`（含 `probe_interval_secs` 共 4 字段）、`shuffle_cache_ttl_secs`、`sticky_failover_reset_secs`、`proxy_url`。不含 `providers` / `routes` / `gateway_auth`（各自有专属端点）。

`PUT /api/config` 部分更新运行时可热生效的配置（字段缺省 = 保持不变）：

- `backoff` 与 `circuit_breaker` 为**全对象替换**（对象出现时其全部字段必填，包括 `probe_interval_secs`），走 `ConfigMutationCoordinator::update_runtime_config` 的统一 mutation 协议（validate → 原子提交 → toml_edit 持久化 + 外部编辑冲突检测）。
- `shuffle_cache_ttl_secs` / `sticky_failover_reset_secs` 为可选标量。
- `server` / `logging` / `analytics` / `proxy_url` / `providers` / `routes` **不可**经该端点修改（DTO 不声明，serde 静默忽略未知字段）——server / logging / analytics 需重启生效，proxy_url 由 `PUT /api/settings` 拥有。
- PUT 成功后按新配置刷新熔断器 stale-probe 阈值（与热重载同路径）。

生效边界：backoff、circuit_breaker（含探测间隔）、两个 TTL 标量保存后对新请求**立即生效**（分发与开闸路径每次从内存配置读取）；server、logging、analytics 为只读展示，仅进程启动时生效。

UI（Settings 页）：`Network` / `Authentication` 之外新增 Backoff、Breaker、Timers（可编辑保存）与 Analytics、System（Server + Logging，只读 +「Requires restart」标注）5 个 tab。

## 配置 Schema

参见 [使用指南 — 配置](/guide/configuration)

## 功能边界

### 不包含
- 多配置文件合并
- 配置版本迁移
- 远程配置拉取

## 变更记录

- [0008 dev 轨 XDG 数据隔离（`.dev/xdg/`）](changes/0008-dev-track-xdg-isolation.md)
- [0007 顶级配置 Admin API 与 Settings 页可视化](changes/0007-gateway-config-visualization.md)
- [0006 XDG Base Directory 标准路径布局](changes/0006-xdg-base-directories.md)
- [0005 出站代理配置：顶层 proxy_url + provider proxy_mode 三态](changes/0005-outbound-proxy-configuration.md)
- [0004 配置未知字段告警重构：serde_ignored 统一 typed 反序列化](changes/0004-warn-unknown-config-fields.md)
- [0003 修复 macOS 文件句柄耗尽与回退白名单遗漏](changes/0003-use-native-fsevent-and-backoff-whitelist.md)
- [0002 热重载 watcher 加固：尾部防抖、目录监听与瞬时失败重试](changes/0002-hot-reload-trailing-debounce.md)
- [0001 初始化](changes/0001-initial.md)
