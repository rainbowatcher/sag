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

# Debug 构建
build-debug:
    cargo build

# 构建前端 (Vue + Vite)
ui-build:
    cd sag-ui && npm run build

# 安装前端依赖
ui-install:
    cd sag-ui && npm install

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
    cd sag-ui && npm run dev

# 预览前端构建产物
ui-preview:
    cd sag-ui && npm run preview

# ── 开发环境 ────────────────────────────────────────────────────────

# 一键启动前后端开发环境 (推荐)
dev config=config:
    python3 scripts/dev.py --config {{ config }}

# ── 代码质量 ────────────────────────────────────────────────────────

# 运行 rustfmt 格式化
fmt:
    cargo fmt --all

# 运行 clippy 检查
clippy:
    cargo clippy --all-targets -- -D warnings

# 运行前端 oxlint
ui-lint:
    cd sag-ui && npm run lint

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
    mdbook build docs

# 安装文档依赖
docs-install:
    cd scripts/mdbook-beautiful-mermaid && npm install

# 启动文档开发服务器 (热重载)
docs-serve:
    mdbook serve docs -p 38718 --open

# ── 审计 ────────────────────────────────────────────────────────────

# cargo audit (需要先安装: cargo install cargo-audit)
audit:
    cargo audit

# ── 辅助 ────────────────────────────────────────────────────────────

# 生成示例配置文件
init-config:
    cp gateway.example.toml gateway.toml
    @echo "Created gateway.toml from example"

# 查看项目树 (排除 target / node_modules)
tree:
    find . -not -path './target/*' -not -path './sag-ui/node_modules/*' -not -path './.git/*' -not -name '*.db*' | head -120
