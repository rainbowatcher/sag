//! Embedded UI serving for the `embed-ui` Cargo feature.
//!
//! With the feature on, `sag-ui/dist/` is rolled into the binary via
//! `rust-embed` and this module serves it as an Axum fallback. With the
//! feature off, the sag binary stays a pure LLM gateway and expects an
//! external host for the SPA (e.g. `pnpm dev`); only the dependency-free
//! [`negotiate`] helper is compiled in that case.
//!
//! Behavior:
//! - `/assets/*` (and any other embedded path) returns the matching file with
//!   a Content-Type guessed from the *original* asset path.
//! - `/` and any other unmatched path returns `index.html` so the SPA's
//!   HTML5 history-mode routing works on a hard refresh.
//! - Text assets are shipped pre-compressed: the UI build emits `.br` /
//!   `.gz` siblings and this module picks one from `Accept-Encoding`
//!   (see [`negotiate`]). Identity is the fallback when no sibling exists
//!   (images, fonts, …) — nothing is compressed at runtime.
//!
//! All sag-router routes (`/v1/*`, `/api/*`, `/health`,
//! `/v1/analytics/stats`) match first because Axum's `.fallback()` only
//! fires for paths that didn't match any prior route.
//!
//! # Invariant: the UI fallback sits *outside* the middleware stack
//!
//! `main.rs` attaches this handler with `app.fallback(...)` **after**
//! `sag_router::build_router()` has already applied its `.layer(...)` calls
//! (`log_http`, tracing, CORS, body limit). Axum bakes layers into the routes
//! that exist at `.layer()` call time, so fallback responses never pass
//! through them. Two consequences follow, and both are load-bearing:
//!
//! - No response-compression layer reaches this handler (sag-router applies
//!   its own `CompressionLayer` to its own routes only), so `Content-Encoding`
//!   must be set here, from the embedded sibling.
//! - There is no middleware adding `Vary` / cache headers either, so this
//!   handler sets `Vary: Accept-Encoding` (and `Cache-Control` for the SPA
//!   shell) itself. If the fallback is ever moved inside the layered router,
//!   these headers must be re-checked for duplication.

// Without the feature nothing calls into the negotiator (only its unit tests
// do), so silence the dead-code lint for the no-feature build.
#[cfg_attr(not(feature = "embed-ui"), allow(dead_code))]
pub(crate) mod negotiate;

#[cfg(feature = "embed-ui")]
use axum::{
    body::Body,
    http::{HeaderMap, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
#[cfg(feature = "embed-ui")]
use negotiate::Encoding;
#[cfg(feature = "embed-ui")]
use rust_embed::Embed;

/// Cache-Control for the SPA shell. `index.html` is the only asset without a
/// content hash in its name, so it must be revalidated on every load —
/// otherwise a stale shell keeps pointing at deleted chunk names.
#[cfg(feature = "embed-ui")]
const INDEX_CACHE_CONTROL: &str = "no-cache";

#[cfg(feature = "embed-ui")]
#[derive(Embed)]
#[folder = "../sag-ui/dist/"]
struct UiAssets;

/// SPA fallback handler. Attached to the router via `.fallback()`.
#[cfg(feature = "embed-ui")]
pub(crate) async fn ui_handler(uri: Uri, headers: HeaderMap) -> Response {
    let accept_encoding = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|value| value.to_str().ok());
    let encoding = negotiate::negotiate(accept_encoding);

    let path = uri.path().trim_start_matches('/');

    // Never serve a raw pre-compressed sibling as a standalone resource: the
    // browser would treat `foo.js.br` as a file of its own.
    if is_precompressed(path) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }

    if let Some(response) = resolve(path, encoding, None) {
        return response;
    }

    // Any non-asset path (including `/`) returns index.html so vue-router
    // can resolve the route client-side.
    match resolve("index.html", encoding, Some(INDEX_CACHE_CONTROL)) {
        Some(response) => response,
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "embedded UI is missing index.html — did you run `pnpm -C sag-ui build`?",
        )
            .into_response(),
    }
}

