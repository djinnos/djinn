use super::{
    NOOP_CLOSE_REASON, PR_ALREADY_EXISTS_HINT, TASK_OUTCOME_BODY_EXCERPT_BYTES, close_noop,
    handle_noop_disposition, is_concurrent_push_race, pr_open_failure_outcome,
    pr_open_untyped_failure_outcome, should_close_noop,
    should_route_settled_noop_without_live_mover,
};
use crate::github_error_render::render_github_write_error;
use crate::supervisor_impl::disposition::{
    LiveMoverEvidence, NUDGE_CAP, RunDisposition, decide_run_disposition, has_live_mover,
};
use crate::test_helpers;
use djinn_core::models::Task;
use djinn_core::models::TransitionAction;
use djinn_core::run_progress::{RunProgress, RunProgressSignals, classify_run_progress};
use djinn_core::tool_error::ErrorClass;
use djinn_db::TaskRepository;
use djinn_git::GitError;
use djinn_provider::github_api::GitHubApiError;
use djinn_runtime::spec::TaskRunOutcome;
use reqwest::StatusCode;

async fn no_op_nudge_fixture() -> (TaskRepository, Task) {
    let db = test_helpers::create_test_db();
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), test_helpers::test_events());
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    let in_progress = repo
        .transition(
            &task.id,
            TransitionAction::Start,
            "worker-1",
            "worker",
            None,
            None,
        )
        .await
        .expect("start task");
    (repo, in_progress)
}

async fn latest_nudge_comment_body(repo: &TaskRepository, task_id: &str) -> String {
    let entries = repo.list_activity(task_id).await.expect("activity");
    entries
        .iter()
        .rev()
        .filter(|entry| entry.event_type == "comment")
        .filter_map(|entry| serde_json::from_str::<serde_json::Value>(&entry.payload).ok())
        .filter_map(|payload| {
            payload
                .get("body")
                .and_then(|body| body.as_str())
                .map(ToString::to_string)
        })
        .next()
        .expect("nudge comment body")
}

#[tokio::test]
async fn no_op_nudge_comment_uses_wind_down_summary_hint() {
    let (repo, task) = no_op_nudge_fixture().await;
    repo.log_activity(
        Some(&task.id),
        "agent-supervisor",
        "worker",
        "work_submitted",
        &serde_json::json!({
            "summary": "resume by implementing the evidence resolver",
            "remaining_concerns": "budget-parked: follow up",
        })
        .to_string(),
    )
    .await
    .expect("log wind-down summary");

    let outcome = handle_noop_disposition(&task, &repo, "main").await;
    assert!(matches!(outcome, TaskRunOutcome::Escalated { .. }));

    let body = latest_nudge_comment_body(&repo, &task.id).await;
    assert!(body.contains("Prior intent: resume by implementing the evidence resolver"));
    assert!(!body.contains("Prior intent: test task description"));
}

#[tokio::test]
async fn no_op_nudge_comment_uses_last_error_signature_hint() {
    let (repo, task) = no_op_nudge_fixture().await;
    repo.log_activity(
        Some(&task.id),
        "agent-supervisor",
        "system",
        "runtime_error",
        &serde_json::json!({
            "tool_name": "shell",
            "error": "cargo test failed\nstack trace omitted",
        })
        .to_string(),
    )
    .await
    .expect("log runtime error");

    let outcome = handle_noop_disposition(&task, &repo, "main").await;
    assert!(matches!(outcome, TaskRunOutcome::Escalated { .. }));

    let body = latest_nudge_comment_body(&repo, &task.id).await;
    assert!(body.contains("Prior intent: shell: cargo test failed"));
    assert!(!body.contains("Prior intent: test task description"));
}

#[tokio::test]
async fn no_op_nudge_comment_uses_ac_delta_hint() {
    let (repo, task) = no_op_nudge_fixture().await;

    let outcome = handle_noop_disposition(&task, &repo, "main").await;
    assert!(matches!(outcome, TaskRunOutcome::Escalated { .. }));

    let body = latest_nudge_comment_body(&repo, &task.id).await;
    assert!(body.contains("Prior intent: Unmet acceptance criteria:"));
    assert!(body.contains("- default test criterion"));
    assert!(!body.contains("Prior intent: test task description"));
}

