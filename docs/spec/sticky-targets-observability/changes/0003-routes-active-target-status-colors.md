---
created: 2026-09-07
type: 修改
---

# 0003 — Routes 页 Active Target 状态圆点颜色

> **已被 0004/0005 部分取代**：0004 把 expired 从统一 `bg-gray-500` 改为按配置位置 muted（首项
> `bg-green-500/40`、非首项/无法确认 `bg-orange-500/40`）；0005 把「对象身份首项判定」改为后端
> `ActiveTargetDto.target_index === 0` 权威判定。本节「expired 改灰」「对象身份首项判定」的描述
> 仅代表 0003 当时的决定，当前行为以 [0004](0004-expired-dot-muted-hue.md)、
> [0005](0005-target-index-and-strategy-switch-cleanup.md) 与 spec.md 为准；pinned 首项绿/非首项
> 橙、FillFirst 恒 emerald 仍按本节不变。

## 动机

Routes 页 Active Target 列的圆形状态点此前只有三态：FillFirst 恒 emerald、
sticky `expired` 用 amber、sticky `pinned` 恒 green。两个问题：

- `expired` 的 amber 与 `pinned` 的 green 都是「暖色点亮」，视觉区分度低，用户难以
  一眼分清「pin 已过期」与「pin 生效中」；
- 颜色无法反映 pin 落在 route 的哪个 target——pinned 命中 route 原始 targets 顺序
  首项（首选目标正常吸收流量）与命中非首项（已在 fallback 上）观感上应有差异。

本次统一着色语义：`expired` 改灰（无效态）、pinned 命中首项绿、pinned 命中非首项橙、
FillFirst 保持 emerald。同时把原本内联在 `RouteList.vue` 的 active-target helper 抽到
`sag-ui/src/lib/activeTarget.ts`，新增 `isActiveFirstTarget`（对象身份保守判定首 target）
与 `activeTargetDotColor`（统一着色），并配套可独立运行的前端测试文件。

## 需求变更

- 本模块无 `requirements.md`（只有 spec.md），按要求跳过需求文档更新，不新建空文件。
- 本变更只调整前端状态圆点的视觉语义与渲染逻辑，不改变后端 `GET|DELETE /api/active-targets`
  的 API 契约与 DTO 字段。着色依赖后端现有行为：按 `route.targets` 原顺序每 target 一个
  DTO，pin 记在其 pin 来源 target 的 DTO 上。

## 代码变更

| 文件 | 变更 |
| --- | --- |
| [`sag-ui/src/lib/activeTarget.ts`](../../../../sag-ui/src/lib/activeTarget.ts) | 新增：`selectActiveTarget` / `selectFillFirstTarget` 自 `RouteList.vue` 内联 helper 迁移；新增 `isActiveFirstTarget`（按对象身份保守判定 pin 是否命中 route.targets 原顺序首项）与 `activeTargetDotColor`（统一状态圆点颜色语义） |
| [`sag-ui/src/lib/activeTarget.test.ts`](../../../../sag-ui/src/lib/activeTarget.test.ts) | 新增：边界测试（exact/wildcard passthrough、空 targets、expired/fill 优先级、多 route entries 交叉、对象身份 vs 字段相等、wildcard 多 pin 取最新） |
| [`sag-ui/src/components/routes/RouteList.vue`](../../../../sag-ui/src/components/routes/RouteList.vue) | 删除内联 `activeTargetFor` / `fillFirstTargetFor`，改用 lib；圆点颜色由内联三元改调 `activeTargetDotColor`（expired 灰、首项绿、非首项橙、FillFirst 保持 emerald） |
| [`sag-ui/tsconfig.json`](../../../../sag-ui/tsconfig.json) | 新增 `allowImportingTsExtensions` 与 `noEmit`，允许测试文件直接 import `.ts` 扩展名并交由 node strip-types 运行 |
| [`sag-ui/package.json`](../../../../sag-ui/package.json) | 新增 `test` / `test:active-target` script（`node --experimental-strip-types` 运行 activeTarget 测试） |

## 测试

- exact route：passthrough 首项 + 同 resolved model 的显式 fallback——pin 落 fallback 报橙、
  落 passthrough 首项报绿（二者 real_model 相同，字段比较无法区分，靠对象身份判别）
- wildcard route：同 provider 的显式 fallback 报橙；passthrough 首项被具体请求模型 pin 报绿
- 空 targets：`isActiveFirstTarget` 恒 `false`（空列表不可能有首项）；无 active 时不渲染圆点（`null`）
- expired 优先于绿/橙：即使过期条目本身是首 target 也渲染灰；非首 target 的 expired 同样灰
- FillFirst 保持 emerald，且与 sticky pin 共存时 sticky 优先（颜色跟随 sticky 条目）
- 对象身份而非字段相等：两个字节相同数据的 DTO，仅第一个（config 首 target 的 entry）算首项
- 多 route entries 交叉：按 `model` 作用域，field-identical 但 `model` 不同的 pin 不互相串扰
- wildcard 多 pin：按 `age_secs` 取最新为 active，与数组顺序无关

## 注意

- **保守判定**：`isActiveFirstTarget` 无法确认 pin 落在首 target 时返回 `false`，圆点渲染
  橙色而非绿色（unverifiable → orange, never green）。
- **代码审查结论**：零 Blocker/Major；仅一个 Minor（非阻塞）——package `test` script 使用
  `npm run` 而非直接写命令，属风格问题，不影响功能。
- 验证：`sag-ui` 下 `npm run test:active-target`（`node --experimental-strip-types`）断言全过；
  `pnpm typecheck` / `pnpm lint` 通过。未运行 full-project build。