/// Looks `url_path` up in the embedded assets, preferring the negotiated
/// pre-compressed sibling when one was embedded.
#[cfg(feature = "embed-ui")]
fn resolve(url_path: &str, encoding: Encoding, cache_control: Option<&str>) -> Option<Response> {
    let compressed =
        encoded_suffix(encoding).and_then(|suffix| UiAssets::get(&format!("{url_path}{suffix}")));
    if let Some(file) = compressed {
        return Some(file_response(url_path, file, encoding, cache_control));
    }
    UiAssets::get(url_path)
        .map(|file| file_response(url_path, file, Encoding::Identity, cache_control))
}

/// Suffix of the embedded pre-compressed sibling for `encoding`.
#[cfg(feature = "embed-ui")]
fn encoded_suffix(encoding: Encoding) -> Option<&'static str> {
    match encoding {
        Encoding::Brotli => Some(".br"),
        Encoding::Gzip => Some(".gz"),
        Encoding::Identity => None,
    }
}

/// True for paths that name a pre-compressed artifact (`.br` / `.gz`).
#[cfg(feature = "embed-ui")]
fn is_precompressed(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".br") || path.ends_with(".gz")
}

/// Builds the response for one embedded file.
///
/// `mime_path` is always the *original* asset path: guessing the media type
/// from `foo.js.br` would yield `application/gzip` — that is the coding, not
/// the type.
#[cfg(feature = "embed-ui")]
fn file_response(
    mime_path: &str,
    file: rust_embed::EmbeddedFile,
    encoding: Encoding,
    cache_control: Option<&str>,
) -> Response {
    let mime = mime_guess::from_path(mime_path)
        .first_or_octet_stream()
        .to_string();
    // `Vary` is unconditional, including for identity: a shared cache that
    // saw the identity response must not replay it to a br-capable client.
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::VARY, "Accept-Encoding");
    if let Some(content_encoding) = encoding.content_encoding() {
        builder = builder.header(header::CONTENT_ENCODING, content_encoding);
    }
    if let Some(cache_control) = cache_control {
        builder = builder.header(header::CACHE_CONTROL, cache_control);
    }
    builder
        .body(Body::from(file.data.into_owned()))
        .unwrap_or_else(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to build response for embedded asset",
            )
                .into_response()
        })
}

#[cfg(all(test, feature = "embed-ui"))]
mod tests {
    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use tower::ServiceExt;

    use super::UiAssets;

    /// First embedded asset whose path ends with `suffix` and is not itself a
    /// pre-compressed sibling. `None` when `sag-ui/dist/` was empty at compile
    /// time (e.g. a bare `cargo test --features embed-ui` without a UI build).
    fn first_asset(suffix: &str) -> Option<String> {
        UiAssets::iter()
            .map(|path| path.to_string())
            .find(|path| path.ends_with(suffix) && !path.ends_with(".br") && !path.ends_with(".gz"))
    }

    fn has_asset(path: &str) -> bool {
        UiAssets::get(path).is_some()
    }

    fn request(uri: &str, accept_encoding: Option<&str>) -> Request<Body> {
        let builder = Request::builder().uri(uri);
        match accept_encoding {
            Some(value) => builder.header(header::ACCEPT_ENCODING, value),
            None => builder,
        }
        .body(Body::empty())
        .expect("request should build")
    }

    async fn get(uri: &str, accept_encoding: Option<&str>) -> axum::response::Response {
        Router::new()
            .fallback(super::ui_handler)
            .oneshot(request(uri, accept_encoding))
            .await
            .expect("router is infallible")
    }

    fn header_str(response: &axum::response::Response, name: header::HeaderName) -> Option<String> {
        response
            .headers()
            .get(name)
            .map(|value| value.to_str().expect("header is ASCII").to_owned())
    }

