# 路由

## 为什么需要路由

SAG 作为网关，核心职责是将客户端请求转发到正确的上游 LLM 供应商。但实际情况中：

- **同一个模型名**在不同供应商上有不同的实际模型名（如 OpenAI 的 `gpt-4o` 对应 Anthropic 的 `claude-sonnet-4`）
- **需要故障转移** — 一个供应商不可用时自动切换到另一个
- **不同模型**需要不同的处理端点（聊天 `/v1/chat/completions`、图像 `/v1/images/generations`）

路由系统就是用来解决这些问题的：**将客户端的模型名映射到具体的供应商和模型，并管理故障转移策略**。

## 路由匹配

SAG 通过 `routes` 配置来定义路由表。当请求到达时，按以下顺序匹配：

1. **精确匹配** — 遍历路由表，找到 `model_name` 与请求模型完全相同的路由
2. **通配匹配** — 如果没有精确匹配，查找 `model_name = "*"` 的兜底路由
3. **匹配失败** — 返回 `404 ModelNotFound`

```toml
[[routes]]
model_name = "gpt-4o"
api_endpoint = "/v1/chat/completions"

[[routes.targets]]
provider = "openai"
real_model = "gpt-4o"
priority = 100

[[routes]]
model_name = "*"          # 兜底路由
api_endpoint = "/v1/chat/completions"

[[routes.targets]]
provider = "openai"
real_model = "gpt-4o"
priority = 10
```

## 配置详解

### `RouteConfig` — 路由定义

| 字段 | 类型 | 说明 |
|------|------|------|
| `model_name` | string | 客户端请求的模型名。支持精确匹配，或使用 `"*"` 作为通配符匹配所有模型 |
| `api_endpoint` | string | 上游 API 端点路径，默认 `"/v1/chat/completions"` |
| `strategy` | string | 目标选择策略：`fill_first` (默认) 或 `round_robin` |
| `retries` | integer (可选) | 每个目标的重试次数，覆盖全局 `retries`。省略则使用全局默认值 |
| `targets` | array | 目标供应商列表，按优先级排序 |

### `RouteTarget` — 路由目标

| 字段 | 类型 | 说明 |
|------|------|------|
| `provider` | string | 供应商名称，必须匹配 `[providers]` 中定义的名称 |
| `real_model` | string (可选) | 实际发送给供应商的模型名。**省略此项**则透传客户端请求的原始模型名 |
| `priority` | integer | 优先级，默认 `0`。**值越大优先级越高**，优先尝试 |
| `retries` | integer (可选) | 此目标的重试次数，覆盖路由级别及全局的 `retries`。省略则回退到路由级别，再回退到全局默认值 |
| `extra` | object (可选) | 针对该目标的额外 JSON 字段，合并到上游请求体中。**应用时机晚于供应商级别的 `body_overrides`**，因此目标级别的 key 具有更高优先级 |

### `api_endpoint` 说明

`api_endpoint` 定义了该路由使用什么 API 端点。SAG 会据此选择处理逻辑：

- `/v1/chat/completions` — 聊天补全请求
- `/v1/images/generations` — 图片生成请求（需要对应端点含 `/images/generations`）

## 目标选择策略

SAG 提供两种目标选择策略：`fill_first`（默认）和 `round_robin`。

### FillFirst — 首次随机 + 缓存

`fill_first` 的核心语义是：**每个优先级组内，首次访问时随机打乱顺序，之后所有请求复用同一顺序**。第一个目标承担所有流量，直到它不可用（被禁用、熔断器打开、或重试耗尽），才会 fallthrough 到下一个目标。

```mermaid
flowchart TD
    A[请求到达] --> B[匹配路由]
    B --> C{该路由已有
    缓存顺序且未过期?}
    C -->|否| D[按 priority 降序排列<br/>同优先级随机打乱<br/>缓存结果 + 时间戳]
    C -->|是| E[使用缓存顺序]
    D --> F[尝试下一个目标]
    E --> F
    F --> G{目标可用?}
    G -->|熔断器开| F
    G -->|provider disabled| F
    G -->|是| H[调用上游 API]
    H --> I{响应结果}
    I -->|成功| J[返回 200]
    I -->|4xx 错误| K[停止重试]
    I -->|5xx/网络超时| L{该目标重试耗尽?}
    L -->|否| H
    L -->|是| F
    F -->|全部失败| O[返回 502]
```

**关键特性**：

1. **首次随机** — 首次访问时同优先级 target 随机打乱，避免所有 SAG 实例启动后都打同一个 provider
2. **顺序缓存 + TTL** — 打乱结果通过 `model_name` 缓存，后续请求严格按同一顺序尝试。缓存有效期由 `shuffle_cache_ttl_secs` 控制（默认 7200 秒 = 2 小时），到期后自动重新打乱
3. **缓存失效** — 当路由配置变更（target 增删、provider/real_model/priority 修改）时自动重新打乱，忽略 TTL
4. **熔断器** — 如果目标重试耗尽，熔断器按错误类型打开对应冷却时长。冷却期内该目标被跳过；冷却结束后自动重新加入，按优先级正常参与竞争

