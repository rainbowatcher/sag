//! In-memory circuit breaker for route targets.
//!
//! A route target is opened after its per-request retry budget is exhausted.
//! While open, later requests skip that target and immediately try the next
//! target in route order. The open duration depends on the error that caused
//! the exhaustion, as classified by [`crate::error::CircuitHint`]:
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
//! detects these stale slots and reclaims them after the configured
//! `stale_probe_threshold` (see [`RouteCircuitBreaker::new`] and
//! [`compute_stale_probe_threshold`]), ensuring no entry leaks forever.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;

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
    /// [`RouteCircuitBreaker::try_acquire_probe`]. Used to detect stale slots (future was cancelled
    /// before `apply_circuit_outcome` released the slot). `None` if no probe
    /// is in flight or the entry was just created / transitioned.
    probe_acquired_at: Option<Instant>,
}

/// Stable identity for a resolved route target.
///
/// `model` is the client-requested model for this dispatch. `real_model` is the
/// concrete upstream model after route resolution. Keeping both avoids sharing
/// circuit state between two logical routes that happen to use the same provider.
///
/// `url` is the upstream API base address the target dispatches to (the
/// `apis.<key>.base_url`, without the protocol path). Folding the address
/// into the key isolates health between targets that point at different
/// addresses, while targets on the **same** address (同址多协议) naturally
/// share one breaker.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RouteTargetKey {
    pub model: String,
    pub provider: String,
    pub real_model: String,
    pub url: String,
}

impl RouteTargetKey {
    pub fn new(model: &str, provider: &str, real_model: &str, url: &str) -> Self {
        Self {
            model: model.to_string(),
            provider: provider.to_string(),
            real_model: real_model.to_string(),
            url: url.to_string(),
        }
    }
}

/// Public, serializable state kind for circuit breaker snapshots.
///
/// This is a projection of the private `CircuitState` enum that does not
/// expose internal `Instant` fields. Serialized as `"open"` / `"half_open"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CircuitStateKind {
    Open,
    HalfOpen,
}

/// Serializable snapshot of a single non-`Closed` circuit breaker entry.
///
/// Only `Open` and `HalfOpen` targets appear in snapshots — `Closed` targets
/// have no entry in the breaker's internal map and are implicitly healthy.
#[derive(Debug, Clone, Serialize)]
pub struct CircuitBreakerEntry {
    pub model: String,
    pub provider: String,
    pub real_model: String,
    /// Upstream API base address this target dispatches to (part of the
    /// breaker key — same address shares health, different addresses are
    /// isolated).
    pub url: String,
    pub state: CircuitStateKind,
    /// UNIX timestamp (seconds) of the hard cooldown lower bound.
    /// For entries whose cooldown has already elapsed (but not yet pruned),
    /// this is clamped to "now" — the target is effectively recoverable.
    pub cooldown_until_secs: u64,
    /// `None` when the target is `HalfOpen` (probe is ready, no future
    /// schedule). `Some(ts)` when `Open` — the next moment a probe is allowed.
    pub next_probe_at_secs: Option<u64>,
    /// Whether a probe request is currently in flight (`HalfOpen` only).
    /// `Open` entries always have this as `false`.
    pub probe_in_flight: bool,
}

/// Minimal circuit state for all route targets.
#[derive(Debug)]
pub struct RouteCircuitBreaker {
    entries: HashMap<RouteTargetKey, CircuitEntry>,
    /// 探针槽位失效阈值。超过此时间仍 `probe_in_flight` 的 HalfOpen 槽位
    /// 视为已取消（探测 future 被 drop 而 `apply_circuit_outcome` 未跑），
    /// 由 `try_acquire_probe` 回收。
    ///
    /// 生产路径应通过 [`compute_stale_probe_threshold`] + 配置计算后
    /// 用 [`RouteCircuitBreaker::new`] 构造；[`Default`] 仅供测试使用
    /// （固定 3600s，不跟随配置变化）。
    stale_probe_threshold: Duration,
}

impl Default for RouteCircuitBreaker {
    /// 仅供测试 fixture 使用。**生产路径必须通过
    /// [`compute_stale_probe_threshold`] 计算阈值并使用
    /// [`RouteCircuitBreaker::new`] 构造**——本实现使用与旧硬编码相等的
    /// 3600s，不跟随 provider / route / target 配置变化。
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            stale_probe_threshold: Duration::from_secs(3600),
        }
    }
}

