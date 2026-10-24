---
created: 2026-10-23
updated: 2026-10-25
type: 移除
---

# 0007 — Provider 列表删除 Status 列

## 动机

Provider 列表里「启用状态」出现了两次：Status 列渲染只读的 enabled/disabled Badge，
Actions 列的 Switch 才是真正的开关。两者表达同一状态，而只读 Badge 紧挨开关容易被误当成
可点击的切换控件；状态已由 Switch 本身（及其 `title` / `aria-label`）表达，独立列只是
重复占宽。

## 需求变更

- 「预期行为 → Provider 管理」列表列由 Name、Base URL、Models Endpoint、Type、Keys 数量、
  Status（开关）、Actions 改为 Name、Base URL、Models Endpoint、Type、Keys 数量、Actions。
- 启停能力不变：仍由 Actions 列的 Switch 直接切换并即时生效，需求条目不删除该能力。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-ui/src/components/providers/ProviderList.vue`](../../../../sag-ui/src/components/providers/ProviderList.vue) | 删除 `columnHelper.accessor("enabled", …)` 列定义（Badge 渲染 + `common.status` / `common.enabled` / `common.disabled` 文案引用） |

## 测试

- 无自动化测试（改动为 TanStack 列定义，属渲染层）：以 `just verify-ui`（`fmt-check` +
  `oxlint` + `vue-tsc` + 前端测试 + i18n 检查）作回归门禁
- 人工核对：Providers 页列头不再出现 Status；Actions 列 Switch 仍可切换并在 toast 中
  反映启用/禁用
