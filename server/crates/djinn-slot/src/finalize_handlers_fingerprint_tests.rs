use std::sync::{Arc, Mutex};

use crate::finalize_handlers::{
    process_finalize_payload, process_finalize_payload_with_outcome,
    record_rejected_integrity_entry,
};
use crate::test_helpers;
use djinn_db::repositories::task_run::TaskRunRepository;
use djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository;
use djinn_git::{
    ResolvedExternalInputV1, VerificationInputFingerprint, VerificationInputFingerprintConfig,
    compute_verification_input_fingerprint_with_config,
};

fn init_git_repo_with_dirty_file() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("djinn-test-git-")
        .tempdir()
        .expect("create temp dir");
    let run_git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run_git(&["init"]);
    run_git(&["config", "--local", "user.email", "test@test.com"]);
    run_git(&["config", "--local", "user.name", "Test User"]);
    run_git(&["config", "--local", "commit.gpgsign", "false"]);
    std::fs::write(dir.path().join("README.md"), "hello\n").expect("write readme");
    run_git(&["add", "README.md"]);
    run_git(&["commit", "-m", "init"]);
    run_git(&["branch", "-m", "main"]);
    // Make a dirty tracked edit so the fingerprint computes a Diff.
    std::fs::write(dir.path().join("README.md"), "hello\ndirty\n").expect("write dirty");
    dir
}

