# 配置

SAG 使用基于 TOML 的配置文件（默认：`gateway.toml`）。

## 日志

`[logging]` 段控制日志级别和输出格式：

| 字段 | 默认值 | 说明 |
|---|---|---|
| `level` | `"info"` | 日志级别过滤器（`"trace"`, `"debug"`, `"info"`, `"warn"`, `"error"`）。也支持 `EnvFilter` 指令，如 `"info,sag_router=debug"`。环境变量 `RUST_LOG` 优先于此值 |
| `format` | `"compact"` | 输出格式：`"compact"`（人类可读单行）或 `"json"`（结构化日志，适合日志聚合器） |

```toml
[logging]
level = "info"
format = "compact"
```

### 日志文件与轮转

在非 TTY 环境（生产部署、test-env）下，SAG 自动将日志写入 `logs/` 目录下的**每日轮转文件**（例如 `logs/sag.2026-06-26.log`），不依赖启动器的 stdout 重定向。开发环境（TTY）下日志输出到终端，保持不变。

每次启动时，SAG 自动清理 **7 天前**的旧日志文件，保留最近一周的日志。

## 示例

```toml
# StickyFailover / FillFirst 策略的 shuffle 缓存有效期（秒）。默认 7200（2 小时）。
# 设 0 表示永不过期，只在配置热重载时重新打乱。
shuffle_cache_ttl_secs = 7200

# 熔断器冷却时长（秒），按错误类型分别配置。省略则使用默认值。
[circuit_breaker]
# 额度耗尽（上游 429 + quota/billing 关键词）。默认：3600（1 小时）。
quota_exhausted_secs = 3600
# 速率限制（上游 429，非额度相关）。默认：10。
rate_limited_secs = 10
# 通用故障（5xx / 网络超时 / 连接错误）。默认：60。
generic_secs = 60

# 指数退避设置：每次重试间隔 `min(base_ms * 2^attempt, max_ms)`，
# 可选 full jitter（实际等待 = random(0, 计算结果)）。
# 调整这些值以在响应延迟和上游负载之间做权衡。
[backoff]
# 全局默认重试次数（每个目标），可被 per-route 或 per-target 覆盖。默认：2。
retries = 2
# 初始退避延迟（毫秒），对应第 0 次重试之前等待。默认：100。
base_ms = 100
# 最大退避延迟上限（毫秒）。默认：5000（5 秒）。
max_ms = 5000
# 是否对每次延迟应用 full jitter。开启时实际等待为 random(0, 上限)，
# 可避免大量并发客户端的 thundering-herd 问题。关闭则使用确定性的
# 退避值（便于测试）。默认：true。
jitter = true

[server]
host = "0.0.0.0"
port = 37210

[analytics]
enabled = true
storage_dsn = "analytics.db"

# 网关客户端认证（可选 — 删除此段以关闭认证）
[gateway_auth]
api_keys = [
    "your-api-key-1",
    "your-api-key-2",
]

[providers.openai]
provider_type = "openai"
api_keys = ["${OPENAI_API_KEY}"]
base_url = "https://api.openai.com/v1"
models_endpoint = "models"
timeout_secs = 600         # 非流式整体超时安全网
ttfb_timeout_secs = 60     # 流式：等待首字节（响应头）的超时
chunk_timeout_secs = 30    # 流式：chunk 间空闲超时

[providers.anthropic]
provider_type = "anthropic"
api_keys = ["${ANTHROPIC_API_KEY}"]
base_url = "https://api.anthropic.com/v1"
models_endpoint = "models"
timeout_secs = 600

[[routes]]
model_name = "gpt-4o"
targets = [
    { provider = "openai", real_model = "gpt-4o", priority = 10 },
]

# 多目标故障转移，目标级别 extra 覆盖供应商 body_overrides
# retries 覆盖全局重试次数（仅此路由）
[[routes]]
model_name = "claude-sonnet"
retries = 3
targets = [
    { provider = "anthropic", real_model = "claude-3-5-sonnet-20241022", priority = 10, retries = 5, extra = { max_tokens = 8192 } },
    { provider = "openai", real_model = "gpt-4o-mini", priority = 1 },
]

# 兜底规则：未匹配的模型直接转发到指定供应商
[[routes]]
model_name = "*"
targets = [
    { provider = "openai", priority = 10 },
]
```

