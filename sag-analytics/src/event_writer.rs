//! Single-transaction writer for analytics events.
//!
//! [`EventWriter::write`] persists one [`AnalyticsEvent`] atomically:
//! identity resolution-or-creation, the `request_results` row and all
//! `target_attempts` rows commit together or not at all. Idempotency is
//! keyed on `request_id`:
//! - a request_id that already exists is detected *before* any identity or
//!   fact row is touched and the write returns `Ok` immediately — a replayed
//!   event is a strict no-op and never changes identity `active` /
//!   `last_seen` (config sync is the authority on those);
//! - `INSERT ... ON CONFLICT(request_id) DO NOTHING` backstops concurrent
//!   writers on `request_results`;
//! - `target_attempts` additionally has a DB-level unique index on
//!   `(request_id, target_id)` (migration M1.2) and the writer inserts with
//!   `ON CONFLICT(request_id, target_id) DO NOTHING`, so no code path can
//!   ever turn retries into duplicate attempt rows.
//!
//! Identity resolution is strictly additive (create if missing, never
//! refresh existing rows — see [`crate::registry::resolve_or_create_route`]).
//! Nullability is enforced from the final status — illegal combinations
//! (e.g. `client_error` carrying a route, `no_route` / `all_disabled`
//! carrying target attempts, `success` without a successful attempt) are
//! rejected with an error instead of being silently "fixed". The
//! `(failure_reason, final_status)` pair must follow the authoritative
//! [`sag_core::FailureReason::final_status`] mapping (`success` never
//! carries a reason), and facts must be time-ordered
//! (`completed_at >= started_at`) so negative `duration_ms` can never land.
//! Token / latency `None` values stay NULL in the DB and are never coerced
//! to 0.
//!
//! Late facts are wired into the rollup state machine: when a committed
//! fact's UTC hour bucket is already `completed` in `rollup_bucket_state`,
//! the same transaction sets `dirty = 1` for that hour so the next rollup
//! round recomputes it. Only `completed` buckets are ever touched — no
//! state row is created or modified for `pending` / `processing` /
//! `failed` / unregistered hours.

use crate::event::AnalyticsEvent;
use crate::registry;
use crate::rollup;
use sag_core::{AppError, FinalStatus, TargetStatus};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;
use uuid::Uuid;

/// Single-transaction fact writer over a shared SQLite pool.
///
/// The pool must have foreign keys enabled and the analytics migrations
/// applied — see [`EventWriter::open`], which builds one with the same
/// connect options as [`crate::SqliteLogStore`].
pub struct EventWriter {
    pool: SqlitePool,
}

impl EventWriter {
    /// Wrap a ready pool (foreign keys enabled, migrations applied).
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Open a pool on `dsn` with the same connect options as
    /// [`crate::SqliteLogStore`] (foreign keys enabled, WAL +
    /// `synchronous = NORMAL` for file DSNs) and apply the
    /// migrations. File-backed DSNs share the physical database with an
    /// existing `SqliteLogStore`.
    ///
    /// In-memory DSNs are rejected with an explicit error: they would create
    /// a private, split-brain database invisible to the rest of the gateway
    /// (this covers `:memory:`, `:memory:?cache=shared`, `sqlite::memory:`,
    /// `file::memory:`, `file:...?mode=memory&cache=shared` and their
    /// variants). Callers that need an in-memory database (e.g. tests) must
    /// build one pool and share it via [`EventWriter::new`].
    pub async fn open(dsn: &str) -> Result<Self, sqlx::Error> {
        if is_in_memory_dsn(dsn) {
            return Err(sqlx::Error::config(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "EventWriter::open rejects in-memory DSN {dsn:?}: it would create a \
                     private, split-brain database invisible to the rest of the gateway; \
                     share one pool via EventWriter::new(pool)"
                ),
            )));
        }

        let options = crate::analytics_connect_options(dsn)?;

        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await?;

        sqlx::migrate!("./migrations").run(&pool).await?;

        Ok(Self { pool })
    }

    /// Access to the underlying pool (e.g. for sharing with the identity
    /// registry or for direct verification in tests).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Persist one event in a single SQLite transaction:
    ///
    /// 1. early idempotency check — a `request_id` already present in
    ///    `request_results` returns `Ok(())` immediately, *before* any
    ///    identity upsert or status update, so a replayed event is a strict
    ///    no-op: it never re-activates a deactivated route and never touches
    ///    `active` / `last_seen` (config sync is their sole authority);
    /// 2. resolve-or-create the route identity (`route_name` → `route_id`,
    ///    additive only: missing routes are created, existing rows are
    ///    returned untouched; `None` for `no_route` / `client_error` keeps
    ///    `route_id` NULL);
    /// 3. `INSERT INTO request_results ... ON CONFLICT(request_id)
    ///    DO NOTHING` — `rows_affected() == 0` (only reachable in a
    ///    concurrent-writer race past the check above) skips the attempt
    ///    inserts and late-fact dirty-hour marking;
    /// 4. insert every `target_attempts` row (each target identity resolved
    ///    or created additively, including passthrough dynamic registration)
    ///    with `ON CONFLICT(request_id, target_id) DO NOTHING` — the
    ///    DB-level unique index (migration M1.2) makes duplicate attempt
    ///    rows impossible on any code path;
    /// 5. late-fact marking — every distinct UTC hour touched by the new
    ///    facts (`completed_at / HOUR_MS`, request and attempts) whose
    ///    `rollup_bucket_state` row is already `completed` gets
    ///    `dirty = 1` in the same transaction (its rollup rows are now
    ///    stale); `pending` / `processing` / `failed` buckets pick the
    ///    facts up when computed and hours with no state row stay
    ///    unregistered — the rollup startup scan registers them;
    /// 6. commit.
    ///
    /// The event is validated against the final-status nullability rules,
    /// the `failure_reason` ↔ `final_status` mapping and fact time order
    /// before the transaction begins; illegal combinations error out and
    /// touch nothing.
    pub async fn write(&self, event: AnalyticsEvent) -> sag_core::Result<()> {
        validate(&event)?;

        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut tx = begin_write(&self.pool).await?;

        // Strict no-op for duplicates: a request that already landed must
        // not touch identity state (active / last_seen) or any fact row.
        if already_written(&mut tx, &event).await? {
            return Ok(());
        }

        let route_id = resolve_route_id(&mut tx, &event, now_ms).await?;
        let inserted = insert_request_result(&mut tx, &event, route_id).await?;

        // A concurrent-writer no-op (`rows_affected() == 0`) means no *new*
        // fact landed — skip attempt inserts and late-fact marking.
        if inserted {
            insert_target_attempts(&mut tx, &event, route_id, now_ms).await?;
            mark_dirty_hours(&mut tx, &event, now_ms).await?;
        }

        commit(tx).await
    }
}

/// Open the single write transaction for one event.
///
/// `BEGIN IMMEDIATE`: take the write lock up front instead of a
/// deferred BEGIN that upgrades mid-transaction (which can fail
/// with SQLITE_BUSY_SNAPSHOT under concurrent writers on the same
/// file in WAL mode). The busy_timeout covers the wait.
async fn begin_write(
    pool: &SqlitePool,
) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>, AppError> {
    pool.begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(|e| AppError::Internal(format!("event write begin: {e}")))
}

