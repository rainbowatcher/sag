use axum::{
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Json,
    Router,
};
use sag_config::ConfigHandle;
use serde_json::json;

use crate::auth::gateway_auth;

/// Build the main Axum router with all routes and middleware.
pub fn build_router(config: ConfigHandle) -> Router {
    // Chat completions route with auth middleware
    let chat_route = Router::new()
        .route("/", post(chat_completions_handler))
        .route_layer(middleware::from_fn_with_state(
            config.clone(),
            gateway_auth,
        ));

    Router::new()
        .route("/health", get(health_handler))
        .nest("/v1/chat/completions", chat_route)
        .with_state(config)
}

/// POST /v1/chat/completions
///
/// Placeholder — will be filled in Stage 2.
async fn chat_completions_handler() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({ "error": { "code": "NOT_IMPLEMENTED", "message": "Stage 2 — coming soon" } })),
    )
}

/// GET /health
async fn health_handler() -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}
