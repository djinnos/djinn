use std::time::Duration;

/// Exponential backoff state for sync push failures.
///
/// Schedule: 30s → 60s → 120s → 240s → 480s → 900s (15-min cap).
#[derive(Clone, Debug, Default)]
pub struct BackoffState {
    failures: u32,
}

impl BackoffState {
    pub const MIN_DELAY_SECS: u64 = 30;
    pub const MAX_DELAY_SECS: u64 = 900; // 15 min

    pub fn new() -> Self {
        Self::default()
    }

    /// Record a failure; returns the delay to wait before the next attempt.
    pub fn record_failure(&mut self) -> Duration {
        self.failures = self.failures.saturating_add(1);
        self.delay()
    }

    /// Record a success and reset the counter.
    pub fn record_success(&mut self) {
        self.failures = 0;
    }

    pub fn failure_count(&self) -> u32 {
        self.failures
    }

    /// Current backoff delay (zero when no failures).
    pub fn delay(&self) -> Duration {
        if self.failures == 0 {
            return Duration::ZERO;
        }
        let shifts = (self.failures - 1).min(5);
        let secs = Self::MIN_DELAY_SECS.saturating_mul(1u64 << shifts);
        Duration::from_secs(secs.min(Self::MAX_DELAY_SECS))
    }

    /// Delay in seconds for status reporting.
    pub fn delay_secs(&self) -> u64 {
        self.delay().as_secs()
    }

    /// Whether the channel needs attention (3+ failures).
    pub fn needs_attention(&self) -> bool {
        self.failures >= 3
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_failures_zero_delay() {
        assert_eq!(BackoffState::new().delay(), Duration::ZERO);
    }

    #[test]
    fn first_failure_30s() {
        let mut b = BackoffState::new();
        assert_eq!(b.record_failure(), Duration::from_secs(30));
    }

    #[test]
    fn second_failure_60s() {
        let mut b = BackoffState::new();
        b.record_failure();
        assert_eq!(b.record_failure(), Duration::from_secs(60));
    }

    #[test]
    fn fifth_failure_480s() {
        let mut b = BackoffState::new();
        for _ in 0..4 {
            b.record_failure();
        }
        assert_eq!(b.record_failure(), Duration::from_secs(480));
    }

    #[test]
    fn caps_at_15min() {
        let mut b = BackoffState::new();
        for _ in 0..20 {
            b.record_failure();
        }
        assert_eq!(b.delay(), Duration::from_secs(900));
    }

    #[test]
    fn success_resets() {
        let mut b = BackoffState::new();
        b.record_failure();
        b.record_failure();
        b.record_success();
        assert_eq!(b.delay(), Duration::ZERO);
        assert_eq!(b.failure_count(), 0);
    }

    #[test]
    fn needs_attention_false_when_failure_count_lt_3() {
        let mut b = BackoffState::new();
        assert!(!b.needs_attention());
        b.record_failure();
        assert!(!b.needs_attention());
        b.record_failure();
        assert!(!b.needs_attention());
    }

    #[test]
    fn needs_attention_true_when_failure_count_gte_3() {
        let mut b = BackoffState::new();
        b.record_failure();
        b.record_failure();
        b.record_failure();
        assert!(b.needs_attention());
        b.record_failure();
        assert!(b.needs_attention());
    }

    #[test]
    fn needs_attention_resets_on_success() {
        let mut b = BackoffState::new();
        for _ in 0..5 {
            b.record_failure();
        }
        assert!(b.needs_attention());
        b.record_success();
        assert!(!b.needs_attention());
    }
}
