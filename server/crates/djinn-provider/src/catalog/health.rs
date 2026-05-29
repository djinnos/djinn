use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Number of consecutive failures before circuit breaker trips.
const CIRCUIT_BREAKER_THRESHOLD: u32 = 3;
/// Initial cooldown after first circuit-breaker trip: 5 seconds.
const INITIAL_COOLDOWN: Duration = Duration::from_secs(5);
/// Maximum cooldown: 5 minutes.
const MAX_COOLDOWN: Duration = Duration::from_secs(5 * 60);

/// Minimum cooldown applied when a model is tripped via [`HealthTracker::record_stall`]
/// (a zero-token / first-LLM-call-hung stall). This is the key knob that makes
/// the model breaker actually drive *failover*: a stalled task gets re-dispatched
/// after an escalating *task* cooldown that starts at 60s and grows to 120s/240s.
/// If a stall only tripped the ordinary 5s model cooldown the model would be
/// "healthy" again long before the task re-dispatches, so dispatch would re-pick
/// the same bad model — no failover. Holding the model unavailable for 5 minutes
/// guarantees the model is still cooling down on the task's next dispatch (even at
/// the 240s task-cooldown rung), so the next model in the creator's ordered list
/// is selected instead. The cooldown still auto-expires, so a one-off stall
/// self-heals; repeated stalls escalate via the normal ladder and pin at the cap.
const STALL_MIN_COOLDOWN: Duration = MAX_COOLDOWN;

/// Wire-format health state for a single model.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelHealth {
    pub model_id: String,
    pub auto_disabled: bool,
    pub consecutive_failures: u32,
    pub total_failures: u32,
    pub total_successes: u32,
    pub disable_ttl_trips: u32,
    /// Seconds until the cooldown expires; `None` when not currently disabled.
    pub cooldown_seconds_remaining: Option<u64>,
}

#[derive(Default)]
struct ModelState {
    auto_disabled: bool,
    cooldown_until: Option<Instant>,
    consecutive_failures: u32,
    total_failures: u32,
    total_successes: u32,
    disable_ttl_trips: u32,
}

impl ModelState {
    fn is_available(&self) -> bool {
        if !self.auto_disabled {
            return true;
        }
        // Cooldown expired → model auto-re-enables on next availability check.
        matches!(self.cooldown_until, Some(until) if Instant::now() >= until)
    }

    fn cooldown_seconds_remaining(&self) -> Option<u64> {
        let until = self.cooldown_until?;
        let now = Instant::now();
        if until > now {
            Some((until - now).as_secs())
        } else {
            None
        }
    }

    fn to_health(&self, model_id: &str) -> ModelHealth {
        ModelHealth {
            model_id: model_id.to_owned(),
            // Report as disabled only when cooldown has not yet expired.
            auto_disabled: self.auto_disabled && !self.is_available(),
            consecutive_failures: self.consecutive_failures,
            total_failures: self.total_failures,
            total_successes: self.total_successes,
            disable_ttl_trips: self.disable_ttl_trips,
            cooldown_seconds_remaining: self.cooldown_seconds_remaining(),
        }
    }

    fn compute_cooldown(&self) -> Duration {
        // Exponential backoff: 5s, 15s, 45s, 135s, 300s (capped).
        // disable_ttl_trips counts how many times this model has been disabled.
        let mut ttl = INITIAL_COOLDOWN;
        for _ in 0..self.disable_ttl_trips {
            ttl = (ttl * 3).min(MAX_COOLDOWN);
        }
        ttl
    }
}

/// Thread-safe in-memory model health tracker with circuit-breaker logic.
///
/// Circuit breaker: after `CIRCUIT_BREAKER_THRESHOLD` consecutive failures the
/// model is auto-disabled with an exponentially growing cooldown.  Models
/// auto re-enable once the cooldown expires.
#[derive(Clone)]
pub struct HealthTracker {
    inner: Arc<Mutex<HashMap<String, ModelState>>>,
}

