//! Tests for the supervisor PR-open path.
//!
//! Focused unit tests for the unchanged-head red-CI remediation rejection
//! guard. The pure predicate [`unchanged_head_rejection_reason`] is tested
//! directly — it does not require a database.

use super::unchanged_head_rejection_reason;

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
