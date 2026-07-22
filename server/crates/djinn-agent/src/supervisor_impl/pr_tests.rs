use super::{
    PR_ALREADY_EXISTS_HINT, TASK_OUTCOME_BODY_EXCERPT_BYTES,
    adopt_recorded_pr_after_create_conflict, handle_noop_disposition,
    handle_settled_noop_without_live_mover, is_concurrent_push_race, pr_open_failure_outcome,
    pr_open_untyped_failure_outcome, should_route_settled_noop_without_live_mover,
};
use crate::github_error_render::render_github_write_error;
use crate::supervisor_impl::disposition::{
    LiveMoverEvidence, NUDGE_CAP, RunDisposition, decide_run_disposition,
};
use crate::test_helpers;
use djinn_core::models::Task;
use djinn_core::models::TransitionAction;
use djinn_core::run_progress::{RunProgressSignals, classify_run_progress};
use djinn_core::tool_error::ErrorClass;
use djinn_db::TaskRepository;
use djinn_git::GitError;
use djinn_provider::github_api::GitHubApiError;
use djinn_runtime::spec::TaskRunOutcome;
use reqwest::StatusCode;

pub(crate) use crate::supervisor_impl::disposition::NUDGE_CAP as TEST_NUDGE_CAP;

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

#[tokio::test]
async fn no_mover_call_site_routes_through_same_disposition_ladder_as_pr_open_fork() {
    let (repo, task) = no_op_nudge_fixture().await;
    repo.log_activity(
        Some(&task.id),
        "agent-supervisor",
        "worker",
        "work_submitted",
        &serde_json::json!({
            "summary": "wind down: finish the no-mover routing regression",
        })
        .to_string(),
    )
    .await
    .expect("log wind-down evidence");

    let signals = RunProgressSignals {
        commits_ahead: 0,
        files_changed: 0,
        ac_newly_satisfied: 0,
    };
    let progress = classify_run_progress(&signals);
    let expected = decide_run_disposition(progress, task.continuation_count, TEST_NUDGE_CAP);
    assert_eq!(expected, RunDisposition::Nudge);

    let outcome =
        handle_settled_noop_without_live_mover(&task, &repo, "main", &LiveMoverEvidence::default())
            .await
            .expect("no live mover routes to no-op disposition");

    assert!(
        matches!(
            (&expected, outcome),
            (RunDisposition::Nudge, TaskRunOutcome::Escalated { .. })
        ),
        "new no-mover call site must produce the same disposition as the PR-open zero-commit fork"
    );
    let body = latest_nudge_comment_body(&repo, &task.id).await;
    assert!(body.contains("Prior intent: wind down: finish the no-mover routing regression"));
    assert!(!body.contains("Prior intent: test task description"));
}

#[tokio::test]
async fn no_op_disposition_idempotency_two_nudges_then_historical_close() {
    let (repo, first_attempt) = no_op_nudge_fixture().await;

    let first_outcome = handle_noop_disposition(&first_attempt, &repo, "main").await;
    assert!(matches!(first_outcome, TaskRunOutcome::Escalated { .. }));
    let after_first = repo
        .get(&first_attempt.id)
        .await
        .expect("load task")
        .expect("task exists");
    assert_eq!(after_first.continuation_count, 1);
    assert_eq!(after_first.status, "open");
    let first_comments = nudge_comment_bodies(&repo, &first_attempt.id).await;
    assert_eq!(first_comments.len(), 1);
    assert!(first_comments[0].contains("corrective attempt 1/2"));

    let second_attempt = start_for_noop_attempt(&repo, &after_first).await;
    assert_eq!(second_attempt.continuation_count, 1);
    let second_outcome = handle_noop_disposition(&second_attempt, &repo, "main").await;
    assert!(matches!(second_outcome, TaskRunOutcome::Escalated { .. }));
    let after_second = repo
        .get(&first_attempt.id)
        .await
        .expect("load task")
        .expect("task exists");
    assert_eq!(after_second.continuation_count, 2);
    assert_eq!(after_second.status, "open");
    let second_comments = nudge_comment_bodies(&repo, &first_attempt.id).await;
    assert_eq!(second_comments.len(), 2);
    assert!(second_comments[0].contains("corrective attempt 1/2"));
    assert!(second_comments[1].contains("corrective attempt 2/2"));
    assert_ne!(second_comments[0], second_comments[1]);

    let third_attempt = start_for_noop_attempt(&repo, &after_second).await;
    assert_eq!(third_attempt.continuation_count, TEST_NUDGE_CAP);
    let third_outcome = handle_noop_disposition(&third_attempt, &repo, "main").await;
    assert!(matches!(third_outcome, TaskRunOutcome::Closed { .. }));
    let after_third = repo
        .get(&first_attempt.id)
        .await
        .expect("load task")
        .expect("task exists");
    assert_eq!(
        after_third.continuation_count, 2,
        "close path must not consume another nudge attempt"
    );
    assert_eq!(after_third.status, "closed");
    assert_eq!(
        nudge_comment_bodies(&repo, &first_attempt.id).await.len(),
        2,
        "third encounter closes via the historical close path without logging a third nudge"
    );
}

