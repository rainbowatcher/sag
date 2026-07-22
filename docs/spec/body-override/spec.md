---
created: 2026-07-05
updated: 2026-07-22
---

# Body Override — 规格

## 关键文件

### 类型定义与配置加载
- `sag-config/src/config.rs` — `ProviderConfig.body_overrides`、`ProviderConfig.model_overrides`、`RouteTarget.extra` 的 struct 定义与默认值；`ProviderConfig.strip_fields` 定义。
- `sag-config/src/loader.rs` — TOML 解析、null 值拒绝校验、热重载持久化（`save_config_to_file` 中的 inline table 同步逻辑）。

### 合并与应用
- `sag-dispatch/src/dispatch/mod.rs` — `merge_model_overrides`：将 `model_overrides[real_model]` 与 `target.extra` 合并为 `target_extra`。
- `sag-providers/src/base.rs` — `ProviderBase::build_request`：依次应用 `body_overrides`、`target_extra`，再调用 `apply_body_strip` 移除指定字段。
- `sag-providers/src/util.rs` — `apply_body_overrides`：将覆盖 map 插入请求体 JSON 的顶层对象；`apply_body_strip`：从 JSON body 顶层移除指定字段。

### 管理 API
- `sag-admin/src/admin_providers.rs` — Provider CRUD 中对 `body_overrides`、`model_overrides` 和 `strip_fields` 的读写。
- `sag-admin/src/admin_routes.rs` — Route CRUD 中对 `RouteTarget.extra` 的读写。

## 核心设计

### 数据流

请求到达后，覆盖值按以下顺序流向请求体：

```
┌─────────────────────────────────────────────────────────────┐
│  Client Request Body                                          │
│  (原始请求 JSON，优先级最低)                                  │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│  Layer 1: ProviderConfig.body_overrides                     │
│  位置: ProviderBase::build_request (第 1 次 apply)           │
│  范围: 该供应商的所有请求                                     │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│  Layer 2: ProviderConfig.model_overrides[real_model]        │
│  位置: merge_model_overrides (作为 merged map 的基底)        │
│  范围: 该供应商指定 real_model 的请求                         │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│  Layer 3: RouteTarget.extra                                 │
│  位置: merge_model_overrides (覆盖 merged map 中的同名字段)  │
│        → ProviderBase::build_request (第 2 次 apply)         │
│  范围: 当前路由目标                                           │
└─────────────────────────────────────────────────────────────┘
	                              │
	                              ▼
	┌─────────────────────────────────────────────────────────────┐
	│  Layer 4: ProviderConfig.strip_fields (最终净化)             │
	│  位置: ProviderBase::build_request (第 3 步: apply_body_strip) │
	│  范围: 该供应商的所有请求（硬性移除，优先级最高）              │
	└─────────────────────────────────────────────────────────────┘
	                              │
	                              ▼
	                    最终 upstream request body
```

### 合并算法

#### `merge_model_overrides`（调度模块）

```rust
pub(in crate::dispatch) fn merge_model_overrides(
    providers: &HashMap<String, ProviderConfig>,
    provider_name: &str,
    real_model: &str,
    target_extra: &HashMap<String, serde_json::Value>,
) -> HashMap<String, serde_json::Value> {
    let model_ov = providers
        .get(provider_name)
        .and_then(|pc| pc.model_overrides.get(real_model))
        .cloned()
        .unwrap_or_default();
    let mut merged = model_ov;           // Layer 2 作为基底
    for (k, v) in target_extra {
        merged.insert(k.clone(), v.clone());  // Layer 3 覆盖 Layer 2
    }
    merged
}
```

#### `apply_body_overrides`（供应商工具）

```rust
pub fn apply_body_overrides(
    mut body: serde_json::Value,
    overrides: &HashMap<String, serde_json::Value>,
) -> serde_json::Value {
    if overrides.is_empty() { return body; }
    if let serde_json::Value::Object(map) = &mut body {
        for (key, value) in overrides {
            map.insert(key.clone(), value.clone());  // 同 key 直接替换
        }
    }
    body
}
```

#### `apply_body_strip`（供应商工具）

```rust
pub fn apply_body_strip(
    mut body: serde_json::Value,
    fields: &[String],
) -> serde_json::Value {
    if fields.is_empty() { return body; }
    if let serde_json::Value::Object(map) = &mut body {
        for key in fields {
            map.remove(key.as_str());  // 从顶层移除指定字段
        }
    }
    body
}
```

#### `ProviderBase::build_request`（供应商基础）

```rust
let body = apply_body_overrides(body, &self.body_overrides);  // Layer 1
let body = apply_body_overrides(body, target_extra);          // Layer 2+3 (已合并)
let body = apply_body_strip(body, &self.strip_fields);        // Layer 4: 最终净化
```

### 类型关系

```
ProviderConfig
├── body_overrides: HashMap<String, Value>          # Layer 1 (全局)
├── model_overrides: HashMap<String, HashMap<String, Value>>  # Layer 2 (按模型)
├── strip_fields: Vec<String>                       # Layer 4 (最终净化)
└── ...

RouteTarget
├── provider: String
├── real_model: Option<String>
├── extra: HashMap<String, Value>                   # Layer 3 (目标级)
└── ...

ResolvedTarget (调度模块生成)
├── provider: String
├── real_model: String
├── extra: HashMap<String, Value>                   # merge_model_overrides 的结果
└── ...
```

## 关键决策

### 为什么使用三层而不是两层或四层？

两层方案（只有全局 + 目标级）会导致同一 `(provider, real_model)` 在多个路由中重复配置相同的模型级参数；四层方案（如增加路由级或客户端级）会增加认知负担而无明显收益。三层架构在「避免重复」和「保留灵活性」之间取得了平衡。

