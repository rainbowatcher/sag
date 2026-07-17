//! In-memory circuit breaker for route targets.
//!
//! A route target is opened after its per-request retry budget is exhausted.
//! While open, later requests skip that target and immediately try the next
//! target in route order. The open duration depends on the error that caused
//! the exhaustion, as classified by [`sag_core::CircuitHint`]:
//!
//! | Trigger | Default Duration |
//! |---|---|
//! | Quota / billing exhausted | 1 hour |
//! | HTTP 429 rate-limit | 10 seconds |
//! | Generic retryable failure | 60 seconds |
//!
//! Durations are configurable via `[circuit_breaker]` in `gateway.toml`.
//!
//! ## Half-open recovery (with periodic probing)
//!
//! The breaker is a three-state machine: `Closed` (healthy) → `Open`
//! (cooldown) → `HalfOpen` (a probe is allowed) → `Closed` on a successful
//! probe, or back to `Open` (rescheduled) on a failed probe.
//!
//! A target recovers as soon as the upstream is actually healthy again —
//! e.g. the provider tops up quota or clears a rate limit mid-cooldown —
//! instead of staying skipped for the whole cooldown. To discover recovery
//! *early*, while the target is still cooling down it is probed periodically
//! every `probe_interval_secs` (configurable, default 60s, with jitter to
//! avoid thundering herds). Only the first request arriving after a probe
//! interval is allowed to hit the upstream (`probe_in_flight` guard); all
//! other concurrent requests skip the target and fall through to the next
//! one. A failed probe simply reschedules the next probe `probe_interval_secs`
//! later, so an exhausted upstream is retried on a steady, low-traffic cadence
//! rather than all at once when the full cooldown elapses.
//!
//! # Known property: no probe cap
//!
//! There is deliberately **no** "give up after N failed probes" backoff. A
//! target whose upstream is truly exhausted (e.g. a hard quota that will not
//! recover for hours) is probed once per `probe_interval_secs` indefinitely
//! until it succeeds. The per-interval probe is a single low-cost request with
//! jitter, so the steady-state overhead is negligible; operators who want a
//! longer gap simply raise `probe_interval_secs`. Removing a target entirely
//! (so its entry prunes) is the intended way to stop probing.
//!
//! ## Stale probe reclamation
//!
//! `HalfOpen + probe_in_flight` can become permanently stuck when the future
//! holding the probe slot is cancelled (e.g. client disconnect causes axum to
//! drop the handler before `apply_circuit_outcome` runs). `try_acquire_probe`
//! detects these stale slots and reclaims them after a timeout
//! ([`STALE_PROBE_THRESHOLD`]), ensuring no entry leaks forever.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// `HalfOpen + probe_in_flight` that persists longer than this threshold is
/// considered stale (probe future was cancelled without releasing the slot).
/// `try_acquire_probe` reclaims such slots before attempting a new acquire.
///
/// ## Trade-off
///
/// This threshold must exceed the worst-case probe duration in the deployment:
/// `max(timeout_secs) × (max_retries + 1)`.  Default configuration gives
/// `timeout_secs=600` and `max_retries=2` → worst case ≈ 1800 s.  We set
/// 3600 s to leave ample headroom.  The cost: a genuinely stuck probe slot
/// can take up to 1 h to be reclaimed.  However, because the target is in
/// cooldown, every new request hitting `try_acquire_probe` checks the
/// threshold – so actual reclamation happens on the first request after
/// the threshold elapses, not after a full hour.
///
/// If your deployment uses higher `timeout_secs` or `max_retries`, adjust
/// this constant accordingly (or lower those timeouts).
const STALE_PROBE_THRESHOLD: Duration = Duration::from_secs(3600);

/// Per-target circuit state.
///
/// A target with no entry in [`RouteCircuitBreaker::entries`] is implicitly
/// `Closed` (healthy). Only `Open` and `HalfOpen` need explicit storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CircuitState {
    /// Cooldown — `cooldown_until` lives on [`CircuitEntry`] so it survives
    /// `Open → HalfOpen → Open` transitions; `next_probe_at` is the earliest
    /// moment a recovery probe is permitted. Only `next_probe_at` here.
    Open { next_probe_at: Instant },
    /// A probe is currently permitted (and, once claimed, in flight).
    HalfOpen,
}