async fn nudge_comment_bodies(repo: &TaskRepository, task_id: &str) -> Vec<String> {
    let entries = repo.list_activity(task_id).await.expect("activity");
    entries
        .iter()
        .filter(|entry| entry.event_type == "comment")
        .filter_map(|entry| serde_json::from_str::<serde_json::Value>(&entry.payload).ok())
        .filter_map(|payload| {
            payload
                .get("body")
                .and_then(|body| body.as_str())
                .map(ToString::to_string)
        })
        .collect()
}

async fn start_for_noop_attempt(repo: &TaskRepository, task: &Task) -> Task {
    repo.transition(
        &task.id,
        TransitionAction::Start,
        "worker-1",
        "worker",
        None,
        None,
    )
    .await
    .expect("start task for no-op attempt")
}

#[test]
fn supervisor_nudge_cap_regression_is_reexported_and_locked() {
    assert_eq!(TEST_NUDGE_CAP, 2, "review any no-op nudge cap change");
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
        status: "in_progress".into(),
        priority: 1,
        owner: String::new(),
        labels: "[]".into(),
        acceptance_criteria: "[]".into(),
        reopen_count: 0,
        continuation_count: 0,
        total_reopen_count: 0,
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
        refinement_run_id: None,
        refinement_intent_id: None,
        refinement_generation: None,
        refinement_round: None,
        refinement_phase: None,
        refinement_role: None,
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

/// A create that 422s "already exists" while a concurrent PR-open has already
/// recorded pr_url on the task must heal to `PrOpened` instead of surfacing a
/// `Failed { pr_open }` conflict envelope (task mbfw, 2026-07-16).
#[tokio::test]
async fn create_conflict_heals_to_pr_opened_when_task_has_recorded_pr_url() {
    let db = test_helpers::create_test_db();
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), test_helpers::test_events());
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    repo.set_pr_url(&task.id, "https://github.com/djinnos/server/pull/7")
        .await
        .expect("set pr_url");

    let err = github_pr_error(422, CAPTURED_CREATE_PR_422_ALREADY_EXISTS);
    let outcome = adopt_recorded_pr_after_create_conflict(&err, &repo, &task, "deadbeef")
        .await
        .expect("conflict with a recorded pr_url must heal");

    match outcome {
        TaskRunOutcome::PrOpened { url, sha } => {
            assert_eq!(url, "https://github.com/djinnos/server/pull/7");
            assert_eq!(sha, "deadbeef");
        }
        other => panic!("expected PrOpened, got {other:?}"),
    }
}

/// Without a recorded pr_url there is nothing to adopt — the conflict must
/// fall through to the normal failure path (which is retried next cycle).
#[tokio::test]
async fn create_conflict_heal_declines_without_recorded_pr_url() {
    let db = test_helpers::create_test_db();
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), test_helpers::test_events());
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

    let err = github_pr_error(422, CAPTURED_CREATE_PR_422_ALREADY_EXISTS);
    assert!(
        adopt_recorded_pr_after_create_conflict(&err, &repo, &task, "deadbeef")
            .await
            .is_none(),
        "no recorded pr_url means nothing to adopt"
    );
}

/// The heal only applies to the "already exists" conflict — every other
/// create failure (e.g. no-commits 422) must keep its typed failure outcome.
#[tokio::test]
async fn create_conflict_heal_ignores_non_conflict_errors() {
    let db = test_helpers::create_test_db();
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let repo = TaskRepository::new(db.clone(), test_helpers::test_events());
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    repo.set_pr_url(&task.id, "https://github.com/djinnos/server/pull/7")
        .await
        .expect("set pr_url");

    let err = github_pr_error(
        422,
        r#"{"message":"Validation Failed: No commits between main and task/demo"}"#,
    );
    assert!(
        adopt_recorded_pr_after_create_conflict(&err, &repo, &task, "deadbeef")
            .await
            .is_none(),
        "non-already-exists failures must not be healed"
    );
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
