//! In-memory circuit breaker for route targets.
//!
//! A route target is opened after its per-request retry budget is exhausted.
//! While open, later requests skip that target and immediately try the next
//! target in route order. After a short cooldown the target is allowed to be
//! probed again; a successful probe closes the circuit, while another exhausted
//! retry budget opens it again.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Default amount of time a route target stays open after retry exhaustion.
pub const DEFAULT_OPEN_DURATION: Duration = Duration::from_secs(60);

/// Stable identity for a resolved route target.
///
/// `model` is the client-requested model for this dispatch. `real_model` is the
/// concrete upstream model after route resolution. Keeping both avoids sharing
/// circuit state between two logical routes that happen to use the same provider.
#[derive(Debug, PartialEq, Eq, Hash)]
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
#[derive(Debug)]
pub struct RouteCircuitBreaker {
    open_duration: Duration,
    open_until: HashMap<RouteTargetKey, Instant>,
}

impl Default for RouteCircuitBreaker {
    fn default() -> Self {
        Self {
            open_duration: DEFAULT_OPEN_DURATION,
            open_until: HashMap::new(),
        }
    }
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
    pub fn open(&mut self, key: RouteTargetKey) {
        let now = Instant::now();
        self.prune_expired(now);
        self.open_until.insert(key, now + self.open_duration);
    }

    /// Mark a target available after a successful upstream response.
    pub fn close(&mut self, key: &RouteTargetKey) {
        self.open_until.remove(key);
    }

    fn prune_expired(&mut self, now: Instant) {
        self.open_until.retain(|_, until| *until > now);
    }

    #[cfg(test)]
    pub fn with_open_duration(open_duration: Duration) -> Self {
        Self {
            open_duration,
            open_until: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

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
        let mut breaker = RouteCircuitBreaker::with_open_duration(Duration::from_secs(5));
        let target = key("openai", "gpt-4o");

        breaker.open(target);

        assert!(breaker.open_remaining(&key("openai", "gpt-4o")).is_some());
    }

    #[test]
    fn close_marks_target_available() {
        let mut breaker = RouteCircuitBreaker::with_open_duration(Duration::from_secs(5));
        let target = key("openai", "gpt-4o");

        breaker.open(target);
        breaker.close(&key("openai", "gpt-4o"));

        assert!(breaker.open_remaining(&key("openai", "gpt-4o")).is_none());
    }

    #[test]
    fn open_entry_expires() {
        let mut breaker = RouteCircuitBreaker::with_open_duration(Duration::from_millis(5));
        let target = key("openai", "gpt-4o");

        breaker.open(target);
        thread::sleep(Duration::from_millis(10));

        assert!(breaker.open_remaining(&key("openai", "gpt-4o")).is_none());
    }

    #[test]
    fn keys_are_isolated() {
        let mut breaker = RouteCircuitBreaker::with_open_duration(Duration::from_secs(5));

        breaker.open(key("openai", "gpt-4o"));

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
        let mut breaker = RouteCircuitBreaker::with_open_duration(Duration::from_millis(5));

        breaker.open(key("old", "model"));
        thread::sleep(Duration::from_millis(10));
        breaker.open(key("new", "model"));

        assert!(!breaker.open_until.contains_key(&key("old", "model")));
        assert!(breaker.open_until.contains_key(&key("new", "model")));
    }
}
