//! Regression suite: a failover chain that exhausts because the model-health
//! breaker is open for EVERY candidate must back off without blaming the task.
//!
//! Before the fix, `apply_chain_exhaustion_side_effects` was called
//! unconditionally on `DispatchOutcome::Failed`, so a breaker-open-for-all
//! exhaustion — in which not a single candidate was ever attempted — advanced
//! `dispatch_failure_streak` exactly like a genuine dispatch failure. The
//! escalating ladder (60+120+240+480+960+1800×4 s ≈ 2.5h) then reached
//! `MAX_DISPATCH_FAILURES` and force-closed the task with "all failover
//! candidates exhausted after multiple attempts". Because the breaker is
//! scope-wide, a provider outage or one revoked credential closed EVERY ready
//! task of the affected user; breaker cooldowns run to 4h and a hard-disabled
//! breaker never auto-recovers, so the outage always outlived the ladder.
//!
//! Each test below asserts a *side effect* (task status, streak counters,
//! cooldown deadlines, durable rows, activity entries) rather than a telemetry
//! label — `OUTCOME_BREAKER` was already recorded before the fix and proves
//! nothing.

use super::*;

use std::time::Duration as StdDuration;

use crate::dispatch::BREAKER_OPEN_EXHAUSTION_SIGNAL_THRESHOLD;
use djinn_db::DispatchStateRepository;
use djinn_provider::catalog::HealthKey;

/// Number of exhaustion cycles driven through the blameless path. Deliberately
/// past `MAX_DISPATCH_FAILURES` so the pre-fix terminal close would definitely
/// have fired.
const CYCLES_PAST_THE_CAP: u32 = MAX_DISPATCH_FAILURES + 2;

const OUTAGE_SCOPE: &str = "outage-user";

async fn open_task_for(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
    title: &str,
) -> djinn_core::models::Task {
    let (task, _project_path) = create_simple_task(db, tx, "task", title).await;
    TaskRepository::new(db.clone(), crate::events::event_bus_for(tx))
        .set_status(&task.id, "open")
        .await
        .unwrap()
}

/// Drive a `(scope, model)` bucket's breaker into the open state through the
/// real `HealthTracker` API (three consecutive failures = `CIRCUIT_BREAKER_
/// THRESHOLD`), so `is_available` reports exactly what production would.
fn trip_breaker_open(health: &djinn_provider::catalog::HealthTracker, model_id: &str) {
    for _ in 0..3 {
        health.record_failure(Some(OUTAGE_SCOPE), model_id);
    }
    assert!(
        !health.is_available(Some(OUTAGE_SCOPE), model_id),
        "fixture precondition: breaker must be OPEN for {model_id}"
    );
}

/// Re-derive the flag exactly as the dispatch call site does, so these tests
/// exercise the real condition instead of hardcoding `true`.
fn breaker_open_for_all(
    health: &djinn_provider::catalog::HealthTracker,
    models: &[String],
) -> bool {
    models
        .iter()
        .all(|model_id| !health.is_available(Some(OUTAGE_SCOPE), model_id))
}

fn remaining_cooldown(actor: &CoordinatorActor, task_id: &str) -> StdDuration {
    actor
        .dispatch_cooldowns
        .get(task_id)
        .map(|expiry| expiry.saturating_duration_since(StdInstant::now()))
        .unwrap_or_default()
}

fn blocked_signal_count(entries: &[djinn_core::models::ActivityEntry]) -> usize {
    entries
        .iter()
        .filter(|e| e.event_type == "breaker_open_dispatch_blocked")
        .count()
}