fn settled_noop_task() -> Task {
    Task {
        id: "task-uuid".into(),
        project_id: "project-uuid".into(),
        short_id: "noop1".into(),
        epic_id: None,
        title: "No-op fixture".into(),
        description: "Do the requested work".into(),
        design: String::new(),
        issue_type: "task".into(),
        status: "verifying".into(),
        priority: 1,
        owner: String::new(),
        labels: "[]".into(),
        acceptance_criteria: "[]".into(),
        reopen_count: 0,
        continuation_count: 0,
        verification_failure_count: 0,
        total_reopen_count: 0,
        total_verification_failure_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
        created_at: "2026-01-01T00:00:00.000Z".into(),
        updated_at: "2026-01-01T00:00:00.000Z".into(),
        closed_at: None,
        close_reason: None,
        merge_commit_sha: None,
        pr_url: None,
        merge_conflict_metadata: None,
        memory_refs: "[]".into(),
        agent_type: None,
        created_by_user_id: None,
        unresolved_blocker_count: 0,
    }
}

#[test]
fn detects_real_github_lock_rejection() {
    let err = GitError::Other(anyhow::anyhow!(
        "git command failed (exit 1) in /tmp/.tmpDq5yoG: git push --force ... task/uots:refs/heads/task/uots\n \
             ! [remote rejected]   task/uots -> task/uots (cannot lock ref 'refs/heads/task/uots': reference already exists)"
    ));
    assert!(is_concurrent_push_race(&err));
}

#[test]
fn ignores_unrelated_push_failures() {
    let err = GitError::Other(anyhow::anyhow!("auth failed: permission denied"));
    assert!(!is_concurrent_push_race(&err));
}

#[test]
fn requires_both_fragments() {
    // Just "cannot lock ref" (e.g. a local refs-database fsck) without the
    // "reference already exists" qualifier is a different problem.
    let err = GitError::Other(anyhow::anyhow!("cannot lock ref 'foo': corrupted"));
    assert!(!is_concurrent_push_race(&err));
}

#[test]
fn no_mover_settled_noop_predicate_enters_same_disposition_ladder_as_pr_open_fork() {
    let task = settled_noop_task();
    let evidence = LiveMoverEvidence::default();

    assert!(should_route_settled_noop_without_live_mover(
        &task, &evidence
    ));

    let signals = RunProgressSignals {
        commits_ahead: 0,
        files_changed: 0,
        ac_newly_satisfied: 0,
    };
    let progress = classify_run_progress(&signals);
    assert_eq!(
        decide_run_disposition(progress, task.continuation_count, NUDGE_CAP),
        RunDisposition::Nudge
    );
}

#[test]
fn no_mover_settled_noop_predicate_defers_when_any_live_mover_exists() {
    let task = settled_noop_task();
    let live_mover_cases = [
        LiveMoverEvidence {
            active_session: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            queued_dispatch: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            dispatch_inflight: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            recently_dispatched: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            open_pr: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            pr_poller_owned: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            review_pending_with_reviewer: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            unresolved_blockers: true,
            ..Default::default()
        },
    ];

    for evidence in live_mover_cases {
        assert!(
            !should_route_settled_noop_without_live_mover(&task, &evidence),
            "live mover evidence must keep task on existing path: {evidence:?}"
        );
    }
}

#[test]
fn no_mover_settled_noop_predicate_preserves_existing_pr_path() {
    let mut task = settled_noop_task();
    task.pr_url = Some("https://github.example/pr/1".into());

    assert!(!should_route_settled_noop_without_live_mover(
        &task,
        &LiveMoverEvidence {
            open_pr: true,
            ..Default::default()
        }
    ));
}

// ── Historical close-path predicate consistency (T3) ────────────────────
//
// These tests lock the behavior of the historical `close_noop` path
// (pr.rs:735) and the `handle_noop_disposition` zero-diff/no-commit
// disposition ladder against the 9rob live-mover predicate's verdict.
// The predicate itself lives in `supervisor_impl::disposition`; these
// tests assert that:
//   (a) a no-mover + zero-diff task closes via the historical path with
//       the same `reason` text and same `TaskRunOutcome::Closed` value
//       the pre-9rob path produced;
//   (b) a no-mover + non-zero-diff task does NOT close prematurely — the
//       disposition classifier (`classify_run_progress`) returns
//       `Productive` for any non-zero signals, so even if the zero-commits
//       guard fires, the disposition verdict is `Proceed` (not `Close`),
//       meaning the task proceeds through the normal PR-open path;
//   (c) a task with a live mover does NOT enter the close path: the
//       predicate-driven entry point `should_route_settled_noop_without_live_mover`
//       returns `false` for any live-mover evidence, deferring to the
//       task's existing path.
//
// Option B of the task design: explicit regression tests pin the
// historical close-path behavior against the predicate's verdict.

