//! Generic per-target retry loop.

use sag_core::AppError;
use tokio::time::{Duration, sleep};
use tracing::{info, warn};

/// Run `op` up to `max_retries + 1` times with a fixed backoff between
/// attempts. Returns the success value, or `None` if every attempt failed
/// (in which case `errors` is populated with the final error per provider).
///
/// Behavior:
/// - Non-retryable errors (4xx-class) abort the loop immediately and
///   push a `{label}: {err}` entry to `errors`.
/// - Retryable errors at the final attempt push a `{label}: {err}
///   (retries exhausted)` entry and stop.
pub async fn run_with_retries<T, F, Fut>(
    label: &str,
    max_retries: u32,
    errors: &mut Vec<String>,
    mut op: F,
) -> Option<T>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<T, AppError>>,
{
    for attempt in 0..=max_retries {
        if attempt > 0 {
            sleep(Duration::from_millis(100)).await;
        }
        info!(
            provider = %label,
            attempt,
            max_retries,
            "trying provider"
        );
        match op(attempt).await {
            Ok(value) => return Some(value),
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
                    return None;
                }
                if attempt == max_retries {
                    errors.push(format!("{label}: {err} (retries exhausted)"));
                    return None;
                }
            }
        }
    }
    None
}

/// Whether an error should trigger a retry on the same provider, or fall
/// through to the next target. 4xx-class errors are terminal; everything
/// else (5xx, network, timeout) is retryable.
pub fn is_retryable_error(err: &AppError) -> bool {
    !matches!(
        err,
        AppError::BadRequest(_) | AppError::ModelNotFound(_) | AppError::Unauthorized(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_retryable_error_excludes_4xx() {
        assert!(!is_retryable_error(&AppError::BadRequest("bad".into())));
        assert!(!is_retryable_error(&AppError::ModelNotFound("m".into())));
        assert!(!is_retryable_error(&AppError::Unauthorized("u".into())));
        assert!(is_retryable_error(&AppError::Internal("i".into())));
        assert!(is_retryable_error(&AppError::AllProvidersExhausted {
            model: "m".into(),
            reason: "r".into(),
        }));
    }
}
