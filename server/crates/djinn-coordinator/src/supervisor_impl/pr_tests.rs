//! Tests for the supervisor PR-open path.
//!
//! Focused unit tests for the unchanged-head red-CI remediation rejection
//! guard. The pure predicate [`unchanged_head_rejection_reason`] is tested
//! directly — it does not require a database.
//!
//! m116 / `llvt` — also tests the publication-failure / mirror-divergence
//! short-circuit ([`super::unpublished_mirror_publication_reason`]) that
//! suppresses the false-strike unchanged-head rejection when the mirror
//! produced a commit but GitHub never received it.

use super::{
    UnchangedHeadContext, unchanged_head_rejection_reason, unpublished_mirror_publication_reason,
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

// ── m116 / llvt: publication-failure / mirror-divergence short-circuit predicate ─
//
// These tests verify the m116 behavior that suppresses the misleading
// unchanged-head escalation when mirror↔GitHub reconciliation evidence
// indicates a GitHub publication failure or head divergence. They are pure
// predicate tests (no DB), mirroring the AC#3 invariant:
//   "Coordinator tests cover the publication-failure stale-head false-strike
//    case and at least one non-diverged unchanged-head case that still
//    escalates normally."
//
// The "non-diverged unchanged-head case" is covered above by
// `unchanged_head_rejects_when_sha_matches_baseline` /
// `unchanged_head_preserves_remediation_state` — those remain green, proving
// that the publication-failure path does not swallow legitimate no-progress
// escalations.

/// Build a minimal `UnchangedHeadContext` for the publication-failure tests.
#[allow(clippy::too_many_arguments)]
fn pub_failure_ctx<'a>(
    head_sha: &'a str,
    remediation_base_sha: Option<&'a str>,
    mirror_head_sha: Option<&'a str>,
    github_head_sha: Option<&'a str>,
    heads_diverged: Option<bool>,
    head_observation_error: Option<&'a str>,
    pr_number: Option<i64>,
    short_id: &'a str,
) -> UnchangedHeadContext<'a> {
    UnchangedHeadContext {
        head_sha,
        remediation_base_sha,
        mirror_head_sha,
        github_head_sha,
        heads_diverged,
        head_observation_error,
        pr_number,
        short_id,
    }
}

#[test]
fn pub_failure_returns_none_without_remediation_baseline() {
    // No durable remediation baseline → cannot trigger the guard at all,
    // publication evidence or not.
    let ctx = pub_failure_ctx(
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        None,
        Some("mirror-newer-sha"),
        Some("github-older-sha"),
        Some(true),
        Some("auth failed"),
        Some(7),
        "itmo",
    );

    let result = unpublished_mirror_publication_reason(&ctx);
    assert!(
        result.is_none(),
        "no remediation baseline must not produce a publication-failure reason"
    );
}

#[test]
fn pub_failure_returns_none_when_head_sha_advanced() {
    // Worker DID push a new local mirror head → no publication failure.
    // This proves a legitimate progress round still allows the unchanged-head
    // rejection logic to fall through (it won't fire here either since the
    // head advanced, but the publication predicate must agree).
    let new_sha = "newly-pushed-sha";
    let old_sha = "old-remediation-base";
    let ctx = pub_failure_ctx(
        new_sha,
        Some(old_sha),
        Some(new_sha),
        Some(new_sha),
        Some(false),
        None,
        Some(42),
        "itmo",
    );

    assert!(
        unpublished_mirror_publication_reason(&ctx).is_none(),
        "an advanced head must not produce a publication-failure reason"
    );
}