/// Stored circuit entry for a single route target.
#[derive(Debug, Clone, Copy)]
struct CircuitEntry {
    state: CircuitState,
    /// Hard cooldown lower bound (derived from the error type, e.g. 3600 s
    /// for quota exhaustion).  Survives `Open → HalfOpen → Open` transitions
    /// so a failed probe never shrinks the original cooling window.
    cooldown_until: Instant,
    /// Set when a `HalfOpen` target has already handed out its single probe
    /// to an in-flight request. While `true`, further requests must skip the
    /// target instead of probing again.
    probe_in_flight: bool,
    /// Timestamp when the probe slot was last acquired via
    /// [`try_acquire_probe`]. Used to detect stale slots (future was cancelled
    /// before `apply_circuit_outcome` released the slot). `None` if no probe
    /// is in flight or the entry was just created / transitioned.
    probe_acquired_at: Option<Instant>,
}

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
    entries: HashMap<RouteTargetKey, CircuitEntry>,
}

impl RouteCircuitBreaker {
    /// How long until the next probe is permitted for an `Open` target, or
    /// `None` if the target is not in a state that skips (Closed / HalfOpen).
    /// Used only for diagnostics/logging.
    pub fn next_probe_in(&self, key: &RouteTargetKey) -> Option<Duration> {
        let now = Instant::now();
        match self.entries.get(key) {
            Some(CircuitEntry {
                state: CircuitState::Open { next_probe_at },
                ..
            }) if *next_probe_at > now => Some(next_probe_at.duration_since(now)),
            _ => None,
        }
    }

    /// Whether the target is `Closed` (healthy, no stored entry). A closed
    /// target may be tried as a normal attempt — it is neither cooling down
    /// nor awaiting a half-open probe.
    pub fn is_closed(&self, key: &RouteTargetKey) -> bool {
        !self.entries.contains_key(key)
    }

    /// Whether a probe is currently permitted: `Closed`, `HalfOpen`, or `Open`
    /// whose `next_probe_at` has arrived. When this returns `true` the caller
    /// may (after claiming the single probe slot) hit the upstream.
    pub fn can_probe_now(&self, key: &RouteTargetKey) -> bool {
        let now = Instant::now();
        match self.entries.get(key) {
            None => true, // Closed
            Some(CircuitEntry {
                state: CircuitState::Open { next_probe_at },
                ..
            }) => *next_probe_at <= now,
            Some(CircuitEntry {
                state: CircuitState::HalfOpen,
                ..
            }) => true,
        }
    }

    /// Mark a target unavailable after its retry budget has been exhausted.
    /// `duration_secs` is the hard cooldown lower bound (from
    /// [`sag_core::CircuitHint`]); `probe_interval_secs` is how often a
    /// recovery probe is permitted while cooling down.
    pub fn open(&mut self, key: RouteTargetKey, duration_secs: u64, probe_interval_secs: u64) {
        let now = Instant::now();
        self.prune_expired(now);
        let cooldown_until = now + Duration::from_secs(duration_secs);
        // First probe is allowed one interval after opening, jittered so
        // many targets opened together don't probe in lockstep.
        let next_probe_at = now + jittered_interval(probe_interval_secs);
        self.entries.insert(
            key,
            CircuitEntry {
                state: CircuitState::Open { next_probe_at },
                cooldown_until,
                probe_in_flight: false,
                probe_acquired_at: None,
            },
        );
    }

    /// Mark a target available after a successful upstream response.
    pub fn close(&mut self, key: &RouteTargetKey) {
        self.entries.remove(key);
    }

