//! Identity registry for analytics routes and targets.
//!
//! System-generated UUID v7 identities live **only** in the database:
//! - `route_name` is the stable business identity (in this codebase it is
//!   `RouteConfig.model_name`, an immutable admin-API primary key);
//! - the target identity key is `(route_name, provider, real_model)` —
//!   reordering targets or changing strategy fields keeps the identity,
//!   changing `provider` / `real_model` creates a new one.
//!
//! [`IdentityRegistry::sync`] reconciles the configured routes/targets with
//! the DB (create-or-reuse, `active` / `first_seen` / `last_seen`
//! bookkeeping, never physical deletes) and only afterwards publishes an
//! in-memory read-through cache. Sync must complete successfully before the
//! registry is used for event writing; the writer additionally
//! resolves-or-creates identities inside its own transaction as a fallback
//! (passthrough targets — `real_model = None` — are registered dynamically
//! at first actual call, so they are never statically registered here).
//!
//! The writer's resolution is **strictly additive**: missing identities are
//! created, but existing ones are never re-activated or timestamp-refreshed
//! ([`resolve_or_create_route`] / [`resolve_or_create_target`]). Config sync
//! is the sole authority over `active` / `last_seen`; a late or replayed
//! event must never resurrect a deactivated route.

use sag_core::{AppError, new_uuid_v7};
use sqlx::sqlite::{SqliteConnection, SqlitePool};
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;

/// A target as declared in configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetIdentity {
    pub provider: String,
    /// `None` = passthrough: the effective model is the requested model and
    /// is only known at request time, so it cannot be statically registered.
    pub real_model: Option<String>,
}

/// A route as declared in configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteIdentity {
    /// Stable business identity (equal to `RouteConfig.model_name`).
    pub route_name: String,
    pub targets: Vec<TargetIdentity>,
}

/// In-memory read-through cache of currently active identities.
#[derive(Default)]
struct IdentityCache {
    routes: HashMap<String, Uuid>,
    targets: HashMap<(String, String, String), Uuid>,
}

/// Identity registry: config sync + read-through cache over
/// `analytics_routes` / `analytics_route_targets`.
pub struct IdentityRegistry {
    pool: SqlitePool,
    cache: RwLock<IdentityCache>,
    ready: AtomicBool,
}

