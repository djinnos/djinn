//! Regression tests for the cumulative, cross-cycle arbitration ceiling
//! ([`MAX_ARBITER_HOLD_CYCLES`]) on the second-strike park rung.
//!
//! Incident gy53 (2026-07-27): every guard on that rung is per-cycle or
//! per-fingerprint and each RESETS by incrementing, so a task that declines via
//! a DIFFERENT guard each round never terminates. gy53 was a large multi-crate
//! retirement where each repair pushed the compile error down a layer, minting a
//! NEW CI failure fingerprint every round — so the first-occurrence-fingerprint
//! guard handed out an unlimited supply of "free" remediations. Result: 65
//! sessions, `intervention_count` 9, `hold_cycle` 9, five Lead arbiter
//! dispatches, ~100% of dispatch capacity for 12+ hours, zero merges board-wide
//! for six hours. The control task fux8 had a comparable `intervention_count`
//! of 5 but only two arbiter dispatches — because its CI happened to go green.
//! The ladder terminated only when the work succeeded.
//!
//! The load-bearing property under test is NOT "some guard eventually parks".
//! It is that the cumulative bound is evaluated BEFORE every sub-guard, so
//! cycle N terminates **regardless of which sub-guard would have declined**. A
//! test that repeats one guard would not prove that, so
//! [`hold_cycle_ceiling_terminates_when_a_different_guard_declines_each_cycle`]
//! deliberately rotates the declining guard between cycles and then presents,
//! at the bound, the very guard that granted a free pass twice before.

use super::*;
use djinn_db::repositories::task_arbitration::{
    CreateArbitrationParams, TaskArbitrationRepository,
};

/// Spend one arbiter hold cycle exactly as the production ladder does: open the
/// arbitration row for `cycle`, then mark it consumed (what an arbiter decision
/// does). `resolve_current_hold_cycle` then reports `cycle + 1` as the next
/// cycle a fresh `try_create` would open.
async fn spend_hold_cycle(db: &Database, task_id: &str, cycle: i32) {
    let arb = TaskArbitrationRepository::new(db.clone());
    let empty = serde_json::json!([]);
    arb.try_create(CreateArbitrationParams {
        task_id,
        hold_cycle: cycle,
        deadline_at: None,
        mirror_head_sha: None,
        github_head_sha: None,
        pr_url: None,
        failing_ci_job_ids: &empty,
        dossier: None,
        directive: Some(&serde_json::json!({ "decision": "reopen" })),
        verification_command: None,
        excluded_models: &empty,
    })
    .await
    .expect("seed arbitration row");
    assert!(
        arb.mark_consumed(task_id, cycle)
            .await
            .expect("consume arbitration row"),
        "seeded arbitration row at cycle {cycle} must transition to consumed"
    );
}

/// Open an arbitration row and leave it UNCONSUMED — a Lead arbiter genuinely
/// in flight for that cycle.
async fn open_in_flight_hold_cycle(db: &Database, task_id: &str, cycle: i32) {
    let arb = TaskArbitrationRepository::new(db.clone());
    let empty = serde_json::json!([]);
    arb.try_create(CreateArbitrationParams {
        task_id,
        hold_cycle: cycle,
        deadline_at: None,
        mirror_head_sha: None,
        github_head_sha: None,
        pr_url: None,
        failing_ci_job_ids: &empty,
        dossier: None,
        directive: None,
        verification_command: None,
        excluded_models: &empty,
    })
    .await
    .expect("seed in-flight arbitration row");
}

/// The `arbiter_hold_cycle_ceiling` audit entries recorded when the cumulative
/// bound terminated a task.
async fn hold_cycle_ceiling_markers(
    repo: &TaskRepository,
    task_id: &str,
) -> Vec<serde_json::Value> {
    repo.query_activity(ActivityQuery {
        task_id: Some(task_id.to_owned()),
        event_type: Some(HOLD_CYCLE_CEILING_MARKER.to_string()),
        limit: 100,
        ..ActivityQuery::default()
    })
    .await
    .unwrap()
    .into_iter()
    .map(|e| serde_json::from_str::<serde_json::Value>(&e.payload).unwrap())
    .collect()
}