#[test]
fn pub_failure_suppresses_when_mirror_advanced_and_github_lagging() {
    // The vy47 / aah4 shape:
    //  - remediation baseline = old failing GitHub PR head
    //  - freshly-pushed local head == baseline (push could not reach GitHub)
    //  - latest attempt evidence: mirror advanced past the baseline
    //  - latest attempt evidence: GitHub head == baseline (no advance)
    //  - heads_diverged = true
    // Expectation: predicate returns Some(reason) identifying divergence,
    // NOT "no new commit was produced".
    let baseline = "github-old-sha-failing-baseline";
    let mirror_advanced = "mirror-newly-pushed-sha";
    let ctx = pub_failure_ctx(
        baseline,
        Some(baseline),
        Some(mirror_advanced),
        Some(baseline),
        Some(true),
        None,
        Some(99),
        "itmo",
    );

    let reason = unpublished_mirror_publication_reason(&ctx)
        .expect("mirror-advanced + github-lagging + divergence flag → publication-failure reason");

    assert!(
        reason.contains("diverged") || reason.contains("divergence"),
        "reason must identify the divergence: {reason}"
    );
    assert!(
        reason.contains(mirror_advanced) || reason.contains("mirror"),
        "reason must reference the mirror head evidence: {reason}"
    );
    assert!(
        reason.contains(baseline),
        "reason must reference the failing remediation baseline: {reason}"
    );
    assert!(
        reason.contains("PR #99"),
        "reason must contain the PR number: {reason}"
    );
    assert!(
        reason.contains("itmo"),
        "reason must contain the task short_id: {reason}"
    );
    // Crucially: this MUST NOT look like a worker-no-commit rejection.
    assert!(
        !reason.contains("no new commit was produced"),
        "publication-failure reason must NOT borrow the no-commit-strike text: {reason}"
    );
    // And it must explicitly say the strike is suppressed.
    assert!(
        reason.contains("NOT") || reason.to_lowercase().contains("suppress"),
        "reason must make clear the unchanged-head strike is suppressed: {reason}"
    );
}

#[test]
fn pub_failure_suppresses_when_publication_error_recorded() {
    // Worker may not have an explicit mirror_head_sha yet, but the latest
    // attempt carried a publication observation error. Together with
    // freshly-pushed local head matching the durable baseline this is a
    // clear publication-failure signal.
    let baseline = "auth-rejected-baseline";
    let ctx = pub_failure_ctx(
        baseline,
        Some(baseline),
        None,
        Some(baseline),
        None,
        Some("HTTP 403: app installation suspended"),
        Some(123),
        "vy47",
    );

    let reason = unpublished_mirror_publication_reason(&ctx)
        .expect("head_observation_error alone (with github_lagging) must trigger");

    assert!(
        reason.contains("auth-rejected-baseline"),
        "reason must reference the failing remediation baseline: {reason}"
    );
    assert!(
        reason.contains("HTTP 403") || reason.contains("publication"),
        "reason must surface the publication error context: {reason}"
    );
    assert!(
        reason.contains("PR #123"),
        "reason must contain the PR number"
    );
    assert!(
        reason.contains("vy47"),
        "reason must contain the task short_id"
    );
}

#[test]
fn pub_failure_returns_none_when_no_divergence_or_publication_error() {
    // The mirror head == remediation baseline, github head == remediation
    // baseline, and nothing else. This is the *legitimate* unchanged-head
    // case where the worker truly produced no new commit. Publication
    // predicate must NOT swallow this — the unchanged-head rejection must
    // still fire.
    let baseline = "truly-unchanged-sha";
    let ctx = pub_failure_ctx(
        baseline,
        Some(baseline),
        Some(baseline),
        Some(baseline),
        Some(false),
        None,
        Some(7),
        "itmo",
    );

    assert!(
        unpublished_mirror_publication_reason(&ctx).is_none(),
        "no divergence / publication-error evidence → predicate must NOT suppress"
    );
}

#[test]
fn pub_failure_returns_none_without_active_worker_signal() {
    // Heads_diverged is Some(true) but `mirror_advanced` is false (mirror head
    // equals the baseline coincidentally), and no publication error. The
    // divergent flag alone isn't strong enough — we still need at least one
    // active signal (mirror advanced OR publication-error) to confirm the
    // worker did something GitHub didn't see.
    let baseline = "baseline-sha";
    let ctx = pub_failure_ctx(
        baseline,
        Some(baseline),
        Some(baseline),
        Some("some-other-github-head"),
        Some(true),
        None,
        Some(8),
        "itmo",
    );

    assert!(
        unpublished_mirror_publication_reason(&ctx).is_none(),
        "divergence flag without active worker signal must NOT trigger suppression"
    );
}

