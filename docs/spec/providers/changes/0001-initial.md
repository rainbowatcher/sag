---
created: 2026-06-03
updated: 2026-06-03
type: 新增
---

# 0001 — 供应商适配器初始化

## 动机
屏蔽上游 LLM API 差异，对客户端暴露统一的 OpenAI 兼容接口。

## 需求变更
- requirements.md 全部初始定义

## 代码变更
| 文件 | 变更 |
|------|------|
| [sag-providers/src/traits.rs](https://github.com/rainbowatcher/sag/blob/main/sag-providers/src/traits.rs) | LlmProvider trait 定义 |
| [sag-providers/src/base.rs](https://github.com/rainbowatcher/sag/blob/main/sag-providers/src/base.rs) | 共享 HTTP client、headers、body_overrides |
| [sag-providers/src/openai.rs](https://github.com/rainbowatcher/sag/blob/main/sag-providers/src/openai.rs) | OpenAI 适配器 |
| [sag-providers/src/anthropic.rs](https://github.com/rainbowatcher/sag/blob/main/sag-providers/src/anthropic.rs) | Anthropic 适配器（请求/响应翻译） |
| [sag-providers/src/lib.rs](https://github.com/rainbowatcher/sag/blob/main/sag-providers/src/lib.rs) | create_provider() 工厂函数 |

## 影响的需求条目
- 全部（初始定义）
