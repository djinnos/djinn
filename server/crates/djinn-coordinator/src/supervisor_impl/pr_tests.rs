//! Tests for the supervisor PR-open path.
//!
//! Focused unit tests for the unchanged-head red-CI remediation rejection
//! guard. The pure predicate [`unchanged_head_rejection_reason`] is tested
//! directly — it does not require a database.

use super::{
    LocalGateBlockKind, implicated_required_check_names, local_gate_block_kind,
    local_gate_block_reason, unchanged_head_rejection_reason,
};
use crate::local_gates::{
    LocalGateOutcome, LocalGateResult, LocalGateUnreproducible, LocalGateUnreproducibleReason,
};

// ── Unchanged-head remediation rejection predicate ──────────────────────────

/// When `ci_last_remediation_base_sha` matches the post-session head SHA, the
/// predicate returns `Some(reason)` — the submit must be rejected.
#[test]
fn unchanged_head_rejects_when_sha_matches_baseline() {
    let base_sha = "abc123def456789012345678901234567890abcd";
    let reason = unchanged_head_rejection_reason(
        Some(base_sha),
        base_sha,
        "task-uuid-1234",
        "itmo",
        Some(42),
    );

    let reason = reason.expect("unchanged head SHA must produce a rejection reason");

    // The reason must explain that no new commit was produced.
    assert!(
        reason.contains("unchanged"),
        "reason must mention 'unchanged': {reason}"
    );
    assert!(
        reason.to_lowercase().contains("no new commit was produced"),
        "reason must explain no new commit was produced: {reason}"
    );
    assert!(
        reason.contains("remediation"),
        "reason must mention remediation: {reason}"
    );
    // The unchanged head SHA must be present.
    assert!(
        reason.contains(base_sha),
        "reason must contain the unchanged head SHA: {reason}"
    );
    // The PR number must be present.
    assert!(
        reason.contains("PR #42"),
        "reason must contain the PR number: {reason}"
    );
    // The task short_id must be present.
    assert!(
        reason.contains("itmo"),
        "reason must contain the task short_id: {reason}"
    );
}

/// When the head SHA changed from the baseline, the predicate returns `None`
/// — the submit is NOT rejected and proceeds through the normal PR-open path.
#[test]
fn changed_head_does_not_take_rejection_path() {
    let base_sha = "abc123def456789012345678901234567890abcd";
    let new_sha = "fedcba9876543210fedcba9876543210fedcba98";

    let result = unchanged_head_rejection_reason(
        Some(base_sha),
        new_sha,
        "task-uuid-1234",
        "itmo",
        Some(42),
    );

    assert!(
        result.is_none(),
        "a changed head SHA must NOT produce a rejection reason"
    );
}

/// When no remediation baseline is active (`ci_last_remediation_base_sha` is
/// `None`), the predicate returns `None` — no rejection, regardless of the head
/// SHA. This is the common case: tasks that have never failed required CI.
#[test]
fn no_baseline_does_not_reject() {
    let head_sha = "abc123def456789012345678901234567890abcd";

    let result =
        unchanged_head_rejection_reason(None, head_sha, "task-uuid-1234", "itmo", Some(42));

    assert!(
        result.is_none(),
        "no remediation baseline must not produce a rejection"
    );
}

/// When the PR number is `None` (no snapshot PR number available), the
/// predicate still rejects — the reason message should contain the None PR
/// number representation but still carry the core fields.
#[test]
fn unchanged_head_rejects_without_pr_number() {
    let base_sha = "abc123def456789012345678901234567890abcd";
    let reason =
        unchanged_head_rejection_reason(Some(base_sha), base_sha, "task-uuid-1234", "itmo", None);

    assert!(
        reason.is_some(),
        "unchanged head must still reject even without a PR number"
    );

    let reason = reason.unwrap();
    assert!(
        reason.contains(base_sha),
        "reason must contain the unchanged head SHA: {reason}"
    );
}

