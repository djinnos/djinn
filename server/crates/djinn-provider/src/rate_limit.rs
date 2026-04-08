use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SuppressionState {
    pub active_until: Option<Instant>,
}

impl SuppressionState {
    pub fn is_active_at(&self, now: Instant) -> bool {
        self.active_until.is_some_and(|until| until > now)
    }
}

fn suppression_state() -> &'static Mutex<SuppressionState> {
    static STATE: OnceLock<Mutex<SuppressionState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(SuppressionState { active_until: None }))
}

pub fn activate_suppression_window(delay: Duration) -> Instant {
    let until = Instant::now() + delay;
    let mut state = suppression_state()
        .lock()
        .expect("rate-limit state poisoned");
    state.active_until = Some(
        state
            .active_until
            .map_or(until, |current| current.max(until)),
    );
    state
        .active_until
        .expect("suppression window should be set")
}

pub fn clear_suppression_window() {
    let mut state = suppression_state()
        .lock()
        .expect("rate-limit state poisoned");
    state.active_until = None;
}

pub fn suppression_state_snapshot() -> SuppressionState {
    *suppression_state()
        .lock()
        .expect("rate-limit state poisoned")
}

pub fn suppression_remaining(now: Instant) -> Option<Duration> {
    let mut state = suppression_state()
        .lock()
        .expect("rate-limit state poisoned");
    match state.active_until {
        Some(until) if until > now => Some(until.duration_since(now)),
        Some(_) => {
            state.active_until = None;
            None
        }
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppression_window_extends_and_expires() {
        clear_suppression_window();
        let first = activate_suppression_window(Duration::from_millis(25));
        let second = activate_suppression_window(Duration::from_millis(50));
        assert!(second >= first);
        assert!(suppression_state_snapshot().is_active_at(Instant::now()));
        assert!(suppression_remaining(Instant::now()).is_some());
        clear_suppression_window();
        assert!(suppression_remaining(Instant::now()).is_none());
    }
}
