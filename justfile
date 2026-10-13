# SAG — Smart AI Gateway 开发任务
#
# 用法:
#   just              # 列出所有任务
#   just dev           # 一键启动前后端开发环境
#   just build         # Release 构建
#   just test          # 运行所有测试

# 默认配置
config := "gateway.toml"
# ── 构建 ────────────────────────────────────────────────────────────

# Release 构建 Rust 项目
build:
    cargo build --release

# 单二进制部署构建（嵌入前端）。顺序串死避免 stale dist：
# 必须先 pnpm build 产 sag-ui/dist/，再让 cargo 把 dist/ 嵌入二进制。
build-test:
    pnpm -C sag-ui run build
    cargo build --release --features embed-ui

# Debug 构建
build-debug:
    cargo build

# 构建前端 (Vue + Vite)
ui-build:
    pnpm -C sag-ui run build

# 安装前端依赖
ui-install:
    pnpm -C sag-ui install

# ── 测试 ────────────────────────────────────────────────────────────

# 运行全部测试（Rust + 前端）。Rust 默认只跑单元/集成测试（跳过 doc test），
# 追加参数会覆盖默认值，例如 `just test --doc` 只跑文档测试。
test *args:
    just test-rust {{ args }}
    just ui-test

# 运行 Rust 测试（无参数时默认 `--tests`，即跳过 doc test）
test-rust *args:
    cargo test --no-fail-fast {{ if args == "" { "--tests" } else { args } }}

# 运行前端测试
ui-test:
    pnpm -C sag-ui run test

# 运行单个 crate 的测试 (e.g. just test-crate sag-config)
test-crate crate:
    cargo test -p {{ crate }} --no-fail-fast

# ── 运行 ────────────────────────────────────────────────────────────

# 仅启动后端 (debug 模式)
run config=config:
    cargo run -- {{ config }}

# 仅启动后端 (release 模式)
run-release config=config:
    cargo run --release -- {{ config }}

# 启动前端开发服务器
ui-dev:
    cd sag-ui && pnpm -C sag-ui run dev

# 预览前端构建产物
ui-preview:
    cd sag-ui && pnpm -C sag-ui run preview

# 启动 Histoire 组件文档开发服务器
ui-story:
    cd sag-ui && pnpm run story:dev

# ── 开发环境 ────────────────────────────────────────────────────────

# 一键启动前后端开发环境 (推荐)
dev *args:
    python3 scripts/dev.py {{ args }}

# ── 代码质量 ────────────────────────────────────────────────────────

# 格式化 Rust + 前端（会改写文件）
fmt: fmt-rust ui-fmt

# 运行 rustfmt 格式化
fmt-rust:
    cargo fmt --all

# 运行 oxfmt 格式化
ui-fmt:
    pnpm -C sag-ui run format

# 格式化检查 Rust + 前端（只读，不改写文件）
fmt-check: fmt-check-rust ui-fmt-check

# 运行 rustfmt 格式化检查（只读）
fmt-check-rust:
    cargo fmt --all --check

# 运行 oxfmt 格式化检查（只读）
ui-fmt-check:
    pnpm -C sag-ui run format:check

# 运行 clippy 检查
clippy:
    cargo clippy --all-targets -- -D warnings

# 运行前端 oxlint
ui-lint *args:
    pnpm -C sag-ui run lint {{ args }}

# 运行前端类型检查 (vue-tsc)
ui-typecheck:
    pnpm -C sag-ui run typecheck

# 运行所有 lint (Rust clippy + 前端 oxlint + vue-tsc)
lint: clippy ui-lint ui-typecheck

# ── 验证门禁 ────────────────────────────────────────────────────────

# 全量验证：fmt-check + lint + test（前后端）。pre-commit hook 的等价入口
verify: verify-rust verify-ui

# Rust 轨验证：fmt-check + clippy + test
verify-rust: fmt-check-rust clippy test-rust

# 前端轨验证：fmt-check + oxlint/typecheck + test
verify-ui: ui-fmt-check ui-lint ui-typecheck ui-test

# ── 清理 ────────────────────────────────────────────────────────────

# 清理所有构建产物。analytics 数据库与滚动日志在用户级 XDG 目录
# （$XDG_DATA_HOME/sag、$XDG_STATE_HOME/sag，默认 ~/.local/share/sag 与
# ~/.local/state/sag），属用户数据，clean 不触碰；测试轨的全部产物在
# .test-env/ 下，`just test-env stop` 后可整体删除。
clean:
    cargo clean
    rm -rf sag-ui/dist sag-ui/node_modules

# 只清理 Rust 构建产物
clean-rust:
    cargo clean

# ── 文档 ────────────────────────────────────────────────────────────

# 构建文档网站
docs:
    pnpm -C docs run build

# 启动文档开发服务器 (热重载)
docs-serve:
    pnpm -C docs run dev

# ── 审计 ────────────────────────────────────────────────────────────

# cargo audit (需要先安装: cargo install cargo-audit)
audit:
    cargo audit

# ── 辅助 ────────────────────────────────────────────────────────────

# 一键初始化开发环境（可重复运行）。配置写在项目根目录（CWD 回退命中）；
# 也可改放 $XDG_CONFIG_HOME/sag/gateway.toml（删掉本文件即可）。
setup:
    git config core.hooksPath scripts/hooks
    @echo "✓ Git hooks installed"
    @if [ ! -f gateway.toml ]; then cp gateway.example.toml gateway.toml && echo "✓ Created gateway.toml"; else echo "  gateway.toml already exists"; fi
    pnpm -C sag-ui install
    @echo "✓ Frontend dependencies installed"
    @echo ""
    @echo "Development environment is ready!"
    @echo "Run 'just dev' to start."

# 查看项目树 (排除 target / node_modules)
tree:
    find . -not -path './target/*' -not -path './sag-ui/node_modules/*' -not -path './.git/*' -not -name '*.db*' | head -120

# 升级 Rust + npm 项目版本号并自动提交：`just bump minor`
# （major/minor/patch；升级后自动 git commit "chore: release vX.Y.Z"，不推送，
# 只提交本次升级文件；--dry-run 预览、--no-commit 跳过提交、--npm 指定 package.json）
bump *args:
    python3 scripts/bump-version.py {{ args }}

# ── 测试环境 (test-env) ─────────────────────────────────────────────────

# 转发到 scripts/test-env.py：`just test-env start` / `stop` / `restart` / `status`
test-env *args:
    python3 scripts/test-env.py {{ args }}
