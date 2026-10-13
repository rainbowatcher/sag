---
created: 2026-10-06
updated: 2026-10-06
type: 修改
---

# 0003 — 移动端弹框统一为底部抽屉（useResponsiveModal）

## 动机

`sag-ui` 的模态窗口此前是两套写法混用：`ProviderAddForm` / `ProviderEditForm` 各自内联了一份「Dialog on desktop / Drawer on mobile」的组件映射（断点 640px），而 `RoutesPage`、`RouteList`、`ModelManager` 仍是纯 `Dialog`。

手机（≤768px）上纯 Dialog 的问题：卡片居中、宽度受 `max-w-[calc(100%-2rem)]` 限制且被 `[50%]` 定位压扁，长表单在软键盘弹出后可视区域只剩一半，关闭按钮只有约 16px 命中区。底部抽屉（vaul）全宽、可拖拽下滑关闭、内容区可滚动，是移动端表单/确认的正确形态。

此外两处内联映射逐字重复，任何断点或组件替换都要改 2 份；本次迁移会让重复份数涨到 6。

## 需求变更

- requirements.md：「Provider 管理」中「编辑：`ProviderEditForm` drawer，使用 `useMediaQuery` 在移动端切换为 dialog」表述与实现相反（实现是桌面 Dialog / 移动 Drawer），已改正并补齐断点值
- requirements.md：「Route 管理」中编辑与删除由「Dialog 弹出」改为「Dialog（≥768px）/ 底部抽屉（<768px）」
- requirements.md：新增「移动端弹框形态」小节，定义 md（768px）断点、哪些界面属于弹框、以及 Popover 类浮层不在本次范围内
- requirements.md：「包含」新增「移动端（<768px）弹框以底部抽屉呈现」；「不包含」移除「移动端侧边栏折叠 / 汉堡菜单」——`AppLayout.vue` 已通过 Sheet 实现移动端侧边栏，该条与现状不符

## 代码变更

| 文件 | 变更 |
|------|------|
| [sag-ui/src/composables/useResponsiveModal.ts](../../../../sag-ui/src/composables/useResponsiveModal.ts) | 新增：导出 `useResponsiveModal()`（返回 `isDesktop` + `Modal` 组件映射）与 `MODAL_DESKTOP_QUERY = "(min-width: 768px)"`；`Modal` 为 8 个槽位（Root/Content/Header/Title/Description/Footer/Close/Trigger）的 `ComputedRef`，桌面映射到 dialog 原语、移动端映射到 drawer 原语 |
| [sag-ui/src/components/providers/ProviderAddForm.vue](../../../../sag-ui/src/components/providers/ProviderAddForm.vue) | 删除内联的 `useMediaQuery` + `Modal` 映射与 dialog/drawer 导入，改用 `useResponsiveModal()`；断点由 640px 变为 768px |
| [sag-ui/src/components/providers/ProviderEditForm.vue](../../../../sag-ui/src/components/providers/ProviderEditForm.vue) | 同上 |
| [sag-ui/src/pages/RoutesPage.vue](../../../../sag-ui/src/pages/RoutesPage.vue) | New Route 弹框改为 `Modal` 双轨；打开态回调提取为 `onAddOpenChange(v: boolean)`；`Modal.Content` 增加 `px-4 sm:px-6`（DrawerContent 无默认内边距），`RouteAddForm` 仍作为 `flex-1` 直接子元素自带滚动区与底部按钮 |
| [sag-ui/src/components/routes/RouteList.vue](../../../../sag-ui/src/components/routes/RouteList.vue) | 三处弹框（Clear Active Target / Reset Fill-First 确认、删除确认、Edit Route）改为 `Modal` 双轨；新增 `onConfirmOpenChange` / `onDeleteOpenChange` / `onEditOpenChange` 三个具名回调替代内联箭头函数；`deleteDialogOpen` 重命名为 `deleteConfirmOpen`；Edit Route 由「整块内容滚动」改为「header 固定 + 内容区滚动」；确认弹框 footer 去掉 `flex flex-row justify-end gap-2 sm:justify-end` 覆盖，改用 `DialogFooter`/`DrawerFooter` 默认（移动端纵向堆叠、桌面端横向右对齐） |
| [sag-ui/src/components/ModelManager.vue](../../../../sag-ui/src/components/ModelManager.vue) | Quick Create Route 确认弹框改为 `Modal` 双轨；新增 `onRouteCreateOpenChange`；表单区改为 `px-4 py-2 sm:px-0`，在抽屉内提供左右内边距、在对话框内不叠加 `DialogContent` 的 `p-6` |

`sag-ui/src/components/ui/**` 未改动（shadcn-vue CLI 生成物，按该目录 `AGENTS.md` 硬规则只读）。

## 测试

无自动化前端测试（requirements.md 已标注「不包含：端到端测试」）。本次验证：

- `pnpm typecheck`（vue-tsc）：通过，无错误
- `pnpm lint`（oxlint）：通过，0 warnings 0 errors
- `pnpm build`（Vite production）：通过
- 浏览器实测（Vite dev server + `agent-browser`，视口 390px / 700px / 900px，连接 37211 测试实例）：

  | 界面 | 390px（md=false） | 700px（md=false） | 900px（md=true） |
  |------|------|------|------|
  | New Route | drawer（bottom） | drawer | dialog |
  | Edit Route | drawer | drawer | dialog |
  | Delete Route 确认 | drawer | drawer | dialog |
  | Add Provider | drawer | drawer | dialog |
  | Edit Provider | drawer | drawer | dialog |
  | Quick Create Route（Provider 弹框内） | drawer ×2（嵌套） | — | dialog ×2（嵌套） |

  探针为 `document.querySelectorAll('[data-slot=drawer-content]')` / `[data-slot=dialog-content]` 计数，并同时校验 `window.innerWidth` 与 `matchMedia('(min-width: 768px)')`，避免视口与页面状态不一致导致误判。
