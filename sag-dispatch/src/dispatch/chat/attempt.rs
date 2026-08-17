//! Chat attempt types and fact conversion helpers.

use sag_core::{ChatCompletionResponse, StreamChunk, TargetAttemptFact, TargetStatus};

use crate::dispatch::retry::RetryOutcome;

/// Unified result of a single chat attempt. The `Instant` records when
/// the upstream provider call started so the success branch can compute
/// `upstream_duration_ms` without including SAG-internal retry/circuit
/// overhead.
pub(crate) enum ChatAttempt {
    Stream(
        futures_util::stream::BoxStream<'static, StreamChunk>,
        std::time::Instant,
    ),
    NonStream(ChatCompletionResponse, std::time::Instant),
}

/// Build the single [`TargetAttemptFact`] for an actually-issued target
/// from its final retry outcome.
///
/// - retry success → one `Success` row (all attempts of the retry
///   sequence collapse into it, no retry count);
/// - retry exhaustion / non-retryable → one `Failure` row; the failure
///   tokens stay NULL (unknown — never fabricated as 0);
/// - streaming success → `None`: the stream target's success fact is
///   built at terminal time inside `build_streaming_response`, which also
///   receives the failure facts accumulated here so the request's single
///   event carries them all.
pub(crate) fn build_target_attempt_fact(
    provider: &str,
    real_model: &str,
    outcome: &RetryOutcome<ChatAttempt>,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    completed_at: chrono::DateTime<chrono::Utc>,
) -> Option<TargetAttemptFact> {
    // `started_at` is captured at the first actual provider call; fall back
    // to `completed_at` only if the closure never ran (cannot happen in
    // practice — the retry loop always issues attempt 0).
    let started_at = started_at.unwrap_or(completed_at);
    match outcome {
        RetryOutcome::Success(ChatAttempt::NonStream(resp, upstream_start)) => {
            let upstream_duration_ms = upstream_start.elapsed().as_millis() as u64;
            Some(TargetAttemptFact {
                provider: provider.to_string(),
                real_model: real_model.to_string(),
                status: TargetStatus::Success,
                failure_reason: None,
                started_at,
                completed_at,
                usage: resp.usage.clone(),
                cached_tokens: resp.usage.as_ref().and_then(|u| u.cached_tokens),
                ttft_ms: Some(upstream_duration_ms),
                upstream_duration_ms: Some(upstream_duration_ms),
            })
        }
        RetryOutcome::Success(ChatAttempt::Stream(_, _)) => None,
        RetryOutcome::Exhausted(err) | RetryOutcome::NonRetryable(err) => Some(TargetAttemptFact {
            provider: provider.to_string(),
            real_model: real_model.to_string(),
            status: TargetStatus::Failure,
            failure_reason: err.failure_reason(),
            started_at,
            completed_at,
            usage: None,
            cached_tokens: None,
            ttft_ms: None,
            upstream_duration_ms: None,
        }),
    }
}
