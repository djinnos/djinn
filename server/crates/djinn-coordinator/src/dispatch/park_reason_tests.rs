//! Regression tests for the truthful park-reason branching added in uv3p Part B.
//!
//! These tests directly exercise `park_reason_detail` / `compute_park_reason` so
//! the 3t22 misattribution (merge-queue failure folded into the generic AC
//! phrasing) stays fixed without needing a full DB-backed coordinator harness.
use crate::CoordinatorActor;
use crate::dispatch::retry::PostInterventionHistory;
use djinn_core::models::ReopenClass;

/// Build a minimal `Task` with the CI fields used by the park reason builder.
fn test_task(
    ci_status: &str,
    ci_blocking_required_check_names: &str,
    ci_failure_fingerprint: Option<&str>,
) -> djinn_core::models::Task {
    djinn_core::models::Task {
        id: "task-3t22".to_string(),
        project_id: "proj-3t22".to_string(),
        short_id: "3t22".to_string(),
        epic_id: None,
        title: "3t22 merge-queue park reason regression".to_string(),
        description: "desc".to_string(),
        design: "design".to_string(),
        issue_type: "task".to_string(),
        status: "open".to_string(),
        priority: 2,
        owner: "system".to_string(),
        labels: "[]".to_string(),
        acceptance_criteria: "[]".to_string(),
        reopen_count: 4,
        continuation_count: 0,
        total_reopen_count: 4,
        intervention_count: 1,
        last_intervention_at: Some("2026-07-04T21:45:00Z".to_string()),
        created_at: "2026-07-04T20:00:00Z".to_string(),
        updated_at: "2026-07-04T22:09:32Z".to_string(),
        closed_at: None,
        close_reason: None,
        merge_commit_sha: None,
        pr_url: None,
        merge_conflict_metadata: None,
        memory_refs: "[]".to_string(),
        agent_type: None,
        created_by_user_id: None,
        ci_status: ci_status.to_string(),
        ci_head_sha: Some("head-sha-3t22".to_string()),
        ci_pr_number: Some(42),
        ci_blocking_required_check_names: ci_blocking_required_check_names.to_string(),
        ci_failure_fingerprint: ci_failure_fingerprint.map(|s| s.to_string()),
        ci_first_seen_at: Some("2026-07-04T22:05:00Z".to_string()),
        ci_last_seen_at: Some("2026-07-04T22:09:00Z".to_string()),
        ci_same_signature_count: 2,
        ci_last_remediation_base_sha: None,
        unresolved_blocker_count: 0,
    }
}

fn post_intervention_history_with_submission(reopen_class: ReopenClass) -> PostInterventionHistory {
    PostInterventionHistory {
        any_submitted: true,
        non_attempt_models: vec![],
        non_attempt_session_labels: vec![],
        submission_pending_review: false,
        latest_submission_at: Some("2026-07-04T21:48:44Z".to_string()),
        most_recent_reopen_class: reopen_class,
    }
}

#[test]
fn merge_queue_failed_park_reason_names_merge_queue_failure_and_checks() {
    // 3t22 shape: post-intervention approval, then merge_queue_failed reopen.
    let task = test_task(
        "passing",
        "[Server Test, Integration Test]",
        Some("server-test::head-sha-3t22"),
    );
    let history = post_intervention_history_with_submission(ReopenClass::MergeQueueFailed);

    let reason = CoordinatorActor::compute_park_reason(&task, &history);

    assert!(
        reason.contains("merge-queue full suite failed"),
        "merge_queue_failed park should name the merge-queue failure; got: {reason}"
    );
    assert!(
        reason.contains("Server Test"),
        "reason should include the failing check names; got: {reason}"
    );
    assert!(
        reason.contains("server-test::head-sha-3t22"),
        "reason should include the fingerprint when available; got: {reason}"
    );
    assert!(
        reason.contains("approved by review"),
        "reason should note the post-intervention work was approved; got: {reason}"
    );
    assert!(
        !reason.contains("acceptance criteria still did not pass"),
        "merge_queue_failed park must NOT use the AC phrasing; got: {reason}"
    );
    // PR-head CI shows passing-with-skips, so the skip caveat must be present.
    assert!(
        reason.contains("PR-head CI status currently shows passing"),
        "reason should warn about passing-with-skips; got: {reason}"
    );
}

#[test]
fn review_rejected_park_reason_keeps_ac_phrasing() {
    let task = test_task("failing", "[Clippy]", None);
    let history = post_intervention_history_with_submission(ReopenClass::ReviewRejected);

    let reason = CoordinatorActor::compute_park_reason(&task, &history);

    assert!(
        reason.contains("acceptance criteria still did not pass"),
        "review_rejected park should keep the AC phrasing; got: {reason}"
    );
    assert!(
        !reason.contains("merge-queue full suite failed"),
        "review_rejected park must NOT mention merge-queue failure; got: {reason}"
    );
}

#[test]
fn merge_queue_failed_with_unknown_checks_falls_back_gracefully() {
    let task = test_task("failing", "[]", None);
    let history = post_intervention_history_with_submission(ReopenClass::MergeQueueFailed);

    let reason = CoordinatorActor::compute_park_reason(&task, &history);

    assert!(
        reason.contains("unknown check(s)"),
        "reason should fall back to unknown check(s) when no check names are available; got: {reason}"
    );
    assert!(
        reason.contains("merge-queue full suite failed"),
        "merge_queue_failed park should still name the merge-queue failure; got: {reason}"
    );
    assert!(
        !reason.contains("acceptance criteria still did not pass"),
        "merge_queue_failed park must NOT use the AC phrasing even with unknown checks; got: {reason}"
    );
}

#[test]
fn non_attempt_history_uses_rotation_phrasing() {
    let task = test_task("unknown", "[]", None);
    let history = PostInterventionHistory {
        any_submitted: false,
        non_attempt_models: vec!["model-a".to_string(), "model-b".to_string()],
        non_attempt_session_labels: vec!["sess aaaaaaaa (model-a)".to_string()],
        submission_pending_review: false,
        latest_submission_at: None,
        most_recent_reopen_class: ReopenClass::ReviewRejected,
    };

    let reason = CoordinatorActor::compute_park_reason(&task, &history);

    assert!(
        reason.contains("terminated pre-submission"),
        "non-attempt history should use pre-submission phrasing; got: {reason}"
    );
    assert!(
        reason.contains("model-a, model-b"),
        "reason should list the models that failed to submit; got: {reason}"
    );
}
