---
created: 2026-09-15
updated: 2026-09-15
type: 修改
---

# 0006 — target_index 改为优先级降序秩

## 动机

前端 `isActiveFirstTarget` 用后端 `target_index === 0` 判定主目标。旧语义下 `target_index`
是 target 在原始 `route.targets` 数组中的 0-based 位置（与 `priority` 无关），一旦配置了
priority，数组首项不一定是最高优先级 target，前端会误判主目标。改为优先级降序秩后，
`target_index === 0` 恒为 route 的最高优先级（primary）target；全部默认优先级 0 时
stable sort 保持数组顺序，行为与旧语义完全一致（无回归）。

## 需求变更

- 本模块无 `requirements.md`（只有 spec.md），按要求跳过需求文档更新，不新建空文件。
- spec.md「状态圆点颜色语义」节：`target_index` 描述从「原始 route.targets 数组 0-based
  索引（与 priority 无关）」更新为「route 优先级降序排序中的秩（stable sort by `priority`
  降序、同优先级保持原始数组顺序、rank 0 = 最高优先级 target）」；绿色语义从「命中
  route.targets 原顺序首项」更新为「命中 route 最高优先级（primary）target」。
- spec.md 后端不变式：`target_index` 与 priority 无关 → 优先级降序秩；补充说明响应 `targets`
  数组本身保持原始 config 顺序不变。
- spec.md 测试策略：`target_index_mirrors_route_targets_array_order` 更新为
  `target_index_follows_priority_order` / `target_index_stable_for_equal_priorities`。

## 代码变更

| 文件 | 变更 |
| --- | --- |
| [`sag-admin/src/admin_active_targets.rs`](../../../../sag-admin/src/admin_active_targets.rs) | 新增私有 helper `priority_ranks(&[RouteTarget]) -> Vec<u32>`：按 `priority` 降序 stable sort（同优先级保持数组顺序），返回按原始数组位置索引的秩（rank 0 = 最高优先级 target），与 `sag-dispatch` 的 `Reverse(priority)` 排序一致；`ActiveTargetDto.target_index` 改填 `ranks[config_index]`；枚举计数器 `target_index` 改名 `config_index`，消除「配置数组位置」与「优先级秩」同名 shadowing（纯重命名，无行为变更） |
| [`sag-ui/src/lib/activeTarget.ts`](../../../../sag-ui/src/lib/activeTarget.ts) | 注释更新：`target_index === 0` 现在指优先级降序秩 0（最高优先级 target），判定逻辑不变 |
| [`sag-ui/src/types/api.ts`](../../../../sag-ui/src/types/api.ts) | `ActiveTargetEntry.target_index` 注释更新为优先级秩语义，字段类型/名称不变 |
| [`sag-ui/src/lib/activeTarget.test.ts`](../../../../sag-ui/src/lib/activeTarget.test.ts) | 注释随语义更新，断言逻辑不变 |
| [`docs/spec/sticky-targets-observability/spec.md`](../../../../docs/spec/sticky-targets-observability/spec.md) | 关键决策 / 不变式 / 测试策略中 `target_index` 语义描述从原始数组索引更新为优先级降序秩 |

## 测试

- `target_index_follows_priority_order`（新增，取代 `target_index_mirrors_route_targets_array_order`）— priorities [10,20,0] 的 route.targets → target_index [1,0,2]（rank 0 = 最高优先级 target），响应数组保持 config 顺序；pin 到最高优先级 target 后仍报 rank 0
- `target_index_stable_for_equal_priorities`（新增）— 全部默认优先级 0 时 stable sort 保持数组顺序，`target_index` 等于数组位置（与旧语义一致）
- `all_route_targets_are_idle_when_active_map_empty` — 补充 target_index 断言（priorities 已降序时秩等于数组位置）
- 纯重命名 + 文档更新，无运行时行为变更
