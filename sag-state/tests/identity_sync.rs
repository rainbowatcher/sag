//! Integration tests for the `ConfigMutationCoordinator` → `IdentityRegistry`
//! sync wiring.
//!
//! These tests cross crate boundaries (sag-state coordinator ↔ sag-analytics
//! identity registry, backed by an in-memory SQLite fixture; one test also
//! touches the filesystem for the hot-reload commit path), so they live in
//! the integration-test directory per the project conventions (AGENTS.md).

use std::collections::HashMap;
use std::sync::Arc;

use sag_analytics::IdentityRegistry;
use sag_config::{
    ConfigHandle, GatewayConfig, KeyStrategy, ProviderConfig, ProviderType, RouteConfig,
    RouteTarget, Strategy, gateway_config_base,
};
use sag_providers::LlmProvider;
use sag_state::ConfigMutationCoordinator;
use tokio::sync::RwLock;

fn base_provider() -> ProviderConfig {
    ProviderConfig {
        provider_type: ProviderType::OpenAI,
        api_keys: vec!["sk-test".into()],
        key_strategy: KeyStrategy::FillFirst,
        base_url: "https://api.openai.com/v1".into(),
        models_endpoint: "models".into(),
        timeout_secs: 600,
        chunk_timeout_secs: None,
        ttfb_timeout_secs: None,
        headers: HashMap::new(),
        body_overrides: HashMap::new(),
        strip_fields: vec![],
        model_strip_fields: HashMap::new(),
        model_overrides: HashMap::new(),
        role_mappings: HashMap::new(),
        enabled: true,
        enabled_models: vec![],
        anthropic_cache_control: false,
    }
}

fn route(model_name: &str, targets: &[(&str, Option<&str>)]) -> RouteConfig {
    RouteConfig {
        model_name: model_name.into(),
        api_endpoint: "/chat/completions".into(),
        strategy: Strategy::FillFirst,
        retries: None,
        timeout_secs: None,
        targets: targets
            .iter()
            .map(|(provider, real_model)| RouteTarget {
                provider: (*provider).into(),
                real_model: real_model.map(str::to_string),
                priority: 1,
                retries: None,
                timeout_secs: None,
                extra: HashMap::new(),
            })
            .collect(),
    }
}

/// Build an in-memory coordinator; `registry: None` keeps the legacy
/// registry-free behavior.
fn make_coordinator(
    registry: Option<Arc<IdentityRegistry>>,
) -> (Arc<ConfigMutationCoordinator>, ConfigHandle) {
    let config = ConfigHandle::from_config(GatewayConfig {
        ..gateway_config_base!()
    });
    let providers: Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let mut coord = ConfigMutationCoordinator::new(config.clone(), providers);
    if let Some(registry) = registry {
        coord = coord.with_identity_registry(registry);
    }
    (Arc::new(coord), config)
}

/// A registry over a fresh in-memory pool with migrations applied.
async fn in_memory_registry() -> Arc<IdentityRegistry> {
    let pool = sag_analytics::open_analytics_pool(":memory:")
        .await
        .unwrap();
    Arc::new(IdentityRegistry::new(pool))
}

/// A single-connection in-memory pool with a short acquire timeout, for
/// deterministic identity-sync fault injection: holding the pool's only
/// connection makes the registry's next `sync` (which begins a
/// transaction) fail fast with a pool-timeout error; releasing the
/// connection restores health. `:memory:` is safe here because a
/// single-connection pool reuses that one connection, so the database
/// persists across acquire/release cycles.
async fn fault_injectable_pool() -> sqlx::SqlitePool {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    let options = SqliteConnectOptions::from_str(":memory:")
        .unwrap()
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_millis(100))
        .connect_with(options)
        .await
        .unwrap();
    sqlx::migrate!("../sag-analytics/migrations")
        .run(&pool)
        .await
        .unwrap();
    pool
}

#[tokio::test]
async fn create_route_creates_route_and_target_identity() {
    let registry = in_memory_registry().await;
    let (coord, config) = make_coordinator(Some(registry.clone()));
    assert!(
        !registry.is_ready(),
        "no sync may run before any route mutation"
    );

    let report = coord
        .create_route(route("gpt-4", &[("openai", Some("gpt-4-real"))]))
        .await
        .expect("route creation must succeed");
    assert!(report.routes_changed >= 1);

    assert!(
        registry.is_ready(),
        "identity sync must run on route create"
    );
    assert!(
        registry.route_id("gpt-4").is_some(),
        "route identity must exist after create"
    );
    assert!(
        registry
            .target_id("gpt-4", "openai", "gpt-4-real")
            .is_some(),
        "static target identity must exist after create"
    );
    assert_eq!(
        config.routes_version(),
        2,
        "config commit still bumps routes_version"
    );
}

