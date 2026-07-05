---
created: 2026-07-05
---

# 0001 — Body Override 语义正式化

## 变更内容

新增 `body-override` 规格模块，将既有的三层 body override 机制（`body_overrides` → `model_overrides` → `target.extra`）正式化为文档化的功能规格。

- **需求文档**（`requirements.md`）：定义三层覆盖的目标、预期行为、配置 Schema、功能边界、与其它模块的交互。
- **规格文档**（`spec.md`）：描述关键文件、数据流图、合并算法、关键设计决策、不变式、测试策略。

## 背景

此前三层覆盖的语义分散在代码注释和指南文档（`routing.md`、`configuration.md`、`providers.md`）中，缺乏集中式的技术规格。随着 `.plan/move-extra-to-provider-model.md` 计划被评估为不建议实施，需要通过正式规格将当前三层架构确立为长期设计基准。

## 设计要点

1. **优先级链**：供应商全局（`body_overrides`）< 模型级（`model_overrides[real_model]`）< 路由目标级（`target.extra`）。
2. **合并位置**：`merge_model_overrides` 在调度层完成模型级与目标级的合并；`ProviderBase::build_request` 依次应用全局覆盖和合并后的目标覆盖。
3. **替换语义**：嵌套 JSON object 采用整体替换，不深度合并。
4. **空值短路**：任意层级为空 map 时不产生副作用。

## 关联文档

- `docs/guide/routing.md` — 用户指南中的优先级与最佳实践
- `docs/guide/configuration.md` — Provider 字段说明与示例
- `docs/guide/providers.md` — Provider 公共字段与 Body Override 对比
- `gateway.example.toml` — 配置注释示例
