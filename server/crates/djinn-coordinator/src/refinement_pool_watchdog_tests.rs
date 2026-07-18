//! Regression tests for the 2026-07-09 whole-board freeze fixes as they touch
//! the coordinator: refinement driving must be O(1) in pool round-trips, the
//! bounded pool ask must let the coordinator degrade instead of hang, and the
//! per-pass watchdog must abandon a wedged pass so the loop keeps running.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant as StdInstant};

use djinn_core::events::{DjinnEventEnvelope, EventBus};
use djinn_db::{CreateSessionParams, SessionRepository, TaskRepository};
use djinn_slot::{PoolMessage, PoolStatus, RunningTaskInfo, SlotPoolHandle};
use tokio::sync::mpsc;

use super::RefinementSession;
use crate::actor::CoordinatorActor;
use crate::refinement::{RefinementLoopState, RefinementPhase};
use crate::refinement_dispatch::refinement_cap_tests::{
    TEST_MODEL, build_refinement_actor, seed_refinement_fixture,
};

/// Counters for the pool asks a stub pool observed.
#[derive(Default)]
struct PoolCallCounts {
    get_status: usize,
    has_session: usize,
    kill_session: usize,
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
                PoolMessage::KillSession { respond_to, .. } => {
                    counts_task.lock().expect("counts").kill_session += 1;
                    let _ = respond_to.send(Ok(()));
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
            session_started_at: None,
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

// ─── Pending-time-vs-execution-budget watchdog (incident 2026-07-12) ────────
//
// The 900s execution timeout must measure actual agent session runtime, not
// the time the task-run pod spends Pending in the k8s scheduler queue before
// any agent session starts. A separate, longer pending-start timeout bounds a
// forever-Pending pod, and its expiry is retryable (not a terminal kill of the
// whole tribunal).

/// A monotonic instant `d` in the past (saturating).
fn ago(d: Duration) -> StdInstant {
    StdInstant::now()
        .checked_sub(d)
        .expect("instant within representable range")
}

/// Seed a running refinement whose in-flight session was dispatched at a
/// caller-controlled instant, with no observed session start yet.
fn seed_running_refinement_dispatched_at(
    actor: &mut CoordinatorActor,
    proposal_id: &str,
    task_id: &str,
    dispatched_at: StdInstant,
) {
    actor.active_refinements.insert(
        proposal_id.to_string(),
        RefinementLoopState::new(proposal_id, 1),
    );
    actor.refinement_sessions.insert(
        proposal_id.to_string(),
        RefinementSession {
            task_id: task_id.to_string(),
            phase: RefinementPhase::AdversaryAttack,
            dispatched_at,
            session_started_at: None,
            model_id: TEST_MODEL.to_owned(),
        },
    );
}

/// Create a refinement task row owned by `user_id` in `project_id`.
async fn seed_refinement_task_row(
    db: &djinn_db::Database,
    project_id: &str,
    user_id: &str,
) -> String {
    djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(user_id.to_owned()), async {
            TaskRepository::new(db.clone(), EventBus::noop())
                .create_fixture_in_project(
                    project_id,
                    None,
                    "Refinement watchdog task",
                    "watchdog test task",
                    "",
                    "refinement",
                    0,
                    "adversary",
                    Some("open"),
                    Some("[]"),
                )
                .await
                .expect("seed refinement task")
                .id
        })
        .await
}

/// (a) A dispatched-but-not-yet-started session whose dispatch is already older
/// than the *execution* timeout is NOT terminated: pod-Pending queue time must
/// not burn the agent's execution budget. No session row exists for the task,
/// so the agent has not started.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_queue_time_does_not_burn_execution_budget() {
    let db = crate::test_helpers::create_test_db();
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);

    let task_id = "refine-pending-task";
    let (pool, _counts) = spawn_counting_pool(vec![task_id.to_string()]);
    let mut actor = build_refinement_actor(&db, &events_tx, pool);

    // Dispatched longer ago than the execution timeout, but within the
    // pending-start timeout — and no session row exists (pod still Pending).
    seed_running_refinement_dispatched_at(
        &mut actor,
        "proposal-pending",
        task_id,
        ago(super::REFINEMENT_SESSION_TIMEOUT + Duration::from_secs(60)),
    );

    actor.drive_active_refinements().await;

    assert!(
        actor.active_refinements.contains_key("proposal-pending"),
        "a Pending-in-queue session must NOT be terminated by the execution timeout"
    );
    let session = actor
        .refinement_sessions
        .get("proposal-pending")
        .expect("session must still be in-flight (execution budget not burned)");
    assert!(
        session.session_started_at.is_none(),
        "session start must not be recorded while no session row exists"
    );
}

