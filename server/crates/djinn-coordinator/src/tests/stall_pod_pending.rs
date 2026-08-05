//! Regression coverage: pod-`Pending` time is not agent idle time.
//!
//! `SlotPool::dispatch` seeds the host `ActivityTracker` at slot RESERVATION —
//! before `runtime.prepare` has created the task-run Job. Until the worker's
//! first bridged `touch_activity`, `RunningTaskInfo::idle_seconds` is simply
//! "seconds since reservation", which includes every second the Pod spent
//! `Pending`: unschedulable on its CPU/memory request, held off by a
//! DiskPressure taint, or pulling its image. The idle-stall watchdog consumed
//! that number verbatim, so a slow-to-schedule Pod could arrive at its very
//! first sweep already past `STALL_TIMEOUT_SECS` and be killed as an
//! `idle_stall` — terminalizing the attempt `TimedOut` and telling the Planner
//! to decompose/rescope a task whose only problem was cluster capacity.
//!
//! The fix anchors the idle clock to pod-Running: `sessions.started_at` is
//! written by the worker from INSIDE the Pod, so it cannot predate the Pod
//! running, and an agent cannot have been idle longer than its own session has
//! existed.
//!
//! These two tests differ in exactly ONE input — the age of the session row —
//! so the negative control proves the watchdog was not simply disabled.

use super::*;
use djinn_db::{
    CreateSessionParams, CreateTaskAttemptParams, SessionRepository, TaskAttemptRepository,
};
use djinn_slot::{PoolMessage, PoolStatus, RunningTaskInfo, SlotPoolHandle};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

const STALL_MODEL: &str = "openai/gpt-5.5";

/// Idle the stub pool reports: 35 minutes, comfortably past the 30-minute
/// `STALL_TIMEOUT_SECS`. This is the reservation-anchored number a Pod that
/// waited 35 minutes for a node would produce on its first sweep.
const RESERVATION_ANCHORED_IDLE_SECS: u64 = 35 * 60;

/// Stub pool that reports one running task with a reservation-anchored idle
/// clock and records every `kill_session` it is asked to perform.
///
/// `worker_activity_observed: false` is the load-bearing detail: the tracker
/// entry backing `idle_seconds` is the pool's own reservation seed, never
/// moved by a real worker.
fn spawn_stub_pool(task_id: String) -> (SlotPoolHandle, Arc<Mutex<Vec<String>>>) {
    let kills: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let kills_task = kills.clone();
    let (tx, mut rx) = mpsc::channel::<PoolMessage>(64);
    tokio::spawn(async move {
        let info = |id: &str| RunningTaskInfo {
            task_id: id.to_owned(),
            model_id: STALL_MODEL.to_owned(),
            slot_id: 0,
            duration_seconds: RESERVATION_ANCHORED_IDLE_SECS,
            idle_seconds: RESERVATION_ANCHORED_IDLE_SECS,
            // The pool seeded the tracker at reservation, so it reports a
            // "tracked" idle clock…
            activity_tracked: true,
            // …but no worker has ever spoken: the Pod may still be Pending.
            worker_activity_observed: false,
            project_id: None,
            token_count: 0,
            turn_count: 0,
            no_progress_streak: 0,
        };
        while let Some(msg) = rx.recv().await {
            match msg {
                PoolMessage::GetSessionForTask {
                    task_id: asked,
                    respond_to,
                } => {
                    let reply = (asked == task_id).then(|| info(&asked));
                    let _ = respond_to.send(Ok(reply));
                }
                PoolMessage::KillSession {
                    task_id: asked,
                    respond_to,
                } => {
                    kills_task.lock().expect("kills").push(asked);
                    let _ = respond_to.send(Ok(()));
                }
                PoolMessage::HasSession {
                    task_id: asked,
                    respond_to,
                } => {
                    let _ = respond_to.send(Ok(asked == task_id));
                }
                PoolMessage::GetStatus { respond_to } => {
                    let _ = respond_to.send(Ok(PoolStatus {
                        active_slots: 1,
                        total_slots: 1,
                        per_model: HashMap::new(),
                        running_tasks: vec![info(&task_id)],
                    }));
                }
                other => drop(other),
            }
        }
    });
    (SlotPoolHandle::from_raw_sender(tx), kills)
}

struct StallFixture {
    task: djinn_core::models::Task,
    session_id: String,
    attempt_id: String,
    kills: Arc<Mutex<Vec<String>>>,
    actor: CoordinatorActor,
}