impl IdentityRegistry {
    /// Wrap a pool whose schema includes the analytics identity tables
    /// (migrations applied, foreign keys enabled).
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            cache: RwLock::new(IdentityCache::default()),
            ready: AtomicBool::new(false),
        }
    }

    /// Whether a [`Self::sync`] has completed successfully at least once.
    /// Assembly should sync the configuration before wiring event emission,
    /// so that configured identities exist before events reference them.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Resolve `route_name` → `route_id` from the cache (active routes only).
    pub fn route_id(&self, route_name: &str) -> Option<Uuid> {
        self.cache
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .routes
            .get(route_name)
            .copied()
    }

    /// Resolve `(route_name, provider, real_model)` → `target_id` from the
    /// cache (active targets only). Cache misses are not an error: the event
    /// writer falls back to in-transaction resolution-or-creation.
    pub fn target_id(&self, route_name: &str, provider: &str, real_model: &str) -> Option<Uuid> {
        self.cache
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .targets
            .get(&(
                route_name.to_string(),
                provider.to_string(),
                real_model.to_string(),
            ))
            .copied()
    }

    /// Reconcile the configured identities with the database in a single
    /// transaction:
    /// - routes / static targets present in config → create-or-reuse
    ///   (same tuple reappearing reuses the original UUID v7), refresh
    ///   `active = 1` and `last_seen`;
    /// - routes / targets absent from config → `active = 0` (never deleted);
    /// - passthrough targets (`real_model = None`) are skipped (dynamic).
    ///
    /// The in-memory cache is rebuilt from the committed state only after
    /// the transaction succeeds, so a failed sync never publishes a
    /// half-reconciled cache. IDs are never written back to any TOML.
    pub async fn sync(&self, routes: &[RouteIdentity]) -> sag_core::Result<()> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| AppError::Internal(format!("identity sync begin: {e}")))?;

        let configured: HashMap<&str, &RouteIdentity> =
            routes.iter().map(|r| (r.route_name.as_str(), r)).collect();

        // Snapshot existing routes once; reconcile each of them:
        //  - route absent from config → active = 0
        //  - targets absent from the route's config → active = 0
        // (routes removed from config get all their targets flipped too).
        let existing_routes: Vec<(String, String)> =
            sqlx::query_as("SELECT route_id, route_name FROM analytics_routes")
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| AppError::Internal(format!("identity sync load routes: {e}")))?;

        for (route_id, route_name) in &existing_routes {
            // Targets absent from the route's current config → active = 0.
            // Routes removed from config get all their targets flipped too,
            // and routes that now declare only passthrough targets lose
            // their previously static tuples.
            let declared = configured
                .get(route_name.as_str())
                .map(|r| static_targets(r))
                .unwrap_or_default();

            let existing_targets: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT target_id, provider, real_model
                 FROM analytics_route_targets WHERE route_id = ?",
            )
            .bind(route_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| AppError::Internal(format!("identity sync load targets: {e}")))?;

            for (target_id, provider, real_model) in &existing_targets {
                if !declared.contains(&(provider.clone(), real_model.clone())) {
                    sqlx::query(
                        "UPDATE analytics_route_targets SET active = 0 WHERE target_id = ?",
                    )
                    .bind(target_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| {
                        AppError::Internal(format!("identity sync deactivate target: {e}"))
                    })?;
                }
            }

            if !configured.contains_key(route_name.as_str()) {
                sqlx::query("UPDATE analytics_routes SET active = 0 WHERE route_id = ?")
                    .bind(route_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| {
                        AppError::Internal(format!("identity sync deactivate route: {e}"))
                    })?;
            }
        }

        // Upsert configured routes and their static targets (create or
        // reuse; a reappearing tuple reuses the original identity).
        for route in routes {
            let route_id = ensure_route(&mut tx, &route.route_name, now_ms).await?;
            for (provider, real_model) in static_targets(route) {
                ensure_target(&mut tx, route_id, &provider, &real_model, now_ms).await?;
            }
        }

        tx.commit()
            .await
            .map_err(|e| AppError::Internal(format!("identity sync commit: {e}")))?;

        self.rebuild_cache().await?;
        self.ready.store(true, Ordering::Release);
        Ok(())
    }

    /// Rebuild the in-memory cache from the committed DB state
    /// (active routes + their active targets).
    async fn rebuild_cache(&self) -> sag_core::Result<()> {
        let routes: Vec<(String, String)> =
            sqlx::query_as("SELECT route_id, route_name FROM analytics_routes WHERE active = 1")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| AppError::Internal(format!("identity cache load routes: {e}")))?;

        let targets: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT t.target_id, r.route_name, t.provider, t.real_model
             FROM analytics_route_targets t
             JOIN analytics_routes r ON r.route_id = t.route_id
             WHERE t.active = 1 AND r.active = 1",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| AppError::Internal(format!("identity cache load targets: {e}")))?;

        let mut cache = IdentityCache::default();
        for (route_id, route_name) in routes {
            if let Ok(id) = Uuid::parse_str(&route_id) {
                cache.routes.insert(route_name, id);
            }
        }
        for (target_id, route_name, provider, real_model) in targets {
            if let Ok(id) = Uuid::parse_str(&target_id) {
                cache.targets.insert((route_name, provider, real_model), id);
            }
        }

        *self.cache.write().unwrap_or_else(|e| e.into_inner()) = cache;
        Ok(())
    }
}

/// The statically-registerable target tuples of a route
/// (`real_model = None` passthrough targets are skipped).
fn static_targets(route: &RouteIdentity) -> HashSet<(String, String)> {
    route
        .targets
        .iter()
        .filter_map(|t| Some((t.provider.clone(), t.real_model.clone()?)))
        .collect()
}