/// Build a task sitting exactly on the second-strike rung
/// (`intervention_count == MAX_PLANNER_INTERVENTIONS`).
async fn task_on_the_park_rung(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
) -> djinn_core::models::Task {
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(tx));
    let task = make_task_with_reopen_count(db, tx, REOPEN_INTERVENTION_THRESHOLD).await;
    repo.reset_intervention_counters(&task.id).await.unwrap();
    let task = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        task.intervention_count, MAX_PLANNER_INTERVENTIONS,
        "fixture must sit exactly on the second-strike rung"
    );
    task
}

/// **The gy53 regression.**
///
/// Each cycle declines through a DIFFERENT sub-guard, so no single guard's
/// counter ever accumulates — exactly the shape that let gy53 ride to
/// `hold_cycle` 9. The task must still stop worker redispatch at the cumulative
/// bound and enter the one-shot final arbiter rather than the guard that would
/// otherwise grant another free remediation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hold_cycle_ceiling_terminates_when_a_different_guard_declines_each_cycle() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let base = task_on_the_park_rung(&db, &tx).await;

    // ── Cycle 0: declines via `first_occurrence_fingerprint` ─────────────────
    let mut round = base.clone();
    round.ci_failure_fingerprint = Some("fp-round-0".to_string());
    assert!(
        !actor
            .route_planner_intervention(&round, "worker", "strike", None, 5)
            .await,
        "a novel CI fingerprint must buy one remediation while under the ceiling"
    );
    spend_hold_cycle(&db, &base.id, 0).await;

    // ── Cycle 1: declines via `no_attempted_remediation` ─────────────────────
    // No fingerprint at all, and no post-intervention session ever submitted —
    // a different guard, on a different counter, from cycle 0's.
    let mut round = base.clone();
    round.ci_failure_fingerprint = None;
    assert!(
        !actor
            .route_planner_intervention(&round, "worker", "strike", None, 5)
            .await,
        "an unattempted remediation must redispatch while under the ceiling"
    );
    spend_hold_cycle(&db, &base.id, 1).await;

    // ── Cycle 2: back to `first_occurrence_fingerprint`, new signature ───────
    let mut round = base.clone();
    round.ci_failure_fingerprint = Some("fp-round-2".to_string());
    assert!(
        !actor
            .route_planner_intervention(&round, "worker", "strike", None, 5)
            .await,
        "a second novel fingerprint must also buy a remediation while under the ceiling"
    );
    spend_hold_cycle(&db, &base.id, 2).await;

    // Three cycles are spent. Every sub-guard's own counter is still low: two
    // distinct fingerprints each seen once, zero submitted remediations.
    let markers_before = park_redispatch_markers(&repo, &base.id).await;
    assert_eq!(
        markers_before.len(),
        3,
        "each of the three declines recorded its own park-redispatch marker: {markers_before:?}"
    );
    let kinds: Vec<&str> = markers_before
        .iter()
        .map(|m| m["kind"].as_str().unwrap())
        .collect();
    assert!(
        kinds.contains(&"first_occurrence_fingerprint")
            && kinds.contains(&"no_attempted_remediation"),
        "the declines must come from DIFFERENT sub-guards, else this test proves nothing: {kinds:?}"
    );

    // ── At the bound: present the guard that granted a free pass twice ───────
    // A brand-new fingerprint again. Pre-fix this returned false and the worker
    // redispatched; the cumulative ceiling must win instead.
    let mut final_round = base.clone();
    final_round.ci_failure_fingerprint = Some("fp-round-3".to_string());
    let handled = actor
        .route_planner_intervention(&final_round, "worker", "strike", None, 5)
        .await;
    assert!(
        handled,
        "at the cumulative ceiling the rung must handle (terminate) the task, NOT decline to a \
         redispatch — a novel fingerprint no longer buys a remediation"
    );

    // The ceiling ran BEFORE the sub-guards: no fourth redispatch marker.
    let markers_after = park_redispatch_markers(&repo, &base.id).await;
    assert_eq!(
        markers_after.len(),
        markers_before.len(),
        "the ceiling must short-circuit ahead of every sub-guard, so no new park-redispatch \
         marker is written: {markers_after:?}"
    );

    // Non-worker-dispatching final-arbiter state, with the reason recorded
    // durably.
    let held = repo.get(&base.id).await.unwrap().unwrap();
    assert_eq!(
        held.status, "needs_lead_intervention",
        "the task must enter its one-shot final arbiter, not be worker-redispatched"
    );
    let ceiling_markers = hold_cycle_ceiling_markers(&repo, &base.id).await;
    assert_eq!(
        ceiling_markers.len(),
        1,
        "the ceiling records exactly one durable audit entry: {ceiling_markers:?}"
    );
    assert_eq!(ceiling_markers[0]["ceiling"], MAX_ARBITER_HOLD_CYCLES);
    assert_eq!(
        ceiling_markers[0]["ci_failure_fingerprint"], "fp-round-3",
        "the audit entry names the fingerprint that would otherwise have won another free round"
    );

    // The prospective ceiling cycle is the single final arbitration row.
    let arb = TaskArbitrationRepository::new(db.clone());
    let final_row = arb
        .get_by_task_and_cycle(&base.id, MAX_ARBITER_HOLD_CYCLES)
        .await
        .unwrap()
        .expect("the ceiling opens one final arbitration row");
    assert_eq!(
        final_row.directive.unwrap()["terminal_disposition_required"],
        true
    );
    assert_eq!(
        final_row.dossier.unwrap()["final_disposition"],
        true,
        "the arbiter must receive the terminal mandate in its dossier"
    );
}

