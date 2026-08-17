//! Unit tests for [`super`] — the model circuit-breaker / health tracker.
//!
//! Split into a sibling file to keep `health.rs` under the repo source-size
//! guard; still a child module of `health` (via `#[path]`), so `use super::*`
//! reaches the parent module's private items.

use super::*;

const TEST_MODEL: &str = "model";
// Most tests exercise the shared (None) bucket; per-scope isolation has its
// own dedicated tests below.
const S: Option<&str> = None;

fn expire_cooldown(ht: &HealthTracker, scope: Option<&str>, model_id: &str) {
    let mut map = ht.inner.lock().unwrap();
    let state = map.get_mut(&HealthKey::new(scope, model_id)).unwrap();
    state.cooldown_until = Some(Instant::now() - Duration::from_millis(1));
}

fn trip_breaker(ht: &HealthTracker, model_id: &str) -> ModelHealth {
    for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
        ht.record_failure(S, model_id);
    }
    ht.model_health(S, model_id)
}

#[test]
fn debug_snapshot_returns_only_non_closed_entries() {
    let ht = HealthTracker::new();
    {
        let mut map = ht.inner.lock().unwrap();
        map.insert(
            HealthKey::new(Some("user-open"), "open-model"),
            ModelState {
                auto_disabled: true,
                cooldown_until: Some(Instant::now() + Duration::from_secs(60)),
                consecutive_failures: 3,
                total_failures: 3,
                total_successes: 0,
                disable_ttl_trips: 1,
                ..Default::default()
            },
        );
        map.insert(
            HealthKey::new(Some("user-half"), "half-model"),
            ModelState {
                auto_disabled: true,
                cooldown_until: Some(Instant::now() - Duration::from_secs(1)),
                consecutive_failures: 4,
                total_failures: 4,
                total_successes: 0,
                disable_ttl_trips: 1,
                ..Default::default()
            },
        );
        map.insert(
            HealthKey::new(Some("user-closed"), "closed-model"),
            ModelState {
                auto_disabled: false,
                cooldown_until: None,
                consecutive_failures: 0,
                total_failures: 0,
                total_successes: 1,
                disable_ttl_trips: 0,
                ..Default::default()
            },
        );
    }

    let snapshot = ht.debug_snapshot();
    assert_eq!(snapshot.len(), 2);
    assert!(
        snapshot
            .iter()
            .any(|entry| entry.model == "open-model" && entry.state == "open")
    );
    assert!(
        snapshot
            .iter()
            .any(|entry| entry.model == "half-model" && entry.state == "half_open")
    );
    assert!(!snapshot.iter().any(|entry| entry.model == "closed-model"));
}

#[test]
fn healthy_model_is_available() {
    let ht = HealthTracker::new();
    assert!(ht.is_available(S, "gpt-4o"));
}

#[test]
fn circuit_breaker_trips_at_threshold() {
    let ht = HealthTracker::new();
    let pre_threshold = CIRCUIT_BREAKER_THRESHOLD - 1;

    for _ in 0..pre_threshold {
        ht.record_failure(S, "bad-model");
    }

    let before_trip = ht.model_health(S, "bad-model");
    assert!(ht.is_available(S, "bad-model"));
    assert!(!before_trip.auto_disabled);
    assert_eq!(before_trip.consecutive_failures, pre_threshold);
    assert_eq!(before_trip.total_failures, pre_threshold);
    assert_eq!(before_trip.disable_ttl_trips, 0);
    assert!(before_trip.cooldown_seconds_remaining.is_none());

    ht.record_failure(S, "bad-model");

    assert!(!ht.is_available(S, "bad-model"));
    let h = ht.model_health(S, "bad-model");
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
        ht.record_failure(S, TEST_MODEL);
    }
    ht.record_success(S, TEST_MODEL);
    for _ in 0..(CIRCUIT_BREAKER_THRESHOLD - 1) {
        ht.record_failure(S, TEST_MODEL);
    }
    let h = ht.model_health(S, TEST_MODEL);
    assert_eq!(h.consecutive_failures, CIRCUIT_BREAKER_THRESHOLD - 1);
    assert!(!h.auto_disabled);
    assert_eq!(h.total_failures, 2 * (CIRCUIT_BREAKER_THRESHOLD - 1));
    assert_eq!(h.total_successes, 1);
    assert_eq!(h.disable_ttl_trips, 0);
    assert!(ht.is_available(S, TEST_MODEL));
}

#[test]
fn reset_clears_state() {
    let ht = HealthTracker::new();
    for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
        ht.record_failure(S, TEST_MODEL);
    }
    ht.reset(S, TEST_MODEL);
    assert!(ht.is_available(S, TEST_MODEL));
    let h = ht.model_health(S, TEST_MODEL);
    assert_eq!(h.total_failures, 0);
}

#[test]
fn enable_re_enables_without_clearing_counters() {
    let ht = HealthTracker::new();
    for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
        ht.record_failure(S, TEST_MODEL);
    }
    ht.enable(S, TEST_MODEL);
    assert!(ht.is_available(S, TEST_MODEL));
    let h = ht.model_health(S, TEST_MODEL);
    assert_eq!(h.total_failures, CIRCUIT_BREAKER_THRESHOLD);
}

#[test]
fn success_resets_escalation_tier_and_trip_window() {
    // Fix 1: a productive session on a model that had been escalating must
    // reset both the cooldown tier (`disable_ttl_trips`) and the rolling
    // trip-rate window, so the next failure starts fresh at the base cooldown
    // and old, since-recovered trips don't count toward the hard-disable
    // ceiling.
    let ht = HealthTracker::new();
    for _ in 0..3 {
        trip_breaker(&ht, TEST_MODEL);
        expire_cooldown(&ht, S, TEST_MODEL);
    }
    let escalated = ht.model_health(S, TEST_MODEL);
    assert_eq!(escalated.disable_ttl_trips, 3);
    assert_eq!(escalated.trips_in_window, 3);

    ht.record_success(S, TEST_MODEL);
    let recovered = ht.model_health(S, TEST_MODEL);
    assert_eq!(recovered.disable_ttl_trips, 0, "success resets the tier");
    assert_eq!(recovered.trips_in_window, 0, "success clears the window");
    assert_eq!(recovered.consecutive_failures, 0);

    // The next trip starts back at the base cooldown, not the escalated one.
    let h = trip_breaker(&ht, TEST_MODEL);
    assert_eq!(h.disable_ttl_trips, 1);
    let remaining = h.cooldown_seconds_remaining.unwrap();
    assert!(remaining <= INITIAL_COOLDOWN.as_secs());
    assert!(remaining >= INITIAL_COOLDOWN.as_secs().saturating_sub(1));
}

#[test]
fn trip_rate_ceiling_hard_disables_until_manual_enable() {
    // Fix 1: a model that trips CEILING times within the window is hard-
    // disabled with NO auto-expiry; only the human `enable` path recovers it.
    let ht = HealthTracker::new();
    for _ in 0..TRIP_RATE_CEILING {
        trip_breaker(&ht, TEST_MODEL);
        expire_cooldown(&ht, S, TEST_MODEL);
    }
    let h = ht.model_health(S, TEST_MODEL);
    assert!(h.hard_disabled);
    assert!(h.auto_disabled);
    assert!(h.cooldown_seconds_remaining.is_none());
    assert_eq!(h.trips_in_window, TRIP_RATE_CEILING as u32);
    // No amount of cooldown expiry re-enables a hard-disabled bucket.
    expire_cooldown(&ht, S, TEST_MODEL);
    assert!(!ht.is_available(S, TEST_MODEL));

    // The existing admin `enable` action clears the hard-disable AND the
    // trip window, so the model is usable again and does not instantly
    // re-hard-disable on the next trip.
    ht.enable(S, TEST_MODEL);
    assert!(ht.is_available(S, TEST_MODEL));
    let enabled = ht.model_health(S, TEST_MODEL);
    assert!(!enabled.hard_disabled);
    assert_eq!(enabled.trips_in_window, 0);
    // Counters (total failures) are preserved by enable.
    assert!(enabled.total_failures >= TRIP_RATE_CEILING as u32);
}

#[test]
fn throttle_stalls_below_persistence_threshold_never_escalate() {
    // A quota throttle (`escalate = false`) resets on a clock, not on model
    // health, so — up to the persistence threshold — it must NOT count toward
    // the hard-disable ceiling or ratchet the escalating cap, and must keep
    // auto-recovering. (Persistent throttling past the threshold DOES escalate;
    // see `persistent_throttle_escalates_after_consecutive_trips`.)
    let ht = HealthTracker::new();
    for _ in 0..(PERSISTENT_THROTTLE_TRIP_THRESHOLD - 1) {
        ht.record_stall(S, TEST_MODEL, false);
        let h = ht.model_health(S, TEST_MODEL);
        assert!(
            !h.hard_disabled,
            "sub-threshold throttles never hard-disable"
        );
        assert_eq!(
            h.trips_in_window, 0,
            "sub-threshold throttles are not counted"
        );
        assert_eq!(h.disable_ttl_trips, 0);
        expire_cooldown(&ht, S, TEST_MODEL);
        assert!(ht.is_available(S, TEST_MODEL), "throttle always self-heals");
    }
}

#[test]
fn persistent_throttle_escalates_after_consecutive_trips() {
    // Fix (persistent-throttle escalation): a plan/subscription that has been
    // over-quota for days keeps flapping (throttle-cooldown → re-enable →
    // crash → repeat) and never escalates. Once a bucket has throttle-tripped
    // PERSISTENT_THROTTLE_TRIP_THRESHOLD consecutive times with no success,
    // further throttle trips escalate like genuine failures — advancing
    // `disable_ttl_trips` AND counting toward the hard-disable ceiling.
    let ht = HealthTracker::new();

    // Trips below the threshold do not escalate.
    for _ in 0..(PERSISTENT_THROTTLE_TRIP_THRESHOLD - 1) {
        ht.record_stall(S, TEST_MODEL, false);
        let h = ht.model_health(S, TEST_MODEL);
        assert_eq!(h.disable_ttl_trips, 0);
        assert_eq!(h.trips_in_window, 0);
        expire_cooldown(&ht, S, TEST_MODEL);
    }

    // The threshold-th consecutive throttle trip escalates.
    ht.record_stall(S, TEST_MODEL, false);
    let h = ht.model_health(S, TEST_MODEL);
    assert_eq!(
        h.disable_ttl_trips, 1,
        "persistent throttle now ratchets the escalating cap"
    );
    assert_eq!(
        h.trips_in_window, 1,
        "and now counts toward the hard-disable ceiling"
    );
    // The floored stall cooldown still applies (>= the task-redispatch ladder).
    assert!(!ht.is_available(S, TEST_MODEL));
}

