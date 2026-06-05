# SAG — Smart AI Gateway

SAG 是一个用于大语言模型（LLM）API 的 HTTP 反向代理/网关。它接受兼容 OpenAI 格式的请求，并将其路由到可配置的上游供应商，支持基于优先级的供应商自动故障转移。

## 功能特性

- **兼容 OpenAI API** — 可直接替换 `POST /v1/chat/completions`
- **多供应商路由** — 将模型路由到不同供应商，支持优先级故障转移
- **供应商适配器** — 内置 OpenAI 和 Anthropic 支持，可通过 trait 扩展
- **使用分析** — 基于 SQLite 的日志记录和聚合统计
- **管理 REST API** — 运行时管理供应商和路由
- **配置热重载** — 无需重启即可更新配置
- **流式支持** — SSE 流式传输直达客户端

## 子包

| 子包 | 说明 |
|-------|------|
| `sag-core` | 共享类型、模型和错误定义 |
| `sag-config` | 基于 TOML 的配置，支持热重载 |
| `sag-providers` | LLM 供应商适配器（OpenAI、Anthropic） |
| `sag-router` | HTTP 路由、中间件和处理器 |
| `sag-analytics` | 使用分析和 SQLite 日志记录 |
| `sag-gateway` | 二进制入口点和启动编排 |