impl HealthTracker {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Record a successful invocation.  Resets consecutive failure counter;
    /// clears auto-disable state if the cooldown has expired.
    pub fn record_success(&self, model_id: &str) {
        let mut map = self.inner.lock().unwrap();
        let state = map.entry(model_id.to_owned()).or_default();
        state.consecutive_failures = 0;
        state.total_successes += 1;
        if state.auto_disabled && state.is_available() {
            state.auto_disabled = false;
            state.cooldown_until = None;
        }
    }

    /// Record a failed invocation.  Trips the circuit breaker when the
    /// consecutive failure threshold is reached.
    pub fn record_failure(&self, model_id: &str) {
        let mut map = self.inner.lock().unwrap();
        let state = map.entry(model_id.to_owned()).or_default();
        state.consecutive_failures += 1;
        state.total_failures += 1;

        // If the previous cooldown expired, clear the flag so we can re-trip.
        if state.auto_disabled && state.is_available() {
            state.auto_disabled = false;
            state.cooldown_until = None;
        }

        if !state.auto_disabled && state.consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD {
            let cooldown = state.compute_cooldown();
            state.auto_disabled = true;
            state.cooldown_until = Some(Instant::now() + cooldown);
            state.disable_ttl_trips += 1;
            tracing::warn!(
                model_id,
                consecutive_failures = state.consecutive_failures,
                cooldown_secs = cooldown.as_secs(),
                "model circuit-breaker tripped"
            );
        }
    }

    /// Record a **stall** — a session that produced/consumed zero tokens and
    /// had its first LLM call hung (the coordinator's zero-token kill). This is
    /// a strong "this model/backend is bad right now" signal, so unlike
    /// [`record_failure`] it trips the breaker **immediately** (no
    /// consecutive-failure threshold) and with a cooldown floored at
    /// [`STALL_MIN_COOLDOWN`].
    ///
    /// The floor is what makes failover actually happen: the task that owned
    /// the stalled session is re-dispatched after an escalating *task* cooldown
    /// (60s → 120s → 240s). A 5-minute model cooldown guarantees the model is
    /// still unavailable when the task re-dispatches, so dispatch picks the next
    /// model in the creator's ordered list instead of re-selecting the bad one.
    /// The cooldown still auto-expires (self-heal), and repeated stalls escalate
    /// `disable_ttl_trips` so a persistently-bad model stays demoted at the cap.
    pub fn record_stall(&self, model_id: &str) {
        let mut map = self.inner.lock().unwrap();
        let state = map.entry(model_id.to_owned()).or_default();
        state.consecutive_failures += 1;
        state.total_failures += 1;

        // If the previous cooldown expired, clear the flag so we can re-trip
        // (and so `disable_ttl_trips` keeps escalating across stalls).
        if state.auto_disabled && state.is_available() {
            state.auto_disabled = false;
            state.cooldown_until = None;
        }

        // Already cooling down from an earlier trip — don't shorten or reset it.
        if state.auto_disabled {
            return;
        }

        // Trip immediately, cooldown floored at STALL_MIN_COOLDOWN so it
        // outlasts the task's redispatch cooldown and forces failover.
        let cooldown = state.compute_cooldown().max(STALL_MIN_COOLDOWN);
        state.auto_disabled = true;
        state.cooldown_until = Some(Instant::now() + cooldown);
        state.disable_ttl_trips += 1;
        tracing::warn!(
            model_id,
            consecutive_failures = state.consecutive_failures,
            cooldown_secs = cooldown.as_secs(),
            "model circuit-breaker tripped on stall (failing over to next model)"
        );
    }

    /// Returns `true` when the model is not circuit-breaker disabled
    /// (or when its cooldown has expired).
    pub fn is_available(&self, model_id: &str) -> bool {
        let map = self.inner.lock().unwrap();
        map.get(model_id).is_none_or(|s| s.is_available())
    }

    /// Return health state for all tracked models, sorted by model ID.
    pub fn all_health(&self) -> Vec<ModelHealth> {
        let map = self.inner.lock().unwrap();
        let mut health: Vec<_> = map.iter().map(|(id, s)| s.to_health(id)).collect();
        health.sort_by(|a, b| a.model_id.cmp(&b.model_id));
        health
    }