#[test]
fn success_resets_persistent_throttle_escalation() {
    // A single productive session must fully reset the throttle streak: the
    // plan's quota came back, so the model returns to the gentle clock-reset
    // treatment and the next throttle starts a fresh streak from trip 1.
    let ht = HealthTracker::new();
    for _ in 0..(PERSISTENT_THROTTLE_TRIP_THRESHOLD - 1) {
        ht.record_stall(S, TEST_MODEL, false);
        expire_cooldown(&ht, S, TEST_MODEL);
    }
    ht.record_success(S, TEST_MODEL);

    // The next throttle trip is treated as trip 1 again — no escalation.
    ht.record_stall(S, TEST_MODEL, false);
    let h = ht.model_health(S, TEST_MODEL);
    assert_eq!(
        h.disable_ttl_trips, 0,
        "success reset the streak, so this throttle does not escalate"
    );
    assert_eq!(h.trips_in_window, 0);
    assert!(!h.hard_disabled);
}

#[test]
fn persistent_throttle_eventually_hard_disables() {
    // The end-to-end backstop: a subscription that keeps 429-ing forever with
    // no success must eventually be hard-disabled (held until a human
    // re-enables) instead of flapping indefinitely and burning task wall-clock.
    let ht = HealthTracker::new();
    for _ in 0..(PERSISTENT_THROTTLE_TRIP_THRESHOLD + TRIP_RATE_CEILING as u32) {
        ht.record_stall(S, TEST_MODEL, false);
        expire_cooldown(&ht, S, TEST_MODEL);
    }
    let h = ht.model_health(S, TEST_MODEL);
    assert!(
        h.hard_disabled,
        "a plan throttling for days must eventually hard-disable"
    );
    assert!(
        !ht.is_available(S, TEST_MODEL),
        "a hard-disabled bucket never self-heals on the clock"
    );
}

#[test]
fn persistent_throttle_escalates_on_wall_clock_window() {
    // The wall-clock companion: even when the consecutive-trip *count* is still
    // low, a throttle streak that has lasted PERSISTENT_THROTTLE_WINDOW (sparse
    // but continuous throttling — long provider retry-after windows) escalates.
    let ht = HealthTracker::new();
    // First throttle trip starts the streak.
    ht.record_stall(S, TEST_MODEL, false);
    assert_eq!(ht.model_health(S, TEST_MODEL).disable_ttl_trips, 0);
    // Age the streak start past the wall-clock window.
    {
        let mut map = ht.inner.lock().unwrap();
        let state = map.get_mut(&HealthKey::new(S, TEST_MODEL)).unwrap();
        state.throttle_streak_started_at =
            Some(Instant::now() - PERSISTENT_THROTTLE_WINDOW - Duration::from_secs(1));
    }
    expire_cooldown(&ht, S, TEST_MODEL);

    // The next throttle trip escalates on the wall-clock bound alone.
    ht.record_stall(S, TEST_MODEL, false);
    let h = ht.model_health(S, TEST_MODEL);
    assert_eq!(
        h.disable_ttl_trips, 1,
        "a throttle streak older than the window escalates regardless of count"
    );
    assert_eq!(h.trips_in_window, 1);
}

#[test]
fn is_throttle_cooling_tracks_throttle_cooldown_only() {
    let ht = HealthTracker::new();
    assert!(
        !ht.is_throttle_cooling(S, TEST_MODEL),
        "an untracked model is not throttle-cooling"
    );

    // A throttle trip flags the model as throttle-cooling…
    ht.record_stall(S, TEST_MODEL, false);
    assert!(ht.is_throttle_cooling(S, TEST_MODEL));

    // …and it stays flagged through the half-open window (cooldown expired but
    // no success has re-proven it) so dispatch keeps deprioritizing it.
    expire_cooldown(&ht, S, TEST_MODEL);
    assert!(
        ht.is_available(S, TEST_MODEL),
        "cooldown expired → available (half-open)"
    );
    assert!(
        ht.is_throttle_cooling(S, TEST_MODEL),
        "still throttle-cooling until a success clears it"
    );

    // A success clears the flag.
    ht.record_success(S, TEST_MODEL);
    assert!(!ht.is_throttle_cooling(S, TEST_MODEL));

    // A GENUINE (non-throttle) breaker trip is not reported as throttle-cooling.
    ht.reset(S, TEST_MODEL);
    ht.record_stall(S, TEST_MODEL, true);
    assert!(
        !ht.is_available(S, TEST_MODEL),
        "genuine stall trips the breaker"
    );
    assert!(
        !ht.is_throttle_cooling(S, TEST_MODEL),
        "a genuine (escalating) trip is not a throttle cooldown"
    );
}

#[test]
fn hard_disable_survives_persistence_round_trip() {
    // A hard-disable must persist across a leader failover (settings-blob
    // snapshot → restore) so a restart doesn't silently re-enable a model a
    // human hasn't cleared.
    let ht = HealthTracker::new();
    for _ in 0..TRIP_RATE_CEILING {
        trip_breaker(&ht, TEST_MODEL);
        expire_cooldown(&ht, S, TEST_MODEL);
    }
    assert!(ht.model_health(S, TEST_MODEL).hard_disabled);

    let snapshot = ht.all_health();
    let restored = HealthTracker::new();
    restored.restore_all(snapshot);
    let h = restored.model_health(S, TEST_MODEL);
    assert!(h.hard_disabled, "hard-disable survived the round trip");
    assert!(!restored.is_available(S, TEST_MODEL));
    assert!(
        h.cooldown_seconds_remaining.is_none(),
        "still no auto-expiry"
    );
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

    // 5s·3^n keeps climbing well past the old 5-minute ceiling now that the
    // cap is 4h: 5·3^7 = 10935s (~3h) is still below the cap…
    state.disable_ttl_trips = 7;
    assert_eq!(state.compute_cooldown(), INITIAL_COOLDOWN * 3u32.pow(7));
    assert!(state.compute_cooldown() < MAX_COOLDOWN);

    // …and 5·3^8 = 32805s (~9h) exceeds it, so it pins at MAX_COOLDOWN.
    state.disable_ttl_trips = 8;
    assert_eq!(state.compute_cooldown(), MAX_COOLDOWN);

    state.disable_ttl_trips = 20;
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
        assert!(!ht.is_available(S, TEST_MODEL));

        expire_cooldown(&ht, S, TEST_MODEL);
        let expired = ht.model_health(S, TEST_MODEL);
        assert!(!expired.auto_disabled);
        assert!(expired.cooldown_seconds_remaining.is_none());
        assert!(ht.is_available(S, TEST_MODEL));
    }

    let expired = ht.model_health(S, TEST_MODEL);
    assert_eq!(expired.disable_ttl_trips, 3);
    assert_eq!(expired.consecutive_failures, CIRCUIT_BREAKER_THRESHOLD * 3);

    ht.record_success(S, TEST_MODEL);
    let reenabled = ht.model_health(S, TEST_MODEL);
    assert!(!reenabled.auto_disabled);
    assert_eq!(reenabled.consecutive_failures, 0);
}

#[test]
fn repeated_trips_escalate_cooldown_then_hard_disable_at_ceiling() {
    let ht = HealthTracker::new();

    // Trips 1..CEILING escalate the cooldown (5s·3^(n-1)) and self-heal after
    // an expired cooldown — never reaching the multi-hour cap because the
    // hard-disable ceiling backstops first.
    for expected_trip in 1..TRIP_RATE_CEILING as u32 {
        let health = trip_breaker(&ht, TEST_MODEL);
        assert_eq!(health.disable_ttl_trips, expected_trip);
        assert!(
            !health.hard_disabled,
            "below the ceiling, not hard-disabled"
        );
        assert_eq!(health.trips_in_window, expected_trip);
        let remaining = health.cooldown_seconds_remaining.unwrap();
        let expected =
            (INITIAL_COOLDOWN.as_secs() * 3u64.pow(expected_trip - 1)).min(MAX_COOLDOWN.as_secs());
        assert!(remaining <= expected);
        assert!(remaining >= expected.saturating_sub(1));
        // Cooldown auto-expires → the bucket is available again (self-heal).
        expire_cooldown(&ht, S, TEST_MODEL);
        assert!(ht.is_available(S, TEST_MODEL));
    }

    // The CEILING-th trip within the window flips the bucket to hard-disabled:
    // no cooldown deadline, and NOT available even after expiry.
    let health = trip_breaker(&ht, TEST_MODEL);
    assert_eq!(health.disable_ttl_trips, TRIP_RATE_CEILING as u32);
    assert!(
        health.hard_disabled,
        "ceiling trip hard-disables the bucket"
    );
    assert!(health.auto_disabled, "hard-disabled reports as disabled");
    assert!(
        health.cooldown_seconds_remaining.is_none(),
        "no auto-expiry"
    );
    assert!(!ht.is_available(S, TEST_MODEL));
    // Even forcing the (absent) cooldown into the past does not re-enable it.
    expire_cooldown(&ht, S, TEST_MODEL);
    assert!(
        !ht.is_available(S, TEST_MODEL),
        "hard-disable never self-heals"
    );
}

#[test]
fn stall_trips_immediately_without_consecutive_threshold() {
    let ht = HealthTracker::new();
    // A single stall (well below CIRCUIT_BREAKER_THRESHOLD) disables the model.
    ht.record_stall(S, TEST_MODEL, true);
    assert!(!ht.is_available(S, TEST_MODEL));
    let h = ht.model_health(S, TEST_MODEL);
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
    ht.record_stall(S, TEST_MODEL, true);
    let remaining = ht
        .model_health(S, TEST_MODEL)
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
    ht.record_stall(S, TEST_MODEL, true);
    assert!(!ht.is_available(S, TEST_MODEL));

    // Cooldown expires → model auto-re-enables (one-off stall self-heals).
    expire_cooldown(&ht, S, TEST_MODEL);
    assert!(ht.is_available(S, TEST_MODEL));
    assert!(!ht.model_health(S, TEST_MODEL).auto_disabled);

    // A successful turn resets the consecutive-failure counter.
    ht.record_success(S, TEST_MODEL);
    let h = ht.model_health(S, TEST_MODEL);
    assert_eq!(h.consecutive_failures, 0);
    assert_eq!(h.total_successes, 1);
    assert!(ht.is_available(S, TEST_MODEL));
}

