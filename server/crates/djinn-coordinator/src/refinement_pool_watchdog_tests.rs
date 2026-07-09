//! Regression tests for the 2026-07-09 whole-board freeze fixes as they touch
//! the coordinator: refinement driving must be O(1) in pool round-trips, the
//! bounded pool ask must let the coordinator degrade instead of hang, and the
//! per-pass watchdog must abandon a wedged pass so the loop keeps running.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant as StdInstant;

use djinn_core::events::DjinnEventEnvelope;
use djinn_slot::{PoolMessage, PoolStatus, RunningTaskInfo, SlotPoolHandle};
use tokio::sync::mpsc;

use super::RefinementSession;
use crate::actor::CoordinatorActor;
use crate::refinement::{RefinementLoopState, RefinementPhase};
use crate::refinement_dispatch::refinement_cap_tests::{TEST_MODEL, build_refinement_actor};

/// Counters for the pool asks a stub pool observed.
#[derive(Default)]
struct PoolCallCounts {
    get_status: usize,
    has_session: usize,
}

/// Spawn a stub pool (via `from_raw_sender`) that answers `GetStatus` with the
/// given `running` task ids and tallies which asks it received. Any session in
/// `running` is reported as live so `drive_one_refinement` short-circuits on the
/// in-memory membership check without any further pool round-trip.
fn spawn_counting_pool(running: Vec<String>) -> (SlotPoolHandle, Arc<Mutex<PoolCallCounts>>) {
    let counts = Arc::new(Mutex::new(PoolCallCounts::default()));
    let counts_task = counts.clone();
    let (tx, mut rx) = mpsc::channel::<PoolMessage>(64);
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match msg {
                PoolMessage::GetStatus { respond_to } => {
                    counts_task.lock().expect("counts").get_status += 1;
                    let running_tasks = running
                        .iter()
                        .map(|id| RunningTaskInfo {
                            task_id: id.clone(),
                            model_id: TEST_MODEL.to_owned(),
                            slot_id: 0,
                            duration_seconds: 0,
                            idle_seconds: 0,
                            activity_tracked: true,
                            project_id: None,
                            token_count: 0,
                            turn_count: 0,
                            no_progress_streak: 0,
                        })
                        .collect();
                    let _ = respond_to.send(Ok(PoolStatus {
                        active_slots: running.len(),
                        total_slots: running.len(),
                        per_model: HashMap::new(),
                        running_tasks,
                    }));
                }
                PoolMessage::HasSession { respond_to, .. } => {
                    counts_task.lock().expect("counts").has_session += 1;
                    let _ = respond_to.send(Ok(true));
                }
                _ => {}
            }
        }
    });
    (SlotPoolHandle::from_raw_sender(tx), counts)
}

/// Seed an active refinement whose in-flight session task is `task_id`.
fn seed_running_refinement(actor: &mut CoordinatorActor, proposal_id: &str, task_id: &str) {
    actor.active_refinements.insert(
        proposal_id.to_string(),
        RefinementLoopState::new(proposal_id, 1),
    );
    actor.refinement_sessions.insert(
        proposal_id.to_string(),
        RefinementSession {
            task_id: task_id.to_string(),
            phase: RefinementPhase::AdversaryAttack,
            dispatched_at: StdInstant::now(),
            model_id: TEST_MODEL.to_owned(),
        },
    );
}

/// Item 4: driving N active refinements issues exactly ONE `get_status` and
/// ZERO `has_session` asks — down from one `has_session` per refinement (the
/// N× amplification that piled onto the coordinator mailbox during the freeze).
#[tokio::test]
async fn drive_active_refinements_issues_one_status_query_for_n_refinements() {
    let db = crate::test_helpers::create_test_db();
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);

    const N: usize = 5;
    let task_ids: Vec<String> = (0..N).map(|i| format!("refine-task-{i}")).collect();
    let (pool, counts) = spawn_counting_pool(task_ids.clone());

    let mut actor = build_refinement_actor(&db, &events_tx, pool);
    for (i, task_id) in task_ids.iter().enumerate() {
        seed_running_refinement(&mut actor, &format!("proposal-{i}"), task_id);
    }

    actor.drive_active_refinements().await;

    let counts = counts.lock().expect("counts");
    assert_eq!(
        counts.get_status, 1,
        "expected exactly one get_status for {N} refinements, got {}",
        counts.get_status
    );
    assert_eq!(
        counts.has_session, 0,
        "expected zero has_session asks (O(1) via get_status), got {}",
        counts.has_session
    );
}

/// Item 1 + degraded path: when the pool never replies, `drive_active_refinements`
/// gives up within the bounded `get_status` ask (`PoolError::Timeout`) instead of
/// hanging, and burns no refinement rounds — the loop stays intact for next tick.
/// `start_paused` auto-advances past `POOL_ASK_TIMEOUT` so the test is fast.
#[tokio::test(start_paused = true)]
async fn drive_active_refinements_degrades_when_pool_never_replies() {
    let db = crate::test_helpers::create_test_db();
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);

    // Receiver kept alive but never drained → get_status enqueues then times out.
    let (tx, _rx) = mpsc::channel::<PoolMessage>(64);
    let pool = SlotPoolHandle::from_raw_sender(tx);

    let mut actor = build_refinement_actor(&db, &events_tx, pool);
    seed_running_refinement(&mut actor, "proposal-a", "refine-task-a");

    // Must return (not hang). If the bound were absent this would block forever.
    actor.drive_active_refinements().await;

    assert!(
        actor.active_refinements.contains_key("proposal-a"),
        "refinement must survive a pool-unresponsive tick (no round burned)"
    );
    assert!(
        actor.refinement_sessions.contains_key("proposal-a"),
        "in-flight session must be left intact when pool liveness is unknown"
    );
    drop(_rx);
}

/// Item 3: a pass that blocks past the watchdog deadline is abandoned so the
/// coordinator loop continues, and a fast pass runs to completion normally.
#[tokio::test(start_paused = true)]
async fn watchdog_abandons_a_wedged_pass_and_runs_the_next() {
    // A pass that never completes must not hang the watchdog: it returns once the
    // deadline elapses (auto-advanced by the paused clock).
    CoordinatorActor::run_pass_with_watchdog("wedged", std::future::pending::<()>()).await;

    // A normal pass runs to completion (side effect observed).
    let ran = Arc::new(Mutex::new(false));
    let ran_inner = ran.clone();
    CoordinatorActor::run_pass_with_watchdog("fast", async move {
        *ran_inner.lock().expect("ran") = true;
    })
    .await;
    assert!(*ran.lock().expect("ran"), "fast pass should have completed");
}