async fn create_run_with_workspace(
    db: &djinn_db::Database,
    project_id: &str,
    task_id: &str,
    workspace_path: Option<&str>,
) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(djinn_db::repositories::task_run::CreateTaskRunParams {
            id: &id,
            project_id,
            task_id,
            trigger_type: djinn_core::models::TaskRunTrigger::NewTask.as_str(),
            status: None,
            workspace_path,
            mirror_ref: None,
        })
        .await
        .expect("create task run");
    id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_review_records_fingerprint_when_worktree_has_diff() {
    let crate::test_helpers::ContextFixture {
        db,
        ctx,
        project,
        epic: _,
        task,
    } = crate::test_helpers::seed_context_fixture().await;
    let worktree = init_git_repo_with_dirty_file();
    let _run_id = create_run_with_workspace(
        &db,
        &project.id,
        &task.id,
        Some(worktree.path().to_str().unwrap()),
    )
    .await;
    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "verdict": "rejected",
        "acceptance_criteria": [],
        "feedback": "needs work"
    }));
    process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;
    let integrity_repo = TaskRejectedSubmissionIntegrityRepository::new(db);
    let latest = integrity_repo
        .latest_for_task(&task.id)
        .await
        .unwrap()
        .expect("rejected review with dirty worktree must record a fingerprint");
    assert!(
        !latest.diff_fingerprint.is_empty(),
        "fingerprint must be non-empty"
    );
    assert_eq!(latest.no_progress_streak, 1);
    assert!(latest.task_run_id.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_review_skips_persistence_when_worktree_is_nodiff() {
    let crate::test_helpers::ContextFixture {
        db,
        ctx,
        project,
        epic: _,
        task,
    } = crate::test_helpers::seed_context_fixture().await;
    // Create a clean git repo with no dirty changes.
    let dir = tempfile::Builder::new()
        .prefix("djinn-test-nodiff-")
        .tempdir()
        .expect("create temp dir");
    let run_git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run_git(&["init"]);
    run_git(&["config", "--local", "user.email", "test@test.com"]);
    run_git(&["config", "--local", "user.name", "Test User"]);
    run_git(&["config", "--local", "commit.gpgsign", "false"]);
    std::fs::write(dir.path().join("README.md"), "hello\n").expect("write");
    run_git(&["add", "README.md"]);
    run_git(&["commit", "-m", "init"]);
    run_git(&["branch", "-m", "main"]);
    // No dirty edits — NoDiff.
    let _run_id = create_run_with_workspace(
        &db,
        &project.id,
        &task.id,
        Some(dir.path().to_str().unwrap()),
    )
    .await;
    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "verdict": "rejected",
        "acceptance_criteria": [],
        "feedback": "needs work but no diff"
    }));
    process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;
    let integrity_repo = TaskRejectedSubmissionIntegrityRepository::new(db);
    let latest = integrity_repo.latest_for_task(&task.id).await.unwrap();
    assert!(
        latest.is_none(),
        "NoDiff worktree must not produce a rejected fingerprint record"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_review_skips_persistence_when_no_worktree() {
    let crate::test_helpers::ContextFixture {
        db,
        ctx,
        project,
        epic: _,
        task,
    } = crate::test_helpers::seed_context_fixture().await;
    // Task run with no workspace_path.
    let _run_id = create_run_with_workspace(&db, &project.id, &task.id, None).await;
    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "verdict": "rejected",
        "acceptance_criteria": [],
        "feedback": "no worktree available"
    }));
    process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;
    let integrity_repo = TaskRejectedSubmissionIntegrityRepository::new(db);
    let latest = integrity_repo.latest_for_task(&task.id).await.unwrap();
    assert!(
        latest.is_none(),
        "no-worktree case must not produce a rejected fingerprint record"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_review_does_not_record_rejected_fingerprint() {
    let crate::test_helpers::ContextFixture {
        db,
        ctx,
        project,
        epic: _,
        task,
    } = crate::test_helpers::seed_context_fixture().await;
    let worktree = init_git_repo_with_dirty_file();
    let _run_id = create_run_with_workspace(
        &db,
        &project.id,
        &task.id,
        Some(worktree.path().to_str().unwrap()),
    )
    .await;
    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "verdict": "approved",
        "acceptance_criteria": [],
        "feedback": null
    }));
    process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;
    let integrity_repo = TaskRejectedSubmissionIntegrityRepository::new(db);
    let latest = integrity_repo.latest_for_task(&task.id).await.unwrap();
    assert!(
        latest.is_none(),
        "approved review must not record rejected fingerprint"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_fingerprint_persists_across_task_run_boundaries() {
    // Simulate: task run 1 records a rejected fingerprint, then a new
    // task run 2 is created (redispatch). The latest_for_task query
    // must still see the rejection from run 1.
    let crate::test_helpers::ContextFixture {
        db,
        ctx,
        project,
        epic: _,
        task,
    } = crate::test_helpers::seed_context_fixture().await;
    let worktree = init_git_repo_with_dirty_file();
    let run1_id = create_run_with_workspace(
        &db,
        &project.id,
        &task.id,
        Some(worktree.path().to_str().unwrap()),
    )
    .await;
    // Record the rejection via handle_submit_review.
    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "verdict": "rejected",
        "acceptance_criteria": [],
        "feedback": "needs work"
    }));
    process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;
    let integrity_repo = TaskRejectedSubmissionIntegrityRepository::new(db.clone());
    let latest = integrity_repo
        .latest_for_task(&task.id)
        .await
        .unwrap()
        .expect("rejection must be recorded");
    assert_eq!(latest.task_run_id.as_deref(), Some(run1_id.as_str()));
    assert_eq!(latest.no_progress_streak, 1);
    // Create a new task run (simulating redispatch).
    let _run2_id = create_run_with_workspace(
        &db,
        &project.id,
        &task.id,
        Some(worktree.path().to_str().unwrap()),
    )
    .await;
    // The latest rejection should still be from run 1 (cross-run persistence).
    let latest_after_redispatch = integrity_repo
        .latest_for_task(&task.id)
        .await
        .unwrap()
        .expect("must persist across task run boundaries");
    assert_eq!(
        latest_after_redispatch.task_run_id.as_deref(),
        Some(run1_id.as_str()),
        "rejection from run 1 must survive redispatch to run 2"
    );
    assert_eq!(
        latest_after_redispatch.diff_fingerprint,
        latest.diff_fingerprint
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_rejected_integrity_entry_direct_call_increments_streak() {
    // Test the shared helper directly: two consecutive rejections should
    // increment the streak from 0→1→2.
    let crate::test_helpers::ContextFixture {
        db,
        ctx,
        project,
        epic: _,
        task,
    } = crate::test_helpers::seed_context_fixture().await;
    let run_id = create_run_with_workspace(&db, &project.id, &task.id, None).await;
    // First rejection.
    record_rejected_integrity_entry(
        &task.id,
        &ctx,
        djinn_core::models::RejectedVerdictKind::ReviewerReject.as_str(),
        None,
        Some(&run_id),
        "sha256:first-reject",
    )
    .await;
    let integrity_repo = TaskRejectedSubmissionIntegrityRepository::new(db.clone());
    let latest = integrity_repo
        .latest_for_task(&task.id)
        .await
        .unwrap()
        .expect("first rejection must be recorded");
    assert_eq!(latest.no_progress_streak, 1);
    assert_eq!(latest.diff_fingerprint, "sha256:first-reject");
    // Second rejection (streak should be 2).
    record_rejected_integrity_entry(
        &task.id,
        &ctx,
        djinn_core::models::RejectedVerdictKind::ReviewerReject.as_str(),
        None,
        Some(&run_id),
        "sha256:second-reject",
    )
    .await;
    let latest2 = integrity_repo
        .latest_for_task(&task.id)
        .await
        .unwrap()
        .expect("second rejection must be recorded");
    assert_eq!(latest2.no_progress_streak, 2);
    assert_eq!(latest2.diff_fingerprint, "sha256:second-reject");
}

//
// These tests tie together accepted/auto-submit shared-fingerprint
// persistence and rejected task-level fingerprint reload semantics.
// Both paths must use comparable digest strings from the shared
// `djinn_git::compute_submission_diff_fingerprint` helper.

/// AC1: Accepted auto-submit persistence and rejected-review persistence
/// both produce comparable digest strings from the shared
/// `compute_submission_diff_fingerprint` helper.
///
/// Sets up a single temp git repo with dirty changes, then:
/// 1. Accepts a submission via the model-called `submit_work` path with
/// `auto_submit_review_metadata` — the finalize handler computes a
/// fingerprint from the worktree and persists it in
/// `auto_submit_reviews.diff_fingerprint`.
/// 2. Rejects a submission via the `submit_review` path with
/// `verdict: "rejected"` — the handler computes a fingerprint from the
/// **same** worktree and persists it in
/// `task_rejected_submission_integrity.diff_fingerprint`.
/// 3. Asserts both fingerprints are equal: the shared helper produces
/// deterministic digests for identical worktree state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_and_rejected_paths_store_comparable_fingerprints_from_shared_helper() {
    let db = crate::test_helpers::create_test_db();
    let ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let project = crate::test_helpers::create_test_project(&db).await;
    let epic = crate::test_helpers::create_test_epic(&db, &project.id).await;
    // Create a single shared worktree with dirty changes.
    let worktree = init_git_repo_with_dirty_file();
    let worktree_path = worktree.path().to_str().unwrap().to_string();
    let task_accepted = crate::test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    let run_accepted =
        create_run_with_workspace(&db, &project.id, &task_accepted.id, Some(&worktree_path)).await;
    // Compute the expected fingerprint directly from the shared helper.
    let expected_fp = djinn_git::compute_submission_diff_fingerprint(&worktree_path)
        .await
        .expect("compute fingerprint must succeed");
    let expected_digest = expected_fp
        .fingerprint()
        .expect("dirty worktree must produce a Diff fingerprint")
        .to_string();
    // Submit through the model-called path with auto_submit_review_metadata.
    let accepted_payload = Some(serde_json::json!({
        "task_id": task_accepted.short_id,
        "commit_title": "feat: accepted submission",
        "summary": "accepted with fingerprint",
        "files_changed": ["README.md"],
        "remaining_concerns": [],
        "auto_submit_review_metadata": {
            "task_run_id": run_accepted,
            "trigger_reason": "idle",
            "diff_fingerprint": expected_digest,
            "verify_source": "local",
            "verify_run_id": "local-run-1",
            "verify_timestamp": "2026-07-01T10:00:00.000Z",
            "session_id": "sess-accepted",
            "model_id": "model-1",
            "no_progress_streak": 0
        }
    }));
    let ok = process_finalize_payload_with_outcome(
        &accepted_payload,
        "submit_work",
        &task_accepted.id,
        &ctx,
    )
    .await;
    assert!(ok, "accepted submit must succeed");
    let accepted_records =
        djinn_db::repositories::verify_run::AutoSubmitReviewRepository::new(db.clone())
            .list_for_task_run(&run_accepted)
            .await
            .unwrap();
    assert_eq!(
        accepted_records.len(),
        1,
        "accepted path must persist one review record"
    );
    let accepted_fingerprint = &accepted_records[0].diff_fingerprint;
    assert_eq!(
        accepted_fingerprint, &expected_digest,
        "accepted fingerprint must match the shared helper digest"
    );
    let task_rejected = crate::test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    let _run_rejected =
        create_run_with_workspace(&db, &project.id, &task_rejected.id, Some(&worktree_path)).await;
    let rejected_payload = Some(serde_json::json!({
        "task_id": task_rejected.id,
        "verdict": "rejected",
        "acceptance_criteria": [],
        "feedback": "needs work"
    }));
    process_finalize_payload(&rejected_payload, "submit_review", &task_rejected.id, &ctx).await;
    let integrity_repo =
        djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository::new(db);
    let rejected_latest = integrity_repo
        .latest_for_task(&task_rejected.id)
        .await
        .unwrap()
        .expect("rejected path must record a fingerprint");
    let rejected_fingerprint = &rejected_latest.diff_fingerprint;
    assert_eq!(
        rejected_fingerprint, &expected_digest,
        "rejected fingerprint must match the shared helper digest"
    );
    assert_eq!(
        accepted_fingerprint, rejected_fingerprint,
        "accepted and rejected paths must produce identical fingerprints \
             from the same worktree via the shared compute_submission_diff_fingerprint helper"
    );
}

