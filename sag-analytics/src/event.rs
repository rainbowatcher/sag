//! Formal analytics event: one request's complete fact set.
//!
//! An [`AnalyticsEvent`] carries exactly one route-level [`RequestResultFact`]
//! plus the full set of [`TargetAttemptFact`]s for the same request (0..n).
//! It is the unit of atomic persistence: either the whole event lands in
//! `request_results` / `target_attempts`, or nothing does.

use sag_core::{RequestResultFact, TargetAttemptFact};

/// One atomic analytics event: exactly one request-level final result plus
/// all actually-issued target attempts of that request.
///
/// `result.request_id` is the idempotency key — re-writing the same event
/// must never duplicate `request_results` / `target_attempts` rows.
#[derive(Debug, Clone)]
pub struct AnalyticsEvent {
    /// The request-level final fact (exactly one per analyzed request).
    pub result: RequestResultFact,
    /// All actually-issued target attempts (0..n). Empty for
    /// `client_error` / `no_route` / `all_disabled` and for
    /// `client_cancelled` that happened with no in-flight attempt.
    pub targets: Vec<TargetAttemptFact>,
}

impl AnalyticsEvent {
    /// Build an event with no target attempts (client_error / no_route /
    /// all_disabled / client_cancelled-without-in-flight-attempt cases).
    pub fn new(result: RequestResultFact) -> Self {
        Self {
            result,
            targets: Vec::new(),
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use sag_core::{FinalStatus, TargetStatus, Usage};
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
    use std::str::FromStr;
    use std::time::Duration;
    use uuid::Uuid;

    /// Fixed reference instant so tests can assert on stored timestamps.
    pub const REF_TS_MS: i64 = 1_700_000_000_000;

    pub fn make_result(
        request_id: Uuid,
        status: FinalStatus,
        route_name: Option<&str>,
    ) -> RequestResultFact {
        use chrono::{TimeZone, Utc};
        let started_at = Utc.timestamp_millis_opt(REF_TS_MS).unwrap();
        RequestResultFact {
            request_id,
            route_name: route_name.map(str::to_string),
            requested_model: "test-model".to_string(),
            final_status: status,
            failure_reason: None,
            stream: false,
            started_at,
            completed_at: started_at + chrono::Duration::milliseconds(250),
            usage: None,
            cached_tokens: None,
            cache_creation_tokens: None,
            ttft_ms: None,
            upstream_duration_ms: None,
        }
    }

    pub fn make_attempt(
        provider: &str,
        real_model: &str,
        status: TargetStatus,
    ) -> TargetAttemptFact {
        use chrono::{TimeZone, Utc};
        let started_at = Utc.timestamp_millis_opt(REF_TS_MS).unwrap();
        TargetAttemptFact {
            provider: provider.to_string(),
            real_model: real_model.to_string(),
            status,
            failure_reason: None,
            started_at,
            completed_at: started_at + chrono::Duration::milliseconds(200),
            usage: None,
            cached_tokens: None,
            ttft_ms: None,
            upstream_duration_ms: None,
        }
    }

    pub fn make_success_usage() -> Usage {
        Usage {
            prompt_tokens: 10,
            completion_tokens: 20,
            total_tokens: 30,
            cached_tokens: None,
            cache_creation_tokens: None,
            reasoning_tokens: None,
        }
    }

    pub fn make_event(
        request_id: Uuid,
        status: FinalStatus,
        route_name: Option<&str>,
        targets: Vec<TargetAttemptFact>,
    ) -> AnalyticsEvent {
        AnalyticsEvent {
            result: make_result(request_id, status, route_name),
            targets,
        }
    }

    /// In-memory pool with foreign keys enabled and migrations applied,
    /// mirroring `SqliteLogStore::new_memory` connect options.
    pub async fn test_pool() -> SqlitePool {
        let options = SqliteConnectOptions::from_str(":memory:")
            .unwrap()
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }
}