/// THE bug. With the breaker open for every candidate, drive the exhaustion
/// path well past `MAX_DISPATCH_FAILURES` and assert the task survives:
///
///  * `dispatch_failure_streak` never gains an entry (in memory or durably),
///  * the task is still `open` with no close reason — never force-closed,
///  * a cooldown IS still applied and IS still escalating (no hot spin),
///  * exactly one operator-visible activity entry explains why it is stuck.
///
/// Pre-fix this test fails on the very first assertion group: the streak
/// reaches 10 and `terminally_fail_task` closes the task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn breaker_open_for_all_candidates_never_force_closes_the_task() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = open_task_for(&db, &tx, "breaker-open-outage").await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let models = vec![
        "anthropic/claude-opus-5".to_string(),
        "openai/gpt-5.6-sol".to_string(),
    ];
    for model in &models {
        trip_breaker_open(&actor.health, model);
    }
    let blameless = breaker_open_for_all(&actor.health, &models);
    assert!(
        blameless,
        "fixture precondition: every candidate must be breaker-disabled"
    );

    // ── One cycle: cooldown applied, blame not ───────────────────────────
    actor
        .apply_chain_exhaustion_side_effects(&task, "worker", &models, &[], blameless)
        .await;
    let first_cooldown = remaining_cooldown(&actor, &task.id);
    assert!(
        first_cooldown > StdDuration::ZERO && first_cooldown <= StdDuration::from_secs(60),
        "the first blameless exhaustion must apply the first cooldown rung (60s); \
         got {first_cooldown:?}. A zero/absent cooldown would hot-spin the \
         coordinator over every ready task on every tick for the whole outage."
    );
    assert!(
        !actor.dispatch_failure_streak.contains_key(&task.id),
        "a breaker-open exhaustion attempted NOTHING — it must not enter the \
         blame ledger at all"
    );

    // ── Past the cap: still nothing blamed, still open ────────────────────
    for _ in 1..CYCLES_PAST_THE_CAP {
        actor
            .apply_chain_exhaustion_side_effects(&task, "worker", &models, &[], blameless)
            .await;
    }

    assert_eq!(
        actor.dispatch_failure_streak.get(&task.id),
        None,
        "after {CYCLES_PAST_THE_CAP} breaker-open exhaustions (> MAX_DISPATCH_FAILURES \
         = {MAX_DISPATCH_FAILURES}) the in-memory failure streak must still be absent"
    );
    assert_eq!(
        actor.breaker_open_backoff_streak.get(&task.id).copied(),
        Some(CYCLES_PAST_THE_CAP),
        "the blameless backoff ladder must count every cycle — it is what makes the \
         cooldown escalate — while staying separate from the blame ledger"
    );

    let after = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        after.status, "open",
        "the task must NOT be force-closed: nothing was ever attempted, so the \
         exhaustion says nothing about this task. Pre-fix this was `closed`."
    );
    assert!(
        after.close_reason.is_none(),
        "no close reason may be recorded; got {:?}",
        after.close_reason
    );

    // ── Durable state: backoff persisted, blame not ───────────────────────
    let durable = DispatchStateRepository::new(db.clone())
        .get(&task.id)
        .await
        .unwrap()
        .expect("blameless backoff must still write through a dispatch_state row");
    assert_eq!(
        durable.failure_streak, 0,
        "the DURABLE failure streak must stay 0 — it survives coordinator restarts, \
         so a poisoned value would keep closing the task after every restart"
    );
    assert!(
        durable.cooldown_until.is_some(),
        "the durable cooldown must be written so the backoff survives a restart"
    );

    // ── Backoff really escalates (not a fixed 60s re-walk) ───────────────
    let escalated = remaining_cooldown(&actor, &task.id);
    assert!(
        escalated >= StdDuration::from_secs(1700),
        "by cycle {CYCLES_PAST_THE_CAP} the ladder must have escalated to the \
         MAX_DISPATCH_COOLDOWN plateau (~1800s); got {escalated:?}"
    );
    assert!(
        escalated > first_cooldown,
        "cooldown must grow across cycles ({first_cooldown:?} → {escalated:?})"
    );

    // ── Loud, not silent ─────────────────────────────────────────────────
    let entries = repo.list_activity(&task.id).await.unwrap();
    assert_eq!(
        blocked_signal_count(&entries),
        1,
        "exactly one operator-visible entry: the task is parked for the whole \
         outage, so a silent skip would just make a loud wrong behaviour quiet — \
         but repeating it every cycle would bury the timeline"
    );
    let signal = entries
        .iter()
        .find(|e| e.event_type == "breaker_open_dispatch_blocked")
        .unwrap();
    for model in &models {
        assert!(
            signal.payload.contains(model.as_str()),
            "the signal must name every breaker-disabled candidate; missing {model}"
        );
    }
    assert!(
        signal.payload.contains("has NOT been closed"),
        "the signal must tell the operator the task is parked, not closed"
    );
    assert!(
        !entries.iter().any(|e| e
            .payload
            .contains("all failover candidates exhausted after")),
        "the terminal-close narration must never appear on this path"
    );
}

/// The operator signal fires exactly at the threshold — not before, not twice.
/// Mirrors the single-candidate-exhaustion precedent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn breaker_open_operator_signal_fires_once_at_the_threshold() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = open_task_for(&db, &tx, "breaker-open-signal-dedup").await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let models = vec!["zai/glm-5.2".to_string()];
    trip_breaker_open(&actor.health, &models[0]);
    let blameless = breaker_open_for_all(&actor.health, &models);

    for _ in 0..(BREAKER_OPEN_EXHAUSTION_SIGNAL_THRESHOLD - 1) {
        actor
            .apply_chain_exhaustion_side_effects(&task, "worker", &models, &[], blameless)
            .await;
    }
    let entries = repo.list_activity(&task.id).await.unwrap();
    assert_eq!(
        blocked_signal_count(&entries),
        0,
        "no signal before the threshold — a one-off provider blip is absorbed by \
         the first cooldown rungs"
    );

    actor
        .apply_chain_exhaustion_side_effects(&task, "worker", &models, &[], blameless)
        .await;
    assert_eq!(
        blocked_signal_count(&repo.list_activity(&task.id).await.unwrap()),
        1,
        "exactly one signal as the streak crosses the threshold"
    );

    for _ in 0..4 {
        actor
            .apply_chain_exhaustion_side_effects(&task, "worker", &models, &[], blameless)
            .await;
    }
    assert_eq!(
        blocked_signal_count(&repo.list_activity(&task.id).await.unwrap()),
        1,
        "the signal must be deduplicated, not re-appended every cycle"
    );

    // A single-candidate lane must NOT also fire the single-candidate
    // environmental-failure signal: nothing was attempted, so there is no
    // environmental failure to report about the model.
    assert_eq!(
        repo.list_activity(&task.id)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.event_type == "single_candidate_failover_exhaustion")
            .count(),
        0,
        "the blameless path must not borrow the attempted-and-failed narration"
    );
}