/// Boundary: one cycle below the bound the ladder still behaves exactly as
/// before. Without this the ceiling could be trivially satisfied by terminating
/// everything, and the regression above would prove nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hold_cycle_ceiling_does_not_fire_below_the_bound() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let base = task_on_the_park_rung(&db, &tx).await;
    for cycle in 0..(MAX_ARBITER_HOLD_CYCLES - 1) {
        spend_hold_cycle(&db, &base.id, cycle).await;
    }

    let mut round = base.clone();
    round.ci_failure_fingerprint = Some("fp-below-bound".to_string());
    assert!(
        !actor
            .route_planner_intervention(&round, "worker", "strike", None, 5)
            .await,
        "with {} of {MAX_ARBITER_HOLD_CYCLES} cycles spent the rung must still decline to a \
         redispatch",
        MAX_ARBITER_HOLD_CYCLES - 1
    );
    let task = repo.get(&base.id).await.unwrap().unwrap();
    assert_eq!(task.status, "open", "below the bound the task stays live");
    assert!(
        hold_cycle_ceiling_markers(&repo, &base.id).await.is_empty(),
        "no ceiling audit entry may be written below the bound"
    );
    let markers = park_redispatch_markers(&repo, &base.id).await;
    assert_eq!(markers.len(), 1);
    assert_eq!(markers[0]["kind"], "first_occurrence_fingerprint");
}

/// A Lead arbiter genuinely in flight is left alone even at/above the bound: it
/// may still approve or supersede, and it is independently bounded by the 24h
/// arbitration deadline and the decision-failure cap. The ceiling only refuses
/// to open a FRESH cycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hold_cycle_ceiling_yields_to_an_in_flight_arbiter() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let base = task_on_the_park_rung(&db, &tx).await;
    for cycle in 0..MAX_ARBITER_HOLD_CYCLES {
        spend_hold_cycle(&db, &base.id, cycle).await;
    }
    open_in_flight_hold_cycle(&db, &base.id, MAX_ARBITER_HOLD_CYCLES).await;

    let mut round = base.clone();
    round.ci_failure_fingerprint = Some("fp-in-flight".to_string());
    assert!(
        !actor
            .route_planner_intervention(&round, "worker", "strike", None, 5)
            .await,
        "an unconsumed arbitration row means an arbiter is live; the ceiling must not pre-empt it"
    );
    let task = repo.get(&base.id).await.unwrap().unwrap();
    assert_eq!(
        task.status, "open",
        "the in-flight arbiter's task must not be terminally failed by the ceiling"
    );
    assert!(
        hold_cycle_ceiling_markers(&repo, &base.id).await.is_empty(),
        "no ceiling audit entry may be written while an arbiter is in flight"
    );
}

