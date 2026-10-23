---
created: 2026-10-23
updated: 2026-10-23
type: 优化
---

# 0005 — 构建侧收尾：`vendor-chart` 分包与删除 favicon 死引用

## 动机

两项构建产物层面的收尾，均来自「内嵌 UI 静态服务」批次（[架构 0004](../../architecture/changes/0004-ui-static-cache-and-fallback.md)）的实测：

1. **图表库拖累路由 chunk**：`AnalyticsPage` 一个 chunk 258 kB，其中 162 kB 是 chart.js / vue-chartjs。而 `/assets/**` 在内嵌服务里是 `immutable` 一年 —— 页面代码一改，整块 hash 变化，163 kB 的图表库跟着失效重下。
2. **favicon 死引用**：`index.html` 里 `<link rel="icon" href="/vite.svg">` 指向从未存在的文件（`sag-ui/public/` 不存在），每次页面加载白付一个 404。

## 需求变更

- `ui/requirements.md` 技术栈表的「构建」行补注：`chart.js` 单独分包、随 `/analytics` 路由懒加载，不进首屏 preload；页面外壳不再引用不存在的静态资源。
- 无行为/交互契约变化（不影响任何页面功能）。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-ui/vite.config.ts`](../../../../sag-ui/vite.config.ts) | `build.rolldownOptions.output.codeSplitting.groups` 新增 `vendor-chart` 组（`test` 只匹配 `node_modules` 下的 `chart.js` / `vue-chartjs`），并显式设 `includeDependenciesRecursively: false` |
| [`sag-ui/index.html`](../../../../sag-ui/index.html) | 删除 `<link rel="icon" type="image/svg+xml" href="/vite.svg">` 一行 |

## 效果（`pnpm -C sag-ui build` 实测）

| chunk | 前 | 后 |
|---|---|---|
| `AnalyticsPage-*.js` | 258 kB | 95.52 kB（gzip 28.63 kB） |
| `vendor-chart-*.js` | —（并在上者内） | 162.76 kB（gzip 55.93 kB） |
| `index.html` | 1845 B | 1783 B |
| 首屏 `modulepreload` | 11 条 | 11 条（`vendor-chart` 不在其中） |

两 chunk 各带 content hash：改 `AnalyticsPage` 不再让 163 kB 的图表库缓存失效。

## 设计取舍

- **`includeDependenciesRecursively: false` 必须显式关**：默认 `true` 会把 vue runtime 一起拖进该组，入口因此静态依赖它，chart.js 反而进首屏 preload —— 分包目标（懒加载）当场失效。
- **不用 `output.manualChunks`**：rolldown 中已 deprecated 且不支持对象形式，回退过去要么退回单 chunk 要么直接报错，负收益，否决。
- **不收紧 `modulePreload`**：按产物分析，现有 11 条 preload 覆盖的都是入口静态依赖链上的 chunk；收紧只会把请求从 `index.html` 推迟到运行期（串行瀑布），判负收益，不做。
- **不补真 favicon**：删除死引用只解决「白付 404」，真图标（含 `apple-touch-icon` 等）属独立任务，避免顺手引入未设计过的品牌资源。

## 测试

无新增自动化测试：构建产物变化由 `pnpm -C sag-ui build` 的 chunk 报告与 `just build-test` 后 `dist/index.html` 的 `modulepreload` 列表人工核对；页面功能回归靠既有单元测试（`pnpm -C sag-ui test`）与 test-env 手动验证（`/analytics` 图表仍渲染、`GET /vite.svg` 为 404 且页面不再请求它）。
