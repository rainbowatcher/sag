# 管理 API

管理 API 在 `/api` 前缀下暴露 REST 端点，用于运行时管理供应商和路由。

## 认证

管理端点使用 Bearer token 认证，与[模型 API](/guide/models-api#认证) 共享相同的 `[gateway_auth]` 配置：

```
Authorization: Bearer <api_key>
```

---

## 端点

### 列出供应商

```
GET /api/providers
```

返回所有已配置的供应商。

### 添加供应商

```
POST /api/providers
Content-Type: application/json

{
  "name": "my-provider",
  "api_keys": ["..."],
  "key_strategy": "fill_first",
  "apis": {
    "chat_completions": {
      "base_url": "https://api.openai.com/v1",
      "models_endpoint": "models"
    }
  },
  "timeout_secs": 600,
  "chunk_timeout_secs": null,
  "ttfb_timeout_secs": null,
  "enabled": true,
  "enabled_models": [],
  "headers": {},
  "body_overrides": {},
  "role_mappings": {},
  "anthropic_cache_control": true
}
```

**字段说明**：

| 字段 | 类型 | 必需 | 默认值 | 说明 |
|------|------|------|--------|------|
| `name` | string | 是 | — | 供应商唯一名称 |
| `apis` | object | 是 | — | 协议 API 目录（至少一个条目，且至少含一个协议 key）。key ∈ `chat_completions` / `responses` / `anthropic_messages` / `image`。每节携带自己的 `base_url`（必填，地址唯一来源）与可选 `models_endpoint`（默认 `models`，相对本节 `base_url`，传入 `/v1/models/` 会被规范化为 `v1/models`），还可声明 `headers` / `body_overrides` / `strip_fields` / `model_overrides` / `role_mappings` / `anthropic_cache_control` / `timeout_secs` / `chunk_timeout_secs` / `ttfb_timeout_secs`，声明时整段覆盖 provider 级同名缺省 |
| `fallback` | string | 条件 | — | 降级协议（translated 路径的编码目标），值必须 ∈ `apis` 的协议 key 集（`image` 非法）。`apis` 恰好一个协议 key 时省略（自动取之）；声明多个协议 key 时必填 |
| `api_keys` | array | 否 | `[]` | API 密钥列表 |
| `key_strategy` | string | 否 | `"fill_first"` | 多密钥选择策略：`"fill_first"` / `"round_robin"` |
| `timeout_secs` | integer | 否 | `600` | 上游请求超时（秒），仅约束非流式请求；`apis.<key>.timeout_secs` 可整段覆盖 |
| `chunk_timeout_secs` | integer \| null | 否 | `null` | 流式 chunk 间最大空闲时间（秒），`null` 表示无限等待 |
| `ttfb_timeout_secs` | integer \| null | 否 | `null`（默认 60） | 流式等待上游响应头（首字节）的最大时间（秒），收到响应头后停止计时 |
| `enabled` | boolean | 否 | `true` | 是否启用。`false` 时路由跳过且管理面板显示禁用 |
| `enabled_models` | array | 否 | `[]` | 模型白名单，空 = 全部允许 |
| `headers` | object | 否 | `{}` | 附加自定义 HTTP 头 |
| `body_overrides` | object | 否 | `{}` | 合并到上游请求体的字段覆盖 |
| `role_mappings` | object | 否 | `{}` | 角色映射表，如 `{ "developer": "system" }` |
| `strip_fields` | array | 否 | `[]` | 从上游 JSON body 顶层移除的字段名 |
| `model_strip_fields` | object | 否 | `{}` | 按 `real_model` 的字段移除表 |
| `model_overrides` | object | 否 | `{}` | 按 `real_model` 的 body 覆盖 |
| `anthropic_cache_control` | boolean | 否 | `true` | Anthropic 缓存控制注入，仅当实际使用协议为 `anthropic_messages` 时生效（`apis.anthropic_messages` 可覆盖） |

### 更新供应商

```
PUT /api/providers
Content-Type: application/json

{
  "name": "my-provider",
  "apis": {
    "chat_completions": {
      "base_url": "https://custom.com/v1",
      "models_endpoint": "v1/models"
    }
  }
}
```

只更新请求体中传入的字段，未传入的字段保持原值。支持更新所有[添加供应商](#添加供应商)中列出的字段。

> `apis` 是例外的**整目录替换**：请求体中出现 `apis` 即整体替换现有目录（不按节合并），需发送完整目录。`fallback` 为三态 patch：省略保持原值，`null` 清除，给值则设置。

> `priority` 不是供应商字段，不会参与路由排序。路由目标的优先级应在 `targets[].priority` 上配置。

### 删除供应商

```
DELETE /api/providers
Content-Type: application/json

{
  "name": "my-provider"
}
```

删除供应商及其关联的路由目标。

### 列出供应商模型

```
GET /api/providers/{name}/models
```

从 provider 配置中返回已启用的 `enabled_models` 列表；**不会**实时调用上游 models endpoint。

#### 响应

```json
{
  "models": ["gpt-4o", "gpt-4o-mini", "gpt-3.5-turbo"]
}
```

### 从上游拉取模型列表

```
POST /api/providers/fetch-models
Content-Type: application/json

{
  "base_url": "https://api.openai.com/v1",
  "models_endpoint": "models",
  "api_kind": "chat_completions",
  "api_keys": ["sk-xxx"],
  "headers": {},
  "proxy_mode": "inherit"
}
```

向指定 `apis.<api_kind>` 节的 `base_url + models_endpoint` 发临时 GET，返回上游模型列表。不落盘、不要求 provider 已保存。

| 字段 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `base_url` | string | 是 | 待测 `apis` 节的 `base_url` |
| `models_endpoint` | string | 否 | 相对路径，默认 `models` |
| `api_kind` | string | 否 | 决定鉴权头形状，默认 `chat_completions` |
| `api_keys` | array | 是 | 至少一条（用于构造鉴权头） |
| `headers` | object | 否 | 附加自定义头 |
| `proxy_mode` | string | 否 | `"inherit"` / `"enabled"` / `"disabled"`，仅本次请求 |

响应：`{ "models": [ ...上游 data 数组... ] }`。上游不可达 → 502。

### 测试模型连接

```
POST /api/model/test
Content-Type: application/json

{
  "model": "gpt-4o-mini",
  "api_keys": ["sk-xxx"],
  "base_url": "https://api.openai.com/v1",
  "api_kind": "chat_completions",
  "headers": {}
}
```

向指定的模型发送一条最短消息（`hi`），验证连接和认证是否正常。

**请求体字段**：

| 字段 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `model` | string | 是 | 要测试的模型名 |
| `base_url` | string | 是 | 临时探测地址（待测 `apis.<kind>` 节的 `base_url`，不读取已保存 provider 的地址） |
| `api_kind` | string | 否 | 待测 API 节：`"chat_completions"`（默认）/ `"responses"` / `"anthropic_messages"`，决定认证头形状与聊天端点路径 |
| `api_keys` | array | 是 | 要测试的 API Keys，会逐一测试每个 key |
| `headers` | object | 否 | 附加的自定义请求头 |

**响应**：

```json
{
  "results": [
    {
      "key_prefix": "sk-xxx...",
      "success": true,
      "message": "OK"
    },
    {
      "key_prefix": "sk-yyy...",
      "success": false,
      "message": "Authentication failed: ..."
    }
  ]
}
```

### 探测协议支持

```
POST /api/providers/probe-protocols
Content-Type: application/json

{
  "api_keys": ["sk-xxx"],
  "model": "gpt-4o-mini",
  "targets": [
    { "protocol": "chat_completions", "base_url": "https://api.openai.com/v1" },
    { "protocol": "responses", "base_url": "https://api.openai.com/v1" },
    { "protocol": "anthropic_messages", "base_url": "https://api.anthropic.com/v1" }
  ],
  "headers": {},
  "proxy_mode": "inherit"
}
```

对每个「协议 × base_url × key」发一条最小请求，判断上游是否真的支持该协议。**只读**，不写配置。

| 字段 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `api_keys` | array | 是 | 逐一测试 |
| `model` | string | 是 | 探测用模型；建议非 reasoning 模型（`max_tokens: 1` 可能被 reasoning 模型拒） |
| `targets` | array | 是 | `{ protocol, base_url, headers? }`；`protocol` ∈ 三协议（`image` → 400） |
| `headers` | object | 否 | 应用到全部 target（模拟 provider 级 headers） |
| `proxy_mode` | string | 否 | 同 fetch-models |

探测 URL = `base_url` + 固定协议 path（`/chat/completions` / `/responses` / `/messages`）。

**响应**：

```json
{
  "results": [
    {
      "protocol": "chat_completions",
      "url": "https://api.openai.com/v1/chat/completions",
      "key_prefix": "sk-xxx...",
      "status": 200,
      "message": "OK",
      "verdict": "supported"
    }
  ]
}
```

`verdict` 判定表：

| HTTP | verdict | 含义 |
|------|---------|------|
| 2xx | `supported` | 支持 |
| 401 / 403 | `auth_failed` | 路径存在，鉴权失败 |
| 404 | `uncertain` | 读 body 区分 model_not_found / 未知路由 |
| 405 | `unsupported` | 不支持 |
| 其他 4xx | `partial` | 路径存在，schema/参数被拒 |
| 5xx / 其他 | `uncertain` | 未知 |
| 传输失败 | `error` | `status` 为 `null` |

UI 侧可将 `supported` / `auth_failed` / `partial` 的 `base_url` 回填进 apis 表单，再经 `PUT /api/providers` 保存。

### 创建路由

```
POST /api/routes
Content-Type: application/json

{
  "model_name": "gpt-4o",
  "pipeline": "chat",
  "targets": [
    {
      "provider": "openai",
      "real_model": "gpt-4o",
      "priority": 100,
      "extra": { "temperature": 0.7 }
    }
  ]
}
```

**字段说明**：

| 字段 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `model_name` | string | 是 | 客户端请求的模型名，`"*"` 为兜底路由 |
| `pipeline` | string | 否 | 请求分派管线：`"chat"`（默认，chat 协议族）或 `"image"`（图像生成） |
| `api_endpoint` | string | 否 | **仅 `pipeline = "image"` 合法**：真实上游路径（如 `/images/generations`），拼接在 `apis.image.base_url` 之后；chat 族路由携带此字段报 400 |
| `strategy` | string | 否 | 目标选择策略（`sticky_failover` / `fill_first` / `round_robin`），默认 `sticky_failover` |
| `retries` | integer | 否 | 路由级重试次数；target 可再覆盖 |
| `targets` | array | 是 | 目标供应商列表 |

**target 字段说明**：

| 字段 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `provider` | string | 是 | 供应商名称 |
| `real_model` | string | 否 | 实际模型名，省略则透传客户端原始模型名 |
| `priority` | integer | 否 | 优先级，默认 0，值越大越优先 |
| `retries` | integer | 否 | 覆盖路由/全局重试次数 |
| `extra` | object | 否 | 针对该目标的额外 JSON 字段，合并到上游请求体 |
| `capabilities` | object | 否 | 能力位图覆盖（`streaming` / `reasoning` / `tools` / `images` / `documents` / `stateful` / `hosted_tools` / `background`），未知 key 报错 |

> `timeout_secs` 目前不由 admin API 设置（固定走路由/供应商链）。

### 列出路由

```
GET /api/routes
```

返回所有配置的路由规则。

### 更新路由

```
PUT /api/routes
Content-Type: application/json

{
  "model_name": "gpt-4o",
  "targets": [
    { "provider": "openai", "real_model": "gpt-4o", "priority": 5 }
  ]
}
```

未传入的字段保持原值。`api_endpoint` 为三态 patch：省略保持、`null` 清除（image 路由改回 chat 时必须清）、给值则设置。`pipeline` 省略则保持。

### 删除路由

```
DELETE /api/routes
Content-Type: application/json

{
  "model_name": "gpt-4o"
}
```

### 读取网关配置

```
GET /api/config
```

返回内存生效的顶级配置快照。`providers` / `routes` / `gateway_auth` 不在其中（各有专属端点）。`privacy` 段里的盐值只以布尔 `salt_configured` 出现：响应体**从不包含 `salt` 键**，也不回显其原值或掩码值。

```json
{
  "server": { "host": "127.0.0.1", "port": 3000 },
  "logging": { "level": "info", "format": "compact" },
  "analytics": null,
  "backoff": {
    "retries": 2,
    "base_ms": 100,
    "max_ms": 5000,
    "jitter": true,
    "retry_on_empty_response": true
  },
  "circuit_breaker": {
    "quota_exhausted_secs": 3600,
    "rate_limited_secs": 10,
    "generic_secs": 60,
    "probe_interval_secs": 60
  },
  "shuffle_cache_ttl_secs": 7200,
  "sticky_failover_reset_secs": 7200,
  "proxy_url": null,
  "privacy": {
    "enabled": false,
    "mode": "mask",
    "restore": true,
    "builtins": {
      "email": true,
      "phone": true,
      "id_card_cn": true,
      "bank_card": true,
      "iban": true,
      "api_key": true,
      "private_key": true
    },
    "custom_regexes": [],
    "custom_words": { "items": [] },
    "allowlist": { "items": [] },
    "salt_configured": false
  }
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `server` | object | `host` / `port`；启动期生效，只读 |
| `logging` | object | `level` / `format`（`compact` / `json`）；启动期生效，只读 |
| `analytics` | object \| null | `enabled` / `storage_dsn` / `call_log_retention_days` / `session_retention_days`；`null` 表示缺 `[analytics]` 段 |
| `backoff` | object | 全局重试与退避参数 |
| `circuit_breaker` | object | 三个冷却时长 + `probe_interval_secs`（半开探测间隔） |
| `shuffle_cache_ttl_secs` | integer | 路由 shuffle 缓存 TTL（秒） |
| `sticky_failover_reset_secs` | integer | sticky 目标复位间隔（秒） |
| `proxy_url` | string \| null | 全局出站代理；`null` 表示直连（由 `PUT /api/settings` 修改） |
| `privacy` | object | 隐私过滤设置，见下表；**不含 `salt`** |

**`privacy` 字段**：

| 字段 | 类型 | 说明 |
|------|------|------|
| `enabled` | boolean | 主开关，默认 `false`（完全不过滤） |
| `mode` | string | 命中后的处理方式：`"mask"`（默认，替换为占位符）/ `"warn"`（原样放行，只记警告与类别计数）/ `"block"`（拒绝请求） |
| `restore` | boolean | 是否在上游响应里把占位符还原为原文，默认 `true` |
| `builtins` | object | 七个内置检测器开关：`email` / `phone` / `id_card_cn` / `bank_card` / `iban` / `api_key` / `private_key`；每个缺键即视为 `true` |
| `custom_regexes` | array | 自定义正则规则，每条 `{ "kind": string, "pattern": string, "case_insensitive": boolean }`；`case_insensitive` 缺省 `false` |
| `custom_words` | object | `{ "items": [string] }`，逐字面命中的词 |
| `allowlist` | object | `{ "items": [string] }`，整段精确相等才算放行（大小写敏感，不是子串匹配），命中的词永不被掩码 |
| `salt_configured` | boolean | 只读；是否已存非空盐值。盐值本身（含掩码形式）永不返回 |

`mask` 的占位符形如 `[[EMAIL_9f2c4a1b7d30]]`（类别大写 + 12 位小写 hex），同一 `(kind, 原文, salt)` 恒得同一占位符。两个词表是 `{ "items": [...] }` 对象而非裸数组；`pattern` 是有语义的原始文本，前后空白同样属于模式的一部分。

### 更新网关配置

```
PUT /api/config
Content-Type: application/json

{
  "privacy": {
    "enabled": true,
    "mode": "mask",
    "restore": true,
    "builtins": {
      "email": true,
      "phone": true,
      "id_card_cn": true,
      "bank_card": true,
      "iban": true,
      "api_key": true,
      "private_key": true
    },
    "custom_regexes": [
      { "kind": "employee_id", "pattern": "EMP-\\d{4}", "case_insensitive": true }
    ],
    "custom_words": { "items": [] },
    "allowlist": { "items": [] },
    "salt": "example-only-salt"
  }
}
```

> 上例的 `example-only-salt` 是虚构占位值，只用于说明字段位置，不是凭据。请勿把真实盐值、密钥或生产配置粘进文档、日志与工单。

只更新请求体中出现的字段，未出现的字段保持原值。`backoff` / `circuit_breaker` / `privacy` 都是**整对象替换**：对象一旦出现，其全部字段必填。`proxy_url` 由 `PUT /api/settings` 拥有，`server` / `logging` / `analytics` 不可经该端点修改（未声明的字段被 serde 静默忽略）。

**`privacy` 的写入规则**：

- 省略 `privacy` 或传 `null`：隐私段完全不变。只改 backoff / TTL 的 PUT 不会创建、补齐或重写已有 `[privacy]` 节。
- `privacy` 出现即整对象替换七个非 salt 字段（`enabled` / `mode` / `restore` / `builtins` / `custom_regexes` / `custom_words` / `allowlist`），缺任何一个 → 422。
- `salt` 是唯一可选字段，且**只写不回显**：
  - 省略 / `null` / `""` → 保持已存盐值，`salt_configured` 维持原样。
  - 非空字符串 → 按字面字节替换（不 trim、不规范化；前后空格是该盐值的一部分）。
  - **没有清空盐值的请求形式**：清空只能手改配置文件。改盐会让所有占位符摘要漂移，`restore` 无法还原改盐前生成的占位符。
- 未知字段被忽略（含 `privacy` 内以及 `builtins` / 词表内的未知键）；未知 `mode` 在反序列化阶段即被拒绝（422，不会走到校验与提交）。
- `custom_regexes[i]` 的 `pattern` 无法编译或 `kind` 为空白 → 400，错误信息带 `privacy.custom_regexes[i]`；`enabled = false` 时同样拒绝。

成功时返回与 `GET /api/config` 同形状的快照（**同样不含 `salt`**）。

**失败与生效语义**：

- 校验失败、外部编辑冲突、写入前的 IO 失败：整个请求不提交。`privacy` 与同一请求里的 `backoff` / `circuit_breaker` / TTL 一起保留旧值（一次 PUT 只走一次统一的 mutation 事务），内存与文件都不变。
- 5xx、响应丢失、网络中断：**不能保证未生效**。提交之后仍可能有后续步骤报错（如 analytics 身份同步），也可能只是响应没送达；应重新 `GET /api/config` 确认，而不是假设旧值仍在。
- 保存成功后对**后续新请求**生效（分发路径每次从内存配置读取），无需重启；进行中的请求不中断；privacy 变更不重建 provider。

### 读取网关设置

```
GET /api/settings
```

返回网关级配置快照：

```json
{
  "proxy_url": "http://proxy.example:8080",
  "api_keys": ["sk-client-a", "sk-client-b"]
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `proxy_url` | string \| null | 全局出站 HTTP/HTTPS 代理；`null` 表示直连 |
| `api_keys` | array | 网关客户端认证密钥（`[gateway_auth].api_keys`）；空数组表示未启用认证 |

### 更新网关设置

```
PUT /api/settings
Content-Type: application/json

{
  "proxy_url": "http://proxy.example:8080",
  "api_keys": ["sk-client-a"]
}
```

| 字段 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `proxy_url` | string \| null | 是 | 设置或清除全局代理（`null` / 空字符串清除） |
| `api_keys` | array | 否 | 替换网关客户端认证密钥列表；省略则保持不变；空数组关闭认证 |

**校验**：

- `proxy_url` 必须是合法 `http://` / `https://` URL，且不含 userinfo；否则 400
- `api_keys` 中不得包含空白条目；否则 400
- 将 `proxy_url` 清空时，若存在 `proxy_mode = "enabled"` 的供应商会返回 400

**副作用**：变更会立即写入配置文件并在内存生效；认证中间件每次请求读取当前配置，因此密钥变更即时生效，无需重启。
