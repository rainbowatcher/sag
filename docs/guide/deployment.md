# 部署

## 构建

### 普通构建（不含 UI）

```bash
cargo build --release
```

产物位于 `target/release/sag`。此模式下 UI 需单独部署（`pnpm -C sag-ui preview` / Nginx / CDN），网关本身只 serve API。

### 单二进制构建（嵌入 UI）

把 `sag-ui/dist/` 在编译期嵌入二进制，得到完全自包含的部署单元 —— API 与管理面板共存于一个进程，只需一个文件即可下发到任意目标机器。

```bash
just build-test
```

> 等价手写步骤：`pnpm -C sag-ui run build && cargo build --release --features embed-ui`。**顺序不可调换** —— 必须先 `pnpm build` 产出 `sag-ui/dist/`，rust-embed 才有的可嵌；否则编译期失败，或运行时 `ui_handler` 直接返回 500 并提示「`UiAssets::get("index.html")` missing — run `pnpm -C sag-ui build`」。

构建后 `./target/release/sag` 已自带全部前端资源，由 `sag-gateway/src/ui_routes/` 在 `/` 与 `/assets/*` 上自动 serve（SPA history-mode 兜 `/index.html`）。

`pnpm -C sag-ui run build` 末尾还会自动跑 `scripts/postbuild-compress.mjs`，为 `sag-ui/dist/` 下的文本产物（`.html` / `.js` / `.css` / `.svg` / `.json` / `.webmanifest`）各生成一份 `.br`（brotli quality 11）与 `.gz`（gzip level 9）副本；`.map`、图片、字体等已压缩格式跳过，压缩收益不足 2% 的文件不落盘。这些副本随 `rust-embed` 一起嵌入二进制（当前约 713 KiB：53 个 `.br` + 51 个 `.gz` 实测 730,544 B；随前端演进浮动。只嵌 `.br` 不嵌 `.gz` 可减半，属可选优化），网关按请求的 `Accept-Encoding` 直接下发预压缩件（`br` 优先于 `gzip`）并带上 `Vary: Accept-Encoding`；客户端不支持或该文件没有预压缩副本时回退裸文件。运行期不做任何压缩，热路径零 CPU 开销。因此三条构建链（`just build-test`、release 工作流、上面的 Dockerfile）都经 `pnpm run build` 自动生效，无需额外步骤。

## 运行

```bash
./target/release/sag --config /path/to/gateway.toml
```

构建时启用 `--features embed-ui` 的二进制自带 UI，浏览器访问 `http://<host>:<port>/` 即看到管理面板；未启用时需要把 Vite preview 反向代理到网关 `/v1` 与 `/admin`。

## Homebrew 安装（macOS Apple Silicon）

macOS arm64 的预编译单二进制（已内嵌 UI）通过 Homebrew tap 分发，本机无需 Rust / Node 工具链：

```bash
brew install rainbowatcher/tap/sag
```

以服务方式常驻前，先准备配置文件 `$(brew --prefix)/etc/sag/gateway.toml`（内容可参考仓库根 `gateway.example.toml`），然后：

```bash
brew services start sag
```

`brew services` 走 formula 的 `service` 块，等价于 `sag --config $(brew --prefix)/etc/sag/gateway.toml`；日志写入 `$(brew --prefix)/var/log/sag.log`。该二进制与本地 `just build-test` 产物同源（tag 触发的 release 工作流在 `macos-26` 上构建并内嵌 `sag-ui/dist/`）。

### 发布流程

版本由 tag 触发的 GitHub Actions 工作流（`.github/workflows/release.yml`）自动维护：先跑测试，再在 Apple Silicon runner 上构建内嵌 UI 的单二进制，创建 GitHub Release，并把 tarball 的版本 / URL / sha256 写回 `rainbowatcher/homebrew-tap`。日常发版只需升级版本号并推送 tag：

```bash
just release minor          # 升级 Rust + npm 版本号并提交
git push origin main vX.Y.Z
```

## 热重载边界

