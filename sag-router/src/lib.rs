// ── sag-router 自有模块 ───────────────────────────────────────────
pub mod handlers;
pub mod middleware;
pub mod routes;

// ── 从 sag-dispatch 重导出（保持向后兼容） ──────────────────────
pub use sag_dispatch::testing::{test_app_state, test_dispatch_runtime};
pub use sag_dispatch::{
    AppState, CachedOrder, DispatchCtx, DispatchRuntime, RouteCircuitBreaker, RouteTargetKey,
    dispatch_chat, dispatch_image, order_targets, resolve_route,
};

// ── 从 sag-admin 重导出（保持向后兼容） ──────────────────────────
pub use sag_admin::build_admin_routes;

// ── 向后兼容：保留 `admin` 模块路径 ──────────────────────────────
pub mod admin {
    pub use sag_admin::build_admin_routes;
}

// ── 自有模块的重导出 ─────────────────────────────────────────────
pub use routes::*;