**示例**：

```toml
[[routes]]
model_name = "gpt-4o"
strategy = "fill_first"

[[routes.targets]]
provider = "openai"
priority = 10

[[routes.targets]]
provider = "anthropic"
priority = 10

[[routes.targets]]
provider = "openai"
real_model = "gpt-4o-backup"
priority = 0
```

首次请求进入时，两个 priority=10 的 target 随机排列。假设顺序是 `[anthropic, openai]`：

- 请求 1-1000：所有请求都发给 anthropic，只要它健康
- 如果 anthropic 熔断：自动跳到 openai
- 冷却结束后：重新尝试 anthropic（FillFirst 自然语义——优先用首选 target）

如果 TOML 中没有配 strategy，默认为 `fill_first`。

### RoundRobin — 轮询

`round_robin` 的核心语义是：**每个请求轮流使用下一个 target**。计数器按 `model_name` 独立维护，请求一次加一。

```mermaid
flowchart TD
    A[请求到达] --> B[匹配路由]
    B --> C[读取该路由的轮询计数器]
    C --> D[按优先级降序排列 target<br/>从 counter 位置开始轮转]
    D --> E[计数器 +1]
    E --> F{尝试当前 target}
    F --> G{目标可用?}
    G -->|否| F
    G -->|是| H[调用上游 API]
    H --> I{响应结果}
    I -->|成功| J[返回 200]
    I -->|4xx 错误| K[停止重试]
    I -->|5xx/网络超时| L{该 target 重试耗尽?}
    L -->|否| H
    L -->|是| F
    F -->|全部失败| M[返回 502]
```

**关键特性**：

1. **每次都转** — 每处理完一个请求（无论成功/失败），计数器前进一位
2. **计数器独立** — 每个 `model_name` 有自己的计数器，互不干扰
3. **按优先级分组** — 和 FillFirst 一样按 priority 降序排列，但同一优先级内按轮询顺序
4. **不跨界** — 高优先级失败尝试完所有同级 target 后，才会进入下一个优先级（与 FillFirst 相同）
5. **熔断器生效** — 熔断器打开时自动跳过该 target，冷却结束后重新加入轮转

**示例**：

```toml
[[routes]]
model_name = "gpt-4o"
strategy = "round_robin"

[[routes.targets]]
provider = "openai"
priority = 10

[[routes.targets]]
provider = "anthropic"
priority = 10
```

- 请求 1 → openai（counter 0 % 2 = 0）
- 请求 2 → anthropic（counter 1 % 2 = 1）
- 请求 3 → openai（counter 2 % 2 = 0）
- ...

**两种策略对比**：

| 特性 | FillFirst | RoundRobin |
|------|-----------|------------|
| 目标选择 | 首次随机后固定 + TTL 过期重排 | 每次轮转 |
| 适用场景 | 主备模式、成本优化 | 负载均衡 |
| 缓存状态 | CachedOrder（TTL + 跨请求记忆） | 无（只记计数器） |
| 配置热重载 | 重新随机 | 计数器继续递增 |
| 熔断冷却后行为 | 自然回归首选（按优先级） | 自然回归轮转 |
| 默认 | 是 | 否 |

## 熔断器策略

SAG 的熔断器根据**错误类型**自动选择冷却时长，三个 duration 均可通过 `[circuit_breaker]` 配置段覆盖：

| 错误类型 | 默认冷却 | 触发条件 |
|---------|---------|----|
| 额度耗尽 | 1 小时 | 上游返回 `429` + quota/billing 相关错误 body |
| 请求过频 | 10 秒 | 上游返回 `429`（速率限制），非额度相关 |
| 通用故障 | 60 秒 | 5xx、网络超时、连接错误等 |

冷却结束后，目标自然重新加入路由——FillFirst 按优先级尝试首选 target，RoundRobin 回到轮转顺序。

```toml
# 熔断器冷却时长（秒），省略则使用默认值。
[circuit_breaker]
quota_exhausted_secs = 3600   # 默认 1 小时
rate_limited_secs = 10        # 默认 10 秒
generic_secs = 60             # 默认 60 秒
```

## shuffle_cache_ttl_secs

控制 FillFirst 策略中 shuffle 缓存的有效期。配置在 `[gateway]` 顶层：

```toml
# FillFirst shuffle 缓存有效期（秒）。默认 7200（2 小时）。
# 设为 0 表示永不过期（只在配置变更时重新打乱）。
shuffle_cache_ttl_secs = 7200
```

## 优先级与故障转移

路由内的目标按 `priority` **降序**排列（值越大越优先）。这里的 `priority` 是 `RouteTarget` 级别字段，不是 `ProviderConfig` 级别字段。