`notify` 监听 `gateway.toml` 变更并热替换 providers / routes / backoff / circuit_breaker / analytics。**`[server].host` 与 `[server].port` 改动不会触发 listen socket 重新 bind** —— 已绑定的 listening socket 不会在端口 / 地址变更时迁移。修改这两个字段后必须停止进程再启动。`[gateway_auth]` 同样需要重启。详见 [配置 → 热重载](/guide/configuration#热重载)。

## Docker

```dockerfile
FROM rust:alpine AS build
RUN apk add --no-cache musl-dev nodejs npm
RUN npm install -g pnpm
WORKDIR /app
COPY . .
RUN pnpm -C sag-ui install
RUN pnpm -C sag-ui run build
RUN cargo build --release --features embed-ui

FROM alpine
COPY --from=build /app/target/release/sag /usr/local/bin/sag
COPY gateway.toml /etc/sag/
EXPOSE 37210
CMD ["sag", "--config", "/etc/sag/gateway.toml"]
```

该 Dockerfile 走 embed-ui 单二进制路径，镜像只有 `/usr/local/bin/sag` + `/etc/sag/gateway.toml` 两个 SAG 相关文件，不需要再挂载前端资源目录。

若不需要嵌入 UI，可省略 `pnpm install` / `pnpm build` 两行，并去掉 `--features embed-ui`。此时网关只 serve API，UI 走 `pnpm -C sag-ui preview` 或独立前端服务，由 reverse proxy 把 `/assets/*` 与 `/` 指过去。

## 环境变量

| 变量 | 说明 |
|----------|------|
| `RUST_LOG` | Tracing 过滤器（例如 `info,sag=debug`） |
| Provider-level `${VAR}` | 在 TOML 中以 `${OPENAI_API_KEY}` 等形式展开 provider API key 与 route 的 `real_model` |

## 测试环境

`scripts/test-env.py` 提供与开发环境**物理隔离**的 release 路径 —— 真实 release 二进制、内嵌 UI、独立端口、独立 SQLite。`start` / `stop` / `restart` / `status` 四子命令对应 systemd / supervisor 风格的 ops 下命令。详见仓库根 [`AGENTS.md`](../../AGENTS.md) 的「测试环境」节。

```bash
just build-test        # 一次性：pnpm build UI → cargo --features embed-ui
just test-env start    # 后台拉起，pid / lock / build-info 写入 .test-env/
just test-env status   # 查 pid / uptime / binary mtime / config mtime / log tail
just test-env restart  # 原子 stop+start
just test-env stop     # SIGTERM → 5s grace → SIGKILL
```

> **默认行为提醒**：裸 `just test-env`（不带子命令）等价于 `just test-env status`，源自 `scripts/test-env.py` 中 argparse `args.cmd` 为 `None` 时 fallback 到 `cmd_status()` 的有意设计 —— status 是 read-only、作默认安全。想真正启动 gateway 必须**显式**写 `just test-env start`。无参调用看到 `[ERR] no test-env state` 不是错，是状态为空指针 —— 此时跑 `just test-env start` 即可拉起。

State（`.test-env/`）、配置（`gateway.test.toml`）、分析库（`.test-env/xdg/data/sag/analytics-test.db`）均与开发环境互不干扰。

### 日志轮转与清理

SAG 遵循 XDG Base Directory 标准，在非 TTY 环境（生产部署、test-env）下自动将日志写入 `$XDG_STATE_HOME/sag/logs/` 目录的**每日轮转文件**（默认 `~/.local/state/sag/logs/sag.log.2026-06-26`）。test-env 启动器向子进程注入项目内的 `XDG_*` 覆盖（`.test-env/xdg/`），日志落在 `.test-env/xdg/state/sag/logs/`；test-env 不再重定向 stdout，但会**捕获 stderr 到 `.test-env/stderr.log`**（每次 start 覆盖），panics 与非 tracing 的 `Error:` 输出不会静默丢失。

- **轮转**：每日自动切换新文件（`tracing-appender` `Rotation::DAILY`）
- **清理**：每次启动时自动删除 **7 天前**的旧日志文件，保留最近一周
- **格式**：配置文件 `[logging].format` 控制（`compact` / `json`），TTY 环境默认 `compact` 输出到终端
- **启动失败可见性**：启动任一步骤失败时，`main` 会通过 tracing 写入 `startup failed error=...` 到日志文件，并同时打印到 stderr（test-env 下落在 `.test-env/stderr.log`），避免仅剩一行 `starting SAG gateway` 的静默退出

`just test-env status` 会从最新的轮转日志文件中读取末尾 20 行，并在存在时展示 `.test-env/stderr.log` 的末尾内容。

## 优雅关闭

SAG 处理 `SIGINT` 与 `SIGTERM` 信号以实现优雅关闭，允许正在处理的请求完成。`just test-env stop` 走 SIGTERM → SIGKILL 升级路径：先发 SIGTERM，5 秒宽限期内进程自行退出则成功，否则 SIGKILL 兜底。
