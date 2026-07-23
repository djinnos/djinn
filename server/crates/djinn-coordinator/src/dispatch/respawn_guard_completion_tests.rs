//! Run-completion regression tests for the respawn guard (task pl4n,
//! 2026-07-23).  Split into its own `#[path]` sibling (like
//! `respawn_guard_tests.rs`) to keep each test file under the size guard;
//! `use super::*` semantics and private-item access are unchanged.
//!
//! A dispatch group carries both the coordinator's `<task>:worker:<uuid>`
//! dispatch-start row and the supervisor's exact `task-run:<id>` row.
//! `submit_work` advanced only the newest pending row to `submitted`; on a
//! successful `WorkerSubmitted` report the sibling `task-run:`-keyed row
//! stayed `pending` forever, so after the PR reopened for rework the guard saw
//! a phantom `pending` attempt and deferred every dispatch on each coordinator
//! tick (~45 minutes) until the periodic reaper mislabeled the successfully
//! submitted run `crashed`.  The supervisor's run-completion path
//! (`terminalize_run_attempt` in `djinn-agent`) now terminalizes the group's
//! leftover `pending` rows as `completed`; this file pins the guard-side
//! contract: a completed task-run must leave no pending `task-run:`-keyed
//! attempt, and the guard must Allow the rework dispatch after the reopen.

use super::*;
use djinn_core::events::EventBus;
use djinn_core::models::task_attempt::TaskAttemptOutcome;
use djinn_db::{Database, EpicRepository, TaskAttemptRepository, TaskRepository};

fn test_db() -> Database {
    Database::open_in_memory().unwrap()
}

/// Create a minimal task row for FK satisfaction.
async fn create_task(db: &Database) -> djinn_core::models::Task {
    let event_bus = EventBus::noop();
    let epic_repo = EpicRepository::new(db.clone(), event_bus.clone());
    let epic = epic_repo
        .create("Epic", "", "", "", "", None)
        .await
        .unwrap();
    let task_repo = TaskRepository::new(db.clone(), event_bus);
    task_repo
        .create(&epic.id, "Test task", "", "", "task", 0, "", None)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_run_leaves_no_pending_taskrun_attempt_and_guard_allows_rework() {
    let db = test_db();
    let task = create_task(&db).await;
    let repo = TaskAttemptRepository::new(db.clone());
    let owner = uuid::Uuid::now_v7().to_string();
    let group = uuid::Uuid::now_v7().to_string();

    // Coordinator dispatch-start row + supervisor exact `task-run:` row,
    // correlated by one dispatch group.
    let coordinator_key = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
    let coordinator_row = repo
        .create_or_get_pending(djinn_db::CreateTaskAttemptParams {
            id: &uuid::Uuid::now_v7().to_string(),
            task_id: &task.id,
            role: "worker",
            dispatch_key: &coordinator_key,
            session_id: None,
            attempt_seq: None,
            dispatch_owner_incarnation_id: Some(&owner),
            dispatch_group_id: Some(&group),
        })
        .await
        .expect("coordinator dispatch-start row");
    let taskrun_key = "task-run:019f9040-9baa-7582-af72-aa8354f114d7";
    repo.create_or_get_pending(djinn_db::CreateTaskAttemptParams {
        id: &uuid::Uuid::now_v7().to_string(),
        task_id: &task.id,
        role: "worker",
        dispatch_key: taskrun_key,
        session_id: None,
        attempt_seq: None,
        dispatch_owner_incarnation_id: Some(&owner),
        dispatch_group_id: Some(&group),
    })
    .await
    .expect("supervisor task-run exact row");

    // The worker submits: `submit_work` advanced exactly one row to
    // `submitted` — in the production incident it was the coordinator's
    // `<task>:worker:<uuid>` row, leaving the `task-run:` row pending.
    repo.advance_to_submitted(djinn_db::SubmitTaskAttemptParams {
        id: &coordinator_row.id,
        submit_ref: Some("ref-1"),
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: Some("submitted for review"),
        summary_json: None,
        log_tail: None,
    })
    .await
    .expect("advance coordinator row to submitted");

    // Run completion (`WorkerSubmitted` terminal report): the supervisor
    // terminalizes the group's leftover pending rows as `completed` —
    // exercising the same group primitive `terminalize_run_attempt` uses.
    repo.terminalize_dispatch_group(
        &group,
        TaskAttemptOutcome::Completed,
        djinn_db::DispatchGroupTerminalEvidence {
            summary: Some("run reached terminal outcome worker_submitted"),
            summary_json: None,
        },
    )
    .await
    .expect("terminalize leftover pending group rows on completion");

    // A completed task-run must leave no pending `task-run:`-keyed attempt.
    let taskrun_row = repo
        .get_by_dispatch_key(taskrun_key)
        .await
        .unwrap()
        .expect("task-run row exists");
    assert_eq!(
        taskrun_row.outcome, "completed",
        "the task-run:-keyed row must be terminal after run completion"
    );
    // The submitted row keeps its signal for the PR lifecycle.
    let live = repo
        .latest_pending_or_submitted(&task.id, Some("worker"))
        .await
        .unwrap()
        .expect("submitted row still live");
    assert_eq!(live.outcome, "submitted");

    // The PR later needs rework: the reopen terminalizes the submitted row.
    super::super::attempt_lifecycle::record_rework_reopen(
        &db,
        &task.id,
        "worker",
        Some("https://github.example/owner/repo/pull/2500"),
        Some("changes requested"),
        None,
    )
    .await;

    // The guard must Allow the rework worker — no phantom pending attempt.
    let decision = run_respawn_guard(
        &db,
        &task.id,
        "worker",
        Some("https://github.example/owner/repo/pull/2500"),
        None,
    )
    .await;
    assert_eq!(
        decision,
        RespawnGuardDecision::Allow,
        "rework dispatch must not be deferred by a completed run's attempt rows"
    );
}
