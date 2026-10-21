//! `call_logs` hot/cold rotation (table rotation + hot-window views).
//!
//! `call_logs` is append-only and grows without bound. Retention is
//! implemented by **rotating the whole table** instead of moving rows: one
//! short DDL-only transaction renames `call_logs` to
//! `call_logs_arch_<UTC yyyyMMdd_HHmmss>` and recreates an empty, identically
//! shaped `call_logs` from the DDL captured out of `sqlite_master` moments
//! earlier. There is no row-level data path, so the cost is independent of
//! the row count.
//!
//! ```text
//! rotate_once(pool, cutoff):
//!   probe MIN(timestamp) FROM call_logs        -- nothing old? return None
//!   BEGIN IMMEDIATE
//!     ⓪ DROP VIEW call_logs_hot / call_logs_all
//!     ① capture table DDL + index DDLs (still spelled `call_logs`)
//!     ② ALTER TABLE call_logs RENAME TO "call_logs_arch_<ts>"
//!     ③ DROP every index the cold table inherited (frees the names)
//!     ④ replay the captured DDL verbatim (hot table + its indexes)
//!     ⑤ assert_replay_faithful (column shape + index set)
//!     ⑥ build_views_in_tx(Some(cutoff))
//!   COMMIT
//! ```
//!
//! Two ordering facts are load-bearing:
//!
//! - **③ before ④**: index names live in one schema-wide namespace. If the
//!   cold table still owns `idx_call_logs_ts` when the captured
//!   `CREATE INDEX idx_call_logs_ts ON call_logs (…)` is replayed, the new hot
//!   table ends up without its indexes (skip or hard error, depending on the
//!   statement text).
//! - **⓪ + ⑥ in the same transaction**: SQLite ≥ 3.25 rewrites table
//!   references inside view definitions on `ALTER TABLE … RENAME`, so the
//!   views must not exist while the rename happens; rebuilding them inside the
//!   rotation transaction means readers see either the old world or the new
//!   one, never a missing or stale view.
//!
//! The two read views are the query surface of the analytics store:
//!
//! - `call_logs_hot` — hot table plus every cold table that still holds
//!   in-window rows (`MAX(timestamp) >= cutoff`), each branch filtered by
//!   `timestamp >= cutoff`;
//! - `call_logs_all` — hot table plus every cold table, unfiltered (the
//!   "history stays reachable through plain SQL" floor).
//!
//! Both are built from the hot table's `pragma_table_info` column list (cid
//! order) with `NULL AS "<col>"` padding for cold tables that predate a
//! column — cold tables are schema snapshots and are never altered. A `None`
//! cutoff means `call_log_retention_days == 0` (rotation disabled): every
//! table is a member of both views and nothing is filtered.
//!
//! Maintenance never panics and never fails a request: callers log the error
//! and retry on the next round.

use chrono::Utc;
use sqlx::SqliteConnection;
use sqlx::sqlite::SqlitePool;
use std::collections::HashSet;

/// The hot table: the single write target, recreated empty by each rotation.
pub const HOT_TABLE: &str = "call_logs";

/// Windowed read view: hot table + in-window cold tables, `timestamp`
/// filtered to the retention window.
pub const HOT_VIEW: &str = "call_logs_hot";

/// Full-history read view: hot table + every cold table, unfiltered.
pub const ALL_VIEW: &str = "call_logs_all";

/// Prefix of every rotated (cold) table. The archive name is
/// `call_logs_arch_<UTC yyyyMMdd_HHmmss>`; view member enumeration matches on
/// this same prefix, so the two cannot drift apart.
pub const ARCH_PREFIX: &str = "call_logs_arch_";

/// Column that carries the retention window. Every member table of a view
/// must have it (it is `NOT NULL` since the initial migration).
const TIMESTAMP_COLUMN: &str = "timestamp";

/// Milliseconds per day — the unit of a retention setting. Callers pass a
/// **day** count (the gateway's `call_log_retention_days`), never a
/// millisecond value.
const DAY_MS: i64 = 86_400_000;

/// One captured `sqlite_master` index row (`sql` is never NULL: implicit
/// `sqlite_autoindex_*` entries are filtered out because they cannot be
/// dropped nor replayed).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
struct IndexDdl {
    name: String,
    sql: String,
}

/// One `pragma_table_info` row: the shape of a table, column by column.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
struct ColumnInfo {
    cid: i64,
    name: String,
    decl_type: String,
    not_null: i64,
    dflt_value: Option<String>,
    pk: i64,
}

/// Rotate `call_logs` when it holds rows older than `cutoff_ms`.
///
/// Returns `Ok(None)` when nothing is old enough (empty or fully in-window
/// hot table), otherwise the name of the cold table the rows were rotated
/// into. Everything happens in one `BEGIN IMMEDIATE` transaction: a failure
/// at any step rolls the rotation back and leaves the hot table, the cold
/// tables and both views exactly as they were.
pub async fn rotate_once(pool: &SqlitePool, cutoff_ms: i64) -> Result<Option<String>, sqlx::Error> {
    // Trigger probe, outside the transaction — the only maintenance query
    // that reads `call_logs` directly. Its result decides *whether* this
    // round rotates, and nothing else: the probe borrows one pooled
    // connection while the `BEGIN IMMEDIATE` below may land on another, so
    // any read that has to agree with the rotation snapshot (view members,
    // row counts, …) must be taken **inside** the transaction, never here.
    let min_ts: Option<i64> =
        sqlx::query_scalar(&format!("SELECT MIN({TIMESTAMP_COLUMN}) FROM {HOT_TABLE}"))
            .fetch_one(pool)
            .await?;
    if min_ts.is_none_or(|ts| ts >= cutoff_ms) {
        return Ok(None);
    }

    let arch = unique_archive_name(pool).await?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;

    // ⓪ Kill the views first: `ALTER TABLE … RENAME` would otherwise rewrite
    //    their definitions to point at the cold table.
    for view in [HOT_VIEW, ALL_VIEW] {
        sqlx::query(&format!("DROP VIEW IF EXISTS {view}"))
            .execute(&mut *tx)
            .await?;
    }

    // ① Capture the DDL while it still refers to `call_logs`. The stored
    //    `sqlite_master.sql` text is the single source of truth for the
    //    column set: SQLite rewrites it on every `ALTER TABLE … ADD COLUMN`,
    //    so the captured text already contains all columns.
    let table_ddl: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1 AND sql IS NOT NULL",
    )
    .bind(HOT_TABLE)
    .fetch_one(&mut *tx)
    .await?;
    let index_ddls = index_ddls(&mut tx, HOT_TABLE).await?;

    // ② Rename the table: its rows become the cold table in place, its
    //    indexes follow the rename (same names, now on the cold table).
    sqlx::query(&format!(
        "ALTER TABLE {HOT_TABLE} RENAME TO {}",
        quote_ident(&arch)
    ))
    .execute(&mut *tx)
    .await?;

    // ③ Retire every index the cold table inherited — load-bearing, see the
    //    module docs: it frees the names for the replay below.
    for idx in index_names_of(&mut tx, &arch).await? {
        sqlx::query(&format!("DROP INDEX IF EXISTS {}", quote_ident(&idx)))
            .execute(&mut *tx)
            .await?;
    }

    // ④ Replay verbatim: no string rewriting. The table DDL creates a new
    //    empty `call_logs` of the captured shape, the index DDLs land on it.
    sqlx::query(&table_ddl).execute(&mut *tx).await?;
    for ddl in &index_ddls {
        sqlx::query(&ddl.sql).execute(&mut *tx).await?;
    }

    // ⑤ Cheap fidelity guard: any mismatch rolls the whole rotation back.
    assert_replay_faithful(&mut tx, &arch, &index_ddls).await?;

    // ⑥ Rebuild both views in the same transaction: no gap between "the old
    //    world" and "the new world" for readers.
    build_views_in_tx(&mut tx, Some(cutoff_ms)).await?;

    tx.commit().await?;
    tracing::info!(
        arch,
        indexes = index_ddls.len(),
        cutoff_ms,
        "call_logs rotated"
    );
    Ok(Some(arch))
}

