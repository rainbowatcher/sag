# 模型 API

模型 API 与 OpenAI / Anthropic 官方 API 兼容，可直接替换 SDK 中的 `base_url`。

## 入口协议

| 端点 | 协议 | 说明 |
|------|------|------|
| `POST /v1/chat/completions` | OpenAI Chat Completions | 流式 / 非流式；`pipeline = "image"` 的路由在此入口做图像生成 |
| `POST /v1/responses` | OpenAI Responses | 无状态子集；`store` / `previous_response_id` 等有状态字段在部分场景被拒绝（422） |
| `POST /v1/messages` | Anthropic Messages | Anthropic SDK 形状；响应与错误均为 Anthropic envelope |

同一模型可同时支持多个入口——请求落在哪个入口，就按「入口协议 ∈ 上游 `apis` 协议集」自动决定直通（Native）或翻译降级（Translated）。详见[路由](/guide/routing)与[配置](/guide/configuration)。

## 认证

```
Authorization: Bearer <api_key>
```

`POST /v1/messages` 额外接受 Anthropic SDK 的 `x-api-key`（与 Bearer 等价）：

```
x-api-key: <api_key>
```

**其他所有端点忽略 `x-api-key`**，必须使用 `Authorization: Bearer`。

API Key 在 `gateway.toml` 的 `[gateway_auth]` 段定义：

| 配置 | 行为 |
|------|------|
| 无 `[gateway_auth]` 段 | 完全开放，无需认证 |
| `api_keys = []` | 完全开放（同上） |
| `api_keys = ["key1", "key2"]` | 只有提供有效 key 的请求可以通过 |

认证失败：`/v1/messages` 返回 Anthropic 错误 envelope；其余端点返回 OpenAI 错误 envelope。

---

## POST /v1/chat/completions

创建聊天补全。兼容 OpenAI Chat Completions API 格式。

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

字段与 OpenAI 一致。未识别的额外字段经 `extra` 透传给上游。

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
      "message": { "role": "assistant", "content": "Hello! How can I help you?" },
      "finish_reason": "stop"
    }
  ],
  "usage": { "prompt_tokens": 10, "completion_tokens": 7, "total_tokens": 17 }
}
```

### 流式响应

`stream: true` 时返回 SSE，格式与 OpenAI 一致（以 `data: [DONE]` 结束）。

### 图像生成

`pipeline = "image"` 的路由通过本入口接收请求（OpenAI images 形状或带 prompt 的 chat 形状，取决于路由配置），上游地址 = `apis.image.base_url` + 路由 `api_endpoint`。非 image 路由不会走此管线。

---

## POST /v1/responses

OpenAI Responses 入口（无状态子集）。

- 请求 / 响应形状与 OpenAI Responses API 一致
- 有状态字段（`store`、`previous_response_id` 等）仅当路由的**全部**目标都声明了 `apis.responses`（即 Native）时允许；否则 **422**
- 流式返回 Responses SSE 帧
- 错误使用 OpenAI envelope

---

## POST /v1/messages

Anthropic Messages 入口。

- 请求 / 响应形状与 Anthropic Messages API 一致
- 支持 `x-api-key` 鉴权（见上文）
- 流式返回 Anthropic SSE
- 错误使用 Anthropic envelope：

```json
{
  "type": "error",
  "error": { "type": "invalid_request_error", "message": "..." }
}
```

图像生成在本入口不受支持（返回 400）。

---

## 路由与故障转移

1. **匹配路由** — 按请求 `model` 查路由表（精确 → 通配符 `*`）
2. **选择目标** — 按优先级降序
3. **尝试供应商** — 依次调用；按错误类别决定是否 failover（4xx 多为 provider-persistent，5xx/超时可重试）
4. **全部失败** — `502 All Providers Exhausted`（或对应 envelope）

配置说明见[路由](/guide/routing)。

### 错误响应

OpenAI envelope（`/v1/chat/completions`、`/v1/responses`、`/v1/models`）：

```json
{
  "error": {
    "code": "MODEL_NOT_FOUND",
    "message": "unknown-model"
  }
}
```

| HTTP | 错误码 | 说明 |
|------|--------|------|
| 400 | `BAD_REQUEST` | 请求参数不被支持 |
| 401 | `UNAUTHORIZED` | API Key 无效或缺失 |
| 404 | `MODEL_NOT_FOUND` | 模型未在路由表中 |
| 422 | `UNPROCESSABLE` | 字段不被支持（如 Responses 有状态字段） |
| 502 | `ALL_PROVIDERS_EXHAUSTED` | 所有目标均失败 |
| 502 | `UPSTREAM_ERROR` | 上游返回错误 |
| 502 | `NETWORK_ERROR` | 网络失败 |
| 500 | `INTERNAL_ERROR` | 内部错误 |

`/v1/messages` 使用 Anthropic envelope（`type: "error"`）。

---

## GET /v1/models

返回路由表中的可用模型（每个路由的 `model_name` 为一个模型 ID）。全部目标指向禁用供应商、或不在 `enabled_models` 白名单内的路由会被排除。

### 响应

```json
{
  "object": "list",
  "data": [
    {
      "id": "gpt-4o",
      "object": "model",
      "created": 0,
      "owned_by": "openai",
      "ingress_protocols": ["chat_completions", "responses"],
      "lanes": ["native", "translated"]
    }
  ]
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `id` | string | 模型名称（路由 `model_name`，含通配符 `*`） |
| `object` | string | 固定 `"model"` |
| `created` | integer | 固定 `0` |
| `owned_by` | string | 首个目标的供应商名 |
| `ingress_protocols` | string[] | 各目标供应商 `apis` 协议 key 的并集（canonical 序）。客户端可据此判断哪些入口可直通 |
| `lanes` | string[] | 固定 `["native", "translated"]` |

两键**恒输出**（不再依赖显式协议声明）。

---

## GET /health

检测网关是否正常运行。无需认证。

```json
{ "status": "ok", "version": "0.4.0" }
```

---

## GET /v1/analytics/stats

聚合使用统计（**诊断口径**：热窗口 + 近似），支持按供应商 / 模型过滤。查询参数：`provider`、`model`、`real_model`（均可选）。响应按 `(provider, model, real_model)` 分组，每组一行：

```json
{
  "data": [
    {
      "provider": "openai",
      "model": "gpt-4o",
      "real_model": "gpt-4o-2024-08-06",
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

该端点读 `call_logs_hot`（热窗口明细），`NULL` Token 当 `0`，写入可丢行，因此**与 `/v1/analytics/v2/*` 的数字不可对比**（对不上是预期行为）。需要验收口径请直接用 v2，详见[分析 → 诊断口径 vs 分析口径](/guide/analytics#诊断口径-vs-分析口径)。

统计 / 趋势 / 分布分析请用 `/v1/analytics/v2/summary` 与 `/v1/analytics/v2/trend`（`scope=route|target`）；旧 `GET /v1/analytics/trend`、`/v1/analytics/distribution`、`/v1/analytics/cache` 已下线，详见[分析](/guide/analytics)。
