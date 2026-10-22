//! Embedded UI serving for the `embed-ui` Cargo feature.
//!
//! With the feature on, `sag-ui/dist/` is rolled into the binary via
//! `rust-embed` and this module serves it as an Axum fallback. With the
//! feature off, the sag binary stays a pure LLM gateway and expects an
//! external host for the SPA (e.g. `pnpm dev`); only the dependency-free
//! [`negotiate`] helper is compiled in that case.
//!
//! Behavior:
//! - Embedded paths resolve to the matching file with a Content-Type guessed
//!   from the *original* asset path.
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
//! # Fallback decision order (each step short-circuits)
//!
//! 1. `api/` or `v1/` prefix → `404` with the project's JSON error body. An
//!    unregistered API path must not turn into a `200` HTML shell, and it must
//!    not see the static `405` (a misleading `Allow: GET, HEAD` for an API
//!    client). Registered routes never reach the fallback at all.
//! 2. non-`GET`/`HEAD` → `405` with `Allow: GET, HEAD` (static surface only).
//! 3. `resolve(path)` hit → serve the file.
//! 4. `assets/` prefix → `404` (Vite namespace, never a client-side route).
//! 5. `/` → shell.
//! 6. `Accept: text/html` → shell (a browser hard-refreshing a dotted route
//!    such as `/providers/glm-4.6` must still get the SPA).
//! 7. last segment contains `.` → `404` (favicon / `*.map` / image probes).
//! 8. otherwise → shell.
//!
//! # Cache-Control is tiered by path, not by file type
//!
//! - `assets/**` → [`ASSET_CACHE_CONTROL`] (`immutable`): Vite guarantees a
//!   content hash in every file name in that directory.
//! - everything else (`index.html`, future root-level files) →
//!   [`INDEX_CACHE_CONTROL`] (`no-cache`): no hash, so the browser must
//!   revalidate. Deliberately *not* "immutable for everything but
//!   index.html": a future `favicon.svg` / `robots.txt` would otherwise get
//!   pinned for a year.
//!
//! `404` / `405` carry no `Cache-Control` on purpose: they have no validator,
//! so browsers never heuristically cache them; `no-store` would only add a
//! header to probes. Every response gets `X-Content-Type-Options: nosniff`.
//!
//! # Invariant: path traversal is structurally impossible
//!
//! Lookups go through `UiAssets::get` and nothing else. In a release build
//! that is a compile-time static table lookup (non-key inputs such as `../`
//! or absolute paths simply miss); in debug builds rust-embed reads from disk
//! and relies on its own (≥ 8.x) path validation. Never hand-roll `fs::read`
//! or join filesystem paths in this module — that would forfeit the
//! invariant.
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
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
#[cfg(feature = "embed-ui")]
use negotiate::Encoding;
#[cfg(feature = "embed-ui")]
use rust_embed::Embed;

/// Cache-Control for the revalidated tier (`index.html` and any future
/// root-level file). Those have no content hash in their name, so they must be
/// revalidated on every load — otherwise a stale shell keeps pointing at
/// deleted chunk names.
#[cfg(feature = "embed-ui")]
const INDEX_CACHE_CONTROL: &str = "no-cache";

/// Cache-Control for Vite's hashed asset namespace. Every file under
/// `assets/` carries a content hash, so the URL is a permanent name for one
/// immutable byte sequence.
#[cfg(feature = "embed-ui")]
const ASSET_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

#[cfg(feature = "embed-ui")]
const NOSNIFF: &str = "nosniff";

#[cfg(feature = "embed-ui")]
#[derive(Embed)]
#[folder = "../sag-ui/dist/"]
struct UiAssets;

