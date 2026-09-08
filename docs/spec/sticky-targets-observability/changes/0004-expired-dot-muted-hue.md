---
created: 2026-09-08
type: 修改
---

# 0004 — expired 圆点改 muted 灰绿/灰橙

## 动机

0003 把 sticky `expired` 统一渲染为 `bg-gray-500`（灰 = 无效态）。问题有二：

- 「无法确认的 target」：`isActiveFirstTarget` 无法证明 pin 落在 route.targets 首项时保守返回
  `false`，但 0003 之后 expired 分支在首项判定**之前**短路为统一灰，用户无法从颜色看出这个
  expired pin 的配置位置（首项 vs fallback）；
- 统一灰丢失了「配置位置」信息：expired 只表示 pin 超过 `sticky_failover_reset_secs` 未更新，
  不等同于当前实际承载流量，但用户仍需要知道它原本 pin 在哪个 target。

本次把 expired 改为 muted 的配置位置色：命中首项灰绿（`bg-green-500/40`）、非首项/无法确认
灰橙（`bg-orange-500/40`）。纯前端局部 UI 调整，不改后端 `GET|DELETE /api/active-targets`
的 API 契约与 DTO 字段。

## 需求变更

- 本模块无 `requirements.md`（只有 spec.md），按要求跳过需求文档更新，不新建空文件。
- 0003 中「expired 恒 gray」的描述被本节取代：expired 判定发生在首项判定之后（先
  `isActiveFirstTarget` 再按状态取色），fill_first 恒 emerald 的优先级不变。

## 代码变更

| 文件 | 变更 |
| --- | --- |
| [`sag-ui/src/lib/activeTarget.ts`](../../../../sag-ui/src/lib/activeTarget.ts) | `activeTargetDotColor`：移除 expired 统一 `bg-gray-500` 的先行短路；改为先算 `isActiveFirstTarget`，expired 时首项 → `bg-green-500/40`、非首项/无法确认 → `bg-orange-500/40`，pinned 保持 green/orange，fill_first 恒 emerald 不变 |
| [`sag-ui/src/lib/activeTarget.test.ts`](../../../../sag-ui/src/lib/activeTarget.test.ts) | 更新 expired 用例：expired 首项 → 灰绿、expired 非首项 → 灰橙、expired 无法确认（无 route entry / 空 targets）→ 灰橙；用例注释说明 expired 表示过期 pin 的配置位置而非当前实际承载流量 |

## 测试

- expired 首项：`isActiveFirstTarget` 仍为 `true`（身份判定与过期无关），圆点 `bg-green-500/40`
- expired 非首项（第二个 config target 的 pin）：`bg-orange-500/40`
- expired 无法确认：无 route entry（`entries` 里没有该 model 的 DTO）→ `bg-orange-500/40`；
  空 targets 路由的 expired pin → `bg-orange-500/40`（保守方向保持：unverifiable → 灰橙，
  never 灰绿）
- pinned 首项/非首项、fill_first 恒 emerald、对象身份判别、多 route 按 model 作用域等既有
  用例不变，仍全绿

## 注意

- **保守判定不变**：无法确认时永远不显示首项绿（expired 场景下即不显示灰绿），显示灰橙。
- **expired ≠ 当前流量**：expired 只表示 pin 已过期（配置位置），不代表该 target 当前在吸收
  流量；灰绿/灰橙的 muted 色只表达「过期 + 配置位置」两维信息。
- 验证：`sag-ui` 下 `node --experimental-strip-types src/lib/activeTarget.test.ts` 断言全过；
  `pnpm exec oxlint src/` / `pnpm typecheck` / `npx oxfmt --check` 通过。未运行 full-project
  build。