`models_endpoint` 是 provider 级配置，用于管理面板刷新上游模型列表。默认值为 `models`，会拼接到 `base_url` 后；支持 `models`、`/models`、`v1/models` 等写法，保存时会规范化为相对路径。

### Provider 完整字段

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `provider_type` | string | `"openai"` | 供应商适配器类型：`"openai"` / `"anthropic"` |
| `api_keys` | array | `[]` | API 密钥列表 |
| `key_strategy` | string | `"fill_first"` | 多密钥选择策略：`"fill_first"`（顺序使用，失败后切换）或 `"round_robin"`（轮询分发） |
| `base_url` | string | `"https://api.openai.com/v1"` | 供应商 API 基础 URL |
| `models_endpoint` | string | `"models"` | 模型列表端点路径（拼接在 `base_url` 后） |
| `timeout_secs` | integer | `600` | 上游请求超时（秒）。流式请求不受此约束，仅作为非流式请求的整体安全网 |
| `chunk_timeout_secs` | integer (可选) | — | 流式请求中 chunk 间的最大空闲时间（秒），超时则断开连接。省略时无限等待 |
| `ttfb_timeout_secs` | integer (可选) | `60` | 流式请求等待上游响应头（首字节）的最大时间（秒）。收到响应头后停止计时，不约束 body 流；超时可重试并 fallback。省略时使用默认 60 秒 |
| `enabled` | boolean | `true` | 是否启用此供应商。`false` 时在路由中跳过并在管理面板中显示为禁用 |
| `enabled_models` | array | `[]` | 允许使用的模型 ID 列表。非空时仅此列表中的模型可使用此供应商。默认空（允许所有模型） |
| `headers` | object | `{}` | 附加到上游请求的自定义 HTTP 头，应用于供应商默认头之后，可覆盖认证、Content-Type 等 |
| `body_overrides` | object | `{}` | 合并到上游请求体顶层的字段覆盖（如 `max_tokens`、`temperature`），优先级低于目标级别的 `extra` |
| `role_mappings` | object | `{}` | 角色映射表：将请求中的角色名映射为上游支持的角色。例如 `{ "developer" = "system" }` 将 developer 消息转为 system 消息。大小写敏感 |
| `anthropic_cache_control` | boolean | `true` | 是否对 Anthropic 请求注入 `cache_control: {\"type\": \"ephemeral\"}` 断点。仅对 `provider_type = "anthropic"` 有效，可显著降低重复前缀请求的成本 |

#### headers 示例

```toml
[providers.anthropic.headers]
Authorization = "Bearer custom-token"
X-Debug = "true"
```

#### body_overrides 示例

```toml
[providers.openai.body_overrides]
max_tokens = 4096
temperature = 0.8
```

#### role_mappings 示例

```toml
[providers.openai.role_mappings]
developer = "system"
```

## 重试退避