/// Rebuild both read views from the current table set in its own
/// `BEGIN IMMEDIATE` transaction.
///
/// `cutoff_ms = None` means retention is disabled (`call_log_retention_days
/// == 0`): every table is a member of both views and nothing is filtered.
/// `Some(cutoff)` narrows `call_logs_hot` to the window. The cutoff is baked
/// into the view body as a literal, which is why the scheduler refreshes
/// daily even on rounds that do not rotate anything.
pub async fn refresh_views(pool: &SqlitePool, cutoff_ms: Option<i64>) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    build_views_in_tx(&mut tx, cutoff_ms).await?;
    tx.commit().await?;
    Ok(())
}

/// Outcome of one [`run_rotation_round`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundOutcome {
    /// `call_log_retention_days == 0` (rotation disabled): the round touched
    /// nothing at all — no retirement, no rotation, no window narrowing.
    Skipped,
    /// The round ran to completion. `rotated` is the cold table the hot rows
    /// were moved into (`None` when nothing was old enough to rotate), and
    /// `retired_indexes` counts the cold-table indexes the sweep dropped.
    Ran {
        rotated: Option<String>,
        retired_indexes: usize,
    },
}

impl RoundOutcome {
    /// The cold table this round produced, if it rotated anything.
    pub fn rotated(&self) -> Option<&str> {
        match self {
            Self::Skipped | Self::Ran { rotated: None, .. } => None,
            Self::Ran {
                rotated: Some(arch),
                ..
            } => Some(arch),
        }
    }

    /// Number of cold-table indexes dropped by this round (`0` when the
    /// round was skipped).
    pub fn retired_indexes(&self) -> usize {
        match self {
            Self::Skipped => 0,
            Self::Ran {
                retired_indexes, ..
            } => *retired_indexes,
        }
    }
}

/// One full maintenance round: **retire → rotate → refresh**, in that order.
///
/// The order is load-bearing (and is the single definition of "a round" —
/// the gateway scheduler and the rotation integration tests both call this
/// function instead of replaying the three steps by hand):
///
/// - retirement runs first, against the cold tables the **previous** rounds
///   produced;
/// - rotation moves the whole hot table into a new cold table and rebuilds
///   the views inside its own transaction;
/// - the refresh runs **unconditionally**, including on rounds that rotated
///   nothing, because the cutoff is baked into the view bodies as a literal —
///   an un-refreshed window would never move forward. On a rotating round it
///   is an idempotent rebuild.
///
/// `retention_days` is supplied by the caller (the gateway re-reads it from
/// the live config on every round), so this crate takes no `sag-config`
/// dependency. `retention_days == 0` disables the feature entirely and
/// returns [`RoundOutcome::Skipped`] without touching the database.
///
/// Failure policy mirrors the scheduler's: a failed **retirement** is logged
/// and the round continues (it must not block rotation); a failed
/// **rotation** aborts the round and is propagated (it rolled back
/// atomically, so the views still describe the pre-round world and there is
/// nothing to refresh); a failed **refresh** is propagated as well (the round
/// ends there anyway). Callers log and retry on the next round — maintenance
/// never panics.
pub async fn run_rotation_round(
    pool: &SqlitePool,
    retention_days: u32,
) -> Result<RoundOutcome, sqlx::Error> {
    if retention_days == 0 {
        return Ok(RoundOutcome::Skipped);
    }
    // Saturating on both ends: every `u32` day count fits in `i64` today
    // (`u32::MAX` days ≈ 3.7e17 ms ≪ `i64::MAX`), but the arithmetic must not
    // panic if the retention type ever widens or the clock jumps backwards —
    // maintenance never panics. A saturated cutoff is "keep everything".
    let cutoff_ms = Utc::now()
        .timestamp_millis()
        .saturating_sub(i64::from(retention_days).saturating_mul(DAY_MS));

    // No test injection point reaches this failure branch (`retire_indexes`
    // talks to the pool directly); the behaviour is warn + continue, matching
    // the original gateway implementation.
    let retired_indexes = match retire_indexes(pool, cutoff_ms).await {
        Ok(dropped) => dropped,
        Err(e) => {
            tracing::warn!(error = %e, "index retirement failed; retrying next cycle");
            0
        }
    };

    let rotated = rotate_once(pool, cutoff_ms).await?;
    refresh_views(pool, Some(cutoff_ms)).await?;

    Ok(RoundOutcome::Ran {
        rotated,
        retired_indexes,
    })
}

