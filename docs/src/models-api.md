# 模型 API

模型 API 是与 OpenAI API 兼容的接口，可直接替换 OpenAI SDK 中的 `base_url`。

## 认证

模型 API 使用 Bearer token 认证：

```
Authorization: Bearer <api_key>
```

API Key 在配置文件 `gateway.toml` 的 `[gateway_auth]` 段中定义。认证行为：

| 配置 | 行为 |
|------|------|
| 无 `[gateway_auth]` 段 | 完全开放，无需认证 |
| `api_keys = []` | 完全开放（同上） |
| `api_keys = ["key1", "key2"]` | 只有提供有效 key 的请求可以通过 |

---

## POST /v1/chat/completions

创建聊天补全。兼容 OpenAI Chat Completions API 格式，支持流式和非流式请求。

### 请求体

```json
{
  "model": "gpt-4o",
  "messages": [
    { "role": "system", "content": "You are a helpful assistant." },
    { "role": "user", "content": "Hello!" }
  ],
  "temperature": 0.7,
  "stream": false,
  "max_tokens": 1024
}
```

请求体中的字段与 OpenAI 一致。非标准的额外字段会通过 `extra` 透传给上游供应商。

### 非流式响应

```json
{
  "id": "chatcmpl-xxx",
  "object": "chat.completion",
  "created": 1686935002,
  "model": "gpt-4o",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "Hello! How can I help you?"
      },
      "finish_reason": "stop"
    }
  ],
  "usage": {
    "prompt_tokens": 10,
    "completion_tokens": 7,
    "total_tokens": 17
  }
}
```

### 流式响应

当请求体 `stream: true` 时，返回 SSE (Server-Sent Events) 流式响应，格式与 OpenAI 一致。

### 路由与故障转移

请求到达后的处理流程：

1. **匹配路由** — 根据请求的 `model` 在路由表中查找（精确匹配 → 通配符 `*`）
2. **选择目标** — 按优先级降序排列目标
3. **尝试供应商** — 依次调用，4xx 错误立即停止，5xx/超时自动故障转移到下一个目标
4. **全部失败** — 返回 `502 All Providers Exhausted`

详细的路由配置说明见[路由](./routing.md)。

### 错误响应

```json
{
  "error": {
    "code": "MODEL_NOT_FOUND",
    "message": "unknown-model"
  }
}
```

| HTTP 状态码 | 错误码 | 说明 |
|-------------|--------|------|
| 400 | `BAD_REQUEST` | 请求中包含供应商不支持的参数 |
| 401 | `UNAUTHORIZED` | API Key 无效或缺失 |
| 404 | `MODEL_NOT_FOUND` | 模型未在路由表中配置 |
| 502 | `ALL_PROVIDERS_EXHAUSTED` | 所有供应商均失败 |
| 502 | `UPSTREAM_ERROR` | 上游供应商返回错误 |
| 502 | `NETWORK_ERROR` | 网络连接失败 |
| 500 | `INTERNAL_ERROR` | 内部错误 |

---

## GET /v1/models

返回网关中可用的模型列表。模型列表来源于路由表配置，每个路由的 `model_name` 作为一个模型 ID 返回。

### 响应

```json
{
  "object": "list",
  "data": [
    {
      "id": "gpt-4o",
      "object": "model",
      "created": 0,
      "owned_by": "openai"
    },
    {
      "id": "claude-sonnet",
      "object": "model",
      "created": 0,
      "owned_by": "anthropic"
    }
  ]
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `id` | string | 模型名称，对应路由表中的 `model_name`（包括通配符 `*` 路由） |
| `object` | string | 固定为 `"model"` |
| `created` | integer | Unix 时间戳（当前固定为 `0`） |
| `owned_by` | string | 该路由第一个目标的供应商名称 |

---

## GET /health

检测网关是否正常运行。无需认证。

### 响应

```json
{
  "status": "ok",
  "version": "0.1.0"
}
```

---

## GET /v1/analytics/stats

查询聚合的使用统计数据。支持按供应商和模型过滤。

查询参数：

| 参数 | 类型 | 必需 | 说明 |
|------|------|------|------|
| `provider` | string | 否 | 按供应商名称过滤 |
| `model` | string | 否 | 按模型名称过滤 |

### 响应

```json
{
  "data": [
    {
      "provider": "openai",
      "model": "gpt-4o",
      "call_count": 100,
      "total_prompt_tokens": 5000,
      "total_completion_tokens": 3000,
      "total_tokens": 8000,
      "avg_duration_ms": 1200.5,
      "error_count": 2
    }
  ]
}
```

详细说明见[分析](./analytics.md)。
