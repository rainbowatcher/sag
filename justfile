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

# 运行所有 Rust 测试
test:
    cargo test --no-fail-fast

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

# 运行 rustfmt 格式化
fmt:
    cargo fmt --all

# 运行 oxfmt 格式化
ui-fmt:
    pnpm -C sag-ui run format

# 运行 clippy 检查
clippy:
    cargo clippy --all-targets -- -D warnings

# 运行前端 oxlint
ui-lint *args:
    pnpm -C sag-ui run lint {{ args }}

# 运行所有 lint (Rust + 前端)
lint: clippy ui-lint

# ── 清理 ────────────────────────────────────────────────────────────

# 清理所有构建产物
clean:
    cargo clean
    rm -rf sag-ui/dist sag-ui/node_modules
    rm -f analytics.db analytics.db-*

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

# 一键初始化开发环境（可重复运行）
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

# ── 测试环境 (test-env) ─────────────────────────────────────────────────

# 转发到 scripts/test-env.py：`just test-env start` / `stop` / `restart` / `status`
test-env *args:
    python3 scripts/test-env.py {{ args }}
