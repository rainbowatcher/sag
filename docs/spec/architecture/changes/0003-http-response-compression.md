---
created: 2026-10-18
updated: 2026-10-18
type: 新增
---

# 0003 — HTTP 响应压缩：SSE 防护头、UI 构建期预压缩、动态压缩层

## 动机

网关此前对响应体不做任何压缩，三个面各有缺口：

1. **SSE 流可能被中间层缓冲**：`text/event-stream` 只带 `Cache-Control: no-cache`，nginx 的 `proxy_buffering` 与 Cloudflare 之类的 CDN 仍可能缓冲或二次压缩流，表现为 token 一次性倾倒而非逐块到达。
2. **内嵌 UI 静态资源裸发**：`rust-embed` 直接下发 `sag-ui/dist/` 原始文件，几百 KB 的 JS/CSS 在弱网下首屏明显偏慢，而运行期压缩又会在热路径上付出 CPU 与延迟。
3. **API JSON 响应不压缩**：`/api/*`（日志列表、统计）与 `/v1/chat/completions` 的非流式响应动辄几十 KB，未压缩直接上线。

三者都在「响应如何离开进程」这一层，因此放在架构模块统一记录。

## 需求变更

- `architecture/requirements.md`「请求生命周期」新增「响应传输与压缩」小节：SSE 必须带防缓冲头；内嵌 UI 静态资源按 `Accept-Encoding` 下发构建期预压缩件；API 响应在 ≥ 1 KiB（含 1024 字节整）且非 SSE / gRPC / 图片时动态压缩。
- `architecture/requirements.md`「功能边界 → 包含」补充压缩为默认开启行为，不提供配置开关。
- `architecture/spec.md` 新增「关键决策：三层压缩」与三条不变式（压缩层必须在 `http_log` 之外、UI fallback 不经过中间件栈、预压缩件不得被直接访问）。

## 代码变更

| 文件 | 变更 |
|------|------|
| [`sag-dispatch/src/dispatch/stream.rs`](../../../../sag-dispatch/src/dispatch/stream.rs) | SSE 响应头改为 `Cache-Control: no-cache, no-transform` 并新增 `X-Accel-Buffering: no`，阻止中间代理缓冲或转码事件流 |
| [`sag-ui/scripts/postbuild-compress.mjs`](../../../../sag-ui/scripts/postbuild-compress.mjs) | 新增构建期预压缩脚本：仅用 `node:zlib`，为文本产物生成 `.br`（brotli q11）与 `.gz`（gzip 9）副本，跳过 `.map` 与已压缩格式，收益 < 2% 不落盘，`dist/` 相对脚本自身解析 |
| [`sag-ui/package.json`](../../../../sag-ui/package.json) | `build` 改为 `vite build && node scripts/postbuild-compress.mjs`，三条构建链（`just build-test` / release 工作流 / Dockerfile）自动生效 |
| [`sag-gateway/src/ui_routes/mod.rs`](../../../../sag-gateway/src/ui_routes/mod.rs) | 原 `sag-gateway/src/ui_routes.rs` 转为目录模块：`ui_handler` 按协商结果取预压缩副本，命中时发 `Content-Encoding` + `Vary: Accept-Encoding`，`index.html` 加 `Cache-Control: no-cache`，`*.br` / `*.gz` 直取返回 404，无副本时裸发兜底 |
| [`sag-gateway/src/ui_routes/negotiate.rs`](../../../../sag-gateway/src/ui_routes/negotiate.rs) | 新增无依赖的 `Accept-Encoding` 协商纯函数：`q` 值优先、`br > gzip > identity`、`*` 仅补未显式列出的编码、畸形 token 跳过；模块本身不受 `embed-ui` 门控，单元测试无需特性 |
| [`sag-gateway/src/main.rs`](../../../../sag-gateway/src/main.rs) | `mod ui_routes;` 改为无条件编译（`embed-ui` 门控下沉到模块内的 handler / `rust-embed` 部分），使协商逻辑的测试可在默认特性下运行 |
| [`sag-router/src/routes.rs`](../../../../sag-router/src/routes.rs) | 在 `http_log` 之后、`trace` 之前插入 `CompressionLayer`：`CompressionLevel::Fastest`，predicate 为 `SizeAbove(1024) ∧ ¬SSE ∧ ¬GRPC ∧ ¬IMAGES` |
| [`sag-router/tests/compression.rs`](../../../../sag-router/tests/compression.rs) | 新增动态压缩集成测试（8 个场景，覆盖 gzip / br 解码回原文、单一编码选择、无 `Accept-Encoding`、低于 1 KiB 阈值、SSE 豁免、非流式 chat 压缩、`identity;q=0` 的 406） |
| [`Cargo.toml`](../../../../Cargo.toml) | `tower-http` 增加 `compression-gzip` / `compression-br` features |
| [`sag-router/Cargo.toml`](../../../../sag-router/Cargo.toml) | `dev-dependencies` 增加 `flate2`，供集成测试解压校验 |
| [`justfile`](../../../../justfile) | `clippy` / `test-rust` 各追加一条 `-p sag-gateway --features embed-ui`，覆盖此前不被 lint / test 编译的 `embed-ui` 门控代码 |
| [`docs/guide/deployment.md`](../../../../docs/guide/deployment.md) | 单二进制构建一节补「构建自带预压缩」说明（产物、跳过规则、`Vary`、体积增量、零运行期开销） |

