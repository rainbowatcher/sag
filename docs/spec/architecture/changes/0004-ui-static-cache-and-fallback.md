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

- `architecture/requirements.md`「响应传输与压缩」的内嵌 UI 静态资源条目已同步更新：
  - `Cache-Control` 分两档——hash 化资产（`assets/**`）`public, max-age=31536000, immutable`；`index.html` 等根级文件 `no-cache` + 构建期 sha256 强 `ETag`（按实际下发表示取值），`If-None-Match` 命中返空 body `304`。
  - 缺失静态资源（`assets/**`、末段带点的子资源探针）返 `404` 且不回 SPA shell。
  - 静态面只接受 `GET` / `HEAD`，其它方法 `405` + `Allow: GET, HEAD`。
  - **未注册**的 `/api/*`、`/v1/*` 路径由 `200` HTML 改为 `404` JSON（`{"error":{"code":"NOT_FOUND",...}}`；已注册路由不经 fallback，行为零变化）。
  - 所有响应（含 `304`）带 `X-Content-Type-Options: nosniff`。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-gateway/src/ui_routes/mod.rs`](../../../../sag-gateway/src/ui_routes/mod.rs) | 兜底判定改为顺序规则：`api/`、`v1/` 前缀先返 404 JSON（`{"error":{"code":"NOT_FOUND",...}}`，与 `AppError::NotFound` 同形）→ 非 `GET`/`HEAD` 返 405 + `Allow: GET, HEAD` → `resolve` 命中即下发 → `assets/` 前缀与「末段带点且 `Accept` 不含 `text/html`」返 404 → 其余回 shell（带点的前端路由靠 `Accept: text/html` 兜住）。`Cache-Control` 分两档（`assets/**` → `public, max-age=31536000, immutable`，其余 → `no-cache`），`cache_control_for` 返回 `&'static str`；全部响应（含 `304`）补 `X-Content-Type-Options: nosniff`，404/405 有意不设 `Cache-Control`。`Body::from(file.data)` 走 `Cow<'static, [u8]>` 的 `from_static` 分支，去掉 `into_owned()`。`no-cache` 档补强 `ETag`（取自**实际下发表示**的构建期 sha256，`.br`/`.gz` 与 identity 各自独立），`If-None-Match`（`*`、`W/` 前缀、逗号列表）命中返空 body `304`（`304` 仍带 `nosniff`，并显式设 `Content-Length: 0`）。`UiAssets` 加 `#[exclude = "*.map"]`。 |
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

## e2e 冒烟（2026-10-22，`just build-test` 后）

`just build-test && just test-env restart` 后对 test-env 实例（`127.0.0.1:37211`、`gateway.test.toml`）用 curl 复核计划 §7 的验收点：

| 请求 | 状态 | 关键响应头 / body |
|---|---|---|
| `GET /api/unknown` | `404` | `Content-Type: application/json`、`X-Content-Type-Options: nosniff`、body `{"error":{"code":"NOT_FOUND","message":"no such API route"}}` |
| `POST /api/unknown` | `404` | 同上（JSON，**无** `Allow`） |
| `GET /v1/unknown` | `404` | `Content-Type: application/json` |
| `GET /assets/gone.js` | `404` | `Content-Type: text/plain; charset=utf-8`、`X-Content-Type-Options: nosniff`、body `not found`（无 `<!doctype`） |
| `GET /` | `200` | `Cache-Control: no-cache`、`Etag: "59eea3b0…13cba"`、`Vary: Accept-Encoding`、`Content-Length: 1845`、`X-Content-Type-Options: nosniff` |
| `HEAD /` | `200` | `Content-Length: 1845`（与 `GET` 一致，零拷贝未丢长度） |
| `GET /` + `If-None-Match: <identity ETag>` | `304` | `Etag` 回显、`Cache-Control: no-cache`、`Vary: Accept-Encoding`、`X-Content-Type-Options: nosniff`、空 body |
| `POST /` | `405` | `Allow: GET, HEAD`、`X-Content-Type-Options: nosniff` |
| `GET /` + `Accept-Encoding: br` | `200` | `Content-Encoding: br`、`Etag: "89accccb…ace57b"`（与 identity ETag **不等**） |

两点记录：

- 304 上的 `Content-Length: 0` 由 handler 显式设置（单元测试在 router 层断言），但 hyper 的 `can_have_content_length` 对 `304` 返回 `false`（`hyper-1.12.0/src/proto/h1/role.rs:541-547`），所以**线上不会出现**该头 —— 这是协议层剥离，不是回归。
- 旧实例（`build-info.json` 的 `binary_mtime` 早于本次 4 个 commit）上 `POST /api/unknown` 仍为 `200 text/html`；`just build-test` 后同一请求变 `404 application/json`，印证 e2e 验收确已在新二进制上执行。

## 后续项（不在本次范围）

- `sag-ui/index.html` 引用的 `/vite.svg` 从未存在（`sag-ui/public/` 不存在）：本次把它从「200 HTML」变成诚实的 404，补真 favicon 另开任务。
- release profile（LTO / `strip`）与删除内嵌 `.gz` 为决策项 D4 / D5，分别另开 PR 与保留现状。