/// AC2: Reject a submission, then create a new task run (simulating
/// redispatch), and retrieve the latest rejected fingerprint by `task_id`.
///
/// The second task run has a *clean* worktree (no diff), so a new
/// submission would produce a different fingerprint from the rejected one.
/// This simulates the real redispatch scenario where the live guard needs
/// to compare the new submission against the stored rejected fingerprint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reject_then_redispatch_reloads_latest_fingerprint_by_task_id() {
    let crate::test_helpers::ContextFixture {
        db,
        ctx,
        project,
        epic: _,
        task,
    } = crate::test_helpers::seed_context_fixture().await;
    let worktree_run1 = init_git_repo_with_dirty_file();
    let run1_id = create_run_with_workspace(
        &db,
        &project.id,
        &task.id,
        Some(worktree_run1.path().to_str().unwrap()),
    )
    .await;
    // Compute the expected rejected fingerprint from the worktree.
    let rejected_fp = djinn_git::compute_submission_diff_fingerprint(worktree_run1.path())
        .await
        .expect("compute fingerprint for run 1");
    let rejected_digest = rejected_fp
        .fingerprint()
        .expect("dirty worktree must produce Diff")
        .to_string();
    // Record the rejection.
    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "verdict": "rejected",
        "acceptance_criteria": [],
        "feedback": "not enough tests"
    }));
    process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;
    let integrity_repo =
        djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository::new(
            db.clone(),
        );
    let latest_run1 = integrity_repo
        .latest_for_task(&task.id)
        .await
        .unwrap()
        .expect("rejection from run 1 must be recorded");
    assert_eq!(latest_run1.diff_fingerprint, rejected_digest);
    assert_eq!(latest_run1.task_run_id.as_deref(), Some(run1_id.as_str()));
    assert_eq!(latest_run1.no_progress_streak, 1);
    let worktree_run2 = tempfile::Builder::new()
        .prefix("djinn-test-run2-")
        .tempdir()
        .expect("create run 2 worktree");
    // Init a clean git repo with no dirty changes.
    let run_git = |dir: &std::path::Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run_git(worktree_run2.path(), &["init"]);
    run_git(
        worktree_run2.path(),
        &["config", "--local", "user.email", "test@test.com"],
    );
    run_git(
        worktree_run2.path(),
        &["config", "--local", "user.name", "Test User"],
    );
    run_git(
        worktree_run2.path(),
        &["config", "--local", "commit.gpgsign", "false"],
    );
    std::fs::write(worktree_run2.path().join("README.md"), "hello\n").expect("write");
    run_git(worktree_run2.path(), &["add", "README.md"]);
    run_git(worktree_run2.path(), &["commit", "-m", "init"]);
    run_git(worktree_run2.path(), &["branch", "-m", "main"]);
    // No dirty edits — clean worktree.
    let run2_id = create_run_with_workspace(
        &db,
        &project.id,
        &task.id,
        Some(worktree_run2.path().to_str().unwrap()),
    )
    .await;
    // ── Reload: the latest_for_task query must still see run 1's rejection.
    let latest_after_redispatch = integrity_repo
        .latest_for_task(&task.id)
        .await
        .unwrap()
        .expect("rejection must persist across task run boundaries");
    assert_eq!(
        latest_after_redispatch.task_run_id.as_deref(),
        Some(run1_id.as_str()),
        "latest_for_task must return run 1's rejection, not run 2"
    );
    assert_eq!(
        latest_after_redispatch.diff_fingerprint, rejected_digest,
        "fingerprint from run 1 must survive into run 2's query"
    );
    assert_eq!(
        latest_after_redispatch.no_progress_streak, 1,
        "streak from run 1 must survive into run 2"
    );
    // The new run (run2_id) is different from the rejection's run.
    assert_ne!(
        latest_after_redispatch.task_run_id.as_deref(),
        Some(run2_id.as_str()),
        "rejection must be associated with run 1, not run 2"
    );
}