/// Retire the indexes of every cold table that has fallen out of the window
/// (`MAX(timestamp) < cutoff_ms`). Cold tables inside the window keep their
/// indexes untouched; the pass is idempotent and returns the number of
/// indexes dropped.
///
/// Rotation itself leaves cold tables without indexes (see the module docs),
/// so in steady state this is a no-op that only sweeps manually added
/// indexes — the contract is generic on purpose.
pub async fn retire_indexes(pool: &SqlitePool, cutoff_ms: i64) -> Result<usize, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let mut dropped = 0usize;
    for cold in cold_table_names(&mut tx).await? {
        let max_ts = cold_max_timestamp(&mut tx, &cold).await?;
        if !max_ts.is_some_and(|ts| ts < cutoff_ms) {
            continue;
        }
        for idx in index_names_of(&mut tx, &cold).await? {
            sqlx::query(&format!("DROP INDEX IF EXISTS {}", quote_ident(&idx)))
                .execute(&mut *tx)
                .await?;
            dropped += 1;
        }
    }
    tx.commit().await?;
    if dropped > 0 {
        tracing::info!(dropped, cutoff_ms, "retired cold-table indexes");
    }
    Ok(dropped)
}

/// Next free archive name: `call_logs_arch_<UTC yyyyMMdd_HHmmss>`, with a
/// `_1`, `_2`, … suffix when the same second is already taken.
pub async fn unique_archive_name(pool: &SqlitePool) -> Result<String, sqlx::Error> {
    let base = format!("{ARCH_PREFIX}{}", Utc::now().format("%Y%m%d_%H%M%S"));
    let mut candidate = base.clone();
    let mut n: u32 = 0;
    loop {
        let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM sqlite_master WHERE name = ?1")
            .bind(&candidate)
            .fetch_optional(pool)
            .await?;
        if exists.is_none() {
            return Ok(candidate);
        }
        n += 1;
        candidate = format!("{base}_{n}");
    }
}

/// Does this sqlx error mean a cached prepared statement went stale
/// (`SQLITE_SCHEMA`, SQLite result code 17)?
///
/// Rotation and view rebuilds bump the schema cookie, and every pool
/// connection caches its own prepared statements, so a read prepared before
/// the DDL can come back as a schema-change error. Query paths use this to
/// retry **once** instead of surfacing a 500.
///
/// Two detectors, deliberately kept as a belt-and-braces pair:
///
/// - **structured**: `sqlx` exposes the numeric SQLite result code through
///   `DatabaseError::code()`, so a real reprepare failure is `"17"`
///   (`SQLITE_SCHEMA` has no extended variants, so the extended code is 17
///   too). This is the authoritative test.
/// - **text**: `sqlx` renders a driver error as `(code: <n>) <sqlite3_errmsg>`
///   and code 17's errmsg is exactly `database schema has changed`. A
///   code-less error (`sqlx::Error::Protocol`, e.g. one built by our own
///   code or by a test) carries no numeric code, so the errmsg is the only
///   signal left — without this fallback such an error would silently stop
///   being retried and surface as a 500.
///
/// The symbolic name `SQLITE_SCHEMA` is **not** matched: sqlx never renders
/// it (`sqlx-sqlite` writes the numeric code plus `sqlite3_errmsg`), so
/// matching it would be a branch no production path can reach.
pub fn is_schema_change(e: &sqlx::Error) -> bool {
    /// `SQLITE_SCHEMA`, as rendered by `sqlx-sqlite`'s `DatabaseError::code`.
    const SQLITE_SCHEMA_CODE: &str = "17";

    let code = e.as_database_error().and_then(|db| db.code());
    if code.as_deref() == Some(SQLITE_SCHEMA_CODE) {
        return true;
    }
    // SQLite's `sqlite3_errmsg` text for result code 17.
    e.to_string().contains("database schema has changed")
}

/// Build `call_logs_hot` and `call_logs_all` inside the caller's transaction
/// (the rotation transaction, or [`refresh_views`]'s own).
pub(crate) async fn build_views_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    cutoff_ms: Option<i64>,
) -> Result<(), sqlx::Error> {
    // The hot table's columns are the single source of truth for the view
    // column list (cid order) — cold tables pad with `NULL AS`.
    let columns = table_columns(&mut *tx, HOT_TABLE).await?;
    if columns.is_empty() {
        return Err(sqlx::Error::Protocol(format!(
            "view build: table {HOT_TABLE} does not exist"
        )));
    }
    let colds = cold_table_names(&mut *tx).await?;

    // `call_logs_hot`: hot table first (its column names become the view's
    // column names), then cold tables by descending name (newest first),
    // keeping only those that still hold in-window rows.
    let mut hot_members = Vec::with_capacity(colds.len() + 1);
    hot_members.push(HOT_TABLE.to_string());
    for cold in &colds {
        if let Some(cutoff) = cutoff_ms {
            let max_ts = cold_max_timestamp(&mut *tx, cold).await?;
            if !max_ts.is_some_and(|ts| ts >= cutoff) {
                continue;
            }
        }
        hot_members.push(cold.clone());
    }
    let hot_body = view_body(&mut *tx, &columns, &hot_members, cutoff_ms).await?;

    // `call_logs_all`: every table, no filter (history floor).
    let mut all_members = Vec::with_capacity(colds.len() + 1);
    all_members.push(HOT_TABLE.to_string());
    all_members.extend(colds.iter().cloned());
    let all_body = view_body(&mut *tx, &columns, &all_members, None).await?;

    replace_view(&mut *tx, HOT_VIEW, &hot_body).await?;
    replace_view(&mut *tx, ALL_VIEW, &all_body).await?;
    Ok(())
}