处理逻辑（两种策略共用）：

```mermaid
flowchart TD
    A[请求到达] --> B[匹配路由]
    B --> C[按策略排序目标]
    C --> D{尝试下一个目标}
    D --> E{目标可用?}
    E -->|否| D
    E -->|是| F[调用上游 API]
    F --> G{响应结果}
    G -->|成功| H[返回 200]
    G -->|4xx 错误| I[停止重试]
    G -->|5xx/网络超时| D
    D -->|全部失败| J[返回 502]
```

- **4xx 错误**（BadRequest、Unauthorized、ModelNotFound）— **立即停止**，不尝试下一个目标。因为这是客户端的问题，换其他供应商也会失败
- **5xx/网络超时/其他** — 收集错误信息，继续尝试下一个目标
- **全部目标失败** — 返回 `502 All Providers Exhausted`，附带上所有失败原因

## 使用场景

### 1. 多供应商故障转移

同一个模型配置多个供应商，主供应商失败时自动切换到备用：

```toml
[[routes]]
model_name = "gpt-4o"
api_endpoint = "/v1/chat/completions"

[[routes.targets]]
provider = "openai"
real_model = "gpt-4o"
priority = 100

[[routes.targets]]
provider = "anthropic"
real_model = "claude-sonnet-4-20250514"
priority = 50
```

客户端请求 `gpt-4o` 时，优先用 OpenAI；如果 OpenAI 返回 5xx 或超时，自动切换到 Anthropic 的 `claude-sonnet-4`。

### 2. 模型名透传

客户端使用的模型名与供应商的模型名一致时，省略 `real_model` 即可：

```toml
[[routes]]
model_name = "gpt-4o-mini"
api_endpoint = "/v1/chat/completions"

[[routes.targets]]
provider = "openai"
priority = 100                # real_model 省略，使用客户端原始模型名
```

### 3. 图片生成路由

聊天和图片使用不同的端点：

```toml
[[routes]]
model_name = "dall-e-3"
api_endpoint = "/v1/images/generations"

[[routes.targets]]
provider = "openai"
real_model = "dall-e-3"
priority = 100
```

### 4. 通配符兜底路由

为未匹配的模型提供默认处理：

```toml
[[routes]]
model_name = "*"
api_endpoint = "/v1/chat/completions"

[[routes.targets]]
provider = "openai"
priority = 10                 # 透传客户端原始模型名
```

精确匹配优先于通配符，所以 `"*"` 只在没有精确匹配时生效。

## 完整示例

```toml
[providers.openai]
type = "openai"
api_keys = ["${OPENAI_API_KEY}"]
base_url = "https://api.openai.com/v1"

[providers.anthropic]
type = "anthropic"
api_keys = ["${ANTHROPIC_API_KEY}"]
base_url = "https://api.anthropic.com"

# 主模型 — OpenAI 优先，Anthropic 兜底
[[routes]]
model_name = "gpt-4o"
api_endpoint = "/v1/chat/completions"

[[routes.targets]]
provider = "openai"
real_model = "gpt-4o"
priority = 100
extra = { temperature = 0.7 }

[[routes.targets]]
provider = "anthropic"
real_model = "claude-sonnet-4-20250514"
priority = 50
extra = { max_tokens = 8192 }

# 轻量模型 — 透传模型名（省略 real_model）
[[routes]]
model_name = "gpt-4o-mini"
api_endpoint = "/v1/chat/completions"

[[routes.targets]]
provider = "openai"
priority = 100

# 图片生成
[[routes]]
model_name = "dall-e-3"
api_endpoint = "/v1/images/generations"

[[routes.targets]]
provider = "openai"
real_model = "dall-e-3"
priority = 100

# 兜底路由（透传原始模型名）
[[routes]]
model_name = "*"
api_endpoint = "/v1/chat/completions"

[[routes.targets]]
provider = "openai"
priority = 10
```

## `extra` 字段详解

`RouteTarget.extra` 允许为每个目标指定额外的 JSON 字段，这些字段会合并到发送给上游供应商的请求体中。

**合并顺序**（后者覆盖前者）：

1. 客户端原始请求体
2. 供应商级别的 `body_overrides`
3. 目标级别的 `extra`（**最高优先级**）

### 使用场景

#### 固定模型参数

某些供应商对同一模型有不同的参数要求：

```toml
[[routes]]
model_name = "my-model"

[[routes.targets]]
provider = "openai"
real_model = "gpt-4o"
priority = 100
extra = { temperature = 0.7, max_tokens = 4096 }

[[routes.targets]]
provider = "anthropic"
real_model = "claude-sonnet-4-20250514"
priority = 50
extra = { max_tokens = 8192, thinking = { type = "enabled", budget_tokens = 1600 } }
```

#### 供应商特定的非标准参数

```toml
[[routes.targets]]
provider = "openai"
real_model = "deepseek-chat"
extra = { model_id = 39 }
```