/// AC3: Historical/unavailable worktree behavior — no workspace_path.
///
/// When the task run has no `workspace_path` (historical or never-assigned),
/// rejected-fingerprint persistence is skipped, `latest_for_task` remains
/// absent, and a structured event is emitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn historical_no_worktree_skips_rejected_fingerprint_and_emits_event() {
    let db = crate::test_helpers::create_test_db();
    let events = Arc::new(Mutex::new(
        Vec::<djinn_core::events::DjinnEventEnvelope>::new(),
    ));
    let mut ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    {
        let events = Arc::clone(&events);
        ctx.event_bus = djinn_core::events::EventBus::new(move |event| {
            events.lock().expect("events mutex").push(event);
        });
    }
    let project = crate::test_helpers::create_test_project(&db).await;
    let epic = crate::test_helpers::create_test_epic(&db, &project.id).await;
    let task = crate::test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    // Task run with NO workspace_path (historical case).
    let _run_id = create_run_with_workspace(&db, &project.id, &task.id, None).await;
    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "verdict": "rejected",
        "acceptance_criteria": [],
        "feedback": "historical run, no workspace"
    }));
    process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;
    // The fingerprint must not be recorded.
    let integrity_repo =
        djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository::new(db);
    let latest = integrity_repo.latest_for_task(&task.id).await.unwrap();
    assert!(
        latest.is_none(),
        "historical/no-worktree case must not produce a rejected fingerprint"
    );
    // A structured event must be emitted indicating the skip.
    let evts = events.lock().expect("events mutex");
    let unavailable_events: Vec<_> = evts
        .iter()
        .filter(|e| e.action == "submission_fingerprint_unavailable")
        .collect();
    assert!(
        !unavailable_events.is_empty(),
        "must emit submission_fingerprint_unavailable event for no-worktree case"
    );
    // The event payload must reference the reason.
    let payload = &unavailable_events[0].payload;
    assert!(
        payload["reason"]
            .as_str()
            .unwrap_or("")
            .contains("unavailable"),
        "unavailable event must carry workspace_unavailable reason, got: {}",
        payload["reason"]
    );
}