`[backoff]` 段控制每个上游 target 的重试次数与退避策略。注意 `retries` 可在三个层级分别覆盖（见下方[重试次数覆盖层级](#重试次数覆盖层级)）。

| 字段 | 默认值 | 说明 |
|---|---|---|
| `retries` | `2` | 每个 target 的最大重试次数。`1` 表示只尝试一次（不重试），`2` 表示最多重试一次，以此类推。设置 `>10` 时会输出配置警告 |
| `base_ms` | `100` | 初始退避延迟（毫秒），对应第 0 次重试前的等待 |
| `max_ms` | `5000` | 退避上限（毫秒），避免高 `retries` 时等待无界增长 |
| `jitter` | `true` | 是否对每次延迟应用 full jitter（`delay = random(0, cap)`） |

退避公式：`min(base_ms * 2^attempt, max_ms)`，其中 `attempt` 从 `0` 计数。例如默认配置下：第 1 次重试前等待 `0~100ms`、第 2 次 `0~200ms`、第 3 次 `0~400ms`、...，最大 `5s`。

`jitter = true`（推荐）可以避免大量并发客户端在重试时同时冲击上游（thundering herd）；`jitter = false` 则使用确定性延迟，便于测试与排障。

### 重试次数覆盖层级

`retries` 按三级优先级解析（高到低）：

1. **Target 级** — `[[routes]].targets[].retries`，只影响该 target
2. **Route 级** — `[[routes]].retries`，影响该路由下所有未单独设置的 target
3. **全局** — `[backoff].retries`，所有路由和 target 的兜底默认值

```toml
[backoff]
retries = 2                        # 全局默认

[[routes]]
model_name = "claude-sonnet"
retries = 3                        # 路由级覆盖：该路由下 target 默认重试 3 次
targets = [
    { provider = "anthropic", retries = 5 },   # target 级覆盖：仅此 target 重试 5 次
    { provider = "openai" },                   # 继承路由级 retries = 3
]

[[routes]]
model_name = "gpt-4o"
# 未设置 retries，继承全局 retries = 2
targets = [
    { provider = "openai" },
]
```

重试仅对**可重试错误**生效（5xx、网络超时、连接错误等）。4xx 类错误（参数错误、鉴权失败、模型不存在等）直接中止，不消耗重试次数。

## 熔断器

`[circuit_breaker]` 段控制目标重试耗尽后的冷却时长，按上游错误类型分别配置：

| 字段 | 默认值 | 说明 |
|---|---|---|
| `quota_exhausted_secs` | `3600` | 额度耗尽（上游 429 + quota/billing 关键词）后的冷却时长（秒），即 1 小时 |
| `rate_limited_secs` | `10` | 速率限制（上游 429，非额度相关）后的冷却时长（秒） |
| `generic_secs` | `60` | 通用故障（5xx / 网络超时 / 连接错误）后的冷却时长（秒） |

冷却期内该 target 被跳过，调度器自动 fallthrough 到下一个 target。冷却结束后 target 自然回归路由——StickyFailover 继续优先使用上一次成功的 sticky target，FillFirst 按优先级尝试首选，RoundRobin 回到轮转顺序。

错误分类逻辑由 `sag_core::CircuitHint` 实现：对上游 429 body 做关键词匹配，先精确匹配 `"rate limit"`（→ `RateLimited` / 10s），再匹配 quota/billing 相关关键词（→ `QuotaExhausted` / 1h），未知 429 和 5xx/网络错误回退到 `Generic`（→ 60s）。详情见[路由 → 熔断器策略](/guide/routing#熔断器策略)。

## 认证

`[gateway_auth]` 段配置 Dashboard 和管理 API 的访问凭证：

- `api_keys` — 允许的 API Key 列表，客户端需在请求头中携带 `Authorization: Bearer <key>` 进行认证
- 删除（或注释掉）整个 `[gateway_auth]` 段可关闭认证，允许所有请求通过
- 保留 `[gateway_auth]` 段但设 `api_keys = []`（空列表）等同于关闭认证，行为与删除该段一致

Dashboard 前端在登录页输入 URL 和 API Key 后，会将连接信息保存在浏览器 `localStorage` 中（有效期 30 天）。重启服务后自动使用保存的 key 重连，无需重新输入。

## 热重载

配置文件使用 `notify` 库监听变化。更新后，供应商和路由会在不重启服务器的情况下即时生效。认证配置（`[gateway_auth]`）和服务器设置（`[server]`）暂不支持热更新，需重启服务。

## 环境变量

供应商 API 密钥可以使用 `${VAR_NAME}` 语法通过环境变量设置。例如：

```toml
api_keys = ["${OPENAI_API_KEY}"]
```

SAG 在启动时会读取环境变量 `OPENAI_API_KEY` 的值替换该配置项。环境变量也支持在 `[[routes]]` 的 `real_model` 中使用。