**Trade-off**:
- ✅ 模型级覆盖（`model_overrides`）消除了同一模型在多路由中的重复配置。
- ✅ 目标级覆盖（`extra`）保留了路由级差异化的能力（如创意路由 vs 精确路由使用同一模型但不同 temperature）。
- ❌ 三层优先级需要文档和训练才能正确理解；用户可能误将全局配置写在目标级。

### 为什么嵌套对象采用整体替换而非深度合并？

`apply_body_overrides` 对 JSON object 类型的值采用 `map.insert(key, value)` 整体替换。例如，若 `body_overrides` 设置了 `thinking = { type = "enabled" }`，而 `target.extra` 设置了 `thinking = { budget_tokens = 1600 }`，最终 `thinking` 的值是 `{ budget_tokens = 1600 }`，而非合并后的 `{ type = "enabled", budget_tokens = 1600 }`。

**Trade-off**:
- ✅ 行为可预测、无歧义：用户明确知道最终值是什么。
- ✅ 实现简单，无需递归合并算法和深度克隆。
- ❌ 无法部分修改嵌套对象（如只改 `thinking.budget_tokens` 而保留 `thinking.type`）。用户必须在最高优先级层提供完整的嵌套对象。

### 为什么 `model_overrides` 在 dispatch 层合并，而非 provider 层？

`merge_model_overrides` 位于 `sag-dispatch/src/dispatch/mod.rs`，在生成 `ResolvedTarget` 时完成合并；`ProviderBase::build_request` 只接收合并后的 `target_extra`。这避免了每个 provider 适配器重复实现相同的合并逻辑。

**Trade-off**:
- ✅ 合并逻辑集中在一处，所有 provider 共享。
- ✅ Provider trait 保持简洁（`target_extra` 参数语义单一）。
- ❌ Dispatch 层需要访问 `config.providers` 来查询 `model_overrides`。

### 为什么 strip_fields 在 override 之后应用，而不是独立的一层覆盖？

`strip_fields` 的语义是「最终净化」而非「覆盖」：它不是替换字段值，而是移除字段本身。放在 override 之后应用确保：

1. **优先级不可逆**：即使某字段在前面三层中被显式覆盖/插入，strip 也能将其移除——这是设计意图（客户端无论如何透传了不该透传的字段，最终都要移除）。
2. **配置矛盾时行为确定**：当 `body_overrides` 设置了某字段而 `strip_fields` 也列出该字段时，strip 胜出。用户明确表达了「无论如何都要移除」的意愿。

**Trade-off**:
- ✅ 语义清晰：override 是「我想要什么值」，strip 是「我绝对不要什么字段」。
- ✅ 实现简单：两行 `apply_body_overrides` + 一行 `apply_body_strip`。
- ❌ 无法在 strip 之后再做 override（但无此需求：strip 是针对上游 schema 限制的被动防御，而非主动配置）。
- ❌ 如果用户误将想要的字段放入 `strip_fields`，即使后面 override 也无法恢复（当前不考虑撤销 strip，因为无此使用场景）。

## 不变式

### 优先级链
- `body_overrides` 中设置的键可以被 `model_overrides[real_model]` 覆盖。
- `model_overrides[real_model]` 中设置的键可以被 `RouteTarget.extra` 覆盖。
- `strip_fields` 中列出的字段在以上三层覆盖**之后**硬性移除，优先级最高。
- 客户端请求体中的键可以被以上任意一层覆盖。

### 空值语义
- 若 `body_overrides` 为空 map，`apply_body_overrides` 短路返回原 body，不修改。
- 若 `model_overrides` 不包含当前 `real_model`，`merge_model_overrides` 以空 map 为基底，结果等价于 `target_extra` 的克隆。
- 若 `target_extra` 为空，`merge_model_overrides` 返回 `model_overrides[real_model]` 的克隆。
- 若 `strip_fields` 为空 vec，`apply_body_strip` 短路返回原 body，不修改。

### 键唯一性
- 合并后的 `target_extra` 是一个 flat `HashMap<String, Value>`，不存在多层嵌套的覆盖优先级。
- 同名的顶层键，以最后应用的一次为准。
- 若某键同时出现在 override 层和 `strip_fields` 中，strip 胜出——该键从最终 body 中移除。

## 测试策略

### 单元测试位置
- `sag-dispatch/src/dispatch/mod.rs` 内联测试 — `merge_model_overrides` 的合并语义：
  - `merge_target_extra_overrides_model_override`：同 key 时 target extra 胜。
  - `merge_no_model_override_returns_target_extra`：无 model_override 时返回 target_extra 克隆。
  - `merge_empty_target_extra_returns_model_overrides`：无 target_extra 时返回 model_override 克隆。
- `sag-config/src/config.rs` 内联测试 — TOML 解析正确性（`parse_provider_with_custom_fields` 已覆盖 `body_overrides`）。
- `sag-providers/src/base.rs` 内联测试 — `build_request` 对 headers 和 body 的应用顺序。
- `sag-providers/src/util.rs` 内联测试 — `apply_body_strip` 的字段移除行为。

### 关键场景
1. 三层同 key 覆盖：确认最终值为 `RouteTarget.extra` 中的值。
2. 不同 key 在各层分布：确认三层所有 key 均出现在最终 body 中。
3. 嵌套 object 替换：确认高层嵌套 object 完全替换低层同 key 嵌套 object。
4. 空 map 短路：确认空覆盖层不触发无意义的克隆和插入。

## 变更记录

- [0002 per-provider strip_fields 字段剥离](changes/0002-strip-fields.md)
- [0001 初始定义](changes/0001-initial.md)
