use std::time::{Duration, Instant};
use std::{
    sync::{Mutex, MutexGuard, OnceLock},
    thread,
};

use djinn_provider::rate_limit::{
    activate_suppression_window, clear_suppression_window, suppression_remaining,
    suppression_state_snapshot,
};

fn rate_limit_test_guard() -> MutexGuard<'static, ()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("rate-limit test guard poisoned")
}

#[test]
fn activating_suppression_records_shared_window() {
    let _guard = rate_limit_test_guard();
    clear_suppression_window();
    let until = activate_suppression_window(Duration::from_secs(2));
    let snapshot = suppression_state_snapshot();
    assert_eq!(snapshot.active_until, Some(until));
    assert!(snapshot.is_active_at(Instant::now()));
}

#[test]
fn expired_snapshot_is_cleared_when_read() {
    let _guard = rate_limit_test_guard();
    clear_suppression_window();
    activate_suppression_window(Duration::from_millis(5));

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if suppression_remaining(Instant::now()).is_none() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "suppression window did not expire before deadline"
        );
        thread::sleep(Duration::from_millis(1));
    }

    assert!(suppression_state_snapshot().active_until.is_none());
}