#[test]
fn repeated_stalls_escalate_trips_and_stay_floored() {
    let ht = HealthTracker::new();
    for expected_trip in 1..=4 {
        ht.record_stall(S, TEST_MODEL, true);
        let h = ht.model_health(S, TEST_MODEL);
        assert_eq!(h.disable_ttl_trips, expected_trip);
        assert_eq!(h.trips_in_window, expected_trip);
        assert!(!h.hard_disabled, "4 trips is below the ceiling");
        assert!(h.auto_disabled);
        // The early rungs (5s/15s/45s/135s) are all below the stall floor, so
        // every stall cooldown is floored at STALL_MIN_COOLDOWN (5 min) — no
        // longer aliased to the multi-hour MAX_COOLDOWN.
        let remaining = h.cooldown_seconds_remaining.unwrap();
        assert!(remaining <= STALL_MIN_COOLDOWN.as_secs());
        assert!(remaining >= STALL_MIN_COOLDOWN.as_secs().saturating_sub(1));
        assert!(!ht.is_available(S, TEST_MODEL));
        expire_cooldown(&ht, S, TEST_MODEL);
    }
}

#[test]
fn throttle_stalls_trip_for_failover_without_escalating_the_cap() {
    // Idea 6: a throttle-classified stall (`escalate = false`) — the
    // Codex/OpenAI empty-200 account-quota signal — must still trip the
    // breaker (so dispatch fails over) AND floor the cooldown at the cap (so
    // the model is unavailable past the task redispatch ladder), but it must
    // NOT advance `disable_ttl_trips`: a quota resets on a clock, not on model
    // health, so ratcheting the escalating cooldown cap would be wrong.
    let ht = HealthTracker::new();
    for _ in 1..=4 {
        ht.record_stall(S, TEST_MODEL, false);
        let h = ht.model_health(S, TEST_MODEL);
        // Tripped + floored for failover, exactly like a genuine stall.
        assert!(h.auto_disabled);
        assert!(!ht.is_available(S, TEST_MODEL));
        let remaining = h.cooldown_seconds_remaining.unwrap();
        assert!(remaining <= STALL_MIN_COOLDOWN.as_secs());
        assert!(remaining >= STALL_MIN_COOLDOWN.as_secs().saturating_sub(1));
        // …but the escalation cap counter never advances.
        assert_eq!(
            h.disable_ttl_trips, 0,
            "a throttle stall must not escalate disable_ttl_trips",
        );
        expire_cooldown(&ht, S, TEST_MODEL);
    }
}

#[test]
fn stall_while_cooling_down_does_not_shorten_cooldown() {
    let ht = HealthTracker::new();
    ht.record_stall(S, TEST_MODEL, true);
    let first = ht.model_health(S, TEST_MODEL);
    assert_eq!(first.disable_ttl_trips, 1);

    // A second stall arriving while still cooling down is a no-op for the
    // cooldown window (no re-trip, no reset) — it only bumps failure counts.
    ht.record_stall(S, TEST_MODEL, true);
    let second = ht.model_health(S, TEST_MODEL);
    assert_eq!(second.disable_ttl_trips, 1, "no re-trip while cooling down");
    assert_eq!(second.consecutive_failures, 2);
    assert!(!ht.is_available(S, TEST_MODEL));
}

#[test]
fn scope_isolates_breaker_state_between_users() {
    // The core of the per-user fix: user A stalling a model must NOT disable
    // it for user B, nor for the shared/system bucket.
    let ht = HealthTracker::new();
    let a = Some("user-a");
    let b = Some("user-b");

    ht.record_stall(a, TEST_MODEL, true);

    assert!(!ht.is_available(a, TEST_MODEL), "A's bucket is disabled");
    assert!(ht.is_available(b, TEST_MODEL), "B's bucket is untouched");
    assert!(
        ht.is_available(None, TEST_MODEL),
        "shared/system bucket is untouched"
    );

    // B failing independently does not affect A's already-tripped state.
    ht.record_failure(b, TEST_MODEL);
    assert_eq!(ht.model_health(a, TEST_MODEL).disable_ttl_trips, 1);
    assert_eq!(ht.model_health(b, TEST_MODEL).consecutive_failures, 1);
}

#[test]
fn enable_and_reset_all_scopes_hit_every_bucket() {
    let ht = HealthTracker::new();
    ht.record_stall(Some("user-a"), TEST_MODEL, true);
    ht.record_stall(Some("user-b"), TEST_MODEL, true);
    ht.record_stall(None, TEST_MODEL, true);
    // A different model must be left alone.
    ht.record_stall(Some("user-a"), "other-model", true);

    let re_enabled = ht.enable_model_all_scopes(TEST_MODEL);
    assert_eq!(re_enabled, 3);
    assert!(ht.is_available(Some("user-a"), TEST_MODEL));
    assert!(ht.is_available(Some("user-b"), TEST_MODEL));
    assert!(ht.is_available(None, TEST_MODEL));
    assert!(!ht.is_available(Some("user-a"), "other-model"));

    // enable keeps counters; reset wipes them.
    assert_eq!(
        ht.model_health(Some("user-a"), TEST_MODEL).total_failures,
        1
    );
    let wiped = ht.reset_model_all_scopes(TEST_MODEL);
    assert_eq!(wiped, 3);
    assert_eq!(
        ht.model_health(Some("user-a"), TEST_MODEL).total_failures,
        0
    );
}

#[test]
fn restore_all_rehydrates_disabled_state_without_new_trip() {
    let ht = HealthTracker::new();
    ht.restore_all(vec![ModelHealth {
        model_id: "a/model".to_string(),
        scope: Some("user-a".to_string()),
        auto_disabled: true,
        consecutive_failures: CIRCUIT_BREAKER_THRESHOLD,
        breaker_eligible_consecutive_failures: CIRCUIT_BREAKER_THRESHOLD,
        total_failures: 10,
        total_successes: 2,
        disable_ttl_trips: 1,
        cooldown_seconds_remaining: Some(4),
        hard_disabled: false,
        hard_disable_probe_tier: 0,
        hard_disable_probe_seconds_remaining: None,
        hard_disable_on_probation: false,
        trips_in_window: 1,
    }]);

    let scope = Some("user-a");
    let h = ht.model_health(scope, "a/model");
    assert!(h.auto_disabled);
    assert!(!ht.is_available(scope, "a/model"));
    assert_eq!(h.consecutive_failures, CIRCUIT_BREAKER_THRESHOLD);
    assert_eq!(h.total_failures, 10);
    assert_eq!(h.total_successes, 2);
    assert_eq!(h.disable_ttl_trips, 1);
    // The same model under a different scope is unaffected by the snapshot.
    assert!(ht.is_available(None, "a/model"));
    let remaining = h.cooldown_seconds_remaining.unwrap();
    assert!(remaining <= 4);
    assert!(remaining >= 3);

    ht.enable(scope, "a/model");
    let enabled = ht.model_health(scope, "a/model");
    assert!(ht.is_available(scope, "a/model"));
    assert!(!enabled.auto_disabled);
    // `enable` keeps the escalation tier (the operator vouched the model is
    // usable now, not that its history never happened)…
    assert_eq!(enabled.disable_ttl_trips, 1);
    // …but as of 2026-08-16 it DOES clear the failure streak. Leaving
    // `consecutive_failures` at the threshold meant the very next failure
    // re-tripped the breaker, so every `enable` had to be chased with a `reset`
    // to make it stick. See `HealthTracker::enable`.
    assert_eq!(enabled.consecutive_failures, 0);
    assert_eq!(enabled.breaker_eligible_consecutive_failures, 0);
    assert_eq!(
        enabled.total_failures, 10,
        "the lifetime audit trail still survives `enable` — that is what \
         distinguishes it from `reset`"
    );

    ht.record_success(scope, "a/model");
    let restored = ht.model_health(scope, "a/model");
    assert!(ht.is_available(scope, "a/model"));
    assert!(!restored.auto_disabled);
    // A success resets the escalation tier (Fix 1): next failure starts fresh.
    assert_eq!(restored.disable_ttl_trips, 0);
    assert_eq!(restored.consecutive_failures, 0);

    for failures in 1..CIRCUIT_BREAKER_THRESHOLD {
        ht.record_failure(scope, "a/model");
        let health = ht.model_health(scope, "a/model");
        assert!(ht.is_available(scope, "a/model"));
        assert!(!health.auto_disabled);
        assert_eq!(health.disable_ttl_trips, 0);
        assert_eq!(health.consecutive_failures, failures);
    }

    let before_expiry = ht.model_health(scope, "a/model");
    assert_eq!(before_expiry.disable_ttl_trips, 0);

    ht.restore_all(vec![ModelHealth {
        model_id: "a/model".to_string(),
        scope: Some("user-a".to_string()),
        auto_disabled: true,
        consecutive_failures: CIRCUIT_BREAKER_THRESHOLD,
        breaker_eligible_consecutive_failures: CIRCUIT_BREAKER_THRESHOLD,
        total_failures: 10,
        total_successes: 2,
        disable_ttl_trips: 1,
        cooldown_seconds_remaining: Some(2),
        hard_disabled: false,
        hard_disable_probe_tier: 0,
        hard_disable_probe_seconds_remaining: None,
        hard_disable_on_probation: false,
        trips_in_window: 1,
    }]);
    assert!(!ht.is_available(scope, "a/model"));
    assert_eq!(ht.model_health(scope, "a/model").disable_ttl_trips, 1);

    expire_cooldown(&ht, scope, "a/model");
    let cooled_off = ht.model_health(scope, "a/model");
    assert!(ht.is_available(scope, "a/model"));
    assert!(!cooled_off.auto_disabled);
    assert_eq!(cooled_off.disable_ttl_trips, 1);
    assert!(cooled_off.cooldown_seconds_remaining.is_none());
}

#[test]
fn task_failure_signal_is_read_once_and_cleared() {
    let ht = HealthTracker::new();
    // No signal recorded → nothing to take.
    assert_eq!(ht.take_task_provider_failure("task-1"), None);

    ht.note_task_provider_failure(
        "task-1",
        TaskFailureSignal {
            throttle: true,
            transient: false,
            retry_after_ms: Some(5 * 60 * 60 * 1000),
        },
    );
    // The side-channel is independent of the breaker buckets.
    assert!(ht.is_available(S, TEST_MODEL));

    let taken = ht
        .take_task_provider_failure("task-1")
        .expect("signal present");
    assert!(taken.throttle);
    assert_eq!(taken.retry_after_ms, Some(5 * 60 * 60 * 1000));

    // Read-and-clear: a second take returns nothing (no stale leak into a
    // later, unrelated redispatch decision).
    assert_eq!(ht.take_task_provider_failure("task-1"), None);
}

#[test]
fn task_failure_signal_latest_wins_and_is_per_task() {
    let ht = HealthTracker::new();
    ht.note_task_provider_failure(
        "task-a",
        TaskFailureSignal {
            throttle: false,
            transient: false,
            retry_after_ms: None,
        },
    );
    // Latest failure for a task overwrites the prior one.
    ht.note_task_provider_failure(
        "task-a",
        TaskFailureSignal {
            throttle: true,
            transient: false,
            retry_after_ms: Some(1_000),
        },
    );
    ht.note_task_provider_failure(
        "task-b",
        TaskFailureSignal {
            throttle: false,
            transient: false,
            retry_after_ms: None,
        },
    );

    let a = ht.take_task_provider_failure("task-a").unwrap();
    assert!(a.throttle);
    assert_eq!(a.retry_after_ms, Some(1_000));
    // task-b is untouched by reads/writes of task-a.
    let b = ht.take_task_provider_failure("task-b").unwrap();
    assert!(!b.throttle);
}

