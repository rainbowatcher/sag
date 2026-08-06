//! Generic per-target retry loop with exponential backoff.

use sag_config::BackoffConfig;
use sag_core::AppError;
use sag_core::error::CircuitHint;
use tokio::time::{Duration, sleep};
use tracing::{info, warn};

/// Compute the exponential-backoff delay for a given attempt (0-indexed).
///
/// Formula: `min(base_ms * 2^attempt, max_ms)`, with optional full jitter
/// (`delay = random(0, computed)`) to avoid thundering-herd retries across
/// many concurrent clients. `attempt = 0` returns the base delay (no
/// doubling); `attempt = 1` doubles it; etc.
///
/// `jitter = false` is deterministic and intended for unit tests.
pub fn compute_backoff(attempt: u32, base_ms: u64, max_ms: u64, jitter: bool) -> Duration {
    // Cap the shift to avoid overflow on `1u64 << attempt` for huge attempt values.
    let exp = attempt.min(63);
    let raw = base_ms.saturating_mul(1u64 << exp);
    let capped = raw.min(max_ms);
    let ms = if jitter {
        fastrand::u64(0..=capped)
    } else {
        capped
    };
    Duration::from_millis(ms)
}

/// Result of running the retry loop. Dispatchers use the distinction between
/// retry exhaustion and non-retryable failures to update route target circuit
/// state only when a target has actually spent its retry budget.
///
/// [`Exhausted`](RetryOutcome::Exhausted) carries the last error so the
/// caller can select an appropriate circuit-breaker open-duration, and
/// [`NonRetryable`](RetryOutcome::NonRetryable) carries the error so the
/// caller can attribute a non-retryable 4xx to the client (return straight
/// to the client) vs provider persistence (failover).
#[derive(Debug)]
pub enum RetryOutcome<T> {
    Success(T),
    NonRetryable(AppError),
    Exhausted(AppError),
}

impl<T> RetryOutcome<T> {
    pub fn into_option(self) -> Option<T> {
        match self {
            Self::Success(value) => Some(value),
            Self::NonRetryable(_) | Self::Exhausted(_) => None,
        }
    }
}

/// Run `op` up to `max_retries + 1` times with exponential backoff (plus
/// jitter) between attempts. Returns the success value, or `None` if every
/// attempt failed (in which case `errors` is populated with the final
/// error per provider).
pub async fn run_with_retries<T, F, Fut>(
    label: &str,
    max_retries: u32,
    backoff: &BackoffConfig,
    errors: &mut Vec<String>,
    op: F,
) -> Option<T>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, AppError>>,
{
    run_with_retries_outcome(label, max_retries, backoff, errors, op)
        .await
        .into_option()
}

/// Run `op` up to `max_retries + 1` times and retain the terminal reason.
///
/// Backoff schedule is driven by `backoff` (typically read from
/// [`GatewayConfig::backoff`](sag_config::GatewayConfig::backoff)):
/// attempt 1 → `0..=backoff.base_ms`, attempt 2 → `0..=backoff.base_ms * 2`,
/// … capped at `backoff.max_ms`. See [`compute_backoff`].
///
/// Behavior:
/// - Non-retryable errors (4xx except 408/429 and non-quota 403) abort the
///   loop immediately and push a `{label}: {err}` entry to `errors`.
/// - Retryable errors at the final attempt push a `{label}: {err}
///   (retries exhausted)` entry and stop.
pub async fn run_with_retries_outcome<T, F, Fut>(
    label: &str,
    max_retries: u32,
    backoff: &BackoffConfig,
    errors: &mut Vec<String>,
    mut op: F,
) -> RetryOutcome<T>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, AppError>>,
{
    for attempt in 0..=max_retries {
        if attempt > 0 {
            let delay =
                compute_backoff(attempt - 1, backoff.base_ms, backoff.max_ms, backoff.jitter);
            info!(
                provider = %label,
                backoff_ms = delay.as_millis() as u64,
                attempt,
                "backing off before retry"
            );
            sleep(delay).await;
        }
        info!(
            provider = %label,
            attempt,
            max_retries,
            "trying provider"
        );
        match op(attempt).await {
            Ok(value) => return RetryOutcome::Success(value),
            Err(err) => {
                let retryable = is_retryable_error(&err);
                warn!(
                    provider = %label,
                    error = %err,
                    retryable,
                    attempt,
                    "provider returned error"
                );
                if !retryable {
                    errors.push(format!("{label}: {err}"));
                    return RetryOutcome::NonRetryable(err);
                }
                if attempt == max_retries {
                    errors.push(format!("{label}: {err} (retries exhausted)"));
                    return RetryOutcome::Exhausted(err);
                }
            }
        }
    }
    RetryOutcome::Exhausted(AppError::Internal("unreachable".into()))
}

