//! `session_registry`: durable per-session metadata behind the logs page
//! session info card.
//!
//! One row per hashed session identity (`SessionKey::map_id`, the same digest
//! `call_logs.session_key` carries). Writes are **earliest-requester-wins**:
//! the request with the smallest `first_seen` owns the row's source,
//! conversation id and opener preview, while `last_seen` always keeps the
//! newest observation (`first_seen` keeps the smallest). That is what makes
//! the preview semantically "the session opener" without any extra
//! bookkeeping — a later request carrying a longer conversation history can
//! never overwrite the earliest opener.
//!
//! The winner is decided by the request timestamp, not by write order, so the
//! row is identical no matter how writes interleave. That matters because the
//! collector's saturated-queue fallback ([`crate::collector`]) writes a fact
//! directly instead of through the FIFO channel, so two facts of the same
//! session can reach SQLite out of order.
//!
//! The table is independent of the `call_logs` rotation (rotation only renames
//! `call_logs` / `call_logs_arch_*`), so retention is handled here by
//! [`prune_session_registry`], driven from the gateway's maintenance round.

use chrono::{DateTime, Utc};
use sqlx::sqlite::SqlitePool;
use tracing::warn;

use crate::store::SessionRecord;

/// Milliseconds per day, mirroring the rotation module's day constant.
const DAY_MS: i64 = 86_400_000;

/// One session registry write, as produced by the dispatch layer.
///
/// Re-exported from [`sag_core::session_preview`] so the dispatch layer builds
/// the fact once and the analytics layer stores it without a conversion step.
pub use sag_core::SessionRegistryFact;

/// Insert the session if unknown, otherwise merge the row with `fact`:
/// `first_seen` keeps the minimum, `last_seen` the maximum, and
/// `session_source` / `conversation_id` / `first_message_preview` follow the
/// earliest request (the smallest `first_seen`). A tie on `first_seen` keeps
/// the already stored values, so an equal timestamp never displaces the
/// opener.
pub async fn upsert_session(
    pool: &SqlitePool,
    fact: &SessionRegistryFact,
) -> Result<(), sqlx::Error> {
    // The three `CASE` overwrites compare against the *stored* row's
    // `first_seen`, so they are emitted before the `first_seen` / `last_seen`
    // merge: `DO UPDATE SET` assignments are applied top to bottom, and
    // keeping the comparisons ahead of the `MIN(...)` keeps them independent
    // of whether a dialect lets a later assignment observe the merged value.
    // (SQLite 3.54 evaluates every `SET` expression against the pre-update
    // row, so either order currently behaves the same — this ordering is what
    // makes the intent unambiguous.)
    sqlx::query(
        r#"
        INSERT INTO session_registry
            (session_key, session_source, conversation_id,
             first_message_preview, first_seen, last_seen)
        VALUES (?, ?, ?, ?, ?, ?)
        ON CONFLICT(session_key) DO UPDATE SET
            session_source = CASE
                WHEN excluded.first_seen < session_registry.first_seen
                THEN excluded.session_source
                ELSE session_registry.session_source END,
            conversation_id = CASE
                WHEN excluded.first_seen < session_registry.first_seen
                THEN excluded.conversation_id
                ELSE session_registry.conversation_id END,
            first_message_preview = CASE
                WHEN excluded.first_seen < session_registry.first_seen
                THEN excluded.first_message_preview
                ELSE session_registry.first_message_preview END,
            first_seen = MIN(session_registry.first_seen, excluded.first_seen),
            last_seen = MAX(session_registry.last_seen, excluded.last_seen)
        "#,
    )
    .bind(&fact.session_key)
    .bind(&fact.session_source)
    .bind(fact.conversation_id.as_deref())
    .bind(fact.first_message_preview.as_deref())
    .bind(fact.first_seen.timestamp_millis())
    .bind(fact.last_seen.timestamp_millis())
    .execute(pool)
    .await?;
    Ok(())
}

/// Raw `session_registry` row (timestamps stored as unix milliseconds).
#[derive(Debug, sqlx::FromRow)]
struct SessionRow {
    session_key: String,
    session_source: String,
    conversation_id: Option<String>,
    first_message_preview: Option<String>,
    first_seen: i64,
    last_seen: i64,
}

impl SessionRow {
    fn into_record(self) -> SessionRecord {
        SessionRecord {
            session_key: self.session_key,
            session_source: self.session_source,
            conversation_id: self.conversation_id,
            first_message_preview: self.first_message_preview,
            first_seen: ms_to_datetime(self.first_seen),
            last_seen: ms_to_datetime(self.last_seen),
        }
    }
}

/// Millisecond timestamp → `DateTime<Utc>`, falling back to the epoch with a
/// warning instead of panicking: the rows are ours, so a parse failure means
/// corruption and analytics reads must not crash the server.
fn ms_to_datetime(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or_else(|| {
        warn!(
            ts_ms = ms,
            "invalid millisecond timestamp in session_registry row; using epoch"
        );
        DateTime::from_timestamp_millis(0).expect("epoch is a valid timestamp")
    })
}