#[test]
fn breaker_metric_snapshot_covers_closed_half_open_and_open() {
    let ht = HealthTracker::new();
    ht.record_success(Some("user-closed"), "closed-model");
    ht.record_stall(Some("user-half"), "half-model", true);
    expire_cooldown(&ht, Some("user-half"), "half-model");
    ht.record_stall(None, "open-model", true);

    let snapshot = ht.breaker_metric_snapshot();
    assert_eq!(snapshot.len(), 3);
    assert_eq!(snapshot[0].scope, "shared");
    assert_eq!(snapshot[0].model, "open-model");
    assert_eq!(snapshot[0].state, BreakerState::Open);
    assert_eq!(snapshot[0].value, 1.0);
    assert_eq!(snapshot[1].scope, "user-closed");
    assert_eq!(snapshot[1].state, BreakerState::Closed);
    assert_eq!(snapshot[1].value, 0.0);
    assert_eq!(snapshot[2].scope, "user-half");
    assert_eq!(snapshot[2].state, BreakerState::HalfOpen);
    assert_eq!(snapshot[2].value, 0.5);
}

#[test]
fn breaker_trip_metric_increments_only_on_trip_transition() {
    djinn_telemetry::init().unwrap();
    let before = rendered_counter_value("djinn_breaker_trips_total");
    let ht = HealthTracker::new();

    for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
        ht.record_failure(Some("trip-metric-user"), "trip-metric-model");
    }
    ht.record_failure(Some("trip-metric-user"), "trip-metric-model");
    ht.record_stall(Some("trip-metric-user"), "trip-metric-model", true);

    assert_eq!(
        ht.model_health(Some("trip-metric-user"), "trip-metric-model")
            .disable_ttl_trips,
        1
    );
    assert!(
        rendered_counter_value("djinn_breaker_trips_total") - before >= 1.0,
        "the authoritative closed-to-open transition should increment the breaker-trip counter"
    );
}

fn rendered_counter_value(metric: &str) -> f64 {
    let rendered = djinn_telemetry::render().unwrap();
    rendered
        .lines()
        .find_map(|line| {
            let value = line.strip_prefix(metric)?.trim();
            value.parse::<f64>().ok()
        })
        .unwrap_or(0.0)
}

#[test]
fn legacy_snapshot_without_scope_loads_as_shared_bucket() {
    // Snapshots persisted before the per-scope change have no `scope` field;
    // serde defaults it to None → the shared bucket.
    let ht = HealthTracker::new();
    let legacy = serde_json::json!([{
        "model_id": "openai/gpt-5.5",
        "auto_disabled": true,
        "consecutive_failures": 3,
        "total_failures": 3,
        "total_successes": 0,
        "disable_ttl_trips": 1,
        "cooldown_seconds_remaining": 30
    }]);
    let snapshot: Vec<ModelHealth> = serde_json::from_value(legacy).unwrap();
    ht.restore_all(snapshot);
    assert!(!ht.is_available(None, "openai/gpt-5.5"));
    assert_eq!(ht.model_health(None, "openai/gpt-5.5").scope, None);
}

// ── Deferred-breaker observation tests ─────────────────────────────────────

#[test]
fn record_failure_observation_does_not_trip_breaker() {
    let ht = HealthTracker::new();
    // Record CIRCUIT_BREAKER_THRESHOLD failures using observation-only API.
    // The breaker must NOT be tripped — observations are deferred until
    // `apply_breaker_check_for` is called.
    for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
        ht.record_failure_observation(S, TEST_MODEL);
    }
    assert!(
        ht.is_available(S, TEST_MODEL),
        "observation-only recording must NOT trip the breaker"
    );
    let health = ht.model_health(S, TEST_MODEL);
    assert!(
        !health.auto_disabled,
        "auto_disabled must remain false after observation-only recording"
    );
    assert_eq!(
        health.consecutive_failures, CIRCUIT_BREAKER_THRESHOLD,
        "consecutive_failures must be incremented by observation"
    );
    assert_eq!(
        health.total_failures, CIRCUIT_BREAKER_THRESHOLD,
        "total_failures must be incremented by observation"
    );
}

#[test]
fn apply_breaker_check_for_trips_when_eligible_threshold_reached() {
    let ht = HealthTracker::new();

    for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
        ht.record_failure_observation(S, TEST_MODEL);
        ht.apply_breaker_check_for(S, TEST_MODEL);
    }

    assert!(
        !ht.is_available(S, TEST_MODEL),
        "breaker must trip only after the breaker-eligible exhausted-chain threshold"
    );
    let health = ht.model_health(S, TEST_MODEL);
    assert!(health.auto_disabled);
    assert_eq!(health.disable_ttl_trips, 1);
}

#[test]
fn apply_breaker_check_for_does_not_trip_below_eligible_threshold() {
    let ht = HealthTracker::new();
    for _ in 0..CIRCUIT_BREAKER_THRESHOLD - 1 {
        ht.record_failure_observation(S, TEST_MODEL);
        ht.apply_breaker_check_for(S, TEST_MODEL);
    }
    assert!(
        ht.is_available(S, TEST_MODEL),
        "breaker must NOT trip below the breaker-eligible exhausted-chain threshold"
    );
    let health = ht.model_health(S, TEST_MODEL);
    assert!(!health.auto_disabled);
}

#[test]
fn record_failure_observation_increments_counters_without_buffering() {
    let ht = HealthTracker::new();
    ht.record_failure_observation(Some("user-a"), "model-a");
    ht.record_failure_observation(Some("user-b"), "model-b");
    // Successive calls increment counters idempotently; the tracker no
    // longer exposes a global observation buffer (chain-scoped observations
    // are owned by the dispatch caller, see `try_dispatch_to_pool`).
    let health_a = ht.model_health(Some("user-a"), "model-a");
    let health_b = ht.model_health(Some("user-b"), "model-b");
    assert_eq!(health_a.consecutive_failures, 1);
    assert_eq!(health_a.total_failures, 1);
    assert_eq!(health_b.consecutive_failures, 1);
    assert_eq!(health_b.total_failures, 1);
    // Neither bucket should have tripped — recording an observation never
    // trips the breaker; that only happens via `apply_breaker_check_for`.
    assert!(ht.is_available(Some("user-a"), "model-a"));
    assert!(ht.is_available(Some("user-b"), "model-b"));
}

#[test]
fn breaker_trips_on_chain_exhaustion_after_deferred_observations() {
    let ht = HealthTracker::new();
    // Simulate 3 chain exhaustions, each recording 1 observation per model
    // and then evaluating the breaker check.  Because `record_failure_observation`
    // is now chain-agnostic, the caller (i.e. `CoordinatorActor::apply_chain_exhaustion_side_effects`)
    // tracks chain-scoped observations itself and passes them explicitly
    // to `apply_breaker_check_for`.
    for _ in 0..3 {
        ht.record_failure_observation(S, "model-a");
        ht.record_failure_observation(S, "model-b");
        let observed: Vec<_> = [HealthKey::new(S, "model-a"), HealthKey::new(S, "model-b")]
            .into_iter()
            .collect();
        // Simulate chain exhaustion: apply breaker for the chain's
        // observations explicitly.
        for key in &observed {
            ht.apply_breaker_check_for(key.scope.as_deref(), &key.model_id);
        }
    }
    assert!(
        !ht.is_available(S, "model-a"),
        "model-a breaker must trip after 3 chain exhaustions"
    );
    assert!(
        !ht.is_available(S, "model-b"),
        "model-b breaker must trip after 3 chain exhaustions"
    );
}

#[test]
fn fallback_rescued_observations_never_count_toward_later_breaker_trip() {
    let ht = HealthTracker::new();

    // Reviewer repro: two model-a failures rescued by successful fallback leave
    // diagnostic consecutive_failures == 2, but no breaker-eligible failures.
    ht.record_failure_observation(S, "model-a");
    ht.record_failure_observation(S, "model-a");
    let diagnostic = ht.model_health(S, "model-a");
    assert_eq!(diagnostic.consecutive_failures, 2);
    assert!(ht.is_available(S, "model-a"));

    // A later exhausted chain containing model-a records one more diagnostic
    // observation and exactly one breaker-eligible exhausted-chain failure.
    ht.record_failure_observation(S, "model-a");
    ht.apply_breaker_check_for(S, "model-a");

    let after_one_exhaustion = ht.model_health(S, "model-a");
    assert_eq!(after_one_exhaustion.consecutive_failures, 3);
    assert!(
        ht.is_available(S, "model-a"),
        "three diagnostic observations must not trip the breaker when only one \
         breaker-eligible exhausted-chain failure occurred"
    );

    // Only repeated exhausted-chain failures reaching the configured threshold
    // trip the breaker.
    for _ in 1..CIRCUIT_BREAKER_THRESHOLD {
        ht.record_failure_observation(S, "model-a");
        ht.apply_breaker_check_for(S, "model-a");
    }
    assert!(
        !ht.is_available(S, "model-a"),
        "breaker trips after the eligible exhausted-chain threshold is reached"
    );
}

#[test]
fn apply_breaker_check_for_respects_expired_cooldown() {
    let ht = HealthTracker::new();
    // Trip the breaker manually via `record_failure`.
    for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
        ht.record_failure(S, TEST_MODEL);
    }
    assert!(!ht.is_available(S, TEST_MODEL));

    // Expire the cooldown.
    expire_cooldown(&ht, S, TEST_MODEL);
    assert!(ht.is_available(S, TEST_MODEL), "after cooldown expiry");

    // New observation + breaker check should re-trip.
    ht.record_failure_observation(S, TEST_MODEL);
    ht.apply_breaker_check_for(S, TEST_MODEL);
    assert!(
        !ht.is_available(S, TEST_MODEL),
        "breaker must re-trip after cooldown expiry + new observation"
    );
}

// ── Transient upstream faults are a LOAD signal, not a health signal ────────