/// Whether an error should trigger a retry on the same provider, or fall
/// through to the next target.
///
/// The following errors are **not** retryable:
/// - `BadRequest`, `ModelNotFound`, `Unauthorized`
/// - `Upstream` with a 4xx status code **except** 408 (timeout), 429 (rate
///   limited) and quota-403 (quota/billing exhausted) — 408/429/403-quota
///   are treated as transient and retryable (429/403-quota already have
///   their own circuit-breaker cooldown path via [`CircuitHint`]).
///
/// Everything else (5xx, network, timeout, internal) is retryable.
pub fn is_retryable_error(err: &AppError) -> bool {
    match err {
        AppError::BadRequest(_) | AppError::ModelNotFound(_) | AppError::Unauthorized(_) => false,
        AppError::Upstream { status, .. } => match status {
            // A 403 is retryable only when it signals quota exhaustion
            // (self-healing — the provider tops up or the billing cycle
            // resets). A non-quota 403 (perms / region / content policy) is
            // a provider-persistent failure: not retryable, failover instead.
            403 => err.circuit_hint() == CircuitHint::QuotaExhausted,
            429 | 408 | 500..=599 => true,
            _ => false,
        },
        _ => true,
    }
}

/// Whether a non-retryable upstream 4xx is attributable to the client
/// (request shape) rather than provider persistence. Only meaningful for
/// non-retryable 4xx: 401/403/404 are attributed to provider persistence
/// (failover + move pin), every other 4xx is client-caused (return the
/// error to the client, no failover, no pin movement, no circuit change).
///
/// A 403 reaching this predicate is always a non-quota 403 - quota-403 is
/// retryable and never lands in [`RetryOutcome::NonRetryable`]. 408/429 are
/// likewise retryable (transient provider failures, not client-caused) and
/// excluded for defense-in-depth, even though they never reach this
/// predicate via the `NonRetryable` path.
pub fn is_client_caused_4xx(err: &AppError) -> bool {
    match err {
        AppError::Upstream { status, .. } => {
            let s = *status;
            (400..500).contains(&s) && !matches!(s, 401 | 403 | 404 | 408 | 429)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn is_retryable_error_excludes_4xx() {
        assert!(!is_retryable_error(&AppError::BadRequest("bad".into())));
        assert!(!is_retryable_error(&AppError::ModelNotFound("m".into())));
        assert!(!is_retryable_error(&AppError::Unauthorized("u".into())));
        assert!(is_retryable_error(&AppError::Internal("i".into())));
        assert!(is_retryable_error(&AppError::TtfbTimeout {
            provider: "p".into(),
            secs: 60,
        }));
        assert!(is_retryable_error(&AppError::AllProvidersExhausted {
            model: "m".into(),
            reason: "r".into(),
        }));

        // Upstream 4xx (except 408/429 and quota-403) are not retryable —
        // client-side / provider-persistent error.
        assert!(!is_retryable_error(&AppError::Upstream {
            provider: "p".into(),
            status: 400,
            body: "bad".into()
        }));
        assert!(!is_retryable_error(&AppError::Upstream {
            provider: "p".into(),
            status: 403,
            body: "permission denied".into()
        }));
        assert!(!is_retryable_error(&AppError::Upstream {
            provider: "p".into(),
            status: 422,
            body: "unprocessable".into()
        }));

        // A 403 whose body signals quota/billing exhaustion is retryable
        // (self-healing) — goes through the circuit-breaker cooldown path.
        assert!(is_retryable_error(&AppError::Upstream {
            provider: "p".into(),
            status: 403,
            body: "quota exceeded".into()
        }));
        assert!(!is_retryable_error(&AppError::Upstream {
            provider: "p".into(),
            status: 403,
            body: "rate limit reached".into()
        }));

        // 429 (rate limited) is retryable — short-circuit-breaker cooldown.
        assert!(is_retryable_error(&AppError::Upstream {
            provider: "p".into(),
            status: 429,
            body: "rate limited".into()
        }));

        // 408 (timeout) is retryable — transient.
        assert!(is_retryable_error(&AppError::Upstream {
            provider: "p".into(),
            status: 408,
            body: "timeout".into()
        }));

        // 5xx upstream errors are retryable.
        assert!(is_retryable_error(&AppError::Upstream {
            provider: "p".into(),
            status: 500,
            body: "internal".into()
        }));
        assert!(is_retryable_error(&AppError::Upstream {
            provider: "p".into(),
            status: 503,
            body: "unavailable".into()
        }));
    }

    #[test]
    fn is_client_caused_4xx_attributes_status_codes() {
        let upstream = |status: u16| AppError::Upstream {
            provider: "p".into(),
            status,
            body: "body".into(),
        };

        // Client-caused: request-shape / unknown 4xx.
        for status in [400, 402, 413, 418, 422, 451] {
            assert!(
                is_client_caused_4xx(&upstream(status)),
                "status {status} should be client-caused"
            );
        }

        // Provider-persistent: auth / missing-model / non-quota 403.
        for status in [401, 403, 404] {
            assert!(
                !is_client_caused_4xx(&upstream(status)),
                "status {status} should be provider-persistent"
            );
        }

        // Non-4xx and non-Upstream errors are never client-caused.
        assert!(!is_client_caused_4xx(&upstream(500)));
        assert!(!is_client_caused_4xx(&AppError::BadRequest("bad".into())));
        assert!(!is_client_caused_4xx(&AppError::Internal("i".into())));
        // 408/429 (retryable) are transient provider failures, not
        // client-caused; they also never reach this predicate via NonRetryable
        // (filtered earlier by `is_retryable_error` -> Exhausted path), but
        // the predicate is kept self-consistent for defense-in-depth.
        assert!(!is_client_caused_4xx(&upstream(408)));
        assert!(!is_client_caused_4xx(&upstream(429)));
    }

    #[tokio::test]
    async fn non_retryable_outcome_carries_the_error() {
        // The dispatch loop needs the status code from the non-retryable
        // error to attribute a 4xx to the client vs provider persistence.
        let mut errors = Vec::new();
        let backoff = BackoffConfig::default();
        let outcome: RetryOutcome<()> =
            run_with_retries_outcome("test", 0, &backoff, &mut errors, |_attempt| async move {
                Err::<(), _>(AppError::Upstream {
                    provider: "p".into(),
                    status: 400,
                    body: "bad param".into(),
                })
            })
            .await;
        match outcome {
            RetryOutcome::NonRetryable(err) => {
                assert!(matches!(err, AppError::Upstream { status: 400, .. }));
            }
            other => panic!("expected NonRetryable, got {other:?}"),
        }
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn backoff_doubles_each_attempt_without_jitter() {
        // No jitter → deterministic. 100, 200, 400, 800, …
        assert_eq!(
            compute_backoff(0, 100, 5_000, false),
            Duration::from_millis(100)
        );
        assert_eq!(
            compute_backoff(1, 100, 5_000, false),
            Duration::from_millis(200)
        );
        assert_eq!(
            compute_backoff(2, 100, 5_000, false),
            Duration::from_millis(400)
        );
        assert_eq!(
            compute_backoff(3, 100, 5_000, false),
            Duration::from_millis(800)
        );
    }

    #[test]
    fn backoff_caps_at_max() {
        // Attempt 10 would be 100 * 1024 = 102400ms, capped at 5000ms.
        assert_eq!(
            compute_backoff(10, 100, 5_000, false),
            Duration::from_millis(5_000)
        );
        // Attempt 63 saturates the shift without overflow.
        assert_eq!(
            compute_backoff(63, 100, 5_000, false),
            Duration::from_millis(5_000)
        );
    }

    #[test]
    fn backoff_with_jitter_stays_in_range() {
        for attempt in 0..10 {
            for _ in 0..20 {
                let d = compute_backoff(attempt, 100, 5_000, true);
                let cap = (100u64 << attempt.min(63)).min(5_000);
                assert!(
                    d.as_millis() <= cap as u128,
                    "jittered backoff {d:?} exceeds cap {cap}ms for attempt {attempt}"
                );
            }
        }
    }

    #[tokio::test]
    async fn retries_actually_sleep_with_backoff() {
        // 3 attempts: attempt 0 (no sleep), then backoff before attempt 1
        // (up to 100ms with jitter), then backoff before attempt 2 (up to
        // 200ms with jitter). All 3 attempts fail to exhaust the retry budget.
        // The unit tests for `compute_backoff` cover the exact formula;
        // full jitter means the actual delay can be as low as 0ms, so we
        // assert an upper bound rather than a lower bound here.
        let start = Instant::now();
        let mut errors = Vec::new();
        let mut calls = 0u32;
        let backoff = BackoffConfig::default();
        let result: Option<()> = run_with_retries("test", 2, &backoff, &mut errors, |_attempt| {
            calls += 1;
            async move { Err::<(), _>(AppError::Internal("fail".into())) }
        })
        .await;
        let elapsed = start.elapsed();
        assert!(result.is_none(), "should have exhausted retries");
        assert_eq!(calls, 3, "op should be called once per attempt");
        assert_eq!(errors.len(), 1);
        // Upper bound: even with max jitter, two backoffs cannot exceed
        // 100ms + 200ms = 300ms (plus scheduling slack).
        assert!(
            elapsed < Duration::from_millis(500),
            "backoff exceeded upper bound, got {elapsed:?}"
        );
    }

    #[test]
    fn backoff_config_defaults_match_legacy_constants() {
        // The pre-configurable retry.rs used 100ms base, 5000ms cap, jitter on.
        // Defaults must keep the same behavior so existing deployments
        // behave identically after upgrading.
        let cfg = BackoffConfig::default();
        assert_eq!(cfg.base_ms, 100);
        assert_eq!(cfg.max_ms, 5_000);
        assert!(cfg.jitter);
    }
}