/// Writer-side resolution-or-creation of a route identity: strictly
/// additive. A missing route is created (`active = 1` on creation, so a
/// never-synced passthrough route still works), but an existing route is
/// returned untouched — `active` / `last_seen` are never re-written here.
/// Config sync remains the sole authority over `active` / `last_seen`; a
/// late or replayed event must never resurrect a deactivated route.
pub(crate) async fn resolve_or_create_route(
    conn: &mut SqliteConnection,
    route_name: &str,
    now_ms: i64,
) -> sag_core::Result<Uuid> {
    let new_id = new_uuid_v7();
    sqlx::query(
        "INSERT INTO analytics_routes (route_id, route_name, active, first_seen, last_seen)
         VALUES (?, ?, 1, ?, ?)
         ON CONFLICT(route_name) DO NOTHING",
    )
    .bind(new_id.to_string())
    .bind(route_name)
    .bind(now_ms)
    .bind(now_ms)
    .execute(&mut *conn)
    .await
    .map_err(|e| AppError::Internal(format!("resolve-or-create route {route_name:?}: {e}")))?;

    let route_id: String =
        sqlx::query_scalar("SELECT route_id FROM analytics_routes WHERE route_name = ?")
            .bind(route_name)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| AppError::Internal(format!("resolve route id for {route_name:?}: {e}")))?;

    parse_uuid(&route_id, "route_id")
}

/// Writer-side resolution-or-creation of a target identity, with the same
/// additive semantics as [`resolve_or_create_route`]: create if missing,
/// never refresh `active` / `last_seen` of an existing row.
pub(crate) async fn resolve_or_create_target(
    conn: &mut SqliteConnection,
    route_id: Uuid,
    provider: &str,
    real_model: &str,
    now_ms: i64,
) -> sag_core::Result<Uuid> {
    let new_id = new_uuid_v7();
    sqlx::query(
        "INSERT INTO analytics_route_targets
             (target_id, route_id, provider, real_model, active, first_seen, last_seen)
         VALUES (?, ?, ?, ?, 1, ?, ?)
         ON CONFLICT(route_id, provider, real_model) DO NOTHING",
    )
    .bind(new_id.to_string())
    .bind(route_id.to_string())
    .bind(provider)
    .bind(real_model)
    .bind(now_ms)
    .bind(now_ms)
    .execute(&mut *conn)
    .await
    .map_err(|e| {
        AppError::Internal(format!(
            "resolve-or-create target ({provider}/{real_model}): {e}"
        ))
    })?;

    let target_id: String = sqlx::query_scalar(
        "SELECT target_id FROM analytics_route_targets
         WHERE route_id = ? AND provider = ? AND real_model = ?",
    )
    .bind(route_id.to_string())
    .bind(provider)
    .bind(real_model)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| AppError::Internal(format!("resolve target id ({provider}/{real_model}): {e}")))?;

    parse_uuid(&target_id, "target_id")
}

/// Resolve-or-create a route identity on the given connection (config sync
/// path; event writing uses [`resolve_or_create_route`]). Reuse is
/// idempotent: a route that already exists keeps its original UUID v7 while
/// `active` / `last_seen` are refreshed.
pub(crate) async fn ensure_route(
    conn: &mut SqliteConnection,
    route_name: &str,
    now_ms: i64,
) -> sag_core::Result<Uuid> {
    let new_id = new_uuid_v7();
    sqlx::query(
        "INSERT INTO analytics_routes (route_id, route_name, active, first_seen, last_seen)
         VALUES (?, ?, 1, ?, ?)
         ON CONFLICT(route_name) DO UPDATE SET active = 1, last_seen = excluded.last_seen",
    )
    .bind(new_id.to_string())
    .bind(route_name)
    .bind(now_ms)
    .bind(now_ms)
    .execute(&mut *conn)
    .await
    .map_err(|e| AppError::Internal(format!("upsert route {route_name:?}: {e}")))?;

    let route_id: String =
        sqlx::query_scalar("SELECT route_id FROM analytics_routes WHERE route_name = ?")
            .bind(route_name)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| AppError::Internal(format!("resolve route id for {route_name:?}: {e}")))?;

    parse_uuid(&route_id, "route_id")
}

