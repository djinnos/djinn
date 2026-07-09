// Tests for the tripwire active-hold gate at the pre-merge boundary.
//
// These verify that the pure computation used by the merge-boundary
// active-hold check in `poll_pr_review_tasks` correctly blocks, allows,
// or detects tamper for each acceptance criterion.

use crate::tripwires::active_hold::{
    ActivityEntryRef, check_label_tamper, compute_active_hold_state,
};
use crate::tripwires::activity_payloads::{
    TripwireEvidenceSpan, TripwireFindingSummary, TripwireGateDecisionPayload,
    TripwireHoldReleasedPayload, TripwireSeverity,
};
use crate::tripwires::{
    TRIPWIRE_EVENT_GATE_HELD, TRIPWIRE_EVENT_GATE_PASSED, TRIPWIRE_EVENT_HOLD_RELEASED,
};

// ── Fixture constants ─────────────────────────────────────────────────────

const TASK_ID: &str = "task-merge-gate-001";
const PROJECT_ID: &str = "proj-merge-gate-001";
const PR_NUMBER: u64 = 77;
const POLICY_REV: &str = "org-policy:22";
const CURRENT_SHA: &str = "sha-current-abc123";
const OLD_SHA: &str = "sha-old-def456";

// ── Fixture helpers ───────────────────────────────────────────────────────

fn enforcement_finding(rule_id: &str, path: &str, key: &str) -> TripwireFindingSummary {
    TripwireFindingSummary {
        rule_id: rule_id.to_owned(),
        reason_code: format!("tripwire.{rule_id}.changed"),
        severity: TripwireSeverity::HumanReviewRequired,
        evidence: TripwireEvidenceSpan::file(path),
        idempotency_key: key.to_owned(),
        content_fingerprint: format!("fp:{key}"),
        downgrade_reason: None,
    }
}

fn report_only_finding(rule_id: &str, path: &str, key: &str) -> TripwireFindingSummary {
    TripwireFindingSummary {
        rule_id: rule_id.to_owned(),
        reason_code: format!("tripwire.{rule_id}.changed"),
        severity: TripwireSeverity::ReportOnly,
        evidence: TripwireEvidenceSpan::file(path),
        idempotency_key: key.to_owned(),
        content_fingerprint: format!("fp:{key}"),
        downgrade_reason: None,
    }
}

fn gate_held_payload(
    head_sha: &str,
    findings: Vec<TripwireFindingSummary>,
) -> TripwireGateDecisionPayload {
    let enforcement_count = findings
        .iter()
        .filter(|f| f.severity == TripwireSeverity::HumanReviewRequired)
        .count() as u32;
    let report_only_count = findings
        .iter()
        .filter(|f| f.severity == TripwireSeverity::ReportOnly)
        .count() as u32;
    TripwireGateDecisionPayload {
        event_type: TRIPWIRE_EVENT_GATE_HELD.to_owned(),
        task_id: TASK_ID.to_owned(),
        project_id: PROJECT_ID.to_owned(),
        pr_number: Some(PR_NUMBER),
        head_sha: head_sha.to_owned(),
        base_sha: None,
        policy_revision: POLICY_REV.to_owned(),
        allowlist_revision: None,
        findings,
        enforcement_finding_count: enforcement_count,
        report_only_finding_count: report_only_count,
        idempotency_key: format!("sha256:gate:{head_sha}"),
        decided_at: Some("2026-07-09T10:00:00Z".to_owned()),
    }
}

fn gate_held_entry(
    head_sha: &str,
    findings: Vec<TripwireFindingSummary>,
    created_at: &str,
) -> ActivityEntryRef {
    let payload = gate_held_payload(head_sha, findings);
    ActivityEntryRef {
        event_type: TRIPWIRE_EVENT_GATE_HELD.to_owned(),
        payload: serde_json::to_string(&payload).unwrap_or_default(),
        created_at: created_at.to_owned(),
    }
}

fn hold_released_entry(
    head_sha: &str,
    released_findings: Vec<TripwireFindingSummary>,
    created_at: &str,
) -> ActivityEntryRef {
    let payload = TripwireHoldReleasedPayload {
        event_type: TRIPWIRE_EVENT_HOLD_RELEASED.to_owned(),
        task_id: TASK_ID.to_owned(),
        project_id: PROJECT_ID.to_owned(),
        pr_number: Some(PR_NUMBER),
        head_sha: head_sha.to_owned(),
        policy_revision: POLICY_REV.to_owned(),
        released_by: "user-lead".to_owned(),
        released_by_role: "lead".to_owned(),
        rationale: "approved after review".to_owned(),
        released_findings,
        carried_forward: false,
        idempotency_key: format!("sha256:release:{head_sha}"),
        released_at: Some(created_at.to_owned()),
    };
    ActivityEntryRef {
        event_type: TRIPWIRE_EVENT_HOLD_RELEASED.to_owned(),
        payload: serde_json::to_string(&payload).unwrap_or_default(),
        created_at: created_at.to_owned(),
    }
}