/// Incident 2026-07-29 (task `nr41`): a burst of OpenAI `server_is_overloaded`
/// 500s drove `openai/gpt-5.6-sol` to `auto_disabled: true` — 15 consecutive
/// failures, 6 disable-TTL trips, 30 total failures against 6 successes —
/// disabling the tribunal's own adversary model for an outage that was not the
/// model's doing.
///
/// A run of transient upstream faults well past the ordinary three-strike
/// threshold must therefore leave the model available.
#[test]
fn repeated_transient_upstream_faults_do_not_auto_disable_a_model() {
    let ht = HealthTracker::new();

    for _ in 0..(CIRCUIT_BREAKER_THRESHOLD * 4) {
        ht.record_transient_failure(S, TEST_MODEL);
    }

    let health = ht.model_health(S, TEST_MODEL);
    assert!(
        !health.auto_disabled,
        "an overloaded upstream must not demote the model: {health:?}"
    );
    assert!(ht.is_available(S, TEST_MODEL));
    assert_eq!(
        health.disable_ttl_trips, 0,
        "no trips means no escalating-cooldown ratchet either"
    );
    // The faults are still fully visible — this is a re-attribution, not a
    // suppression. An operator reading model_health sees every one of them.
    assert_eq!(health.consecutive_failures, CIRCUIT_BREAKER_THRESHOLD * 4);
    assert_eq!(health.total_failures, CIRCUIT_BREAKER_THRESHOLD * 4);
    assert_eq!(
        health.breaker_eligible_consecutive_failures, 0,
        "transient faults must never become breaker-eligible"
    );
}

/// The counterpart guard: genuine typed provider failures (auth, invalid
/// request, unparseable output — everything that routes through
/// `record_failure`) must still trip at the ordinary threshold. If this ever
/// fails, the fix above has been over-applied and real breakage stops demoting
/// models.
#[test]
fn repeated_genuine_failures_still_auto_disable_a_model() {
    let ht = HealthTracker::new();

    for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
        ht.record_failure(S, TEST_MODEL);
    }

    let health = ht.model_health(S, TEST_MODEL);
    assert!(
        health.auto_disabled,
        "genuine failures must still trip the breaker at the ordinary threshold: {health:?}"
    );
    assert!(!ht.is_available(S, TEST_MODEL));
}

/// The transient ladder is longer, not absent. A backend that is permanently
/// gone (the kimi-for-coding/`k2p7` signature: instant transport death, zero
/// tokens, re-dispatched forever) must still be demoted eventually rather than
/// re-selected indefinitely.
#[test]
fn a_sustained_run_of_transient_faults_eventually_trips_the_breaker() {
    let ht = HealthTracker::new();

    for _ in 0..(TRANSIENT_BREAKER_THRESHOLD - 1) {
        ht.record_transient_failure(S, TEST_MODEL);
    }
    assert!(
        ht.is_available(S, TEST_MODEL),
        "one short of the transient threshold is still available"
    );

    ht.record_transient_failure(S, TEST_MODEL);
    assert!(
        !ht.is_available(S, TEST_MODEL),
        "a dead endpoint still demotes — twenty strikes in, not three"
    );
}

/// A success proves the provider recovered, so the transient streak resets and
/// the next outage starts counting from zero rather than inheriting the last
/// one's progress toward the threshold.
#[test]
fn a_success_resets_the_transient_streak() {
    let ht = HealthTracker::new();

    for _ in 0..(TRANSIENT_BREAKER_THRESHOLD - 1) {
        ht.record_transient_failure(S, TEST_MODEL);
    }
    ht.record_success(S, TEST_MODEL);
    for _ in 0..(TRANSIENT_BREAKER_THRESHOLD - 1) {
        ht.record_transient_failure(S, TEST_MODEL);
    }

    assert!(
        ht.is_available(S, TEST_MODEL),
        "the streak restarted after the success, so the threshold is not reached"
    );
}

/// The per-task side-channel has three readers with different ownership.
/// `peek` must be non-destructive so the session-exit classifier and the
/// refinement loop cannot steal the signal the dispatch-reappearance path
/// consumes for its A3/A6 backoff.
#[test]
fn peek_task_provider_failure_does_not_consume_the_signal() {
    let ht = HealthTracker::new();
    let signal = TaskFailureSignal {
        throttle: false,
        transient: true,
        retry_after_ms: None,
    };
    ht.note_task_provider_failure("task-1", signal);

    assert_eq!(ht.peek_task_provider_failure("task-1"), Some(signal));
    assert_eq!(
        ht.peek_task_provider_failure("task-1"),
        Some(signal),
        "peek must be repeatable"
    );
    assert_eq!(
        ht.take_task_provider_failure("task-1"),
        Some(signal),
        "the owning reader still receives it"
    );
    assert_eq!(ht.peek_task_provider_failure("task-1"), None);
}

#[test]
fn clear_task_provider_failure_drops_the_signal() {
    let ht = HealthTracker::new();
    ht.note_task_provider_failure(
        "task-2",
        TaskFailureSignal {
            throttle: false,
            transient: true,
            retry_after_ms: None,
        },
    );
    ht.clear_task_provider_failure("task-2");
    assert_eq!(ht.peek_task_provider_failure("task-2"), None);
}

// ---------------------------------------------------------------------------
// Half-open recovery from a hard-disable (quarantine + single probe).
//
// Incident being fixed: 2026-08-12 → 2026-08-16. `openai/gpt-5.6-terra` — the
// only candidate in a user's `implement` lane — hit the 8-trips/6h ceiling
// during a transient provider incident and latched `hard_disabled: true`. Four
// days later it still read `hard_disabled: true` with `trips_in_window: 0`:
// every trip that justified the latch had aged out of the rolling window and
// nothing ever reconsidered, so every dispatch went `breaker_open` →
// `failover_chain_exhausted` → cooldown. The autonomous build loop was dead the
// whole time; a manual reset produced 10/10 successes immediately.
//
// The counter-pressure these tests must also hold: the permanence was NOT
// gratuitous. It exists to stop the flap where a hopeless model is re-enabled
// on a clock, grabs a priority slot, crashes in <10s and repeats — one
// production task lost 5.75h to 8 consecutive 429 crashes. So every test below
// that proves recovery is paired with one that bounds exposure.
// ---------------------------------------------------------------------------

use djinn_core::clock::TestClock;
use std::time::SystemTime;

fn test_clock() -> Arc<TestClock> {
    Arc::new(TestClock::new(SystemTime::UNIX_EPOCH, Instant::now()))
}

fn tracker_on(clock: &Arc<TestClock>) -> HealthTracker {
    HealthTracker::with_clock(clock.clone() as Arc<dyn Clock>)
}

/// Drive a bucket to the trip-rate ceiling on the *test* clock, leaving `now`
/// exactly at the instant of the latching trip (so quarantine deadlines can be
/// reasoned about relative to it).
///
/// Each trip needs three breaker-eligible failures, and the escalating cooldown
/// between trips must expire before the next trip can register. The cooldowns
/// sum to ~1.5h, comfortably inside `TRIP_RATE_WINDOW`, so all eight trips land
/// in one window — which is what the ceiling requires.
fn hard_disable_via_trip_rate(ht: &HealthTracker, clock: &TestClock) {
    for trip in 0..TRIP_RATE_CEILING {
        if trip > 0 {
            let remaining = ht
                .model_health(S, TEST_MODEL)
                .cooldown_seconds_remaining
                .unwrap_or(0);
            clock.advance_mono(Duration::from_secs(remaining + 1));
        }
        for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
            ht.record_failure(S, TEST_MODEL);
        }
    }
    let health = ht.model_health(S, TEST_MODEL);
    assert!(
        health.hard_disabled,
        "fixture precondition: {TRIP_RATE_CEILING} trips inside the window must hard-disable"
    );
    assert_eq!(health.trips_in_window, TRIP_RATE_CEILING as u32);
}

#[test]
fn quarantine_admits_exactly_one_probe_and_a_clean_probe_recovers_the_model() {
    // THE INCIDENT, REPRODUCED. This is the test that fails against the
    // pre-fix `is_available` (`if self.hard_disabled { return false; }`).
    let clock = test_clock();
    let ht = tracker_on(&clock);
    hard_disable_via_trip_rate(&ht, &clock);

    // `now` is the latching trip. One second before the quarantine deadline the
    // bucket is unavailable AND the latching trip is still inside the rolling
    // window — the latch is still justified by live evidence.
    clock.advance_mono(HARD_DISABLE_QUARANTINE_BASE - Duration::from_secs(1));
    let health = ht.model_health(S, TEST_MODEL);
    assert_eq!(
        health.trips_in_window, 1,
        "one second before the deadline the latching trip has not yet aged out"
    );
    assert!(
        !ht.is_available(S, TEST_MODEL),
        "no probe is admitted before the quarantine deadline"
    );

    // Exactly at the deadline the rolling window is provably empty — this is
    // the `trips_in_window: 0, hard_disabled: true` state the incident bucket
    // sat in for four days — and exactly one probe is admitted.
    clock.advance_mono(Duration::from_secs(1));
    let health = ht.model_health(S, TEST_MODEL);
    assert_eq!(
        health.trips_in_window, 0,
        "the base quarantine equals TRIP_RATE_WINDOW precisely so that every trip \
         justifying the latch has aged out by the time the probe fires"
    );
    assert!(
        health.hard_disabled,
        "the probe is not a re-enable: the bucket is still latched"
    );
    assert!(
        ht.is_available(S, TEST_MODEL),
        "REGRESSION GUARD (4-day outage): a decayed hard-disable must admit a probe"
    );

    // Consuming the probe closes the window again immediately — the lane does
    // not re-open, one task is exposed.
    ht.note_dispatch_accepted(S, TEST_MODEL);
    assert!(
        !ht.is_available(S, TEST_MODEL),
        "the probe is consumed at dispatch acceptance; the lane does not re-open"
    );

    // The probe succeeds → the lane reopens at full throughput, but onto
    // PROBATION, not a clean slate.
    ht.record_success(S, TEST_MODEL);
    let health = ht.model_health(S, TEST_MODEL);
    assert!(
        !health.hard_disabled,
        "a clean probe releases the quarantine"
    );
    assert!(!health.auto_disabled);
    assert!(
        health.hard_disable_on_probation,
        "one session is enough to reopen the lane, not enough to forgive eight trips"
    );
    assert_eq!(health.trips_in_window, 0);
    assert_eq!(health.disable_ttl_trips, 0);
    assert!(ht.is_available(S, TEST_MODEL));
    assert_eq!(
        health.hard_disable_probe_seconds_remaining, None,
        "not quarantined → no next-probe deadline is reported"
    );

    // Four more clean sessions (five in total) forgive the history.
    for _ in 1..HARD_DISABLE_PROBATION_SUCCESSES {
        assert!(
            ht.model_health(S, TEST_MODEL).hard_disable_on_probation,
            "probation is not cleared early"
        );
        ht.record_success(S, TEST_MODEL);
    }
    let health = ht.model_health(S, TEST_MODEL);
    assert!(
        !health.hard_disable_on_probation,
        "a model that goes {HARD_DISABLE_PROBATION_SUCCESSES}-for-\
         {HARD_DISABLE_PROBATION_SUCCESSES} has re-proven itself"
    );
    assert_eq!(health.hard_disable_probe_tier, 0);
    assert!(ht.is_available(S, TEST_MODEL));
}