/// Idempotency gate: true when `request_id` already landed in
/// `request_results`.
async fn already_written(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    event: &AnalyticsEvent,
) -> sag_core::Result<bool> {
    let exists: i64 =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM request_results WHERE request_id = ?)")
            .bind(event.result.request_id.to_string())
            .fetch_one(&mut **tx)
            .await
            .map_err(|e| AppError::Internal(format!("event write idempotency check: {e}")))?;
    Ok(exists != 0)
}

/// Additive identity resolution for the request route: create if missing,
/// never refresh existing rows (config sync is the authority on
/// active/last_seen). Route-less statuses (`no_route` / `client_error`)
/// keep `route_id` NULL.
async fn resolve_route_id(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    event: &AnalyticsEvent,
    now_ms: i64,
) -> sag_core::Result<Option<Uuid>> {
    match event.result.route_name.as_deref() {
        Some(name) => Ok(Some(
            registry::resolve_or_create_route(tx, name, now_ms).await?,
        )),
        None => Ok(None),
    }
}

/// Persist the `request_results` row (idempotent by `request_id`).
/// Returns whether this call actually inserted a new row — `false` is a
/// concurrent-writer race past the idempotency check.
async fn insert_request_result(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    event: &AnalyticsEvent,
    route_id: Option<Uuid>,
) -> sag_core::Result<bool> {
    let (prompt_tokens, completion_tokens, total_tokens) =
        token_tuple(is_success(&event.result.final_status), &event.result.usage);
    let result = sqlx::query(
        r#"
        INSERT INTO request_results (
            request_id, route_id, final_status, failure_reason, requested_model,
            actual_model, stream, started_at, completed_at, duration_ms,
            prompt_tokens, completion_tokens, total_tokens,
            cached_tokens, cache_creation_tokens, ttft_ms, upstream_duration_ms
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(request_id) DO NOTHING
        "#,
    )
    .bind(event.result.request_id.to_string())
    .bind(route_id.map(|id| id.to_string()))
    .bind(final_status_str(event.result.final_status))
    .bind(event.result.failure_reason.map(|r| r.as_str()))
    .bind(&event.result.requested_model)
    .bind(actual_model(event))
    .bind(event.result.stream as i32)
    .bind(event.result.started_at.timestamp_millis())
    .bind(event.result.completed_at.timestamp_millis())
    .bind(duration_ms(
        event.result.completed_at,
        event.result.started_at,
    ))
    .bind(prompt_tokens)
    .bind(completion_tokens)
    .bind(total_tokens)
    .bind(
        event
            .result
            .cached_tokens
            .filter(|_| is_success(&event.result.final_status))
            .map(|v| v as i64),
    )
    .bind(
        event
            .result
            .cache_creation_tokens
            .filter(|_| is_success(&event.result.final_status))
            .map(|v| v as i64),
    )
    .bind(event.result.ttft_ms.map(|v| v as i64))
    .bind(event.result.upstream_duration_ms.map(|v| v as i64))
    .execute(&mut **tx)
    .await
    .map_err(|e| AppError::Internal(format!("event write request_results: {e}")))?;
    Ok(result.rows_affected() > 0)
}

/// Persist every `target_attempts` row. No-op for an empty `targets` list.
///
/// Only route-bearing events can carry attempts (enforced by `validate`);
/// a valid `no_route` / `client_error` event has no targets and succeeds
/// with `route_id` NULL instead.
async fn insert_target_attempts(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    event: &AnalyticsEvent,
    route_id: Option<Uuid>,
    now_ms: i64,
) -> sag_core::Result<()> {
    if event.targets.is_empty() {
        return Ok(());
    }

    let route_id = route_id.ok_or_else(|| {
        AppError::Internal(
            "event write: target attempts require a resolved route (validation missed it)"
                .to_string(),
        )
    })?;

    for attempt in &event.targets {
        let target_id = registry::resolve_or_create_target(
            tx,
            route_id,
            &attempt.provider,
            &attempt.real_model,
            now_ms,
        )
        .await?;

        let (prompt_tokens, completion_tokens, total_tokens) =
            token_tuple(attempt.status == TargetStatus::Success, &attempt.usage);
        sqlx::query(
            r#"
            INSERT INTO target_attempts (
                request_id, target_id, route_id, actual_model, status,
                failure_reason, stream,
                started_at, completed_at, duration_ms,
                prompt_tokens, completion_tokens, total_tokens,
                cached_tokens, ttft_ms, upstream_duration_ms
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(request_id, target_id) DO NOTHING
            "#,
        )
        .bind(event.result.request_id.to_string())
        .bind(target_id.to_string())
        .bind(route_id.to_string())
        .bind(&attempt.real_model)
        .bind(target_status_str(attempt.status))
        .bind(attempt.failure_reason.map(|r| r.as_str()))
        .bind(event.result.stream as i32)
        .bind(attempt.started_at.timestamp_millis())
        .bind(attempt.completed_at.timestamp_millis())
        .bind(duration_ms(attempt.completed_at, attempt.started_at))
        .bind(prompt_tokens)
        .bind(completion_tokens)
        .bind(total_tokens)
        .bind(
            attempt
                .cached_tokens
                .filter(|_| attempt.status == TargetStatus::Success)
                .map(|v| v as i64),
        )
        .bind(attempt.ttft_ms.map(|v| v as i64))
        .bind(attempt.upstream_duration_ms.map(|v| v as i64))
        .execute(&mut **tx)
        .await
        .map_err(|e| AppError::Internal(format!("event write target_attempts: {e}")))?;
    }
    Ok(())
}

/// Late-fact marking: a newly committed fact whose UTC hour bucket is
/// already `completed` makes that bucket's rollup rows stale, so mark
/// the hour `dirty = 1` in the *same* transaction — the next rollup
/// round recomputes it. Only `completed` buckets are touched:
/// pending / processing / failed pick the fact up when computed, and
/// hours with no state row must not be created here (the rollup
/// startup scan registers hours with facts itself).
async fn mark_dirty_hours(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    event: &AnalyticsEvent,
    now_ms: i64,
) -> sag_core::Result<()> {
    let mut hours = Vec::new();
    hours.push((event.result.completed_at.timestamp_millis() / rollup::HOUR_MS) * rollup::HOUR_MS);
    for attempt in &event.targets {
        hours.push((attempt.completed_at.timestamp_millis() / rollup::HOUR_MS) * rollup::HOUR_MS);
    }
    hours.sort_unstable();
    hours.dedup();
    for hour in hours {
        sqlx::query(
            "UPDATE rollup_bucket_state SET dirty = 1, updated_at = ? \
             WHERE hour = ? AND state = 'completed'",
        )
        .bind(now_ms)
        .bind(hour)
        .execute(&mut **tx)
        .await
        .map_err(|e| AppError::Internal(format!("event write mark dirty hour={hour}: {e}")))?;
    }
    Ok(())
}

async fn commit(tx: sqlx::Transaction<'_, sqlx::Sqlite>) -> sag_core::Result<()> {
    tx.commit()
        .await
        .map_err(|e| AppError::Internal(format!("event write commit: {e}")))?;
    Ok(())
}

/// Whether SQLite would resolve `dsn` to an in-memory (or otherwise
/// per-connection private) database. [`EventWriter::open`] must reject these
/// DSNs up front: each new connection would get its *own* database, so the
/// pool and the rest of the gateway would silently write into separate,
/// split-brain databases. Normal file DSNs (plain paths, `file:...` URIs,
/// `sqlite:` URLs) are unaffected. Crate-visible: the shared connect-options
/// helper uses it to skip WAL for in-memory databases.
///
/// SQLite itself recognizes the following spellings (see
/// <https://www.sqlite.org/uri.html> and
/// <https://www.sqlite.org/inmemorydb.html>):
/// - the filename `:memory:` — with or without URI query parameters
///   (`:memory:`, `:memory:?cache=shared`);
/// - `file:` URIs whose path is `:memory:` (`file::memory:`,
///   `file::memory:?cache=shared`), whose path is empty (`file:` — a
///   private temporary database per connection), or whose query carries
///   `mode=memory` (`file:memdb1?mode=memory&cache=shared`);
/// - an empty filename — also a private per-connection temporary database.
///
/// A trailing fragment (`#...`) is ignored, and `file:` URIs are
/// percent-decoded before classifying — exactly as SQLite interprets them —
/// so `file:memdb1?mode=memory#frag` and `file:%3Amemory%3A` /
/// `file:memdb1?mode%3Dmemory` are rejected too, while ordinary
/// percent-encoded file names (`file:my%20db.db`) are not affected.
///
/// `sqlite:` URLs additionally run through sqlx's own parser first, whose
/// parse result validates the URL while the raw query is inspected with the
/// same percent-decoded form semantics sqlx applies (its rebuilt canonical
/// URL is not used — it panics for filenames containing `:`); a parse
/// error is not classified here and surfaces from `open` itself.
pub(crate) fn is_in_memory_dsn(dsn: &str) -> bool {
    // Raw filename semantics (the `sqlite:` scheme is not part of the
    // filename, mirroring sqlx's own prefix stripping): SQLite parses a
    // filename as a URI when it starts with `file:` or `:`. Split off the
    // fragment and query, then classify like SQLite's URI parser does.
    let raw = dsn
        .trim_start_matches("sqlite://")
        .trim_start_matches("sqlite:");
    // A fragment (`#...`) never changes database semantics, so it must not
    // hide a `mode=memory` query (`file:memdb1?mode=memory#frag`).
    let raw = match raw.find('#') {
        Some(i) => &raw[..i],
        None => raw,
    };
    let (path, query) = match raw.find('?') {
        Some(i) => (&raw[..i], &raw[i + 1..]),
        None => (raw, ""),
    };

    // `sqlite:` URL form: sqlx's parser is the authority on how the URL
    // splits into filename and query parameters (percent-decoded form
    // encoding), so its parse result validates the URL while the raw query
    // is inspected with the same semantics. sqlx's rebuilt canonical URL
    // (`to_url_lossy`) is not consulted: it panics for filenames containing
    // `:` (e.g. `sqlite:file:memdb1?...`). (Fall through afterwards:
    // `sqlite:file::memory:` parses as a non-memory filename, but SQLite
    // still resolves it to an in-memory database at connect time.)
    if dsn.starts_with("sqlite:")
        && SqliteConnectOptions::from_str(dsn).is_ok()
        && query.split('&').any(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            percent_decode(k) == "mode" && percent_decode(v) == "memory"
        })
    {
        return true;
    }

    if path == ":memory:" || path.is_empty() {
        // `:memory:` (private or shared) and the empty filename (private
        // per-connection temporary database) — both split-brain hazards.
        return true;
    }
    if let Some(file_path) = path.strip_prefix("file:") {
        // SQLite percent-decodes `file:` URIs before interpreting them, so
        // `file:%3Amemory%3A` and `file:memdb1?mode%3Dmemory` resolve to
        // in-memory databases just like their unescaped spellings.
        let file_path = percent_decode(file_path);
        let query = percent_decode(query);
        if file_path == ":memory:"
            || file_path.is_empty()
            || query.split('&').any(|kv| kv == "mode=memory")
        {
            return true;
        }
    }
    false
}

/// Percent-decode `%XX` escapes (RFC 3986) the way SQLite's `file:` URI
/// parser does. `+` is left as a literal plus (it only means space in form
/// encoding, which SQLite does not apply), and malformed escapes are left
/// untouched so ordinary filenames never change meaning.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Token columns for a fact: only final success carries non-NULL values;
/// failure / cancelled must never be stored as 0 (they stay NULL).
fn token_tuple(
    success: bool,
    usage: &Option<sag_core::Usage>,
) -> (Option<i64>, Option<i64>, Option<i64>) {
    if success {
        usage
            .as_ref()
            .map(|u| {
                (
                    Some(u.prompt_tokens as i64),
                    Some(u.completion_tokens as i64),
                    Some(u.total_tokens as i64),
                )
            })
            .unwrap_or((None, None, None))
    } else {
        (None, None, None)
    }
}

/// `actual_model` for `request_results`: only `success` carries the model of
/// the final successful attempt (derived from its `real_model`).
fn actual_model(event: &AnalyticsEvent) -> Option<String> {
    if event.result.final_status == FinalStatus::Success {
        event
            .targets
            .iter()
            .rev()
            .find(|t| t.status == TargetStatus::Success)
            .map(|t| t.real_model.clone())
    } else {
        None
    }
}

/// `duration_ms = completed_at - started_at` (spec invariant).
fn duration_ms(
    completed: chrono::DateTime<chrono::Utc>,
    started: chrono::DateTime<chrono::Utc>,
) -> i64 {
    (completed - started).num_milliseconds()
}

fn is_success(status: &FinalStatus) -> bool {
    *status == FinalStatus::Success
}

fn final_status_str(status: FinalStatus) -> &'static str {
    match status {
        FinalStatus::Success => "success",
        FinalStatus::ClientError => "client_error",
        FinalStatus::NoRoute => "no_route",
        FinalStatus::AllDisabled => "all_disabled",
        FinalStatus::ModelFailure => "model_failure",
        FinalStatus::ClientCancelled => "client_cancelled",
    }
}