#[tokio::test]
async fn delete_route_deactivates_identity() {
    let registry = in_memory_registry().await;
    let (coord, _config) = make_coordinator(Some(registry.clone()));
    coord
        .create_route(route("gpt-4", &[("openai", Some("gpt-4-real"))]))
        .await
        .unwrap();
    assert!(registry.route_id("gpt-4").is_some());

    coord
        .delete_route("gpt-4")
        .await
        .expect("route deletion must succeed");

    assert_eq!(
        registry.route_id("gpt-4"),
        None,
        "deleted route must be absent from the active cache (active = 0)"
    );
    assert_eq!(
        registry.target_id("gpt-4", "openai", "gpt-4-real"),
        None,
        "targets of a deleted route must be inactive too"
    );
}

#[tokio::test]
async fn recreate_route_reuses_original_identity() {
    let registry = in_memory_registry().await;
    let (coord, _config) = make_coordinator(Some(registry.clone()));
    coord
        .create_route(route("gpt-4", &[("openai", Some("gpt-4-real"))]))
        .await
        .unwrap();
    let first_id = registry
        .route_id("gpt-4")
        .expect("identity must exist after first create");

    coord.delete_route("gpt-4").await.unwrap();
    coord
        .create_route(route("gpt-4", &[("openai", Some("gpt-4-real"))]))
        .await
        .unwrap();

    assert_eq!(
        registry.route_id("gpt-4"),
        Some(first_id),
        "a reappearing route must reuse its original UUID v7"
    );
}

#[tokio::test]
async fn provider_only_mutations_do_not_touch_identities() {
    let registry = in_memory_registry().await;
    let (coord, _config) = make_coordinator(Some(registry.clone()));

    // Seed one route identity, then verify provider-only mutations neither
    // create nor change any identity.
    coord
        .create_route(route("gpt-4", &[("openai", Some("gpt-4-real"))]))
        .await
        .unwrap();
    let route_id = registry.route_id("gpt-4").unwrap();
    let target_id = registry.target_id("gpt-4", "openai", "gpt-4-real").unwrap();

    coord
        .add_provider("p1".into(), base_provider())
        .await
        .expect("add provider must succeed");
    let updated = ProviderConfig {
        api_keys: vec!["sk-test-2".into()],
        ..base_provider()
    };
    coord
        .update_provider("p1", updated)
        .await
        .expect("update provider must succeed");

    assert!(
        registry.is_ready(),
        "registry stays ready after provider-only mutations"
    );
    assert_eq!(
        registry.route_id("gpt-4"),
        Some(route_id),
        "route identity must be unchanged by provider-only mutations"
    );
    assert_eq!(
        registry.target_id("gpt-4", "openai", "gpt-4-real"),
        Some(target_id),
        "target identity must be unchanged by provider-only mutations"
    );
    assert_eq!(
        registry.route_id("claude"),
        None,
        "no identity may be created by provider-only mutations"
    );
}

#[tokio::test]
async fn update_route_reconciles_changed_targets() {
    let registry = in_memory_registry().await;
    let (coord, _config) = make_coordinator(Some(registry.clone()));
    coord
        .create_route(route("gpt-4", &[("openai", Some("gpt-4-real"))]))
        .await
        .unwrap();
    assert!(
        registry
            .target_id("gpt-4", "openai", "gpt-4-real")
            .is_some()
    );

    // Same model_name, different target set: the routes list changed, so the
    // identity sync must deactivate the dropped tuple and create the new one.
    coord
        .update_route(
            "gpt-4",
            route("gpt-4", &[("anthropic", Some("claude-real"))]),
        )
        .await
        .expect("route update must succeed");

    assert_eq!(
        registry.target_id("gpt-4", "openai", "gpt-4-real"),
        None,
        "dropped target must be deactivated"
    );
    assert!(
        registry
            .target_id("gpt-4", "anthropic", "claude-real")
            .is_some(),
        "new target identity must be created"
    );
}