/// One `CREATE VIEW` body: one `SELECT` branch per member table, `UNION ALL`
/// joined, every branch projecting the hot table's columns in cid order
/// (missing columns padded with `NULL AS "<col>"`).
async fn view_body(
    conn: &mut SqliteConnection,
    columns: &[String],
    members: &[String],
    cutoff_ms: Option<i64>,
) -> Result<String, sqlx::Error> {
    // The window is applied in **two layers** and both are required:
    // `build_views_in_tx` decides *membership* (a cold table joins
    // `call_logs_hot` only while `MAX(timestamp) >= cutoff`), and this
    // per-branch `WHERE` filters the *rows* of a member that straddles the
    // cutoff. Dropping the membership test would scan fully expired cold
    // tables on every query; dropping this row filter would leak expired rows
    // through a partially expired member. Do not remove either one.
    let filter = match cutoff_ms {
        Some(ms) => format!(" WHERE {TIMESTAMP_COLUMN} >= {ms}"),
        None => String::new(),
    };
    let mut branches = Vec::with_capacity(members.len());
    for member in members {
        let present: HashSet<String> = table_columns(&mut *conn, member)
            .await?
            .into_iter()
            .collect();
        let select = columns
            .iter()
            .map(|col| {
                if present.contains(col) {
                    quote_ident(col)
                } else {
                    format!("NULL AS {}", quote_ident(col))
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        branches.push(format!(
            "SELECT {select} FROM {}{filter}",
            quote_ident(member)
        ));
    }
    // A single member degenerates to a plain `SELECT` — no dangling
    // `UNION ALL`.
    Ok(branches.join(" UNION ALL "))
}

/// `DROP VIEW IF EXISTS` + `CREATE VIEW` for one view.
async fn replace_view(
    conn: &mut SqliteConnection,
    view: &str,
    body: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(&format!("DROP VIEW IF EXISTS {view}"))
        .execute(&mut *conn)
        .await?;
    sqlx::query(&format!("CREATE VIEW {view} AS {body}"))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Column shape of `table`, in `cid` order.
///
/// The table name is spliced into `pragma_table_info('…')` as a **string
/// literal** (the pragma table-valued function takes a name, not a bindable
/// identifier), so it goes through [`escape_literal`]. Every current caller
/// passes an internal identifier — a compile-time constant (`HOT_TABLE`) or a
/// name read back out of `sqlite_master` — which is why doubling single
/// quotes is sufficient here. Reusing this helper on caller-supplied table
/// names means re-evaluating that assumption first.
async fn table_columns(
    conn: &mut SqliteConnection,
    table: &str,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(&format!(
        "SELECT name FROM pragma_table_info('{}') ORDER BY cid",
        escape_literal(table)
    ))
    .fetch_all(&mut *conn)
    .await
}

/// Full `pragma_table_info` rows of `table` (cid order) — the column shape
/// used by the replay fidelity check. Same literal-escaping caveat as
/// [`table_columns`].
async fn column_infos(
    conn: &mut SqliteConnection,
    table: &str,
) -> Result<Vec<ColumnInfo>, sqlx::Error> {
    sqlx::query_as::<_, ColumnInfo>(&format!(
        "SELECT cid, name, type AS decl_type, \"notnull\" AS not_null, dflt_value, pk \
         FROM pragma_table_info('{}') ORDER BY cid",
        escape_literal(table)
    ))
    .fetch_all(&mut *conn)
    .await
}

/// Every cold table, newest first (`name DESC`).
async fn cold_table_names(conn: &mut SqliteConnection) -> Result<Vec<String>, sqlx::Error> {
    // `ARCH_PREFIX` is a fixed literal, so only its underscores need the
    // `LIKE` escape.
    let pattern = format!("{}%", ARCH_PREFIX.replace('_', "\\_"));
    sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE ?1 ESCAPE '\\' \
         ORDER BY name DESC",
    )
    .bind(pattern)
    .fetch_all(&mut *conn)
    .await
}

/// `MAX(timestamp)` of one cold table (`None` for an empty table, and for a
/// cold table without a `timestamp` column — impossible in practice, warned
/// about defensively).
async fn cold_max_timestamp(
    conn: &mut SqliteConnection,
    table: &str,
) -> Result<Option<i64>, sqlx::Error> {
    let columns = table_columns(&mut *conn, table).await?;
    if !columns.iter().any(|col| col == TIMESTAMP_COLUMN) {
        tracing::warn!(
            table,
            "cold table has no timestamp column; skipping window evaluation"
        );
        return Ok(None);
    }
    sqlx::query_scalar(&format!(
        "SELECT MAX({TIMESTAMP_COLUMN}) FROM {}",
        quote_ident(table)
    ))
    .fetch_one(&mut *conn)
    .await
}

/// Names of the (droppable) indexes of `table`. Implicit
/// `sqlite_autoindex_*` entries are excluded: they have no `sql` text and
/// cannot be dropped.
async fn index_names_of(
    conn: &mut SqliteConnection,
    table: &str,
) -> Result<Vec<String>, sqlx::Error> {
    Ok(index_ddls(&mut *conn, table)
        .await?
        .into_iter()
        .map(|idx| idx.name)
        .collect())
}

/// Captured `sqlite_master` index rows of `table`, ordered by name.
async fn index_ddls(
    conn: &mut SqliteConnection,
    table: &str,
) -> Result<Vec<IndexDdl>, sqlx::Error> {
    sqlx::query_as::<_, IndexDdl>(
        "SELECT name, sql FROM sqlite_master WHERE type = 'index' AND tbl_name = ?1 \
         AND sql IS NOT NULL ORDER BY name",
    )
    .bind(table)
    .fetch_all(&mut *conn)
    .await
}

/// Verify the replayed hot table is a faithful copy of the rotated one:
/// identical column shape, identical index set (names + statements), and no
/// index left on the cold table. A mismatch aborts the rotation.
///
/// **Error kind**: the three failures below are reported as
/// `sqlx::Error::Protocol`. That is a deliberate, bounded category mismatch —
/// sqlx has no "internal invariant broken" variant, and `sag_core::AppError`
/// is the HTTP layer's type, not a maintenance-internal one. It is safe
/// because [`is_schema_change`] recognises a stale statement by the numeric
/// SQLITE_SCHEMA code (17) or by SQLite's own errmsg text, **never** by the
/// word "schema", so these self-produced messages can never be misread as a
/// retryable schema change. Keep the wording below free of SQLite's errmsg
/// text.
///
/// **Coverage**: all three branches are unreachable from the outside, which is
/// why no test injects them (an accepted trade-off, argued in
/// `rotate_once_rolls_back_atomically_on_failure`): the archive name is
/// collision-checked, step ③ releases every index name before the replay, and
/// the replayed DDL is captured verbatim from the same schema — so a mismatch
/// would mean the transaction itself is broken, and the check exists to turn
/// that into a rollback rather than a silent half-rotation. The branches only
/// read, so a missed mismatch cannot corrupt data.
async fn assert_replay_faithful(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    arch: &str,
    captured: &[IndexDdl],
) -> Result<(), sqlx::Error> {
    let hot_columns = column_infos(&mut *tx, HOT_TABLE).await?;
    let cold_columns = column_infos(&mut *tx, arch).await?;
    if hot_columns != cold_columns {
        return Err(sqlx::Error::Protocol(format!(
            "rotation replay mismatch: replayed {HOT_TABLE} column shape differs from cold table \
             {arch} ({} vs {} columns)",
            hot_columns.len(),
            cold_columns.len()
        )));
    }

    let replayed = index_ddls(&mut *tx, HOT_TABLE).await?;
    if replayed.len() != captured.len() || replayed != captured {
        return Err(sqlx::Error::Protocol(format!(
            "rotation replay mismatch: replayed {HOT_TABLE} has {} indexes, captured {} \
             (expected names {})",
            replayed.len(),
            captured.len(),
            captured
                .iter()
                .map(|idx| idx.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    let leftover = index_names_of(&mut *tx, arch).await?;
    if !leftover.is_empty() {
        return Err(sqlx::Error::Protocol(format!(
            "rotation replay mismatch: cold table {arch} still owns indexes {}",
            leftover.join(", ")
        )));
    }
    Ok(())
}

/// Quote a SQL identifier (double quotes, embedded quotes doubled). Archive
/// names are generated from a safe charset, but every interpolated identifier
/// goes through this anyway.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Escape a value for a single-quoted SQL literal (`pragma_table_info`
/// arguments are literals, not bindable identifiers).
fn escape_literal(value: &str) -> String {
    value.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_analytics_pool;
    use sqlx::Row;

    async fn test_pool() -> SqlitePool {
        // `:memory:` uses a single connection, so every statement of a test
        // observes the same private database.
        open_analytics_pool(":memory:").await.unwrap()
    }

    fn now_ms() -> i64 {
        Utc::now().timestamp_millis()
    }

    /// Insert one row into the hot table and return its `id`.
    async fn insert_row(pool: &SqlitePool, request_id: &str, ts: i64) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "INSERT INTO call_logs (request_id, timestamp) VALUES (?1, ?2) RETURNING id",
        )
        .bind(request_id)
        .bind(ts)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// `SELECT COUNT(*)` on a table or view.
    async fn count(pool: &SqlitePool, name: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {}", quote_ident(name)))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn cold_tables(pool: &SqlitePool) -> Vec<String> {
        let mut conn = pool.acquire().await.unwrap();
        cold_table_names(&mut conn).await.unwrap()
    }

    async fn columns_of(pool: &SqlitePool, table: &str) -> Vec<String> {
        let mut conn = pool.acquire().await.unwrap();
        table_columns(&mut conn, table).await.unwrap()
    }

    async fn infos_of(pool: &SqlitePool, table: &str) -> Vec<ColumnInfo> {
        let mut conn = pool.acquire().await.unwrap();
        column_infos(&mut conn, table).await.unwrap()
    }

    async fn index_ddls_of(pool: &SqlitePool, table: &str) -> Vec<IndexDdl> {
        let mut conn = pool.acquire().await.unwrap();
        index_ddls(&mut conn, table).await.unwrap()
    }

    /// `(name, sql)` of the two read views, name-ordered — for asserting the
    /// definitions did not change.
    async fn view_sqls(pool: &SqlitePool) -> Vec<(String, String)> {
        let mut rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, sql FROM sqlite_master WHERE type = 'view' AND name IN (?1, ?2) \
             ORDER BY name",
        )
        .bind(HOT_VIEW)
        .bind(ALL_VIEW)
        .fetch_all(pool)
        .await
        .unwrap();
        rows.sort();
        rows
    }

    /// Synthetic cold table with a legacy (narrow) column set.
    async fn create_legacy_cold(pool: &SqlitePool, name: &str) {
        sqlx::query(&format!(
            "CREATE TABLE {} (id INTEGER PRIMARY KEY, request_id TEXT NOT NULL, \
             timestamp INTEGER NOT NULL)",
            quote_ident(name)
        ))
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_legacy(pool: &SqlitePool, table: &str, request_id: &str, ts: i64) {
        sqlx::query(&format!(
            "INSERT INTO {} (request_id, timestamp) VALUES (?1, ?2)",
            quote_ident(table)
        ))
        .bind(request_id)
        .bind(ts)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn index_names(pool: &SqlitePool, table: &str) -> Vec<String> {
        index_ddls_of(pool, table)
            .await
            .into_iter()
            .map(|idx| idx.name)
            .collect()
    }

    /// `call_logs_arch_<yyyyMMdd>_<HHmmss>` (no collision suffix).
    fn is_archive_name(name: &str) -> bool {
        let Some(ts) = name.strip_prefix(ARCH_PREFIX) else {
            return false;
        };
        let bytes = ts.as_bytes();
        bytes.len() == 15
            && bytes[8] == b'_'
            && bytes
                .iter()
                .enumerate()
                .all(|(i, b)| i == 8 || b.is_ascii_digit())
    }

    #[tokio::test]
    async fn rotate_once_skips_when_hot_empty_or_all_in_window() {
        let pool = test_pool().await;
        let now = now_ms();

        // Empty hot table: MIN(timestamp) is NULL → nothing to rotate.
        assert_eq!(rotate_once(&pool, now).await.unwrap(), None);
        assert!(cold_tables(&pool).await.is_empty());

        // Everything inside the window: MIN >= cutoff → nothing to rotate.
        insert_row(&pool, "r1", now - 1_000).await;
        assert_eq!(rotate_once(&pool, now - 10_000).await.unwrap(), None);
        assert!(cold_tables(&pool).await.is_empty());
        assert_eq!(count(&pool, HOT_TABLE).await, 1);

        pool.close().await;
    }

    #[tokio::test]
    async fn rotate_once_moves_all_rows_and_resets_hot() {
        let pool = test_pool().await;
        let now = now_ms();
        for i in 0..3 {
            insert_row(&pool, &format!("r{i}"), now - 5_000 + i * 1_000).await;
        }

        let arch = rotate_once(&pool, now)
            .await
            .unwrap()
            .expect("rotation must trigger");
        assert!(is_archive_name(&arch), "unexpected archive name {arch}");

        assert_eq!(count(&pool, HOT_TABLE).await, 0, "hot table must be empty");
        assert_eq!(
            count(&pool, &arch).await,
            3,
            "all rows move to the cold table"
        );
        assert_eq!(cold_tables(&pool).await, vec![arch.clone()]);

        // `sqlite_sequence` followed the rename, so the fresh hot table
        // starts its own id sequence at 1 (cross-table tracing uses
        // `request_id`).
        assert_eq!(insert_row(&pool, "fresh", now).await, 1);

        pool.close().await;
    }

    #[tokio::test]
    async fn rotate_once_replayed_ddl_is_faithful() {
        let pool = test_pool().await;
        let now = now_ms();
        insert_row(&pool, "r1", now - 1_000).await;

        let columns_before = infos_of(&pool, HOT_TABLE).await;
        assert_eq!(
            columns_before.len(),
            29,
            "migrated call_logs has 29 columns"
        );
        let indexes_before = index_ddls_of(&pool, HOT_TABLE).await;
        assert_eq!(
            indexes_before.len(),
            6,
            "standard index set is 6 indexes (incl. session_key)"
        );

        let arch = rotate_once(&pool, now).await.unwrap().unwrap();

        // Column shape (cid, name, type, notnull, dflt_value, pk) unchanged.
        assert_eq!(infos_of(&pool, HOT_TABLE).await, columns_before);
        // AUTOINCREMENT survived the verbatim replay.
        let table_sql: String =
            sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1")
                .bind(HOT_TABLE)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            table_sql.contains("AUTOINCREMENT"),
            "replayed table must keep AUTOINCREMENT: {table_sql}"
        );
        assert_eq!(insert_row(&pool, "fresh", now).await, 1);

        // The replay order is load-bearing: the cold table must have given the
        // index names back, otherwise the hot table would have none.
        assert_eq!(index_ddls_of(&pool, HOT_TABLE).await, indexes_before);
        assert!(
            index_names(&pool, &arch).await.is_empty(),
            "cold table must not keep the standard indexes"
        );

        pool.close().await;
    }

    #[tokio::test]
    async fn rotate_once_rolls_back_atomically_on_failure() {
        let pool = test_pool().await;
        let now = now_ms();
        insert_row(&pool, "r1", now - 5_000).await;
        refresh_views(&pool, None).await.unwrap();

        // Block step ⓪ by squatting `call_logs_all` with a *table*: `DROP VIEW
        // IF EXISTS` on a table fails, and it fails only after `call_logs_hot`
        // has already been dropped in the same transaction — so the rollback
        // has to restore that DDL. (A failure after the rename is not reachable
        // from the outside: the index names stay owned by the hot table until
        // step ③, and the archive name is collision-checked.)
        sqlx::query(&format!("DROP VIEW {ALL_VIEW}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(&format!("CREATE TABLE {ALL_VIEW} (x INTEGER)"))
            .execute(&pool)
            .await
            .unwrap();
        let hot_view_before: Option<String> =
            sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'view' AND name = ?1")
                .bind(HOT_VIEW)
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert!(hot_view_before.is_some(), "precondition: hot view exists");

        let err = rotate_once(&pool, now).await.unwrap_err();
        assert!(
            err.to_string().contains(ALL_VIEW),
            "unexpected error: {err}"
        );

        assert_eq!(
            count(&pool, HOT_TABLE).await,
            1,
            "hot table must survive the rollback"
        );
        assert!(
            cold_tables(&pool).await.is_empty(),
            "the rename must be rolled back"
        );
        let hot_view_after: Option<String> =
            sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'view' AND name = ?1")
                .bind(HOT_VIEW)
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_eq!(
            hot_view_after, hot_view_before,
            "the dropped view must be restored by the rollback"
        );
        assert_eq!(count(&pool, HOT_VIEW).await, 1);
        let squat_kind: String =
            sqlx::query_scalar("SELECT type FROM sqlite_master WHERE name = ?1")
                .bind(ALL_VIEW)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(squat_kind, "table", "the obstruction must be untouched");

        pool.close().await;
    }

    #[tokio::test]
    async fn unique_archive_name_resolves_same_second_collisions() {
        let pool = test_pool().await;
        let base = format!("{ARCH_PREFIX}{}", Utc::now().format("%Y%m%d_%H%M%S"));

        sqlx::query(&format!("CREATE TABLE {} (x INTEGER)", quote_ident(&base)))
            .execute(&pool)
            .await
            .unwrap();
        let first = unique_archive_name(&pool).await.unwrap();
        assert_eq!(first, format!("{base}_1"));

        sqlx::query(&format!("CREATE TABLE {} (x INTEGER)", quote_ident(&first)))
            .execute(&pool)
            .await
            .unwrap();
        let second = unique_archive_name(&pool).await.unwrap();
        assert_eq!(second, format!("{base}_2"));

        pool.close().await;
    }

    #[tokio::test]
    async fn refresh_views_pads_missing_columns_in_hot_cid_order() {
        let pool = test_pool().await;
        let now = now_ms();
        let cold = "call_logs_arch_20200101_000000";
        create_legacy_cold(&pool, cold).await;
        insert_legacy(&pool, cold, "old", now - 1_000).await;

        refresh_views(&pool, None).await.unwrap();

        let hot_columns = columns_of(&pool, HOT_TABLE).await;
        assert_eq!(
            columns_of(&pool, HOT_VIEW).await,
            hot_columns,
            "view columns must follow the hot table's cid order"
        );

        let row = sqlx::query(&format!("SELECT * FROM {HOT_VIEW}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.columns().len(), hot_columns.len());
        // Columns the cold table has are carried over…
        assert_eq!(row.try_get::<i64, _>("id").unwrap(), 1);
        assert_eq!(row.try_get::<String, _>("request_id").unwrap(), "old");
        assert_eq!(row.try_get::<i64, _>("timestamp").unwrap(), now - 1_000);
        // …columns it lacks are padded with NULL.
        assert!(
            row.try_get::<Option<String>, _>("loss_summary")
                .unwrap()
                .is_none()
        );
        assert!(
            row.try_get::<Option<String>, _>("user_agent")
                .unwrap()
                .is_none()
        );

        pool.close().await;
    }

    #[tokio::test]
    async fn refresh_views_hot_membership_and_row_window() {
        let pool = test_pool().await;
        let now = now_ms();
        let cutoff = now - 10 * DAY_MS;

        insert_row(&pool, "h1", now - DAY_MS).await;
        insert_row(&pool, "h2", now).await;

        // Partially expired: MAX >= cutoff, MIN < cutoff → member, but only
        // its in-window row is visible through `call_logs_hot`.
        let partial = "call_logs_arch_20260101_000000";
        create_legacy_cold(&pool, partial).await;
        insert_legacy(&pool, partial, "partial-out", cutoff - DAY_MS).await;
        insert_legacy(&pool, partial, "partial-in", cutoff + DAY_MS).await;

        // Fully expired: not a member of `call_logs_hot` at all.
        let expired = "call_logs_arch_20250101_000000";
        create_legacy_cold(&pool, expired).await;
        insert_legacy(&pool, expired, "expired", cutoff - 30 * DAY_MS).await;

        refresh_views(&pool, Some(cutoff)).await.unwrap();

        assert_eq!(count(&pool, HOT_VIEW).await, 3, "2 hot + 1 in-window cold");
        assert_eq!(count(&pool, ALL_VIEW).await, 5, "every row stays reachable");
        let expired_rows: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {HOT_VIEW} WHERE request_id = 'expired'"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(expired_rows, 0);
        let out_rows: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {HOT_VIEW} WHERE request_id = 'partial-out'"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(out_rows, 0, "rows before the cutoff are filtered out");

        pool.close().await;
    }

    #[tokio::test]
    async fn refresh_views_none_and_retention_zero_semantics() {
        let pool = test_pool().await;
        let now = now_ms();
        let cold = "call_logs_arch_20200101_000000";
        insert_row(&pool, "h1", now - 400 * DAY_MS).await;
        create_legacy_cold(&pool, cold).await;
        insert_legacy(&pool, cold, "old", now - 500 * DAY_MS).await;

        // `None` is the `call_log_retention_days == 0` shape: every table is a
        // member and nothing is filtered (the scheduler round skips
        // rotate/retire entirely, so this view shape is terminal).
        refresh_views(&pool, None).await.unwrap();

        assert_eq!(count(&pool, HOT_VIEW).await, 2);
        assert_eq!(count(&pool, ALL_VIEW).await, 2);
        assert_eq!(
            view_sqls(&pool).await.len(),
            2,
            "both views must exist after a refresh"
        );
        // Nothing was rotated or retired: the cold table is untouched.
        assert_eq!(cold_tables(&pool).await, vec![cold.to_string()]);

        pool.close().await;
    }

    #[tokio::test]
    async fn retire_indexes_drops_only_expired_tables_indexes() {
        let pool = test_pool().await;
        let now = now_ms();
        let cutoff = now - 10 * DAY_MS;

        let expired = "call_logs_arch_20250101_000000";
        let in_window = "call_logs_arch_20260901_000000";
        for (name, ts) in [
            (expired, cutoff - 30 * DAY_MS),
            (in_window, cutoff + DAY_MS),
        ] {
            create_legacy_cold(&pool, name).await;
            insert_legacy(&pool, name, "r", ts).await;
            // A manually added, derived-name index: `retire_indexes` must be a
            // generic sweep, not "drop the five standard names".
            sqlx::query(&format!(
                "CREATE INDEX {} ON {} ({TIMESTAMP_COLUMN})",
                quote_ident(&format!("{name}_ts_idx")),
                quote_ident(name)
            ))
            .execute(&pool)
            .await
            .unwrap();
        }

        assert_eq!(retire_indexes(&pool, cutoff).await.unwrap(), 1);
        assert!(
            index_names(&pool, expired).await.is_empty(),
            "expired cold table must lose all its indexes"
        );
        assert_eq!(
            index_names(&pool, in_window).await,
            vec![format!("{in_window}_ts_idx")],
            "in-window cold table keeps its indexes"
        );

        // Idempotent: a second pass finds nothing to retire.
        assert_eq!(retire_indexes(&pool, cutoff).await.unwrap(), 0);
        assert_eq!(index_names(&pool, in_window).await.len(), 1);

        pool.close().await;
    }

    #[tokio::test]
    async fn sqlx_cached_statements_survive_rotation() {
        let pool = test_pool().await;
        let now = now_ms();
        let cutoff = now - 10 * DAY_MS;
        insert_row(&pool, "h-in-window", now - 2 * DAY_MS).await;
        insert_row(&pool, "h-expired", now - 30 * DAY_MS).await;
        refresh_views(&pool, Some(cutoff)).await.unwrap();

        // The read the query layer will issue, run twice so the connection's
        // prepared-statement cache actually holds it.
        let read = |pool: SqlitePool| async move {
            sqlx::query_scalar::<_, i64>(&format!(
                "SELECT COUNT(*) FROM {HOT_VIEW} WHERE {TIMESTAMP_COLUMN} >= ?1"
            ))
            .bind(cutoff)
            .fetch_one(&pool)
            .await
        };
        assert_eq!(read(pool.clone()).await.unwrap(), 1);
        assert_eq!(read(pool.clone()).await.unwrap(), 1);

        // Rotation bumps the schema cookie and rebuilds the views in one
        // transaction; the cached statement must survive (re-prepare) or be
        // retryable via `is_schema_change`.
        let arch = rotate_once(&pool, cutoff)
            .await
            .unwrap()
            .expect("rotation must trigger");
        assert!(cold_tables(&pool).await.contains(&arch));
        assert_eq!(
            read(pool.clone()).await.unwrap(),
            1,
            "cached statement must survive the rotation DDL"
        );

        // Error classification used by the retry-once wrapper: sqlx reports
        // the numeric SQLite result code, and an unrelated error must not be
        // retried. (The classification table itself is unit-tested in
        // `is_schema_change_matches_numeric_code_and_errmsg`.)
        let unrelated = sqlx::query("SELECT * FROM no_such_table_xyz")
            .execute(&pool)
            .await
            .unwrap_err();
        let code = unrelated
            .as_database_error()
            .and_then(|db| db.code())
            .map(|code| code.into_owned());
        assert_eq!(
            code.as_deref(),
            Some("1"),
            "sqlx reports the numeric SQLite result code"
        );
        assert!(
            !is_schema_change(&unrelated),
            "unrelated errors must not be retried: {unrelated}"
        );

        pool.close().await;
    }

    /// Minimal `DatabaseError` stub: sqlx has no public constructor for a
    /// driver error, so the structured (numeric code) detector is exercised
    /// through the trait. `Display` reproduces `sqlx-sqlite`'s rendering,
    /// `(code: <n>) <errmsg>`.
    #[derive(Debug)]
    struct StubDbError {
        code: &'static str,
        message: &'static str,
    }

    impl std::fmt::Display for StubDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "(code: {}) {}", self.code, self.message)
        }
    }

    impl std::error::Error for StubDbError {}

    impl sqlx::error::DatabaseError for StubDbError {
        fn message(&self) -> &str {
            self.message
        }

        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some(self.code.into())
        }

        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    fn db_error(code: &'static str, message: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(StubDbError { code, message }))
    }

    #[test]
    fn is_schema_change_matches_numeric_code_and_errmsg() {
        // Structured detector: `SQLITE_SCHEMA` is result code 17 (it has no
        // extended variants), rendered as "17" by `DatabaseError::code`.
        assert!(is_schema_change(&db_error("17", "whatever")));
        // Text detector: SQLite's own errmsg for code 17 — the only signal a
        // code-less error carries.
        assert!(is_schema_change(&sqlx::Error::Protocol(
            "(code: 17) database schema has changed".to_string()
        )));
        // Neither detector fires on unrelated errors…
        assert!(!is_schema_change(&db_error("5", "database is locked")));
        assert!(!is_schema_change(&sqlx::Error::Protocol(
            "no such table: nope".to_string()
        )));
        // …and the symbolic name is not a signal: sqlx renders the numeric
        // code plus `sqlite3_errmsg`, never `SQLITE_SCHEMA`.
        assert!(!is_schema_change(&sqlx::Error::Protocol(
            "(code: 17) SQLITE_SCHEMA".to_string()
        )));
    }

    #[tokio::test]
    async fn run_rotation_round_skips_when_retention_disabled() {
        let pool = test_pool().await;
        let now = now_ms();
        insert_row(&pool, "ancient", now - 400 * DAY_MS).await;

        assert_eq!(
            run_rotation_round(&pool, 0).await.unwrap(),
            RoundOutcome::Skipped,
            "retention 0 must skip the round explicitly"
        );
        assert!(cold_tables(&pool).await.is_empty(), "nothing was rotated");
        assert_eq!(count(&pool, HOT_TABLE).await, 1, "the row stays hot");

        pool.close().await;
    }

    #[tokio::test]
    async fn run_rotation_round_rotates_backlog() {
        let pool = test_pool().await;
        let now = now_ms();
        insert_row(&pool, "expired", now - 400 * DAY_MS).await;
        insert_row(&pool, "in-window", now - DAY_MS).await;

        let outcome = run_rotation_round(&pool, 90).await.unwrap();
        let arch = outcome
            .rotated()
            .expect("a backlog must rotate")
            .to_string();
        assert!(is_archive_name(&arch), "unexpected archive name {arch}");
        assert_eq!(outcome.retired_indexes(), 0, "fresh cold table has none");

        // Both rows moved (rotation is table-wide), and the round's refresh
        // narrowed `call_logs_hot` to the window.
        assert_eq!(count(&pool, HOT_TABLE).await, 0);
        assert_eq!(count(&pool, &arch).await, 2);
        assert_eq!(count(&pool, HOT_VIEW).await, 1, "only the in-window row");
        assert_eq!(count(&pool, ALL_VIEW).await, 2, "history stays reachable");

        pool.close().await;
    }

    /// Guardrail: the `retention_days` → cutoff arithmetic must survive
    /// `u32::MAX` (no overflow panic, no rotation — an effectively infinite
    /// window keeps every row hot and drops no cold-table index).
    #[tokio::test]
    async fn run_rotation_round_survives_max_retention() {
        let pool = test_pool().await;
        let now = now_ms();
        insert_row(&pool, "in-window", now - DAY_MS).await;
        let expired = "call_logs_arch_20200101_000000";
        create_legacy_cold(&pool, expired).await;
        insert_legacy(&pool, expired, "expired", now - 400 * DAY_MS).await;
        sqlx::query(&format!(
            "CREATE INDEX \"{expired}_ts\" ON \"{expired}\" (timestamp)"
        ))
        .execute(&pool)
        .await
        .unwrap();

        let outcome = run_rotation_round(&pool, u32::MAX).await.unwrap();
        assert_eq!(outcome.rotated(), None, "an infinite window never rotates");
        assert_eq!(outcome.retired_indexes(), 0, "nothing is out of window");
        assert_eq!(count(&pool, HOT_TABLE).await, 1);
        assert_eq!(
            count(&pool, HOT_VIEW).await,
            2,
            "every table stays a member"
        );

        pool.close().await;
    }

    #[tokio::test]
    async fn run_rotation_round_without_backlog_only_refreshes() {
        let pool = test_pool().await;
        let now = now_ms();
        insert_row(&pool, "in-window", now - DAY_MS).await;
        // A fully expired cold table: no rotation is needed, but the round's
        // refresh must still narrow the window (the cutoff is a view-body
        // literal, so an unrefreshed view would keep it as a member).
        let expired = "call_logs_arch_20200101_000000";
        create_legacy_cold(&pool, expired).await;
        insert_legacy(&pool, expired, "expired", now - 400 * DAY_MS).await;
        refresh_views(&pool, None).await.unwrap();
        assert_eq!(count(&pool, HOT_VIEW).await, 2, "precondition: all members");

        let outcome = run_rotation_round(&pool, 90).await.unwrap();
        assert_eq!(
            outcome.rotated(),
            None,
            "nothing old enough in the hot table"
        );
        assert!(cold_tables(&pool).await.contains(&expired.to_string()));
        assert_eq!(
            count(&pool, HOT_VIEW).await,
            1,
            "expired cold table drops out"
        );
        assert_eq!(count(&pool, ALL_VIEW).await, 2);

        pool.close().await;
    }

    #[tokio::test]
    async fn refresh_views_is_idempotent() {
        let pool = test_pool().await;
        let now = now_ms();
        let cutoff = now - 10 * DAY_MS;
        insert_row(&pool, "h-in-window", now - DAY_MS).await;
        insert_row(&pool, "h-expired", now - 30 * DAY_MS).await;
        let cold = "call_logs_arch_20260101_000000";
        create_legacy_cold(&pool, cold).await;
        insert_legacy(&pool, cold, "cold-in-window", cutoff + DAY_MS).await;

        refresh_views(&pool, Some(cutoff)).await.unwrap();
        let views_first = view_sqls(&pool).await;
        let hot_first = count(&pool, HOT_VIEW).await;
        let all_first = count(&pool, ALL_VIEW).await;

        refresh_views(&pool, Some(cutoff)).await.unwrap();
        assert_eq!(view_sqls(&pool).await, views_first);
        assert_eq!(count(&pool, HOT_VIEW).await, hot_first);
        assert_eq!(count(&pool, ALL_VIEW).await, all_first);

        // A second rotation round finds an empty hot table → no-op.
        assert!(rotate_once(&pool, cutoff).await.unwrap().is_some());
        assert_eq!(rotate_once(&pool, cutoff).await.unwrap(), None);

        pool.close().await;
    }
}
