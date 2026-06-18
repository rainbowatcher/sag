//! Embedded UI serving for the `embed-ui` Cargo feature.
//!
//! This module is only compiled when the `embed-ui` feature is enabled
//! (declared via `#[cfg(feature = "embed-ui")]` in `main.rs`). With the
//! feature off, the sag binary stays a pure LLM gateway and expects an
//! external host for the SPA (e.g. `pnpm dev`).
//!
//! Behavior:
//! - `/assets/*` paths return the matching embedded file with a guessed
//!   Content-Type (mime_guess).
//! - `/` and any other unmatched path returns `index.html` so the SPA's
//!   HTML5 history-mode routing works on a hard refresh.
//!
//! All sag-router routes (`/v1/*`, `/api/*`, `/health`,
//! `/v1/analytics/stats`) match first because Axum's `.fallback()` only
//! fires for paths that didn't match any prior route.

use axum::{
    body::Body,
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "../sag-ui/dist/"]
struct UiAssets;

/// SPA fallback handler. Attached to the router via `.fallback()`.
pub async fn ui_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/').to_owned();
    if let Some(file) = UiAssets::get(&path) {
        return file_response(&path, file);
    }
    // Any non-asset path (including `/`) returns index.html so vue-router
    // can resolve the route client-side.
    match UiAssets::get("index.html") {
        Some(file) => file_response("index.html", file),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "embedded UI is missing index.html — did you run `pnpm -C sag-ui build`?",
        )
            .into_response(),
    }
}

fn file_response(url_path: &str, file: rust_embed::EmbeddedFile) -> Response {
    let mime = mime_guess::from_path(url_path)
        .first_or_octet_stream()
        .to_string();
    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .body(Body::from(file.data.into_owned()))
        .unwrap_or_else(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to build response for embedded asset",
            )
                .into_response()
        })
}