    /// If the target is `Open` and a probe is now due (`next_probe_at` ≤ now),
    /// transition it to `HalfOpen` so the next request may claim the probe
    /// slot. No-op for `Closed` / `HalfOpen` / not-yet-due `Open`. Returns
    /// `true` if the target is now in a state where it may be tried.
    pub fn to_half_open_if_due(&mut self, key: &RouteTargetKey) -> bool {
        let now = Instant::now();
        let due = matches!(
            self.entries.get(key),
            Some(CircuitEntry {
                state: CircuitState::Open { next_probe_at },
                ..
            }) if *next_probe_at <= now
        );
        if due {
            if let Some(entry) = self.entries.get_mut(key) {
                entry.state = CircuitState::HalfOpen;
                // cooldown_until is preserved across the transition.
                entry.probe_in_flight = false;
                entry.probe_acquired_at = None;
            }
        }
        matches!(
            self.entries.get(key),
            None | Some(CircuitEntry {
                state: CircuitState::HalfOpen,
                ..
            })
        )
    }

    /// Attempt to claim the single half-open probe slot for `key`.
    ///
    /// Returns `true` (and marks the slot busy) only when the target is
    /// `HalfOpen` and no probe is currently in flight. This guarantees that
    /// exactly one in-flight request probes the upstream while a probe is due;
    /// every other concurrent request must skip it.
    ///
    /// # Stale probe reclamation
    ///
    /// Before checking the slot, this method reclaims any `HalfOpen +
    /// probe_in_flight` slot whose [`probe_acquired_at`] is older than
    /// [`STALE_PROBE_THRESHOLD`]. This handles the case where a probe future
    /// was cancelled (e.g. client disconnect) before
    /// [`apply_circuit_outcome`] released the slot, preventing permanent
    /// entry leaks.
    pub fn try_acquire_probe(&mut self, key: &RouteTargetKey) -> bool {
        let now = Instant::now();
        if let Some(entry) = self.entries.get_mut(key) {
            // Reclaim stale probe slots (future was cancelled before releasing).
            if entry.state == CircuitState::HalfOpen
                && entry.probe_in_flight
                && entry
                    .probe_acquired_at
                    .map(|t| now.duration_since(t) > STALE_PROBE_THRESHOLD)
                    .unwrap_or(false)
            {
                entry.probe_in_flight = false;
            }
            if entry.state == CircuitState::HalfOpen && !entry.probe_in_flight {
                entry.probe_in_flight = true;
                entry.probe_acquired_at = Some(now);
                return true;
            }
        }
        false
    }

    /// Record the outcome of a probe (or any attempt) against `key`.
    ///
    /// - `ok = true` → close the circuit (target healthy again).
    /// - `ok = false` → reschedule the next probe `probe_interval_secs` later
    ///   (keeping the target `Open`). This is the self-healing polling cadence:
    ///   an exhausted upstream is retried on a steady, low-traffic rhythm
    ///   rather than being hammered when the full cooldown elapses.
    ///
    /// # Invariant
    ///
    /// On failure the original `cooldown_until` is preserved (bumped forward
    /// to at least `now + probe_interval_secs` but never shrunk). This
    /// ensures that a long hard cooldown (e.g. 1h quota exhaustion) is not
    /// accidentally collapsed to the probe interval by a failed probe.
    pub fn record_probe_result(&mut self, key: RouteTargetKey, ok: bool, probe_interval_secs: u64) {
        if ok {
            self.close(&key);
        } else {
            let now = Instant::now();
            let next_probe_at = now + jittered_interval(probe_interval_secs);
            if let Some(entry) = self.entries.get_mut(&key) {
                // cooldown_until lives on CircuitEntry, not CircuitState, so
                // it is always available regardless of current state (Open or
                // HalfOpen).  Never shrink the original hard cooldown.
                entry.cooldown_until = entry
                    .cooldown_until
                    .max(now + Duration::from_secs(probe_interval_secs));
                entry.state = CircuitState::Open { next_probe_at };
                entry.probe_in_flight = false;
                entry.probe_acquired_at = None;
            }
        }
    }

    /// Remove entries whose `cooldown_until` has passed, regardless of state
    /// (`Open` or `HalfOpen`).  A `HalfOpen` entry whose cooldown has expired
    /// is meaningless — the target is no longer being cooled down.
    fn prune_expired(&mut self, now: Instant) {
        self.entries.retain(|_, entry| entry.cooldown_until > now);
    }
}

