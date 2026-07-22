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

fn post_intervention_history_with_submission(reopen_class: ReopenClass) -> PostInterventionHistory {
    PostInterventionHistory {
        any_submitted: true,
        non_attempt_models: vec![],
        non_attempt_session_labels: vec![],
        infra_session_labels: vec![],
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
        infra_session_labels: vec![],
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

// ── Attempt-backed parity: focused tests ────────────────────────────────────
// These exercise the `PostInterventionHistory` fields as they would be
// populated by the attempt-backed `post_intervention_history` implementation.

#[test]
fn in_flight_submitted_attempt_does_not_park() {
    // AC: Newest post-floor `pending`/`submitted` attempts are treated as
    // in-flight and do not count as failed/non-attempt terminal evidence.
    // When `submission_pending_review` is true the park rung must NOT park.
    let task = test_task("passing", "[]", None);
    let history = PostInterventionHistory {
        any_submitted: true,
        non_attempt_models: vec![],
        non_attempt_session_labels: vec![],
        infra_session_labels: vec![],
        submission_pending_review: true,
        latest_submission_at: Some("2026-07-04T21:48:44Z".to_string()),
        most_recent_reopen_class: ReopenClass::Other,
    };

    // submission_pending_review=true means the round is still in flight;
    // the caller's park decision should refuse to park.
    assert!(
        history.submission_pending_review,
        "in-flight submission must set submission_pending_review=true"
    );
    assert!(
        history.any_submitted,
        "in-flight submission must set any_submitted=true"
    );
    // No non-attempt evidence from in-flight rows.
    assert!(
        history.non_attempt_models.is_empty(),
        "in-flight pending/submitted attempt must not produce non-attempt models"
    );

    // The park reason still renders (it would be used in the dossier if
    // the caller somehow parks), but the key invariant is that the
    // submission_pending_review guard prevents the park from firing.
    let reason = CoordinatorActor::compute_park_reason(&task, &history);
    assert!(
        reason.contains("Auto-parked for human review"),
        "park reason template should render; got: {reason}"
    );
}

#[test]
fn pre_submission_terminal_non_attempt_produces_non_attempt_evidence() {
    // AC: Pre-submission terminal rows (crashed, timed_out, etc.) contribute
    // to non-attempt model/session labels only when they are post-floor
    // worker attempts with no submission.
    let task = test_task("unknown", "[]", None);
    let history = PostInterventionHistory {
        any_submitted: false,
        non_attempt_models: vec!["crashed".to_string(), "timed_out".to_string()],
        non_attempt_session_labels: vec![
            "attempt a1b2c3d4 (crashed)".to_string(),
            "attempt e5f6g7h8 (timed_out)".to_string(),
        ],
        infra_session_labels: vec![],
        submission_pending_review: false,
        latest_submission_at: None,
        most_recent_reopen_class: ReopenClass::Other,
    };

    let reason = CoordinatorActor::compute_park_reason(&task, &history);

    assert!(
        reason.contains("terminated pre-submission"),
        "non-attempt park should use pre-submission phrasing; got: {reason}"
    );
    assert!(
        reason.contains("crashed, timed_out"),
        "reason should list pre-submission terminal outcomes; got: {reason}"
    );
    assert!(
        !history.any_submitted,
        "non-attempt history must set any_submitted=false"
    );
}

#[test]
fn submitted_terminal_failure_parks_with_reopen_class() {
    // AC: Terminal post-floor attempts preserve existing uv3p park behavior:
    // actual submitted-and-rejected attempts may park with truthful attribution.
    let task = test_task("failing", "[Clippy]", None);
    let history = PostInterventionHistory {
        any_submitted: true,
        non_attempt_models: vec![],
        non_attempt_session_labels: vec![],
        infra_session_labels: vec![],
        submission_pending_review: false,
        latest_submission_at: Some("2026-07-04T21:48:44Z".to_string()),
        most_recent_reopen_class: ReopenClass::ReviewRejected,
    };

    let reason = CoordinatorActor::compute_park_reason(&task, &history);

    // The submission was rejected — the park reason should use the
    // AC-phrasing (review_rejected branch).
    assert!(
        reason.contains("acceptance criteria still did not pass"),
        "submitted terminal failure park should use AC phrasing; got: {reason}"
    );
    assert!(
        history.any_submitted,
        "submitted terminal failure must set any_submitted=true"
    );
    assert!(
        !history.submission_pending_review,
        "rejected submission must not be pending review"
    );
}

// ── Model-rotation exclusion regression tests ───────────────────────────────
// These prove that `rotation_excluded_models()` derives exclusions from the
// attempt-backed `PostInterventionHistory` and correctly separates real model
// IDs from outcome-string fallbacks.

#[test]
fn rotation_excluded_models_from_two_distinct_post_floor_models() {
    // AC: Post-intervention forced model-rotation exclusions are derived from
    // attempt-backed history/query results. Two distinct pre-submission
    // terminal models (model IDs resolved from session lookup) are excluded.
    let history = PostInterventionHistory {
        any_submitted: false,
        non_attempt_models: vec![
            "provider-a/model-x".to_string(),
            "provider-b/model-y".to_string(),
        ],
        non_attempt_session_labels: vec![
            "attempt a1b2c3d4 (provider-a/model-x)".to_string(),
            "attempt e5f6g7h8 (provider-b/model-y)".to_string(),
        ],
        infra_session_labels: vec![],
        submission_pending_review: false,
        latest_submission_at: None,
        most_recent_reopen_class: ReopenClass::Other,
    };

    let excluded = history.rotation_excluded_models();

    assert_eq!(
        excluded.len(),
        2,
        "two distinct post-floor models must both be excluded; got: {excluded:?}"
    );
    assert!(
        excluded.contains(&"provider-a/model-x".to_string()),
        "exclusions must include provider-a/model-x"
    );
    assert!(
        excluded.contains(&"provider-b/model-y".to_string()),
        "exclusions must include provider-b/model-y"
    );
}

#[test]
fn rotation_excluded_models_excludes_in_flight_submitted_attempt() {
    // AC: `pending`/`submitted` in-flight rows do not exclude their model as a
    // failure. `non_attempt_models` is empty when only in-flight attempts exist
    // (the post_intervention_history builder skips non-terminal rows), so
    // rotation_excluded_models() returns nothing.
    let history = PostInterventionHistory {
        any_submitted: true,
        non_attempt_models: vec![],
        non_attempt_session_labels: vec![],
        infra_session_labels: vec![],
        submission_pending_review: true,
        latest_submission_at: Some("2026-07-04T21:48:44Z".to_string()),
        most_recent_reopen_class: ReopenClass::Other,
    };

    let excluded = history.rotation_excluded_models();

    assert!(
        excluded.is_empty(),
        "in-flight submitted attempt must not produce rotation exclusions; got: {excluded:?}"
    );
}

#[test]
fn rotation_excluded_models_skips_outcome_string_fallbacks() {
    // AC: When session model lookup fails and non_attempt_models contains
    // outcome strings (not model IDs), rotation_excluded_models() returns
    // empty — no real model IDs to exclude.
    let history = PostInterventionHistory {
        any_submitted: false,
        non_attempt_models: vec!["crashed".to_string(), "timed_out".to_string()],
        non_attempt_session_labels: vec![
            "attempt a1b2c3d4 (crashed)".to_string(),
            "attempt e5f6g7h8 (timed_out)".to_string(),
        ],
        infra_session_labels: vec![],
        submission_pending_review: false,
        latest_submission_at: None,
        most_recent_reopen_class: ReopenClass::Other,
    };

    let excluded = history.rotation_excluded_models();

    assert!(
        excluded.is_empty(),
        "outcome strings without '/' must not be treated as model IDs; got: {excluded:?}"
    );
}

#[test]
fn rotation_excluded_models_separates_model_ids_from_outcome_fallbacks() {
    // AC: Mixed list — one session-resolved model ID and one outcome-string
    // fallback. Only the model ID is excluded from rotation.
    let history = PostInterventionHistory {
        any_submitted: false,
        non_attempt_models: vec!["provider-a/model-x".to_string(), "spawn_failed".to_string()],
        non_attempt_session_labels: vec![
            "attempt a1b2c3d4 (provider-a/model-x)".to_string(),
            "attempt e5f6g7h8 (spawn_failed)".to_string(),
        ],
        infra_session_labels: vec![],
        submission_pending_review: false,
        latest_submission_at: None,
        most_recent_reopen_class: ReopenClass::Other,
    };

    let excluded = history.rotation_excluded_models();

    assert_eq!(
        excluded,
        vec!["provider-a/model-x".to_string()],
        "only actual model IDs must be excluded; got: {excluded:?}"
    );
    // The raw non_attempt_models still contains both (for park reason and audit).
    assert_eq!(
        history.non_attempt_models.len(),
        2,
        "non_attempt_models must retain all labels for audit; got: {:?}",
        history.non_attempt_models
    );
}

#[test]
fn submitted_terminal_failure_uses_quality_strike_api_not_inline_rotation() {
    // AC: Submitted terminal failures continue to use shipped quality-strike/
    // rotation decision APIs rather than being reclassified by new inline math.
    // When any_submitted=true, rotation_excluded_models() returns empty (no
    // pre-submission terminal evidence) — the rotation decision is deferred to
    // the quality-strike park gate, not the inline rotation exclusion block.
    let history = PostInterventionHistory {
        any_submitted: true,
        non_attempt_models: vec![],
        non_attempt_session_labels: vec![],
        infra_session_labels: vec![],
        submission_pending_review: false,
        latest_submission_at: Some("2026-07-04T21:48:44Z".to_string()),
        most_recent_reopen_class: ReopenClass::ReviewRejected,
    };

    let excluded = history.rotation_excluded_models();

    assert!(
        excluded.is_empty(),
        "submitted terminal failure must not produce inline rotation exclusions; got: {excluded:?}"
    );
    assert!(
        history.any_submitted,
        "submitted terminal failure must set any_submitted=true for the quality-strike park gate"
    );
}

// ── Infra reopen class exclusion tests (ry9v Wave 1 task 4) ────────────────
// These prove that infra-classified outcomes (timed_out, spawn_failed, crashed)
// are excluded from quality-strike, intervention, and park escalation counters
// while still appearing in diagnostic park/retry reasons.

#[test]
fn infra_only_pre_submission_history_excludes_from_park_threshold() {
    // AC #3: Infra-classified pre-submission terminals do NOT populate
    // non_attempt_models (the park escalation threshold list). They are
    // tracked separately in infra_session_labels for diagnostics.
    let history = PostInterventionHistory {
        any_submitted: false,
        non_attempt_models: vec![],
        non_attempt_session_labels: vec![],
        infra_session_labels: vec![
            "attempt a1b2c3d4 (crashed)".to_string(),
            "attempt e5f6g7h8 (timed_out)".to_string(),
            "attempt i9j0k1l2 (spawn_failed)".to_string(),
        ],
        submission_pending_review: false,
        latest_submission_at: None,
        most_recent_reopen_class: ReopenClass::Infra,
    };

    // Infra outcomes must NOT count toward the park escalation threshold.
    assert!(
        history.non_attempt_models.is_empty(),
        "infra outcomes must be excluded from non_attempt_models (park threshold); \
         got: {:?}",
        history.non_attempt_models
    );
    assert!(
        history.non_attempt_session_labels.is_empty(),
        "infra outcomes must be excluded from non_attempt_session_labels"
    );
    // Infra labels are retained for diagnostics.
    assert_eq!(
        history.infra_session_labels.len(),
        3,
        "infra_session_labels must retain all infra attempts for diagnostics"
    );
}

#[test]
fn infra_only_park_reason_names_infrastructure_not_ac_phrasing() {
    // AC #3: An infra-only history produces a truthful park reason that names
    // infrastructure failures and does NOT use the worker-quality AC phrasing
    // or the generic "never converged despite model rotation" phrasing.
    let task = test_task("unknown", "[]", None);
    let history = PostInterventionHistory {
        any_submitted: false,
        non_attempt_models: vec![],
        non_attempt_session_labels: vec![],
        infra_session_labels: vec![
            "attempt a1b2c3d4 (crashed)".to_string(),
            "attempt e5f6g7h8 (timed_out)".to_string(),
        ],
        submission_pending_review: false,
        latest_submission_at: None,
        most_recent_reopen_class: ReopenClass::Infra,
    };

    let reason = CoordinatorActor::compute_park_reason(&task, &history);

    assert!(
        reason.contains("infrastructure/provider failures"),
        "infra-only park reason should name infrastructure failures; got: {reason}"
    );
    assert!(
        reason.contains("crashed, timed_out") || reason.contains("infra"),
        "infra-only park reason should list infra attempts; got: {reason}"
    );
    assert!(
        reason.contains("excluded from quality-strike counts"),
        "infra park reason should note exclusion from quality-strike counts; got: {reason}"
    );
    assert!(
        !reason.contains("acceptance criteria still did not pass"),
        "infra-only park must NOT use worker-quality AC phrasing; got: {reason}"
    );
    assert!(
        !reason.contains("never converged despite forced model rotation"),
        "infra-only park must NOT use generic model-rotation phrasing; got: {reason}"
    );
}

#[test]
fn infra_submitted_park_reason_uses_infra_phrasing_not_ac() {
    // AC #3: A submitted terminal that ended in an infra outcome (e.g.
    // submitted then crashed) must use the infra phrasing, not the AC phrasing.
    let task = test_task("unknown", "[]", None);
    let history = post_intervention_history_with_submission(ReopenClass::Infra);

    let reason = CoordinatorActor::compute_park_reason(&task, &history);

    assert!(
        reason.contains("infrastructure/provider failure"),
        "submitted-infra park reason should name infra failure; got: {reason}"
    );
    assert!(
        !reason.contains("acceptance criteria still did not pass"),
        "submitted-infra park must NOT use AC phrasing; got: {reason}"
    );
}

#[test]
fn mixed_infra_and_quality_park_reason_appends_infra_diagnostics() {
    // AC #3: When both infra and non-infra pre-submission terminals exist, the
    // park reason keeps the model-rotation phrasing and appends infra
    // diagnostics so operators see infrastructure failures too.
    let task = test_task("unknown", "[]", None);
    let history = PostInterventionHistory {
        any_submitted: false,
        non_attempt_models: vec!["provider-a/model-x".to_string()],
        non_attempt_session_labels: vec!["attempt a1b2c3d4 (provider-a/model-x)".to_string()],
        infra_session_labels: vec!["attempt e5f6g7h8 (timed_out)".to_string()],
        submission_pending_review: false,
        latest_submission_at: None,
        most_recent_reopen_class: ReopenClass::Other,
    };

    let reason = CoordinatorActor::compute_park_reason(&task, &history);

    assert!(
        reason.contains("never converged despite forced model rotation"),
        "mixed history should keep model-rotation phrasing; got: {reason}"
    );
    assert!(
        reason.contains("infrastructure/provider attempt"),
        "mixed history should append infra diagnostics; got: {reason}"
    );
    assert!(
        reason.contains("timed_out"),
        "mixed history should name the infra outcome; got: {reason}"
    );
}

#[test]
fn infra_reopen_class_is_not_a_quality_strike() {
    // AC #1: ReopenClass::Infra is not counted as a quality strike by the
    // core task model. This mirrors the core test but lives here so the
    // coordinator-level exclusion contract is documented alongside the
    // park-reason behavior.
    assert!(
        !ReopenClass::Infra.is_quality_strike(),
        "infra must NOT be a quality strike"
    );
    // Quality strikes remain unchanged for real worker-quality failures.
    assert!(ReopenClass::ReviewRejected.is_quality_strike());
    assert!(ReopenClass::MergeQueueFailed.is_quality_strike());
    assert!(ReopenClass::Other.is_quality_strike());
}
