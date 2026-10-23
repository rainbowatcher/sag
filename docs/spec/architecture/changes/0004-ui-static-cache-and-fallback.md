---
created: 2026-10-22
updated: 2026-10-23
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
  - 缺失静态资源（`assets/**`、末段扩展名属静态扩展名白名单的子资源探针）返 `404` 且不回 SPA shell。
  - 静态面只接受 `GET` / `HEAD`，其它方法 `405` + `Allow: GET, HEAD`。
  - **未注册**的 `/api/*`、`/v1/*` 路径由 `200` HTML 改为 `404` JSON（`{"error":{"code":"NOT_FOUND",...}}`；已注册路由不经 fallback，行为零变化）。
  - 收尾（2026-10-23）后「子资源探针」的判定从「末段含点」收紧为 24 项静态扩展名白名单：`/providers/v1.2` 这类带点路由回到 shell，`/gone.js` 仍 404。
  - 所有响应（含 `304`）带 `X-Content-Type-Options: nosniff`。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-gateway/src/ui_routes/mod.rs`](../../../../sag-gateway/src/ui_routes/mod.rs) | 兜底判定改为顺序规则：`api/`、`v1/` 前缀先返 404 JSON（`{"error":{"code":"NOT_FOUND",...}}`，与 `AppError::NotFound` 同形）→ 非 `GET`/`HEAD` 返 405 + `Allow: GET, HEAD` → `resolve` 命中即下发 → `assets/` 前缀与「末段扩展名属 `ASSET_EXTENSIONS` 白名单且 `Accept` 不含 `text/html`」返 404 → 其余回 shell（带点的前端路由靠 `Accept: text/html` 或非白名单扩展名兜住）。`Cache-Control` 分两档（`assets/**` → `public, max-age=31536000, immutable`，其余 → `no-cache`），`cache_control_for` 返回 `&'static str`；全部响应（含 `304`）补 `X-Content-Type-Options: nosniff`，404/405 有意不设 `Cache-Control`。`Body::from(file.data)` 走 `Cow<'static, [u8]>` 的 `from_static` 分支，去掉 `into_owned()`。`no-cache` 档补强 `ETag`（取自**实际下发表示**的构建期 sha256，`.br`/`.gz` 与 identity 各自独立），`If-None-Match`（`*`、`W/` 前缀、逗号列表）命中返空 body `304`（`304` 仍带 `nosniff`，并显式设 `Content-Length: 0`）。`UiAssets` 加 `#[exclude = "*.map"]`。收尾把 `looks_like_asset` 从「末段含点」改为 24 项扩展名白名单（见「后续收尾」）。 |
| [`sag-gateway/tests/fixtures/embed/`](../../../../sag-gateway/tests/fixtures/embed/) | 测试专用 fixture `sample.js` + `sample.js.map`，让 `#[exclude = "*.map"]` 的断言非空转 |
| [`Cargo.toml`](../../../../Cargo.toml) | `[profile.release]` 新增 `lto = "thin"` / `codegen-units = 1` / `strip = "symbols"`（收尾，决策 D4） |
| [`sag-ui/vite.config.ts`](../../../../sag-ui/vite.config.ts) | rolldown `codeSplitting.groups` 拆出 `vendor-chart`（chart.js / vue-chartjs，收尾） |
| [`sag-ui/index.html`](../../../../sag-ui/index.html) | 删除 `/vite.svg` 死引用（收尾） |
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

`ui_routes` 的 `#[cfg(all(test, feature = "embed-ui"))]` 测试区由 6 个增至 26 个：新增缓存分档 2、缺失资源 2、API 命名空间 3、带点路由 1、方法限制 2、`nosniff` 1、ETag/304 5（含 br 表示独立 ETag 与零拷贝字节回归）、sourcemap 排除 1；收尾再补 3（白名单收窄 2：`dotted_route_without_html_accept_gets_shell`、`missing_static_file_returns_404`；fixture 非空转 1：`exclude_attribute_drops_sourcemaps`）。`negotiate.rs` 的 12 个协商用例不变。`cargo test -p sag-gateway --features embed-ui` = 49 passed；无 feature = 23 passed。

## 后续收尾（2026-10-23）

首批 6 个 commit 交付后的一批收尾改动，五项（前两项属同一事项：白名单收窄 + 其非空转验证）。

### 1. `looks_like_asset` 收窄为扩展名白名单（**行为变化**）

原判定「末段含 `.`」在 `Accept: */*` 下把带点的客户端路由（`/providers/v1.2`）也判成子资源探针，直接 404 —— 非浏览器客户端访问带点路由拿不到 shell。收尾改为白名单 `ASSET_EXTENSIONS`（24 项：`js`/`mjs`/`cjs`/`css`/`map`/`svg`/`png`/`jpg`/`jpeg`/`gif`/`webp`/`avif`/`ico`/`bmp`/`woff`/`woff2`/`ttf`/`otf`/`eot`/`txt`/`xml`/`wasm`/`br`/`gz`，扩展名大小写不敏感），末段扩展名命中白名单才判为子资源探针。

| 请求 | 收窄前 | 收窄后 |
|---|---|---|
| `GET /providers/v1.2`（`Accept: */*`） | `404` | `200` HTML shell |
| `GET /gone.js`（`Accept: */*`） | `404` | `404`（不变） |
| `GET /__missing__.svg`（`Accept: */*`） | `404` | `404`（不变） |

意图不变（缺失的**子资源**不回 shell），但「什么算子资源」从「末段含点」收紧为「已知静态扩展名」——只列构建真会产出的文件类型。

### 2. sourcemap 排除改为非空转验证

`sag-ui/dist/` 当前不含 `.map`，直接对 `UiAssets` 断言 `#[exclude = "*.map"]` 是空转（属性生效与否都会过）。新增测试专用 `FixtureAssets`（`#[folder = "tests/fixtures/embed/"]` + `#[exclude = "*.map"]`）与 fixture `sample.js` / `sample.js.map`：断言 `sample.js` 在、`sample.js.map` 不在、`iter()` 也不列出 `.map`。fixture 两文件一并提交，断言因此恒为非空转。

### 3. 删除 `/vite.svg` 死引用

`sag-ui/index.html` 的 `<link rel="icon" href="/vite.svg">` 指向从未存在的文件（`sag-ui/public/` 不存在），原状是每次页面加载白付一个 404。收尾直接删掉该行：`index.html` 1845 B → 1783 B（ETag 随之变化），真 favicon 仍属后续任务。

### 4. release profile 体积调优（决策 D4 落地）

根 `Cargo.toml` 新增 `[profile.release] lto = "thin"`、`codegen-units = 1`、`strip = "symbols"`。内嵌 UI 的 `sag` 二进制实测：

| 采样 | 前 | 后 | 变化 |
|---|---|---|---|
| 决策阶段单次采样 | 26.4 MB | 17.4 MB | −34%，构建时长 +31.6% |
| 收尾复测（本机 `target/release/sag`） | 25,696,768 B | 21,290,640 B | −17.1% |

两次采样差距来自 `codegen-units`：本机 `.cargo/config.toml` 的 `[profile.release] codegen-units = 16` 会**覆盖** manifest 里的 `1`（cargo 配置优先级高于 manifest），所以本地构建只吃到 `strip` 的收益（`lto = "thin"` 两边原本就一致）；CI / 干净环境才拿到 cgu=1 的全量收益。代价是 `strip = "symbols"` 让 panic backtrace 不再有符号名。

### 5. `vendor-chart` 分包

`sag-ui/vite.config.ts` 用 rolldown 的 `output.codeSplitting.groups` 把 `chart.js` / `vue-chartjs` 拆成独立 `vendor-chart` chunk（`test` 只匹配这两个包）：

| chunk | 前 | 后 |
|---|---|---|
| `AnalyticsPage-*.js` | 258 kB | 95.52 kB（gzip 28.63 kB） |
| `vendor-chart-*.js` | —（并在上者内） | 162.76 kB（gzip 55.93 kB） |
| 首屏 `modulepreload` | 11 条 | 11 条（不变，`vendor-chart` 不在其中） |

chart.js 只被 `/analytics` 路由的 `V2TrendChart` 使用，拆开后两 chunk 各带 content hash：改页面不再让 162 kB 的图表库缓存失效（`/assets/**` 是 `immutable` 一年）。`includeDependenciesRecursively: false` 必须显式关——默认 `true` 会把 vue runtime 一起拖进该组，入口就静态依赖它，chart.js 反而进首屏 preload。

两条评估后**不做**的偏差：

- `output.manualChunks`：rolldown 中已 deprecated 且不支持对象形式，用它会退回单 chunk 或直接报错，负收益，否决。
- `modulePreload` 收紧：按产物分析，现有 11 条 preload 覆盖的都是入口静态依赖链上的 chunk，收紧只会把请求从 `index.html` 推迟到运行期（串行瀑布），判负收益，不做。

## e2e 冒烟（2026-10-23，`just build-test` + `just test-env start` 后）

对 test-env 实例（`127.0.0.1:37211`、`gateway.test.toml`、`binary_mtime` = 本次构建）用 curl 复核同一组验收点，数值为收尾后产物（`index.html` 删了 favicon 引用、chunk hash 全部变化、二进制变小）：

| 请求 | 状态 | 关键响应头 / body |
|---|---|---|
| `GET /api/unknown` | `404` | `Content-Type: application/json`、`Content-Length: 60`、`X-Content-Type-Options: nosniff`、body `{"error":{"code":"NOT_FOUND","message":"no such API route"}}` |
| `POST /api/unknown` | `404` | 同上（JSON，**无** `Allow`） |
| `GET /v1/unknown` | `404` | `Content-Type: application/json`、`Content-Length: 60` |
| `GET /assets/gone.js` | `404` | `Content-Type: text/plain; charset=utf-8`、`Content-Length: 9`、`X-Content-Type-Options: nosniff`、body `not found`（无 `<!doctype`） |
| `GET /` | `200` | `Cache-Control: no-cache`、`Etag: "0b2f1d71…b7ce4"`、`Vary: Accept-Encoding`、`Content-Length: 1783`、`X-Content-Type-Options: nosniff` |
| `HEAD /` | `200` | `Content-Length: 1783`（与 `GET` 一致，零拷贝未丢长度） |
| `GET /` + `If-None-Match: <identity ETag>` | `304` | `Etag` 回显、`Cache-Control: no-cache`、`Vary: Accept-Encoding`、`X-Content-Type-Options: nosniff`、空 body、**无** `Content-Length` |
| `POST /` | `405` | `Allow: GET, HEAD`、`Content-Length: 0`、`X-Content-Type-Options: nosniff` |
| `GET /` + `Accept-Encoding: br` | `200` | `Content-Encoding: br`、`Content-Length: 550`、`Etag: "21673815…eb52a1"`（与 identity ETag **不等**） |

与 2026-10-22 那轮的差异：`index.html` 1845 → 1783 B（删 favicon 引用）、identity ETag `59eea3b0…13cba` → `0b2f1d71…b7ce4`、br 表示 550 B / ETag `89accccb…ace57b` → `21673815…eb52a1`，状态码与头的集合完全一致。

收尾改动本身的补充验证（同一实例）：

| 请求 | 状态 | 说明 |
|---|---|---|
| `GET /providers/v1.2`（`Accept: */*`） | `200` | HTML shell、`Cache-Control: no-cache`、`Content-Length: 1783` —— 白名单收窄的行为变化 |
| `GET /gone.js`（`Accept: */*`） | `404` | `text/plain`、`Content-Length: 9` —— 白名单内探针仍 404 |
| `GET /assets/vendor-chart-bB_br26w.js` | `200` | `Cache-Control: public, max-age=31536000, immutable`、`Content-Length: 162762` —— 新分包 chunk 走 immutable 档 |
| `GET /vite.svg` | `404` | 引用已删，仍是 404（无真 favicon） |

两点记录：

- 304 上的 `Content-Length: 0` 由 handler 层显式设置，hyper 按 RFC 9110 对 `304` 剥离该头，**线上不可见** —— 单元测试断言的是 handler 层（`hyper-1.12.0/src/proto/h1/role.rs:541-547` 的 `can_have_content_length` 对 `304` 返回 `false`）。设了却没发出去是协议要求，不是 bug。
- 旧实例（`build-info.json` 的 `binary_mtime` 早于本次 4 个 commit）上 `POST /api/unknown` 仍为 `200 text/html`；`just build-test` 后同一请求变 `404 application/json`，印证 e2e 验收确已在新二进制上执行。

## 后续项（不在本次范围）

- 真 favicon 仍缺：收尾只删掉了 `/vite.svg` 死引用，补一个真图标另开任务。
- 删除内嵌 `.gz` 为决策项 D5，保留现状。
- 决策 D4 的 `codegen-units = 1` 全量收益只在 CI / 干净环境生效，本地 `.cargo/config.toml` 的 `codegen-units = 16` 仍覆盖它。