/// AC3: Historical/unavailable worktree behavior — workspace_path does not exist on disk.
///
/// When the task run has a `workspace_path` but the directory has been
/// deleted (e.g. after a cleanup or eviction), rejected-fingerprint
/// persistence is skipped, `latest_for_task` remains absent, and a
/// structured event is emitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nonexistent_worktree_path_skips_rejected_fingerprint_and_emits_event() {
    let db = crate::test_helpers::create_test_db();
    let events = Arc::new(Mutex::new(
        Vec::<djinn_core::events::DjinnEventEnvelope>::new(),
    ));
    let mut ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    {
        let events = Arc::clone(&events);
        ctx.event_bus = djinn_core::events::EventBus::new(move |event| {
            events.lock().expect("events mutex").push(event);
        });
    }
    let project = crate::test_helpers::create_test_project(&db).await;
    let epic = crate::test_helpers::create_test_epic(&db, &project.id).await;
    let task = crate::test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    // Task run with a workspace_path that does not exist on disk.
    let _run_id = create_run_with_workspace(
        &db,
        &project.id,
        &task.id,
        Some("/nonexistent/path/to/workspace"),
    )
    .await;
    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "verdict": "rejected",
        "acceptance_criteria": [],
        "feedback": "workspace deleted after cleanup"
    }));
    process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;
    // The fingerprint must not be recorded.
    let integrity_repo =
        djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository::new(db);
    let latest = integrity_repo.latest_for_task(&task.id).await.unwrap();
    assert!(
        latest.is_none(),
        "nonexistent worktree path must not produce a rejected fingerprint"
    );
    // A structured event must be emitted indicating the skip.
    let evts = events.lock().expect("events mutex");
    let unavailable_events: Vec<_> = evts
        .iter()
        .filter(|e| e.action == "submission_fingerprint_unavailable")
        .collect();
    assert!(
        !unavailable_events.is_empty(),
        "must emit submission_fingerprint_unavailable event for nonexistent worktree"
    );
}

