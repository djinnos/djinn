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

struct ModelState {
    auto_disabled: bool,
    cooldown_until: Option<Instant>,
    consecutive_failures: u32,
    total_failures: u32,
    total_successes: u32,
    disable_ttl_trips: u32,
}

impl Default for ModelState {
    fn default() -> Self {
        Self {
            auto_disabled: false,
            cooldown_until: None,
            consecutive_failures: 0,
            total_failures: 0,
            total_successes: 0,
            disable_ttl_trips: 0,
        }
    }
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

    /// Returns `true` when the model is not circuit-breaker disabled
    /// (or when its cooldown has expired).
    pub fn is_available(&self, model_id: &str) -> bool {
        let map = self.inner.lock().unwrap();
        map.get(model_id).map_or(true, |s| s.is_available())
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

    #[test]
    fn healthy_model_is_available() {
        let ht = HealthTracker::new();
        assert!(ht.is_available("gpt-4o"));
    }

    #[test]
    fn circuit_breaker_trips_at_threshold() {
        let ht = HealthTracker::new();
        for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
            ht.record_failure("bad-model");
        }
        assert!(!ht.is_available("bad-model"));
        let h = ht.model_health("bad-model");
        assert!(h.auto_disabled);
        assert!(h.cooldown_seconds_remaining.is_some());
    }

    #[test]
    fn success_resets_consecutive_counter() {
        let ht = HealthTracker::new();
        for _ in 0..(CIRCUIT_BREAKER_THRESHOLD - 1) {
            ht.record_failure("model");
        }
        ht.record_success("model");
        let h = ht.model_health("model");
        assert_eq!(h.consecutive_failures, 0);
        assert!(!h.auto_disabled);
    }

    #[test]
    fn reset_clears_state() {
        let ht = HealthTracker::new();
        for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
            ht.record_failure("model");
        }
        ht.reset("model");
        assert!(ht.is_available("model"));
        let h = ht.model_health("model");
        assert_eq!(h.total_failures, 0);
    }

    #[test]
    fn enable_re_enables_without_clearing_counters() {
        let ht = HealthTracker::new();
        for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
            ht.record_failure("model");
        }
        ht.enable("model");
        assert!(ht.is_available("model"));
        // Counters preserved
        let h = ht.model_health("model");
        assert_eq!(h.total_failures, CIRCUIT_BREAKER_THRESHOLD);
    }

    #[test]
    fn exponential_backoff_doubles_each_trip() {
        let ht = HealthTracker::new();
        // First trip.
        for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
            ht.record_failure("model");
        }
        let h1 = ht.model_health("model");
        assert_eq!(h1.disable_ttl_trips, 1);

        // Re-enable and trip again.
        ht.enable("model");
        {
            let mut map = ht.inner.lock().unwrap();
            let s = map.get_mut("model").unwrap();
            s.consecutive_failures = 0;
        }
        for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
            ht.record_failure("model");
        }
        let h2 = ht.model_health("model");
        assert_eq!(h2.disable_ttl_trips, 2);
        // Second trip cooldown should be 3x the first (i.e. ≥ 15s).
        let secs = h2.cooldown_seconds_remaining.unwrap_or(0);
        assert!(
            secs > INITIAL_COOLDOWN.as_secs(),
            "second trip cooldown should be longer"
        );
    }

    #[test]
    fn restore_all_rehydrates_persisted_state() {
        let ht = HealthTracker::new();
        ht.restore_all(vec![ModelHealth {
            model_id: "a/model".to_string(),
            auto_disabled: true,
            consecutive_failures: 3,
            total_failures: 10,
            total_successes: 2,
            disable_ttl_trips: 1,
            cooldown_seconds_remaining: Some(120),
        }]);

        let h = ht.model_health("a/model");
        assert!(h.auto_disabled);
        assert_eq!(h.total_failures, 10);
    }
}