fn target_status_str(status: TargetStatus) -> &'static str {
    match status {
        TargetStatus::Success => "success",
        TargetStatus::Failure => "failure",
        TargetStatus::Cancelled => "cancelled",
    }
}

/// Enforce the final-status ↔ identity nullability contract before writing.
///
/// - `no_route` / `client_error` → no route, no target attempts;
/// - `all_disabled` → a route is required, but no target attempts (nothing
///   was attempted);
/// - every other status → a route is required;
/// - `success` → at least one successful target attempt (the source of
///   `actual_model`); `client_cancelled` may carry 0..n attempts — an
///   in-flight attempt is recorded, a cancel before any attempt is not.
///
/// On top of nullability, the writer is the last data-integrity line for
/// the failure classification and fact time order:
/// - `failure_reason` must map to `final_status` via the authoritative
///   [`sag_core::FailureReason::final_status`] table, and `success` never
///   carries a reason;
/// - every fact must be time-ordered (`completed_at >= started_at`, equal
///   is legal) so negative `duration_ms` can never land.
///
/// Illegal combinations return an error and write nothing; they are never
/// silently corrected.
fn validate(event: &AnalyticsEvent) -> sag_core::Result<()> {
    let request_id = event.result.request_id;
    let status = event.result.final_status;

    let invalid = |detail: String| {
        AppError::Internal(format!(
            "invalid analytics event {request_id} ({status:?}): {detail}"
        ))
    };

    match status {
        FinalStatus::NoRoute | FinalStatus::ClientError => {
            if event.result.route_name.is_some() {
                return Err(invalid(
                    "no_route/client_error must not carry a route_name".to_string(),
                ));
            }
            if !event.targets.is_empty() {
                return Err(invalid(
                    "no_route/client_error cannot have target attempts".to_string(),
                ));
            }
        }
        FinalStatus::AllDisabled => {
            if event.result.route_name.is_none() {
                return Err(invalid("all_disabled requires a route_name".to_string()));
            }
            if !event.targets.is_empty() {
                return Err(invalid(
                    "all_disabled cannot have target attempts (nothing was attempted)".to_string(),
                ));
            }
        }
        _ => {
            if event.result.route_name.is_none() {
                return Err(invalid(
                    "this final status requires a route_name".to_string(),
                ));
            }
        }
    }

    if status == FinalStatus::Success
        && !event
            .targets
            .iter()
            .any(|t| t.status == TargetStatus::Success)
    {
        return Err(invalid(
            "success requires at least one successful target attempt".to_string(),
        ));
    }

    // `Success` never carries a failure_reason — no reason maps to it.
    if status == FinalStatus::Success && event.result.failure_reason.is_some() {
        return Err(invalid(
            "success must not carry a failure_reason".to_string(),
        ));
    }

    // Any other reason must map to the final status via the authoritative
    // `FailureReason::final_status` table. Unregistered combinations (e.g.
    // `client_cancelled` + `invalid_request`, `client_error` +
    // `all_exhausted`) are data-integrity violations — reject loudly, never
    // silently "fix" or downgrade.
    if let Some(reason) = event.result.failure_reason {
        let mapped = reason.final_status();
        if mapped != status {
            return Err(invalid(format!(
                "failure_reason {reason:?} maps to {mapped:?}, not {status:?}"
            )));
        }
    }

    // Facts must be time-ordered: `completed_at >= started_at` (equal is
    // legal — e.g. an instant rejection). Negative durations would corrupt
    // `duration_ms` and must never land.
    if event.result.completed_at < event.result.started_at {
        return Err(invalid(
            "request fact completed_at must not be earlier than started_at".to_string(),
        ));
    }
    for attempt in &event.targets {
        if attempt.completed_at < attempt.started_at {
            return Err(invalid(format!(
                "target attempt {provider} completed_at must not be earlier than started_at",
                provider = attempt.provider
            )));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::test_support::{
        REF_TS_MS, make_attempt, make_event, make_success_usage, test_pool,
    };
    use crate::rollup::{BucketState, BucketStatus, HOUR_MS, RollupEngine};
    use chrono::{Duration, TimeZone};
    use sag_core::{FailureReason, TargetAttemptFact, TargetStatus, Usage};
    use uuid::Uuid;

    #[tokio::test]
    async fn write_persists_request_and_all_attempts_atomically() {
        let pool = test_pool().await;
        let writer = EventWriter::new(pool.clone());

        let request_id = Uuid::new_v4();
        let mut event = make_event(
            request_id,
            FinalStatus::Success,
            Some("chat"),
            vec![
                make_attempt("openai", "gpt-4o", TargetStatus::Failure),
                make_attempt("openai", "gpt-4o-mini", TargetStatus::Success),
            ],
        );
        event.result.usage = Some(make_success_usage());
        event.result.cached_tokens = Some(5);

        writer.write(event).await.unwrap();

        // request_results: exactly one row with success semantics.
        let row: (String, String, String, i64, i64, i64) = sqlx::query_as(
            "SELECT route_id, final_status, actual_model, stream, started_at, duration_ms
             FROM request_results",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let route_id = Uuid::parse_str(&row.0).unwrap();
        assert_eq!(route_id.get_version_num(), 7);
        assert_eq!(row.1, "success");
        // actual_model derived from the final successful attempt.
        assert_eq!(row.2, "gpt-4o-mini");
        assert_eq!(row.3, 0);
        assert_eq!(row.4, crate::event::test_support::REF_TS_MS);
        assert_eq!(row.5, 250);

        // target_attempts: all attempts persisted with correct statuses.
        let attempts: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT actual_model, status, duration_ms FROM target_attempts ORDER BY actual_model",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].0, "gpt-4o");
        assert_eq!(attempts[0].1, "failure");
        assert_eq!(attempts[1].0, "gpt-4o-mini");
        assert_eq!(attempts[1].1, "success");
        assert_eq!(attempts[0].2, 200);
    }

    #[tokio::test]
    async fn request_and_attempts_commit_or_rollback_together() {
        let pool = test_pool().await;
        let writer = EventWriter::new(pool.clone());

        // A no_route event is rejected; nothing may be persisted.
        let err = writer
            .write(make_event(
                Uuid::new_v4(),
                FinalStatus::NoRoute,
                Some("chat"),
                vec![],
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must not carry a route_name"));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM request_results")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn duplicate_request_id_does_not_duplicate_attempts() {
        let pool = test_pool().await;
        let writer = EventWriter::new(pool.clone());

        let request_id = Uuid::new_v4();
        let event = || {
            make_event(
                request_id,
                FinalStatus::Success,
                Some("chat"),
                vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
            )
        };

        writer.write(event()).await.unwrap();
        writer.write(event()).await.unwrap();
        writer.write(event()).await.unwrap();

        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM request_results")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM target_attempts")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn illegal_status_route_combinations_are_rejected() {
        let pool = test_pool().await;
        let writer = EventWriter::new(pool.clone());

        let cases: Vec<(FinalStatus, Option<&str>, Vec<TargetAttemptFact>)> = vec![
            // client_error with a route.
            (FinalStatus::ClientError, Some("chat"), vec![]),
            // no_route with target attempts.
            (
                FinalStatus::NoRoute,
                None,
                vec![make_attempt("openai", "gpt-4o", TargetStatus::Failure)],
            ),
            // success without a route.
            (
                FinalStatus::Success,
                None,
                vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
            ),
            // success without a successful attempt.
            (
                FinalStatus::Success,
                Some("chat"),
                vec![make_attempt("openai", "gpt-4o", TargetStatus::Failure)],
            ),
            // all_disabled without a route.
            (FinalStatus::AllDisabled, None, vec![]),
            // all_disabled with target attempts (nothing was attempted).
            (
                FinalStatus::AllDisabled,
                Some("chat"),
                vec![make_attempt("openai", "gpt-4o", TargetStatus::Failure)],
            ),
        ];

        for (status, route_name, targets) in cases {
            let event = make_event(Uuid::new_v4(), status, route_name, targets);
            let err = writer.write(event).await.unwrap_err();
            assert!(
                err.to_string().contains("invalid analytics event"),
                "unexpected error for {status:?}: {err}"
            );
        }

        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM request_results")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM target_attempts")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn illegal_failure_reason_status_combinations_are_rejected() {
        let pool = test_pool().await;
        let writer = EventWriter::new(pool.clone());

        // failure_reason must map to final_status via the authoritative
        // FailureReason::final_status table; every other combination errors
        // out and must not write any identity or fact row.
        let mut client_cancelled = make_event(
            Uuid::new_v4(),
            FinalStatus::ClientCancelled,
            Some("chat"),
            vec![make_attempt("openai", "gpt-4o", TargetStatus::Cancelled)],
        );
        client_cancelled.result.failure_reason = Some(FailureReason::InvalidRequest);

        let mut success = make_event(
            Uuid::new_v4(),
            FinalStatus::Success,
            Some("chat"),
            vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
        );
        success.result.failure_reason = Some(FailureReason::Timeout);

        let mut client_error = make_event(Uuid::new_v4(), FinalStatus::ClientError, None, vec![]);
        client_error.result.failure_reason = Some(FailureReason::AllExhausted);

        for (event, expect) in [
            // client_cancelled + invalid_request → invalid_request maps to
            // ClientError, not ClientCancelled.
            (client_cancelled, "maps to ClientError"),
            // success + timeout → success never carries a failure_reason.
            (success, "must not carry a failure_reason"),
            // client_error + all_exhausted → all_exhausted maps to
            // ModelFailure, not ClientError.
            (client_error, "maps to ModelFailure"),
        ] {
            let err = writer.write(event).await.unwrap_err();
            assert!(
                err.to_string().contains("invalid analytics event"),
                "unexpected error: {err}"
            );
            assert!(
                err.to_string().contains(expect),
                "expected {expect:?} in error detail, got: {err}"
            );
        }

        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM request_results")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM target_attempts")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn legal_failure_reason_status_combinations_persist() {
        let pool = test_pool().await;
        let writer = EventWriter::new(pool.clone());

        // Legal client_cancelled without any target attempt.
        let mut cancelled = make_event(
            Uuid::new_v4(),
            FinalStatus::ClientCancelled,
            Some("chat"),
            vec![],
        );
        cancelled.result.failure_reason = Some(FailureReason::ClientCancelled);
        writer.write(cancelled).await.unwrap();

        // Legal model_failure with attempts and a matching reason.
        let mut model_failure = make_event(
            Uuid::new_v4(),
            FinalStatus::ModelFailure,
            Some("chat"),
            vec![make_attempt("openai", "gpt-4o", TargetStatus::Failure)],
        );
        model_failure.result.failure_reason = Some(FailureReason::AllExhausted);
        writer.write(model_failure).await.unwrap();

        let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT final_status, failure_reason, route_id FROM request_results
             ORDER BY final_status",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "client_cancelled");
        assert_eq!(rows[0].1.as_deref(), Some("client_cancelled"));
        assert!(rows[0].2.is_some(), "client_cancelled keeps its route");
        assert_eq!(rows[1].0, "model_failure");
        assert_eq!(rows[1].1.as_deref(), Some("all_exhausted"));

        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM target_attempts")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1,
            "the model_failure attempt must persist"
        );
    }

    #[tokio::test]
    async fn completed_at_before_started_at_is_rejected() {
        let pool = test_pool().await;
        let writer = EventWriter::new(pool.clone());

        // Request fact out of order: completed_at < started_at.
        let mut event = make_event(
            Uuid::new_v4(),
            FinalStatus::Success,
            Some("chat"),
            vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
        );
        event.result.completed_at = event.result.started_at - Duration::milliseconds(1);
        let err = writer.write(event).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("completed_at must not be earlier than started_at"),
            "unexpected error: {err}"
        );

        // Target attempt fact out of order.
        let mut event = make_event(
            Uuid::new_v4(),
            FinalStatus::ModelFailure,
            Some("chat"),
            vec![make_attempt("openai", "gpt-4o", TargetStatus::Failure)],
        );
        event.targets[0].completed_at = event.targets[0].started_at - Duration::milliseconds(1);
        let err = writer.write(event).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("completed_at must not be earlier than started_at"),
            "unexpected error: {err}"
        );

        // Neither rejection may write any row.
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM request_results")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM target_attempts")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );

        // Equal timestamps are legal and persist as duration_ms = 0.
        let mut equal = make_event(Uuid::new_v4(), FinalStatus::NoRoute, None, vec![]);
        equal.result.completed_at = equal.result.started_at;
        writer.write(equal).await.unwrap();
        let duration: i64 = sqlx::query_scalar("SELECT duration_ms FROM request_results")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(duration, 0, "equal timestamps must persist as 0ms");
    }

    #[tokio::test]
    async fn null_tokens_and_latency_stay_null_never_zero() {
        let pool = test_pool().await;
        let writer = EventWriter::new(pool.clone());

        // Success event with no usage / no cached tokens / no latency.
        let mut success = make_event(
            Uuid::new_v4(),
            FinalStatus::Success,
            Some("chat"),
            vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
        );
        success.result.usage = None;
        success.result.cached_tokens = None;
        success.result.cache_creation_tokens = None;
        success.result.ttft_ms = None;
        success.result.upstream_duration_ms = None;
        writer.write(success).await.unwrap();

        type LatencyRow = (
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        );
        let req: LatencyRow = sqlx::query_as(
            "SELECT prompt_tokens, completion_tokens, total_tokens,
                        cached_tokens, ttft_ms, upstream_duration_ms
                 FROM request_results",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            req,
            (None, None, None, None, None, None),
            "NULLs must stay NULL, never 0"
        );

        // A failed attempt carrying usage must NOT persist tokens as 0.
        let mut failed = make_event(
            Uuid::new_v4(),
            FinalStatus::ModelFailure,
            Some("chat"),
            vec![make_attempt("openai", "gpt-4o", TargetStatus::Failure)],
        );
        failed.targets[0].usage = Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 20,
            total_tokens: 30,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        });
        writer.write(failed).await.unwrap();

        let att: (Option<i64>, Option<i64>, Option<i64>) = sqlx::query_as(
            "SELECT prompt_tokens, completion_tokens, total_tokens FROM target_attempts",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            att,
            (None, None, None),
            "failed attempts must not store tokens (not even 0)"
        );
    }

    #[tokio::test]
    async fn writer_reuses_registry_synced_identities() {
        use crate::registry::{IdentityRegistry, RouteIdentity, TargetIdentity};

        let pool = test_pool().await;
        let registry = IdentityRegistry::new(pool.clone());
        registry
            .sync(&[RouteIdentity {
                route_name: "chat".to_string(),
                targets: vec![TargetIdentity {
                    provider: "openai".to_string(),
                    real_model: Some("gpt-4o".to_string()),
                }],
            }])
            .await
            .unwrap();
        let synced_route_id = registry.route_id("chat").unwrap();
        let synced_target_id = registry.target_id("chat", "openai", "gpt-4o").unwrap();

        let writer = EventWriter::new(pool.clone());
        writer
            .write(make_event(
                Uuid::new_v4(),
                FinalStatus::Success,
                Some("chat"),
                vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
            ))
            .await
            .unwrap();

        // The event reused the synced identities instead of creating new ones.
        let (route_id, target_id): (String, String) = sqlx::query_as(
            "SELECT r.route_id, t.target_id
             FROM request_results r
             JOIN target_attempts t ON t.request_id = r.request_id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(route_id, synced_route_id.to_string());
        assert_eq!(target_id, synced_target_id.to_string());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM analytics_routes")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn passthrough_target_is_registered_dynamically() {
        let pool = test_pool().await;
        let writer = EventWriter::new(pool.clone());

        // Event for a route that was never synced + passthrough model.
        writer
            .write(make_event(
                Uuid::new_v4(),
                FinalStatus::Success,
                Some("chat"),
                vec![make_attempt("openai", "req-model", TargetStatus::Success)],
            ))
            .await
            .unwrap();

        let (route_name, provider, real_model): (String, String, String) = sqlx::query_as(
            "SELECT r.route_name, t.provider, t.real_model
             FROM analytics_route_targets t
             JOIN analytics_routes r ON r.route_id = t.route_id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(route_name, "chat");
        assert_eq!(provider, "openai");
        assert_eq!(real_model, "req-model");
    }

    #[tokio::test]
    async fn no_route_and_client_error_persist_without_identity() {
        let pool = test_pool().await;
        let writer = EventWriter::new(pool.clone());

        // Legal route-less final statuses must land, not roll back for
        // lack of a route_id.
        writer
            .write(make_event(
                Uuid::new_v4(),
                FinalStatus::NoRoute,
                None,
                vec![],
            ))
            .await
            .unwrap();
        writer
            .write(make_event(
                Uuid::new_v4(),
                FinalStatus::ClientError,
                None,
                vec![],
            ))
            .await
            .unwrap();

        let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT request_id, final_status, route_id FROM request_results ORDER BY final_status",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, "client_error");
        assert_eq!(rows[1].1, "no_route");
        assert_eq!(rows[0].2, None, "client_error must keep route_id NULL");
        assert_eq!(rows[1].2, None, "no_route must keep route_id NULL");

        // One request_results row each; zero attempts; zero identities.
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM target_attempts")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM analytics_routes")
                .fetch_one(&pool)
                .await
                .unwrap(),
            0,
            "no_route/client_error must not create identities"
        );
    }

    #[tokio::test]
    async fn client_cancelled_persists_with_and_without_attempts() {
        let pool = test_pool().await;
        let writer = EventWriter::new(pool.clone());

        // Cancelled before any attempt was issued: 0 target attempts is
        // legal for client_cancelled and must persist.
        writer
            .write(make_event(
                Uuid::new_v4(),
                FinalStatus::ClientCancelled,
                Some("chat"),
                vec![],
            ))
            .await
            .unwrap();

        // Cancelled with an in-flight attempt: the attempt is recorded.
        writer
            .write(make_event(
                Uuid::new_v4(),
                FinalStatus::ClientCancelled,
                Some("chat"),
                vec![make_attempt("openai", "gpt-4o", TargetStatus::Cancelled)],
            ))
            .await
            .unwrap();

        let (requests, attempts): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM request_results),
                    (SELECT COUNT(*) FROM target_attempts)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(requests, 2, "both client_cancelled events must persist");
        assert_eq!(attempts, 1, "only the in-flight attempt is recorded");
    }

    #[tokio::test]
    async fn replayed_event_is_strict_noop_including_identity_state() {
        use crate::registry::{IdentityRegistry, RouteIdentity, TargetIdentity};

        let pool = test_pool().await;
        let registry = IdentityRegistry::new(pool.clone());
        registry
            .sync(&[RouteIdentity {
                route_name: "chat".to_string(),
                targets: vec![TargetIdentity {
                    provider: "openai".to_string(),
                    real_model: Some("gpt-4o".to_string()),
                }],
            }])
            .await
            .unwrap();
        let writer = EventWriter::new(pool.clone());

        let request_id = Uuid::new_v4();
        let event = || {
            make_event(
                request_id,
                FinalStatus::Success,
                Some("chat"),
                vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
            )
        };
        writer.write(event()).await.unwrap();

        // Deactivate the route (removed from config) — sync is the authority.
        registry.sync(&[]).await.unwrap();
        let last_seen: i64 = sqlx::query_scalar("SELECT last_seen FROM analytics_routes")
            .fetch_one(&pool)
            .await
            .unwrap();

        // Give the clock room so a (buggy) refresh would be observable.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        // Replays of the same request_id must be strict no-ops.
        writer.write(event()).await.unwrap();
        writer.write(event()).await.unwrap();

        let (requests, attempts): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM request_results),
                    (SELECT COUNT(*) FROM target_attempts)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (requests, attempts),
            (1, 1),
            "replay must not duplicate facts"
        );

        let (active, last_seen_after): (i64, i64) =
            sqlx::query_as("SELECT active, last_seen FROM analytics_routes")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(active, 0, "replay must not re-activate a deactivated route");
        assert_eq!(
            last_seen_after, last_seen,
            "replay must not touch last_seen"
        );
        let target_active: i64 = sqlx::query_scalar("SELECT active FROM analytics_route_targets")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            target_active, 0,
            "replay must not re-activate a deactivated target"
        );
    }

    #[tokio::test]
    async fn new_event_does_not_reactivate_synced_inactive_route() {
        use crate::registry::{IdentityRegistry, RouteIdentity, TargetIdentity};

        let pool = test_pool().await;
        let registry = IdentityRegistry::new(pool.clone());
        registry
            .sync(&[RouteIdentity {
                route_name: "chat".to_string(),
                targets: vec![TargetIdentity {
                    provider: "openai".to_string(),
                    real_model: Some("gpt-4o".to_string()),
                }],
            }])
            .await
            .unwrap();
        let writer = EventWriter::new(pool.clone());

        // A request for the still-active route.
        writer
            .write(make_event(
                Uuid::new_v4(),
                FinalStatus::Success,
                Some("chat"),
                vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
            ))
            .await
            .unwrap();

        // Route removed from config → deactivated.
        registry.sync(&[]).await.unwrap();
        let last_seen: i64 = sqlx::query_scalar("SELECT last_seen FROM analytics_routes")
            .fetch_one(&pool)
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        // A *new* request (fresh request_id) for the removed route still
        // lands, but must not resurrect the identity — config sync remains
        // the authority on active/last_seen.
        writer
            .write(make_event(
                Uuid::new_v4(),
                FinalStatus::Success,
                Some("chat"),
                vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
            ))
            .await
            .unwrap();

        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM request_results")
                .fetch_one(&pool)
                .await
                .unwrap(),
            2,
            "the new request must still be persisted"
        );
        let (active, last_seen_after): (i64, i64) =
            sqlx::query_as("SELECT active, last_seen FROM analytics_routes")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(active, 0, "a new event must not re-activate the route");
        assert_eq!(
            last_seen_after, last_seen,
            "writer must not touch last_seen"
        );
    }

    #[tokio::test]
    async fn duplicate_target_tuple_in_one_event_writes_one_attempt() {
        let pool = test_pool().await;
        let writer = EventWriter::new(pool.clone());

        // Same (provider, real_model) twice in one event: the writer must
        // not turn a repeated tuple into two attempt rows.
        writer
            .write(make_event(
                Uuid::new_v4(),
                FinalStatus::Success,
                Some("chat"),
                vec![
                    make_attempt("openai", "gpt-4o", TargetStatus::Success),
                    make_attempt("openai", "gpt-4o", TargetStatus::Success),
                ],
            ))
            .await
            .unwrap();

        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM target_attempts")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1,
            "ON CONFLICT(request_id, target_id) DO NOTHING must collapse duplicates"
        );
    }

    #[tokio::test]
    async fn target_attempts_unique_index_rejects_duplicate_pair() {
        let pool = test_pool().await;
        let writer = EventWriter::new(pool.clone());

        let request_id = Uuid::new_v4();
        writer
            .write(make_event(
                request_id,
                FinalStatus::Success,
                Some("chat"),
                vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
            ))
            .await
            .unwrap();

        // DB-level backstop, exercised directly: a raw INSERT of an
        // existing (request_id, target_id) pair must fail with a UNIQUE
        // constraint violation (migration M1.2).
        let target_id: String = sqlx::query_scalar("SELECT target_id FROM target_attempts")
            .fetch_one(&pool)
            .await
            .unwrap();
        let err = sqlx::query(
            "INSERT INTO target_attempts (
                request_id, target_id, route_id, actual_model, status,
                started_at, completed_at, duration_ms
             )
             VALUES (?, ?, (SELECT route_id FROM target_attempts LIMIT 1),
                     'gpt-4o', 'success', 0, 0, 0)",
        )
        .bind(request_id.to_string())
        .bind(target_id)
        .execute(&pool)
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("UNIQUE"),
            "expected UNIQUE constraint violation, got: {err}"
        );
    }

    #[tokio::test]
    async fn open_rejects_every_in_memory_dsn_spelling() {
        // Every spelling SQLite resolves to an in-memory / per-connection
        // database must fail loudly instead of creating a private
        // split-brain database; callers share a pool via `new` instead.
        for dsn in [
            ":memory:",
            ":memory:?cache=shared",
            "sqlite::memory:",
            "sqlite::memory:?cache=shared",
            "sqlite://?mode=memory",
            "sqlite:file:memdb1?mode=memory&cache=shared",
            "file::memory:",
            "file::memory:?cache=shared",
            "file:memdb1?mode=memory&cache=shared",
            // A fragment must not hide the in-memory markers.
            "file:memdb1?mode=memory#frag",
            "file::memory:#frag",
            ":memory:?cache=shared#frag",
            // Percent-encoded spellings resolve to in-memory databases too.
            "file:%3Amemory%3A",
            "file:%3Amemory%3A?cache=shared",
            "file:memdb1?mode%3Dmemory",
            "file:memdb1?mode%3Dmemory&cache=shared",
            "file:memdb1?mode%3Dmemory#frag",
            "sqlite:file:memdb1?mode%3Dmemory",
            "sqlite:file:%3Amemory%3A",
        ] {
            let err = match EventWriter::open(dsn).await {
                Err(e) => e,
                Ok(_) => panic!("open({dsn:?}) must reject an in-memory DSN"),
            };
            assert!(
                err.to_string().contains("in-memory")
                    && err.to_string().contains("EventWriter::new"),
                "open({dsn:?}) must fail with a clear message, got: {err}"
            );
        }
    }

    #[test]
    fn is_in_memory_dsn_handles_fragments_and_percent_encoding() {
        // In-memory spellings — including fragment and percent-encoded
        // variants — must all be classified in-memory.
        for dsn in [
            ":memory:",
            ":memory:?cache=shared",
            "sqlite::memory:",
            "sqlite://?mode=memory",
            "file::memory:",
            "file::memory:?cache=shared",
            "file:memdb1?mode=memory&cache=shared",
            // fragment: `#...` must not hide the markers
            "file:memdb1?mode=memory#frag",
            "file::memory:#frag",
            ":memory:?cache=shared#frag",
            // percent-encoding (decoded by SQLite for `file:` URIs)
            "file:%3Amemory%3A",
            "file:%3Amemory%3A?cache=shared",
            "file:memdb1?mode%3Dmemory",
            "file:memdb1?mode%3Dmemory&cache=shared",
            "file:memdb1?mode%3Dmemory#frag",
            "sqlite:file:memdb1?mode%3Dmemory",
            "sqlite:file:%3Amemory%3A",
        ] {
            assert!(is_in_memory_dsn(dsn), "{dsn:?} must be in-memory");
        }

        // Persistent spellings must never be flagged: plain paths, `sqlite:`
        // URLs, `mode=rwc` URIs and percent-encoded ordinary file names.
        for dsn in [
            "analytics.db",
            "data/analytics.db",
            "sqlite://analytics.db",
            "sqlite://data/analytics.db?mode=rwc",
            "file:analytics.db",
            "file:analytics.db?mode=rwc",
            "file:analytics.db?mode=rwc&cache=shared",
            "file:analytics.db#frag",
            "file:my%20analytics.db",
            "sqlite:file:analytics.db?mode=rwc",
            "file:memdb1?mode=ro",
            "file:memdb1?mode%3Dro",
        ] {
            assert!(!is_in_memory_dsn(dsn), "{dsn:?} must be persistent");
        }
    }

    #[tokio::test]
    async fn open_accepts_file_dsn_plain_and_url_form() {
        let dir = std::env::temp_dir().join(format!("sag-analytics-open-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // Plain path DSN: pool + migrations + write.
        let writer = EventWriter::open(dir.join("writer.db").to_str().unwrap())
            .await
            .unwrap();
        writer
            .write(make_event(
                Uuid::new_v4(),
                FinalStatus::NoRoute,
                None,
                vec![],
            ))
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_results")
            .fetch_one(writer.pool())
            .await
            .unwrap();
        assert_eq!(count, 1);
        drop(writer);

        // `sqlite:` URL form pointing at a file (mode=rwc creates it):
        // shared cache on a file is fine, only in-memory DSNs are rejected.
        let url_dsn = format!(
            "sqlite://{}?mode=rwc&cache=shared",
            dir.join("writer-url.db").to_str().unwrap()
        );
        let writer = EventWriter::open(&url_dsn).await.unwrap();
        writer
            .write(make_event(
                Uuid::new_v4(),
                FinalStatus::NoRoute,
                None,
                vec![],
            ))
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_results")
            .fetch_one(writer.pool())
            .await
            .unwrap();
        assert_eq!(count, 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    // ── late facts: dirty-marking completed buckets ──

    /// UTC hour start containing `REF_TS_MS` (2023-11-14T22:00:00Z).
    const REF_HOUR: i64 = 1_699_999_200_000;

    /// A success event whose request and attempt complete `offset_ms` after
    /// `REF_TS_MS` — kept well inside `REF_HOUR` (`REF_TS_MS` is 13m20s
    /// past the hour start), so the whole event lands in one UTC hour.
    fn late_event(request_id: Uuid, offset_ms: i64) -> AnalyticsEvent {
        let mut event = make_event(
            request_id,
            FinalStatus::Success,
            Some("chat"),
            vec![make_attempt("openai", "gpt-4o", TargetStatus::Success)],
        );
        event.result.completed_at = chrono::Utc
            .timestamp_millis_opt(REF_TS_MS + offset_ms)
            .unwrap();
        event
    }

    /// Let the rollup engine register and complete the bucket for `hour`.
    async fn complete_bucket(pool: &SqlitePool, hour: i64) {
        let engine = RollupEngine::new(pool.clone());
        engine
            .register_missing_hours(hour, hour + HOUR_MS)
            .await
            .unwrap();
        assert_eq!(engine.run_once().await.unwrap().completed, 1);
        let st = engine.bucket_status(hour).await.unwrap().unwrap();
        assert_eq!(st.state, BucketState::Completed);
        assert!(!st.dirty);
    }

    async fn bucket_status(pool: &SqlitePool, hour: i64) -> BucketStatus {
        RollupEngine::new(pool.clone())
            .bucket_status(hour)
            .await
            .unwrap()
            .unwrap()
    }

    /// Mirror of the private `BucketState::from_db` mapping, for asserting
    /// that non-completed buckets keep their exact state.
    fn expect_state(state: &str) -> BucketState {
        match state {
            "pending" => BucketState::Pending,
            "processing" => BucketState::Processing,
            "failed" => BucketState::Failed,
            other => panic!("unexpected state {other}"),
        }
    }

    #[tokio::test]
    async fn late_fact_marks_completed_bucket_dirty_once() {
        let pool = test_pool().await;
        let writer = EventWriter::new(pool.clone());

        // Seed the hour and let the rollup engine complete its bucket.
        writer
            .write(late_event(Uuid::new_v4(), 60_000))
            .await
            .unwrap();
        complete_bucket(&pool, REF_HOUR).await;

        // A late fact (new request_id, same UTC hour) marks the completed
        // bucket dirty in the same transaction; state stays completed.
        let late_id = Uuid::new_v4();
        writer.write(late_event(late_id, 120_000)).await.unwrap();
        let st = bucket_status(&pool, REF_HOUR).await;
        assert_eq!(st.state, BucketState::Completed);
        assert!(st.dirty);

        // A replayed event is a strict no-op: it neither repeats nor
        // clears the marking, and never touches the bucket state.
        writer.write(late_event(late_id, 120_000)).await.unwrap();
        let st = bucket_status(&pool, REF_HOUR).await;
        assert_eq!(st.state, BucketState::Completed);
        assert!(st.dirty);
    }

    #[tokio::test]
    async fn event_with_multiple_completed_hours_marks_all_distinct() {
        let pool = test_pool().await;
        let writer = EventWriter::new(pool.clone());

        // Two completed buckets: REF_HOUR and the next hour.
        writer
            .write(late_event(Uuid::new_v4(), 60_000))
            .await
            .unwrap();
        complete_bucket(&pool, REF_HOUR).await;
        let h1 = REF_HOUR + HOUR_MS;
        let mut seed = late_event(Uuid::new_v4(), 0);
        seed.result.completed_at = chrono::Utc.timestamp_millis_opt(h1 + 60_000).unwrap();
        seed.targets[0].completed_at = chrono::Utc.timestamp_millis_opt(h1 + 60_000).unwrap();
        writer.write(seed).await.unwrap();
        complete_bucket(&pool, h1).await;

        // One event whose request completes in REF_HOUR but whose second
        // attempt completes in h1 dirties both distinct hours exactly once.
        let cross = Uuid::new_v4();
        let mut event = late_event(cross, 120_000);
        let mut attempt = make_attempt("openai", "gpt-4o-mini", TargetStatus::Success);
        attempt.completed_at = chrono::Utc.timestamp_millis_opt(h1 + 60_000).unwrap();
        event.targets.push(attempt);
        writer.write(event).await.unwrap();

        let st0 = bucket_status(&pool, REF_HOUR).await;
        let st1 = bucket_status(&pool, h1).await;
        assert_eq!(st0.state, BucketState::Completed);
        assert_eq!(st1.state, BucketState::Completed);
        assert!(st0.dirty);
        assert!(st1.dirty);
    }

    #[tokio::test]
    async fn non_completed_and_unregistered_hours_are_never_marked() {
        let pool = test_pool().await;
        let writer = EventWriter::new(pool.clone());

        let now = chrono::Utc::now().timestamp_millis();
        let states = ["pending", "processing", "failed"];
        for (i, state) in states.iter().enumerate() {
            sqlx::query(
                "INSERT INTO rollup_bucket_state \
                 (hour, state, dirty, attempt, processing_started_at, updated_at) \
                 VALUES (?, ?, 0, 0, NULL, ?)",
            )
            .bind(REF_HOUR + (i as i64 + 1) * HOUR_MS)
            .bind(state)
            .bind(now)
            .execute(&pool)
            .await
            .unwrap();
        }

        // Facts landing in pending / processing / failed hours are not
        // marked — those buckets pick the fact up when computed.
        for (i, state) in states.iter().enumerate() {
            let hour = REF_HOUR + (i as i64 + 1) * HOUR_MS;
            writer
                .write(late_event(
                    Uuid::new_v4(),
                    (i as i64 + 1) * HOUR_MS + 60_000,
                ))
                .await
                .unwrap();
            let st = bucket_status(&pool, hour).await;
            assert!(!st.dirty, "bucket {state} must not be marked dirty");
            assert_eq!(st.state, expect_state(state));
        }

        // A fact in an hour with no state row must not create one — the
        // rollup startup scan registers hours with facts itself.
        let unregistered = REF_HOUR + 4 * HOUR_MS;
        writer
            .write(late_event(Uuid::new_v4(), 4 * HOUR_MS + 60_000))
            .await
            .unwrap();
        assert!(
            RollupEngine::new(pool.clone())
                .bucket_status(unregistered)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn failed_write_rolls_back_facts_and_dirty_mark_together() {
        let pool = test_pool().await;
        let writer = EventWriter::new(pool.clone());

        writer
            .write(late_event(Uuid::new_v4(), 60_000))
            .await
            .unwrap();
        complete_bucket(&pool, REF_HOUR).await;

        // Force the dirty-marking UPDATE to fail mid-transaction: the whole
        // write — fact rows and the dirty mark — must roll back together.
        sqlx::query(
            "CREATE TRIGGER fail_rollup_update BEFORE UPDATE ON rollup_bucket_state \
             BEGIN SELECT RAISE(ABORT, 'forced failure'); END",
        )
        .execute(&pool)
        .await
        .unwrap();

        let err = writer
            .write(late_event(Uuid::new_v4(), 120_000))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("forced failure"),
            "unexpected error: {err}"
        );

        // No fact landed and the completed bucket is still clean.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_results")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        let st = bucket_status(&pool, REF_HOUR).await;
        assert_eq!(st.state, BucketState::Completed);
        assert!(!st.dirty);
    }
}