fn gate_passed_entry(head_sha: &str, created_at: &str) -> ActivityEntryRef {
    ActivityEntryRef {
        event_type: TRIPWIRE_EVENT_GATE_PASSED.to_owned(),
        payload: format!("{{\"head_sha\":\"{head_sha}\"}}"),
        created_at: created_at.to_owned(),
    }
}

// ── AC: CI-green but held → blocks merge ──────────────────────────────────

/// When CI is green for the current head but a tripwire gate-held event
/// with enforcement findings exists for that head, `compute_active_hold_state`
/// must report `held == true` so the merge-boundary gate blocks.
#[test]
fn ci_green_but_held_blocks_merge() {
    let findings = vec![
        enforcement_finding("migration_change", "migrations/001.sql", "key-mig"),
        enforcement_finding("unsafe_code_change", "src/native.rs", "key-unsafe"),
    ];
    let entries = vec![
        // Simulates CI passing (an earlier gate-passed event or no event at all;
        // the gate-passed event doesn't affect hold state computation)
        gate_passed_entry(CURRENT_SHA, "2026-07-09T09:00:00Z"),
        // Gate held with enforcement findings for the current head
        gate_held_entry(CURRENT_SHA, findings, "2026-07-09T10:00:00Z"),
    ];

    let state = compute_active_hold_state(&entries, CURRENT_SHA);

    assert!(
        state.held,
        "active hold must block merge when enforcement findings exist for current head"
    );
    assert_eq!(state.head_sha, CURRENT_SHA);
    assert_eq!(state.active_findings.len(), 2);
    assert!(
        state
            .active_findings
            .iter()
            .any(|f| f.rule_id.as_str() == "migration_change")
    );
    assert!(
        state
            .active_findings
            .iter()
            .any(|f| f.rule_id.as_str() == "unsafe_code_change")
    );
    assert_eq!(state.policy_revision.as_deref(), Some(POLICY_REV));
    assert!(state.gate_idempotency_key.is_some());
}

/// A merge-boundary active-hold check must also block when the held event
/// contains only enforcement findings (no report-only mix).
#[test]
fn ci_green_held_with_only_enforcement_findings_blocks() {
    let findings = vec![enforcement_finding(
        "dependency_identity_change",
        "Cargo.toml",
        "key-dep",
    )];
    let entries = vec![gate_held_entry(
        CURRENT_SHA,
        findings,
        "2026-07-09T10:00:00Z",
    )];

    let state = compute_active_hold_state(&entries, CURRENT_SHA);
    assert!(state.held, "must block on single enforcement finding");
    assert_eq!(state.active_findings.len(), 1);
}

// ── AC: Released current-head → allows merge path ─────────────────────────

/// When the gate-held findings for the current head are all released via a
/// `tripwire.hold.released` event, `held == false` and the merge path may
/// proceed.
#[test]
fn released_current_head_allows_merge() {
    let f1 = enforcement_finding("migration_change", "migrations/001.sql", "key-mig");
    let f2 = enforcement_finding("unsafe_code_change", "src/native.rs", "key-unsafe");
    let entries = vec![
        gate_held_entry(
            CURRENT_SHA,
            vec![f1.clone(), f2.clone()],
            "2026-07-09T10:00:00Z",
        ),
        hold_released_entry(
            CURRENT_SHA,
            vec![f1.clone(), f2.clone()],
            "2026-07-09T11:00:00Z",
        ),
    ];

    let state = compute_active_hold_state(&entries, CURRENT_SHA);
    assert!(
        !state.held,
        "must allow merge when all enforcement findings are released"
    );
    assert!(state.is_clear());
    assert!(state.active_findings.is_empty());
    // Released keys are tracked for audit.
    assert!(state.released_finding_keys.contains("key-mig"));
    assert!(state.released_finding_keys.contains("key-unsafe"));
}

/// Partial release (only one of two findings) must still hold.
#[test]
fn partial_release_still_blocks() {
    let f1 = enforcement_finding("migration_change", "migrations/001.sql", "key-mig");
    let f2 = enforcement_finding("unsafe_code_change", "src/native.rs", "key-unsafe");
    let entries = vec![
        gate_held_entry(
            CURRENT_SHA,
            vec![f1.clone(), f2.clone()],
            "2026-07-09T10:00:00Z",
        ),
        // Only release f1 — f2 remains held
        hold_released_entry(CURRENT_SHA, vec![f1.clone()], "2026-07-09T11:00:00Z"),
    ];

    let state = compute_active_hold_state(&entries, CURRENT_SHA);
    assert!(state.held, "must still block when only partial release");
    assert_eq!(state.active_findings.len(), 1);
    assert_eq!(
        state.active_findings[0].rule_id.as_str(),
        "unsafe_code_change"
    );
}