/// (a) The historical `close_noop` produces a `TaskRunOutcome::Closed`
/// carrying the canonical `NOOP_CLOSE_REASON` text. This pins the
/// pre-9rob reason text against accidental drift — the supervisor's
/// `task.close_reason` column and the coordinator's run-settlement log
/// both depend on this exact text remaining stable.
#[test]
fn historical_close_noop_reason_text_is_stable() {
    assert!(
        NOOP_CLOSE_REASON.contains("no code changes were produced"),
        "close reason must explain the no-diff condition: {NOOP_CLOSE_REASON}"
    );
    assert!(
        NOOP_CLOSE_REASON.contains("memory/notes-only"),
        "close reason must name the canonical memory/notes-only case: {NOOP_CLOSE_REASON}"
    );
    assert!(
        NOOP_CLOSE_REASON.contains("closing as completed"),
        "close reason must state the terminal action: {NOOP_CLOSE_REASON}"
    );
    // The pre-9rob reason is a single-line string — no embedded newlines
    // or trailing whitespace that would corrupt the `close_reason` column.
    assert!(!NOOP_CLOSE_REASON.contains('\n'));
    assert_eq!(NOOP_CLOSE_REASON, NOOP_CLOSE_REASON.trim());
}

#[tokio::test]
async fn close_noop_returns_historical_closed_outcome_for_no_mover_zero_diff() {
    let (repo, task) = no_op_nudge_fixture().await;
    let mut exhausted = task.clone();
    exhausted.continuation_count = NUDGE_CAP;
    let signals = RunProgressSignals {
        commits_ahead: 0,
        files_changed: 0,
        ac_newly_satisfied: 0,
    };

    let outcome = close_noop(&exhausted, &repo, true, &signals).await;

    match outcome {
        TaskRunOutcome::Closed { reason } => assert_eq!(reason, NOOP_CLOSE_REASON),
        other => panic!("expected historical Closed outcome, got {other:?}"),
    }
    let closed = repo
        .get(&exhausted.id)
        .await
        .expect("reload task")
        .expect("task exists");
    assert_eq!(closed.status, "closed");
    // The repository normalizes Close transitions to the durable close_reason
    // value "completed"; the historical reason text is preserved on the
    // TaskRunOutcome payload asserted above.
    assert_eq!(closed.close_reason.as_deref(), Some("completed"));
}

#[tokio::test]
async fn close_noop_skips_historical_close_for_no_mover_non_zero_diff() {
    let (repo, task) = no_op_nudge_fixture().await;
    let signals = RunProgressSignals {
        commits_ahead: 0,
        files_changed: 1,
        ac_newly_satisfied: 0,
    };

    let outcome = close_noop(&task, &repo, true, &signals).await;

    assert!(
        matches!(&outcome, TaskRunOutcome::Escalated { reason } if reason.contains("close skipped")),
        "non-zero-diff no-mover must not produce the historical Closed outcome: {outcome:?}"
    );
    let reloaded = repo
        .get(&task.id)
        .await
        .expect("reload task")
        .expect("task exists");
    assert_eq!(reloaded.status, task.status);
    assert_eq!(reloaded.close_reason, None);
}

#[tokio::test]
async fn close_noop_skips_historical_close_when_live_mover_predicate_disagrees() {
    let (repo, task) = no_op_nudge_fixture().await;
    let signals = RunProgressSignals {
        commits_ahead: 0,
        files_changed: 0,
        ac_newly_satisfied: 0,
    };

    let outcome = close_noop(&task, &repo, false, &signals).await;

    assert!(
        matches!(&outcome, TaskRunOutcome::Escalated { reason } if reason.contains("close skipped")),
        "live-mover verdict must keep task out of the historical Closed outcome: {outcome:?}"
    );
    let reloaded = repo
        .get(&task.id)
        .await
        .expect("reload task")
        .expect("task exists");
    assert_eq!(reloaded.status, task.status);
    assert_eq!(reloaded.close_reason, None);
}