impl RouteCircuitBreaker {
    /// Construct a breaker with a config-derived stale-probe threshold.
    ///
    /// Use [`compute_stale_probe_threshold`] (and the config-walking helper
    /// `stale_probe_threshold_from_config` in `sag-dispatch`) to compute
    /// `stale_probe_threshold` from the current gateway configuration.
    pub fn new(stale_probe_threshold: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            stale_probe_threshold,
        }
    }

    /// Update the stale-probe threshold. Called when the config changes
    /// (hot-reload or admin mutation) to keep the threshold in sync with
    /// the current provider/route timeout settings.
    ///
    /// In-flight probe state (entries, `probe_in_flight`) is fully preserved —
    /// only the threshold parameter used by [`Self::try_acquire_probe`] is updated.
    pub fn set_stale_probe_threshold(&mut self, threshold: Duration) {
        self.stale_probe_threshold = threshold;
    }

    /// Test-only accessor for the configured stale-probe threshold.
    #[cfg(test)]
    pub(crate) fn stale_probe_threshold(&self) -> Duration {
        self.stale_probe_threshold
    }

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
    /// [`crate::error::CircuitHint`]); `probe_interval_secs` is how often a
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
        if due && let Some(entry) = self.entries.get_mut(key) {
            entry.state = CircuitState::HalfOpen;
            // cooldown_until is preserved across the transition.
            entry.probe_in_flight = false;
            entry.probe_acquired_at = None;
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
    /// probe_in_flight` slot whose `probe_acquired_at` is older than the
    /// breaker's `stale_probe_threshold`. This handles the case where a probe
    /// future was cancelled (e.g. client disconnect) before
    /// `apply_circuit_outcome` released the slot, preventing permanent
    /// entry leaks.
    pub fn try_acquire_probe(&mut self, key: &RouteTargetKey) -> bool {
        let now = Instant::now();
        if let Some(entry) = self.entries.get_mut(key) {
            // Reclaim stale probe slots (future was cancelled before releasing).
            if entry.state == CircuitState::HalfOpen
                && entry.probe_in_flight
                && entry
                    .probe_acquired_at
                    .map(|t| now.duration_since(t) > self.stale_probe_threshold)
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

    /// Return a serializable snapshot of all non-`Closed` targets.
    ///
    /// **Instant → SystemTime conversion safety**: `prune_expired` is only
    /// called on write paths (`open` / `record_probe_result`), so this
    /// read-only method may encounter entries whose `cooldown_until` or
    /// `next_probe_at` has already elapsed. We use `checked_duration_since`
    /// (not `duration_since`, which panics on `target < now`) and clamp
    /// expired moments to "now" so callers see "recoverable" rather than a
    /// panic.
    pub fn snapshot(&self) -> Vec<CircuitBreakerEntry> {
        let now_instant = Instant::now();
        let now_system = SystemTime::now();
        let now_epoch = now_system
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|t| t.as_secs())
            .unwrap_or(0);

        // Convert a future/past Instant to a UNIX-seconds timestamp.
        // Expired instants clamp to `now_epoch` (target is recoverable).
        let to_epoch = |target: Instant| -> u64 {
            match target.checked_duration_since(now_instant) {
                Some(d) => (now_system + d)
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|t| t.as_secs())
                    .unwrap_or(now_epoch),
                None => now_epoch,
            }
        };

        let mut entries: Vec<CircuitBreakerEntry> = self
            .entries
            .iter()
            .map(|(k, e)| CircuitBreakerEntry {
                model: k.model.clone(),
                provider: k.provider.clone(),
                real_model: k.real_model.clone(),
                url: k.url.clone(),
                state: match e.state {
                    CircuitState::Open { .. } => CircuitStateKind::Open,
                    CircuitState::HalfOpen => CircuitStateKind::HalfOpen,
                },
                cooldown_until_secs: to_epoch(e.cooldown_until),
                next_probe_at_secs: match e.state {
                    CircuitState::Open { next_probe_at } => Some(to_epoch(next_probe_at)),
                    CircuitState::HalfOpen => None,
                },
                probe_in_flight: e.probe_in_flight,
            })
            .collect();
        // Sort for deterministic API output (HashMap order is non-deterministic).
        entries.sort_by(|a, b| {
            a.model
                .cmp(&b.model)
                .then(a.provider.cmp(&b.provider))
                .then(a.real_model.cmp(&b.real_model))
                .then(a.url.cmp(&b.url))
        });
        entries
    }

    /// Remove entries whose `cooldown_until` has passed, regardless of state
    /// (`Open` or `HalfOpen`).  A `HalfOpen` entry whose cooldown has expired
    /// is meaningless — the target is no longer being cooled down.
    fn prune_expired(&mut self, now: Instant) {
        self.entries.retain(|_, entry| entry.cooldown_until > now);
    }
}