## 设计要点

### 三层压缩，各管一段

| 层 | 覆盖对象 | 时机 | 代价 |
|------|------|------|------|
| SSE 防护头 | 所有 `text/event-stream` 响应 | 运行期（仅设头） | 零 |
| UI 预压缩 | `sag-ui/dist/` 下的文本产物 | 构建期（brotli q11 / gzip 9） | 二进制体积 ≈ 713 KiB（当前 53 个 `.br` + 51 个 `.gz` 实测 730,544 B） |
| 动态压缩层 | ≥ 1 KiB（含 1024 字节整）的 API 响应 | 运行期（`Fastest`） | 每请求一次快速编码 |

内嵌 UI 与 API 响应走不同路径，不是实现分裂而是中间件挂载顺序的必然结果：`build_router` 的 `.layer()` 只对当时已注册的路由生效，而 `main.rs` 在 `build_router` 返回后才 `.fallback(ui_handler)`，所以 UI fallback 永远不经过压缩层、CORS、body limit 与 `log_http`。因此 UI 侧的 `Content-Encoding` / `Vary` / `Cache-Control` 必须由 handler 自己设置。

### 压缩层与 `http_log` 的顺序

`CompressionLayer` 注册在 `http_log` 之后（= 在更外层）。`log_http` 会缓冲响应体以记录明文预览与 `content-length`；若压缩层在它内侧，日志会记录压缩后的乱码，且 `content-length` 会被 tower-http 移除。这条顺序是有意为之，改动中间件顺序时须重新核对。

### predicate 为何不用默认值

tower-http 的 `DefaultPredicate` 尺寸下限是 32 字节，对网关响应太低；1 KiB 是压缩开始划算的拐点。同时 `SizeAbove` 把「尺寸未知」视为「可压缩」，而 SSE 响应正是尺寸未知，所以必须显式排除 SSE（并沿用默认的 gRPC / 图片排除）。级别取 `Fastest` 而非 `Default`：网关是延迟敏感的热路径，压缩比的边际收益不如 CPU 时间。

### 已知边界

- `Accept-Encoding: identity;q=0` 时 tower-http 返回 **406**（无可用编码可满足），这是 tower-http 的行为，不做额外兜底。**该 406 对 SSE 路径同样生效**：它发生在 predicate 判定之前，`NotForContentType::SSE` 拦不住。
- 预压缩脚本只处理文本扩展名白名单；新增资产类型（如 `.txt` / `.wasm`）需要同步扩白名单。
- `*.br` / `*.gz` 不可作为独立资源访问（404），否则浏览器会把 `foo.js.br` 当成独立文件。

## 测试

- `sag-dispatch/src/dispatch/stream.rs`：`sse_response_sets_anti_buffering_headers` 断言 `Content-Type` / `Cache-Control: no-cache, no-transform` / `X-Accel-Buffering: no` / `Connection: keep-alive` 四项头。
- `sag-gateway/src/ui_routes/negotiate.rs`：12 个协商单元测试（缺失 / 空头、`identity`、`gzip` / `br` 单独出现、并列时 `br` 胜、`q` 高者胜、`q=0` 拒绝、`*` 补位与整体拒绝、显式 0 压过通配符、畸形 token 跳过与大小写不敏感）。
- `sag-gateway/src/ui_routes/mod.rs`（`embed-ui`）：brotli / gzip 命中预压缩副本、无 `Accept-Encoding` 走 identity 且带 `Vary`、`*.br` / `*.gz` 直取 404、SPA 兜底带 `Cache-Control: no-cache`、`/` 协商压缩 index。
- `sag-router/tests/compression.rs`：`large_json_is_gzipped_and_decodes_to_the_identity_body`、`large_json_is_brotli_encoded`、`a_single_encoding_is_chosen_when_several_are_acceptable`、`missing_accept_encoding_serves_identity`、`small_response_below_the_size_floor_is_not_compressed`、`identity_q_zero_yields_406_from_tower_http`、`sse_stream_is_never_compressed`（帧刻意超过 1 KiB，证明拦截来自内容类型而非尺寸阈值）、`non_stream_chat_response_is_compressed`。
- 门禁：`just verify`（`fmt-check` + `clippy` + 全部测试，前后端）。`just clippy` / `just test-rust` 现额外覆盖 `embed-ui` 特性，前提是 `sag-ui/dist/` 已存在（`just ui-build` 或 `just build-test`）。