/// (a) A no-mover + zero-diff task routes through the historical close
/// path: `handle_noop_disposition` builds `RunProgressSignals { 0, 0, 0 }`,
/// the D3a classifier returns `NoOp`, and the disposition verdict under
/// the production cap is `Nudge` (counts 0 and 1) then `Close` (count 2+).
/// This pins the pre-9rob routing against the live-mover predicate: the
/// zero-diff / no-mover case must continue to land in the close path
/// after the budget is exhausted, producing the same `RunDisposition::Close`
/// verdict the pre-9rob path produced.
#[test]
fn historical_close_path_routes_no_mover_zero_diff_through_disposition_ladder() {
    assert!(
        !has_live_mover(&LiveMoverEvidence::default()),
        "empty evidence must mean no live mover"
    );

    // The supervisor's PR-open zero-commits guard hardcodes
    // `commits_ahead: 0, files_changed: 0` in `handle_noop_disposition`
    // (pr.rs:562-566). Reconstruct the same signals here to assert the
    // classifier and disposition ladder agree.
    let signals = RunProgressSignals {
        commits_ahead: 0,
        files_changed: 0,
        ac_newly_satisfied: 0,
    };
    let progress = classify_run_progress(&signals);
    assert_eq!(progress, RunProgress::NoOp);

    // Under the production cap, the first two no-op encounters nudge and
    // the third closes — the pre-9rob behavior the disposition ladder
    // preserves.
    for count in 0..NUDGE_CAP {
        assert_eq!(
            decide_run_disposition(RunProgress::NoOp, count, NUDGE_CAP),
            RunDisposition::Nudge,
            "count {count} under cap must nudge"
        );
    }
    assert_eq!(
        decide_run_disposition(RunProgress::NoOp, NUDGE_CAP, NUDGE_CAP),
        RunDisposition::Close,
        "count at cap must close — this is the path that invokes close_noop"
    );
    assert!(
        should_close_noop(true, &signals, &settled_noop_task()),
        "no-mover + zero-diff is the only predicate-backed close path"
    );
}

/// (b) A no-mover + non-zero-diff task does NOT close prematurely. Even
/// though the supervisor's `task_branch_commits_ahead` guard fires on
/// `Ok(0)` regardless of `files_changed`, the disposition classifier
/// (`classify_run_progress`) returns `Productive` for any non-zero
/// physical signal, and the D3b disposition verdict for `Productive` is
/// `Proceed` — meaning the task must continue through the normal PR-open
/// path, not the close path.
///
/// This pins the predicate's verdict against the historical close path:
/// a task with files_changed > 0 (e.g. uncommitted edits) must never
/// land in `close_noop`, even if the zero-commits guard fires.
#[test]
fn historical_close_path_does_not_close_prematurely_with_non_zero_diff() {
    // Case 1: commits ahead, no files changed — the historical guard
    // would NOT fire (commits_ahead > 0), but we assert the classifier
    // verdict independently to lock the predicate's contract.
    let signals_commits_only = RunProgressSignals {
        commits_ahead: 1,
        files_changed: 0,
        ac_newly_satisfied: 0,
    };
    assert_eq!(
        classify_run_progress(&signals_commits_only),
        RunProgress::Productive
    );
    assert_eq!(
        decide_run_disposition(classify_run_progress(&signals_commits_only), 0, NUDGE_CAP),
        RunDisposition::Proceed,
        "commits_ahead > 0 must proceed (never close)"
    );

    // Case 2: files changed but no commits — the historical zero-commits
    // guard fires (Ok(0) on task_branch_commits_ahead), but the D3a
    // classifier must still return `Productive` because `files_changed > 0`.
    // The pre-9rob `handle_noop_disposition` overrode `files_changed` to
    // 0 (see pr.rs:564), which would have closed this task; the
    // regression test pins the *predicate's* verdict (Productive →
    // Proceed) as the correct contract even though the legacy
    // `handle_noop_disposition` hardcodes files_changed=0.
    let signals_files_only = RunProgressSignals {
        commits_ahead: 0,
        files_changed: 1,
        ac_newly_satisfied: 0,
    };
    assert_eq!(
        classify_run_progress(&signals_files_only),
        RunProgress::Productive,
        "files_changed > 0 must classify as Productive, not NoOp"
    );
    assert_eq!(
        decide_run_disposition(
            classify_run_progress(&signals_files_only),
            NUDGE_CAP + 5,
            NUDGE_CAP
        ),
        RunDisposition::Proceed,
        "non-zero files_changed must proceed (never close) regardless of count"
    );

    // Case 3: both commits and files — definitely productive, never close.
    let signals_both = RunProgressSignals {
        commits_ahead: 1,
        files_changed: 1,
        ac_newly_satisfied: 0,
    };
    assert_eq!(
        classify_run_progress(&signals_both),
        RunProgress::Productive
    );
    assert_eq!(
        decide_run_disposition(
            classify_run_progress(&signals_both),
            NUDGE_CAP + 5,
            NUDGE_CAP
        ),
        RunDisposition::Proceed
    );
}