/// NEGATIVE CONTROL: the safety net was narrowed, not disabled.
///
/// A genuine exhaustion — candidates actually attempted, non-empty
/// `exhausted_observations`, breaker NOT open for all candidates — must still
/// advance `dispatch_failure_streak` on every cycle and must still force-close
/// the task at `MAX_DISPATCH_FAILURES`. If this test ever goes green while the
/// blameless test also passes only because the streak was globally disabled,
/// this assertion catches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn genuine_exhaustion_still_advances_the_streak_and_force_closes_at_the_cap() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    // No PR on this task, so the terminal gate force-closes rather than
    // handing off to the PR poller.
    let task = open_task_for(&db, &tx, "genuine-exhaustion-control").await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let attempted_model = "anthropic/claude-opus-5".to_string();
    let healthy_model = "openai/gpt-5.6-sol".to_string();
    let models = vec![attempted_model.clone(), healthy_model.clone()];

    // Both candidates start available: this chain was really traversed.
    assert!(
        !breaker_open_for_all(&actor.health, &models),
        "control precondition: the breaker must NOT be open for all candidates"
    );

    // Observations from THIS chain, as `try_dispatch_to_pool` would return.
    let observations = vec![HealthKey::new(Some(OUTAGE_SCOPE), &attempted_model)];

    for cycle in 1..=MAX_DISPATCH_FAILURES {
        // Recompute per cycle exactly as the dispatch loop does. The observed
        // candidate's breaker trips partway through, but `healthy_model` stays
        // available, so the chain remains blameable throughout.
        let blameless = breaker_open_for_all(&actor.health, &models);
        assert!(
            !blameless,
            "cycle {cycle}: this control must stay on the blameable path"
        );
        actor
            .apply_chain_exhaustion_side_effects(&task, "worker", &models, &observations, blameless)
            .await;

        if cycle < MAX_DISPATCH_FAILURES {
            assert_eq!(
                actor.dispatch_failure_streak.get(&task.id).copied(),
                Some(cycle),
                "cycle {cycle}: a genuinely attempted exhaustion MUST advance the \
                 failure streak"
            );
            assert!(
                actor.dispatch_cooldowns.contains_key(&task.id),
                "cycle {cycle}: the blameable path still backs off"
            );
        }
    }

    let after = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        after.status, "closed",
        "the terminal safety net must still fire at MAX_DISPATCH_FAILURES for a \
         task whose candidates were actually attempted and actually failed"
    );
    assert!(
        after.close_reason.is_some(),
        "the force-close must record a close reason"
    );
    assert!(
        !actor.dispatch_failure_streak.contains_key(&task.id),
        "the terminal close clears the streak"
    );
    assert_eq!(
        blocked_signal_count(&repo.list_activity(&task.id).await.unwrap()),
        0,
        "the blameless breaker signal must never appear on a genuinely attempted \
         exhaustion"
    );
}

/// A breaker outage must not silently spend the task's safety-net budget: after
/// a long blameless park, a later genuine failure still needs the full
/// `MAX_DISPATCH_FAILURES` attempted exhaustions before the task is closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blameless_park_does_not_consume_the_terminal_budget() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = open_task_for(&db, &tx, "budget-not-consumed").await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let parked_model = "anthropic/claude-opus-5".to_string();
    let models = vec![parked_model.clone()];
    trip_breaker_open(&actor.health, &parked_model);
    for _ in 0..CYCLES_PAST_THE_CAP {
        let blameless = breaker_open_for_all(&actor.health, &models);
        actor
            .apply_chain_exhaustion_side_effects(&task, "worker", &models, &[], blameless)
            .await;
    }
    assert_eq!(repo.get(&task.id).await.unwrap().unwrap().status, "open");

    // The outage heals; the very next genuine exhaustion starts the blame
    // ladder at rung 1, not at rung 11.
    actor.health.reset(Some(OUTAGE_SCOPE), &parked_model);
    let observations = vec![HealthKey::new(Some(OUTAGE_SCOPE), &parked_model)];
    actor
        .apply_chain_exhaustion_side_effects(&task, "worker", &models, &observations, false)
        .await;
    assert_eq!(
        actor.dispatch_failure_streak.get(&task.id).copied(),
        Some(1),
        "the first genuine exhaustion after a blameless park must be streak 1"
    );
    assert!(
        !actor.breaker_open_backoff_streak.contains_key(&task.id),
        "a genuinely attempted exhaustion resets the blameless ladder"
    );
    assert_eq!(repo.get(&task.id).await.unwrap().unwrap().status, "open");
}
