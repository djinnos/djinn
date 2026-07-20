//! Merge-queue requeue-loop regression tests for the respawn guard (incident
//! gton).  Split into its own `#[path]` sibling (like `respawn_guard_tests.rs`)
//! to keep each test file under the size guard; `use super::*` semantics and
//! private-item access are unchanged.
//!
//! Task gton (adopted PR #1765, no live worker session) entered a 13-cycle
//! merge-queue requeue loop: GitHub dequeued the PR for `failed_checks` (a
//! merge_group-only test failure — PR-level CI stays green, so the task-row
//! `PrReworkSignal` is blind), the poller reopened the task via `PrCiFailed`,
//! but the respawn guard re-adopted the open PR and handed it back to
//! `pr_review` every pass, so the poller re-enqueued and the same failure
//! recurred — strike-free, no worker, no intervention.
//!
//! Root cause: the durable `reopened` rework marker uses a fixed dispatch key.
//! Once a rework worker had run and left a NEWER non-reopened terminal attempt
//! (`completed`), a later merge-queue reopen with no live attempt to
//! terminalize could not re-assert the signal (the old `ON CONFLICT DO
//! NOTHING`), so the stale marker stayed pinned behind the `completed` row in
//! `created_at` order and the guard saw a non-reopened latest attempt → it
//! re-adopted the PR.  The fix makes `insert_rework_marker` REFRESH the marker
//! so it is the newest attempt again after every reopen, forcing the guard down
//! the rework-dispatch path (which reaches the reopen-count intervention gate,
//! since a merge-queue reopen is a `merge_queue_failed` quality strike).

use super::*;
use djinn_core::events::EventBus;
use djinn_core::models::task_attempt::TaskAttemptOutcome;
use djinn_db::{Database, EpicRepository, TaskAttemptRepository, TaskRepository};

fn test_db() -> Database {
    Database::open_in_memory().unwrap()
}

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

const PR: &str = "https://github.example/owner/repo/pull/1765";

