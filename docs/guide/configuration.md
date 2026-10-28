# 配置

SAG 使用基于 TOML 的配置文件，查找顺序：

1. `--config <path>` 或位置参数（显式指定）
2. 项目根目录 `./gateway.toml`（存在时，向后兼容）
3. `$XDG_CONFIG_HOME/sag/gateway.toml`（默认 `~/.config/sag/gateway.toml`，XDG Base Directory 标准；开发轨的 `XDG_CONFIG_HOME` 被注入为 `.dev/xdg/config`，故为 `.dev/xdg/config/sag/gateway.toml`）

analytics 数据库与滚动日志同样遵循 XDG 标准：相对 `sqlite://` DSN 锚定到 `$XDG_DATA_HOME/sag`（默认 `~/.local/share/sag`），日志写入 `$XDG_STATE_HOME/sag/logs`（默认 `~/.local/state/sag/logs`）。开发轨（`just dev` / `just run`）由 `scripts/dev_env.py` 注入项目内 `XDG_*` 覆盖，上述路径整体平移到 `.dev/xdg/`（分析库 `.dev/xdg/data/sag/analytics.db`、日志（仅非 TTY）`.dev/xdg/state/sag/logs/`、配置 fallback `.dev/xdg/config/sag/gateway.toml`），不碰生产数据。详见 [部署 → 日志轮转与清理](/guide/deployment)。

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

在非 TTY 环境（生产部署、test-env）下，SAG 自动将日志写入 `$XDG_STATE_HOME/sag/logs/` 目录下的**每日轮转文件**（默认 `~/.local/state/sag/logs/sag.log.2026-06-26`），不依赖启动器的 stdout 重定向。开发环境（TTY）下日志输出到终端，保持不变。

开发轨注入了 `XDG_STATE_HOME=.dev/xdg/state`，非 TTY（如 `just dev` 的管道、`just run` 的重定向）时轮转文件落在 `.dev/xdg/state/sag/logs/`（`just dev-clean` 可整体清除）；TTY 交互下（直接 `just run`）仍只输出终端、不产生文件（见上）。

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
# 相对 `sqlite://` 路径锚定到 $XDG_DATA_HOME/sag（默认 ~/.local/share/sag）。
# 此处即解析为 ~/.local/share/sag/analytics.db；开发轨（`just dev` / `just run`）
# 注入了 XDG_DATA_HOME=.dev/xdg/data，故解析为 .dev/xdg/data/sag/analytics.db。
# 也可写绝对路径或 ":memory:"。
storage_dsn = "sqlite://analytics.db"
# session_registry（日志页会话信息卡的数据源）的保留天数，默认 7。
# 省略该键就等于启用 7 天清理：超期的会话记录会被直接删行（不归档）。
# 0 = 永久保留（关闭清理）；它不参与 call_logs 的冷热轮转。
session_retention_days = 7

# 网关客户端认证（可选 — 删除此段以关闭认证）
[gateway_auth]
api_keys = [
    "your-api-key-1",
    "your-api-key-2",
]

[providers.openai]
api_keys = ["${OPENAI_API_KEY}"]
proxy_mode = "inherit"     # inherit（默认）/ enabled / disabled；不读系统代理环境变量
timeout_secs = 600         # 非流式整体超时安全网
ttfb_timeout_secs = 60     # 流式：等待首字节（响应头）的超时
chunk_timeout_secs = 30    # 流式：chunk 间空闲超时
# strip_fields = ["chat_template_kwargs"]  # override 之后硬性剥离的顶层字段

# Provider 没有 provider 级 base_url —— 地址唯一来源是各 apis 节的 base_url。
# 协议集 = apis 的协议 key 集；lane 自动判定（ingress ∈ 协议集 → Native）。
[providers.openai.apis.chat_completions]
base_url = "https://api.openai.com/v1"
models_endpoint = "models"

[providers.anthropic]
api_keys = ["${ANTHROPIC_API_KEY}"]
timeout_secs = 600

[providers.anthropic.apis.anthropic_messages]
base_url = "https://api.anthropic.com/v1"
models_endpoint = "models"

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

`models_endpoint` 是 per-API 配置（`[providers.<name>.apis.<key>]` 节），用于管理面板刷新上游模型列表。默认值为 `models`，相对本节的 `base_url` 拼接；支持 `models`、`/models`、`v1/models` 等写法，保存时会规范化为相对路径。