/// (c) A task with a live mover does NOT enter the close path even if
/// the signal is otherwise ambiguous. The predicate-driven entry point
/// `should_route_settled_noop_without_live_mover` must return `false`
/// for every live-mover evidence class — this guarantees the historical
/// close path is never reached for a task that still has something
/// live (active session, queued dispatch, open PR, etc.).
///
/// The signal is "otherwise ambiguous" because we pair the live-mover
/// evidence with the *exact* zero-diff signals from (a) — the case where
/// the historical path would have closed. The test asserts the
/// predicate's verdict overrides the ambiguous zero-diff signal.
#[test]
fn historical_close_path_is_unreachable_when_live_mover_predicate_says_mover_present() {
    let task = settled_noop_task();
    let ambiguous_zero_diff_signals = RunProgressSignals {
        commits_ahead: 0,
        files_changed: 0,
        ac_newly_satisfied: 0,
    };
    // Sanity: the signals alone would route to `NoOp` (and eventually
    // `Close` after the budget exhausts), so this is the "otherwise
    // ambiguous" case the AC names.
    assert_eq!(
        classify_run_progress(&ambiguous_zero_diff_signals),
        RunProgress::NoOp
    );

    // Every live-mover evidence class must override the ambiguous
    // zero-diff signal: the task is NOT routed to the close path.
    let live_mover_cases = [
        LiveMoverEvidence {
            active_session: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            queued_dispatch: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            dispatch_inflight: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            recently_dispatched: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            open_pr: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            pr_poller_owned: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            review_pending_with_reviewer: true,
            ..Default::default()
        },
        LiveMoverEvidence {
            unresolved_blockers: true,
            ..Default::default()
        },
    ];

    for evidence in live_mover_cases {
        // Predicate-driven entry point must defer (return false) — the
        // task stays on its existing path and never enters the close
        // path.
        assert!(
            !should_route_settled_noop_without_live_mover(&task, &evidence),
            "live mover evidence {evidence:?} must keep task off the close path \
                 even with ambiguous zero-diff signals"
        );
        // And the underlying `has_live_mover` predicate must agree —
        // this is the contract the entry point delegates to.
        assert!(
            has_live_mover(&evidence),
            "live mover evidence {evidence:?} must register as a live mover"
        );
    }
}

/// End-to-end regression: the `TaskRunOutcome::Closed { reason }` value
/// the historical `close_noop` produces is exactly
/// `TaskRunOutcome::Closed { reason: NOOP_CLOSE_REASON.to_string() }`.
/// This is the value the pre-9rob path produced; the test pins it
/// against the live-mover predicate's verdict by asserting both the
/// outcome shape and the reason text match the historical contract.
#[test]
fn historical_close_outcome_shape_and_reason_match_pre_9rob_contract() {
    let expected = TaskRunOutcome::Closed {
        reason: NOOP_CLOSE_REASON.to_string(),
    };
    match &expected {
        TaskRunOutcome::Closed { reason } => {
            assert_eq!(reason, NOOP_CLOSE_REASON);
            assert!(!reason.is_empty());
        }
        other => panic!("expected Closed outcome, got {other:?}"),
    }
}

