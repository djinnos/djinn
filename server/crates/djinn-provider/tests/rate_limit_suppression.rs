use std::time::{Duration, Instant};

use djinn_provider::rate_limit::{
    activate_suppression_window, clear_suppression_window, suppression_remaining,
    suppression_state_snapshot,
};

#[test]
fn activating_suppression_records_shared_window() {
    clear_suppression_window();
    let until = activate_suppression_window(Duration::from_secs(2));
    let snapshot = suppression_state_snapshot();
    assert_eq!(snapshot.active_until, Some(until));
    assert!(snapshot.is_active_at(Instant::now()));
}

#[test]
fn expired_snapshot_is_cleared_when_read() {
    clear_suppression_window();
    activate_suppression_window(Duration::from_millis(5));

    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if suppression_remaining(Instant::now()).is_none() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "suppression window did not expire before deadline"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    assert!(suppression_state_snapshot().active_until.is_none());
}