/// SPA fallback handler. Attached to the router via `.fallback()`.
#[cfg(feature = "embed-ui")]
pub(crate) async fn ui_handler(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');

    // Step 1: API namespaces never fall back to the SPA shell. An unregistered
    // `/api/*` or `/v1/*` path is a 404 with the project's JSON error body, not
    // HTML, and not a misleading `Allow: GET, HEAD`.
    if is_api_namespace(path) {
        return not_found_json();
    }

    // Step 2: the static surface only speaks GET/HEAD. `HEAD` bodies are
    // stripped by hyper; nothing to do here.
    if method != Method::GET && method != Method::HEAD {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [
                (header::ALLOW, "GET, HEAD"),
                (header::X_CONTENT_TYPE_OPTIONS, NOSNIFF),
            ],
            "",
        )
            .into_response();
    }

    let accept_encoding = headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|value| value.to_str().ok());
    let encoding = negotiate::negotiate(accept_encoding);

    // Never serve a raw pre-compressed sibling as a standalone resource: the
    // browser would treat `foo.js.br` as a file of its own.
    if is_precompressed(path) {
        return not_found();
    }

    // Step 3: an embedded file wins. Lookup is a table lookup in release
    // builds, so `../` / absolute inputs simply miss (see module invariant).
    if let Some(response) = resolve(path, encoding, cache_control_for(path)) {
        return response;
    }

    // Steps 4-7: decide between a real 404 and the SPA shell.
    let is_asset_namespace = path.starts_with("assets/");
    let is_dotted_subresource = path != "/" && looks_like_asset(path);
    if is_asset_namespace || (is_dotted_subresource && !wants_html(&headers)) {
        return not_found();
    }

    // Step 8: any remaining path (including `/`) returns index.html so
    // vue-router can resolve the route client-side.
    match resolve("index.html", encoding, INDEX_CACHE_CONTROL) {
        Some(response) => response,
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::X_CONTENT_TYPE_OPTIONS, NOSNIFF)],
            "embedded UI is missing index.html — did you run `pnpm -C sag-ui build`?",
        )
            .into_response(),
    }
}

/// True for the API namespaces served by `sag-router`.
#[cfg(feature = "embed-ui")]
fn is_api_namespace(path: &str) -> bool {
    path.starts_with("api/") || path.starts_with("v1/")
}

/// Plain-text 404 for the static surface. No `Cache-Control`: without a
/// validator browsers never heuristically cache it (see module docs).
#[cfg(feature = "embed-ui")]
fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::X_CONTENT_TYPE_OPTIONS, NOSNIFF)],
        "not found",
    )
        .into_response()
}

/// JSON 404 for unregistered API paths, shaped like
/// `AppError::NotFound`'s `IntoResponse` (`sag-core/src/error.rs`).
#[cfg(feature = "embed-ui")]
fn not_found_json() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::X_CONTENT_TYPE_OPTIONS, NOSNIFF)],
        axum::Json(serde_json::json!({
            "error": { "code": "NOT_FOUND", "message": "no such API route" }
        })),
    )
        .into_response()
}

/// `assets/**` is immutable, everything else must revalidate. Unconditional:
/// both tiers always have a value, so there is no `Option` to thread around.
#[cfg(feature = "embed-ui")]
fn cache_control_for(path: &str) -> &'static str {
    if path.starts_with("assets/") {
        ASSET_CACHE_CONTROL
    } else {
        INDEX_CACHE_CONTROL
    }
}

/// True when the request advertises an HTML preference (browser navigation).
#[cfg(feature = "embed-ui")]
fn wants_html(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/html"))
}

/// True when the last path segment looks like a file name (`favicon.ico`,
/// `foo.js.map`, `logo.svg`). Used to answer subresource probes with a 404
/// instead of the SPA shell.
#[cfg(feature = "embed-ui")]
fn looks_like_asset(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.'))
}