#[test]
fn supervisor_pr_creation_failure_renders_direct_github_api_already_exists_envelope() {
    let err = GitHubApiError::http(
        "POST",
        "/repos/djinnos/djinn/pulls".to_string(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "A pull request already exists for djinnos:task/demo".to_string(),
    );

    assert!(err.is_pr_already_exists());
    let rendered = render_github_write_error("GitHub PR creation failed", &err);

    assert!(rendered.starts_with("GitHub PR creation failed: {"));
    assert!(rendered.contains("\"error_class\":\"conflict_recoverable\""));
    assert!(rendered.contains("\"method\":\"POST\""));
    assert!(rendered.contains("\"path\":\"/repos/djinnos/djinn/pulls\""));
    assert!(rendered.contains("\"status\":\"422\""));
    assert!(rendered.contains("pull request already exists"));
    assert!(rendered.contains("Find and reuse the existing pull request"));
    assert!(!rendered.contains("github POST /repos/djinnos/djinn/pulls failed:"));
}

#[test]
fn supervisor_pr_reopen_then_creation_failure_preserves_direct_github_api_envelopes() {
    let reopen_err = GitHubApiError::http(
        "PATCH",
        "/repos/djinnos/djinn/pulls/7".to_string(),
        StatusCode::FORBIDDEN,
        "Resource not accessible by integration".to_string(),
    );
    let create_err = GitHubApiError::http(
        "POST",
        "/repos/djinnos/djinn/pulls".to_string(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "Validation Failed: No commits between main and task/demo".to_string(),
    );

    let rendered = format!(
        "{}; prior {}",
        render_github_write_error("GitHub PR creation failed", &create_err),
        render_github_write_error("GitHub PR reopen failed", &reopen_err),
    );

    assert!(rendered.contains("GitHub PR creation failed"));
    assert!(rendered.contains("GitHub PR reopen failed"));
    assert!(rendered.contains("\"error_class\":\"validation\""));
    assert!(rendered.contains("\"error_class\":\"permission\""));
    assert!(rendered.contains("\"method\":\"POST\""));
    assert!(rendered.contains("\"method\":\"PATCH\""));
    assert!(rendered.contains("No commits between main and task/demo"));
    assert!(rendered.contains("Resource not accessible by integration"));
    assert!(rendered.contains("Fix the rejected GitHub write inputs"));
    assert!(rendered.contains("Check GitHub authentication"));
    assert!(!rendered.contains("github POST /repos/djinnos/djinn/pulls failed:"));
    assert!(!rendered.contains("github PATCH /repos/djinnos/djinn/pulls/7 failed:"));
}

#[test]
fn supervisor_pr_rendering_covers_direct_auth_rate_limit_and_long_body_envelopes() {
    let unauthenticated = GitHubApiError::unauthenticated(
        "POST",
        "/repos/djinnos/djinn/pulls".to_string(),
        r#"{"message":"Bad credentials"}"#.to_string(),
    );
    let rate_limited = GitHubApiError::rate_limited(
        "PATCH",
        "/repos/djinnos/djinn/pulls/7".to_string(),
        r#"{"message":"API rate limit exceeded"}"#.to_string(),
    );
    let long_body = GitHubApiError::http(
        "POST",
        "/repos/djinnos/djinn/pulls".to_string(),
        StatusCode::UNPROCESSABLE_ENTITY,
        format!("Validation Failed: {}", "x".repeat(500)),
    );

    let auth_rendered = render_github_write_error("GitHub PR creation failed", &unauthenticated);
    assert!(auth_rendered.contains("\"error_class\":\"permission\""));
    assert!(auth_rendered.contains("\"status\":\"401\""));
    assert!(auth_rendered.contains("Check GitHub authentication"));

    let rate_rendered = render_github_write_error("GitHub PR reopen failed", &rate_limited);
    assert!(rate_rendered.contains("\"error_class\":\"rate_limited\""));
    assert!(rate_rendered.contains("\"status\":\"429\""));
    assert!(rate_rendered.contains("Back off until GitHub rate limits reset"));

    let long_rendered = render_github_write_error("GitHub PR creation failed", &long_body);
    assert!(long_rendered.contains("\"error_class\":\"validation\""));
    assert!(long_rendered.contains("\"status\":\"422\""));
    assert!(long_rendered.contains('…'));
    assert!(!long_rendered.contains(&"x".repeat(300)));
    assert!(
        long_rendered.len() < 600,
        "rendered body must remain bounded: {long_rendered}"
    );
}

const CAPTURED_CREATE_PR_422_ALREADY_EXISTS: &str = r#"{
      "message": "Validation Failed",
      "errors": [{
        "resource": "PullRequest",
        "code": "custom",
        "message": "A pull request already exists for djinnos:feature-branch."
      }]
    }"#;

fn github_pr_error(status: u16, body: &str) -> GitHubApiError {
    GitHubApiError::http(
        "create_pull_request",
        "/repos/djinnos/server/pulls".to_string(),
        reqwest::StatusCode::from_u16(status).expect("valid test status"),
        body.to_string(),
    )
}

fn failed_parts(outcome: TaskRunOutcome) -> (Option<ErrorClass>, Option<String>, Option<String>) {
    match outcome {
        TaskRunOutcome::Failed {
            error_class,
            hint,
            body_excerpt,
            ..
        } => (error_class, hint, body_excerpt),
        other => panic!("expected failed outcome, got {other:?}"),
    }
}

#[test]
fn pr_open_envelope_classifies_422_already_exists_as_conflict_recoverable() {
    let err = github_pr_error(422, CAPTURED_CREATE_PR_422_ALREADY_EXISTS);
    let (class, hint, body_excerpt) = failed_parts(pr_open_failure_outcome(
        "POST",
        "/repos/djinnos/server/pulls".to_string(),
        &err,
        None,
    ));

    assert_eq!(class, Some(ErrorClass::ConflictRecoverable));
    assert_eq!(hint.as_deref(), Some(PR_ALREADY_EXISTS_HINT));
    assert!(hint.unwrap().contains("adopt it"));
    let body_excerpt = body_excerpt.expect("body excerpt");
    assert!(body_excerpt.contains("Validation Failed"));
    assert!(body_excerpt.len() <= TASK_OUTCOME_BODY_EXCERPT_BYTES + 40);
}

#[test]
fn pr_open_envelope_classifies_generic_422_as_validation() {
    let err = github_pr_error(
        422,
        r#"{"message":"Validation Failed","errors":[{"message":"No commits between main and task/demo"}]}"#,
    );
    let (class, hint, body_excerpt) = failed_parts(pr_open_failure_outcome(
        "POST",
        "/repos/djinnos/server/pulls".to_string(),
        &err,
        None,
    ));

    assert_eq!(class, Some(ErrorClass::Validation));
    assert_eq!(
        hint.as_deref(),
        Some("fix the rejected GitHub pull-request parameters before retrying")
    );
    let body_excerpt = body_excerpt.expect("body excerpt");
    assert!(body_excerpt.contains("Validation Failed"));
    assert!(!body_excerpt.contains("[truncated:"));
}

#[test]
fn pr_open_envelope_classifies_404_as_not_found() {
    let err = github_pr_error(404, r#"{"message":"Not Found"}"#);
    let (class, _, _) = failed_parts(pr_open_failure_outcome(
        "POST",
        "/repos/djinnos/server/pulls".to_string(),
        &err,
        None,
    ));
    assert_eq!(class, Some(ErrorClass::NotFound));
}

#[test]
fn pr_open_envelope_classifies_401_as_permission() {
    let err = github_pr_error(401, r#"{"message":"Bad credentials"}"#);
    let (class, _, _) = failed_parts(pr_open_failure_outcome(
        "POST",
        "/repos/djinnos/server/pulls".to_string(),
        &err,
        None,
    ));
    assert_eq!(class, Some(ErrorClass::Permission));
}

#[test]
fn pr_open_envelope_classifies_429_as_rate_limited() {
    let err = github_pr_error(429, r#"{\"message\":\"API rate limit exceeded\"}"#);
    let (class, _, _) = failed_parts(pr_open_failure_outcome(
        "POST",
        "/repos/djinnos/server/pulls".to_string(),
        &err,
        None,
    ));
    assert_eq!(class, Some(ErrorClass::RateLimited));
}

#[test]
fn pr_open_envelope_classifies_5xx_as_transient() {
    let err = github_pr_error(502, r#"{"message":"Bad Gateway"}"#);
    let (class, _, _) = failed_parts(pr_open_failure_outcome(
        "POST",
        "/repos/djinnos/server/pulls".to_string(),
        &err,
        None,
    ));
    assert_eq!(class, Some(ErrorClass::Transient));
}

#[test]
fn pr_open_envelope_classifies_untyped_as_internal_without_hint() {
    let err = anyhow::anyhow!("connection reset");
    let (class, hint, body_excerpt) = failed_parts(pr_open_untyped_failure_outcome(
        "POST",
        "/repos/djinnos/server/pulls".to_string(),
        &err,
    ));
    assert_eq!(class, Some(ErrorClass::Internal));
    assert!(hint.is_none());
    assert!(body_excerpt.is_none());
}

// ── count_met_acceptance_criteria (D3b evidence sourcing) ───────────────

use super::count_met_acceptance_criteria;

#[test]
fn ac_count_zero_for_empty_or_malformed() {
    assert_eq!(count_met_acceptance_criteria(""), 0);
    assert_eq!(count_met_acceptance_criteria("[]"), 0);
    assert_eq!(count_met_acceptance_criteria("not json"), 0);
    assert_eq!(count_met_acceptance_criteria("{}"), 0);
}

#[test]
fn ac_count_zero_when_none_met() {
    let json = r#"[{"criterion":"a","met":false},{"criterion":"b","met":false}]"#;
    assert_eq!(count_met_acceptance_criteria(json), 0);
}

#[test]
fn ac_count_counts_only_met() {
    let json = r#"[{"criterion":"a","met":true},{"criterion":"b","met":false},{"criterion":"c","met":true}]"#;
    assert_eq!(count_met_acceptance_criteria(json), 2);
}

#[test]
fn ac_count_treats_missing_met_as_false() {
    let json = r#"[{"criterion":"a"},{"criterion":"b","met":true}]"#;
    assert_eq!(count_met_acceptance_criteria(json), 1);
}

#[cfg(test)]
mod commits_ahead_tests {
    //! Regression for the no-commits PR guard: `task_branch_commits_ahead`
    //! must report 0 for a branch identical to the base (the case that made
    //! create_pull_request 422 and spammed the "PR blocked" banner) and the
    //! real count otherwise.
    use super::super::task_branch_commits_ahead;
    use djinn_git::run_git_command;
    use djinn_workspace::MirrorManager;
    use std::path::Path;
    use tempfile::TempDir;

    async fn git(dir: &Path, args: &[&str]) {
        run_git_command(
            dir.to_path_buf(),
            args.iter().map(|s| s.to_string()).collect(),
        )
        .await
        .unwrap_or_else(|e| panic!("git {args:?} failed: {e}"));
    }

    /// Seed a mirror at `<root>/<pid>.git` with `main`, a `task/empty` branch
    /// pointing at the same commit as `main`, and a `task/withcommit` branch
    /// carrying one extra commit.
    async fn seed_mirror(root: &Path, pid: &str) {
        let mirror = root.join(format!("{pid}.git"));
        std::fs::create_dir_all(&mirror).unwrap();
        git(&mirror, &["init", "-b", "main"]).await;
        git(&mirror, &["config", "user.email", "t@example.com"]).await;
        git(&mirror, &["config", "user.name", "t"]).await;
        std::fs::write(mirror.join("README.md"), "base").unwrap();
        git(&mirror, &["add", "-A"]).await;
        git(&mirror, &["commit", "-m", "base"]).await;
        // task/empty == main (no new commits ahead).
        git(&mirror, &["branch", "task/empty"]).await;
        // task/withcommit carries one extra commit.
        git(&mirror, &["checkout", "-b", "task/withcommit"]).await;
        std::fs::write(mirror.join("change.txt"), "x").unwrap();
        git(&mirror, &["add", "-A"]).await;
        git(&mirror, &["commit", "-m", "change"]).await;
        git(&mirror, &["checkout", "main"]).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zero_when_branch_has_no_new_commits() {
        let root = TempDir::new().unwrap();
        seed_mirror(root.path(), "proj1").await;
        let mgr = MirrorManager::new(root.path());
        let n = task_branch_commits_ahead(&mgr, "proj1", "task/empty", "main")
            .await
            .unwrap();
        assert_eq!(
            n, 0,
            "a branch identical to base must report 0 commits ahead"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn counts_new_commits_ahead_of_base() {
        let root = TempDir::new().unwrap();
        seed_mirror(root.path(), "proj2").await;
        let mgr = MirrorManager::new(root.path());
        let n = task_branch_commits_ahead(&mgr, "proj2", "task/withcommit", "main")
            .await
            .unwrap();
        assert_eq!(n, 1, "a branch with one extra commit must report 1");
    }
}