// ── AC: Stale release after push → current head remains blocked ───────────

/// When the PR head advances (new push), a release event for the OLD head
/// must not clear the hold on the NEW head. The old head's state is
/// irrelevant (superseded by the newer head).
#[test]
fn stale_release_after_push_blocks_current_head() {
    let f_old = enforcement_finding("migration_change", "migrations/001.sql", "key-mig-old");
    let f_new = enforcement_finding("migration_change", "migrations/002.sql", "key-mig-new");

    let entries = vec![
        // Gate held for OLD head
        gate_held_entry(OLD_SHA, vec![f_old.clone()], "2026-07-09T08:00:00Z"),
        // Gate held for NEW head (after push)
        gate_held_entry(CURRENT_SHA, vec![f_new.clone()], "2026-07-09T09:00:00Z"),
        // Release for OLD head (stale — doesn't affect current head)
        hold_released_entry(OLD_SHA, vec![f_old.clone()], "2026-07-09T10:00:00Z"),
    ];

    // Old head is released
    let old_state = compute_active_hold_state(&entries, OLD_SHA);
    assert!(!old_state.held, "old head should be released");

    // Current head is STILL held — release was for a different head
    let current_state = compute_active_hold_state(&entries, CURRENT_SHA);
    assert!(
        current_state.held,
        "current head must remain held after push — stale release must not clear new head"
    );
    assert_eq!(current_state.active_findings.len(), 1);
    assert_eq!(
        current_state.active_findings[0].idempotency_key,
        "key-mig-new"
    );
}

/// A release event that matches the head SHA but uses finding idempotency
/// keys from a different gate evaluation must not clear the current head's
/// findings.
#[test]
fn release_with_wrong_finding_keys_does_not_clear() {
    let f_current = enforcement_finding("migration_change", "migrations/002.sql", "key-mig-v2");
    let f_stale = enforcement_finding("migration_change", "migrations/001.sql", "key-mig-v1");

    let entries = vec![
        gate_held_entry(CURRENT_SHA, vec![f_current.clone()], "2026-07-09T09:00:00Z"),
        // Release for current SHA but with stale finding keys
        hold_released_entry(CURRENT_SHA, vec![f_stale], "2026-07-09T10:00:00Z"),
    ];

    let state = compute_active_hold_state(&entries, CURRENT_SHA);
    assert!(
        state.held,
        "must remain held when release uses mismatched finding keys"
    );
    assert_eq!(state.active_findings.len(), 1);
}

// ── AC: Missing-label reapplication at merge boundary ─────────────────────

/// When an active hold exists but the task does not carry the
/// `human-review-hold` label (removed outside the release path),
/// `check_label_tamper` must detect the tamper and produce a payload
/// for logging.
#[test]
fn missing_label_reapplication_detected() {
    let findings = vec![enforcement_finding(
        "ci_workflow_change",
        ".github/workflows/ci.yml",
        "key-ci",
    )];
    let entries = vec![gate_held_entry(
        CURRENT_SHA,
        findings,
        "2026-07-09T10:00:00Z",
    )];
    let state = compute_active_hold_state(&entries, CURRENT_SHA);
    assert!(state.held, "must be held");

    // Simulate label missing (task_has_hold_label = false)
    let recon = check_label_tamper(
        &state,
        TASK_ID,
        PROJECT_ID,
        Some(PR_NUMBER),
        false, // label was removed
        "final_pre_merge_check",
        "2026-07-09T12:00:00Z",
    );

    assert!(recon.tamper_detected, "must detect label tamper");
    let payload = recon.payload.expect("tamper payload must be present");
    assert_eq!(payload.task_id, TASK_ID);
    assert_eq!(payload.project_id, PROJECT_ID);
    assert_eq!(payload.pr_number, Some(PR_NUMBER));
    assert_eq!(payload.head_sha, CURRENT_SHA);
    assert_eq!(payload.removed_by, "unknown");
    assert_eq!(payload.detection_source, "final_pre_merge_check");
    assert!(payload.reapplied, "must signal reapplication");
    assert_eq!(payload.tampered_findings.len(), 1);
    assert_eq!(
        payload.tampered_findings[0].rule_id.as_str(),
        "ci_workflow_change"
    );
}

