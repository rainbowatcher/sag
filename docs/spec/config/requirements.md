---
created: 2026-06-03
updated: 2026-09-03
---

# 配置

## 目标

提供 TOML 格式的配置文件，支持完整字段验证、环境变量插值和文件级热重载。

## 预期行为

### 加载

1. 解析命令行参数，确定配置文件路径（`--config <path>` > 位置参数 > `gateway.toml`）
2. Peek 配置文件中的 `[logging]` 段（在完整加载前初始化 tracing）
3. 完整反序列化 TOML → `GatewayConfig` 结构体
4. 运行 `validate()` → 收集所有错误（不 fail-fast）
5. 通过 `notify` crate 监听文件变化

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
- 单 key + RoundRobin → warning（非 error）

### 保存

管理 API 修改配置后，`save_config_to_file()` 将内存中的 `GatewayConfig` 序列化回 TOML 文件：
- 保留原有 TOML 格式（通过 `toml_edit` 做原位编辑，不用 toml crate 重新序列化整个文件）
- 保留注释和空白
- 默认值跳过序列化（`skip_serializing_if`）
- `extra` / `body_overrides` / `headers` 使用原地编辑，保留 key 顺序

## 配置 Schema

参见 [使用指南 — 配置](/guide/configuration)

## 功能边界

### 不包含
- 多配置文件合并
- 配置版本迁移
- 远程配置拉取

## 变更记录

- [0003 修复 macOS 文件句柄耗尽与回退白名单遗漏](changes/0003-use-native-fsevent-and-backoff-whitelist.md)
- [0002 热重载 watcher 加固：尾部防抖、目录监听与瞬时失败重试](changes/0002-hot-reload-trailing-debounce.md)
- [0001 初始化](changes/0001-initial.md)
