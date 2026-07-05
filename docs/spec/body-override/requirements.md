---
created: 2026-07-05
updated: 2026-07-05
---

# Body Override — 需求

## 目标
为上游请求体提供三层可组合的字段覆盖机制，使运维人员能够在供应商全局、模型级和路由目标级三个粒度上注入或修改请求参数，且高优先级覆盖始终能够覆盖低优先级的同名字段。

## 预期行为

### 三层覆盖机制
- **供应商全局覆盖**（`ProviderConfig.body_overrides`）：对该供应商的所有请求生效，用于设置供应商级别的默认值（如统一的 `max_tokens` 或自定义 header 配套参数）。
- **模型级覆盖**（`ProviderConfig.model_overrides[real_model]`）：仅对指定的真实模型生效，用于为不同模型设置差异化默认参数（如 `gpt-4o` 用高 temperature，`gpt-4o-mini` 用低 temperature）。
- **路由目标级覆盖**（`RouteTarget.extra`）：仅对当前路由目标生效，用于为特定路由场景设置最终覆盖（如创意类路由提高 temperature）。

### 优先级与合并语义
- 三层覆盖按 **供应商全局 → 模型级 → 路由目标级** 的顺序依次应用，后写入的覆盖先写入的。
- 同名字段在高层覆盖低层；不同名字段在各层之间累积保留。
- 客户端请求体中的字段优先级低于所有三层覆盖（即覆盖机制可以修改客户端传来的任何字段）。
- 嵌套对象（JSON object）在覆盖时采用**整体替换**策略，而非深度合并。

### 配置与持久化
- 三层覆盖的配置均通过 TOML 文件定义，支持热重载。
- `body_overrides` 和 `model_overrides` 位于 `[providers.<name>]` 段下；`extra` 位于 `[[routes]]` 的 `targets` 内联表中。
- 管理 API（admin）支持对 `body_overrides` 和 `model_overrides` 的 CRUD 操作；`extra` 通过路由管理 API 修改。

### 空值与禁用
- 当某一层级的覆盖 map 为空时，该层级不产生任何效果。
- 当 `model_overrides` 中不存在当前 `real_model` 对应的条目时，模型级覆盖跳过，不影响其它层级。

## 配置 Schema

### ProviderConfig 中的覆盖字段
```toml
[providers.openai]
# 全局覆盖：对该供应商所有请求生效
[providers.openai.body_overrides]
max_tokens = 4096

# 模型级覆盖：仅对指定 real_model 生效
[providers.openai.model_overrides]
gpt-4o = { temperature = 0.8 }
gpt-4o-mini = { max_tokens = 2048 }
```

### RouteTarget 中的覆盖字段
```toml
[[routes]]
model_name = "creative-gpt"
targets = [
  { provider = "openai", real_model = "gpt-4o", extra = { temperature = 0.9 } },
]
```

完整字段定义参见 [配置模块](/spec/config/requirements)。

## 功能边界

### 包含
- 三层覆盖的优先级定义与合并规则。
- 覆盖字段在请求体中的位置：所有键均插入/替换到请求 JSON 的顶层。
- 热重载支持：配置变更后新请求立即生效。
- 管理 API 对覆盖配置的读写支持。

### 不包含
- 请求体字段的校验（如 `temperature` 必须在 0~2 之间）—— 由上游供应商返回错误。
- 覆盖值的类型校验（TOML 解析阶段仅保证合法 JSON 值）。
- 深度合并嵌套对象（ nested object 替换而非递归合并）。
- 覆盖的审计日志或历史版本追踪。

## 与其它模块的交互

- **配置模块**：`ProviderConfig` 和 `RouteTarget` 结构体中定义覆盖字段的 schema；`loader.rs` 负责从 TOML 解析并校验 null 值拒绝。
- **调度模块**：`merge_model_overrides` 合并模型级与目标级覆盖；`dispatch_chat` / `dispatch_image` 将合并结果传递给 provider。
- **供应商模块**：`ProviderBase::build_request` 依次应用 `body_overrides` 和合并后的 `target_extra`；`apply_body_overrides` 执行实际的键值插入。
- **管理 API 模块**：`admin_providers.rs` 暴露 `body_overrides` 和 `model_overrides` 的读写端点；`admin_routes.rs` 暴露 `extra` 的读写端点。
- **路由模块**：路由解析确定 `real_model`，作为 `model_overrides` 的查询 key。

## 变更记录

- [0001 初始定义](changes/0001-initial.md)