/// AC3 (supplemental): When the worktree exists but has no diff (NoDiff),
/// the rejected fingerprint persistence is skipped, `latest_for_task`
/// remains absent, and a structured event with reason "no_diff" is emitted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_worktree_skips_rejected_fingerprint_with_nodiff_event() {
    let db = crate::test_helpers::create_test_db();
    let events = Arc::new(Mutex::new(
        Vec::<djinn_core::events::DjinnEventEnvelope>::new(),
    ));
    let mut ctx =
        test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    {
        let events = Arc::clone(&events);
        ctx.event_bus = djinn_core::events::EventBus::new(move |event| {
            events.lock().expect("events mutex").push(event);
        });
    }
    let project = crate::test_helpers::create_test_project(&db).await;
    let epic = crate::test_helpers::create_test_epic(&db, &project.id).await;
    let task = crate::test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    // Create a clean git repo with no dirty changes.
    let dir = tempfile::Builder::new()
        .prefix("djinn-test-nodiff-event-")
        .tempdir()
        .expect("create temp dir");
    let run_git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run_git(&["init"]);
    run_git(&["config", "--local", "user.email", "test@test.com"]);
    run_git(&["config", "--local", "user.name", "Test User"]);
    run_git(&["config", "--local", "commit.gpgsign", "false"]);
    std::fs::write(dir.path().join("README.md"), "hello\n").expect("write");
    run_git(&["add", "README.md"]);
    run_git(&["commit", "-m", "init"]);
    run_git(&["branch", "-m", "main"]);
    // No dirty edits — NoDiff.
    let _run_id = create_run_with_workspace(
        &db,
        &project.id,
        &task.id,
        Some(dir.path().to_str().unwrap()),
    )
    .await;
    let payload = Some(serde_json::json!({
        "task_id": task.id,
        "verdict": "rejected",
        "acceptance_criteria": [],
        "feedback": "clean worktree, no diff"
    }));
    process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;
    let integrity_repo =
        djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository::new(db);
    let latest = integrity_repo.latest_for_task(&task.id).await.unwrap();
    assert!(
        latest.is_none(),
        "NoDiff worktree must not produce a rejected fingerprint"
    );
    // The submission_fingerprint_unavailable event must be emitted with "no_diff".
    let evts = events.lock().expect("events mutex");
    let unavailable_events: Vec<_> = evts
        .iter()
        .filter(|e| e.action == "submission_fingerprint_unavailable")
        .collect();
    assert!(
        !unavailable_events.is_empty(),
        "must emit submission_fingerprint_unavailable event for NoDiff case"
    );
    let payload = &unavailable_events[0].payload;
    assert_eq!(
        payload["reason"].as_str().unwrap_or(""),
        "no_diff",
        "NoDiff case must emit reason 'no_diff'"
    );
}

