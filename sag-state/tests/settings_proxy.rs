//! Integration tests for the coordinator's settings mutation
//! (`update_settings`) and its proxy hot-update semantics.
//!
//! These tests cross crate boundaries and touch the filesystem (atomic TOML
//! persistence), so they live in the integration-test directory per the
//! project conventions (AGENTS.md).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use sag_providers::LlmProvider;
use sag_state::ConfigMutationCoordinator;
use tokio::sync::RwLock;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_temp_path(prefix: &str) -> std::path::PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir()
        .canonicalize()
        .unwrap_or_else(|_| std::env::temp_dir());
    dir.join(format!("{prefix}_{}_{n}.toml", std::process::id()))
}

/// File-backed config with an `inherit` provider (`p1`) and a `disabled`
/// provider (`p2`) — the two modes a global proxy change must affect
/// differently.
fn initial_toml() -> &'static str {
    r#"
        [server]
        host = "127.0.0.1"
        port = 37210

        [providers.p1]
        api_keys = ["sk-test"]

        [providers.p2]
        api_keys = ["sk-test"]
        proxy_mode = "disabled"
    "#
}

fn make_coordinator(
    path: &std::path::Path,
) -> (Arc<ConfigMutationCoordinator>, sag_config::ConfigHandle) {
    let handle = sag_config::load_config(path).expect("config must load");
    let providers: Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let coord = Arc::new(ConfigMutationCoordinator::new(handle.clone(), providers));
    (coord, handle)
}

fn file_contains(path: &std::path::Path, needle: &str) -> bool {
    std::fs::read_to_string(path)
        .expect("config file must be readable")
        .contains(needle)
}

/// Setting the global proxy persists it to the TOML file, hot-rebuilds the
/// `inherit` provider (effective proxy changed), leaves the `disabled`
/// provider untouched, bumps `routes_version`, and a later URL change /
/// clear keeps following the same rules.
#[tokio::test]
async fn update_settings_persists_and_hot_rebuilds_providers() {
    let path = unique_temp_path("sag_settings_proxy");
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, initial_toml()).unwrap();

    let (coord, handle) = make_coordinator(&path);
    let version_before = handle.routes_version();

    // Set the global proxy: only the inherit provider's effective proxy
    // moves (None -> Some), so only p1 is rebuilt.
    let report = coord
        .update_settings(Some("http://proxy.example:8080".into()))
        .await
        .expect("setting a proxy must succeed");
    assert_eq!(report.providers_rebuilt, vec!["p1"]);
    assert!(
        !report.providers_rebuilt.iter().any(|n| n == "p2"),
        "disabled provider must not rebuild on a global proxy change"
    );
    assert!(
        file_contains(&path, "proxy_url = \"http://proxy.example:8080\""),
        "the global proxy must be persisted to the config file"
    );
    assert_eq!(
        handle.read().await.proxy_url.as_deref(),
        Some("http://proxy.example:8080")
    );
    assert_eq!(
        handle.routes_version(),
        version_before + 1,
        "a settings mutation must bump routes_version exactly once"
    );

    // Change the URL: the inherit provider's effective proxy moves again.
    let report = coord
        .update_settings(Some("http://proxy2.example:8080".into()))
        .await
        .expect("changing the proxy must succeed");
    assert_eq!(report.providers_rebuilt, vec!["p1"]);
    assert!(
        file_contains(&path, "proxy_url = \"http://proxy2.example:8080\""),
        "the new URL must be persisted"
    );

    // Clear the global proxy: p1's effective proxy moves Some -> None
    // (rebuild to direct), p2 stays untouched, and the file key is removed.
    let report = coord
        .update_settings(None)
        .await
        .expect("clearing the proxy must succeed");
    assert_eq!(report.providers_rebuilt, vec!["p1"]);
    assert!(
        !file_contains(&path, "proxy_url"),
        "clearing the proxy must remove the key from the config file"
    );
    assert_eq!(handle.read().await.proxy_url, None);

    let _ = std::fs::remove_file(&path);
}

/// A malformed `proxy_url` (userinfo credentials, unparseable URL) is
/// rejected up front and nothing is persisted.
#[tokio::test]
async fn update_settings_rejects_malformed_proxy_url() {
    let path = unique_temp_path("sag_settings_bad_proxy");
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, initial_toml()).unwrap();

    let (coord, handle) = make_coordinator(&path);
    let version_before = handle.routes_version();

    // Credentials embedded in the proxy URL are rejected (SAG never stores
    // or forwards proxy credentials).
    let err = coord
        .update_settings(Some("http://user:pass@proxy.example:8080".into()))
        .await
        .expect_err("userinfo proxy URL must be rejected");
    assert!(err.to_string().contains("userinfo"));

    // An unparseable URL is rejected too.
    let err = coord
        .update_settings(Some("not a url".into()))
        .await
        .expect_err("unparseable proxy URL must be rejected");
    assert!(err.to_string().contains("proxy"));

    // Nothing was committed: no version bump, no persisted change.
    assert_eq!(
        handle.routes_version(),
        version_before,
        "a rejected mutation must not bump routes_version"
    );
    assert!(
        !file_contains(&path, "proxy_url"),
        "a rejected mutation must not touch the config file"
    );

    let _ = std::fs::remove_file(&path);
}

