//! Tests for the pre-dispatch respawn guard (`super::*` = `respawn_guard`).
//!
//! Split out of `respawn_guard.rs` to keep the module under the size guard;
//! included via `#[path]` so it remains the same child module (private items
//! and `use super::*` semantics unchanged).

use super::*;
use djinn_core::events::EventBus;
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

// ─── run_respawn_guard tests ─────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_allows_when_no_prior_attempts() {
    let db = test_db();
    let task = create_task(&db).await;

    let decision = run_respawn_guard(&db, &task.id, "worker", None, None).await;
    assert_eq!(decision, RespawnGuardDecision::Allow);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_defers_when_pending_attempt_exists() {
    let db = test_db();
    let task = create_task(&db).await;

    // Insert a pending attempt (simulating a dispatch-start that landed).
    let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
    super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
        .await
        .expect("record_dispatch_start should succeed");

    let decision = run_respawn_guard(&db, &task.id, "worker", None, None).await;
    assert_eq!(
        decision,
        RespawnGuardDecision::Defer(GuardReason::RespawnGuard)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_defers_when_submitted_attempt_exists() {
    let db = test_db();
    let task = create_task(&db).await;

    // Insert a pending attempt, then advance it to submitted.
    let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
    super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
        .await
        .expect("record_dispatch_start should succeed");

    super::super::attempt_lifecycle::advance_to_submitted(
        &db,
        super::super::attempt_lifecycle::SubmitAdvancementParams {
            task_id: &task.id,
            role: "worker",
            submit_ref: Some("ref-1"),
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("submitted"),
            summary_json: None,
        },
    )
    .await;

    let decision = run_respawn_guard(&db, &task.id, "worker", None, None).await;
    assert_eq!(
        decision,
        RespawnGuardDecision::Defer(GuardReason::RespawnGuard)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_allows_after_terminal_attempt() {
    let db = test_db();
    let task = create_task(&db).await;

    // Create and terminally close an attempt.
    let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
    super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
        .await
        .expect("record_dispatch_start should succeed");

    super::super::attempt_lifecycle::advance_latest_to_terminal(
        &db,
        super::super::attempt_lifecycle::TerminalAdvancementParams {
            task_id: &task.id,
            role: "worker",
            outcome: djinn_core::models::task_attempt::TaskAttemptOutcome::Completed,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("done"),
            summary_json: None,
            log_tail: None,
        },
    )
    .await;

    let decision = run_respawn_guard(&db, &task.id, "worker", None, None).await;
    assert_eq!(decision, RespawnGuardDecision::Allow);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_allows_for_different_role() {
    let db = test_db();
    let task = create_task(&db).await;

    // Insert a pending attempt for "reviewer".
    let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "reviewer");
    super::super::attempt_lifecycle::record_dispatch_start(
        &db, &task.id, "reviewer", None, &dk,
    )
    .await
    .expect("record_dispatch_start should succeed");

    // Guard for "worker" role should allow (different role).
    let decision = run_respawn_guard(&db, &task.id, "worker", None, None).await;
    assert_eq!(decision, RespawnGuardDecision::Allow);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_ignores_deferred_attempts() {
    let db = test_db();
    let task = create_task(&db).await;

    // Insert a deferred guard-only row (previous guard deferral).
    record_guard_deferred_attempt(
        &db,
        &task.id,
        "worker",
        GuardReason::RespawnGuard,
        Some("previous deferral"),
    )
    .await
    .expect("guard deferred row should insert");

    // The guard should allow because `deferred` is not in
    // ('pending', 'submitted').
    let decision = run_respawn_guard(&db, &task.id, "worker", None, None).await;
    assert_eq!(decision, RespawnGuardDecision::Allow);
}

// ─── record_guard_deferred_attempt tests ─────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_deferred_creates_audit_row() {
    let db = test_db();
    let task = create_task(&db).await;

    let attempt_id = record_guard_deferred_attempt(
        &db,
        &task.id,
        "worker",
        GuardReason::RespawnGuard,
        Some("duplicate spawn blocked"),
    )
    .await
    .expect("should return attempt id");

    let repo = TaskAttemptRepository::new(db);
    let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(attempt.task_id, task.id);
    assert_eq!(attempt.role, "worker");
    assert_eq!(attempt.outcome, "deferred");
    assert_eq!(
        attempt.guard_decision_enum().unwrap(),
        Some(GuardDecision::Defer)
    );
    assert_eq!(
        attempt.guard_reason.as_deref(),
        Some(GuardReason::RespawnGuard.as_str())
    );
    assert_eq!(attempt.summary.as_deref(), Some("duplicate spawn blocked"));
    // Session_id must be NULL for guard-only rows.
    assert!(attempt.session_id.is_none());
    // Terminal_at must be set (deferred is terminal).
    assert!(attempt.terminal_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_deferred_with_capacity_reason() {
    let db = test_db();
    let task = create_task(&db).await;

    let attempt_id = record_guard_deferred_attempt(
        &db,
        &task.id,
        "worker",
        GuardReason::Capacity,
        Some("user at per-model cap"),
    )
    .await
    .expect("should return attempt id");

    let repo = TaskAttemptRepository::new(db);
    let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(attempt.outcome, "deferred");
    assert_eq!(
        attempt.guard_reason.as_deref(),
        Some(GuardReason::Capacity.as_str())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_deferred_idempotent_on_same_dispatch_key() {
    let db = test_db();
    let task = create_task(&db).await;

    // Both calls use different dispatch keys (make_dispatch_key generates
    // unique keys), so two rows are created. Idempotency is on the
    // dispatch_key column, and each guard deferral gets its own key.
    let id1 = record_guard_deferred_attempt(
        &db,
        &task.id,
        "worker",
        GuardReason::RespawnGuard,
        Some("first deferral"),
    )
    .await;
    let id2 = record_guard_deferred_attempt(
        &db,
        &task.id,
        "worker",
        GuardReason::RespawnGuard,
        Some("second deferral"),
    )
    .await;

    // Both should succeed (different dispatch keys).
    assert!(id1.is_some());
    assert!(id2.is_some());
    assert_ne!(id1, id2);

    let repo = TaskAttemptRepository::new(db);
    let all = repo.list_for_task(&task.id).await.unwrap();
    assert_eq!(all.len(), 2);
    // All should be deferred.
    for attempt in &all {
        assert_eq!(attempt.outcome, "deferred");
    }
}

// ─── Guard ordering / no-counter side effects ────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_does_not_create_pending_or_submitted_rows() {
    let db = test_db();
    let task = create_task(&db).await;

    // Record a guard deferral.
    record_guard_deferred_attempt(
        &db,
        &task.id,
        "worker",
        GuardReason::RespawnGuard,
        Some("test"),
    )
    .await;

    // Verify the deferred row is NOT visible as pending/submitted to the
    // guard — only pending/submitted rows should block dispatch.
    let repo = TaskAttemptRepository::new(db.clone());
    let in_flight = repo
        .latest_pending_or_submitted(&task.id, Some("worker"))
        .await
        .unwrap();
    assert!(
        in_flight.is_none(),
        "deferred rows must not be visible as pending/submitted"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_ordering_pending_before_submitted() {
    let db = test_db();
    let task = create_task(&db).await;

    // Create a pending attempt.
    let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
    super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
        .await
        .unwrap();

    // Guard must defer (pending exists).
    let decision = run_respawn_guard(&db, &task.id, "worker", None, None).await;
    assert_eq!(
        decision,
        RespawnGuardDecision::Defer(GuardReason::RespawnGuard)
    );
}

// ─── Open-PR adoption guard tests ───────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_adopts_when_pr_url_present() {
    let db = test_db();
    let task = create_task(&db).await;

    let decision = run_respawn_guard(
        &db,
        &task.id,
        "worker",
        Some("https://github.example/owner/repo/pull/42"),
        None,
    )
    .await;
    assert_eq!(
        decision,
        RespawnGuardDecision::Adopted {
            pr_url: "https://github.example/owner/repo/pull/42".to_owned(),
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_ignores_empty_pr_url() {
    let db = test_db();
    let task = create_task(&db).await;

    // An empty pr_url should not trigger adoption.
    let decision = run_respawn_guard(&db, &task.id, "worker", Some(""), None).await;
    assert_eq!(decision, RespawnGuardDecision::Allow);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_pr_adoption_takes_precedence_over_pending_attempt() {
    let db = test_db();
    let task = create_task(&db).await;

    // Insert a pending attempt.
    let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
    super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
        .await
        .expect("record_dispatch_start should succeed");

    // Even with a pending attempt, if pr_url is set, the guard adopts.
    let decision = run_respawn_guard(
        &db,
        &task.id,
        "worker",
        Some("https://github.example/owner/repo/pull/42"),
        None,
    )
    .await;
    assert_eq!(
        decision,
        RespawnGuardDecision::Adopted {
            pr_url: "https://github.example/owner/repo/pull/42".to_owned(),
        }
    );
}

// ─── PrReworkSignal derivation tests ─────────────────────────────────

const CONFLICT_METADATA: &str = r#"{"conflicting_files":["src/note/mod.rs"],"base_branch":"main","merge_target":"main"}"#;

#[test]
fn rework_signal_from_task_row_mapping() {
    // Failing required CI wins regardless of conflict metadata.
    assert_eq!(
        PrReworkSignal::from_task_row(CiStatus::Failing.as_str(), None),
        Some(PrReworkSignal::FailingCi)
    );
    assert_eq!(
        PrReworkSignal::from_task_row(CiStatus::Failing.as_str(), Some(CONFLICT_METADATA)),
        Some(PrReworkSignal::FailingCi)
    );
    // Populated conflict metadata signals rework even with green CI.
    assert_eq!(
        PrReworkSignal::from_task_row(CiStatus::Passing.as_str(), Some(CONFLICT_METADATA)),
        Some(PrReworkSignal::MergeConflict)
    );
    assert_eq!(
        PrReworkSignal::from_task_row(CiStatus::Pending.as_str(), Some(CONFLICT_METADATA)),
        Some(PrReworkSignal::MergeConflict)
    );
    // Healthy: green/pending/unknown CI and no (or blank) metadata.
    for ci in [
        CiStatus::Passing.as_str(),
        CiStatus::Pending.as_str(),
        CiStatus::Unknown.as_str(),
    ] {
        assert_eq!(PrReworkSignal::from_task_row(ci, None), None);
        assert_eq!(PrReworkSignal::from_task_row(ci, Some("")), None);
        assert_eq!(PrReworkSignal::from_task_row(ci, Some("   ")), None);
    }
}

// ─── CI-remediation adoption bypass tests ───────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_does_not_adopt_when_ci_gate_failing() {
    let db = test_db();
    let task = create_task(&db).await;

    // Open PR + failing required CI (PrCiFailed remediation reopen): the
    // guard must NOT adopt.  With no pending attempt it must Allow so a
    // remediation worker dispatches.
    let decision = run_respawn_guard(
        &db,
        &task.id,
        "worker",
        Some("https://github.example/owner/repo/pull/42"),
        PrReworkSignal::from_task_row(CiStatus::Failing.as_str(), None),
    )
    .await;
    assert_eq!(decision, RespawnGuardDecision::Allow);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_defers_when_ci_gate_failing_and_pending_attempt_exists() {
    let db = test_db();
    let task = create_task(&db).await;

    // A remediation worker is already in flight (pending attempt).
    let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
    super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
        .await
        .expect("record_dispatch_start should succeed");

    // Open PR + failing required CI: adoption is bypassed, but step 2
    // still defers so no duplicate remediation worker is dispatched.
    let decision = run_respawn_guard(
        &db,
        &task.id,
        "worker",
        Some("https://github.example/owner/repo/pull/42"),
        PrReworkSignal::from_task_row(CiStatus::Failing.as_str(), None),
    )
    .await;
    assert_eq!(
        decision,
        RespawnGuardDecision::Defer(GuardReason::RespawnGuard)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_adopts_when_ci_gate_green_pending_or_absent() {
    let db = test_db();
    let task = create_task(&db).await;

    // Any non-failing CI gate state (and no conflict metadata) preserves
    // the adoption behavior: the PR is merely awaiting review/merge, so
    // no duplicate worker spawns.
    for ci_status in [
        CiStatus::Passing.as_str(),
        CiStatus::Pending.as_str(),
        CiStatus::Unknown.as_str(),
    ] {
        let decision = run_respawn_guard(
            &db,
            &task.id,
            "worker",
            Some("https://github.example/owner/repo/pull/42"),
            PrReworkSignal::from_task_row(ci_status, None),
        )
        .await;
        assert_eq!(
            decision,
            RespawnGuardDecision::Adopted {
                pr_url: "https://github.example/owner/repo/pull/42".to_owned(),
            },
            "ci_status={ci_status:?} must still adopt"
        );
    }
}

// ─── Merge-conflict adoption bypass tests (PrConflict reopen) ────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_does_not_adopt_when_merge_conflict_metadata_present() {
    let db = test_db();
    let task = create_task(&db).await;

    // Open PR + populated merge_conflict_metadata + green CI (PrConflict
    // reopen: the PR conflicts with main while its own checks pass): the
    // guard must NOT adopt.  With no pending attempt it must Allow so a
    // conflict-resolution worker dispatches.
    let decision = run_respawn_guard(
        &db,
        &task.id,
        "worker",
        Some("https://github.example/owner/repo/pull/42"),
        PrReworkSignal::from_task_row(CiStatus::Passing.as_str(), Some(CONFLICT_METADATA)),
    )
    .await;
    assert_eq!(decision, RespawnGuardDecision::Allow);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_defers_when_merge_conflict_and_pending_attempt_exists() {
    let db = test_db();
    let task = create_task(&db).await;

    // A conflict-resolution worker is already in flight (pending attempt).
    let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
    super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
        .await
        .expect("record_dispatch_start should succeed");

    // Open PR + merge conflict: adoption is bypassed, but step 2 still
    // defers so no duplicate conflict-resolution worker is dispatched.
    let decision = run_respawn_guard(
        &db,
        &task.id,
        "worker",
        Some("https://github.example/owner/repo/pull/42"),
        PrReworkSignal::from_task_row(CiStatus::Passing.as_str(), Some(CONFLICT_METADATA)),
    )
    .await;
    assert_eq!(
        decision,
        RespawnGuardDecision::Defer(GuardReason::RespawnGuard)
    );
}

// ─── Latest-attempt rework fallback tests ────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_does_not_adopt_when_latest_attempt_reopened() {
    let db = test_db();
    let task = create_task(&db).await;

    // Simulate the PrChangesRequested / merge-queue-dequeue flow: the PR
    // poller terminalizes the in-flight worker attempt as `reopened`
    // before reopening the task.  No task-row rework column exists for
    // these paths, so the guard's latest-attempt fallback must bypass
    // adoption and Allow a rework worker.
    let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
    super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
        .await
        .expect("record_dispatch_start should succeed");
    super::super::attempt_lifecycle::advance_latest_to_terminal(
        &db,
        super::super::attempt_lifecycle::TerminalAdvancementParams {
            task_id: &task.id,
            role: "worker",
            outcome: djinn_core::models::task_attempt::TaskAttemptOutcome::Reopened,
            pr_url: Some("https://github.example/owner/repo/pull/42"),
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("Reviewer requested changes on PR"),
            summary_json: None,
            log_tail: None,
        },
    )
    .await;

    let decision = run_respawn_guard(
        &db,
        &task.id,
        "worker",
        Some("https://github.example/owner/repo/pull/42"),
        None,
    )
    .await;
    assert_eq!(decision, RespawnGuardDecision::Allow);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_reopened_fallback_ignores_guard_only_audit_rows() {
    let db = test_db();
    let task = create_task(&db).await;

    // A `reopened` attempt followed by guard-only audit rows from earlier
    // wedged ticks (adopted_pr / deferred, exactly what the starved
    // production tasks accumulated) must still bypass adoption: guard
    // rows must not mask the rework signal.
    let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
    super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
        .await
        .expect("record_dispatch_start should succeed");
    super::super::attempt_lifecycle::advance_latest_to_terminal(
        &db,
        super::super::attempt_lifecycle::TerminalAdvancementParams {
            task_id: &task.id,
            role: "worker",
            outcome: djinn_core::models::task_attempt::TaskAttemptOutcome::Reopened,
            pr_url: Some("https://github.example/owner/repo/pull/42"),
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("reopened for rework"),
            summary_json: None,
            log_tail: None,
        },
    )
    .await;
    record_adopted_pr_attempt(
        &db,
        &task.id,
        "worker",
        "https://github.example/owner/repo/pull/42",
        Some("stale adoption from a wedged tick"),
    )
    .await
    .expect("adopted_pr audit row should insert");
    record_guard_deferred_attempt(
        &db,
        &task.id,
        "worker",
        GuardReason::RespawnGuard,
        Some("stale deferral"),
    )
    .await
    .expect("deferred audit row should insert");

    let decision = run_respawn_guard(
        &db,
        &task.id,
        "worker",
        Some("https://github.example/owner/repo/pull/42"),
        None,
    )
    .await;
    assert_eq!(decision, RespawnGuardDecision::Allow);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_adopts_when_latest_attempt_completed() {
    let db = test_db();
    let task = create_task(&db).await;

    // A `completed`-latest attempt means the PR is healthy work-in-review
    // (e.g. the 422-adopt backstop): adoption must be preserved even
    // though an older attempt cycle could have been reopened before.
    let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
    super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
        .await
        .expect("record_dispatch_start should succeed");
    super::super::attempt_lifecycle::advance_latest_to_terminal(
        &db,
        super::super::attempt_lifecycle::TerminalAdvancementParams {
            task_id: &task.id,
            role: "worker",
            outcome: djinn_core::models::task_attempt::TaskAttemptOutcome::Completed,
            pr_url: Some("https://github.example/owner/repo/pull/42"),
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("done"),
            summary_json: None,
            log_tail: None,
        },
    )
    .await;

    let decision = run_respawn_guard(
        &db,
        &task.id,
        "worker",
        Some("https://github.example/owner/repo/pull/42"),
        None,
    )
    .await;
    assert_eq!(
        decision,
        RespawnGuardDecision::Adopted {
            pr_url: "https://github.example/owner/repo/pull/42".to_owned(),
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_reopened_fallback_is_role_scoped() {
    let db = test_db();
    let task = create_task(&db).await;

    // A `reopened` attempt for a DIFFERENT role must not bypass adoption
    // for this role.
    let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "reviewer");
    super::super::attempt_lifecycle::record_dispatch_start(
        &db, &task.id, "reviewer", None, &dk,
    )
    .await
    .expect("record_dispatch_start should succeed");
    super::super::attempt_lifecycle::advance_latest_to_terminal(
        &db,
        super::super::attempt_lifecycle::TerminalAdvancementParams {
            task_id: &task.id,
            role: "reviewer",
            outcome: djinn_core::models::task_attempt::TaskAttemptOutcome::Reopened,
            pr_url: None,
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("reviewer attempt reopened"),
            summary_json: None,
            log_tail: None,
        },
    )
    .await;

    let decision = run_respawn_guard(
        &db,
        &task.id,
        "worker",
        Some("https://github.example/owner/repo/pull/42"),
        None,
    )
    .await;
    assert_eq!(
        decision,
        RespawnGuardDecision::Adopted {
            pr_url: "https://github.example/owner/repo/pull/42".to_owned(),
        }
    );
}

// ─── Worker-role-only adoption tests (task 4tl2) ────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_does_not_adopt_for_reviewer_role_with_open_pr() {
    let db = test_db();
    let task = create_task(&db).await;

    // A reviewer dispatch for a task that already carries an open PR (the
    // needs_task_review shape) must NOT be adopted — adoption is worker-role
    // only.  With no in-flight reviewer attempt the guard must Allow so the
    // reviewer actually runs (task 4tl2: reviewer was starved by adoption).
    let decision = run_respawn_guard(
        &db,
        &task.id,
        "reviewer",
        Some("https://github.example/owner/repo/pull/42"),
        None,
    )
    .await;
    assert_eq!(decision, RespawnGuardDecision::Allow);

    // The guard itself records no attempt rows for an Allow.
    let repo = TaskAttemptRepository::new(db);
    let all = repo.list_for_task(&task.id).await.unwrap();
    assert!(all.is_empty(), "reviewer guard must not create attempt rows");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_defers_reviewer_role_when_reviewer_attempt_in_flight() {
    let db = test_db();
    let task = create_task(&db).await;

    // With adoption skipped for reviewers, the step-2 in-flight dedup still
    // prevents a duplicate reviewer even when an open PR is present.
    let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "reviewer");
    super::super::attempt_lifecycle::record_dispatch_start(
        &db, &task.id, "reviewer", None, &dk,
    )
    .await
    .expect("record_dispatch_start should succeed");

    let decision = run_respawn_guard(
        &db,
        &task.id,
        "reviewer",
        Some("https://github.example/owner/repo/pull/42"),
        None,
    )
    .await;
    assert_eq!(
        decision,
        RespawnGuardDecision::Defer(GuardReason::RespawnGuard)
    );
}

// ─── Rework-marker gate tests (ylme / kv6i) ─────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rework_reopen_terminalizes_inflight_submitted_and_guard_allows_worker() {
    let db = test_db();
    let task = create_task(&db).await;

    // Worker submitted its work (task-review stage): pending → submitted.
    let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
    super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
        .await
        .expect("record_dispatch_start should succeed");
    super::super::attempt_lifecycle::advance_to_submitted(
        &db,
        super::super::attempt_lifecycle::SubmitAdvancementParams {
            task_id: &task.id,
            role: "worker",
            submit_ref: Some("ref-1"),
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("submitted for review"),
            summary_json: None,
        },
    )
    .await;

    // Reviewer rejects (task_review_reject application path): the in-flight
    // submitted worker attempt must be terminalized to `reopened`.
    super::super::attempt_lifecycle::record_rework_reopen(
        &db,
        &task.id,
        "worker",
        None,
        Some("review rejected"),
        None,
    )
    .await;

    let repo = TaskAttemptRepository::new(db.clone());
    let terminalized = repo.get_by_dispatch_key(&dk).await.unwrap().unwrap();
    assert_eq!(
        terminalized.outcome, "reopened",
        "the submitted worker attempt must terminalize to reopened"
    );

    // No marker row is inserted when an in-flight attempt was terminalized.
    let all = repo.list_for_task(&task.id).await.unwrap();
    assert_eq!(
        all.len(),
        1,
        "no extra marker row when in-flight attempt was terminalized"
    );

    // The guard now Allows a worker dispatch even though pr_url is set: the
    // reopened latest attempt is the rework signal.
    let decision = run_respawn_guard(
        &db,
        &task.id,
        "worker",
        Some("https://github.example/owner/repo/pull/7"),
        None,
    )
    .await;
    assert_eq!(decision, RespawnGuardDecision::Allow);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rework_reopen_inserts_marker_when_no_inflight_and_guard_allows_then_defers() {
    let db = test_db();
    let task = create_task(&db).await;

    // kv6i: a rework reopen fires while NO attempt is in flight (nothing to
    // terminalize) — a durable `reopened` marker row must be inserted so the
    // guard still sees the task as rework.
    super::super::attempt_lifecycle::record_rework_reopen(
        &db,
        &task.id,
        "worker",
        Some("https://github.example/owner/repo/pull/9"),
        Some("changes requested, no live attempt"),
        None,
    )
    .await;

    let repo = TaskAttemptRepository::new(db.clone());
    let all = repo.list_for_task(&task.id).await.unwrap();
    assert_eq!(all.len(), 1, "a rework marker row must be inserted");
    assert_eq!(all[0].outcome, "reopened");
    assert_eq!(
        all[0].dispatch_key,
        super::super::attempt_lifecycle::rework_marker_dispatch_key(&task.id, "worker")
    );

    // Idempotent: a second reopen with still no in-flight attempt does not
    // stack a second marker (latest is already reopened).
    super::super::attempt_lifecycle::record_rework_reopen(
        &db,
        &task.id,
        "worker",
        None,
        Some("still reworking"),
        None,
    )
    .await;
    assert_eq!(
        repo.list_for_task(&task.id).await.unwrap().len(),
        1,
        "marker insert must be idempotent"
    );

    // Guard Allows a worker dispatch: the marker is the rework signal.
    let decision = run_respawn_guard(
        &db,
        &task.id,
        "worker",
        Some("https://github.example/owner/repo/pull/9"),
        None,
    )
    .await;
    assert_eq!(decision, RespawnGuardDecision::Allow);

    // The rework worker dispatches: a fresh pending row lands (newer than
    // the marker), so the marker is "consumed" — the latest attempt is no
    // longer `reopened`.  A subsequent pass no longer Allows a duplicate:
    // with the open PR present, step-1 adoption resumes (dedup — matching
    // `guard_pr_adoption_takes_precedence_over_pending_attempt`).
    let dk = super::super::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
    super::super::attempt_lifecycle::record_dispatch_start(&db, &task.id, "worker", None, &dk)
        .await
        .expect("record_dispatch_start should succeed");
    let decision2 = run_respawn_guard(
        &db,
        &task.id,
        "worker",
        Some("https://github.example/owner/repo/pull/9"),
        None,
    )
    .await;
    assert_eq!(
        decision2,
        RespawnGuardDecision::Adopted {
            pr_url: "https://github.example/owner/repo/pull/9".to_owned(),
        },
        "marker consumed: with the PR still open the guard adopts rather than re-dispatching"
    );

    // And when no PR is presented, the step-2 in-flight dedup defers on the
    // live pending attempt directly.
    let decision3 = run_respawn_guard(&db, &task.id, "worker", None, None).await;
    assert_eq!(
        decision3,
        RespawnGuardDecision::Defer(GuardReason::RespawnGuard)
    );
}

// ─── record_adopted_pr_attempt tests ────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopted_pr_creates_audit_row() {
    let db = test_db();
    let task = create_task(&db).await;

    let attempt_id = record_adopted_pr_attempt(
        &db,
        &task.id,
        "worker",
        "https://github.example/owner/repo/pull/42",
        Some("adopted existing PR"),
    )
    .await
    .expect("should return attempt id");

    let repo = TaskAttemptRepository::new(db);
    let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(attempt.task_id, task.id);
    assert_eq!(attempt.role, "worker");
    assert_eq!(attempt.outcome, "adopted_pr");
    assert_eq!(
        attempt.guard_decision_enum().unwrap(),
        Some(GuardDecision::Allow)
    );
    assert_eq!(
        attempt.guard_reason.as_deref(),
        Some(GuardReason::OpenPrAdoption.as_str())
    );
    assert_eq!(
        attempt.pr_url.as_deref(),
        Some("https://github.example/owner/repo/pull/42")
    );
    assert_eq!(attempt.summary.as_deref(), Some("adopted existing PR"));
    // Session_id must be NULL for guard-only rows.
    assert!(attempt.session_id.is_none());
    // Terminal_at must be set (adopted_pr is terminal).
    assert!(attempt.terminal_at.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopted_pr_idempotent_on_same_task_role() {
    let db = test_db();
    let task = create_task(&db).await;

    // Both calls use the same deterministic dispatch key, so the second
    // call should be a no-op (ON CONFLICT DO NOTHING).
    let id1 = record_adopted_pr_attempt(
        &db,
        &task.id,
        "worker",
        "https://github.example/owner/repo/pull/42",
        Some("first adoption"),
    )
    .await;
    let id2 = record_adopted_pr_attempt(
        &db,
        &task.id,
        "worker",
        "https://github.example/owner/repo/pull/42",
        Some("second adoption"),
    )
    .await;

    // Both should return the same id (idempotent).
    assert!(id1.is_some());
    assert!(id2.is_some());
    assert_eq!(id1, id2);

    let repo = TaskAttemptRepository::new(db);
    let all = repo.list_for_task(&task.id).await.unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].outcome, "adopted_pr");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopted_pr_row_is_not_pending_or_submitted() {
    let db = test_db();
    let task = create_task(&db).await;

    // Record an adopted-PR row.
    record_adopted_pr_attempt(
        &db,
        &task.id,
        "worker",
        "https://github.example/owner/repo/pull/42",
        Some("test"),
    )
    .await
    .expect("should succeed");

    // The adopted_pr row must NOT be visible as pending/submitted to the
    // guard — only pending/submitted rows should block dispatch.
    let repo = TaskAttemptRepository::new(db.clone());
    let in_flight = repo
        .latest_pending_or_submitted(&task.id, Some("worker"))
        .await
        .unwrap();
    assert!(
        in_flight.is_none(),
        "adopted_pr rows must not be visible as pending/submitted"
    );
}

// ─── 422 backstop regression: existing behavior preserved ───────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_allows_when_no_pr_url_and_no_pending() {
    let db = test_db();
    let task = create_task(&db).await;

    // Simulates the case where the 422 backstop in supervisor_pr_open
    // would handle a race: the task has no pr_url yet and no pending
    // attempt, so the guard allows and the spawn path proceeds.
    let decision = run_respawn_guard(&db, &task.id, "worker", None, None).await;
    assert_eq!(decision, RespawnGuardDecision::Allow);
}
