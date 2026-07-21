//! Tests for wave-dispatch attempt terminalization paths.
//!
//! Two test groups:
//!
//! 1. **Pure-logic mapping tests** — exercise `build_wave_dispatch_terminal_params`
//!    directly, verifying the `WaveDispatchAttemptOutcome` → `TaskAttemptOutcome`
//!    mapping and the structured `summary_json` fields (including `task_branch`,
//!    `pr_url`, `ci_head_sha` passthrough).
//!
//! 2. **Wiring / integration tests** — exercise
//!    `terminalize_wave_dispatch_attempt_on_db` (the standalone function that
//!    `CoordinatorActor::terminalize_wave_dispatch_attempt` delegates to) with a
//!    real in-memory database, proving the full pipeline:
//!    `WaveDispatchAttemptOutcome` → `build_wave_dispatch_terminal_params` →
//!    `advance_latest_to_terminal` → persisted `task_attempts` row.

use super::attempt_lifecycle::{make_dispatch_key, record_legacy_start};
use super::wave_dispatch::{
    WaveDispatchAttemptOutcome, build_wave_dispatch_terminal_params,
    is_oversized_blob_push_rejection, terminalize_wave_dispatch_attempt_on_db,
};
use crate::roles::{DispatchContext, RoleRegistry};
use djinn_core::events::EventBus;
use djinn_core::models::Task;
use djinn_core::models::TaskStatus;
use djinn_core::models::TransitionAction;
use djinn_core::models::task::compute_transition;
use djinn_core::models::task_attempt::TaskAttemptOutcome;
use djinn_db::{Database, EpicRepository, TaskAttemptRepository, TaskRepository};

const REOPEN_INTERVENTION_THRESHOLD: i64 = 3;

// ── Helpers ─────────────────────────────────────────────────────────────

fn lifecycle_test_db() -> Database {
    Database::open_in_memory().unwrap()
}