/// AC1: The blocking system event payload structure must carry all required
/// fields: task_id, PR number, unchanged head SHA, remediation base SHA,
/// and the blocking reason. Verify the predicate produces a reason string
/// that includes every field the system event would emit.
#[test]
fn unchanged_head_rejection_includes_all_event_fields() {
    let base_sha = "abc123def456789012345678901234567890abcd";
    let task_id = "task-uuid-e2e-1234";
    let short_id = "w396";
    let pr_number = Some(42);

    let reason =
        unchanged_head_rejection_reason(Some(base_sha), base_sha, task_id, short_id, pr_number)
            .expect("unchanged head must produce a rejection reason");

    // The system event payload (emitted by check_unchanged_remediation_head)
    // includes: task_id, short_id, pr_number, head_sha, remediation_base_sha.
    // The reason string must carry the same information for human readability.
    assert!(
        reason.contains(base_sha),
        "must carry the unchanged head SHA"
    );
    assert!(
        reason.contains("PR #42"),
        "must carry the PR number from the event"
    );
    assert!(
        reason.contains(short_id),
        "must carry the task short_id from the event"
    );

    // The blocking reason must explain why the submit was rejected.
    assert!(
        reason.contains("unchanged"),
        "must state the head is unchanged"
    );
    assert!(
        reason.to_lowercase().contains("no new commit was produced"),
        "must explain that no new commit was produced"
    );
    assert!(
        reason.contains("remediation"),
        "must mention the task remains in remediation"
    );
}

/// AC1: The unchanged-head rejection preserves remediation state. When the
/// submit is rejected, the task must remain in remediation — the predicate
/// returns Some (indicating rejection) so the caller can keep the task parked.
/// The predicate must NOT return None (which would allow the submit to proceed).
#[test]
fn unchanged_head_preserves_remediation_state() {
    let base_sha = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    // The task is in remediation: ci_last_remediation_base_sha is set.
    // The submit pushes the same head SHA → must reject.
    let result = unchanged_head_rejection_reason(
        Some(base_sha),
        base_sha,
        "task-uuid-remediation",
        "itmo",
        Some(7),
    );

    // Returning Some means "reject the submit and keep the task in remediation."
    assert!(
        result.is_some(),
        "unchanged head must reject and preserve remediation state"
    );

    // The reason must not suggest the task is advancing.
    let reason = result.unwrap();
    assert!(
        !reason.to_lowercase().contains("advancing")
            && !reason.to_lowercase().contains("proceeding"),
        "rejection reason must not suggest the task is advancing: {reason}"
    );
    assert!(
        reason.contains("remains in remediation"),
        "reason must explicitly state the task remains in remediation: {reason}"
    );
}

/// The rejection reason is deterministic for the same inputs — calling the
/// predicate twice with identical args produces the same reason string.
#[test]
fn rejection_reason_is_deterministic() {
    let base_sha = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    let reason1 = unchanged_head_rejection_reason(
        Some(base_sha),
        base_sha,
        "task-uuid-9999",
        "w396",
        Some(7),
    );
    let reason2 = unchanged_head_rejection_reason(
        Some(base_sha),
        base_sha,
        "task-uuid-9999",
        "w396",
        Some(7),
    );

    assert_eq!(reason1, reason2, "rejection reason must be deterministic");
}

#[test]
fn local_gate_reproduced_failure_blocks_submit_and_approval() {
    let results = vec![LocalGateResult::ReproducedFailure(LocalGateOutcome {
        required_check_name: "Quality Gate".to_owned(),
        command: "scripts/ci.sh".to_owned(),
        exit_code: 1,
        log_tail: "assertion failed".to_owned(),
        observed_head_sha: "failing-head".to_owned(),
    })];

    assert_eq!(
        local_gate_block_kind(&results),
        Some(LocalGateBlockKind::ReproducedFailure)
    );
    let reason = local_gate_block_reason(LocalGateBlockKind::ReproducedFailure, &results);
    assert!(reason.contains("Required CI reproduced locally and failed"));
    assert!(reason.contains("scripts/ci.sh"));
    assert!(reason.contains("assertion failed"));
}

#[test]
fn local_gate_unreproducible_routes_to_lead_and_is_not_passing() {
    let results = vec![LocalGateResult::Unreproducible(LocalGateUnreproducible {
        required_check_name: "Quality Gate".to_owned(),
        observed_head_sha: "failing-head".to_owned(),
        reason: LocalGateUnreproducibleReason::ProviderUnreproducible,
        details: Some("workflow run not found".to_owned()),
    })];

    assert_eq!(
        local_gate_block_kind(&results),
        Some(LocalGateBlockKind::Unreproducible)
    );
    let reason = local_gate_block_reason(LocalGateBlockKind::Unreproducible, &results);
    assert!(reason.contains("could not be reproduced locally"));
    assert!(reason.contains("not treated as passing"));
    assert!(reason.contains("Routing to lead/human intervention"));
}

