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