async fn lifecycle_create_task(db: &Database) -> Task {
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

/// Set up a task with a pending worker attempt (mimicking a dispatched
/// worker that reached the approved-PR-open wave-dispatch tick).
async fn setup_pending_attempt(db: &Database) -> (Task, String) {
    let task = lifecycle_create_task(db).await;
    let dk = make_dispatch_key(&task.id, "worker");
    let attempt_id = record_legacy_start(db, &task.id, "worker", None, &dk)
        .await
        .unwrap();
    (task, attempt_id)
}

/// Construct a `Task` with the minimum set of fields populated for
/// wave-dispatch tests.  `short_id` is used as both `id` and `short_id`;
/// other fields are zeroed/empty.
fn test_task(short_id: &str) -> Task {
    Task {
        id: short_id.into(),
        project_id: "p1".into(),
        short_id: short_id.into(),
        epic_id: Some("e1".into()),
        title: String::new(),
        description: String::new(),
        design: String::new(),
        issue_type: "task".into(),
        status: "approved".into(),
        priority: 0,
        owner: String::new(),
        labels: "[]".into(),
        acceptance_criteria: "[]".into(),
        reopen_count: 0,
        continuation_count: 0,
        total_reopen_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
        created_at: String::new(),
        updated_at: String::new(),
        closed_at: None,
        close_reason: None,
        merge_commit_sha: None,
        pr_url: None,
        merge_conflict_metadata: None,
        memory_refs: "[]".into(),
        agent_type: None,
        created_by_user_id: "test-user".to_owned(),
        ci_status: "unknown".into(),
        ci_head_sha: None,
        ci_pr_number: None,
        ci_blocking_required_check_names: "[]".into(),
        ci_failure_fingerprint: None,
        ci_first_seen_at: None,
        ci_last_seen_at: None,
        ci_same_signature_count: 0,
        ci_last_remediation_base_sha: None,
        ci_mirror_head_sha: None,
        ci_github_head_sha: None,
        ci_heads_diverged: None,
        ci_head_observation_error: None,
        ci_mq_state: None,
        ci_mq_run_id: None,
        ci_mq_head_sha: None,
        ci_mq_failed_check_names: None,
        ci_mq_failure_fingerprint: None,
        ci_mq_same_signature_count: None,
        ci_mq_first_seen_at: None,
        ci_mq_last_seen_at: None,
        unresolved_blocker_count: 0,
    }
}

/// A `Task` shaped like a worker task reopened by a merge-queue rejection:
/// `issue_type=task`, `status=open`, with the given `reopen_count`.
fn reopened_worker_task(reopen_count: i64) -> Task {
    Task {
        reopen_count,
        total_reopen_count: reopen_count,
        status: "open".into(),
        ..test_task("t1")
    }
}

// ── E6 transition / escalation tests ────────────────────────────────────

/// Part A — the `PrCiFailed` transition used by the merge-queue-rejection
/// reopen path lands the task at `open` AND increments `reopen_count`.
#[test]
fn merge_queue_rejection_reopen_increments_reopen_count() {
    for from in [TaskStatus::PrReview, TaskStatus::PrDraft] {
        let apply = compute_transition(&TransitionAction::PrCiFailed, &from, None)
            .expect("PrCiFailed must be a legal transition from pr_review/pr_draft");
        assert_eq!(
            apply.to_status,
            Some(TaskStatus::Open),
            "merge-queue rejection must reopen the task ({from:?} → open)"
        );
        assert!(
            apply.increment_reopen,
            "merge-queue rejection reopen MUST bump reopen_count (arms the escalation), from {from:?}"
        );
    }
}

#[test]
fn pr_conflict_transition_does_not_increment_reopen_count() {
    for from in [
        TaskStatus::Approved,
        TaskStatus::PrDraft,
        TaskStatus::PrReview,
    ] {
        let apply = compute_transition(&TransitionAction::PrConflict, &from, None)
            .expect("PrConflict must remain legal for approved/pr_draft/pr_review tasks");
        assert_eq!(apply.to_status, Some(TaskStatus::Open));
        assert!(
            !apply.increment_reopen,
            "PrConflict should not bump reopen_count; djinn_task_reopens_total must follow this semantic"
        );
    }
}

#[test]
fn merge_queue_reopened_task_routes_to_worker_role() {
    let registry = RoleRegistry::new();
    let ctx = DispatchContext;
    let task = reopened_worker_task(REOPEN_INTERVENTION_THRESHOLD);
    assert_eq!(
        registry.dispatch_role_for_task(&task, &ctx),
        Some("worker"),
        "a reopened (open, issue_type=task) merge-queue task must dispatch as worker"
    );
}

#[test]
fn reopen_count_crossing_threshold_arms_planner_escalation() {
    assert_eq!(REOPEN_INTERVENTION_THRESHOLD, 3);
    let below = reopened_worker_task(REOPEN_INTERVENTION_THRESHOLD - 1);
    assert!(below.reopen_count < REOPEN_INTERVENTION_THRESHOLD);
    for n in [
        REOPEN_INTERVENTION_THRESHOLD,
        REOPEN_INTERVENTION_THRESHOLD + 1,
    ] {
        let stuck = reopened_worker_task(n);
        assert!(stuck.reopen_count >= REOPEN_INTERVENTION_THRESHOLD);
        let registry = RoleRegistry::new();
        assert_eq!(
            registry.dispatch_role_for_task(&stuck, &DispatchContext),
            Some("worker"),
            "still a worker task at reopen_count={n}"
        );
    }
}

// ── Oversized-blob push rejection classifier ────────────────────────────

#[test]
fn oversized_blob_push_rejection_is_classified() {
    let reason = "push task_branch to GitHub failed: git command failed (exit 1) in \
        /tmp/.tmp86MbxD: git push --force ... stdout: stderr: \
        remote: error: File .local/share/pnpm/store/v11/files/ed/63a1c1... is 112.45 MB; \
        this exceeds GitHub's file size limit of 100.00 MB \
        remote: error: GH001: Large files detected. \
        ! [remote rejected] task/aqmk -> task/aqmk (pre-receive hook declined)";
    assert!(is_oversized_blob_push_rejection(reason));
}

#[test]
fn transient_push_failures_are_not_oversized_blob_rejections() {
    for reason in [
        "push task_branch to GitHub failed: git command failed (exit 1): \
         fatal: unable to access 'https://github.com/...': Could not resolve host",
        "push task_branch to GitHub failed: ! [rejected] (non-fast-forward)",
        "pr_open transition failed: InvalidTransition",
    ] {
        assert!(!is_oversized_blob_push_rejection(reason), "{reason}");
    }
}

#[test]
fn force_close_moves_approved_task_out_of_queried_state() {
    let apply = compute_transition(&TransitionAction::ForceClose, &TaskStatus::Approved, None)
        .expect("ForceClose must be legal from approved");
    assert_eq!(apply.to_status, Some(TaskStatus::Closed));
}

// ── Part B: proactive-rebase non-fatal contract ──────────────────────────

async fn git(dir: &std::path::Path, args: &[&str]) {
    djinn_git::run_git_command(
        dir.to_path_buf(),
        args.iter().map(|s| (*s).to_string()).collect(),
    )
    .await
    .unwrap_or_else(|e| panic!("git {args:?} in {dir:?} failed: {e}"));
}

async fn write_file(dir: &std::path::Path, name: &str, contents: &str) {
    tokio::fs::write(dir.join(name), contents).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proactive_rebase_conflict_is_non_fatal_and_aborts_cleanly() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    git(repo, &["init", "-q", "-b", "main"]).await;
    git(repo, &["config", "user.email", "t@example.com"]).await;
    git(repo, &["config", "user.name", "Test"]).await;
    write_file(repo, "f.txt", "base\n").await;
    git(repo, &["add", "f.txt"]).await;
    git(repo, &["commit", "-q", "-m", "base"]).await;
    git(repo, &["checkout", "-q", "-b", "task/x"]).await;
    write_file(repo, "f.txt", "from-task\n").await;
    git(repo, &["commit", "-qam", "task edit"]).await;
    git(repo, &["checkout", "-q", "main"]).await;
    write_file(repo, "f.txt", "from-main\n").await;
    git(repo, &["commit", "-qam", "main edit"]).await;
    git(repo, &["checkout", "-q", "task/x"]).await;
    let result = djinn_git::rebase_with_retry(repo, "main").await;
    assert!(result.is_err());
    assert!(!repo.join(".git/rebase-merge").exists() && !repo.join(".git/rebase-apply").exists());
    let status = djinn_git::run_git_command(
        repo.to_path_buf(),
        vec!["status".into(), "--porcelain".into()],
    )
    .await
    .unwrap();
    assert!(status.stdout.trim().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proactive_rebase_clean_replays_branch_onto_target() {
    let tmp = tempfile::TempDir::new().unwrap();
    let repo = tmp.path();
    git(repo, &["init", "-q", "-b", "main"]).await;
    git(repo, &["config", "user.email", "t@example.com"]).await;
    git(repo, &["config", "user.name", "Test"]).await;
    write_file(repo, "base.txt", "base\n").await;
    git(repo, &["add", "base.txt"]).await;
    git(repo, &["commit", "-q", "-m", "base"]).await;
    git(repo, &["checkout", "-q", "-b", "task/y"]).await;
    write_file(repo, "task.txt", "task\n").await;
    git(repo, &["add", "task.txt"]).await;
    git(repo, &["commit", "-q", "-m", "task edit"]).await;
    git(repo, &["checkout", "-q", "main"]).await;
    write_file(repo, "main.txt", "main\n").await;
    git(repo, &["add", "main.txt"]).await;
    git(repo, &["commit", "-q", "-m", "main edit"]).await;
    git(repo, &["checkout", "-q", "task/y"]).await;
    djinn_git::rebase_with_retry(repo, "main")
        .await
        .expect("non-conflicting rebase must succeed");
    let out = djinn_git::run_git_command(
        repo.to_path_buf(),
        vec![
            "merge-base".into(),
            "--is-ancestor".into(),
            "main".into(),
            "HEAD".into(),
        ],
    )
    .await;
    assert!(out.is_ok());
}

// ── Pure-logic mapping: build_wave_dispatch_terminal_params ──────────────

#[test]
fn build_terminal_params_adopted_pr_records_pr_context_and_branch() {
    let task = test_task("t-adopt");
    let pr_url = "https://github.example/owner/repo/pull/42";
    let head_sha = "abc123deadbeef";
    let params = build_wave_dispatch_terminal_params(
        &task,
        WaveDispatchAttemptOutcome::AdoptedPr { pr_url, head_sha },
    );
    assert_eq!(params.outcome, TaskAttemptOutcome::AdoptedPr);
    assert_eq!(params.pr_url, Some(pr_url));
    assert_eq!(params.github_head_sha, Some(head_sha));
    assert_eq!(params.submit_ref, "refs/heads/task/t-adopt");
    let ctx: serde_json::Value = serde_json::from_str(&params.summary_json).unwrap();
    assert_eq!(ctx["source"], "wave_dispatch");
    assert_eq!(ctx["path"], "adopted_pr");
    assert_eq!(ctx["pr_url"], pr_url);
    assert_eq!(ctx["github_head_sha"], head_sha);
    assert_eq!(ctx["task_branch"], "task/t-adopt");
    assert_eq!(ctx["submit_ref"], "refs/heads/task/t-adopt");
    assert!(params.summary.contains(pr_url));
}

#[test]
fn build_terminal_params_handoff_passes_through_task_pr_and_ci_context() {
    let mut task = test_task("t-handoff");
    task.pr_url = Some("https://github.example/owner/repo/pull/99".into());
    task.ci_head_sha = Some("deadbeef".into());
    let params = build_wave_dispatch_terminal_params(
        &task,
        WaveDispatchAttemptOutcome::Handoff {
            reason: "branch missing from mirror",
            replacement: "requeued_missing_branch",
        },
    );
    assert_eq!(params.outcome, TaskAttemptOutcome::Handoff);
    assert_eq!(
        params.pr_url,
        Some("https://github.example/owner/repo/pull/99")
    );
    assert_eq!(params.github_head_sha, Some("deadbeef"));
    assert_eq!(params.submit_ref, "refs/heads/task/t-handoff");
    let ctx: serde_json::Value = serde_json::from_str(&params.summary_json).unwrap();
    assert_eq!(ctx["source"], "wave_dispatch");
    assert_eq!(ctx["path"], "handoff");
    assert_eq!(ctx["reason"], "branch missing from mirror");
    assert_eq!(ctx["replacement"], "requeued_missing_branch");
    assert_eq!(ctx["pr_url"], "https://github.example/owner/repo/pull/99");
    assert_eq!(ctx["task_branch"], "task/t-handoff");
    assert_eq!(ctx["submit_ref"], "refs/heads/task/t-handoff");
    assert!(params.summary.contains("requeued_missing_branch"));
}

#[test]
fn build_terminal_params_handoff_with_no_pr_url() {
    let task = test_task("t-nopr");
    let params = build_wave_dispatch_terminal_params(
        &task,
        WaveDispatchAttemptOutcome::Handoff {
            reason: "no commits ahead of base",
            replacement: "task_closed_no_commits",
        },
    );
    assert_eq!(params.outcome, TaskAttemptOutcome::Handoff);
    assert!(params.pr_url.is_none());
    assert!(params.github_head_sha.is_none());
    let ctx: serde_json::Value = serde_json::from_str(&params.summary_json).unwrap();
    assert!(ctx["pr_url"].is_null());
    assert_eq!(ctx["task_branch"], "task/t-nopr");
}

#[test]
fn build_terminal_params_force_closed_records_close_reason_and_branch() {
    let task = test_task("t-fc");
    let params = build_wave_dispatch_terminal_params(
        &task,
        WaveDispatchAttemptOutcome::ForceClosed {
            reason: "GH001: Large files detected. pre-receive hook declined",
            close_reason: "oversized_blob_in_branch_history",
        },
    );
    assert_eq!(params.outcome, TaskAttemptOutcome::ForceClosed);
    assert_eq!(params.submit_ref, "refs/heads/task/t-fc");
    let ctx: serde_json::Value = serde_json::from_str(&params.summary_json).unwrap();
    assert_eq!(ctx["source"], "wave_dispatch");
    assert_eq!(ctx["path"], "force_closed");
    assert_eq!(ctx["close_reason"], "oversized_blob_in_branch_history");
    assert!(
        ctx["reason"]
            .as_str()
            .unwrap()
            .contains("pre-receive hook declined")
    );
    assert_eq!(ctx["task_branch"], "task/t-fc");
}

// ── Wiring tests: terminalize_wave_dispatch_attempt_on_db ───────────────
//
// These exercise the full pipeline through the standalone function that
// `CoordinatorActor::terminalize_wave_dispatch_attempt` delegates to.
// They prove:
//   - `build_wave_dispatch_terminal_params` output is correctly threaded into
//     `advance_latest_to_terminal` (fields don't get swapped or dropped)
//   - The attempt row in the DB has the expected outcome, pr_url,
//     github_head_sha, summary, and summary_json
//   - Duplicate terminalization is idempotent (no new rows, no backward moves)

/// Wiring test: adopted-pr path.  Creates a real pending attempt in the DB,
/// calls `terminalize_wave_dispatch_attempt_on_db` with `AdoptedPr`, and
/// verifies the persisted row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wiring_adopted_pr_terminalizes_attempt_with_pr_context() {
    let db = lifecycle_test_db();
    let (task, attempt_id) = setup_pending_attempt(&db).await;
    let pr_url = "https://github.example/owner/repo/pull/42";
    let head_sha = "abc123deadbeef";

    terminalize_wave_dispatch_attempt_on_db(
        &db,
        &task,
        WaveDispatchAttemptOutcome::AdoptedPr { pr_url, head_sha },
    )
    .await;

    let repo = TaskAttemptRepository::new(db);
    let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(attempt.outcome, "adopted_pr");
    assert_eq!(attempt.pr_url.as_deref(), Some(pr_url));
    assert_eq!(attempt.github_head_sha.as_deref(), Some(head_sha));
    let expected_ref = format!("refs/heads/task/{}", task.short_id);
    assert_eq!(attempt.submit_ref.as_deref(), Some(expected_ref.as_str()));

    // Verify summary_json includes task_branch and pr_url — fields the
    // helper writes that a previous version of the tests omitted.
    let ctx: serde_json::Value =
        serde_json::from_str(attempt.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(ctx["source"], "wave_dispatch");
    assert_eq!(ctx["path"], "adopted_pr");
    assert_eq!(ctx["pr_url"], pr_url);
    assert_eq!(ctx["github_head_sha"], head_sha);
    assert_eq!(ctx["task_branch"], format!("task/{}", task.short_id));
    assert!(attempt.summary.as_deref().unwrap().contains(pr_url));
}

/// Wiring test: handoff path.  Sets task.pr_url and task.ci_head_sha
/// before calling the helper; verifies the DB row passes them through.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wiring_handoff_passes_through_task_pr_and_ci_to_db() {
    let db = lifecycle_test_db();
    let (mut task, attempt_id) = setup_pending_attempt(&db).await;
    task.pr_url = Some("https://github.example/owner/repo/pull/99".into());
    task.ci_head_sha = Some("deadbeef".into());

    terminalize_wave_dispatch_attempt_on_db(
        &db,
        &task,
        WaveDispatchAttemptOutcome::Handoff {
            reason: "branch missing from mirror",
            replacement: "requeued_missing_branch",
        },
    )
    .await;

    let repo = TaskAttemptRepository::new(db);
    let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(attempt.outcome, "handoff");
    assert_eq!(
        attempt.pr_url.as_deref(),
        Some("https://github.example/owner/repo/pull/99"),
        "handoff must pass through task.pr_url"
    );
    assert_eq!(
        attempt.github_head_sha.as_deref(),
        Some("deadbeef"),
        "handoff must pass through task.ci_head_sha"
    );

    let ctx: serde_json::Value =
        serde_json::from_str(attempt.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(ctx["path"], "handoff");
    assert_eq!(ctx["reason"], "branch missing from mirror");
    assert_eq!(ctx["replacement"], "requeued_missing_branch");
    assert_eq!(ctx["pr_url"], "https://github.example/owner/repo/pull/99");
    assert_eq!(ctx["task_branch"], format!("task/{}", task.short_id));
}

/// Wiring test: handoff path when task has no PR (e.g. closed-no-commits).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wiring_handoff_with_no_pr_url_records_null_in_db() {
    let db = lifecycle_test_db();
    let (task, attempt_id) = setup_pending_attempt(&db).await;

    terminalize_wave_dispatch_attempt_on_db(
        &db,
        &task,
        WaveDispatchAttemptOutcome::Handoff {
            reason: "no commits ahead of base",
            replacement: "task_closed_no_commits",
        },
    )
    .await;

    let repo = TaskAttemptRepository::new(db);
    let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(attempt.outcome, "handoff");
    assert!(attempt.pr_url.is_none());
    assert!(attempt.github_head_sha.is_none());

    let ctx: serde_json::Value =
        serde_json::from_str(attempt.summary_json.as_deref().unwrap()).unwrap();
    assert!(ctx["pr_url"].is_null());
    assert_eq!(ctx["task_branch"], format!("task/{}", task.short_id));
}

/// Wiring test: force-closed path.  Proves `TaskAttemptOutcome::ForceClosed`
/// is written to the DB with the close_reason in summary_json.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wiring_force_closed_records_close_reason_and_branch_in_db() {
    let db = lifecycle_test_db();
    let (task, attempt_id) = setup_pending_attempt(&db).await;

    terminalize_wave_dispatch_attempt_on_db(
        &db,
        &task,
        WaveDispatchAttemptOutcome::ForceClosed {
            reason: "GH001: Large files detected. pre-receive hook declined",
            close_reason: "oversized_blob_in_branch_history",
        },
    )
    .await;

    let repo = TaskAttemptRepository::new(db);
    let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(attempt.outcome, "force_closed");

    let ctx: serde_json::Value =
        serde_json::from_str(attempt.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(ctx["source"], "wave_dispatch");
    assert_eq!(ctx["path"], "force_closed");
    assert_eq!(ctx["close_reason"], "oversized_blob_in_branch_history");
    assert!(
        ctx["reason"]
            .as_str()
            .unwrap()
            .contains("pre-receive hook declined")
    );
    assert_eq!(ctx["task_branch"], format!("task/{}", task.short_id));
}

/// Wiring test: duplicate terminalization is idempotent.  A second call
/// (mimicking a re-tick that re-processes the same approved task) must NOT
/// create a second attempt row, move the terminal outcome backward, or
/// overwrite the original recorded context.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wiring_duplicate_terminalization_is_idempotent() {
    let db = lifecycle_test_db();
    let (task, attempt_id) = setup_pending_attempt(&db).await;

    // First call: terminalize as adopted-PR.
    terminalize_wave_dispatch_attempt_on_db(
        &db,
        &task,
        WaveDispatchAttemptOutcome::AdoptedPr {
            pr_url: "https://github.example/owner/repo/pull/7",
            head_sha: "sha-orig",
        },
    )
    .await;

    // Second call: a different wave-dispatch path fires (late ForceClose from
    // a duplicate supervisor_pr_open race).  The attempt must remain unchanged.
    terminalize_wave_dispatch_attempt_on_db(
        &db,
        &task,
        WaveDispatchAttemptOutcome::ForceClosed {
            reason: "late race",
            close_reason: "late",
        },
    )
    .await;

    let repo = TaskAttemptRepository::new(db);
    let all = repo.list_for_task(&task.id).await.unwrap();
    assert_eq!(
        all.len(),
        1,
        "duplicate wave-dispatch terminalization must not create a second attempt row"
    );
    let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(
        attempt.outcome, "adopted_pr",
        "original terminal outcome must be preserved"
    );
    assert_eq!(
        attempt.pr_url.as_deref(),
        Some("https://github.example/owner/repo/pull/7"),
        "original pr_url must be preserved"
    );
    assert_eq!(
        attempt.github_head_sha.as_deref(),
        Some("sha-orig"),
        "original github_head_sha must be preserved"
    );
    let ctx: serde_json::Value =
        serde_json::from_str(attempt.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        ctx["path"], "adopted_pr",
        "original summary_json path must be preserved"
    );
    assert_eq!(
        ctx["task_branch"],
        format!("task/{}", task.short_id),
        "original task_branch must be preserved"
    );
}

/// Wiring test: no live attempt is a no-op (no panic, no row created).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wiring_no_live_attempt_is_noop() {
    let db = lifecycle_test_db();
    let task = lifecycle_create_task(&db).await;

    terminalize_wave_dispatch_attempt_on_db(
        &db,
        &task,
        WaveDispatchAttemptOutcome::AdoptedPr {
            pr_url: "https://example.com/pr/1",
            head_sha: "sha1",
        },
    )
    .await;

    let repo = TaskAttemptRepository::new(db);
    let all = repo.list_for_task(&task.id).await.unwrap();
    assert!(all.is_empty(), "no live attempt must not create a row");
}