#[test]
fn local_gate_passes_only_when_every_implicated_check_reproduces_green() {
    let results = vec![LocalGateResult::ReproducedPass(LocalGateOutcome {
        required_check_name: "Quality Gate".to_owned(),
        command: "scripts/ci.sh".to_owned(),
        exit_code: 0,
        log_tail: "ok".to_owned(),
        observed_head_sha: "failing-head".to_owned(),
    })];

    assert_eq!(local_gate_block_kind(&results), None);
}

// ── Helper to build a minimal Task for pure predicate tests ──────────────

fn test_task(check_names_json: &str) -> djinn_core::models::Task {
    djinn_core::models::Task {
        id: "test-task-id".to_string(),
        project_id: "proj".to_string(),
        short_id: "t1".to_string(),
        epic_id: None,
        title: String::new(),
        description: String::new(),
        design: String::new(),
        issue_type: "task".to_string(),
        status: "pr_review".to_string(),
        priority: 0,
        owner: String::new(),
        labels: "[]".to_string(),
        acceptance_criteria: "[]".to_string(),
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
        memory_refs: "[]".to_string(),
        agent_type: None,
        created_by_user_id: None,
        ci_status: "unknown".to_string(),
        ci_head_sha: None,
        ci_pr_number: None,
        ci_blocking_required_check_names: check_names_json.to_string(),
        ci_failure_fingerprint: None,
        ci_first_seen_at: None,
        ci_last_seen_at: None,
        ci_same_signature_count: 0,
        ci_last_remediation_base_sha: None,
        unresolved_blocker_count: 0,
    }
}

// ── implicated_required_check_names parsing ──────────────────────────────

/// `implicated_required_check_names` correctly parses the JSON array stored
/// on the task's `ci_blocking_required_check_names` field.
#[test]
fn implicated_required_check_names_parses_json_array() {
    let task = test_task(&serde_json::to_string(&vec!["Quality Gate", "lint", "tests"]).unwrap());

    let names = implicated_required_check_names(&task);
    assert_eq!(names, vec!["Quality Gate", "lint", "tests"]);
}

/// Empty array yields empty vector — no implicated checks means the gate is skipped.
#[test]
fn implicated_required_check_names_empty_array_yields_empty() {
    let task = test_task("[]");

    let names = implicated_required_check_names(&task);
    assert!(names.is_empty());
}

/// Empty string / whitespace-only entries are filtered out.
#[test]
fn implicated_required_check_names_filters_empty_entries() {
    let task = test_task(&serde_json::to_string(&vec!["Quality Gate", "", "  ", "lint"]).unwrap());

    let names = implicated_required_check_names(&task);
    assert_eq!(names, vec!["Quality Gate", "lint"]);
}

/// Malformed JSON yields empty vector — defensive, never panics.
#[test]
fn implicated_required_check_names_malformed_json_yields_empty() {
    let task = test_task("not-json");

    let names = implicated_required_check_names(&task);
    assert!(names.is_empty());
}

// ── Reviewer pre-approve blocking (predicate-level) ──────────────────────

/// When any implicated check reproduces as a failure, the gate blocks
/// approval (not just submit). The same predicate is used for both paths.
#[test]
fn reviewer_pre_approve_blocks_on_reproduced_failure() {
    let results = vec![
        LocalGateResult::ReproducedPass(LocalGateOutcome {
            required_check_name: "lint".to_owned(),
            command: "cargo clippy".to_owned(),
            exit_code: 0,
            log_tail: "ok".to_owned(),
            observed_head_sha: "abc123".to_owned(),
        }),
        LocalGateResult::ReproducedFailure(LocalGateOutcome {
            required_check_name: "Quality Gate".to_owned(),
            command: "cargo test".to_owned(),
            exit_code: 1,
            log_tail: "FAILED: test_foo".to_owned(),
            observed_head_sha: "abc123".to_owned(),
        }),
    ];

    // Gate blocks even though one check passed.
    assert_eq!(
        local_gate_block_kind(&results),
        Some(LocalGateBlockKind::ReproducedFailure)
    );
    let reason = local_gate_block_reason(LocalGateBlockKind::ReproducedFailure, &results);
    assert!(reason.contains("cargo test"));
    assert!(reason.contains("FAILED: test_foo"));
    assert!(reason.contains("Submit/approval is blocked"));
}