### Provider 完整字段

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `apis` | table | — | 协议 API 目录（**必填，至少一个协议 key**）。`[providers.<name>.apis.<kind>]`，`kind` ∈ `chat_completions` / `responses` / `anthropic_messages` / `image`。每节携带自己的 `base_url`（地址唯一来源）与可选的协议相关覆盖字段；`image` 是非协议条目，不参与 lane 判定 |
| `fallback` | string (可选) | — | 降级协议（translated 路径的编码目标）。`apis` 恰好一个协议 key 时可省略（自动取之）；多个协议 key 时必填，值必须 ∈ apis 协议 key 集。该值决定 translated 请求实际使用的 `apis` 节（上游地址、鉴权、协议相关覆盖字段）与能力档，详见[多协议供应商与降级协议](/guide/providers#多协议供应商与降级协议) |
| `api_keys` | array | `[]` | API 密钥列表（所有 API 共享的凭据池） |
| `key_strategy` | string | `"fill_first"` | 多密钥选择策略：`"fill_first"`（顺序使用，失败后切换）或 `"round_robin"`（轮询分发） |
| `proxy_mode` | string | `"inherit"` | 出站代理三态：`"inherit"`（默认，跟随顶层 `proxy_url`）/ `"enabled"`（强制走全局代理，无全局代理则配置错误）/ `"disabled"`（强制直连）。SAG **不读**系统代理环境变量 |
| `timeout_secs` | integer | `600` | 上游请求超时（秒）。流式请求不受此约束，仅作为非流式请求的整体安全网。provider 级为缺省值，`apis.<key>.timeout_secs` 整段覆盖 |
| `chunk_timeout_secs` | integer (可选) | — | 流式请求中 chunk 间的最大空闲时间（秒），超时则断开连接。省略时无限等待 |
| `ttfb_timeout_secs` | integer (可选) | `60` | 流式请求等待上游响应头（首字节）的最大时间（秒）。收到响应头后停止计时，不约束 body 流；超时可重试并 fallback。省略时使用默认 60 秒 |
| `enabled` | boolean | `true` | 是否启用此供应商。`false` 时在路由中跳过并在管理面板中显示为禁用 |
| `enabled_models` | array | `[]` | 允许使用的模型 ID 列表。非空时仅此列表中的模型可使用此供应商。默认空（允许所有模型） |
| `headers` | object | `{}` | 附加到上游请求的自定义 HTTP 头，应用于供应商默认头之后，可覆盖认证、Content-Type 等 |
| `body_overrides` | object | `{}` | 合并到上游请求体顶层的字段覆盖（如 `max_tokens`、`temperature`），对该供应商的**所有模型**生效。优先级低于 `model_overrides` 和目标级别的 `extra` |
| `strip_fields` | array | `[]` | 在三层 body override **之后**硬性移除的顶层字段（最终净化）。配置矛盾时 strip 胜出；空列表 no-op |
| `model_strip_fields` | object | `{}` | 逐模型字段剥离表，以 `real_model`（最终 `body.model`）为 key。与 `strip_fields` 取并集后在 override 之后统一剥离 |
| `model_overrides` | object | `{}` | 逐模型的 body 字段覆盖，以 `real_model` 名称为 key。仅对匹配的模型生效，优先级高于 `body_overrides`，低于目标级别的 `extra`。用于避免在多个路由 target 中重复配置同一 `(provider, real_model)` 的参数 |
| `role_mappings` | object | `{}` | 角色映射表：将请求中的角色名映射为上游支持的角色。例如 `{ "developer" = "system" }` 将 developer 消息转为 system 消息。大小写敏感 |
| `anthropic_cache_control` | boolean | `true` | 是否对 Anthropic Messages 请求注入 `cache_control: {\"type\": \"ephemeral\"}` 断点。仅当实际使用协议为 `anthropic_messages` 时生效（`apis.anthropic_messages` 可覆盖），可显著降低重复前缀请求的成本 |

#### headers 示例

```toml
[providers.anthropic.headers]
Authorization = "Bearer custom-token"
X-Debug = "true"
```

#### body_overrides 示例

```toml
# 对该供应商的所有请求都生效
[providers.openai.body_overrides]
max_tokens = 4096
temperature = 0.8
```

#### model_overrides 示例

```toml
# 仅对指定 real_model 生效，避免在多个路由中重复配置 extra
[providers.openai.model_overrides]
gpt-4o = { temperature = 0.7, max_tokens = 4096 }
gpt-4o-mini = { max_tokens = 2048 }
```

#### role_mappings 示例

```toml
[providers.openai.role_mappings]
developer = "system"
```

## Body Override 优先级详解

SAG 提供分层 body override 机制，按以下优先级从低到高排列：

```
客户端原始请求体
    ↓ 被覆盖
Provider body_overrides（全局，所有模型）
    ↓ 被覆盖
Provider model_overrides[real_model]（该模型专属）
    ↓ 被覆盖
RouteTarget extra（该路由目标，override 链最高优先级）
    ↓ 被剥离
Provider strip_fields / model_strip_fields（最终净化，硬性移除）
```

### 设计原则

| 层级 | 配置位置 | 何时使用 |
|------|---------|---------|
| `body_overrides` | `[providers.<name>]` | 设置供应商级别的**全局默认值**，影响该供应商的所有模型 |
| `model_overrides` | `[providers.<name>.model_overrides]` | 设置**模型专属参数**，避免在多个路由 target 中重复配置 `extra` |
| `extra` | `[[routes]].targets[]` | **路由级微调**，仅在特定路由目标需要与 `model_overrides` 不同的参数时使用 |
| `strip_fields` / `model_strip_fields` | `[providers.<name>]` | **最终净化**：在以上三层 override 之后硬性移除顶层字段（配置矛盾时 strip 胜出）。`model_strip_fields` 按最终 `body.model`（即 `real_model`）匹配并与 `strip_fields` 取并集；空列表 no-op |

### 示例

```toml
[providers.openai]
api_keys = ["${OPENAI_API_KEY}"]
# 最终净化：override 之后硬性剥离（可选）
strip_fields = ["chat_template_kwargs"]

# 全局默认：所有 openai 请求 max_tokens=4096
[providers.openai.body_overrides]
max_tokens = 4096

# 模型专属：gpt-4o 额外设置 temperature=0.7
[providers.openai.model_overrides]
gpt-4o = { temperature = 0.7 }

[[routes]]
model_name = "gpt-4o"
targets = [
    # 最终参数：max_tokens=4096 + temperature=0.7（若在 strip_fields 中列出则被移除）
    { provider = "openai", real_model = "gpt-4o", priority = 10 },
]

[[routes]]
model_name = "gpt-4o-creative"
targets = [
    # 最终参数：max_tokens=4096 + temperature=0.9（extra 覆盖 model_overrides）
    { provider = "openai", real_model = "gpt-4o", priority = 10, extra = { temperature = 0.9 } },
]
```

> **最佳实践**：优先使用 `model_overrides` 配置模型专属参数，只在需要路由级差异时才使用 `extra`。参见[路由 → Body Override 优先级与最佳实践](/guide/routing#body-override-优先级与最佳实践)。

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
- 也可在 Dashboard 的 **Settings → Gateway API Keys** 中在线增删密钥；变更会写回配置文件并即时生效（认证中间件每次请求读取当前内存配置）

Dashboard 前端在登录页输入 URL 和 API Key 后，会将连接信息保存在浏览器 `localStorage` 中（有效期 30 天）。重启服务后自动使用保存的 key 重连，无需重新输入。

## 热重载

配置文件使用 `notify` 库监听变化。更新后，供应商和路由会在不重启服务器的情况下即时生效。服务器设置（`[server]`）不支持热更新，需重启服务。通过 Settings 页 / `PUT /api/settings` 修改的 `gateway_auth.api_keys` 会立即在内存生效并持久化；外部直接改配置文件中的 `[gateway_auth]` 则需重启（文件热重载路径不重建认证中间件依赖的 router）。

## 环境变量

供应商 API 密钥可以使用 `${VAR_NAME}` 语法通过环境变量设置。例如：

```toml
api_keys = ["${OPENAI_API_KEY}"]
```

SAG 在启动时会读取环境变量 `OPENAI_API_KEY` 的值替换该配置项。环境变量也支持在 `[[routes]]` 的 `real_model` 中使用。