/// Looks `url_path` up in the embedded assets, preferring the negotiated
/// pre-compressed sibling when one was embedded.
#[cfg(feature = "embed-ui")]
fn resolve(url_path: &str, encoding: Encoding, cache_control: &'static str) -> Option<Response> {
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
    cache_control: &'static str,
) -> Response {
    let mime = mime_guess::from_path(mime_path)
        .first_or_octet_stream()
        .to_string();
    // `Vary` is unconditional, including for identity: a shared cache that
    // saw the identity response must not replay it to a br-capable client.
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .header(header::VARY, "Accept-Encoding")
        .header(header::CACHE_CONTROL, cache_control)
        .header(header::X_CONTENT_TYPE_OPTIONS, NOSNIFF);
    if let Some(content_encoding) = encoding.content_encoding() {
        builder = builder.header(header::CONTENT_ENCODING, content_encoding);
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

    /// Builds a request with an explicit method and extra headers.
    fn request_with(uri: &str, method: &str, headers: &[(&str, &str)]) -> Request<Body> {
        let mut builder = Request::builder().uri(uri).method(method);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::empty()).expect("request should build")
    }

    async fn send(request: Request<Body>) -> axum::response::Response {
        Router::new()
            .fallback(super::ui_handler)
            .oneshot(request)
            .await
            .expect("router is infallible")
    }

    async fn get(uri: &str, accept_encoding: Option<&str>) -> axum::response::Response {
        send(request(uri, accept_encoding)).await
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

    // ---------------------------------------------------------------
    // Stage 1: cache tiers, fallback narrowing, method restriction.
    // ---------------------------------------------------------------

    async fn body_string(response: axum::response::Response) -> String {
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn hashed_asset_gets_immutable_cache_control() {
        let Some(path) = first_asset(".js") else {
            return;
        };
        assert!(
            path.starts_with("assets/"),
            "expected a hashed asset under assets/, got {path}"
        );

        let response = get(&format!("/{path}"), None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_str(&response, header::CACHE_CONTROL).as_deref(),
            Some("public, max-age=31536000, immutable")
        );
    }

    #[tokio::test]
    async fn root_shell_is_not_immutable() {
        if !has_asset("index.html") {
            return;
        }

        let response = get("/", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_str(&response, header::CACHE_CONTROL).as_deref(),
            Some("no-cache")
        );
    }

    #[tokio::test]
    async fn missing_asset_returns_404_not_shell() {
        let response = get("/assets/gone.js", None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_string(response).await;
        assert!(
            !body.to_ascii_lowercase().contains("<!doctype"),
            "a missing asset must not return the SPA shell, got {body}"
        );
    }

    #[tokio::test]
    async fn missing_root_file_returns_404() {
        // Guaranteed-absent name: never use a real file (`/vite.svg` may be
        // added later and would turn this into a false failure).
        let response = send(request_with(
            "/__missing__.svg",
            "GET",
            &[("accept", "*/*")],
        ))
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_string(response).await;
        assert!(
            !body.to_ascii_lowercase().contains("<!doctype"),
            "got {body}"
        );
    }

    #[tokio::test]
    async fn unknown_api_path_returns_404_json() {
        let response = get("/api/unknown", None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            header_str(&response, header::CONTENT_TYPE).as_deref(),
            Some("application/json")
        );
        let body = body_string(response).await;
        assert!(
            !body.to_ascii_lowercase().contains("<!doctype"),
            "got {body}"
        );
        assert!(
            body.contains("\"code\":\"NOT_FOUND\""),
            "unexpected error body {body}"
        );
    }

    #[tokio::test]
    async fn unknown_api_path_ignores_method_restriction() {
        let response = send(request_with("/api/unknown", "POST", &[])).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(header_str(&response, header::ALLOW), None);
        assert_eq!(
            header_str(&response, header::CONTENT_TYPE).as_deref(),
            Some("application/json")
        );
    }

    #[tokio::test]
    async fn unknown_v1_path_returns_404_json() {
        let response = get("/v1/unknown", None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            header_str(&response, header::CONTENT_TYPE).as_deref(),
            Some("application/json")
        );
    }

    #[tokio::test]
    async fn dotted_route_with_html_accept_gets_shell() {
        if !has_asset("index.html") {
            return;
        }

        let response = send(request_with(
            "/providers/glm-4.6",
            "GET",
            &[("accept", "text/html,application/xhtml+xml")],
        ))
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_str(&response, header::CONTENT_TYPE).as_deref(),
            Some("text/html")
        );
    }

    #[tokio::test]
    async fn non_get_method_is_rejected() {
        let response = send(request_with("/", "POST", &[])).await;
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            header_str(&response, header::ALLOW).as_deref(),
            Some("GET, HEAD")
        );
    }

    #[tokio::test]
    async fn head_is_allowed() {
        if !has_asset("index.html") {
            return;
        }

        let response = send(request_with("/", "HEAD", &[])).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            header_str(&response, header::CONTENT_TYPE).as_deref(),
            Some("text/html")
        );
    }

    #[tokio::test]
    async fn all_responses_are_nosniff() {
        let Some(asset) = first_asset(".js") else {
            return;
        };

        let cases: [(&str, &str); 5] = [
            ("/", "GET"),
            (&format!("/{asset}"), "GET"),
            ("/assets/gone.js", "GET"),
            ("/api/unknown", "GET"),
            ("/", "POST"),
        ];
        for (uri, method) in cases {
            let response = send(request_with(uri, method, &[])).await;
            assert_eq!(
                header_str(&response, header::X_CONTENT_TYPE_OPTIONS).as_deref(),
                Some("nosniff"),
                "{method} {uri} is missing nosniff"
            );
        }
    }
}