/// gy53 itself carried an open PR. The old generic terminal gate handed it to
/// the PR poller and stranded the epic. A PR-bearing task must instead receive
/// the same one-shot final arbiter, with no eager successor cycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hold_cycle_ceiling_dispatches_one_final_arbiter_for_pr_task() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let base = task_on_the_park_rung(&db, &tx).await;
    repo.set_pr_url(&base.id, "https://github.com/djinnos/djinn/pull/2655")
        .await
        .unwrap();
    let base = repo.get(&base.id).await.unwrap().unwrap();
    for cycle in 0..MAX_ARBITER_HOLD_CYCLES {
        spend_hold_cycle(&db, &base.id, cycle).await;
    }

    let mut round = base.clone();
    round.ci_failure_fingerprint = Some("fp-pr-bearing".to_string());
    assert!(
        actor
            .route_planner_intervention(&round, "worker", "strike", None, 5)
            .await,
        "a PR-bearing task at the ceiling must still be handled, never redispatched"
    );
    let handed_off = repo.get(&base.id).await.unwrap().unwrap();
    assert_eq!(
        handed_off.status, "needs_lead_intervention",
        "the PR-bearing task must reach the final arbiter instead of being parked in pr_review"
    );

    let ceiling_markers = hold_cycle_ceiling_markers(&repo, &base.id).await;
    assert_eq!(
        ceiling_markers.len(),
        1,
        "the ceiling audit entry is written once per task, not once per tick: {ceiling_markers:?}"
    );
    let handoff_markers = repo
        .query_activity(ActivityQuery {
            task_id: Some(base.id.clone()),
            event_type: Some("pr_terminal_handoff".to_string()),
            limit: 100,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        handoff_markers.len(),
        1,
        "terminal PR handoff must be durable and idempotent for one head"
    );
    assert!(
        park_redispatch_markers(&repo, &base.id).await.is_empty(),
        "no sub-guard may run after the ceiling trips"
    );
    let arb = TaskArbitrationRepository::new(db.clone());
    let final_row = arb
        .get_by_task_and_cycle(&base.id, MAX_ARBITER_HOLD_CYCLES)
        .await
        .unwrap()
        .expect("one final row exists");
    assert!(final_row.consumed_at.is_none());
    assert!(
        arb.get_by_task_and_cycle(&base.id, MAX_ARBITER_HOLD_CYCLES + 1)
            .await
            .unwrap()
            .is_none(),
        "dispatching the final arbiter must not eagerly open a successor cycle"
    );
}

/// The arbiter dossier must carry the task's own arbitration history. Without
/// it every Lead session decides in ignorance of the identical reopens before
/// it — gy53 ran five arbiter dispatches that could not see each other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn arbiter_dossier_carries_hold_cycle_and_prior_decisions() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);

    let base = task_on_the_park_rung(&db, &tx).await;
    // One prior cycle, consumed with a `reopen` decision (what `spend_hold_cycle`
    // records) — the history a fresh arbiter must be handed.
    spend_hold_cycle(&db, &base.id, 0).await;

    // Clear the fingerprint guard and the attempted-remediation gate so the
    // rung actually reaches the arbiter-dispatch path and builds a dossier.
    let mut round = base.clone();
    round.ci_failure_fingerprint = None;
    seed_terminated_post_intervention_sessions(&db, &tx, &base.id, &["m-a", "m-b"]).await;

    assert!(
        actor
            .route_planner_intervention(&round, "worker", "strike", None, 5)
            .await,
        "with the sub-guards cleared and budget remaining, the rung dispatches the Lead arbiter"
    );

    let arb = TaskArbitrationRepository::new(db.clone());
    let record = arb
        .get_by_task_and_cycle(&base.id, 1)
        .await
        .unwrap()
        .expect("hold cycle 1 must have been opened");
    let dossier = record.dossier.expect("arbiter dossier must be persisted");
    assert_eq!(
        dossier["hold_cycle"], 1,
        "the dossier must tell the arbiter which cycle it is on: {dossier}"
    );
    let prior = dossier["prior_arbiter_decisions"]
        .as_array()
        .expect("prior_arbiter_decisions must be an array");
    assert_eq!(
        prior.len(),
        1,
        "the dossier must summarize the one prior cycle: {dossier}"
    );
    assert_eq!(prior[0]["hold_cycle"], 0);
    assert_eq!(
        prior[0]["decision"], "reopen",
        "the prior cycle's actual decision must be visible to the next arbiter: {dossier}"
    );
}
