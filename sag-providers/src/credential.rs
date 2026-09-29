//! Southbound credential abstraction for provider adapters.
//!
//! [`CredentialProvider`] is the strategy seam between "which secret do I
//! send upstream" and the adapter request path. Phase 1 ships two
//! implementations compiled into the binary (no dynamic plugins):
//!
//! - [`StaticKeys`] — existing `api_keys` + `key_strategy` selection;
//! - [`OAuthClientCredentials`] — OAuth 2.0 client-credentials (optional
//!   `refresh_token` grant) with in-process token cache, early-expiry
//!   buffer, single-flight fetch, and `invalidate()` for 401 recovery.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use sag_config::{AuthMode, KeyStrategy, OAuthConfig, ProviderConfig};
use sag_core::{AppError, Result};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, warn};

/// Seconds before actual expiry when a cached token is treated as stale.
const TOKEN_EXPIRY_BUFFER: Duration = Duration::from_secs(60);

/// A resolved upstream credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// Static API key. Rendering is per-API-kind:
    /// chat/responses/image → `Authorization: Bearer <key>`;
    /// anthropic_messages → `x-api-key: <key>`.
    Static(String),
    /// OAuth access token → `Authorization: Bearer <token>` (ignores the
    /// kind's static-key shape; Phase 1 rejects anthropic+oauth in config
    /// validation).
    Bearer(String),
}

/// Fetch / refresh provider credentials for the adapter request path.
#[async_trait]
pub trait CredentialProvider: Send + Sync {
    /// Return a usable credential. Implementations own cache-hit vs
    /// refresh decisions internally.
    async fn get_credential(&self) -> Result<Credential>;

    /// Drop any cached credential so the next [`Self::get_credential`]
    /// forces a re-fetch (used on upstream 401).
    async fn invalidate(&self);
}

/// Static `api_keys` selection — the pre-OAuth behavior.
pub struct StaticKeys {
    api_keys: Vec<String>,
    key_strategy: KeyStrategy,
    counter: AtomicUsize,
}

impl StaticKeys {
    pub fn new(api_keys: Vec<String>, key_strategy: KeyStrategy) -> Self {
        Self {
            api_keys,
            key_strategy,
            counter: AtomicUsize::new(0),
        }
    }

    /// Select the current API key based on the configured strategy.
    pub fn current_key(&self) -> &str {
        match self.key_strategy {
            KeyStrategy::FillFirst => &self.api_keys[0],
            KeyStrategy::RoundRobin => {
                let idx = self.counter.fetch_add(1, Ordering::Relaxed) % self.api_keys.len();
                &self.api_keys[idx]
            }
        }
    }
}

#[async_trait]
impl CredentialProvider for StaticKeys {
    async fn get_credential(&self) -> Result<Credential> {
        Ok(Credential::Static(self.current_key().to_string()))
    }

    async fn invalidate(&self) {
        // Static keys have no cache to invalidate.
    }
}

/// OAuth 2.0 client-credentials credential provider.
///
/// Token cache is per-provider, in-process, and not persisted. Hot-reload
/// config changes rebuild the adapter (and thus this object), discarding
/// the old cache. Concurrent cold starts are single-flighted via
/// [`Self::fetch_lock`].
pub struct OAuthClientCredentials {
    provider: String,
    config: OAuthConfig,
    http_client: reqwest::Client,
    cache: RwLock<Option<CachedToken>>,
    fetch_lock: Mutex<()>,
}

struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

impl OAuthClientCredentials {
    pub fn new(provider: String, config: OAuthConfig, http_client: reqwest::Client) -> Self {
        Self {
            provider,
            config,
            http_client,
            cache: RwLock::new(None),
            fetch_lock: Mutex::new(()),
        }
    }