/// Simulate a dispatch loop polling the breaker every minute for `days`,
/// treating every admitted dispatch as a probe that crashes. Returns the
/// elapsed-minute stamp of each admitted dispatch.
fn simulate_dead_model_dispatch_loop(ht: &HealthTracker, clock: &TestClock, days: u64) -> Vec<u64> {
    let mut admitted = Vec::new();
    for minute in 0..(days * 24 * 60) {
        if ht.is_available(S, TEST_MODEL) {
            admitted.push(minute);
            // Exactly the 2026-07 flap shape: the dispatch is accepted, the
            // session grabs a slot and dies almost immediately.
            ht.note_dispatch_accepted(S, TEST_MODEL);
            ht.record_stall(S, TEST_MODEL, true);
        }
        clock.advance_mono(Duration::from_secs(60));
    }
    admitted
}

#[test]
fn a_failing_probe_relatches_and_dispatch_exposure_stays_bounded_for_thirty_days() {
    // THE FLAP MUST STAY IMPOSSIBLE. This is the assertion that protects the
    // 5.75h/8-crash incident from recurring: over a month of continuous polling
    // against a model that fails every single probe, the *count* of tasks
    // actually exposed is single digits, and no two exposures are close
    // together.
    let clock = test_clock();
    let ht = tracker_on(&clock);
    hard_disable_via_trip_rate(&ht, &clock);

    let admitted = simulate_dead_model_dispatch_loop(&ht, &clock, 30);

    assert!(
        admitted.len() <= 8,
        "30 days of polling a permanently-broken model exposed {} tasks; the \
         quarantine ladder must keep this in single digits (the flap incident \
         burned 8 tasks in 5.75 HOURS)",
        admitted.len()
    );
    assert!(
        !admitted.is_empty(),
        "…but it must not be a fuse either: a breaker that can never probe is \
         what caused the 4-day outage"
    );

    // No exposure inside the first 5.75h — the exact wall-clock the flap
    // incident burned through 8 crashes.
    let flap_incident_minutes = 345;
    assert!(
        admitted.iter().all(|m| *m >= flap_incident_minutes),
        "a task was exposed inside the 5.75h window the original flap consumed: {admitted:?}"
    );

    // Every gap between consecutive exposures is at least the base quarantine.
    for pair in admitted.windows(2) {
        let gap_minutes = pair[1] - pair[0];
        assert!(
            gap_minutes >= HARD_DISABLE_QUARANTINE_BASE.as_secs() / 60,
            "consecutive probes {} and {} are only {gap_minutes} minutes apart",
            pair[0],
            pair[1]
        );
    }

    // And the model is still effectively disabled at the end of the month.
    let health = ht.model_health(S, TEST_MODEL);
    assert!(
        health.hard_disabled,
        "a model that fails every probe stays latched"
    );
    assert!(!ht.is_available(S, TEST_MODEL));
}

/// Simulate a dispatch loop polling the breaker every minute for `days` against
/// a model that succeeds one session in `success_every` and fails the rest.
/// Returns the elapsed-minute stamp of each admitted dispatch.
///
/// This is the input class both production incidents actually belong to. A model
/// that fails 100% of the time reaches the quarantine ladder trivially; the
/// dangerous one is the model that wins occasionally, because every win is an
/// opportunity to launder its history.
fn simulate_intermittent_model_dispatch_loop(
    ht: &HealthTracker,
    clock: &TestClock,
    days: u64,
    success_every: u64,
) -> Vec<u64> {
    let mut admitted = Vec::new();
    let mut session: u64 = 0;
    for minute in 0..(days * 24 * 60) {
        if ht.is_available(S, TEST_MODEL) {
            admitted.push(minute);
            ht.note_dispatch_accepted(S, TEST_MODEL);
            if session.is_multiple_of(success_every) {
                ht.record_success(S, TEST_MODEL);
            } else {
                ht.record_failure(S, TEST_MODEL);
            }
            session += 1;
        }
        clock.advance_mono(Duration::from_secs(60));
    }
    admitted
}

#[test]
fn an_intermittently_succeeding_model_cannot_launder_its_quarantine_history() {
    // D1 — THE REGRESSION THAT MATTERS MOST. A probe success releases the
    // quarantine, and the first version of this change also cleared the trip
    // window, the escalating-cooldown tier and the quarantine tier along with
    // it, leaving the bucket byte-identical to a never-tripped model. Measured
    // against the `xiaomi-token-plan-sgp/mimo-v2.5-pro` profile (78 failures /
    // 16 successes ≈ one success in six) that produced ONE lucky probe followed
    // by 4712 exposed sessions in 30 days and no re-latch — strictly worse than
    // the permanent latch it replaced.
    //
    // The fix is probation: the release preserves the tier, and a single trip
    // re-quarantines one rung higher until the model produces
    // `HARD_DISABLE_PROBATION_SUCCESSES` clean sessions in a row.
    let clock = test_clock();
    let ht = tracker_on(&clock);
    hard_disable_via_trip_rate(&ht, &clock);

    let admitted = simulate_intermittent_model_dispatch_loop(&ht, &clock, 30, 6);

    // Measured: 17 sessions, at minutes
    // [360,361,362,363, 1083, 2523, 5403,5404,5405,5406, 11166, 21246,
    //  31326,31327,31328,31329, 41409].
    // The shape is the design working: a probe that wins reopens the lane onto
    // probation, three more failures re-trip it, and it goes straight back in
    // one rung higher (6h → 12h → 24h → 48h → 96h → 7d). Before the probation
    // gate the same input produced 4712 sessions and never re-latched; on
    // `origin/main` it produces 0 and never recovers.
    assert!(
        admitted.len() <= 24,
        "30 days against a one-success-in-six model exposed {} sessions (17 when \
         this was written). Each released quarantine costs one probe plus the \
         CIRCUIT_BREAKER_THRESHOLD failures it takes to re-trip, across at most \
         8 quarantine cycles — far above that means a lucky probe is laundering \
         the bucket's history: {admitted:?}",
        admitted.len()
    );
    assert!(
        !admitted.is_empty(),
        "…and it still probes: this must not become a fuse again"
    );

    let health = ht.model_health(S, TEST_MODEL);
    assert!(
        health.hard_disabled,
        "after 30 days a one-success-in-six model must end up quarantined, not \
         running free ({health:?})"
    );
    assert!(
        health.hard_disable_probe_tier >= 1,
        "each re-quarantine resumes the ladder one rung higher rather than \
         restarting at the 6h base; tier is {}",
        health.hard_disable_probe_tier
    );

    // The probation gate is what does this. Prove it directly: a released
    // bucket that never manages a clean streak stays one trip from quarantine.
    let clock = test_clock();
    let ht = tracker_on(&clock);
    hard_disable_via_trip_rate(&ht, &clock);
    clock.advance_mono(HARD_DISABLE_QUARANTINE_BASE);
    ht.note_dispatch_accepted(S, TEST_MODEL);
    ht.record_success(S, TEST_MODEL);
    assert!(ht.model_health(S, TEST_MODEL).hard_disable_on_probation);
    // One trip — CIRCUIT_BREAKER_THRESHOLD failures — and it is back in.
    for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
        ht.record_failure(S, TEST_MODEL);
    }
    let health = ht.model_health(S, TEST_MODEL);
    assert!(
        health.hard_disabled,
        "ONE trip re-quarantines a bucket on probation; it does not get eight"
    );
    assert_eq!(
        health.hard_disable_probe_tier, 1,
        "and it resumes the ladder one rung higher than the quarantine it left"
    );
    assert!(!health.hard_disable_on_probation);
    // …at the longer quarantine, measured.
    clock.advance_mono(quarantine_for_tier(1) - Duration::from_secs(1));
    assert!(!ht.is_available(S, TEST_MODEL));
    clock.advance_mono(Duration::from_secs(1));
    assert!(ht.is_available(S, TEST_MODEL));
}

#[test]
fn a_success_without_an_admitted_probe_cannot_release_a_quarantine() {
    // D2. A session already in flight when the latch was set completes
    // successfully afterwards. That says nothing about whether the quarantine
    // was right — for a flapping model such successes are routine — so it must
    // not release the latch, and must not wipe the history that justified it.
    // Precondition in production is simply `max_sessions > 1` on the same
    // `(scope, model)`.
    let clock = test_clock();
    let ht = tracker_on(&clock);
    hard_disable_via_trip_rate(&ht, &clock);
    let latched = ht.model_health(S, TEST_MODEL);

    clock.advance_mono(Duration::from_secs(60));
    ht.record_success(S, TEST_MODEL);

    let health = ht.model_health(S, TEST_MODEL);
    assert!(
        health.hard_disabled,
        "only an ADMITTED PROBE's success may release a quarantine"
    );
    assert!(!health.hard_disable_on_probation);
    assert!(!ht.is_available(S, TEST_MODEL));
    assert_eq!(
        health.trips_in_window, latched.trips_in_window,
        "a stray success must not wipe the rolling trip window"
    );
    assert_eq!(
        health.disable_ttl_trips, latched.disable_ttl_trips,
        "…nor the escalating-cooldown ladder"
    );
    assert_eq!(health.hard_disable_probe_tier, 0);
    assert_eq!(
        health.total_successes, 1,
        "the success is still counted for diagnostics"
    );

    // The quarantine is neither shortened nor extended by it: the probe still
    // arrives exactly one base quarantine after the latching trip.
    clock.advance_mono(HARD_DISABLE_QUARANTINE_BASE - Duration::from_secs(61));
    assert!(!ht.is_available(S, TEST_MODEL));
    clock.advance_mono(Duration::from_secs(1));
    assert!(ht.is_available(S, TEST_MODEL));
}