/// AC1 (supplemental): The accepted auto-submit path through
/// `settle_auto_submit_if_eligible` and the rejected settlement path both
/// persist fingerprints that come from the review event's
/// `diff_fingerprint` field (which was originally computed by the shared
/// `compute_submission_diff_fingerprint` helper upstream).
///
/// This test validates that both the eligible (accepted) and ineligible
/// (rejected) settlement paths store the same fingerprint when provided
/// the same review event diff.
#[tokio::test]
async fn settlement_accepted_and_rejected_paths_store_same_review_fingerprint() {
    use crate::output_parser::AutoSubmitSettlement;
    use djinn_core::auto_submit_decision::{
        AutoSubmitDecision, ReviewAutoSubmitDecisionEvent, VerifyFreshnessEvaluatedEvent,
    };
    use djinn_core::canonical_verify::FreshnessVerdict;
    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_core::models::{AutoSubmitTriggerReason, TaskRunTrigger, VerifyRunRecord};
    use djinn_db::repositories::task_run::CreateTaskRunParams;
    fn ctx_with_events(
        db: djinn_db::Database,
        events: Arc<Mutex<Vec<DjinnEventEnvelope>>>,
    ) -> crate::host::SlotContext {
        let mut ctx =
            test_helpers::agent_context_from_db(db, tokio_util::sync::CancellationToken::new());
        ctx.event_bus = EventBus::new(move |event| {
            events.lock().expect("events mutex").push(event);
        });
        ctx
    }
    fn make_settlement(
        task_run_id: &str,
        eligible: bool,
        fingerprint: &str,
    ) -> AutoSubmitSettlement {
        let decision = AutoSubmitDecision {
            eligible,
            trigger_reason: AutoSubmitTriggerReason::ControlledTermination,
            block_reason: None,
            freshness_verdict: FreshnessVerdict::accept(),
        };
        AutoSubmitSettlement {
            task_run_id: task_run_id.to_string(),
            decision,
            freshness_event: VerifyFreshnessEvaluatedEvent {
                diff_fingerprint: fingerprint.to_string(),
                has_verify_run: true,
                freshness_verdict: FreshnessVerdict::accept(),
                trigger_reason: AutoSubmitTriggerReason::ControlledTermination,
                submit_id: None,
            },
            review_event: ReviewAutoSubmitDecisionEvent {
                eligible,
                trigger_reason: AutoSubmitTriggerReason::ControlledTermination,
                block_reason: None,
                diff_fingerprint: fingerprint.to_string(),
                freshness_verdict: FreshnessVerdict::accept(),
                submit_id: None,
                session_id: Some("sess-compare".to_string()),
                model_id: Some("model-compare".to_string()),
                no_progress_streak: 0,
                model_called_submit_work: false,
            },
            verify_run: Some(VerifyRunRecord {
                id: "verify-compare".to_string(),
                task_run_id: task_run_id.to_string(),
                verify_source: "ci".to_string(),
                verify_run_id: "ci-compare".to_string(),
                command_version: None,
                profile_version: None,
                completed_at: "2026-07-01T00:00:00.000Z".to_string(),
                result: "pass".to_string(),
                diff_fingerprint: fingerprint.to_string(),
                check_coverage: None,
                created_at: "2026-07-01T00:00:01.000Z".to_string(),
                ..VerifyRunRecord::default()
            }),
            commit_title: Some("settlement compare".to_string()),
            summary: Some("settlement compare".to_string()),
            files_changed: vec!["README.md".to_string()],
            remaining_concerns: vec![],
        }
    }
    let db = crate::test_helpers::create_test_db();
    let events_a = Arc::new(Mutex::new(Vec::new()));
    let ctx_a = ctx_with_events(db.clone(), Arc::clone(&events_a));
    let project = crate::test_helpers::create_test_project(&db).await;
    let epic = crate::test_helpers::create_test_epic(&db, &project.id).await;
    let task_a = crate::test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    let run_a = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &run_a,
            project_id: &project.id,
            task_id: &task_a.id,
            trigger_type: TaskRunTrigger::NewTask.as_str(),
            status: None,
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .expect("create run_a");
    let shared_fingerprint = "sha256:shared-fp-from-helper";
    let mut output_a = crate::output_parser::ParsedAgentOutput::empty();
    output_a.auto_submit = Some(make_settlement(&run_a, true, shared_fingerprint));
    let outcome_a =
        crate::lifecycle::teardown::settle_auto_submit_if_eligible(&task_a.id, &ctx_a, &output_a)
            .await;
    assert_eq!(
        outcome_a,
        crate::lifecycle::teardown::AutoSubmitSettlementOutcome::Submitted
    );
    let review_records =
        djinn_db::repositories::verify_run::AutoSubmitReviewRepository::new(db.clone())
            .list_for_task_run(&run_a)
            .await
            .unwrap();
    assert_eq!(review_records.len(), 1, "accepted must persist review");
    assert_eq!(review_records[0].diff_fingerprint, shared_fingerprint);
    let events_r = Arc::new(Mutex::new(Vec::new()));
    let ctx_r = ctx_with_events(db.clone(), Arc::clone(&events_r));
    let task_r = crate::test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    let run_r = uuid::Uuid::now_v7().to_string();
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &run_r,
            project_id: &project.id,
            task_id: &task_r.id,
            trigger_type: TaskRunTrigger::NewTask.as_str(),
            status: None,
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .expect("create run_r");
    let mut output_r = crate::output_parser::ParsedAgentOutput::empty();
    output_r.auto_submit = Some(make_settlement(&run_r, false, shared_fingerprint));
    let outcome_r =
        crate::lifecycle::teardown::settle_auto_submit_if_eligible(&task_r.id, &ctx_r, &output_r)
            .await;
    assert_eq!(
        outcome_r,
        crate::lifecycle::teardown::AutoSubmitSettlementOutcome::Skipped
    );
    // Rejected path should NOT create an auto_submit_review (settlement skipped).
    let review_records_r =
        djinn_db::repositories::verify_run::AutoSubmitReviewRepository::new(db.clone())
            .list_for_task_run(&run_r)
            .await
            .unwrap();
    assert!(
        review_records_r.is_empty(),
        "rejected settlement must not persist review"
    );
    // But must persist in task_rejected_submission_integrity.
    let integrity_repo =
        djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository::new(db);
    let rejected_latest = integrity_repo
        .latest_for_task(&task_r.id)
        .await
        .unwrap()
        .expect("rejected settlement must record integrity entry");
    assert_eq!(
        rejected_latest.diff_fingerprint, shared_fingerprint,
        "rejected settlement fingerprint must match the shared fingerprint"
    );
    assert_eq!(
        review_records[0].diff_fingerprint, rejected_latest.diff_fingerprint,
        "accepted review and rejected integrity must store identical fingerprints \
         from the same shared submission fingerprint source"
    );
}

