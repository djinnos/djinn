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
    assert_eq!(enabled.disable_ttl_trips, 1);
    assert_eq!(enabled.consecutive_failures, CIRCUIT_BREAKER_THRESHOLD);

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
            retry_after_ms: None,
        },
    );
    // Latest failure for a task overwrites the prior one.
    ht.note_task_provider_failure(
        "task-a",
        TaskFailureSignal {
            throttle: true,
            retry_after_ms: Some(1_000),
        },
    );
    ht.note_task_provider_failure(
        "task-b",
        TaskFailureSignal {
            throttle: false,
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