#[test]
fn pub_failure_returns_none_without_divergent_signal() {
    // Mirror advanced, but no divergence flag, no github head, AND no
    // publication error. Without at least one divergent signal
    // (heads_diverged, github_lagging, or pub_error), the predicate must
    // remain conservative and let the unchanged-head rejection fire.
    let baseline = "baseline-sha";
    let ctx = pub_failure_ctx(
        baseline,
        Some(baseline),
        Some("mirror-newer-sha"),
        None,
        None,
        None,
        Some(8),
        "itmo",
    );

    assert!(
        unpublished_mirror_publication_reason(&ctx).is_none(),
        "without divergence / lagging / pub_error, suppression must not trigger"
    );
}

#[test]
fn pub_failure_combines_mirror_advanced_with_publication_error() {
    // Both signals active (mirror advanced AND a publication error). The
    // reason must surface both pieces of evidence for operators.
    let baseline = "baseline-sha";
    let ctx = pub_failure_ctx(
        baseline,
        Some(baseline),
        Some("mirror-newer-sha"),
        Some(baseline),
        Some(true),
        Some("TLS handshake failed"),
        Some(11),
        "llvt",
    );

    let reason = unpublished_mirror_publication_reason(&ctx)
        .expect("both signals active → suppression must trigger");

    assert!(
        reason.contains("TLS handshake failed") || reason.contains("publication"),
        "reason must surface the publication error: {reason}"
    );
    assert!(
        reason.contains("mirror") || reason.contains("diverged"),
        "reason must surface the divergence evidence: {reason}"
    );
    assert!(
        !reason.contains("no new commit was produced"),
        "reason must NOT borrow the no-commit-strike text: {reason}"
    );
}

#[test]
fn pub_failure_suppresses_with_observation_error_unknown_github_head_mirror_advanced() {
    // The reviewer-requested regression case: head_observation_error=Some(...)
    // with github_head_sha=None and heads_diverged=None, but the mirror head
    // advanced. Before the fix this returned None (falling through to the
    // misleading unchanged-head rejection) because divergent required
    // explicit_divergence || github_lagging, both of which were false/unknown.
    //
    // Now has_pub_error itself qualifies as a divergent signal, so this must
    // suppress the unchanged-head rejection.
    let baseline = "baseline-sha";
    let mirror_new = "mirror-advanced-sha";
    let ctx = pub_failure_ctx(
        baseline,
        Some(baseline),
        Some(mirror_new),
        None, // github_head_sha unknown — no PR branch observed
        None, // heads_diverged unknown
        Some("HTTP 403: app installation token revoked"),
        Some(55),
        "llvt",
    );

    let reason = unpublished_mirror_publication_reason(&ctx).expect(
        "observation error + mirror advanced + unknown GitHub head → must suppress unchanged-head",
    );

    assert!(
        reason.contains("publication") || reason.contains("403") || reason.contains("revoked"),
        "reason must surface the publication/observation error: {reason}"
    );
    assert!(
        !reason.contains("no new commit was produced"),
        "reason must NOT borrow the no-commit-strike text: {reason}"
    );
    assert!(
        reason.contains("NOT") || reason.to_lowercase().contains("suppress"),
        "reason must make clear the unchanged-head strike is suppressed: {reason}"
    );
}

#[test]
fn pub_failure_suppresses_with_observation_error_unknown_github_head_mirror_unknown() {
    // Strongest minimal case: head_observation_error=Some(...) is the ONLY
    // signal — github_head_sha=None, heads_diverged=None, mirror_head_sha
    // is also None (no attempt recorded the mirror head yet). Before the fix
    // this returned None; now the publication error alone is sufficient.
    let baseline = "baseline-sha";
    let ctx = pub_failure_ctx(
        baseline,
        Some(baseline),
        None, // mirror_head_sha unknown
        None, // github_head_sha unknown
        None, // heads_diverged unknown
        Some("GitHub API rate limit exceeded; observation failed"),
        Some(77),
        "itmo",
    );

    let reason = unpublished_mirror_publication_reason(&ctx)
        .expect("observation error alone (all other heads unknown) → must suppress unchanged-head");

    assert!(
        reason.contains("rate limit")
            || reason.contains("publication")
            || reason.contains("observation"),
        "reason must surface the publication/observation error: {reason}"
    );
    assert!(
        !reason.contains("no new commit was produced"),
        "reason must NOT borrow the no-commit-strike text: {reason}"
    );
    assert!(
        reason.contains("NOT") || reason.to_lowercase().contains("suppress"),
        "reason must make clear the unchanged-head strike is suppressed: {reason}"
    );
}