    async fn fetch_token(&self) -> Result<TokenResponse> {
        // Prefer refresh_token grant when configured; fall back to
        // client_credentials (RFC 6749).
        let mut form: Vec<(&str, &str)> = Vec::new();
        if let Some(ref refresh_token) = self.config.refresh_token
            && !refresh_token.trim().is_empty()
        {
            form.push(("grant_type", "refresh_token"));
            form.push(("refresh_token", refresh_token.as_str()));
        } else {
            form.push(("grant_type", "client_credentials"));
        }
        form.push(("client_id", self.config.client_id.as_str()));
        form.push(("client_secret", self.config.client_secret.as_str()));
        if let Some(ref scope) = self.config.scope
            && !scope.trim().is_empty()
        {
            form.push(("scope", scope.as_str()));
        }
        if let Some(ref audience) = self.config.audience
            && !audience.trim().is_empty()
        {
            form.push(("audience", audience.as_str()));
        }

        let response = self
            .http_client
            .post(&self.config.token_url)
            .form(&form)
            .send()
            .await
            .map_err(|e| AppError::CredentialFetch {
                provider: self.provider.clone(),
                status: None,
                message: e.to_string(),
            })?;

        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = response.text().await.unwrap_or_default();
            warn!(
                provider = %self.provider,
                status,
                %body,
                "oauth token endpoint returned error"
            );
            return Err(AppError::CredentialFetch {
                provider: self.provider.clone(),
                status: Some(status),
                message: body,
            });
        }

        let body: serde_json::Value =
            response
                .json()
                .await
                .map_err(|e| AppError::CredentialFetch {
                    provider: self.provider.clone(),
                    status: Some(status),
                    message: format!("invalid token response: {e}"),
                })?;

        let access_token = body
            .get("access_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AppError::CredentialFetch {
                provider: self.provider.clone(),
                status: Some(status),
                message: "token response missing access_token".into(),
            })?
            .to_string();

        // Missing expires_in → assume 1h (common IdP default).
        let expires_in = body
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(3600);

        Ok(TokenResponse {
            access_token,
            expires_in,
        })
    }

    fn fresh_cached(token: TokenResponse) -> CachedToken {
        let lifetime = Duration::from_secs(token.expires_in);
        // Treat the token as expired TOKEN_EXPIRY_BUFFER seconds early.
        // `saturating_sub` → tokens with expires_in <= buffer expire
        // immediately (next get_credential re-fetches).
        let expires_at = Instant::now() + lifetime.saturating_sub(TOKEN_EXPIRY_BUFFER);
        CachedToken {
            access_token: token.access_token,
            expires_at,
        }
    }
}

#[async_trait]
impl CredentialProvider for OAuthClientCredentials {
    async fn get_credential(&self) -> Result<Credential> {
        // Fast path: fresh cache hit.
        {
            let cache = self.cache.read().await;
            if let Some(token) = cache.as_ref()
                && token.expires_at > Instant::now()
            {
                return Ok(Credential::Bearer(token.access_token.clone()));
            }
        }

        // Single-flight: serialize cold fetches so concurrent requests
        // result in exactly one token-endpoint call.
        let _guard = self.fetch_lock.lock().await;

        // Double-check after acquiring the lock — another task may have
        // refreshed while we waited.
        {
            let cache = self.cache.read().await;
            if let Some(token) = cache.as_ref()
                && token.expires_at > Instant::now()
            {
                return Ok(Credential::Bearer(token.access_token.clone()));
            }
        }

        debug!(provider = %self.provider, "fetching oauth access token");
        let token = self.fetch_token().await?;
        let access_token = token.access_token.clone();
        *self.cache.write().await = Some(Self::fresh_cached(token));
        Ok(Credential::Bearer(access_token))
    }

    async fn invalidate(&self) {
        *self.cache.write().await = None;
    }
}

/// Build the credential provider for a provider config.
///
/// `http_client` is reused for token fetches so proxy semantics stay
/// consistent with the adapter's upstream traffic.
pub fn build_credential_provider(
    name: &str,
    config: &ProviderConfig,
    http_client: reqwest::Client,
) -> Result<Arc<dyn CredentialProvider>> {
    match config.auth_mode {
        AuthMode::Static => Ok(Arc::new(StaticKeys::new(
            config.api_keys.clone(),
            config.key_strategy,
        ))),
        AuthMode::OAuth => {
            let oauth = config.oauth.clone().ok_or_else(|| {
                AppError::Config(format!(
                    "provider '{name}': auth_mode='oauth' requires [providers.{name}.oauth]"
                ))
            })?;
            Ok(Arc::new(OAuthClientCredentials::new(
                name.to_string(),
                oauth,
                http_client,
            )))
        }
    }
}