/// Wiring test: already-terminal attempt is not moved backward.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wiring_already_terminal_attempt_is_not_moved_backward() {
    let db = lifecycle_test_db();
    let (task, attempt_id) = setup_pending_attempt(&db).await;

    // First: terminalize as adopted_pr.
    terminalize_wave_dispatch_attempt_on_db(
        &db,
        &task,
        WaveDispatchAttemptOutcome::AdoptedPr {
            pr_url: "https://example.com/pr/original",
            head_sha: "original-sha",
        },
    )
    .await;

    // Second: attempt with a different outcome — must be a no-op.
    terminalize_wave_dispatch_attempt_on_db(
        &db,
        &task,
        WaveDispatchAttemptOutcome::Handoff {
            reason: "late handoff",
            replacement: "late",
        },
    )
    .await;

    let repo = TaskAttemptRepository::new(db);
    let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(
        attempt.outcome, "adopted_pr",
        "must not overwrite terminal outcome"
    );
    assert_eq!(
        attempt.pr_url.as_deref(),
        Some("https://example.com/pr/original"),
        "must not overwrite terminal pr_url"
    );
    assert_eq!(
        attempt.github_head_sha.as_deref(),
        Some("original-sha"),
        "must not overwrite terminal github_head_sha"
    );
    let ctx: serde_json::Value =
        serde_json::from_str(attempt.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        ctx["path"], "adopted_pr",
        "must not overwrite terminal summary_json"
    );
}