/// Compute a probe interval with jitter in `[interval, interval + interval/2]`.
///
/// Jitter spreads probes from many targets that opened together, avoiding a
/// synchronized thundering herd. Uses `fastrand` for cheap per-call randomness.
fn jittered_interval(interval_secs: u64) -> Duration {
    if interval_secs == 0 {
        return Duration::ZERO;
    }
    let base = Duration::from_secs(interval_secs);
    let jitter = Duration::from_secs(interval_secs / 2);
    base + Duration::from_secs(fastrand::u64(0..=jitter.as_secs()))
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
        assert!(breaker.is_closed(&key("openai", "gpt-4o")));
        assert!(breaker.can_probe_now(&key("openai", "gpt-4o")));
    }

    #[test]
    fn open_marks_target_unavailable_until_first_probe_due() {
        let mut breaker = RouteCircuitBreaker::default();
        breaker.open(key("openai", "gpt-4o"), 3600, 60);
        // Immediately after opening, the first probe is not yet due.
        assert!(!breaker.can_probe_now(&key("openai", "gpt-4o")));
        assert!(breaker.next_probe_in(&key("openai", "gpt-4o")).is_some());
    }

    #[test]
    fn close_marks_target_available() {
        let mut breaker = RouteCircuitBreaker::default();
        breaker.open(key("openai", "gpt-4o"), 5, 5);
        breaker.close(&key("openai", "gpt-4o"));
        assert!(breaker.is_closed(&key("openai", "gpt-4o")));
    }

    #[test]
    fn open_prunes_expired_entries() {
        let mut breaker = RouteCircuitBreaker::default();
        // duration 0 → cooldown expires immediately; interval 0 → first probe due now.
        breaker.open(key("old", "model"), 0, 0);
        breaker.open(key("new", "model"), 60, 60);
        assert!(!breaker.entries.contains_key(&key("old", "model")));
        assert!(breaker.entries.contains_key(&key("new", "model")));
    }

    #[test]
    fn probe_due_transitions_to_half_open() {
        let mut breaker = RouteCircuitBreaker::default();
        breaker.open(key("openai", "gpt-4o"), 3600, 0); // first probe due immediately
        assert!(
            breaker.to_half_open_if_due(&key("openai", "gpt-4o")),
            "open with due probe should become half-open (tryable)"
        );
        assert!(
            breaker.can_probe_now(&key("openai", "gpt-4o")),
            "half-open is probeable"
        );
    }

    #[test]
    fn half_open_allows_only_one_probe() {
        let mut breaker = RouteCircuitBreaker::default();
        breaker.open(key("openai", "gpt-4o"), 3600, 0);
        assert!(breaker.to_half_open_if_due(&key("openai", "gpt-4o")));
        assert!(
            breaker.try_acquire_probe(&key("openai", "gpt-4o")),
            "first probe should be granted"
        );
        assert!(
            !breaker.try_acquire_probe(&key("openai", "gpt-4o")),
            "second probe must be denied while one is in flight"
        );
    }

    #[test]
    fn probe_success_closes() {
        let mut breaker = RouteCircuitBreaker::default();
        breaker.open(key("openai", "gpt-4o"), 60, 60);
        breaker.to_half_open_if_due(&key("openai", "gpt-4o"));
        breaker.record_probe_result(key("openai", "gpt-4o"), true, 60);
        assert!(breaker.is_closed(&key("openai", "gpt-4o")));
        assert!(breaker.entries.get(&key("openai", "gpt-4o")).is_none());
    }

    #[test]
    fn probe_failure_reschedules_interval_not_full_cooldown() {
        let mut breaker = RouteCircuitBreaker::default();
        breaker.open(key("openai", "gpt-4o"), 3600, 60); // hard cooldown 1h
        breaker.to_half_open_if_due(&key("openai", "gpt-4o"));
        breaker.try_acquire_probe(&key("openai", "gpt-4o"));
        breaker.record_probe_result(key("openai", "gpt-4o"), false, 60);
        // Still not probeable (rescheduled ~60s out), NOT a fresh 1h cooldown.
        assert!(!breaker.can_probe_now(&key("openai", "gpt-4o")));
        let remaining = breaker.next_probe_in(&key("openai", "gpt-4o")).unwrap();
        assert!(
            remaining.as_secs() <= 90,
            "failed probe should reschedule ~interval (60s), got {remaining:?}"
        );
    }

    #[test]
    fn probe_failure_preserves_cooldown_until() {
        // Bug A regression: `record_probe_result(ok=false)` when entry is
        // `HalfOpen` must preserve the original hard cooldown_until (≈3600s)
        // instead of collapsing it to `probe_interval_secs` (60s).
        let mut breaker = RouteCircuitBreaker::default();
        // interval=0 so the first probe is due immediately → entry reaches
        // HalfOpen on the first to_half_open_if_due call.
        breaker.open(key("openai", "gpt-4o"), 3600, 0);
        assert!(
            breaker.to_half_open_if_due(&key("openai", "gpt-4o")),
            "probe should be due"
        );
        assert!(
            breaker.try_acquire_probe(&key("openai", "gpt-4o")),
            "probe slot acquired"
        );
        // Entry is now HalfOpen — this is the real probe-failure path.
        breaker.record_probe_result(key("openai", "gpt-4o"), false, 60);

        let now = Instant::now();
        let entry = breaker
            .entries
            .get(&key("openai", "gpt-4o"))
            .expect("entry must still exist after probe failure");
        let remaining = entry.cooldown_until.duration_since(now).as_secs();
        assert!(
            remaining > 3000,
            "cooldown_until should still be ~3600s from open, got {remaining}s remaining"
        );
        // Also verify next_probe_at was pushed ~60s (not 3600s).
        match entry.state {
            CircuitState::Open { next_probe_at } => {
                let probe_in = next_probe_at.duration_since(now).as_secs();
                assert!(
                    probe_in <= 90,
                    "failed probe should reschedule ~interval (60s), got {probe_in}s"
                );
            }
            _ => panic!("entry must be Open after a failed probe"),
        }
    }

    #[test]
    fn stale_probe_slot_is_reclaimed() {
        // Bug B regression: a probe future that was cancelled (client
        // disconnect) leaves probe_in_flight=true permanently. After
        // STALE_PROBE_THRESHOLD elapses, try_acquire_probe must reclaim
        // the slot.
        let mut breaker = RouteCircuitBreaker::default();
        breaker.open(key("openai", "gpt-4o"), 3600, 0); // interval 0 → first probe due immediately
        breaker.to_half_open_if_due(&key("openai", "gpt-4o"));
        breaker.try_acquire_probe(&key("openai", "gpt-4o"));
        // Verify the slot is taken.
        assert!(!breaker.try_acquire_probe(&key("openai", "gpt-4o")));
        // Manually wind back probe_acquired_at to simulate a stale slot.
        if let Some(entry) = breaker.entries.get_mut(&key("openai", "gpt-4o")) {
            entry.probe_acquired_at = Some(Instant::now() - Duration::from_secs(4000));
        }
        // Reclaim and re-acquire in one call.
        assert!(
            breaker.try_acquire_probe(&key("openai", "gpt-4o")),
            "stale probe slot must be reclaimed and re-acquired"
        );
    }

    #[test]
    fn stale_probe_within_threshold_not_reclaimed() {
        // Verify that an in-flight probe slot within STALE_PROBE_THRESHOLD
        // is NOT reclaimed, preserving the "one probe at a time" invariant.
        let mut breaker = RouteCircuitBreaker::default();
        breaker.open(key("openai", "gpt-4o"), 7200, 0);
        assert!(breaker.to_half_open_if_due(&key("openai", "gpt-4o")));
        assert!(
            breaker.try_acquire_probe(&key("openai", "gpt-4o")),
            "first probe acquired"
        );
        // Wind back probe_acquired_at by 100s — well within 3600s threshold.
        if let Some(entry) = breaker.entries.get_mut(&key("openai", "gpt-4o")) {
            entry.probe_acquired_at = Some(Instant::now() - Duration::from_secs(100));
        }
        // Should NOT be reclaimed; second acquire must fail.
        assert!(
            !breaker.try_acquire_probe(&key("openai", "gpt-4o")),
            "in-flight probe within threshold must not be reclaimed"
        );
    }
}