/// Seed a task + running task_run + running session + pending `worker`
/// attempt, and point the actor at a stub pool reporting a 35-minute
/// reservation-anchored idle clock.
async fn seed_stall_fixture(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
    slug: &str,
) -> StallFixture {
    let (task, _note) = create_task_with_note(db, tx, slug).await;
    TaskRepository::new(db.clone(), crate::events::event_bus_for(tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = format!("run-{slug}");
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    let session = SessionRepository::new(db.clone(), crate::events::event_bus_for(tx))
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: STALL_MODEL,
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(&run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    let attempt_id = uuid::Uuid::now_v7().to_string();
    TaskAttemptRepository::new(db.clone())
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &attempt_id,
            task_id: &task.id,
            role: "worker",
            dispatch_key: &format!("{}:worker:{attempt_id}", task.id),
            session_id: Some(&session.id),
            attempt_seq: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    let (pool, kills) = spawn_stub_pool(task.id.clone());
    let mut actor = coordinator_actor_for_tests(db, tx);
    actor.pool = pool;
    // Exhaust the Slow-verdict extension budget up front so both tests measure
    // the SAME decision — "does this session reach the kill path at all?" —
    // rather than one of them merely being deferred by a grant. The extension
    // ladder is orthogonal to the pod-Pending question and is covered
    // elsewhere.
    actor.stall_extension_count.insert(
        session.id.clone(),
        actor.worker_lifecycle_config.slow_extension.max_extensions,
    );

    StallFixture {
        task,
        session_id: session.id,
        attempt_id,
        kills,
        actor,
    }
}

/// Outcome recorded on the seeded attempt, or `None` if it is still pending.
async fn attempt_outcome(db: &Database, attempt_id: &str, task_id: &str) -> Option<String> {
    TaskAttemptRepository::new(db.clone())
        .list_for_task(task_id)
        .await
        .unwrap()
        .into_iter()
        .find(|a| a.id == attempt_id)
        .map(|a| a.outcome.as_str().to_owned())
}

/// Total consecutive failures the model circuit breaker has been fed for
/// `STALL_MODEL` across every scope.
fn breaker_consecutive_failures(actor: &CoordinatorActor) -> u32 {
    actor
        .health
        .all_health()
        .into_iter()
        .filter(|h| h.model_id == STALL_MODEL)
        .map(|h| h.consecutive_failures)
        .sum()
}

/// A session whose Pod only just reached `Running` — its in-pod `sessions` row
/// is seconds old — must NOT be stall-killed just because the pool's
/// reservation-anchored idle clock already reads 35 minutes. Those 35 minutes
/// were spent `Pending`, waiting on cluster capacity.
///
/// Asserts the three side effects that make this incident expensive, not just
/// that a field carries a phase: no kill, no `TimedOut` attempt, no breaker
/// failure. Before the fix all three fired.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pod_pending_time_is_not_counted_as_idle_and_no_stall_kill_fires() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    // The session row is BRAND NEW: the worker wrote it from inside the Pod the
    // moment the Pod finally got scheduled. Everything before it was Pending.
    let mut fx = seed_stall_fixture(&db, &tx, "stall-pod-pending").await;

    fx.actor.enforce_session_stall_timeout().await;

    assert!(
        !fx.actor.stall_killed.contains(&fx.session_id),
        "a session whose Pod only just started must not be marked stall-killed — \
         the 35-minute idle clock is pod-Pending time, not agent idle time"
    );
    assert!(
        fx.kills.lock().expect("kills").is_empty(),
        "no kill_session may be issued for a Pod that just finished scheduling"
    );
    assert_eq!(
        attempt_outcome(&db, &fx.attempt_id, &fx.task.id).await,
        Some("pending".to_owned()),
        "the attempt must stay pending — a scheduling delay is not a TimedOut attempt"
    );
    assert_eq!(
        breaker_consecutive_failures(&fx.actor),
        0,
        "cluster capacity is not model health: record_failure must not be fed"
    );
}

/// Negative control. Same stub pool, same 35-minute reservation-anchored idle
/// clock, same exhausted extension budget — the ONLY difference is that this
/// session has genuinely existed for 40 minutes, so its Pod has been `Running`
/// the whole time and the agent really is hung.
///
/// It must still be killed exactly as before and retain its `TimedOut` attempt.
/// The timeout is coordinator liveness evidence rather than a typed in-pod
/// provider error, so it is deliberately breaker-neutral.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn genuinely_idle_running_pod_is_still_stall_killed() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut fx = seed_stall_fixture(&db, &tx, "stall-running-idle").await;
    // The single differing input: the Pod has been Running for 40 minutes.
    SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .backdate_started_at(&fx.session_id, "40 minutes")
        .await
        .unwrap();

    fx.actor.enforce_session_stall_timeout().await;

    assert!(
        fx.actor.stall_killed.contains(&fx.session_id),
        "a genuinely idle agent in a long-Running Pod must still be stall-killed"
    );
    assert_eq!(
        fx.kills.lock().expect("kills").as_slice(),
        &[fx.task.id.clone()],
        "the stall kill must reach the pool"
    );
    assert_eq!(
        attempt_outcome(&db, &fx.attempt_id, &fx.task.id).await,
        Some("timed_out".to_owned()),
        "a real idle stall must still terminalize the attempt as TimedOut"
    );
    assert_eq!(
        breaker_consecutive_failures(&fx.actor),
        0,
        "a coordinator idle timeout has no typed provider evidence and must not feed the breaker"
    );
}

/// The pure anchor: pre-pod-Running seconds are uncountable, but a Running
/// Pod's idle clock passes through untouched, and an unparseable witness is
/// never treated as evidence.
#[test]
fn anchor_clamps_only_when_the_session_is_younger_than_the_idle_clock() {
    use crate::dispatch::session_recovery::anchor_idle_to_pod_running;

    // Pod waited 35 minutes to schedule; the session is 12 seconds old.
    assert_eq!(anchor_idle_to_pod_running(2100, Some(12)), 12);
    // Genuinely idle inside a long-Running Pod — nothing is clamped away.
    assert_eq!(anchor_idle_to_pod_running(2100, Some(2400)), 2100);
    // Absence of the witness is not evidence.
    assert_eq!(anchor_idle_to_pod_running(2100, None), 2100);
}

/// The liveness classifier's `Pending` handling was unreachable: evidence was
/// built with `pod_phase = Running` for any task the pool merely had a slot
/// mapping for, whether or not a Pod had ever started. With a truthful phase a
/// never-scheduled Pod classifies `Live` (spare it) instead of `Slow` (a
/// kill-path verdict that burns extension budget and then kills).
#[test]
fn unstarted_pod_classifies_live_instead_of_slow() {
    use crate::dispatch::liveness::{ActivitySignal, PodPhase, Verdict, classify};
    use crate::dispatch::session_recovery::build_liveness_evidence;

    let mut db_state = unstarted_db_state();
    let pool_info = RunningTaskInfo {
        task_id: "task-pending".to_owned(),
        model_id: STALL_MODEL.to_owned(),
        slot_id: 0,
        duration_seconds: RESERVATION_ANCHORED_IDLE_SECS,
        idle_seconds: RESERVATION_ANCHORED_IDLE_SECS,
        activity_tracked: true,
        worker_activity_observed: false,
        project_id: None,
        token_count: 0,
        turn_count: 0,
        no_progress_streak: 0,
    };

    let evidence = build_liveness_evidence(Some(&pool_info), &db_state);
    assert_eq!(evidence.pod_phase, Some(PodPhase::Pending));
    assert_eq!(evidence.activity, ActivitySignal::Idle);
    assert_eq!(
        classify(&evidence).verdict,
        Verdict::Live,
        "a Pod that never started must not be classified Slow — Slow is a kill-path \
         verdict that consumes extension budget and then falls through to the kill"
    );

    // Once the worker speaks, the same evidence is a Running Pod and the idle
    // clock is real again: the classifier convicts exactly as before.
    let mut started = pool_info.clone();
    started.worker_activity_observed = true;
    db_state.active_session_id = Some("sess-live".to_owned());
    let evidence = build_liveness_evidence(Some(&started), &db_state);
    assert_eq!(evidence.pod_phase, Some(PodPhase::Running));
    assert_eq!(classify(&evidence).verdict, Verdict::Slow);
}

/// A `CurrentLivenessState` for a task whose Pod has not started: no in-pod
/// session row exists yet.
pub(super) fn unstarted_db_state() -> djinn_db::CurrentLivenessState {
    djinn_db::CurrentLivenessState {
        task_status: Some("in_progress".to_owned()),
        task_is_terminal: false,
        active_session_id: None,
        active_session_status: None,
        latest_task_run_id: Some("run-1".to_owned()),
        latest_task_run_status: Some("running".to_owned()),
        session_liveness_verdict: None,
        session_liveness_outcome_kind: None,
        session_liveness_outcome_reason: None,
        session_liveness_evidence: None,
        task_run_liveness_outcome_kind: None,
        task_run_liveness_outcome_reason: None,
        task_run_liveness_evidence: None,
        task_created_at: None,
        session_started_at: None,
        task_run_started_at: None,
        task_run_ended_at: None,
        // The task was claimed for this dispatch: `open → in_progress`. That is
        // the only transition it can have — a task sitting at `in_progress`
        // with a live task_run has necessarily been claimed, so `None` ("no
        // recorded transition at all") would describe a state that cannot
        // occur. `open` is not a session-held status, so
        // `handed_off_from_session_held_status` stays `false`: nothing has
        // handed this task on, which is exactly right for a Pod that has not
        // started yet.
        last_transition_from_status: Some("open".to_owned()),
    }
}