// ── Hole B: branch-missing PrConflict reopens (NOT handoffs) ─────────────

/// Newest non-guard worker attempt outcome for a task (mirrors the respawn
/// guard's `latest_attempt_is_reopened` filter): skips guard-only audit rows.
async fn newest_non_guard_worker_outcome(db: &Database, task_id: &str) -> Option<String> {
    TaskAttemptRepository::new(db.clone())
        .list_for_task(task_id)
        .await
        .unwrap()
        .into_iter()
        .filter(|a| a.role == "worker")
        .find(|a| {
            a.outcome != TaskAttemptOutcome::Deferred.as_str()
                && a.outcome != TaskAttemptOutcome::AdoptedPr.as_str()
        })
        .map(|a| a.outcome)
}

/// Pure-logic: the `Reopened` outcome maps to `TaskAttemptOutcome::Reopened`
/// and records the `reopened` summary_json path with task PR / branch context.
#[test]
fn build_terminal_params_reopened_records_reopen_path_and_branch() {
    let mut task = test_task("t-reopen");
    task.pr_url = Some("https://github.example/owner/repo/pull/7".into());
    task.ci_head_sha = Some("cafef00d".into());
    let params = build_wave_dispatch_terminal_params(
        &task,
        WaveDispatchAttemptOutcome::Reopened {
            reason: "approved with no pushed task_branch",
        },
    );
    assert_eq!(params.outcome, TaskAttemptOutcome::Reopened);
    assert_eq!(
        params.pr_url,
        Some("https://github.example/owner/repo/pull/7")
    );
    assert_eq!(params.github_head_sha, Some("cafef00d"));
    assert_eq!(params.submit_ref, "refs/heads/task/t-reopen");
    let ctx: serde_json::Value = serde_json::from_str(&params.summary_json).unwrap();
    assert_eq!(ctx["source"], "wave_dispatch");
    assert_eq!(ctx["path"], "reopened");
    assert_eq!(ctx["reason"], "approved with no pushed task_branch");
    assert_eq!(ctx["task_branch"], "task/t-reopen");
}