#[test]
fn health_blind_failures_cannot_postpone_the_probe_forever() {
    // D3. Not every dispatch path consults the breaker — evidence-dispatch
    // recovery reaches the pool through `resolve_dispatch_models_for_role`,
    // which never reads the HealthTracker. If any failure record re-anchored the
    // quarantine, such a path failing every few hours would postpone the probe
    // indefinitely: a silent permanent outage, with the tier pinned at 0 so
    // nothing in the health output would show it. Measured before the fix: one
    // failure record every 5h ⇒ ZERO probes in 30 simulated days.
    //
    // The assertion is a comparison, not a threshold: the noisy run must admit
    // exactly the same probes as a quiet one.
    let quiet_clock = test_clock();
    let quiet = tracker_on(&quiet_clock);
    hard_disable_via_trip_rate(&quiet, &quiet_clock);
    let baseline = simulate_dead_model_dispatch_loop(&quiet, &quiet_clock, 30);

    let clock = test_clock();
    let ht = tracker_on(&clock);
    hard_disable_via_trip_rate(&ht, &clock);
    let mut admitted = Vec::new();
    let noise_period_minutes = 5 * 60;
    for minute in 0..(30 * 24 * 60u64) {
        if ht.is_available(S, TEST_MODEL) {
            admitted.push(minute);
            ht.note_dispatch_accepted(S, TEST_MODEL);
            ht.record_stall(S, TEST_MODEL, true);
        }
        if minute.is_multiple_of(noise_period_minutes) {
            // A health-blind dispatch path failing against the quarantined
            // bucket, with no probe outstanding.
            ht.record_failure(S, TEST_MODEL);
        }
        clock.advance_mono(Duration::from_secs(60));
    }

    assert!(
        !baseline.is_empty(),
        "fixture precondition: the quiet run must admit probes at all"
    );
    assert_eq!(
        admitted,
        baseline,
        "a failure with no probe outstanding must not move the quarantine \
         deadline; noisy run admitted {} probes vs {} for the quiet run",
        admitted.len(),
        baseline.len()
    );
}

#[test]
fn a_transient_upstream_fault_re_quarantines_without_escalating() {
    // D4. This crate's position is that a transient 5xx is a LOAD signal, not a
    // model-health signal — it trips at TRANSIENT_BREAKER_THRESHOLD (20) rather
    // than CIRCUIT_BREAKER_THRESHOLD (3) for exactly that reason, and the
    // 4-day outage's latch was itself minted during a transient provider
    // incident. So a probe that dies on a 5xx serves another quarantine period,
    // not a longer one.
    let clock = test_clock();
    let ht = tracker_on(&clock);
    hard_disable_via_trip_rate(&ht, &clock);
    clock.advance_mono(HARD_DISABLE_QUARANTINE_BASE);
    assert!(ht.is_available(S, TEST_MODEL));

    ht.note_dispatch_accepted(S, TEST_MODEL);
    ht.record_transient_failure(S, TEST_MODEL);
    let health = ht.model_health(S, TEST_MODEL);
    assert!(
        health.hard_disabled,
        "the probe still failed: stay quarantined"
    );
    assert_eq!(
        health.hard_disable_probe_tier, 0,
        "a transient upstream fault must not double the quarantine"
    );

    // Same tier ⇒ the next probe is one BASE away, not two.
    clock.advance_mono(HARD_DISABLE_QUARANTINE_BASE - Duration::from_secs(1));
    assert!(
        !ht.is_available(S, TEST_MODEL),
        "boundary: one second early"
    );
    clock.advance_mono(Duration::from_secs(1));
    assert!(ht.is_available(S, TEST_MODEL));

    // A genuine failure on the next probe DOES escalate, so the ladder is not
    // simply disabled.
    ht.note_dispatch_accepted(S, TEST_MODEL);
    ht.record_failure(S, TEST_MODEL);
    assert_eq!(ht.model_health(S, TEST_MODEL).hard_disable_probe_tier, 1);
}

#[test]
fn model_health_reports_the_next_probe_deadline_to_operators() {
    // D5. `model_health` is the surface that was actually used to diagnose the
    // outage, and `hard_disabled: true, trips_in_window: 0, cooldown: null` gave
    // an operator no way to distinguish a permanent state from a wait.
    let clock = test_clock();
    let ht = tracker_on(&clock);
    hard_disable_via_trip_rate(&ht, &clock);

    let health = ht.model_health(S, TEST_MODEL);
    assert_eq!(
        health.hard_disable_probe_seconds_remaining,
        Some(HARD_DISABLE_QUARANTINE_BASE.as_secs()),
        "a quarantined bucket must say when it will next re-examine itself"
    );
    assert!(health.cooldown_seconds_remaining.is_none());

    clock.advance_mono(HARD_DISABLE_QUARANTINE_BASE - Duration::from_secs(90));
    assert_eq!(
        ht.model_health(S, TEST_MODEL)
            .hard_disable_probe_seconds_remaining,
        Some(90)
    );

    clock.advance_mono(Duration::from_secs(90));
    assert_eq!(
        ht.model_health(S, TEST_MODEL)
            .hard_disable_probe_seconds_remaining,
        Some(0),
        "0 means a probe is admissible right now"
    );
    assert!(ht.is_available(S, TEST_MODEL));

    // A healthy bucket reports no deadline at all.
    ht.reset(S, TEST_MODEL);
    assert_eq!(
        ht.model_health(S, TEST_MODEL)
            .hard_disable_probe_seconds_remaining,
        None
    );
}

#[test]
fn probation_survives_a_restart_so_it_cannot_be_laundered_through_a_deploy() {
    let clock = test_clock();
    let ht = tracker_on(&clock);
    hard_disable_via_trip_rate(&ht, &clock);
    clock.advance_mono(quarantine_for_tier(0));
    ht.note_dispatch_accepted(S, TEST_MODEL);
    ht.record_failure(S, TEST_MODEL);
    clock.advance_mono(quarantine_for_tier(1));
    ht.note_dispatch_accepted(S, TEST_MODEL);
    ht.record_success(S, TEST_MODEL);

    let snapshot = ht.all_health();
    assert!(snapshot[0].hard_disable_on_probation);
    assert_eq!(snapshot[0].hard_disable_probe_tier, 1);

    let boot_clock = test_clock();
    let booted = tracker_on(&boot_clock);
    booted.restore_all(snapshot);
    let health = booted.model_health(S, TEST_MODEL);
    assert!(
        health.hard_disable_on_probation,
        "a restart must not forgive an unproven bucket"
    );
    assert_eq!(health.hard_disable_probe_tier, 1);
    assert!(
        booted.is_available(S, TEST_MODEL),
        "probation is still fully dispatchable"
    );

    // …and one trip still re-quarantines it, at tier 2.
    for _ in 0..CIRCUIT_BREAKER_THRESHOLD {
        booted.record_failure(S, TEST_MODEL);
    }
    let health = booted.model_health(S, TEST_MODEL);
    assert!(health.hard_disabled);
    assert_eq!(health.hard_disable_probe_tier, 2);
}

#[test]
fn repeated_probe_failures_escalate_the_quarantine_to_the_ceiling() {
    // GENUINELY-DEAD MODEL. The gap between successive probes must grow —
    // 6h, 12h, 24h, 48h, 96h — and then pin at the 7-day ceiling rather than
    // growing without bound (which would be a fuse again).
    let clock = test_clock();
    let ht = tracker_on(&clock);
    hard_disable_via_trip_rate(&ht, &clock);

    let admitted = simulate_dead_model_dispatch_loop(&ht, &clock, 30);
    let hour = 60u64;
    let gaps: Vec<u64> = std::iter::once(admitted[0])
        .chain(admitted.windows(2).map(|p| p[1] - p[0]))
        .collect();

    assert_eq!(
        gaps,
        vec![
            6 * hour,
            12 * hour,
            24 * hour,
            48 * hour,
            96 * hour,
            168 * hour,
            168 * hour,
            168 * hour,
        ],
        "quarantine must double per failed probe and then pin at the 7-day ceiling"
    );

    let health = ht.model_health(S, TEST_MODEL);
    assert_eq!(
        health.hard_disable_probe_tier, HARD_DISABLE_MAX_PROBE_TIER,
        "the escalation tier saturates instead of overflowing"
    );
    assert_eq!(
        quarantine_for_tier(health.hard_disable_probe_tier),
        HARD_DISABLE_QUARANTINE_MAX
    );
    // The tier is clamped, so an absurd number of further failures cannot push
    // the quarantine past the documented ceiling.
    assert_eq!(
        quarantine_for_tier(u32::MAX),
        HARD_DISABLE_QUARANTINE_MAX,
        "the ceiling is a real ceiling"
    );
}

#[test]
fn one_bad_probe_session_escalates_the_quarantine_exactly_one_tier() {
    // A single doomed session can emit more than one failure record (a stall
    // AND a typed failure). Escalation is charged per PROBE, not per record,
    // so one bad probe must not jump the model from the 6h base to the ceiling.
    let clock = test_clock();
    let ht = tracker_on(&clock);
    hard_disable_via_trip_rate(&ht, &clock);
    clock.advance_mono(HARD_DISABLE_QUARANTINE_BASE);
    assert!(ht.is_available(S, TEST_MODEL));

    ht.note_dispatch_accepted(S, TEST_MODEL);
    ht.record_stall(S, TEST_MODEL, true);
    ht.record_failure(S, TEST_MODEL);
    ht.record_transient_failure(S, TEST_MODEL);
    ht.apply_breaker_check_for(S, TEST_MODEL);
    assert_eq!(
        ht.model_health(S, TEST_MODEL).hard_disable_probe_tier,
        1,
        "four failure records from one probe must charge exactly one tier"
    );

    // The un-escalated records still re-anchor the deadline at the latest bad
    // evidence, so the next probe is a full tier-1 quarantine away.
    clock.advance_mono(quarantine_for_tier(1) - Duration::from_secs(1));
    assert!(
        !ht.is_available(S, TEST_MODEL),
        "boundary: one second early"
    );
    clock.advance_mono(Duration::from_secs(1));
    assert!(
        ht.is_available(S, TEST_MODEL),
        "boundary: exactly at the deadline"
    );
}

#[test]
fn an_unobserved_probe_still_closes_the_window_for_a_full_quarantine() {
    // The probe's verdict may never arrive: the task is killed, the leader
    // fails over, the pod OOMs. The bound must not depend on the verdict —
    // consuming the probe re-arms the quarantine immediately, so the breaker
    // fails CLOSED rather than leaving the lane open.
    let clock = test_clock();
    let ht = tracker_on(&clock);
    hard_disable_via_trip_rate(&ht, &clock);
    clock.advance_mono(HARD_DISABLE_QUARANTINE_BASE);
    assert!(ht.is_available(S, TEST_MODEL));
    ht.note_dispatch_accepted(S, TEST_MODEL);

    // No success, no failure — nothing at all is recorded afterwards.
    for _ in 0..(HARD_DISABLE_QUARANTINE_BASE.as_secs() / 60) {
        assert!(
            !ht.is_available(S, TEST_MODEL),
            "an unresolved probe must not leave the lane open"
        );
        clock.advance_mono(Duration::from_secs(60));
    }
    assert!(
        ht.is_available(S, TEST_MODEL),
        "…and the bucket is not wedged either: the next quarantine still elapses"
    );
}