/// An unreproducible check blocks approval and is never treated as passing.
/// The reason explicitly says it routes to lead/human intervention.
#[test]
fn reviewer_pre_approve_blocks_on_unreproducible_and_is_not_passing() {
    let results = vec![LocalGateResult::Unreproducible(LocalGateUnreproducible {
        required_check_name: "deploy-preview".to_owned(),
        observed_head_sha: "abc123".to_owned(),
        reason: LocalGateUnreproducibleReason::EmptyCommand,
        details: None,
    })];

    assert_eq!(
        local_gate_block_kind(&results),
        Some(LocalGateBlockKind::Unreproducible)
    );
    let reason = local_gate_block_reason(LocalGateBlockKind::Unreproducible, &results);
    assert!(
        reason.contains("not treated as passing"),
        "unreproducible must never be reported as passing: {reason}"
    );
    assert!(
        reason.contains("Routing to lead/human intervention"),
        "unreproducible must route to lead/human: {reason}"
    );
}

// ── Unreproducible priority over reproduced failure ─────────────────────

/// When results contain both unreproducible AND reproduced failure, the
/// gate reports unreproducible (which routes to lead/human intervention)
/// rather than just blocking on the failure. This ensures an unreproducible
/// check is always escalated, never silently masked by other failures.
#[test]
fn unreproducible_takes_priority_over_reproduced_failure() {
    let results = vec![
        LocalGateResult::ReproducedFailure(LocalGateOutcome {
            required_check_name: "tests".to_owned(),
            command: "cargo test".to_owned(),
            exit_code: 1,
            log_tail: "FAILED".to_owned(),
            observed_head_sha: "abc123".to_owned(),
        }),
        LocalGateResult::Unreproducible(LocalGateUnreproducible {
            required_check_name: "deploy".to_owned(),
            observed_head_sha: "abc123".to_owned(),
            reason: LocalGateUnreproducibleReason::SetupStepFailed,
            details: Some("apt-get install failed".to_owned()),
        }),
    ];

    assert_eq!(
        local_gate_block_kind(&results),
        Some(LocalGateBlockKind::Unreproducible),
        "unreproducible must take priority over reproduced failure"
    );
}

// ── Worker pre-submit blocking with multiple checks ─────────────────────

/// When multiple checks are implicated and all pass, the gate allows submit.
#[test]
fn worker_pre_submit_passes_when_all_checks_reproduce_green() {
    let results = vec![
        LocalGateResult::ReproducedPass(LocalGateOutcome {
            required_check_name: "Quality Gate".to_owned(),
            command: "cargo clippy --all-targets".to_owned(),
            exit_code: 0,
            log_tail: "ok".to_owned(),
            observed_head_sha: "abc123".to_owned(),
        }),
        LocalGateResult::ReproducedPass(LocalGateOutcome {
            required_check_name: "Tests".to_owned(),
            command: "cargo test".to_owned(),
            exit_code: 0,
            log_tail: "ok".to_owned(),
            observed_head_sha: "abc123".to_owned(),
        }),
    ];

    assert_eq!(local_gate_block_kind(&results), None);
}

/// Empty results vector (no implicated checks) means the gate is skipped —
/// returns None (no block).
#[test]
fn worker_pre_submit_skips_gate_when_no_implicated_checks() {
    let results: Vec<LocalGateResult> = vec![];
    assert_eq!(local_gate_block_kind(&results), None);
}

// ── Block reason includes command and log tail ──────────────────────────

/// The reproduced-failure block reason must include the command, exit code,
/// and log tail from each failed check so the worker/reviewer has actionable
/// context.
#[test]
fn block_reason_includes_actionable_context() {
    let results = vec![LocalGateResult::ReproducedFailure(LocalGateOutcome {
        required_check_name: "ci/test".to_owned(),
        command: "npm run test:ci".to_owned(),
        exit_code: 2,
        log_tail: "FAIL src/app.test.ts\nExpected: true\nReceived: false".to_owned(),
        observed_head_sha: "def456".to_owned(),
    })];

    let reason = local_gate_block_reason(LocalGateBlockKind::ReproducedFailure, &results);
    assert!(reason.contains("npm run test:ci"), "must include command");
    assert!(reason.contains("2"), "must include exit code");
    assert!(
        reason.contains("FAIL src/app.test.ts"),
        "must include log tail"
    );
}