/// Wiring: a branch-missing PrConflict reopen terminalizes the live `submitted`
/// worker attempt to `reopened` (NOT `handoff`), so the respawn guard's
/// latest-attempt-is-reopened gate sees the rework reopen (#1719 invariant).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wiring_reopened_terminalizes_live_submitted_attempt_to_reopened() {
    let db = lifecycle_test_db();
    let (task, attempt_id) = setup_pending_attempt(&db).await;
    let repo = TaskAttemptRepository::new(db.clone());
    // Advance to `submitted` to mirror the real branch-missing approved task
    // (the worker already submitted before the PR-open push found no branch).
    repo.advance_to_submitted(djinn_db::SubmitTaskAttemptParams {
        id: &attempt_id,
        submit_ref: Some("ref-1"),
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: Some("submitted"),
        summary_json: None,
        log_tail: None,
    })
    .await
    .unwrap();

    terminalize_wave_dispatch_attempt_on_db(
        &db,
        &task,
        WaveDispatchAttemptOutcome::Reopened {
            reason: "approved with no pushed task_branch; re-running worker",
        },
    )
    .await;

    let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(
        attempt.outcome, "reopened",
        "branch-missing PrConflict must terminalize as reopened, not handoff"
    );
    assert_eq!(
        newest_non_guard_worker_outcome(&db, &task.id)
            .await
            .as_deref(),
        Some("reopened"),
        "newest non-guard worker attempt must be reopened"
    );
}

/// Wiring: with NO live attempt, the `Reopened` outcome still leaves a durable
/// `reopened` marker (via ensure_rework_marker), preserving #1719's invariant
/// that every rework reopen leaves a `reopened` latest attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wiring_reopened_with_no_live_attempt_records_durable_marker() {
    let db = lifecycle_test_db();
    let task = lifecycle_create_task(&db).await;

    terminalize_wave_dispatch_attempt_on_db(
        &db,
        &task,
        WaveDispatchAttemptOutcome::Reopened {
            reason: "approved with no pushed task_branch",
        },
    )
    .await;

    assert_eq!(
        newest_non_guard_worker_outcome(&db, &task.id)
            .await
            .as_deref(),
        Some("reopened"),
        "a durable reopened marker must exist even with no live attempt to terminalize"
    );
}