/// Point lookup by hashed session identity (`session_key` is the primary key,
/// so this is a single index probe).
pub async fn get_session(
    pool: &SqlitePool,
    key: &str,
) -> Result<Option<SessionRecord>, sqlx::Error> {
    let row = sqlx::query_as::<_, SessionRow>(
        r#"
        SELECT session_key, session_source, conversation_id,
               first_message_preview, first_seen, last_seen
        FROM session_registry
        WHERE session_key = ?
        "#,
    )
    .bind(key)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(SessionRow::into_record))
}

/// Delete rows whose `last_seen` is older than `now - retention_days * 1 day`,
/// returning how many rows were removed.
///
/// `retention_days == 0` disables the feature entirely and deletes nothing —
/// the same "0 = keep forever / cleanup off" semantics as
/// `call_log_retention_days`. A non-zero value, however, deletes the rows
/// outright: unlike `call_logs` there is no archive table and no rotation, so
/// expired session metadata is gone for good (the card simply renders nothing
/// for that session). The cutoff is exclusive (`last_seen < cutoff`), so a row
/// seen exactly at the cutoff boundary survives.
pub async fn prune_session_registry(
    pool: &SqlitePool,
    retention_days: u32,
    now: DateTime<Utc>,
) -> Result<u64, sqlx::Error> {
    if retention_days == 0 {
        return Ok(0);
    }
    // Saturating on both ends: maintenance never panics, and a saturated
    // cutoff means "keep everything".
    let cutoff_ms = now
        .timestamp_millis()
        .saturating_sub(i64::from(retention_days).saturating_mul(DAY_MS));

    let deleted = sqlx::query("DELETE FROM session_registry WHERE last_seen < ?")
        .bind(cutoff_ms)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open_analytics_pool;
    use chrono::TimeZone;

    fn ts(ms: i64) -> DateTime<Utc> {
        Utc.timestamp_millis_opt(ms).unwrap()
    }

    async fn pool() -> SqlitePool {
        open_analytics_pool(":memory:").await.unwrap()
    }

    fn fact(key: &str, source: &str, at_ms: i64) -> SessionRegistryFact {
        let at = ts(at_ms);
        SessionRegistryFact {
            session_key: key.to_string(),
            session_source: source.to_string(),
            conversation_id: Some(format!("conv-{key}")),
            first_message_preview: Some(format!("preview-{key}")),
            first_seen: at,
            last_seen: at,
        }
    }

    #[tokio::test]
    async fn upsert_first_write_stores_every_field() {
        let pool = pool().await;
        let f = fact("abc123", "meta", 1_700_000_000_000);
        upsert_session(&pool, &f).await.unwrap();

        let rec = get_session(&pool, "abc123").await.unwrap().unwrap();
        assert_eq!(rec.session_key, "abc123");
        assert_eq!(rec.session_source, "meta");
        assert_eq!(rec.conversation_id.as_deref(), Some("conv-abc123"));
        assert_eq!(rec.first_message_preview.as_deref(), Some("preview-abc123"));
        assert_eq!(rec.first_seen, ts(1_700_000_000_000));
        assert_eq!(rec.last_seen, ts(1_700_000_000_000));
        pool.close().await;
    }

    /// Earliest request wins: a write whose `first_seen` is later only
    /// advances `last_seen`. Everything else — source, conversation id, opener
    /// preview and `first_seen` — keeps the earliest observation.
    #[tokio::test]
    async fn second_write_only_advances_last_seen() {
        let pool = pool().await;
        let first = fact("abc123", "meta", 1_700_000_000_000);
        upsert_session(&pool, &first).await.unwrap();

        let mut second = first.clone();
        second.last_seen = ts(1_700_000_500_000);
        // A later request would carry a different source / longer history;
        // none of it may overwrite the earliest request's values.
        second.session_source = "top".to_string();
        second.conversation_id = Some("conv-later".to_string());
        second.first_message_preview = Some("later preview".to_string());
        second.first_seen = ts(1_700_000_500_000);
        upsert_session(&pool, &second).await.unwrap();

        let rec = get_session(&pool, "abc123").await.unwrap().unwrap();
        assert_eq!(rec.last_seen, ts(1_700_000_500_000), "last_seen advances");
        assert_eq!(rec.first_seen, ts(1_700_000_000_000), "first_seen frozen");
        assert_eq!(rec.session_source, "meta", "source frozen");
        assert_eq!(rec.conversation_id.as_deref(), Some("conv-abc123"));
        assert_eq!(rec.first_message_preview.as_deref(), Some("preview-abc123"));
        pool.close().await;
    }

    /// Out-of-order arrival: the collector's saturated-queue fallback writes a
    /// fact directly, so a mid-conversation request can be persisted before the
    /// session opener. The opener still wins on arrival-later, because the
    /// winner is decided by `first_seen`, not by write order.
    #[tokio::test]
    async fn out_of_order_write_promotes_the_earliest_opener() {
        let pool = pool().await;

        // Arrives first, but started later: a mid-conversation request.
        let later = SessionRegistryFact {
            session_key: "abc123".to_string(),
            session_source: "top".to_string(),
            conversation_id: Some("conv-later".to_string()),
            first_message_preview: Some("later".to_string()),
            first_seen: ts(1_700_000_500_000),
            last_seen: ts(1_700_000_500_000),
        };
        upsert_session(&pool, &later).await.unwrap();

        // Arrives second, but is the session opener.
        let opener = SessionRegistryFact {
            session_key: "abc123".to_string(),
            session_source: "meta".to_string(),
            conversation_id: Some("conv-earlier".to_string()),
            first_message_preview: Some("earlier".to_string()),
            first_seen: ts(1_700_000_000_000),
            last_seen: ts(1_700_000_000_000),
        };
        upsert_session(&pool, &opener).await.unwrap();

        let rec = get_session(&pool, "abc123").await.unwrap().unwrap();
        assert_eq!(
            rec.first_message_preview.as_deref(),
            Some("earlier"),
            "the earliest request owns the opener preview"
        );
        assert_eq!(rec.conversation_id.as_deref(), Some("conv-earlier"));
        assert_eq!(rec.session_source, "meta");
        assert_eq!(rec.first_seen, ts(1_700_000_000_000));
        assert_eq!(
            rec.last_seen,
            ts(1_700_000_500_000),
            "last_seen keeps the newest observation"
        );
        pool.close().await;
    }

    /// Equal `first_seen` is a tie: the stored opener is not displaced, so the
    /// first writer keeps its preview / conversation id / source.
    #[tokio::test]
    async fn equal_timestamps_keep_the_first_writer() {
        let pool = pool().await;
        let at = 1_700_000_000_000;
        upsert_session(&pool, &fact("abc123", "meta", at))
            .await
            .unwrap();

        let same_ts = SessionRegistryFact {
            session_key: "abc123".to_string(),
            session_source: "top".to_string(),
            conversation_id: Some("conv-other".to_string()),
            first_message_preview: Some("other preview".to_string()),
            first_seen: ts(at),
            last_seen: ts(at),
        };
        upsert_session(&pool, &same_ts).await.unwrap();

        let rec = get_session(&pool, "abc123").await.unwrap().unwrap();
        assert_eq!(rec.session_source, "meta");
        assert_eq!(rec.conversation_id.as_deref(), Some("conv-abc123"));
        assert_eq!(rec.first_message_preview.as_deref(), Some("preview-abc123"));
        assert_eq!(rec.first_seen, ts(at));
        assert_eq!(rec.last_seen, ts(at));
        pool.close().await;
    }

    #[tokio::test]
    async fn get_session_reports_miss() {
        let pool = pool().await;
        assert!(get_session(&pool, "nope").await.unwrap().is_none());
        pool.close().await;
    }

    /// Prune drops rows strictly older than the cutoff and keeps the rest,
    /// including the row sitting exactly on the cutoff boundary.
    #[tokio::test]
    async fn prune_deletes_old_rows_and_keeps_recent() {
        let pool = pool().await;
        let now = ts(1_700_000_000_000);
        let cutoff = now.timestamp_millis() - 7 * DAY_MS;

        upsert_session(&pool, &fact("expired", "meta", cutoff - 1))
            .await
            .unwrap();
        upsert_session(&pool, &fact("boundary", "meta", cutoff))
            .await
            .unwrap();
        upsert_session(&pool, &fact("fresh", "fb", now.timestamp_millis()))
            .await
            .unwrap();

        let deleted = prune_session_registry(&pool, 7, now).await.unwrap();
        assert_eq!(deleted, 1);
        assert!(get_session(&pool, "expired").await.unwrap().is_none());
        assert!(get_session(&pool, "boundary").await.unwrap().is_some());
        assert!(get_session(&pool, "fresh").await.unwrap().is_some());
        pool.close().await;
    }

    /// `retention_days == 0` disables cleanup entirely (keep forever).
    #[tokio::test]
    async fn prune_with_zero_retention_keeps_everything() {
        let pool = pool().await;
        let now = ts(1_700_000_000_000);
        upsert_session(&pool, &fact("ancient", "meta", 1))
            .await
            .unwrap();

        let deleted = prune_session_registry(&pool, 0, now).await.unwrap();
        assert_eq!(deleted, 0);
        assert!(get_session(&pool, "ancient").await.unwrap().is_some());
        pool.close().await;
    }

    /// `pc` / `fb` sources have no conversation id (and may have no preview):
    /// the NULLs must round-trip as `None`.
    #[tokio::test]
    async fn null_optional_fields_round_trip() {
        let pool = pool().await;
        let at = ts(1_700_000_000_000);
        let f = SessionRegistryFact {
            session_key: "fbkey".to_string(),
            session_source: "fb".to_string(),
            conversation_id: None,
            first_message_preview: None,
            first_seen: at,
            last_seen: at,
        };
        upsert_session(&pool, &f).await.unwrap();

        let rec = get_session(&pool, "fbkey").await.unwrap().unwrap();
        assert_eq!(rec.session_source, "fb");
        assert_eq!(rec.conversation_id, None);
        assert_eq!(rec.first_message_preview, None);
        pool.close().await;
    }
}