/// When the task DOES carry the label, no tamper is detected even with an
/// active hold.
#[test]
fn label_present_no_tamper() {
    let findings = vec![enforcement_finding(
        "boundary_path_change",
        "src/auth/mod.rs",
        "key-boundary",
    )];
    let entries = vec![gate_held_entry(
        CURRENT_SHA,
        findings,
        "2026-07-09T10:00:00Z",
    )];
    let state = compute_active_hold_state(&entries, CURRENT_SHA);
    assert!(state.held);

    let recon = check_label_tamper(
        &state,
        TASK_ID,
        PROJECT_ID,
        Some(PR_NUMBER),
        true, // label is present
        "final_pre_merge_check",
        "2026-07-09T12:00:00Z",
    );

    assert!(
        !recon.tamper_detected,
        "no tamper when label is present and hold is active"
    );
    assert!(recon.payload.is_none());
}

/// When there is no active hold, a missing label is not tamper (the hold
/// was released, label removal is expected).
#[test]
fn no_active_hold_missing_label_is_not_tamper() {
    let findings = vec![enforcement_finding(
        "migration_change",
        "migrations/001.sql",
        "key-mig",
    )];
    let entries = vec![
        gate_held_entry(CURRENT_SHA, findings.clone(), "2026-07-09T10:00:00Z"),
        hold_released_entry(CURRENT_SHA, findings, "2026-07-09T11:00:00Z"),
    ];
    let state = compute_active_hold_state(&entries, CURRENT_SHA);
    assert!(!state.held, "hold is released");

    let recon = check_label_tamper(
        &state,
        TASK_ID,
        PROJECT_ID,
        Some(PR_NUMBER),
        false, // label missing, but hold is released
        "final_pre_merge_check",
        "2026-07-09T12:00:00Z",
    );

    assert!(
        !recon.tamper_detected,
        "no tamper when hold is released, even with missing label"
    );
}

// ── Additional boundary tests ─────────────────────────────────────────────

/// No gate events at all → no hold (PR is clean, merge may proceed).
#[test]
fn no_gate_events_allows_merge() {
    let entries: Vec<ActivityEntryRef> = vec![];
    let state = compute_active_hold_state(&entries, CURRENT_SHA);
    assert!(!state.held, "no events means no hold");
    assert!(state.is_clear());
    assert!(state.policy_revision.is_none());
    assert!(state.gate_idempotency_key.is_none());
}

/// Report-only findings must NOT produce an active hold.
#[test]
fn report_only_findings_do_not_block_merge() {
    let findings = vec![report_only_finding(
        "ci_workflow_change",
        ".github/workflows/ci.yml",
        "key-ci-ro",
    )];
    let entries = vec![gate_held_entry(
        CURRENT_SHA,
        findings,
        "2026-07-09T10:00:00Z",
    )];

    let state = compute_active_hold_state(&entries, CURRENT_SHA);
    assert!(
        !state.held,
        "report-only findings must not create an active hold"
    );
    assert!(state.active_findings.is_empty());
}

/// Mixed enforcement + report-only: only enforcement findings are active.
#[test]
fn mixed_findings_only_enforcement_blocks() {
    let f_enf = enforcement_finding("migration_change", "migrations/001.sql", "key-mig");
    let f_ro = report_only_finding(
        "ci_workflow_change",
        ".github/workflows/ci.yml",
        "key-ci-ro",
    );
    let entries = vec![gate_held_entry(
        CURRENT_SHA,
        vec![f_enf, f_ro],
        "2026-07-09T10:00:00Z",
    )];

    let state = compute_active_hold_state(&entries, CURRENT_SHA);
    assert!(state.held, "enforcement findings must block");
    assert_eq!(
        state.active_findings.len(),
        1,
        "only enforcement findings counted as active"
    );
    assert_eq!(
        state.active_findings[0].rule_id.as_str(),
        "migration_change"
    );
}

/// Tamper idempotency key is deterministic for the same inputs.
#[test]
fn tamper_idempotency_key_is_deterministic() {
    use crate::tripwires::active_hold::build_tamper_reconciliation_key;

    let key1 =
        build_tamper_reconciliation_key(TASK_ID, Some(PR_NUMBER), CURRENT_SHA, "gate-key-001");
    let key2 =
        build_tamper_reconciliation_key(TASK_ID, Some(PR_NUMBER), CURRENT_SHA, "gate-key-001");
    assert_eq!(key1, key2, "tamper key must be deterministic");

    // Different inputs → different key
    let key3 =
        build_tamper_reconciliation_key(TASK_ID, Some(PR_NUMBER), CURRENT_SHA, "gate-key-002");
    assert_ne!(
        key1, key3,
        "different gate key must produce different tamper key"
    );
}