/// (b) A session that starts late (long after dispatch) still gets the full
/// execution budget measured from session start, not from dispatch. Even though
/// dispatch is older than the execution timeout, a just-created session row
/// resets the clock to session start, so the loop is not terminated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_starting_session_gets_full_budget_from_start() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);

    let task_id = seed_refinement_task_row(&db, &fixture.project_id, &fixture.user_id).await;

    // Materialize a session row NOW — i.e. the agent just started, long after
    // dispatch. `started_at` defaults to the insert time.
    SessionRepository::new(db.clone(), EventBus::noop())
        .create(CreateSessionParams {
            project_id: &fixture.project_id,
            task_id: Some(&task_id),
            model: TEST_MODEL,
            agent_type: "adversary",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("materialize just-started session");

    let (pool, _counts) = spawn_counting_pool(vec![task_id.clone()]);
    let mut actor = build_refinement_actor(&db, &events_tx, pool);

    // Dispatched longer ago than the execution timeout — but the session only
    // just started, so the budget (from start) is nowhere near exhausted.
    seed_running_refinement_dispatched_at(
        &mut actor,
        &fixture.proposal_id,
        &task_id,
        ago(super::REFINEMENT_SESSION_TIMEOUT + Duration::from_secs(120)),
    );

    actor.drive_active_refinements().await;

    assert!(
        actor.active_refinements.contains_key(&fixture.proposal_id),
        "a session that just started must not be terminated despite an old dispatch time"
    );
    let session = actor
        .refinement_sessions
        .get(&fixture.proposal_id)
        .expect("session must still be in-flight");
    assert!(
        session.session_started_at.is_some(),
        "observed session start must be cached once a session row exists"
    );
}

/// (c) A dispatch that sits Pending past the pending-start timeout without ever
/// starting a session is abandoned and retried (bounded by the retry cap), NOT
/// terminated: the tribunal survives, the in-flight session is cleared, the
/// dispatch-failure counter increments, and the still-Pending slot is torn down.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_start_timeout_retries_without_terminating() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);

    let task_id = seed_refinement_task_row(&db, &fixture.project_id, &fixture.user_id).await;

    let (pool, counts) = spawn_counting_pool(vec![task_id.clone()]);
    let mut actor = build_refinement_actor(&db, &events_tx, pool);

    // No session row was ever created; dispatched longer ago than the
    // pending-start timeout (pod stuck Pending in the scheduler queue).
    seed_running_refinement_dispatched_at(
        &mut actor,
        &fixture.proposal_id,
        &task_id,
        ago(super::REFINEMENT_PENDING_START_TIMEOUT + Duration::from_secs(60)),
    );

    actor.drive_active_refinements().await;

    assert!(
        actor.active_refinements.contains_key(&fixture.proposal_id),
        "pending-start timeout must be RETRYABLE — the tribunal must not be terminated"
    );
    assert!(
        !actor.refinement_sessions.contains_key(&fixture.proposal_id),
        "the abandoned in-flight session must be cleared so the phase re-dispatches"
    );
    assert_eq!(
        actor.active_refinements[&fixture.proposal_id].dispatch_failures, 1,
        "the dispatch-failure counter must increment on a pending-start abandonment"
    );
    assert_eq!(
        counts.lock().expect("counts").kill_session,
        1,
        "the still-Pending slot must be torn down via kill_session"
    );
}

/// (d) Once the dispatch-failure retry cap is reached, a further pending-start
/// abandonment escalates to termination (the tribunal is stopped).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_start_timeout_escalates_to_termination_at_retry_cap() {
    let db = crate::test_helpers::create_test_db();
    let fixture = seed_refinement_fixture(&db).await;
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);

    let task_id = seed_refinement_task_row(&db, &fixture.project_id, &fixture.user_id).await;

    let (pool, _counts) = spawn_counting_pool(vec![task_id.clone()]);
    let mut actor = build_refinement_actor(&db, &events_tx, pool);

    seed_running_refinement_dispatched_at(
        &mut actor,
        &fixture.proposal_id,
        &task_id,
        ago(super::REFINEMENT_PENDING_START_TIMEOUT + Duration::from_secs(60)),
    );
    // Pre-load the failure counter to one below the cap so this abandonment
    // reaches it.
    actor
        .active_refinements
        .get_mut(&fixture.proposal_id)
        .expect("seeded refinement")
        .dispatch_failures = super::REFINEMENT_DISPATCH_RETRY_CAP - 1;

    actor.drive_active_refinements().await;

    assert!(
        !actor.active_refinements.contains_key(&fixture.proposal_id),
        "reaching the retry cap must terminate the refinement (removed after retain)"
    );
    assert!(
        !actor.refinement_sessions.contains_key(&fixture.proposal_id),
        "in-flight session must be cleared on terminal escalation"
    );
}
