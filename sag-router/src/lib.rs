// ── sag-router 自有模块 ───────────────────────────────────────────
pub mod handlers;
pub mod middleware;
pub mod routes;
pub mod session;

// ── 从 sag-state 重导出（共享状态） ───────────────────────────
pub use sag_state::{ActiveTargets, AppState, FillFirstOrders, testing::test_app_state};

// ── 从 sag-dispatch 重导出（保持向后兼容） ──────────────────────
pub use sag_dispatch::testing::test_dispatch_runtime;
pub use sag_dispatch::{
    CachedOrder, DispatchCtx, DispatchRuntime, dispatch_chat, dispatch_image, order_targets,
    resolve_route, resolve_route_with_ingress, stale_probe_threshold_from_config,
};

// ── 从 sag-core 重导出（熔断器原语直供） ────────────────────────
pub use sag_core::{RouteCircuitBreaker, RouteTargetKey};

// ── 从 sag-admin 重导出（保持向后兼容） ──────────────────────────
pub use sag_admin::build_admin_routes;

// ── 向后兼容：保留 `admin` 模块路径 ──────────────────────────────
pub mod admin {
    pub use sag_admin::build_admin_routes;
}

// ── 自有模块的重导出 ─────────────────────────────────────────────
pub use routes::*;