    #[tokio::test]
    async fn brotli_is_served_from_precompressed_sibling() {
        let Some(path) = first_asset(".js") else {
            return; // dist/ was not built; nothing to assert against.
        };
        let compressed = format!("{path}.br");
        if !has_asset(&compressed) {
            return;
        }

        let response = get(&format!("/{path}"), Some("br")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_str(&response, header::CONTENT_ENCODING).as_deref(),
            Some("br")
        );
        assert_eq!(
            header_str(&response, header::VARY).as_deref(),
            Some("Accept-Encoding")
        );
        let content_type = header_str(&response, header::CONTENT_TYPE).expect("content-type");
        assert!(
            content_type.contains("javascript"),
            "unexpected content-type {content_type} for {path}"
        );

        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let expected = UiAssets::get(&compressed).expect("sibling").data;
        assert_eq!(body.as_ref(), expected.as_ref());
    }

    #[tokio::test]
    async fn gzip_is_served_from_precompressed_sibling() {
        let Some(path) = first_asset(".js") else {
            return;
        };
        let compressed = format!("{path}.gz");
        if !has_asset(&compressed) {
            return;
        }

        let response = get(&format!("/{path}"), Some("gzip")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_str(&response, header::CONTENT_ENCODING).as_deref(),
            Some("gzip")
        );
        assert_eq!(
            header_str(&response, header::VARY).as_deref(),
            Some("Accept-Encoding")
        );
        let content_type = header_str(&response, header::CONTENT_TYPE).expect("content-type");
        assert!(
            content_type.contains("javascript"),
            "unexpected content-type {content_type} for {path}"
        );
    }

    #[tokio::test]
    async fn no_accept_encoding_serves_identity_with_vary() {
        let Some(path) = first_asset(".js") else {
            return;
        };

        let response = get(&format!("/{path}"), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(header_str(&response, header::CONTENT_ENCODING), None);
        assert_eq!(
            header_str(&response, header::VARY).as_deref(),
            Some("Accept-Encoding")
        );

        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let expected = UiAssets::get(&path).expect("asset").data;
        assert_eq!(body.as_ref(), expected.as_ref());
    }

    #[tokio::test]
    async fn precompressed_paths_are_not_served() {
        let Some(path) = first_asset(".js") else {
            return;
        };

        for suffix in [".br", ".gz"] {
            if !has_asset(&format!("{path}{suffix}")) {
                continue;
            }
            let response = get(&format!("/{path}{suffix}"), Some("br")).await;
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{path}{suffix} must not be downloadable as a plain file"
            );
        }
    }

    #[tokio::test]
    async fn spa_fallback_serves_index_with_cache_control() {
        if !has_asset("index.html") {
            return;
        }

        let response = get("/some/deep/route", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_str(&response, header::CONTENT_TYPE).as_deref(),
            Some("text/html")
        );
        assert_eq!(
            header_str(&response, header::CACHE_CONTROL).as_deref(),
            Some("no-cache")
        );
        assert_eq!(
            header_str(&response, header::VARY).as_deref(),
            Some("Accept-Encoding")
        );
        assert_eq!(header_str(&response, header::CONTENT_ENCODING), None);
    }

    #[tokio::test]
    async fn root_negotiates_index_compression() {
        if !has_asset("index.html") {
            return;
        }
        let expect_brotli = has_asset("index.html.br");

        let response = get("/", Some("br, gzip")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_str(&response, header::CONTENT_TYPE).as_deref(),
            Some("text/html")
        );
        assert_eq!(
            header_str(&response, header::CONTENT_ENCODING).as_deref(),
            if expect_brotli { Some("br") } else { None }
        );
        assert_eq!(
            header_str(&response, header::CACHE_CONTROL).as_deref(),
            Some("no-cache")
        );
        assert_eq!(
            header_str(&response, header::VARY).as_deref(),
            Some("Accept-Encoding")
        );
    }
}
