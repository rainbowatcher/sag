//! In-memory circuit breaker for route targets.
//!
//! A route target is opened after its per-request retry budget is exhausted.
//! While open, later requests skip that target and immediately try the next
//! target in route order. The open duration depends on the error that caused
//! the exhaustion:
//!
//! | Trigger | Duration |
//! |---|---|
//! | Quota / billing exhausted | 1 hour |
//! | HTTP 429 rate-limit | 10 seconds |
//! | Generic retryable failure | 60 seconds |
//!
//! ## Penalized targets (FillFirst only)
//!
//! When a circuit closes, the target is *not* immediately promoted back to
//! primary. It stays **penalized** — skipped by the dispatch loop until the
//! current active target (the one that replaced it) also exhausts its retries.
//! This prevents flip-flopping between two targets every 60 seconds.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// Stable identity for a resolved route target.
///
/// `model` is the client-requested model for this dispatch. `real_model` is the
/// concrete upstream model after route resolution. Keeping both avoids sharing
/// circuit state between two logical routes that happen to use the same provider.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RouteTargetKey {
    pub model: String,
    pub provider: String,
    pub real_model: String,
}

impl RouteTargetKey {
    pub fn new(model: &str, provider: &str, real_model: &str) -> Self {
        Self {
            model: model.to_string(),
            provider: provider.to_string(),
            real_model: real_model.to_string(),
        }
    }
}

/// Minimal circuit state for all route targets.
#[derive(Debug, Default)]
pub struct RouteCircuitBreaker {
    open_until: HashMap<RouteTargetKey, Instant>,
    /// Targets that were previously open but whose circuit has since
    /// closed. In FillFirst mode these are skipped until the current
    /// active target also exhausts retries. Keyed by model name.
    penalized: HashMap<String, HashSet<RouteTargetKey>>,
}

impl RouteCircuitBreaker {
    /// Return how long the target remains open. `None` means the target may be
    /// tried now. Expired entries are pruned on writes, keeping this hot path
    /// read-only for concurrent dispatch.
    pub fn open_remaining(&self, key: &RouteTargetKey) -> Option<Duration> {
        let now = Instant::now();
        self.open_until
            .get(key)
            .copied()
            .and_then(|until| (until > now).then(|| until.duration_since(now)))
    }

    /// Mark a target unavailable after its retry budget has been exhausted.
    /// `duration_secs` is typically derived from [`sag_core::CircuitHint`].
    pub fn open(&mut self, key: RouteTargetKey, duration_secs: u64) {
        let now = Instant::now();
        self.prune_expired(now);
        self.open_until
            .insert(key.clone(), now + Duration::from_secs(duration_secs));
        // Additionally penalize: even after the circuit closes, FillFirst
        // will skip this target until the current active one also fails.
        self.penalized
            .entry(key.model.clone())
            .or_default()
            .insert(key);
    }

    /// Mark a target available after a successful upstream response.
    /// Does NOT clear penalized status — a penalized target stays
    /// deprioritized until explicitly cleared via [`clear_penalized`].
    pub fn close(&mut self, key: &RouteTargetKey) {
        self.open_until.remove(key);
    }

    /// Whether this target is penalized (previously tripped, circuit closed,
    /// but not yet cleared). FillFirst dispatch skips penalized targets in
    /// the first pass.
    pub fn is_penalized(&self, model: &str, key: &RouteTargetKey) -> bool {
        self.penalized
            .get(model)
            .map(|s| s.contains(key))
            .unwrap_or(false)
    }

    /// Clear ALL penalized targets for a given model. Called when the
    /// current active target (the one that replaced penalized ones) also
    /// exhausts retries — at that point the penalized targets are eligible
    /// again.
    pub fn clear_penalized(&mut self, model: &str) {
        let now = Instant::now();
        self.prune_expired(now);
        self.penalized.remove(model);
    }

    fn prune_expired(&mut self, now: Instant) {
        self.open_until.retain(|_, until| *until > now);
    }

    #[cfg(test)]
    pub fn with_open_duration(_open_duration: Duration) -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(provider: &str, real_model: &str) -> RouteTargetKey {
        RouteTargetKey::new("gpt-4o", provider, real_model)
    }

    #[test]
    fn target_starts_closed() {
        let breaker = RouteCircuitBreaker::default();

        assert!(breaker.open_remaining(&key("openai", "gpt-4o")).is_none());
    }

    #[test]
    fn open_marks_target_unavailable() {
        let mut breaker = RouteCircuitBreaker::default();
        let target = key("openai", "gpt-4o");

        breaker.open(target, 5);

        assert!(breaker.open_remaining(&key("openai", "gpt-4o")).is_some());
        assert!(breaker.is_penalized("gpt-4o", &key("openai", "gpt-4o")));
    }

    #[test]
    fn close_marks_target_available_but_stays_penalized() {
        let mut breaker = RouteCircuitBreaker::default();
        let target = key("openai", "gpt-4o");

        breaker.open(target, 5);
        breaker.close(&key("openai", "gpt-4o"));

        // Circuit is closed (not open)
        assert!(breaker.open_remaining(&key("openai", "gpt-4o")).is_none());
        // But still penalized — FillFirst should skip it
        assert!(breaker.is_penalized("gpt-4o", &key("openai", "gpt-4o")));
    }

    #[test]
    fn clear_penalized_removes_all_for_model() {
        let mut breaker = RouteCircuitBreaker::default();

        breaker.open(key("openai", "gpt-4o"), 5);
        breaker.open(key("anthropic", "claude"), 5);

        breaker.clear_penalized("gpt-4o");

        assert!(!breaker.is_penalized("gpt-4o", &key("openai", "gpt-4o")));
        assert!(!breaker.is_penalized("gpt-4o", &key("anthropic", "claude")));
    }

    #[test]
    fn penalized_is_per_model() {
        let mut breaker = RouteCircuitBreaker::default();

        breaker.open(key("openai", "gpt-4o"), 5);

        // Penalized under "gpt-4o" model
        assert!(breaker.is_penalized("gpt-4o", &key("openai", "gpt-4o")));
        // NOT penalized under a different model
        assert!(!breaker.is_penalized("claude-sonnet", &key("openai", "gpt-4o")));
    }

    #[test]
    fn open_entry_expires() {
        let mut breaker = RouteCircuitBreaker::default();
        let target = key("openai", "gpt-4o");

        breaker.open(target, 0); // 0-second duration → expires immediately

        assert!(breaker.open_remaining(&key("openai", "gpt-4o")).is_none());
        // Still penalized after expiry
        assert!(breaker.is_penalized("gpt-4o", &key("openai", "gpt-4o")));
    }

    #[test]
    fn keys_are_isolated() {
        let mut breaker = RouteCircuitBreaker::default();

        breaker.open(key("openai", "gpt-4o"), 5);

        assert!(breaker.open_remaining(&key("openai", "gpt-4o")).is_some());
        assert!(
            breaker
                .open_remaining(&key("anthropic", "gpt-4o"))
                .is_none()
        );
        assert!(
            breaker
                .open_remaining(&key("openai", "gpt-4o-mini"))
                .is_none()
        );
    }

    #[test]
    fn open_prunes_expired_entries() {
        let mut breaker = RouteCircuitBreaker::default();

        breaker.open(key("old", "model"), 0);
        // 0-second duration — by the time we call open() again it's expired
        breaker.open(key("new", "model"), 60);

        assert!(!breaker.open_until.contains_key(&key("old", "model")));
        assert!(breaker.open_until.contains_key(&key("new", "model")));
    }
}