/// Render auth header pairs for a credential + API kind.
///
/// - `Credential::Bearer` → always `Authorization: Bearer <token>`
///   (chat/responses/image; anthropic+oauth is rejected in config).
/// - `Credential::Static` → per-kind shape via
///   [`sag_providers::provider_auth_headers`]-equivalent logic.
pub fn render_auth_headers(
    kind: sag_core::ApiKind,
    credential: &Credential,
) -> Vec<(String, String)> {
    match credential {
        Credential::Bearer(token) => {
            vec![("Authorization".to_string(), format!("Bearer {token}"))]
        }
        Credential::Static(key) => match kind {
            sag_core::ApiKind::AnthropicMessages => vec![
                ("x-api-key".to_string(), key.clone()),
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
            ],
            _ => vec![("Authorization".to_string(), format!("Bearer {key}"))],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn oauth_config(token_url: &str) -> OAuthConfig {
        OAuthConfig {
            token_url: token_url.to_string(),
            client_id: "cid".into(),
            client_secret: "sec".into(),
            scope: Some("scope-a".into()),
            audience: None,
            refresh_token: None,
        }
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    /// Multi-request mock IdP. `hit_count` increments per request; the
    /// handler records each request body (form-urlencoded) and responds
    /// with the configured token payload / status.
    struct MockIdp {
        base_url: String,
        hit_count: Arc<AtomicUsize>,
        bodies: Arc<tokio::sync::Mutex<Vec<String>>>,
    }

    async fn spawn_mock_idp(status: u16, body: String) -> MockIdp {
        let hit_count = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let hits = hit_count.clone();
        let bodies_for_task = bodies.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let hits = hits.clone();
                let bodies = bodies_for_task.clone();
                let status = status;
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let mut req = Vec::new();
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                req.extend_from_slice(&buf[..n]);
                                if req.windows(4).any(|w| w == b"\r\n\r\n") {
                                    if let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n")
                                    {
                                        let head = String::from_utf8_lossy(&req[..pos]);
                                        let body_len = head
                                            .lines()
                                            .find_map(|l| {
                                                let l = l.to_ascii_lowercase();
                                                l.strip_prefix("content-length:")
                                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                            })
                                            .unwrap_or(0);
                                        let have = req.len() - (pos + 4);
                                        if have >= body_len {
                                            break;
                                        }
                                    } else {
                                        break;
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    hits.fetch_add(1, Ordering::SeqCst);
                    if let Some(pos) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                        let body_bytes = &req[pos + 4..];
                        let text = String::from_utf8_lossy(body_bytes).to_string();
                        bodies.lock().await.push(text);
                    }
                    let resp = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.flush().await;
                    drop(stream);
                });
            }
        });
        MockIdp {
            base_url: format!("http://{addr}"),
            hit_count,
            bodies,
        }
    }

    fn token_json(access_token: &str, expires_in: u64) -> String {
        format!(
            r#"{{"access_token":"{access_token}","token_type":"Bearer","expires_in":{expires_in}}}"#
        )
    }

    #[test]
    fn static_keys_fill_first_and_round_robin() {
        let keys = StaticKeys::new(
            vec!["sk-a".into(), "sk-b".into(), "sk-c".into()],
            KeyStrategy::FillFirst,
        );
        assert_eq!(keys.current_key(), "sk-a");
        assert_eq!(keys.current_key(), "sk-a");

        let keys = StaticKeys::new(vec!["sk-a".into(), "sk-b".into()], KeyStrategy::RoundRobin);
        assert_eq!(keys.current_key(), "sk-a");
        assert_eq!(keys.current_key(), "sk-b");
        assert_eq!(keys.current_key(), "sk-a");
    }

    #[tokio::test]
    async fn static_keys_credential_is_static_and_invalidate_is_noop() {
        let keys = Arc::new(StaticKeys::new(vec!["sk-x".into()], KeyStrategy::FillFirst));
        let cred = keys.get_credential().await.unwrap();
        assert_eq!(cred, Credential::Static("sk-x".into()));
        keys.invalidate().await;
        let cred2 = keys.get_credential().await.unwrap();
        assert_eq!(cred2, Credential::Static("sk-x".into()));
    }

    #[test]
    fn render_auth_headers_static_and_bearer() {
        use sag_core::ApiKind;
        let bearer =
            render_auth_headers(ApiKind::ChatCompletions, &Credential::Bearer("tok".into()));
        assert_eq!(bearer, vec![("Authorization".into(), "Bearer tok".into())]);

        // Bearer ignores anthropic kind shape.
        let bearer_anti = render_auth_headers(
            ApiKind::AnthropicMessages,
            &Credential::Bearer("tok".into()),
        );
        assert_eq!(
            bearer_anti,
            vec![("Authorization".into(), "Bearer tok".into())]
        );

        let static_anti = render_auth_headers(
            ApiKind::AnthropicMessages,
            &Credential::Static("sk-ant".into()),
        );
        assert_eq!(
            static_anti,
            vec![
                ("x-api-key".into(), "sk-ant".into()),
                ("anthropic-version".into(), "2023-06-01".into()),
            ]
        );

        let static_chat =
            render_auth_headers(ApiKind::ChatCompletions, &Credential::Static("sk".into()));
        assert_eq!(
            static_chat,
            vec![("Authorization".into(), "Bearer sk".into())]
        );
    }

    #[tokio::test]
    async fn cache_hit_hits_token_endpoint_once() {
        let idp = spawn_mock_idp(200, token_json("tok-1", 3600)).await;
        let oauth = oauth_config(&format!("{}/token", idp.base_url));
        let provider = OAuthClientCredentials::new("test".into(), oauth, client());

        let c1 = provider.get_credential().await.unwrap();
        let c2 = provider.get_credential().await.unwrap();
        assert_eq!(c1, Credential::Bearer("tok-1".into()));
        assert_eq!(c2, Credential::Bearer("tok-1".into()));
        assert_eq!(idp.hit_count.load(Ordering::SeqCst), 1, "cache must hit");
    }

    #[tokio::test]
    async fn expired_token_is_refetched() {
        // expires_in=0 → lifetime 0 → saturating_sub(buffer)=0 →
        // expires_at ≈ now → immediately stale → refetch on next call.
        let idp = spawn_mock_idp(200, token_json("tok-exp", 0)).await;
        let oauth = oauth_config(&format!("{}/token", idp.base_url));
        let provider = OAuthClientCredentials::new("test".into(), oauth, client());

        let _ = provider.get_credential().await.unwrap();
        let _ = provider.get_credential().await.unwrap();
        assert_eq!(
            idp.hit_count.load(Ordering::SeqCst),
            2,
            "expired token must refetch"
        );
    }

    #[tokio::test]
    async fn invalidate_forces_refetch() {
        let idp = spawn_mock_idp(200, token_json("tok-a", 3600)).await;
        let oauth = oauth_config(&format!("{}/token", idp.base_url));
        let provider = OAuthClientCredentials::new("test".into(), oauth, client());

        let c1 = provider.get_credential().await.unwrap();
        assert_eq!(c1, Credential::Bearer("tok-a".into()));
        provider.invalidate().await;
        let c2 = provider.get_credential().await.unwrap();
        // Mock still returns tok-a, but a second fetch must have occurred.
        assert_eq!(c2, Credential::Bearer("tok-a".into()));
        assert_eq!(idp.hit_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn refresh_token_grant_is_preferred() {
        let idp = spawn_mock_idp(200, token_json("tok-rt", 3600)).await;
        let mut oauth = oauth_config(&format!("{}/token", idp.base_url));
        oauth.refresh_token = Some("rt-secret".into());
        let provider = OAuthClientCredentials::new("test".into(), oauth, client());

        let cred = provider.get_credential().await.unwrap();
        assert_eq!(cred, Credential::Bearer("tok-rt".into()));
        let bodies = idp.bodies.lock().await;
        assert_eq!(bodies.len(), 1);
        assert!(
            bodies[0].contains("grant_type=refresh_token"),
            "expected refresh_token grant, body: {}",
            bodies[0]
        );
        assert!(
            bodies[0].contains("refresh_token=rt-secret"),
            "expected refresh_token param, body: {}",
            bodies[0]
        );
    }

    #[tokio::test]
    async fn client_credentials_used_without_refresh_token() {
        let idp = spawn_mock_idp(200, token_json("tok-cc", 3600)).await;
        let oauth = oauth_config(&format!("{}/token", idp.base_url));
        let provider = OAuthClientCredentials::new("test".into(), oauth, client());

        let _ = provider.get_credential().await.unwrap();
        let bodies = idp.bodies.lock().await;
        assert!(bodies[0].contains("grant_type=client_credentials"));
        assert!(bodies[0].contains("client_id=cid"));
        assert!(bodies[0].contains("client_secret=sec"));
        assert!(bodies[0].contains("scope=scope-a"));
    }

    #[tokio::test]
    async fn token_endpoint_http_errors_map_to_credential_fetch() {
        for status in [400u16, 401, 403, 500] {
            let idp = spawn_mock_idp(status, r#"{"error":"bad"}"#.into()).await;
            let oauth = oauth_config(&format!("{}/token", idp.base_url));
            let provider = OAuthClientCredentials::new("azure".into(), oauth, client());

            let err = provider.get_credential().await.unwrap_err();
            match err {
                AppError::CredentialFetch {
                    provider,
                    status: got,
                    message,
                } => {
                    assert_eq!(provider, "azure");
                    assert_eq!(got, Some(status));
                    assert!(!message.is_empty());
                }
                other => panic!("expected CredentialFetch, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn token_endpoint_network_error_maps_to_credential_fetch_status_none() {
        // Bind then drop the listener so the port refuses connections.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let oauth = oauth_config(&format!("http://{addr}/token"));
        let provider = OAuthClientCredentials::new("azure".into(), oauth, client());
        let err = provider.get_credential().await.unwrap_err();
        match err {
            AppError::CredentialFetch {
                provider,
                status,
                message,
            } => {
                assert_eq!(provider, "azure");
                assert_eq!(status, None, "network failure has no HTTP status");
                assert!(!message.is_empty());
            }
            other => panic!("expected CredentialFetch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn concurrent_get_credential_single_flights() {
        let idp = spawn_mock_idp(200, token_json("tok-cf", 3600)).await;
        let oauth = oauth_config(&format!("{}/token", idp.base_url));
        let provider = Arc::new(OAuthClientCredentials::new("test".into(), oauth, client()));

        let mut handles = Vec::new();
        for _ in 0..32 {
            let p = provider.clone();
            handles.push(tokio::spawn(async move { p.get_credential().await }));
        }
        for h in handles {
            let cred = h.await.unwrap().unwrap();
            assert_eq!(cred, Credential::Bearer("tok-cf".into()));
        }
        assert_eq!(
            idp.hit_count.load(Ordering::SeqCst),
            1,
            "32 concurrent cold starts must single-flight to 1 token fetch"
        );
    }

    #[tokio::test]
    async fn missing_access_token_is_credential_fetch() {
        let idp = spawn_mock_idp(200, r#"{"token_type":"Bearer"}"#.into()).await;
        let oauth = oauth_config(&format!("{}/token", idp.base_url));
        let provider = OAuthClientCredentials::new("test".into(), oauth, client());
        let err = provider.get_credential().await.unwrap_err();
        assert!(matches!(err, AppError::CredentialFetch { .. }));
    }

    #[test]
    fn build_credential_provider_dispatches_on_auth_mode() {
        let static_cfg = ProviderConfig {
            api_keys: vec!["sk".into()],
            ..Default::default()
        };
        let p = build_credential_provider("s", &static_cfg, client()).unwrap();
        // Just construct — type check via runtime get.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            assert_eq!(
                p.get_credential().await.unwrap(),
                Credential::Static("sk".into())
            );
        });

        let mut oauth_cfg = ProviderConfig {
            api_keys: vec![],
            auth_mode: AuthMode::OAuth,
            oauth: Some(oauth_config("http://127.0.0.1:9/token")),
            ..Default::default()
        };
        // Empty apis is fine for credential construction.
        oauth_cfg.apis.clear();
        let p2 = build_credential_provider("o", &oauth_cfg, client()).unwrap();
        // Construction succeeds; fetch will fail (unreachable IdP) but the
        // variant is OAuth.
        rt.block_on(async {
            let err = p2.get_credential().await.unwrap_err();
            assert!(matches!(err, AppError::CredentialFetch { .. }));
        });
    }

    #[test]
    fn build_credential_provider_oauth_requires_section() {
        let cfg = ProviderConfig {
            api_keys: vec![],
            auth_mode: AuthMode::OAuth,
            oauth: None,
            ..Default::default()
        };
        let err = match build_credential_provider("o", &cfg, client()) {
            Err(e) => e,
            Ok(_) => panic!("expected Config error when oauth section is missing"),
        };
        assert!(matches!(err, AppError::Config(_)));
    }
}