/// 根据配置计算探针槽位失效阈值。超过此阈值仍 `HalfOpen + probe_in_flight`
/// 的槽位视为已被取消（探测 future 被 drop 而 `apply_circuit_outcome` 未跑），
/// 由 `try_acquire_probe` 回收。
///
/// 公式：
/// ```text
/// per_attempt   = max_per_attempt_secs
/// probe_worst   = per_attempt × (effective_retries + 1)
/// backoff_total = (max_backoff_ms × effective_retries) / 1000   // 保守上界
/// threshold     = max(probe_worst + backoff_total, FLOOR)
/// ```
///
/// - `max_per_attempt_secs`：单次尝试的 wall-clock 上界。非流式由
///   `effective_timeout` 决定；流式请求不受 `effective_timeout` 限制，取
///   `ttfb_timeout_secs` 起步值参与比较，保证 ttfb 跑满时一次尝试仍落在
///   阈值内。调用方应传入 `max(effective_timeout, max_ttfb_timeout)`。
/// - `effective_retries`：每 (route, target) 实际生效的重试次数
///   （target → route → global 三层回退后的最终值）。
/// - `max_backoff_ms`：`backoff.max_ms`，全局退避上限。
/// - `FLOOR = 600s`：与 `sag_config::default_timeout_secs()` 一致，覆盖配置
///   极端 reduced 的情况，并对超长 streaming 探针提供最低安全垫。
///
/// # Streaming 与「误回收」
///
/// 流式请求在 ttfb 之后由 `chunk_timeout_secs` 治理过程中的 idle 间隔。只要
/// token 持续吐出，wall-clock 可远超 `per_attempt × attempts`。这意味着一个
/// 仍在合法吐 chunk 的活跃 probe 可能越过阈值被 `try_acquire_probe` 回收，
/// 触发该目标的二次探针，**但 doubled probe 至多多打一次**，不破坏「同时
/// 最多一个探针」不变式（回收逻辑串行于 `to_half_open_if_due` 之后），也不
/// 会让坏上游 escape。
///
/// 因此本公式对流式采取「合理上界 + 显式承认极端误回收」，而非追求严格边界。
pub fn compute_stale_probe_threshold(
    max_per_attempt_secs: u64,
    effective_retries: u32,
    max_backoff_ms: u64,
) -> Duration {
    const FLOOR: u64 = 600; // 与 sag_config::default_timeout_secs() 一致

    let attempts = effective_retries as u64 + 1;
    let probe_worst = max_per_attempt_secs.saturating_mul(attempts);
    let backoff_total = max_backoff_ms.saturating_mul(effective_retries as u64) / 1000;
    let raw = probe_worst.saturating_add(backoff_total);
    Duration::from_secs(raw.max(FLOOR))
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

    const TEST_URL: &str = "https://api.example.com/v1";

    fn key(provider: &str, real_model: &str) -> RouteTargetKey {
        RouteTargetKey::new("gpt-4o", provider, real_model, TEST_URL)
    }

    /// 不同地址 → 不同 key（健康隔离）；同址多协议 → 同一 key（共享）。
    #[test]
    fn key_url_dimension_isolates_by_address() {
        let a = RouteTargetKey::new("m", "p", "rm", "https://a.example.com/v1");
        let b = RouteTargetKey::new("m", "p", "rm", "https://b.example.com/v1");
        assert_ne!(a, b, "different addresses must be distinct keys");

        // 同址多协议：地址相同、协议不同（key 无协议维度）→ 共享 breaker。
        let same_addr_1 = RouteTargetKey::new("m", "p", "rm", "https://a.example.com/v1");
        let same_addr_2 = RouteTargetKey::new("m", "p", "rm", "https://a.example.com/v1");
        assert_eq!(same_addr_1, same_addr_2);

        let mut breaker = RouteCircuitBreaker::default();
        breaker.open(a.clone(), 3600, 60);
        assert!(
            !breaker.is_closed(&same_addr_1),
            "same address shares the breaker"
        );
        assert!(breaker.is_closed(&b), "different address stays isolated");
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
        assert!(!breaker.entries.contains_key(&key("openai", "gpt-4o")));
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
        // stale_probe_threshold (3600s via Default) elapses, try_acquire_probe
        // must reclaim
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
        // Verify that an in-flight probe slot within stale_probe_threshold
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

    // ── snapshot() tests ──────────────────────────────────────────────

    #[test]
    fn snapshot_empty_returns_empty_vec() {
        let breaker = RouteCircuitBreaker::default();
        assert!(breaker.snapshot().is_empty());
    }

    #[test]
    fn snapshot_open_entry_has_next_probe_at_some() {
        let mut breaker = RouteCircuitBreaker::default();
        breaker.open(key("openai", "gpt-4o"), 3600, 60);
        let snap = breaker.snapshot();
        assert_eq!(snap.len(), 1);
        let entry = &snap[0];
        assert_eq!(entry.model, "gpt-4o");
        assert_eq!(entry.provider, "openai");
        assert_eq!(entry.real_model, "gpt-4o");
        assert_eq!(entry.url, TEST_URL);
        assert_eq!(entry.state, CircuitStateKind::Open);
        assert!(
            entry.next_probe_at_secs.is_some(),
            "Open entry should have next_probe_at_secs"
        );
        assert!(!entry.probe_in_flight);
    }

    #[test]
    fn snapshot_half_open_entry_has_next_probe_at_none() {
        let mut breaker = RouteCircuitBreaker::default();
        breaker.open(key("openai", "gpt-4o"), 3600, 0);
        breaker.to_half_open_if_due(&key("openai", "gpt-4o"));
        let snap = breaker.snapshot();
        assert_eq!(snap.len(), 1);
        let entry = &snap[0];
        assert_eq!(entry.state, CircuitStateKind::HalfOpen);
        assert!(
            entry.next_probe_at_secs.is_none(),
            "HalfOpen entry should have next_probe_at_secs = None"
        );
    }

    #[test]
    fn snapshot_expired_entry_does_not_panic() {
        // An entry whose cooldown_until is in the past (but not yet pruned,
        // since snapshot is read-only) must not panic.
        let mut breaker = RouteCircuitBreaker::default();
        breaker.open(key("openai", "gpt-4o"), 3600, 60);
        // Manually wind cooldown_until into the past.
        if let Some(entry) = breaker.entries.get_mut(&key("openai", "gpt-4o")) {
            entry.cooldown_until = Instant::now() - Duration::from_secs(100);
            // Also wind next_probe_at into the past to test that path.
            entry.state = CircuitState::Open {
                next_probe_at: Instant::now() - Duration::from_secs(50),
            };
        }
        // This must not panic.
        let snap = breaker.snapshot();
        assert_eq!(snap.len(), 1);
        // Both timestamps should be clamped to ~now (non-zero, reasonable).
        let now_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|t| t.as_secs())
            .unwrap_or(0);
        let diff = snap[0].cooldown_until_secs.abs_diff(now_secs);
        assert!(
            diff < 5,
            "expired cooldown should clamp to now, diff={diff}s"
        );
    }

    #[test]
    fn snapshot_serializes_to_snake_case_state() {
        let mut breaker = RouteCircuitBreaker::default();
        // interval=60 → first probe not due yet → entry stays Open
        breaker.open(key("openai", "gpt-4o"), 3600, 60);
        let snap = breaker.snapshot();
        let json = serde_json::to_string(&snap).unwrap();
        assert!(
            json.contains("\"state\":\"open\""),
            "serialized state should be snake_case 'open', got: {json}"
        );

        // Test HalfOpen serialization: interval=0 → first probe due immediately
        breaker.open(key("anthropic", "claude"), 3600, 0);
        breaker.to_half_open_if_due(&key("anthropic", "claude"));
        let snap2 = breaker.snapshot();
        let json2 = serde_json::to_string(&snap2).unwrap();
        assert!(
            json2.contains("\"state\":\"half_open\""),
            "serialized state should be snake_case 'half_open', got: {json2}"
        );
    }

    // ── compute_stale_probe_threshold() tests ─────────────────────

    #[test]
    fn compute_threshold_floor_kicks_in_for_small_values() {
        // timeout=0, retries=0, max_ms=0 → raw=0, floor=600
        let threshold = compute_stale_probe_threshold(0, 0, 0);
        assert_eq!(threshold, Duration::from_secs(600));
    }

    #[test]
    fn compute_threshold_floor_kicks_in_for_normal_but_small() {
        // timeout=120, retries=2, max_ms=5000 → 120×3 + 10 = 370 < 600 → floor
        let threshold = compute_stale_probe_threshold(120, 2, 5000);
        assert_eq!(threshold, Duration::from_secs(600));
    }

    #[test]
    fn compute_threshold_above_floor() {
        // timeout=600, retries=2, max_ms=5000 → 600×3 + 10 = 1810 > 600
        let threshold = compute_stale_probe_threshold(600, 2, 5000);
        assert_eq!(threshold, Duration::from_secs(1810));
    }

    #[test]
    fn compute_threshold_saturating_no_overflow() {
        // u64 boundaries — saturating_mul/add must not panic
        let threshold = compute_stale_probe_threshold(u64::MAX, 10, u64::MAX);
        // Should saturate at u64::MAX seconds (practically infinite, but no panic)
        assert_eq!(threshold, Duration::from_secs(u64::MAX));
    }

    #[test]
    fn compute_threshold_retries_zero_single_attempt() {
        // retries=0 → attempts=1, no backoff
        let threshold = compute_stale_probe_threshold(600, 0, 5000);
        assert_eq!(threshold, Duration::from_secs(600));
    }

    // ── set_stale_probe_threshold() tests ──────────────────────────

    #[test]
    fn set_stale_probe_threshold_preserves_in_flight_probe() {
        // Updating the threshold must not disturb in-flight probe state:
        // entries, `probe_in_flight`, and `probe_acquired_at` survive.
        let mut breaker = RouteCircuitBreaker::new(Duration::from_secs(3600));
        breaker.open(key("openai", "gpt-4o"), 3600, 0);
        breaker.to_half_open_if_due(&key("openai", "gpt-4o"));
        assert!(breaker.try_acquire_probe(&key("openai", "gpt-4o")));
        let acquired_at = breaker
            .entries
            .get(&key("openai", "gpt-4o"))
            .unwrap()
            .probe_acquired_at;

        // Lower the threshold (e.g. config hot-reload reduced timeouts).
        breaker.set_stale_probe_threshold(Duration::from_secs(600));
        assert_eq!(breaker.stale_probe_threshold(), Duration::from_secs(600));

        let entry = breaker.entries.get(&key("openai", "gpt-4o")).unwrap();
        assert!(
            entry.probe_in_flight,
            "probe_in_flight must survive threshold update"
        );
        assert_eq!(
            entry.probe_acquired_at, acquired_at,
            "probe_acquired_at must survive threshold update"
        );
        // Second acquire must still be denied (slot still held).
        assert!(!breaker.try_acquire_probe(&key("openai", "gpt-4o")));
    }

    #[test]
    fn set_stale_probe_threshold_affects_reclamation_window() {
        // After lowering the threshold, a slot older than the new threshold
        // (but younger than the old) must now be reclaimed.
        let mut breaker = RouteCircuitBreaker::new(Duration::from_secs(3600));
        breaker.open(key("openai", "gpt-4o"), 3600, 0);
        breaker.to_half_open_if_due(&key("openai", "gpt-4o"));
        breaker.try_acquire_probe(&key("openai", "gpt-4o"));
        // 1000s in flight — within old 3600s, beyond new 600s.
        if let Some(entry) = breaker.entries.get_mut(&key("openai", "gpt-4o")) {
            entry.probe_acquired_at = Some(Instant::now() - Duration::from_secs(1000));
        }
        // Still held under old threshold.
        assert!(!breaker.try_acquire_probe(&key("openai", "gpt-4o")));
        // Lower threshold → 1000s now exceeds 600s → reclaim + re-acquire.
        breaker.set_stale_probe_threshold(Duration::from_secs(600));
        assert!(
            breaker.try_acquire_probe(&key("openai", "gpt-4o")),
            "lowered threshold must reclaim the now-stale slot"
        );
    }
}