#[test]
fn note_dispatch_accepted_is_inert_for_a_healthy_or_cooling_bucket() {
    let clock = test_clock();
    let ht = tracker_on(&clock);
    // Untracked bucket: no entry is created.
    ht.note_dispatch_accepted(S, "never/seen");
    assert!(ht.all_health().is_empty());

    // Healthy bucket: counters untouched.
    ht.record_success(S, TEST_MODEL);
    ht.note_dispatch_accepted(S, TEST_MODEL);
    let health = ht.model_health(S, TEST_MODEL);
    assert!(!health.hard_disabled);
    assert_eq!(health.hard_disable_probe_tier, 0);
    assert!(ht.is_available(S, TEST_MODEL));

    // Ordinary (non-hard) cooldown: consuming a probe is a no-op, the ordinary
    // cooldown ladder still owns the bucket.
    trip_breaker(&ht, TEST_MODEL);
    ht.note_dispatch_accepted(S, TEST_MODEL);
    assert!(!ht.is_available(S, TEST_MODEL));
    clock.advance_mono(INITIAL_COOLDOWN + Duration::from_secs(1));
    assert!(
        ht.is_available(S, TEST_MODEL),
        "the ordinary escalating cooldown is unchanged by the quarantine work"
    );
}

#[test]
fn human_enable_overrides_the_quarantine_immediately_and_survives_one_failure() {
    // HUMAN CONTROLS STAY AUTHORITATIVE. `enable` must take effect at once,
    // mid-quarantine, without waiting for any probe window.
    let clock = test_clock();
    let ht = tracker_on(&clock);
    hard_disable_via_trip_rate(&ht, &clock);
    // Fail a probe first so there is a non-zero tier to clear.
    clock.advance_mono(HARD_DISABLE_QUARANTINE_BASE);
    ht.note_dispatch_accepted(S, TEST_MODEL);
    ht.record_failure(S, TEST_MODEL);
    assert_eq!(ht.model_health(S, TEST_MODEL).hard_disable_probe_tier, 1);
    clock.advance_mono(Duration::from_secs(60 * 60));
    assert!(!ht.is_available(S, TEST_MODEL), "mid-quarantine");

    let total_failures_before = ht.model_health(S, TEST_MODEL).total_failures;
    ht.enable(S, TEST_MODEL);
    let health = ht.model_health(S, TEST_MODEL);
    assert!(
        ht.is_available(S, TEST_MODEL),
        "a human re-enable takes effect immediately regardless of quarantine state"
    );
    assert!(!health.hard_disabled);
    assert!(!health.auto_disabled);
    assert_eq!(health.hard_disable_probe_tier, 0);
    assert_eq!(health.trips_in_window, 0);
    // Deliberate 2026-08-16 change: `enable` now clears the failure STREAKS, so
    // the operator does not have to chase it with `reset` to stop the very next
    // failure re-tripping the breaker.
    assert_eq!(health.consecutive_failures, 0);
    assert_eq!(health.breaker_eligible_consecutive_failures, 0);
    ht.record_failure(S, TEST_MODEL);
    assert!(
        ht.is_available(S, TEST_MODEL),
        "one failure after a human re-enable must not instantly re-trip the breaker"
    );
    // …but the lifetime audit trail is preserved — that is what still
    // distinguishes `enable` from `reset`.
    assert!(ht.model_health(S, TEST_MODEL).total_failures > total_failures_before);
}

#[test]
fn human_reset_clears_the_quarantine_and_the_counters() {
    let clock = test_clock();
    let ht = tracker_on(&clock);
    hard_disable_via_trip_rate(&ht, &clock);
    assert!(!ht.is_available(S, TEST_MODEL));

    ht.reset(S, TEST_MODEL);
    let health = ht.model_health(S, TEST_MODEL);
    assert!(ht.is_available(S, TEST_MODEL));
    assert!(!health.hard_disabled);
    assert_eq!(health.hard_disable_probe_tier, 0);
    assert_eq!(health.total_failures, 0, "reset also wipes the audit trail");

    // `enable_model_all_scopes` (the `model_health(enable)` MCP path) behaves
    // the same across every scope.
    let ht = tracker_on(&clock);
    for scope in [Some("user-a"), Some("user-b")] {
        for _ in 0..(TRIP_RATE_CEILING * CIRCUIT_BREAKER_THRESHOLD as usize) {
            ht.record_failure(scope, TEST_MODEL);
            let remaining = ht
                .model_health(scope, TEST_MODEL)
                .cooldown_seconds_remaining
                .unwrap_or(0);
            clock.advance_mono(Duration::from_secs(remaining + 1));
        }
        assert!(ht.model_health(scope, TEST_MODEL).hard_disabled);
    }
    assert_eq!(ht.enable_model_all_scopes(TEST_MODEL), 2);
    for scope in [Some("user-a"), Some("user-b")] {
        assert!(ht.is_available(scope, TEST_MODEL));
        assert!(!ht.model_health(scope, TEST_MODEL).hard_disabled);
    }
}

#[test]
fn a_restart_re_anchors_the_quarantine_and_can_never_probe_at_boot() {
    // RESTART SEMANTICS. `Instant`s do not cross the persist boundary, so the
    // deadline is re-anchored at `now` from the PERSISTED tier. A restart must
    // therefore only ever delay the next probe — never fire one at boot, and
    // never demote a long-quarantined dead model back to the 6h base.
    let clock = test_clock();
    let ht = tracker_on(&clock);
    hard_disable_via_trip_rate(&ht, &clock);
    // Fail two probes to reach tier 2 (a 24h quarantine).
    for _ in 0..2 {
        clock.advance_mono(quarantine_for_tier(
            ht.model_health(S, TEST_MODEL).hard_disable_probe_tier,
        ));
        assert!(ht.is_available(S, TEST_MODEL));
        ht.note_dispatch_accepted(S, TEST_MODEL);
        ht.record_failure(S, TEST_MODEL);
    }
    let snapshot = ht.all_health();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].hard_disable_probe_tier, 2);

    // A restart loop: 100 boots, each with an hour of uptime, admits nothing.
    let restart_clock = test_clock();
    let restarted = tracker_on(&restart_clock);
    for _ in 0..100 {
        restarted.restore_all(snapshot.clone());
        for _ in 0..60 {
            assert!(
                !restarted.is_available(S, TEST_MODEL),
                "a restart must never resurrect a quarantined model into a probe"
            );
            restart_clock.advance_mono(Duration::from_secs(60));
        }
    }

    // After a single boot the wait is the persisted tier's quarantine (24h),
    // not the 6h base — the escalation survived.
    let boot_clock = test_clock();
    let booted = tracker_on(&boot_clock);
    booted.restore_all(snapshot);
    assert_eq!(
        booted.model_health(S, TEST_MODEL).hard_disable_probe_tier,
        2
    );
    boot_clock.advance_mono(HARD_DISABLE_QUARANTINE_BASE);
    assert!(
        !booted.is_available(S, TEST_MODEL),
        "the persisted tier survived the restart: 6h is not enough at tier 2"
    );
    boot_clock.advance_mono(quarantine_for_tier(2) - HARD_DISABLE_QUARANTINE_BASE);
    assert!(
        booted.is_available(S, TEST_MODEL),
        "…and the quarantine still ends, so a restart cannot make a latch permanent"
    );
}

#[test]
fn pre_half_open_snapshots_load_as_quarantined_at_the_base_tier() {
    // Back-compat: a snapshot persisted before this change has no
    // `hard_disable_probe_tier`. It must deserialize, stay hard-disabled, and
    // recover on the base quarantine — i.e. the very buckets stranded by the
    // 4-day outage heal themselves after one deploy plus 6h.
    let raw = r#"[{
        "model_id": "openai/gpt-5.6-terra",
        "scope": "user-a",
        "auto_disabled": true,
        "consecutive_failures": 3,
        "total_failures": 24,
        "total_successes": 1753,
        "disable_ttl_trips": 8,
        "cooldown_seconds_remaining": null,
        "hard_disabled": true,
        "trips_in_window": 0
    }]"#;
    let snapshot: Vec<ModelHealth> = serde_json::from_str(raw).expect("legacy snapshot parses");
    assert_eq!(snapshot[0].hard_disable_probe_tier, 0);

    let clock = test_clock();
    let ht = tracker_on(&clock);
    ht.restore_all(snapshot);
    let scope = Some("user-a");
    let model = "openai/gpt-5.6-terra";
    assert!(!ht.is_available(scope, model));
    clock.advance_mono(HARD_DISABLE_QUARANTINE_BASE - Duration::from_secs(1));
    assert!(!ht.is_available(scope, model));
    clock.advance_mono(Duration::from_secs(1));
    assert!(
        ht.is_available(scope, model),
        "a bucket stranded by the outage recovers on the base quarantine"
    );
    ht.note_dispatch_accepted(scope, model);
    ht.record_success(scope, model);
    assert!(!ht.model_health(scope, model).hard_disabled);
}

#[test]
fn debug_snapshot_distinguishes_a_waiting_quarantine_from_an_admissible_probe() {
    let clock = test_clock();
    let ht = tracker_on(&clock);
    hard_disable_via_trip_rate(&ht, &clock);

    let entry = ht.debug_snapshot().into_iter().next().unwrap();
    assert_eq!(entry.state, "hard_disabled");
    assert!(
        entry.until.is_some(),
        "a quarantined bucket now renders its next-probe deadline; it used to \
         render `null`, which is why the outage read as an uninterpretable \
         permanent state"
    );

    clock.advance_mono(HARD_DISABLE_QUARANTINE_BASE);
    let entry = ht.debug_snapshot().into_iter().next().unwrap();
    assert_eq!(entry.state, "hard_disabled_probe");
    // The metrics gauge agrees: a bucket in its probe window is half-open.
    let metric = ht.breaker_metric_snapshot().into_iter().next().unwrap();
    assert_eq!(metric.state, BreakerState::HalfOpen);
}

#[test]
fn throttle_cooling_deprioritization_is_unchanged_by_the_quarantine() {
    // The throttle path (`PERSISTENT_THROTTLE_TRIP_THRESHOLD`,
    // `last_trip_throttle`, `is_throttle_cooling`) must behave exactly as before
    // for a bucket that is not hard-disabled.
    let clock = test_clock();
    let ht = tracker_on(&clock);
    ht.record_stall(S, TEST_MODEL, false);
    assert!(ht.is_throttle_cooling(S, TEST_MODEL));
    assert!(!ht.model_health(S, TEST_MODEL).hard_disabled);
    assert_eq!(
        ht.model_health(S, TEST_MODEL).disable_ttl_trips,
        0,
        "a non-persistent throttle still does not ratchet the cap"
    );
    clock.advance_mono(STALL_MIN_COOLDOWN + Duration::from_secs(1));
    assert!(
        ht.is_available(S, TEST_MODEL),
        "a one-off throttle still self-heals"
    );
    ht.record_success(S, TEST_MODEL);
    assert!(!ht.is_throttle_cooling(S, TEST_MODEL));
}
