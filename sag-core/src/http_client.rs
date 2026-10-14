//! Shared HTTP client construction with explicit proxy control.
//!
//! Both provider adapters (`sag-providers`) and admin tooling
//! (`sag-admin`) build `reqwest::Client`s that must follow SAG's proxy
//! policy: **never** silently honor the system proxy environment variables
//! (`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` and their lowercase forms).
//! A client is either given an explicit proxy URL, or it connects directly
//! with the system proxy explicitly disabled.

use std::time::Duration;

/// How long an **idle** connection may stay in the pool before being
/// dropped. reqwest's default is 90s; SAG tightens it to reduce the chance
/// of reusing a connection that the peer / an intermediary already closed
/// (surfacing as a mid-stream `close_notify` / EOF).
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of **idle** connections kept per host. reqwest's default
/// is `usize::MAX` (unbounded); SAG caps it to bound stale-connection reuse.
const POOL_MAX_IDLE_PER_HOST: usize = 4;

/// Build a [`reqwest::Client`] with SAG's proxy policy.
///
/// - `proxy: None` → **direct connection**. The client explicitly disables
///   the system proxy (`ClientBuilder::no_proxy`), so `HTTP_PROXY` /
///   `HTTPS_PROXY` / `ALL_PROXY` (and case variants) are ignored. This is
///   deliberate: SAG never relies on reqwest's default system-proxy
///   discovery.
/// - `proxy: Some(connect_url)` → the client routes **all** HTTP/HTTPS
///   traffic through that single proxy URL via [`reqwest::Proxy::all`], and
///   ignores the system proxy environment variables.
/// - `timeout: None` → **no client-level total request deadline**
///   (callers like the streaming path impose their own per-request /
///   per-chunk timeouts). `Some(d)` applies a total timeout covering
///   connect through body completion.
///
/// The connection pool is tuned in **both** branches with
/// [`POOL_IDLE_TIMEOUT`] and [`POOL_MAX_IDLE_PER_HOST`]. These options only
/// constrain the *idle* pool — they never truncate an in-flight stream body.
/// No client-level total `.timeout()` is added by this tuning.
///
/// `connect_url` must be a valid `http://` or `https://` proxy URL — e.g. a
/// `sag_config::ProxyConfig` converted via its `to_connect_url()`. This
/// function deliberately does not depend on `sag-config` (which depends on
/// `sag-core`); callers holding a `ProxyConfig` turn it into a connect URL
/// before calling. Proxy credentials (userinfo) are not supported.
pub fn build_http_client(
    proxy: Option<&str>,
    timeout: Option<Duration>,
) -> Result<reqwest::Client, reqwest::Error> {
    let mut builder = reqwest::Client::builder();
    match proxy {
        Some(connect_url) => {
            // `Proxy::all` covers both http and https targets and — per its
            // contract — disables the automatic "system" proxy, so the
            // client uses ONLY this explicit proxy.
            builder = builder.proxy(reqwest::Proxy::all(connect_url)?);
            builder = builder
                .pool_idle_timeout(POOL_IDLE_TIMEOUT)
                .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST);
        }
        None => {
            // Explicitly disable the system proxy. This lives in its own
            // branch on purpose: calling `no_proxy()` *after* `.proxy(...)`
            // would clear the explicit proxy (it empties the proxy list).
            builder = builder.no_proxy();
            builder = builder
                .pool_idle_timeout(POOL_IDLE_TIMEOUT)
                .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST);
        }
    }
    if let Some(dur) = timeout {
        builder = builder.timeout(dur);
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three construction branches must all produce a usable client:
    /// direct (no proxy) and explicit `http://` / `https://` proxies. We
    /// only assert that the client builds and can issue a request object —
    /// no IO — because reqwest's proxy routing is internal and not
    /// observable from the outside.
    #[test]
    fn direct_client_builds() {
        let client = build_http_client(None, None).expect("direct client builds");
        let _req = client.get("http://127.0.0.1:1/");
    }

    #[test]
    fn explicit_http_proxy_client_builds() {
        let client =
            build_http_client(Some("http://127.0.0.1:8080"), None).expect("http proxy builds");
        let _req = client.get("http://127.0.0.1:1/");
    }

    #[test]
    fn explicit_https_proxy_client_builds() {
        let client = build_http_client(Some("https://proxy.example.com:8443"), None)
            .expect("https proxy builds");
        let _req = client.get("https://example.com/");
    }

    #[test]
    fn timeout_is_applied_when_some() {
        let client =
            build_http_client(None, Some(Duration::from_secs(5))).expect("timeout client builds");
        let _req = client.get("http://127.0.0.1:1/");
    }

    /// Pin the pool tuning values: a silent change to either constant must
    /// fail this test (the spec's 30s / 4 contract).
    #[test]
    fn pool_connection_constants_match_plan() {
        assert_eq!(POOL_IDLE_TIMEOUT, Duration::from_secs(30));
        assert_eq!(POOL_MAX_IDLE_PER_HOST, 4);
    }

    /// Pool tuning must coexist with both an explicit proxy and a
    /// request-level timeout without conflicting.
    #[test]
    fn pool_options_coexist_with_proxy_and_timeout() {
        let client = build_http_client(Some("http://127.0.0.1:8080"), Some(Duration::from_secs(5)))
            .expect("proxy + timeout + pool options build");
        let _req = client.get("http://127.0.0.1:1/");
    }

    #[test]
    fn invalid_proxy_url_is_rejected() {
        // A string that cannot parse as a URL (even after the implicit
        // `http://` fallback) must fail the build instead of silently
        // producing a client with no proxy.
        assert!(build_http_client(Some("not a url"), None).is_err());
    }
}
