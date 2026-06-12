use axum::{extract::Request, middleware::Next, response::Response};
use tracing::Instrument;
use tracing::info_span;

/// 根 Trace 中间件 — 在请求进入的第一时间生成 `trace_id`，
/// 注入到 `request.extensions()`，并创建根 tracing span。
///
/// 所有下游中间件和 handler 都从 extensions 读取 trace_id，
/// tracing span 会自动形成 parent-child 层级，使每一条日志
/// 都携带相同的 trace_id，实现跨中间件的全请求链路追踪。
pub async fn trace_middleware(mut request: Request, next: Next) -> Response {
    let trace_id = uuid::Uuid::new_v4();
    request.extensions_mut().insert(trace_id);

    let span = info_span!(
        "http_request",
        trace_id = %trace_id,
        method = %request.method(),
        uri = %request.uri(),
    );

    async { next.run(request).await }.instrument(span).await
}