    /// Replace all tracked model health state with a persisted snapshot.
    pub fn restore_all(&self, snapshot: Vec<ModelHealth>) {
        let mut map = self.inner.lock().unwrap();
        map.clear();
        for health in snapshot {
            let mut state = ModelState {
                auto_disabled: health.auto_disabled,
                cooldown_until: None,
                consecutive_failures: health.consecutive_failures,
                total_failures: health.total_failures,
                total_successes: health.total_successes,
                disable_ttl_trips: health.disable_ttl_trips,
            };

            if health.auto_disabled {
                if let Some(seconds) = health.cooldown_seconds_remaining {
                    if seconds > 0 {
                        state.cooldown_until = Some(Instant::now() + Duration::from_secs(seconds));
                    } else {
                        state.auto_disabled = false;
                    }
                } else {
                    state.auto_disabled = false;
                }
            }

            map.insert(health.model_id, state);
        }
    }

    /// Return health state for a single model (returns zero state if untracked).
    pub fn model_health(&self, model_id: &str) -> ModelHealth {
        let map = self.inner.lock().unwrap();
        map.get(model_id)
            .map(|s| s.to_health(model_id))
            .unwrap_or_else(|| ModelHealth {
                model_id: model_id.to_owned(),
                auto_disabled: false,
                consecutive_failures: 0,
                total_failures: 0,
                total_successes: 0,
                disable_ttl_trips: 0,
                cooldown_seconds_remaining: None,
            })
    }

    /// Reset failure/success counters and re-enable a model.
    pub fn reset(&self, model_id: &str) {
        let mut map = self.inner.lock().unwrap();
        map.insert(model_id.to_owned(), ModelState::default());
    }

    /// Reset all tracked models.
    pub fn reset_all(&self) {
        let mut map = self.inner.lock().unwrap();
        map.clear();
    }

    /// Re-enable an auto-disabled model without clearing counters.
    pub fn enable(&self, model_id: &str) {
        let mut map = self.inner.lock().unwrap();
        let state = map.entry(model_id.to_owned()).or_default();
        state.auto_disabled = false;
        state.cooldown_until = None;
    }
}