/// Resolve-or-create a target identity on the given connection (config sync
/// path; event writing uses [`resolve_or_create_target`]). Same reuse
/// semantics as [`ensure_route`]; `(route_id, provider, real_model)` is the
/// UNIQUE identity key.
pub(crate) async fn ensure_target(
    conn: &mut SqliteConnection,
    route_id: Uuid,
    provider: &str,
    real_model: &str,
    now_ms: i64,
) -> sag_core::Result<Uuid> {
    let new_id = new_uuid_v7();
    sqlx::query(
        "INSERT INTO analytics_route_targets
             (target_id, route_id, provider, real_model, active, first_seen, last_seen)
         VALUES (?, ?, ?, ?, 1, ?, ?)
         ON CONFLICT(route_id, provider, real_model)
         DO UPDATE SET active = 1, last_seen = excluded.last_seen",
    )
    .bind(new_id.to_string())
    .bind(route_id.to_string())
    .bind(provider)
    .bind(real_model)
    .bind(now_ms)
    .bind(now_ms)
    .execute(&mut *conn)
    .await
    .map_err(|e| AppError::Internal(format!("upsert target ({provider}/{real_model}): {e}")))?;

    let target_id: String = sqlx::query_scalar(
        "SELECT target_id FROM analytics_route_targets
         WHERE route_id = ? AND provider = ? AND real_model = ?",
    )
    .bind(route_id.to_string())
    .bind(provider)
    .bind(real_model)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| AppError::Internal(format!("resolve target id ({provider}/{real_model}): {e}")))?;

    parse_uuid(&target_id, "target_id")
}