#[tokio::test]
async fn identity_sync_failure_is_observable_and_never_silent() {
    let pool = sag_analytics::open_analytics_pool(":memory:")
        .await
        .unwrap();
    let registry = Arc::new(IdentityRegistry::new(pool.clone()));
    let (coord, config) = make_coordinator(Some(registry));
    let version_before = config.routes_version();

    // Closing the pool makes the registry's next sync fail.
    pool.close().await;

    let err = coord
        .create_route(route("gpt-4", &[("openai", Some("gpt-4-real"))]))
        .await
        .expect_err("a failed identity sync must surface as a mutation error");
    let msg = err.to_string();
    assert!(
        msg.contains("identity sync"),
        "error must name the identity sync: {msg}"
    );
    assert!(
        msg.contains("applied"),
        "error must state that the config commit was applied: {msg}"
    );

    // The config commit is not rolled back — the error makes the
    // half-committed state explicit (config in effect + version bumped,
    // identities stale until a later sync heals them).
    assert_eq!(config.routes_version(), version_before + 1);
    assert!(
        config
            .read()
            .await
            .routes
            .iter()
            .any(|r| r.model_name == "gpt-4"),
        "config commit must still be in effect"
    );
}

#[tokio::test]
async fn failed_route_sync_is_retried_by_later_provider_only_mutation() {
    let pool = fault_injectable_pool().await;
    let registry = Arc::new(IdentityRegistry::new(pool.clone()));
    let (coord, config) = make_coordinator(Some(registry.clone()));

    // Healthy baseline: a route mutation syncs identities.
    coord
        .create_route(route("gpt-4", &[("openai", Some("gpt-4-real"))]))
        .await
        .expect("route creation must succeed on a healthy pool");
    let gpt4_id = registry.route_id("gpt-4").unwrap();

    // Inject failure: hold the pool's only connection. The route commit
    // still goes through, but the identity sync fails (pool timeout) and
    // must surface as an error, not be swallowed.
    let conn = pool.acquire().await.unwrap();
    let err = coord
        .create_route(route("claude", &[("anthropic", Some("claude-real"))]))
        .await
        .expect_err("a failed identity sync must surface as a mutation error");
    assert!(err.to_string().contains("identity sync"));
    assert!(
        config
            .read()
            .await
            .routes
            .iter()
            .any(|r| r.model_name == "claude"),
        "config commit must still be in effect after a failed sync"
    );
    assert!(
        registry.route_id("claude").is_none(),
        "identity must be stale while the sync is failing"
    );

    // While the pool is still blocked, a provider-only mutation (routes
    // unchanged) must ALSO fail: the pending flag forces the retry, and the
    // retry fails again — proving the pending state is set and the retry is
    // actually attempted on provider-only mutations (without pending this
    // mutation would have succeeded silently even with the pool blocked).
    let err = coord
        .add_provider("p1".into(), base_provider())
        .await
        .expect_err("a pending sync must be retried and fail while the pool is blocked");
    assert!(err.to_string().contains("identity sync"));
    assert!(registry.route_id("claude").is_none());

    // Restore health. A provider-only mutation (routes still unchanged)
    // must now retry the pending sync and succeed, healing the identities.
    drop(conn);
    coord
        .add_provider("p2".into(), base_provider())
        .await
        .expect("provider-only mutation with a healthy pool must succeed");
    assert!(
        registry.route_id("claude").is_some(),
        "pending identity sync must be retried by a provider-only mutation"
    );
    assert!(
        registry
            .target_id("claude", "anthropic", "claude-real")
            .is_some(),
        "the retried sync must materialize the route's static targets"
    );
    assert_eq!(
        registry.route_id("gpt-4"),
        Some(gpt4_id),
        "pre-existing identities must be unchanged by the retry"
    );

    // The retry cleared the pending flag: with the pool blocked again, a
    // provider-only mutation must succeed without attempting any sync
    // (no route change, nothing pending) — the retry fires exactly once.
    let conn = pool.acquire().await.unwrap();
    coord
        .add_provider("p3".into(), base_provider())
        .await
        .expect("no sync may run for provider-only mutations once the pending retry is cleared");
    drop(conn);
}