impl Default for HealthTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_MODEL: &str = "model";

    fn expire_cooldown(ht: &HealthTracker, model_id: &str) {
        let mut map = ht.inner.lock().unwrap();
        let state = map.get_mut(model_id).unwrap();
        state.cooldown_until = Some(Instant::now() - Duration::from_millis(1));
    }

    fn trip_breaker(ht: &HealthTracker, model_id: &str) -> ModelHealth {
        for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
            ht.record_failure(model_id);
        }
        ht.model_health(model_id)
    }

    #[test]
    fn healthy_model_is_available() {
        let ht = HealthTracker::new();
        assert!(ht.is_available("gpt-4o"));
    }

    #[test]
    fn circuit_breaker_trips_at_threshold() {
        let ht = HealthTracker::new();
        let pre_threshold = CIRCUIT_BREAKER_THRESHOLD - 1;

        for _ in 0..pre_threshold {
            ht.record_failure("bad-model");
        }

        let before_trip = ht.model_health("bad-model");
        assert!(ht.is_available("bad-model"));
        assert!(!before_trip.auto_disabled);
        assert_eq!(before_trip.consecutive_failures, pre_threshold);
        assert_eq!(before_trip.total_failures, pre_threshold);
        assert_eq!(before_trip.disable_ttl_trips, 0);
        assert!(before_trip.cooldown_seconds_remaining.is_none());

        ht.record_failure("bad-model");

        assert!(!ht.is_available("bad-model"));
        let h = ht.model_health("bad-model");
        assert!(h.auto_disabled);
        assert_eq!(h.consecutive_failures, CIRCUIT_BREAKER_THRESHOLD);
        assert_eq!(h.total_failures, CIRCUIT_BREAKER_THRESHOLD);
        assert_eq!(h.disable_ttl_trips, 1);
        let remaining = h.cooldown_seconds_remaining.unwrap();
        assert!(remaining <= INITIAL_COOLDOWN.as_secs());
        assert!(remaining >= INITIAL_COOLDOWN.as_secs().saturating_sub(1));
    }

    #[test]
    fn success_resets_consecutive_counter() {
        let ht = HealthTracker::new();
        for _ in 0..(CIRCUIT_BREAKER_THRESHOLD - 1) {
            ht.record_failure(TEST_MODEL);
        }
        ht.record_success(TEST_MODEL);
        for _ in 0..(CIRCUIT_BREAKER_THRESHOLD - 1) {
            ht.record_failure(TEST_MODEL);
        }
        let h = ht.model_health(TEST_MODEL);
        assert_eq!(h.consecutive_failures, CIRCUIT_BREAKER_THRESHOLD - 1);
        assert!(!h.auto_disabled);
        assert_eq!(h.total_failures, 2 * (CIRCUIT_BREAKER_THRESHOLD - 1));
        assert_eq!(h.total_successes, 1);
        assert_eq!(h.disable_ttl_trips, 0);
        assert!(ht.is_available(TEST_MODEL));
    }

    #[test]
    fn reset_clears_state() {
        let ht = HealthTracker::new();
        for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
            ht.record_failure(TEST_MODEL);
        }
        ht.reset(TEST_MODEL);
        assert!(ht.is_available(TEST_MODEL));
        let h = ht.model_health(TEST_MODEL);
        assert_eq!(h.total_failures, 0);
    }

    #[test]
    fn enable_re_enables_without_clearing_counters() {
        let ht = HealthTracker::new();
        for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
            ht.record_failure(TEST_MODEL);
        }
        ht.enable(TEST_MODEL);
        assert!(ht.is_available(TEST_MODEL));
        let h = ht.model_health(TEST_MODEL);
        assert_eq!(h.total_failures, CIRCUIT_BREAKER_THRESHOLD);
    }

    #[test]
    fn compute_cooldown_grows_exponentially_and_caps() {
        let mut state = ModelState::default();

        assert_eq!(state.compute_cooldown(), INITIAL_COOLDOWN);

        state.disable_ttl_trips = 1;
        assert_eq!(state.compute_cooldown(), INITIAL_COOLDOWN * 3);

        state.disable_ttl_trips = 2;
        assert_eq!(state.compute_cooldown(), INITIAL_COOLDOWN * 9);

        state.disable_ttl_trips = 3;
        assert_eq!(state.compute_cooldown(), INITIAL_COOLDOWN * 27);

        state.disable_ttl_trips = 4;
        assert_eq!(state.compute_cooldown(), MAX_COOLDOWN);

        state.disable_ttl_trips = 12;
        assert_eq!(state.compute_cooldown(), MAX_COOLDOWN);
    }

    #[test]
    fn repeated_trips_increase_cooldown_and_expired_cooldown_reenables() {
        let ht = HealthTracker::new();

        for (expected_trip, expected_secs) in [(1_u32, 5_u64), (2, 15), (3, 45)] {
            let health = trip_breaker(&ht, TEST_MODEL);
            assert_eq!(health.disable_ttl_trips, expected_trip);
            assert!(health.auto_disabled);
            let remaining = health.cooldown_seconds_remaining.unwrap();
            assert!(remaining <= expected_secs);
            assert!(remaining >= expected_secs.saturating_sub(1));
            assert!(!ht.is_available(TEST_MODEL));

            expire_cooldown(&ht, TEST_MODEL);
            let expired = ht.model_health(TEST_MODEL);
            assert!(!expired.auto_disabled);
            assert!(expired.cooldown_seconds_remaining.is_none());
            assert!(ht.is_available(TEST_MODEL));
        }

        let expired = ht.model_health(TEST_MODEL);
        assert_eq!(expired.disable_ttl_trips, 3);
        assert_eq!(expired.consecutive_failures, CIRCUIT_BREAKER_THRESHOLD * 3);

        ht.record_success(TEST_MODEL);
        let reenabled = ht.model_health(TEST_MODEL);
        assert!(!reenabled.auto_disabled);
        assert_eq!(reenabled.consecutive_failures, 0);
    }

    #[test]
    fn repeated_trips_cooldown_caps_at_maximum() {
        let ht = HealthTracker::new();

        for expected_trip in 1..=6 {
            let health = trip_breaker(&ht, TEST_MODEL);
            assert_eq!(health.disable_ttl_trips, expected_trip);
            let remaining = health.cooldown_seconds_remaining.unwrap();
            assert!(remaining <= MAX_COOLDOWN.as_secs());
            if expected_trip == 4 {
                assert!(remaining <= 135);
                assert!(remaining >= 134);
            }
            if expected_trip >= 5 {
                assert!(remaining >= MAX_COOLDOWN.as_secs().saturating_sub(1));
            }

            expire_cooldown(&ht, TEST_MODEL);
        }

        let health = ht.model_health(TEST_MODEL);
        assert_eq!(health.disable_ttl_trips, 6);
        assert!(ht.is_available(TEST_MODEL));
    }

    #[test]
    fn stall_trips_immediately_without_consecutive_threshold() {
        let ht = HealthTracker::new();
        // A single stall (well below CIRCUIT_BREAKER_THRESHOLD) disables the model.
        ht.record_stall(TEST_MODEL);
        assert!(!ht.is_available(TEST_MODEL));
        let h = ht.model_health(TEST_MODEL);
        assert!(h.auto_disabled);
        assert_eq!(h.consecutive_failures, 1);
        assert_eq!(h.total_failures, 1);
        assert_eq!(h.disable_ttl_trips, 1);
    }

    #[test]
    fn stall_cooldown_outlasts_task_redispatch_cooldown() {
        // The coordinator re-dispatches a stalled task after an escalating
        // *task* cooldown: 60s (streak 1) → 120s → 240s. The model cooldown
        // from a stall must exceed those so the model is still unavailable when
        // the task re-dispatches, forcing failover to the next model.
        const MAX_TASK_REDISPATCH_COOLDOWN_SECS: u64 = 240;

        let ht = HealthTracker::new();
        ht.record_stall(TEST_MODEL);
        let remaining = ht
            .model_health(TEST_MODEL)
            .cooldown_seconds_remaining
            .expect("stalled model must be cooling down");
        assert!(
            remaining > MAX_TASK_REDISPATCH_COOLDOWN_SECS,
            "stall cooldown {remaining}s must outlast the {MAX_TASK_REDISPATCH_COOLDOWN_SECS}s task redispatch cooldown"
        );
        // Specifically: floored at the 5-minute cap.
        assert!(remaining <= STALL_MIN_COOLDOWN.as_secs());
        assert!(remaining >= STALL_MIN_COOLDOWN.as_secs().saturating_sub(1));
    }

    #[test]
    fn stall_self_heals_after_cooldown_then_success_resets() {
        let ht = HealthTracker::new();
        ht.record_stall(TEST_MODEL);
        assert!(!ht.is_available(TEST_MODEL));

        // Cooldown expires → model auto-re-enables (one-off stall self-heals).
        expire_cooldown(&ht, TEST_MODEL);
        assert!(ht.is_available(TEST_MODEL));
        assert!(!ht.model_health(TEST_MODEL).auto_disabled);

        // A successful turn resets the consecutive-failure counter.
        ht.record_success(TEST_MODEL);
        let h = ht.model_health(TEST_MODEL);
        assert_eq!(h.consecutive_failures, 0);
        assert_eq!(h.total_successes, 1);
        assert!(ht.is_available(TEST_MODEL));
    }

    #[test]
    fn repeated_stalls_escalate_trips_and_stay_capped() {
        let ht = HealthTracker::new();
        for expected_trip in 1..=4 {
            ht.record_stall(TEST_MODEL);
            let h = ht.model_health(TEST_MODEL);
            assert_eq!(h.disable_ttl_trips, expected_trip);
            assert!(h.auto_disabled);
            // Every stall cooldown is floored at the cap.
            let remaining = h.cooldown_seconds_remaining.unwrap();
            assert!(remaining <= MAX_COOLDOWN.as_secs());
            assert!(remaining >= MAX_COOLDOWN.as_secs().saturating_sub(1));
            assert!(!ht.is_available(TEST_MODEL));
            expire_cooldown(&ht, TEST_MODEL);
        }
    }

    #[test]
    fn stall_while_cooling_down_does_not_shorten_cooldown() {
        let ht = HealthTracker::new();
        ht.record_stall(TEST_MODEL);
        let first = ht.model_health(TEST_MODEL);
        assert_eq!(first.disable_ttl_trips, 1);

        // A second stall arriving while still cooling down is a no-op for the
        // cooldown window (no re-trip, no reset) — it only bumps failure counts.
        ht.record_stall(TEST_MODEL);
        let second = ht.model_health(TEST_MODEL);
        assert_eq!(second.disable_ttl_trips, 1, "no re-trip while cooling down");
        assert_eq!(second.consecutive_failures, 2);
        assert!(!ht.is_available(TEST_MODEL));
    }

    #[test]
    fn restore_all_rehydrates_disabled_state_without_new_trip() {
        let ht = HealthTracker::new();
        ht.restore_all(vec![ModelHealth {
            model_id: "a/model".to_string(),
            auto_disabled: true,
            consecutive_failures: CIRCUIT_BREAKER_THRESHOLD,
            total_failures: 10,
            total_successes: 2,
            disable_ttl_trips: 1,
            cooldown_seconds_remaining: Some(4),
        }]);

        let h = ht.model_health("a/model");
        assert!(h.auto_disabled);
        assert!(!ht.is_available("a/model"));
        assert_eq!(h.consecutive_failures, CIRCUIT_BREAKER_THRESHOLD);
        assert_eq!(h.total_failures, 10);
        assert_eq!(h.total_successes, 2);
        assert_eq!(h.disable_ttl_trips, 1);
        let remaining = h.cooldown_seconds_remaining.unwrap();
        assert!(remaining <= 4);
        assert!(remaining >= 3);

        ht.enable("a/model");
        let enabled = ht.model_health("a/model");
        assert!(ht.is_available("a/model"));
        assert!(!enabled.auto_disabled);
        assert_eq!(enabled.disable_ttl_trips, 1);
        assert_eq!(enabled.consecutive_failures, CIRCUIT_BREAKER_THRESHOLD);

        ht.record_success("a/model");
        let restored = ht.model_health("a/model");
        assert!(ht.is_available("a/model"));
        assert!(!restored.auto_disabled);
        assert_eq!(restored.disable_ttl_trips, 1);
        assert_eq!(restored.consecutive_failures, 0);

        for failures in 1..CIRCUIT_BREAKER_THRESHOLD {
            ht.record_failure("a/model");
            let health = ht.model_health("a/model");
            assert!(ht.is_available("a/model"));
            assert!(!health.auto_disabled);
            assert_eq!(health.disable_ttl_trips, 1);
            assert_eq!(health.consecutive_failures, failures);
        }

        let before_expiry = ht.model_health("a/model");
        assert_eq!(before_expiry.disable_ttl_trips, 1);

        ht.restore_all(vec![ModelHealth {
            model_id: "a/model".to_string(),
            auto_disabled: true,
            consecutive_failures: CIRCUIT_BREAKER_THRESHOLD,
            total_failures: 10,
            total_successes: 2,
            disable_ttl_trips: 1,
            cooldown_seconds_remaining: Some(2),
        }]);
        assert!(!ht.is_available("a/model"));
        assert_eq!(ht.model_health("a/model").disable_ttl_trips, 1);

        expire_cooldown(&ht, "a/model");
        let cooled_off = ht.model_health("a/model");
        assert!(ht.is_available("a/model"));
        assert!(!cooled_off.auto_disabled);
        assert_eq!(cooled_off.disable_ttl_trips, 1);
        assert!(cooled_off.cooldown_seconds_remaining.is_none());
    }
}