fn parse_uuid(s: &str, what: &str) -> sag_core::Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| AppError::Internal(format!("invalid {what} in DB: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::test_support::test_pool;

    fn route(name: &str, targets: &[(&str, Option<&str>)]) -> RouteIdentity {
        RouteIdentity {
            route_name: name.to_string(),
            targets: targets
                .iter()
                .map(|(p, m)| TargetIdentity {
                    provider: p.to_string(),
                    real_model: m.map(str::to_string),
                })
                .collect(),
        }
    }

    async fn active_flags(pool: &SqlitePool) -> (Vec<(String, i64)>, Vec<(String, i64)>) {
        let routes: Vec<(String, i64)> =
            sqlx::query_as("SELECT route_name, active FROM analytics_routes")
                .fetch_all(pool)
                .await
                .unwrap();
        let targets: Vec<(String, i64)> =
            sqlx::query_as("SELECT target_id, active FROM analytics_route_targets")
                .fetch_all(pool)
                .await
                .unwrap();
        (routes, targets)
    }

    #[tokio::test]
    async fn sync_creates_identities_and_populates_cache() {
        let pool = test_pool().await;
        let registry = IdentityRegistry::new(pool.clone());
        assert!(!registry.is_ready());

        registry
            .sync(&[route(
                "chat",
                &[("openai", Some("gpt-4o")), ("openai", None)],
            )])
            .await
            .unwrap();

        assert!(registry.is_ready());
        let route_id = registry.route_id("chat").expect("route cached");
        assert_eq!(route_id.get_version_num(), 7);
        let target_id = registry
            .target_id("chat", "openai", "gpt-4o")
            .expect("static target cached");
        assert_eq!(target_id.get_version_num(), 7);

        // Passthrough targets are not statically registered.
        assert!(registry.target_id("chat", "openai", "test-model").is_none());

        // DB state: one route, one static target, both active, UUID v7 text.
        let route_row: (String, i64, i64, i64) =
            sqlx::query_as("SELECT route_id, active, first_seen, last_seen FROM analytics_routes")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(route_row.0, route_id.to_string());
        assert_eq!(route_row.1, 1);
        assert_eq!(route_row.2, route_row.3);
        let target_row: (String, i64) =
            sqlx::query_as("SELECT target_id, active FROM analytics_route_targets")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(target_row.0, target_id.to_string());
        assert_eq!(target_row.1, 1);
    }

    #[tokio::test]
    async fn sync_reuses_existing_ids_and_refreshes_last_seen() {
        let pool = test_pool().await;
        let registry = IdentityRegistry::new(pool.clone());

        registry
            .sync(&[route("chat", &[("openai", Some("gpt-4o"))])])
            .await
            .unwrap();
        let route_id = registry.route_id("chat").unwrap();
        let target_id = registry.target_id("chat", "openai", "gpt-4o").unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        registry
            .sync(&[route("chat", &[("openai", Some("gpt-4o"))])])
            .await
            .unwrap();

        assert_eq!(registry.route_id("chat"), Some(route_id));
        assert_eq!(
            registry.target_id("chat", "openai", "gpt-4o"),
            Some(target_id)
        );

        // Same single rows; last_seen refreshed, first_seen preserved.
        let (route_id2, first_seen, last_seen): (String, i64, i64) =
            sqlx::query_as("SELECT route_id, first_seen, last_seen FROM analytics_routes")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(route_id2, route_id.to_string());
        assert!(last_seen > first_seen);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM analytics_route_targets")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn sync_flips_active_for_removed_route_and_reuses_on_reappearance() {
        let pool = test_pool().await;
        let registry = IdentityRegistry::new(pool.clone());

        registry
            .sync(&[route("chat", &[("openai", Some("gpt-4o"))])])
            .await
            .unwrap();
        let route_id = registry.route_id("chat").unwrap();
        let target_id = registry.target_id("chat", "openai", "gpt-4o").unwrap();

        // Route removed from config → active = 0, cache empty.
        registry.sync(&[]).await.unwrap();
        assert!(registry.route_id("chat").is_none());
        let (routes, targets) = active_flags(&pool).await;
        assert_eq!(routes, vec![("chat".to_string(), 0)]);
        // Targets of a removed route are deactivated too (absent from config).
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0], (target_id.to_string(), 0));

        // Route reappears → same UUID v7 identity, reactivated.
        registry
            .sync(&[route("chat", &[("openai", Some("gpt-4o"))])])
            .await
            .unwrap();
        assert_eq!(registry.route_id("chat"), Some(route_id));
        assert_eq!(
            registry.target_id("chat", "openai", "gpt-4o"),
            Some(target_id)
        );
        let (routes, targets) = active_flags(&pool).await;
        assert_eq!(routes, vec![("chat".to_string(), 1)]);
        assert_eq!(targets, vec![(target_id.to_string(), 1)]);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM analytics_routes")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1,
            "routes are never physically deleted"
        );
    }

    #[tokio::test]
    async fn target_removal_flips_inactive_and_reappearance_reuses_id() {
        let pool = test_pool().await;
        let registry = IdentityRegistry::new(pool.clone());

        registry
            .sync(&[route(
                "chat",
                &[("openai", Some("gpt-4o")), ("openai", Some("gpt-4o-mini"))],
            )])
            .await
            .unwrap();
        let mini_id = registry.target_id("chat", "openai", "gpt-4o-mini").unwrap();

        // Drop one target from the route config; the other stays.
        registry
            .sync(&[route("chat", &[("openai", Some("gpt-4o"))])])
            .await
            .unwrap();
        assert!(
            registry
                .target_id("chat", "openai", "gpt-4o-mini")
                .is_none()
        );
        let flags: Vec<(String, i64)> =
            sqlx::query_as("SELECT target_id, active FROM analytics_route_targets")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(flags.len(), 2, "targets are never physically deleted");
        let mini_flag = flags
            .iter()
            .find(|(id, _)| *id == mini_id.to_string())
            .unwrap();
        assert_eq!(mini_flag.1, 0);
        let gpt4o_flag = flags
            .iter()
            .find(|(id, _)| *id != mini_id.to_string())
            .unwrap();
        assert_eq!(gpt4o_flag.1, 1);

        // Reappearing tuple reuses the original identity.
        registry
            .sync(&[route(
                "chat",
                &[("openai", Some("gpt-4o")), ("openai", Some("gpt-4o-mini"))],
            )])
            .await
            .unwrap();
        assert_eq!(
            registry.target_id("chat", "openai", "gpt-4o-mini"),
            Some(mini_id)
        );
    }
}