#[tokio::test]
async fn hot_reload_commit_path_syncs_route_identities() {
    let file_path =
        std::env::temp_dir().join(format!("sag_id_sync_reload_{}.toml", std::process::id()));
    let _ = std::fs::remove_file(&file_path);

    let write_config = |routes_toml: &str| {
        std::fs::write(
            &file_path,
            format!(
                r#"
            [server]
            listen = "127.0.0.1:37210"

            [providers.p1]
            type = "openai"
            api_keys = ["sk-test"]
            {routes_toml}
            "#
            ),
        )
        .unwrap();
    };

    // Initial file: no routes.
    write_config("");
    let handle = sag_config::load_config(&file_path).unwrap();
    let providers: Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let registry = in_memory_registry().await;
    let coord = Arc::new(
        ConfigMutationCoordinator::new(handle.clone(), providers)
            .with_identity_registry(registry.clone()),
    );
    assert!(!registry.is_ready(), "no sync before the first reload");

    // File now declares route gpt-4 -> p1 (real_model gpt-4-real): the
    // reload commit path must reconcile identities.
    write_config(
        r#"
            [[routes]]
            model_name = "gpt-4"
            strategy = "fill_first"
            [[routes.targets]]
            provider = "p1"
            real_model = "gpt-4-real"
            priority = 1
    "#,
    );
    coord
        .reload_from_path(&file_path)
        .await
        .expect("reload adding a route must succeed");
    assert!(registry.is_ready());
    assert!(registry.route_id("gpt-4").is_some());
    assert!(registry.target_id("gpt-4", "p1", "gpt-4-real").is_some());

    // File drops the route: the reload commit path must deactivate it.
    write_config("");
    coord
        .reload_from_path(&file_path)
        .await
        .expect("reload removing a route must succeed");
    assert_eq!(
        registry.route_id("gpt-4"),
        None,
        "route removed from the file must be deactivated"
    );
    assert_eq!(
        registry.target_id("gpt-4", "p1", "gpt-4-real"),
        None,
        "its targets must be deactivated too"
    );

    let _ = std::fs::remove_file(&file_path);
}

#[tokio::test]
async fn failed_reload_sync_is_retried_by_same_path_reload() {
    let file_path = std::env::temp_dir().join(format!(
        "sag_id_sync_reload_retry_{}.toml",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&file_path);

    let write_config = |routes_toml: &str| {
        std::fs::write(
            &file_path,
            format!(
                r#"
            [server]
            listen = "127.0.0.1:37210"

            [providers.p1]
            type = "openai"
            api_keys = ["sk-test"]
            {routes_toml}
            "#
            ),
        )
        .unwrap();
    };

    // Initial file: no routes.
    write_config("");
    let handle = sag_config::load_config(&file_path).unwrap();
    let providers: Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>> =
        Arc::new(RwLock::new(HashMap::new()));
    let pool = fault_injectable_pool().await;
    let registry = Arc::new(IdentityRegistry::new(pool.clone()));
    let coord = Arc::new(
        ConfigMutationCoordinator::new(handle.clone(), providers)
            .with_identity_registry(registry.clone()),
    );

    // Block the pool, then reload a file that adds a route: the config
    // commit goes through but the identity sync fails and must surface.
    let conn = pool.acquire().await.unwrap();
    write_config(
        r#"
            [[routes]]
            model_name = "gpt-4"
            strategy = "fill_first"
            [[routes.targets]]
            provider = "p1"
            real_model = "gpt-4-real"
            priority = 1
    "#,
    );
    let err = coord
        .reload_from_path(&file_path)
        .await
        .expect_err("a failed identity sync must surface as a reload error");
    assert!(err.to_string().contains("identity sync"));
    assert!(
        registry.route_id("gpt-4").is_none(),
        "identity must be stale while the sync is failing"
    );

    // Restore health. Re-reading the SAME path — routes unchanged versus
    // the already-committed config — must still retry the pending sync:
    // without pending, this reload would skip the sync entirely.
    drop(conn);
    coord
        .reload_from_path(&file_path)
        .await
        .expect("reload must retry the pending identity sync");
    assert!(
        registry.route_id("gpt-4").is_some(),
        "the retried reload sync must materialize the route identity"
    );
    assert!(
        registry.target_id("gpt-4", "p1", "gpt-4-real").is_some(),
        "the retried reload sync must materialize the static target identity"
    );

    let _ = std::fs::remove_file(&file_path);
}
