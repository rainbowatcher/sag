---
created: 2026-10-22
updated: 2026-10-22
type: 优化
---

# 0004 — 内嵌 UI 静态服务：缓存分档、兜底收窄、零拷贝与 ETag

## 动机

构建期预压缩（[0003](0003-http-response-compression.md)）已落地，但「怎么发」还有四个洞，均来自实测（test-env `127.0.0.1:37211`）：

1. **hash 化资产没有任何 `Cache-Control`**：`/assets/index-*.js` 响应既无 `Cache-Control` 也无 `Last-Modified`，浏览器连启发式新鲜度都无从推导，每次页面加载都会重下全部 chunk。
2. **缺失资源回 SPA shell**：`/assets/gone-XYZ.js` 返回 `200 text/html`，拼错的 chunk 名被当成前端路由。
3. **静态面接受任意方法**：`POST /` 返回 `200` shell；未注册的 `GET/POST /api/unknown` 同样拿到 HTML shell —— API 客户端收到 HTML，`POST` 还会被误判。
4. **每次请求一次全量 memcpy**：`Body::from(file.data.into_owned())` 把 release 下本可零拷贝的 `Cow::Borrowed` 复制成 `Vec<u8>`；`index.html` 也缺少 validator，无法换成 304。

## 需求变更

- `architecture/requirements.md`「响应传输与压缩」：hash 化资产一年 `immutable`，`index.html` 每次重校验（可 304）；缺失静态资源 404，静态面只接受 `GET`/`HEAD`；**未注册**的 `/api/*`、`/v1/*` 路径由 `200` HTML 改为 `404` JSON（已注册路由不经 fallback，行为零变化）。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-gateway/src/ui_routes/mod.rs`](../../../../sag-gateway/src/ui_routes/mod.rs) | 兜底判定改为顺序规则：`api/`、`v1/` 前缀先返 404 JSON（`{"error":{"code":"NOT_FOUND",...}}`，与 `AppError::NotFound` 同形）→ 非 `GET`/`HEAD` 返 405 + `Allow: GET, HEAD` → `resolve` 命中即下发 → `assets/` 前缀与「末段带点且 `Accept` 不含 `text/html`」返 404 → 其余回 shell（带点的前端路由靠 `Accept: text/html` 兜住）。`Cache-Control` 分两档（`assets/**` → `public, max-age=31536000, immutable`，其余 → `no-cache`），`cache_control_for` 返回 `&'static str`；全部响应补 `X-Content-Type-Options: nosniff`，404/405 有意不设 `Cache-Control`。`Body::from(file.data)` 走 `Cow<'static, [u8]>` 的 `from_static` 分支，去掉 `into_owned()`。`no-cache` 档补强 `ETag`（取自**实际下发表示**的构建期 sha256，`.br`/`.gz` 与 identity 各自独立），`If-None-Match`（`*`、`W/` 前缀、逗号列表）命中返空 body `304`。`UiAssets` 加 `#[exclude = "*.map"]`。 |
| [`sag-gateway/Cargo.toml`](../../../../sag-gateway/Cargo.toml) | `embed-ui` 追加可选依赖 `serde_json`（构造 404 JSON），`rust-embed` 开启 `include-exclude`（`#[exclude]` 所需） |
| [`docs/spec/architecture/spec.md`](../spec.md) | 不变式补缓存分档与 ETag/404 语义 |
| [`docs/guide/deployment.md`](../../../guide/deployment.md) | 部署章节补浏览器侧效果（hash 资产一年 `immutable`，`index.html` 重校验） |

## 行为变更（有意）

| 请求 | 变更前 | 变更后 |
|---|---|---|
| `GET /assets/index-*.js` | 200，无 `Cache-Control` | 200，`public, max-age=31536000, immutable` |
| `GET /assets/gone.js` | 200 HTML shell | 404 `text/plain` |
| `GET /__missing__.svg` | 200 HTML shell | 404 |
| `POST /` | 200 HTML shell | 405 + `Allow: GET, HEAD` |
| `GET /api/unknown`、`GET /v1/unknown` | 200 HTML shell | 404 JSON（`NOT_FOUND`） |
| `POST /api/unknown` | 200 HTML shell | 404 JSON（**不是** 405，无 `Allow`） |
| `GET /`（带 `If-None-Match`） | 200 + 571 B body | 304，空 body |
| `HEAD /`、`HEAD /assets/*` | 200 + 正确 `Content-Length` | 不变（hyper 剥离 body） |

## 不变式

- `assets/**` 是唯一 `immutable` 档：规则按前缀而非「除 index.html 外」，避免未来根级无 hash 文件（`favicon.svg`、`robots.txt`）被钉死一年。
- ETag 必须取自**实际下发表示**（identity 或 `.br`/`.gz` sibling），否则 `If-None-Match` 会让 br 缓存表示与 identity 字节错配。
- 静态面的 405 只作用于非 API 命名空间：API 命名空间的未知路径永远先拿 404。
- 路径穿越结构性不可能：查找只走 `UiAssets::get`（release 为编译期静态表），禁止在 handler 内手搓 `fs::read`。

## 测试

`ui_routes` 的 `#[cfg(all(test, feature = "embed-ui"))]` 测试区由 6 个增至 23 个：新增缓存分档 2、缺失资源 2、API 命名空间 3、带点路由 1、方法限制 2、`nosniff` 1、ETag/304 5（含 br 表示独立 ETag 与零拷贝字节回归）、sourcemap 排除 1；`negotiate.rs` 的 12 个协商用例不变。

## 后续项（不在本次范围）

- `sag-ui/index.html` 引用的 `/vite.svg` 从未存在（`sag-ui/public/` 不存在）：本次把它从「200 HTML」变成诚实的 404，补真 favicon 另开任务。
- release profile（LTO / `strip`）与删除内嵌 `.gz` 为决策项 D4 / D5，分别另开 PR 与保留现状。