/// Simulate a merge-queue dequeue reopen exactly as
/// `terminalize_for_pr_outcome` does: terminalize any live attempt to
/// `reopened` and ensure a durable rework marker.
async fn merge_queue_reopen(db: &Database, task_id: &str, cycle: u32) {
    super::super::attempt_lifecycle::record_rework_reopen(
        db,
        task_id,
        "worker",
        Some(PR),
        Some(&format!(
            "merge queue rejected PR (reason: failed_checks) — cycle {cycle}"
        )),
        None,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mergequeue_reopen_after_completed_worker_re_asserts_rework_not_adopt() {
    let db = test_db();
    let task = create_task(&db).await;

    // First merge-queue dequeue reopens for rework; no live attempt to
    // terminalize, so a durable `reopened` marker is written.
    merge_queue_reopen(&db, &task.id, 1).await;
    assert_eq!(
        run_respawn_guard(&db, &task.id, "worker", Some(PR), None).await,
        RespawnGuardDecision::Allow,
        "fresh rework marker must bypass adoption"
    );

    // A rework worker dispatches and finishes: a NEWER non-reopened terminal
    // attempt than the marker (the masking row).
    let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
    super::super::attempt_lifecycle::record_legacy_start(&db, &task.id, "worker", None, &dk)
        .await
        .expect("record_legacy_start should succeed");
    super::super::attempt_lifecycle::advance_latest_to_terminal(
        &db,
        super::super::attempt_lifecycle::TerminalAdvancementParams {
            task_id: &task.id,
            role: "worker",
            outcome: TaskAttemptOutcome::Completed,
            pr_url: Some(PR),
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("rework worker completed"),
            summary_json: None,
            log_tail: None,
        },
    )
    .await;

    // The SECOND merge-queue dequeue: still no live attempt to terminalize.
    // Pre-fix this was a no-op and the guard adopted; post-fix the marker is
    // re-asserted as the newest attempt.
    merge_queue_reopen(&db, &task.id, 2).await;

    // No handoff, no adoption: the guard must take the rework-dispatch path.
    assert_eq!(
        run_respawn_guard(&db, &task.id, "worker", Some(PR), None).await,
        RespawnGuardDecision::Allow,
        "a merge-queue reopen after a completed worker must re-assert rework, \
         not re-adopt the open PR (incident gton requeue loop)"
    );

    // Exactly one marker row exists (re-assert refreshes it, never stacks).
    let repo = TaskAttemptRepository::new(db.clone());
    let markers = repo
        .list_for_task(&task.id)
        .await
        .unwrap()
        .into_iter()
        .filter(|a| {
            a.dispatch_key
                == super::super::attempt_lifecycle::rework_marker_dispatch_key(&task.id, "worker")
        })
        .count();
    assert_eq!(markers, 1, "marker re-assert must not stack duplicate rows");
}

/// Full incident replay: adopt a healthy open PR → hand it to the poller →
/// merge-queue reopen after a completed rework worker → the guard must NOT
/// re-adopt/handoff but bypass to the rework-dispatch path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopt_then_handoff_then_mergequeue_loop_bypasses_adoption() {
    let db = test_db();
    let task = create_task(&db).await; // open, retains pr_url
    let task_repo = TaskRepository::new(db.clone(), EventBus::noop());

    // Healthy adoption on the first ready pass (#1785 handoff preserved).
    assert_eq!(
        run_respawn_guard(&db, &task.id, "worker", Some(PR), None).await,
        RespawnGuardDecision::Adopted {
            pr_url: PR.to_owned()
        },
        "a healthy open PR is still adopted"
    );
    record_adopted_pr_attempt(&db, &task.id, "worker", PR, Some("adopted")).await;
    assert!(handoff_adopted_pr_to_poller(&task_repo, &task.id, &task.status, PR).await);

    // Poller enqueues, merge_group fails, reopens (cycle 1) → back to open.
    merge_queue_reopen(&db, &task.id, 1).await;
    assert_eq!(
        run_respawn_guard(&db, &task.id, "worker", Some(PR), None).await,
        RespawnGuardDecision::Allow,
        "cycle 1: marker present → bypass adoption"
    );

    // A rework worker runs and completes (masking row lands).
    let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
    super::super::attempt_lifecycle::record_legacy_start(&db, &task.id, "worker", None, &dk)
        .await
        .expect("record_legacy_start should succeed");
    super::super::attempt_lifecycle::advance_latest_to_terminal(
        &db,
        super::super::attempt_lifecycle::TerminalAdvancementParams {
            task_id: &task.id,
            role: "worker",
            outcome: TaskAttemptOutcome::Completed,
            pr_url: Some(PR),
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("rework completed"),
            summary_json: None,
            log_tail: None,
        },
    )
    .await;

    // Cycle 2 merge-queue reopen: the guard must still bypass adoption despite
    // the newer `completed` attempt (this was the wedge).
    merge_queue_reopen(&db, &task.id, 2).await;
    assert_eq!(
        run_respawn_guard(&db, &task.id, "worker", Some(PR), None).await,
        RespawnGuardDecision::Allow,
        "cycle 2: re-asserted marker → bypass adoption, no requeue loop"
    );
}

/// A genuinely healthy adopted PR (no rework reopen) with a stale prior
/// `completed` attempt must STILL adopt — the re-assert must not fire without a
/// reopen, so #1785 healthy-adoption behavior is preserved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthy_adopted_pr_with_completed_attempt_still_adopts() {
    let db = test_db();
    let task = create_task(&db).await;

    let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
    super::super::attempt_lifecycle::record_legacy_start(&db, &task.id, "worker", None, &dk)
        .await
        .expect("record_legacy_start should succeed");
    super::super::attempt_lifecycle::advance_latest_to_terminal(
        &db,
        super::super::attempt_lifecycle::TerminalAdvancementParams {
            task_id: &task.id,
            role: "worker",
            outcome: TaskAttemptOutcome::Completed,
            pr_url: Some(PR),
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("work landed, PR healthy in review"),
            summary_json: None,
            log_tail: None,
        },
    )
    .await;

    assert_eq!(
        run_respawn_guard(&db, &task.id, "worker", Some(PR), None).await,
        RespawnGuardDecision::Adopted {
            pr_url: PR.to_owned()
        },
        "no reopen → no marker → healthy PR is adopted (no false rework bypass)"
    );
}
