//! Regression coverage for "deploy/reap interruptions are environmental".
//!
//! A worker/reviewer session killed by an infrastructure event — a coordinator
//! deploy/rollout, a k8s pod eviction, or a startup reap of a run that deploy
//! orphaned — leaves the task dispatch-ready again for the SAME role. The
//! dispatch reappearance path would normally count that as a same-role failure
//! (streak++ + escalating cooldown), unfairly pushing an innocent in-flight
//! task toward strikes/interventions on every deploy. These tests lock the fix:
//! when the prior attempt terminalized as the environmental `interrupted`
//! outcome, the reappearance is spared (no streak, no cooldown, dispatched
//! immediately); a genuine `crashed` attempt still counts as a failure.

use super::*;
use djinn_core::models::task_attempt::TaskAttemptOutcome;
use djinn_db::{TaskAttemptRepository, TerminalTaskAttemptParams};

/// Seed a terminal `(task, role)` attempt with the given outcome, exactly as a
/// dispatch-start + terminalization would leave it.
async fn seed_terminal_attempt(
    db: &Database,
    task_id: &str,
    role: &str,
    outcome: TaskAttemptOutcome,
) {
    let repo = TaskAttemptRepository::new(db.clone());
    let id = uuid::Uuid::now_v7().to_string();
    let dispatch_key = format!("{task_id}:{role}:{id}");
    repo.create_or_get_pending(djinn_db::CreateTaskAttemptParams {
        id: &id,
        task_id,
        role,
        dispatch_key: &dispatch_key,
        session_id: None,
        dispatch_owner_incarnation_id: None,
        dispatch_group_id: None,
        attempt_seq: None,
    })
    .await
    .unwrap();
    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &id,
        outcome,
        pr_url: None,
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: Some("seeded terminal attempt"),
        summary_json: None,
        log_tail: None,
    })
    .await
    .unwrap();
}

/// A reviewer task whose prior session ended in an environmental `interrupted`
/// (deploy/reap) and reappears for the same role must be dispatched immediately
/// with NO dispatch-failure streak and NO cooldown — the interruption was not a
/// task failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn environmental_interrupt_reappearance_dispatches_without_streak_or_cooldown() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) =
        create_simple_task(&db, &tx, "task", "reviewer interrupted by deploy").await;
    let task = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "needs_task_review")
        .await
        .unwrap();

    // The prior reviewer session was killed by infrastructure: its attempt was
    // terminalized environmental.
    seed_terminal_attempt(&db, &task.id, "reviewer", TaskAttemptOutcome::Interrupted).await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    // Seed the reappearance marker that a pre-deploy dispatch (rehydrated at
    // boot) would leave: same role, recent.
    actor.last_dispatched.insert(
        task.id.clone(),
        DispatchMarker {
            instant: StdInstant::now(),
            role: "reviewer".to_owned(),
        },
    );

    actor.dispatch_ready_tasks(None).await;

    assert_eq!(
        actor.dispatched, 1,
        "an environmentally-interrupted task must re-dispatch immediately"
    );
    assert!(
        !actor.dispatch_failure_streak.contains_key(&task.id),
        "an environmental interruption must NOT add a dispatch-failure streak"
    );
    assert!(
        !actor.dispatch_cooldowns.contains_key(&task.id),
        "an environmental interruption must NOT apply an escalating cooldown"
    );
}

/// Control / regression: a reviewer task whose prior session genuinely `crashed`
/// and reappears for the same role IS counted as a same-role failure — streak
/// advances to 1 and a cooldown is applied (no dispatch this pass). This proves
/// the environmental exemption is narrow and genuine failures still back off.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crashed_reappearance_still_counts_as_failure_with_streak_and_cooldown() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) =
        create_simple_task(&db, &tx, "task", "reviewer crashed for real").await;
    let task = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "needs_task_review")
        .await
        .unwrap();

    // The prior reviewer session genuinely crashed — a real failure.
    seed_terminal_attempt(&db, &task.id, "reviewer", TaskAttemptOutcome::Crashed).await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.last_dispatched.insert(
        task.id.clone(),
        DispatchMarker {
            instant: StdInstant::now(),
            role: "reviewer".to_owned(),
        },
    );

    actor.dispatch_ready_tasks(None).await;

    assert_eq!(
        actor.dispatched, 0,
        "a same-role crash reappearance backs off (cooldown) rather than dispatching this pass"
    );
    assert_eq!(
        actor.dispatch_failure_streak.get(&task.id).copied(),
        Some(1),
        "a genuine crash reappearance must advance the dispatch-failure streak"
    );
    assert!(
        actor.dispatch_cooldowns.contains_key(&task.id),
        "a genuine crash reappearance must apply an escalating cooldown"
    );
}
