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

// ── Local gate block comment / detail formatting ─────────────────────────────

use super::{
    LOCAL_GATE_BLOCK_EVENT, format_local_gate_block_comment, gate_result_to_detail_json,
    truncate_for_comment,
};
use crate::local_gates::{GateOutcome, GatePlanResult, GateResult};
use std::time::Duration;

fn make_blocking_gate_result(gate_id: &'static str, outcome: GateOutcome) -> GateResult {
    let exit_code = match outcome {
        GateOutcome::Failed => Some(1),
        _ => None,
    };
    GateResult {
        gate_id,
        outcome,
        blocking: true,
        command: vec!["cargo".into(), "fmt".into(), "--check".into()],
        cwd: "server".into(),
        timeout: Duration::from_secs(120),
        exit_code,
        stdout_summary: String::new(),
        stderr_summary: "error: some file is not formatted".into(),
        duration: Some(Duration::from_millis(250)),
        artifact: None,
    }
}

#[test]
fn gate_result_detail_json_includes_required_fields() {
    let r = make_blocking_gate_result("rustfmt", GateOutcome::Failed);
    let detail = gate_result_to_detail_json(&r);

    assert_eq!(detail["gate_id"], "rustfmt");
    assert_eq!(detail["outcome"], "failed");
    assert_eq!(
        detail["command"],
        serde_json::json!(["cargo", "fmt", "--check"])
    );
    assert_eq!(detail["cwd"], "server");
    assert_eq!(detail["timeout_secs"], 120);
    assert_eq!(detail["exit_code"], 1);
    assert_eq!(
        detail["stderr_summary"],
        "error: some file is not formatted"
    );
    assert_eq!(detail["duration_ms"], 250);
    assert_eq!(
        detail["blocking_reason"],
        "required gate command exited non-zero"
    );
}

#[test]
fn gate_result_detail_json_unavailable_reason_is_distinct() {
    let r = make_blocking_gate_result("clippy", GateOutcome::Unavailable);
    let detail = gate_result_to_detail_json(&r);

    assert_eq!(detail["gate_id"], "clippy");
    assert_eq!(detail["outcome"], "unavailable");
    assert_eq!(
        detail["blocking_reason"],
        "required command or working directory unavailable"
    );
    assert!(detail["exit_code"].is_null());
}

#[test]
fn format_gate_block_comment_lists_all_blocking_gates() {
    let result = GatePlanResult {
        results: vec![
            make_blocking_gate_result("rustfmt", GateOutcome::Failed),
            make_blocking_gate_result("server-size-guard", GateOutcome::Unavailable),
            GateResult {
                gate_id: "advisory-lint",
                outcome: GateOutcome::Skipped,
                blocking: false,
                command: vec!["echo".into()],
                cwd: String::new(),
                timeout: Duration::from_secs(10),
                exit_code: None,
                stdout_summary: String::new(),
                stderr_summary: String::new(),
                duration: None,
                artifact: None,
            },
        ],
    };

    let comment =
        format_local_gate_block_comment("abc123", &["rustfmt", "server-size-guard"], &result);

    // Must mention the commit SHA.
    assert!(
        comment.contains("abc123"),
        "comment must mention the commit SHA: {comment}"
    );
    // Must list blocking gates.
    assert!(
        comment.contains("rustfmt"),
        "comment must mention rustfmt: {comment}"
    );
    assert!(
        comment.contains("server-size-guard"),
        "comment must mention server-size-guard: {comment}"
    );
    // Must NOT list advisory (non-blocking) gates.
    assert!(
        !comment.contains("advisory-lint"),
        "comment must not mention advisory gates: {comment}"
    );
    // Must show the unavailable status distinctly.
    assert!(
        comment.contains("unavailable"),
        "comment must mention 'unavailable' for unavailable gates: {comment}"
    );
    // Must include the blocked gate summary.
    assert!(
        comment.contains("Blocked gates:"),
        "comment must have a 'Blocked gates:' summary line: {comment}"
    );
}

#[test]
fn truncate_for_comment_preserves_short_strings() {
    assert_eq!(truncate_for_comment("hello world", 100), "hello world");
}

#[test]
fn truncate_for_comment_truncates_long_strings() {
    let long = "a".repeat(500);
    let truncated = truncate_for_comment(&long, 100);
    // max_len (100) + ellipsis char (3 UTF-8 bytes) = 103
    assert!(
        truncated.len() <= 103,
        "must truncate to max_len + ellipsis, got len {}",
        truncated.len()
    );
    assert!(truncated.ends_with('…'), "must end with ellipsis");
    assert!(truncated.starts_with("aaa"), "must preserve head of string");
}

#[test]
fn local_gate_block_event_type_is_stable() {
    // Pin the event type string so it is never accidentally changed (would
    // break activity-log dedup queries across deploys).
    assert_eq!(LOCAL_GATE_BLOCK_EVENT, "local_gate_block");
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