/// Complete C2 input fingerprint mutation matrix. These cases model evidence
/// captured at C1 and prove every declared input class changes the value the
/// completion validator must compare before consuming that evidence.
async fn c2_fingerprint(
    worktree: &std::path::Path,
    config: &VerificationInputFingerprintConfig,
) -> String {
    match compute_verification_input_fingerprint_with_config(worktree, config)
        .await
        .expect("compute C2 fingerprint")
    {
        VerificationInputFingerprint::Available(digest) => digest.fingerprint,
        VerificationInputFingerprint::Unavailable(reason) => panic!("C2 unavailable: {reason}"),
    }
}

async fn assert_after_c1_mutation_changes_complete_c2(
    name: &str,
    config: VerificationInputFingerprintConfig,
    mutate: impl FnOnce(&std::path::Path),
) {
    let worktree = init_git_repo_with_dirty_file();
    let c1 = c2_fingerprint(worktree.path(), &config).await;
    mutate(worktree.path());
    let c2 = c2_fingerprint(worktree.path(), &config).await;
    assert_ne!(c1, c2, "{name}: stale C1 evidence must not match C2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_c1_tracked_text_mutation_changes_c2() {
    assert_after_c1_mutation_changes_complete_c2(
        "tracked text",
        VerificationInputFingerprintConfig::default(),
        |root| std::fs::write(root.join("README.md"), "hello\nchanged after C1\n").unwrap(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_c1_ignored_generated_configuration_mutation_changes_c2() {
    assert_after_c1_mutation_changes_complete_c2(
        "ignored/generated configuration",
        VerificationInputFingerprintConfig::default(),
        |root| {
            std::fs::write(root.join(".gitignore"), "generated.conf\n").unwrap();
            std::fs::write(root.join("generated.conf"), "changed after C1\n").unwrap();
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_c1_untracked_binary_mutation_changes_c2() {
    assert_after_c1_mutation_changes_complete_c2(
        "untracked binary",
        VerificationInputFingerprintConfig::default(),
        |root| std::fs::write(root.join("generated.bin"), [0_u8, 255, 17, 42]).unwrap(),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_c1_submodule_state_mutation_changes_c2() {
    assert_after_c1_mutation_changes_complete_c2(
        "submodule state",
        VerificationInputFingerprintConfig::default(),
        |root| {
            std::fs::write(root.join(".gitmodules"), "[submodule \"vendor/input\"]\npath = vendor/input\nurl = https://example.invalid/input\n").unwrap();
            std::fs::create_dir_all(root.join("vendor/input")).unwrap();
            std::fs::write(root.join("vendor/input/.git"), "gitdir: ../.git/modules/input\n").unwrap();
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn after_c1_declared_external_input_mutation_changes_c2() {
    let external = tempfile::tempdir().unwrap();
    std::fs::write(external.path().join("toolchain.txt"), "v1\n").unwrap();
    let mut config = VerificationInputFingerprintConfig::default();
    config.manifest.read_only_external_inputs.push(
        djinn_core::canonical_verify::DeclaredExternalInputV1 {
            id: "toolchain".into(),
            locator: "host://toolchain".into(),
        },
    );
    config.external_inputs.push(ResolvedExternalInputV1 {
        id: "toolchain".into(),
        path: external.path().to_path_buf(),
    });
    let worktree = init_git_repo_with_dirty_file();
    let c1 = c2_fingerprint(worktree.path(), &config).await;
    std::fs::write(external.path().join("toolchain.txt"), "v2 after C1\n").unwrap();
    assert_ne!(
        c1,
        c2_fingerprint(worktree.path(), &config).await,
        "declared external input must be resolved at C2"
    );
}