/// Clearing the global proxy while an `enabled` provider depends on it is a
/// config error: the mutation is rejected and the file keeps the proxy.
#[tokio::test]
async fn update_settings_rejects_clearing_global_while_enabled_provider_exists() {
    let path = unique_temp_path("sag_settings_enabled");
    let _ = std::fs::remove_file(&path);
    std::fs::write(
        &path,
        r#"
            proxy_url = "http://proxy.example:8080"

            [server]
            host = "127.0.0.1"
            port = 37210

            [providers.p1]
            api_keys = ["sk-test"]
            proxy_mode = "enabled"
        "#,
    )
    .unwrap();

    let (coord, handle) = make_coordinator(&path);
    let version_before = handle.routes_version();

    let err = coord
        .update_settings(None)
        .await
        .expect_err("clearing the global proxy with an enabled provider must be rejected");
    assert!(err.to_string().contains("enabled"));

    assert_eq!(
        handle.routes_version(),
        version_before,
        "a rejected mutation must not bump routes_version"
    );
    assert!(
        file_contains(&path, "proxy_url = \"http://proxy.example:8080\""),
        "the rejected mutation must leave the file unchanged"
    );

    let _ = std::fs::remove_file(&path);
}

/// An empty/whitespace-only `proxy_url` normalizes to `None` (no proxy),
/// matching the effective-proxy resolution and the save path (key removed),
/// so in-memory state and the persisted file never diverge.
#[tokio::test]
async fn update_settings_normalizes_blank_proxy_to_none() {
    let path = unique_temp_path("sag_settings_blank");
    let _ = std::fs::remove_file(&path);
    std::fs::write(&path, initial_toml()).unwrap();

    let (coord, handle) = make_coordinator(&path);
    let report = coord
        .update_settings(Some("http://proxy.example:8080".into()))
        .await
        .expect("setting a proxy must succeed");
    assert_eq!(report.providers_rebuilt, vec!["p1"]);

    // A whitespace-only value clears the proxy exactly like `None`.
    let report = coord
        .update_settings(Some("   ".into()))
        .await
        .expect("blank value must clear the proxy");
    assert_eq!(report.providers_rebuilt, vec!["p1"]);
    assert_eq!(
        handle.read().await.proxy_url,
        None,
        "blank proxy must normalize to None in memory"
    );
    assert!(
        !file_contains(&path, "proxy_url"),
        "blank proxy must be removed from the config file"
    );

    let _ = std::fs::remove_file(&path);
}

/// Even with an empty provider set the settings mutation validates the URL
/// up front (the provider sync only resolves proxies for configured
/// providers, so the up-front check is what catches this case).
#[tokio::test]
async fn update_settings_validates_even_with_empty_provider_set() {
    let handle = sag_config::ConfigHandle::from_config(sag_config::GatewayConfig {
        ..sag_config::gateway_config_base!()
    });
    let providers: Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let coord = ConfigMutationCoordinator::new(handle, providers);

    let err = coord
        .update_settings(Some("http://user:pass@proxy.example:8080".into()))
        .await
        .expect_err("malformed URL must be rejected even with no providers");
    assert!(err.to_string().contains("userinfo"));

    // A valid value is accepted (in-memory config: no file to persist).
    let report = coord
        .update_settings(Some("http://proxy.example:8080".into()))
        .await
        .expect("valid settings must be accepted");
    assert!(report.providers_added.is_empty());
    assert!(report.providers_rebuilt.is_empty());
    assert!(report.no_op);
}

/// Hot-reload: an external edit that changes the disk config's top-level
/// `proxy_url` rebuilds the `inherit` provider (its effective proxy moved)
/// and leaves the `disabled` provider untouched. The reload path is
/// read-only — the file on disk is the source of truth and must not be
/// written back, rewritten, or re-serialized.
#[tokio::test]
async fn reload_from_path_rebuilds_only_affected_providers() {
    let path = unique_temp_path("sag_settings_reload");
    let _ = std::fs::remove_file(&path);
    let initial = r#"
        proxy_url = "http://proxy.example:8080"

        [server]
        host = "127.0.0.1"
        port = 37210

        [providers.p1]
        api_keys = ["sk-test"]

        [providers.p2]
        api_keys = ["sk-test"]
        proxy_mode = "disabled"
    "#;
    std::fs::write(&path, initial).unwrap();

    let (coord, handle) = make_coordinator(&path);
    let version_before = handle.routes_version();

    // Simulate an external edit: rewrite the top-level proxy URL on disk.
    let edited = initial.replace(
        "proxy_url = \"http://proxy.example:8080\"",
        "proxy_url = \"http://proxy2.example:8080\"",
    );
    assert_ne!(edited, initial, "the edit must actually change the file");
    std::fs::write(&path, &edited).unwrap();
    // Baseline of the exact edited bytes, so we can prove the reload never
    // writes back / rewrites the file.
    let disk_before_reload = std::fs::read(&path).unwrap();

    // Reload: p1's effective proxy moved (inherit), p2 stays direct
    // (disabled), so only p1 is rebuilt.
    let report = coord
        .reload_from_path(&path)
        .await
        .expect("reload must succeed");
    assert_eq!(report.providers_rebuilt, vec!["p1"]);
    assert!(
        !report.providers_rebuilt.iter().any(|n| n == "p2"),
        "disabled provider must not rebuild on a global proxy change"
    );
    assert!(
        report.providers_added.is_empty() && report.providers_removed.is_empty(),
        "a global proxy change must only rebuild, never add/remove providers"
    );
    assert_eq!(
        handle.read().await.proxy_url.as_deref(),
        Some("http://proxy2.example:8080"),
        "the reloaded config must be live in memory"
    );
    assert_eq!(
        handle.routes_version(),
        version_before + 1,
        "a successful reload must bump routes_version exactly once"
    );

    // The reload path must not have written the file back: no
    // re-serialization, no normalization, no key reordering.
    let disk_after = std::fs::read(&path).unwrap();
    assert_eq!(
        disk_after, disk_before_reload,
        "reload must not write back or rewrite the config file"
    );

    let _ = std::fs::remove_file(&path);
}
