// Tests for tripwire_gate — split from tripwire_gate.rs for file-size guard.
//
// Covers: PrFile→ChangedFile conversion, end-to-end gate evaluation for all
// seven rule families, report-only/hold/passed outcomes, idempotency keys,
// rollout/backfill mode determination, and policy-revision scoping.

use super::tripwire_gate::*;
use crate::tripwires::{
    ActivityEntryRef, ChangedFileStatus, GateOutcome, TRIPWIRE_EVENT_GATE_HELD,
    TRIPWIRE_EVENT_GATE_PASSED, TRIPWIRE_EVENT_GATE_REPORT_ONLY, TripwireEvaluationInput,
    TripwireFindingSeverity, TripwireGateDecision, TripwirePolicy, all_rule_evaluators, evaluate,
};
use djinn_provider::github_api::PrFile;

/// Build a `PrFile` with the given fields and no patch.
fn pr_file(filename: &str, status: &str, additions: u32, deletions: u32) -> PrFile {
    PrFile {
        sha: "deadbeef".to_owned(),
        filename: filename.to_owned(),
        status: status.to_owned(),
        additions,
        deletions,
        changes: additions + deletions,
        patch: None,
    }
}

/// Build a `PrFile` with a patch string.
fn pr_file_with_patch(
    filename: &str,
    status: &str,
    additions: u32,
    deletions: u32,
    patch: &str,
) -> PrFile {
    PrFile {
        sha: "deadbeef".to_owned(),
        filename: filename.to_owned(),
        status: status.to_owned(),
        additions,
        deletions,
        changes: additions + deletions,
        patch: Some(patch.to_owned()),
    }
}

/// Convert PrFiles → ChangedFiles and evaluate with default policy.
fn evaluate_from_pr_files(pr_files: Vec<PrFile>) -> TripwireGateDecision {
    let changed_files = convert_pr_files(&pr_files);
    let input = TripwireEvaluationInput {
        task_id: "task-001".to_owned(),
        project_id: "proj-001".to_owned(),
        pr_number: Some(42),
        head_sha: "abc123".to_owned(),
        policy: TripwirePolicy::default(),
        allowlist_revision: None,
        changed_files,
    };
    run_gate(&input).decision
}

/// Convert PrFiles → ChangedFiles and evaluate with a custom policy.
fn evaluate_from_pr_files_with_policy(
    pr_files: Vec<PrFile>,
    policy: TripwirePolicy,
) -> TripwireGateDecision {
    let changed_files = convert_pr_files(&pr_files);
    let input = TripwireEvaluationInput {
        task_id: "task-001".to_owned(),
        project_id: "proj-001".to_owned(),
        pr_number: Some(42),
        head_sha: "abc123".to_owned(),
        policy,
        allowlist_revision: None,
        changed_files,
    };
    run_gate(&input).decision
}

/// Select the event type from a gate decision.
fn event_type_for(decision: &TripwireGateDecision) -> &'static str {
    match decision.outcome {
        GateOutcome::Held => TRIPWIRE_EVENT_GATE_HELD,
        GateOutcome::Passed => TRIPWIRE_EVENT_GATE_PASSED,
        GateOutcome::ReportOnly => TRIPWIRE_EVENT_GATE_REPORT_ONLY,
    }
}

/// Build an `ActivityEntryRef` with a gate event payload.
fn gate_activity_entry(
    event_type: &str,
    head_sha: &str,
    policy_revision: &str,
    idempotency_key: &str,
    created_at: &str,
) -> ActivityEntryRef {
    use crate::tripwires::TripwireGateDecisionPayload;
    let payload = TripwireGateDecisionPayload {
        event_type: event_type.to_owned(),
        task_id: "task-001".to_owned(),
        project_id: "proj-001".to_owned(),
        pr_number: Some(42),
        head_sha: head_sha.to_owned(),
        base_sha: None,
        policy_revision: policy_revision.to_owned(),
        allowlist_revision: None,
        findings: vec![],
        enforcement_finding_count: 0,
        report_only_finding_count: 0,
        idempotency_key: idempotency_key.to_owned(),
        decided_at: Some(created_at.to_owned()),
    };
    ActivityEntryRef {
        event_type: event_type.to_owned(),
        payload: serde_json::to_string(&payload).unwrap_or_default(),
        created_at: created_at.to_owned(),
    }
}

// ── Conversion tests ────────────────────────────────────────────────

#[test]
fn convert_pr_files_maps_added_status() {
    let files = vec![pr_file("src/new.rs", "added", 50, 0)];
    let converted = convert_pr_files(&files);
    assert_eq!(converted.len(), 1);
    assert_eq!(converted[0].status, ChangedFileStatus::Added);
    assert_eq!(converted[0].path, "src/new.rs");
    assert_eq!(converted[0].additions, 50);
    assert_eq!(converted[0].deletions, 0);
}

#[test]
fn convert_pr_files_maps_removed_status() {
    let files = vec![pr_file("src/old.rs", "removed", 0, 120)];
    let converted = convert_pr_files(&files);
    assert_eq!(converted[0].status, ChangedFileStatus::Deleted);
}

#[test]
fn convert_pr_files_maps_renamed_status() {
    let files = vec![pr_file("src/renamed.rs", "renamed", 5, 5)];
    let converted = convert_pr_files(&files);
    assert_eq!(converted[0].status, ChangedFileStatus::Renamed);
}

#[test]
fn convert_pr_files_maps_modified_and_unknown_to_modified() {
    let files = vec![
        pr_file("a.rs", "modified", 10, 5),
        pr_file("b.rs", "copied", 3, 0),
    ];
    let converted = convert_pr_files(&files);
    assert_eq!(converted[0].status, ChangedFileStatus::Modified);
    assert_eq!(converted[1].status, ChangedFileStatus::Modified);
}

#[test]
fn convert_pr_files_produces_empty_hunks_without_patch() {
    let files = vec![pr_file("src/lib.rs", "modified", 10, 5)];
    let converted = convert_pr_files(&files);
    assert!(converted[0].hunks.is_empty());
}

#[test]
fn convert_pr_files_parses_patch_into_hunks() {
    let patch = "@@ -1,2 +1,3 @@\n unchanged\n+added line\n-old line\n";
    let files = vec![pr_file_with_patch("src/main.rs", "modified", 1, 1, patch)];
    let converted = convert_pr_files(&files);
    assert_eq!(converted[0].hunks.len(), 1);
    let hunk = &converted[0].hunks[0];
    assert_eq!(hunk.old_start, 1);
    assert_eq!(hunk.old_lines, 2);
    assert_eq!(hunk.new_start, 1);
    assert_eq!(hunk.new_lines, 3);
    assert_eq!(
        hunk.diff_lines,
        vec![
            " unchanged".to_owned(),
            "+added line".to_owned(),
            "-old line".to_owned(),
        ]
    );
}

#[test]
fn convert_pr_files_parses_multiple_hunks() {
    let patch = "@@ -1,1 +1,2 @@\n a\n+b\n@@ -10,1 +11,1 @@\n-c\n+d\n";
    let files = vec![pr_file_with_patch("src/multi.rs", "modified", 2, 2, patch)];
    let converted = convert_pr_files(&files);
    assert_eq!(converted[0].hunks.len(), 2);
    assert_eq!(converted[0].hunks[0].new_start, 1);
    assert_eq!(converted[0].hunks[1].new_start, 11);
}

#[test]
fn convert_pr_files_empty_input() {
    let converted = convert_pr_files(&[]);
    assert!(converted.is_empty());
}

#[test]
fn parse_patch_to_hunks_empty_string() {
    assert!(parse_patch_to_hunks("").is_empty());
}

#[test]
fn parse_patch_to_hunks_no_hunk_header() {
    assert!(parse_patch_to_hunks("some random text\nno hunk headers").is_empty());
}

// ── Rule 1: migration_change (file-level) ──────────────────────────

#[test]
fn migration_change_from_pr_file_produces_held_gate() {
    let files = vec![pr_file(
        "migrations/20260101_create_users.sql",
        "added",
        20,
        0,
    )];
    let decision = evaluate_from_pr_files(files);
    assert_eq!(decision.outcome, GateOutcome::Held);
    assert!(decision.enforcement_finding_count > 0);
    assert!(
        decision
            .findings
            .iter()
            .any(|f| f.rule_id.as_str() == "migration_change")
    );
    assert_eq!(event_type_for(&decision), TRIPWIRE_EVENT_GATE_HELD);
}

// ── Rule 2: dependency_identity_change ─────────────────────────────

#[test]
fn dependency_identity_change_from_pr_file_produces_held_gate() {
    let files = vec![pr_file("Cargo.toml", "modified", 5, 3)];
    let decision = evaluate_from_pr_files(files);
    assert_eq!(decision.outcome, GateOutcome::Held);
    assert!(
        decision
            .findings
            .iter()
            .any(|f| f.rule_id.as_str() == "dependency_identity_change")
    );
}

// ── Rule 3: network_egress_change ──────────────────────────────────

#[test]
fn network_egress_change_from_pr_file_produces_held_gate() {
    let patch = "@@ -1,1 +1,3 @@\n old line\n+Webhook::register(endpoint);\n+notify(payload);\n";
    let files = vec![pr_file_with_patch("src/http.rs", "modified", 2, 0, patch)];
    let decision = evaluate_from_pr_files(files);
    assert_eq!(decision.outcome, GateOutcome::Held);
    assert!(
        decision
            .findings
            .iter()
            .any(|f| f.rule_id.as_str() == "network_egress_change")
    );
    let egress_finding = decision
        .findings
        .iter()
        .find(|f| f.rule_id.as_str() == "network_egress_change")
        .unwrap();
    assert!(egress_finding.evidence.start_line.is_some());
}

// ── Rule 4: unsafe_code_change ─────────────────────────────────────

#[test]
fn unsafe_code_change_from_pr_file_produces_held_gate() {
    let patch = "@@ -1,0 +1,2 @@\n+unsafe {\n+    ptr::read_volatile(addr);\n+}\n";
    let files = vec![pr_file_with_patch("src/ffi.rs", "modified", 3, 0, patch)];
    let decision = evaluate_from_pr_files(files);
    assert_eq!(decision.outcome, GateOutcome::Held);
    assert!(
        decision
            .findings
            .iter()
            .any(|f| f.rule_id.as_str() == "unsafe_code_change")
    );
}

// ── Rule 5: boundary_path_change ───────────────────────────────────

#[test]
fn boundary_path_change_from_pr_file_produces_held_gate() {
    let files = vec![pr_file("src/auth/permissions.rs", "added", 100, 0)];
    let decision = evaluate_from_pr_files(files);
    assert_eq!(decision.outcome, GateOutcome::Held);
    assert!(
        decision
            .findings
            .iter()
            .any(|f| f.rule_id.as_str() == "boundary_path_change")
    );
    let boundary_finding = decision
        .findings
        .iter()
        .find(|f| f.rule_id.as_str() == "boundary_path_change")
        .unwrap();
    assert!(boundary_finding.allowlist_revision.is_some());
}

// ── Rule 6: large_delete_or_rewrite ────────────────────────────────

#[test]
fn large_delete_or_rewrite_from_pr_file_produces_held_gate() {
    let files = vec![pr_file("src/old_module.rs", "modified", 10, 600)];
    let decision = evaluate_from_pr_files(files);
    assert_eq!(decision.outcome, GateOutcome::Held);
    assert!(
        decision
            .findings
            .iter()
            .any(|f| f.rule_id.as_str() == "large_delete_or_rewrite")
    );
}

// ── Rule 7: ci_workflow_change ─────────────────────────────────────

#[test]
fn ci_workflow_change_from_pr_file_produces_held_gate() {
    let files = vec![pr_file(".github/workflows/ci.yml", "modified", 15, 5)];
    let decision = evaluate_from_pr_files(files);
    assert_eq!(decision.outcome, GateOutcome::Held);
    assert!(
        decision
            .findings
            .iter()
            .any(|f| f.rule_id.as_str() == "ci_workflow_change")
    );
}

// ── All seven rule families ────────────────────────────────────────

#[test]
fn all_seven_rule_families_from_pr_files_produce_findings() {
    let files = vec![
        pr_file("migrations/001.sql", "added", 10, 0),
        pr_file("Cargo.toml", "modified", 2, 1),
        pr_file_with_patch(
            "src/webhook.rs",
            "modified",
            2,
            0,
            "@@ -1,0 +1,2 @@\n+Webhook::register(endpoint);\n+notify(payload);\n",
        ),
        pr_file_with_patch(
            "src/ffi.rs",
            "modified",
            2,
            0,
            "@@ -1,0 +1,2 @@\n+unsafe {\n+    ptr::read_volatile(addr);\n",
        ),
        pr_file("src/auth/mod.rs", "added", 50, 0),
        pr_file("src/legacy.rs", "modified", 5, 600),
        pr_file(".github/workflows/ci.yml", "modified", 10, 5),
    ];
    let decision = evaluate_from_pr_files(files);
    assert_eq!(decision.outcome, GateOutcome::Held);
    let rule_ids: Vec<&str> = decision
        .findings
        .iter()
        .map(|f| f.rule_id.as_str())
        .collect();
    assert!(rule_ids.contains(&"migration_change"));
    assert!(rule_ids.contains(&"dependency_identity_change"));
    assert!(rule_ids.contains(&"network_egress_change"));
    assert!(rule_ids.contains(&"unsafe_code_change"));
    assert!(rule_ids.contains(&"boundary_path_change"));
    assert!(rule_ids.contains(&"large_delete_or_rewrite"));
    assert!(rule_ids.contains(&"ci_workflow_change"));
}

// ── Report-only scenario ───────────────────────────────────────────

#[test]
fn report_only_finding_from_pr_file_produces_report_only_gate() {
    let mut policy = TripwirePolicy::default();
    policy.migration.report_only = true;
    let files = vec![pr_file("migrations/001_init.sql", "added", 50, 0)];
    let decision = evaluate_from_pr_files_with_policy(files, policy);
    assert_eq!(decision.outcome, GateOutcome::ReportOnly);
    assert_eq!(decision.enforcement_finding_count, 0);
    assert!(decision.report_only_finding_count > 0);
    assert_eq!(event_type_for(&decision), TRIPWIRE_EVENT_GATE_REPORT_ONLY);
    for f in &decision.findings {
        assert_eq!(f.severity, TripwireFindingSeverity::ReportOnly);
    }
}

#[test]
fn report_only_network_egress_from_pr_file() {
    let mut policy = TripwirePolicy::default();
    policy.network_egress.report_only = true;
    let patch = "@@ -1,0 +1,2 @@\n+Webhook::register(endpoint);\n+notify(payload);\n";
    let files = vec![pr_file_with_patch("src/http.rs", "modified", 2, 0, patch)];
    let decision = evaluate_from_pr_files_with_policy(files, policy);
    assert_eq!(decision.outcome, GateOutcome::ReportOnly);
    assert!(
        decision
            .findings
            .iter()
            .any(|f| f.rule_id.as_str() == "network_egress_change")
    );
}

// ── Passed (no findings) ───────────────────────────────────────────

#[test]
fn no_matching_pr_files_produce_passed_gate() {
    let files = vec![pr_file("src/main.rs", "modified", 5, 2)];
    let decision = evaluate_from_pr_files(files);
    assert_eq!(decision.outcome, GateOutcome::Passed);
    assert_eq!(decision.enforcement_finding_count, 0);
    assert_eq!(decision.report_only_finding_count, 0);
    assert!(decision.findings.is_empty());
    assert_eq!(event_type_for(&decision), TRIPWIRE_EVENT_GATE_PASSED);
}

// ── Idempotency key determinism ────────────────────────────────────

#[test]
fn gate_idempotency_key_is_deterministic_from_pr_files() {
    let files = vec![pr_file("migrations/001.sql", "added", 10, 0)];
    let d1 = evaluate_from_pr_files(files.clone());
    let d2 = evaluate_from_pr_files(files);
    assert_eq!(d1.idempotency_key, d2.idempotency_key);
}

// ── Payload validation ─────────────────────────────────────────────

#[test]
fn payload_validation_passes_from_pr_files() {
    let files = vec![pr_file("migrations/001.sql", "added", 10, 0)];
    let changed_files = convert_pr_files(&files);
    let input = TripwireEvaluationInput {
        task_id: "task-001".to_owned(),
        project_id: "proj-001".to_owned(),
        pr_number: Some(42),
        head_sha: "abc123".to_owned(),
        policy: TripwirePolicy::default(),
        allowlist_revision: None,
        changed_files,
    };
    let result = run_gate(&input);
    result
        .payload
        .validate()
        .expect("payload must pass validation");
}

// ── Mixed findings: enforcement dominates ──────────────────────────

#[test]
fn mixed_findings_enforcement_dominates_over_report_only() {
    let mut policy = TripwirePolicy::default();
    policy.ci_workflow.report_only = true;
    let files = vec![
        pr_file("migrations/002.sql", "added", 20, 0),
        pr_file(".github/workflows/release.yml", "modified", 10, 5),
    ];
    let decision = evaluate_from_pr_files_with_policy(files, policy);
    assert_eq!(decision.outcome, GateOutcome::Held);
    assert!(decision.enforcement_finding_count > 0);
    assert!(decision.report_only_finding_count > 0);
    assert_eq!(event_type_for(&decision), TRIPWIRE_EVENT_GATE_HELD);
}

// ── Patch absent: egress/unsafe cannot surface ─────────────────────

#[test]
fn pr_file_without_patch_does_not_trigger_egress_or_unsafe() {
    let files = vec![pr_file("src/webhook.rs", "modified", 5, 0)];
    let decision = evaluate_from_pr_files(files);
    assert_eq!(decision.outcome, GateOutcome::Passed);
    assert!(decision.findings.is_empty());
}

// ── Generated/vendor files excluded ────────────────────────────────

#[test]
fn generated_files_are_excluded_from_evaluation() {
    use crate::tripwires::ChangedFile;
    let changed_files = vec![ChangedFile {
        path: "generated/bindings.rs".to_owned(),
        old_path: None,
        status: ChangedFileStatus::Added,
        additions: 5000,
        deletions: 0,
        hunks: Vec::new(),
        is_generated: true,
        is_vendor: false,
    }];
    let input = TripwireEvaluationInput {
        task_id: "task-001".to_owned(),
        project_id: "proj-001".to_owned(),
        pr_number: Some(42),
        head_sha: "abc123".to_owned(),
        policy: TripwirePolicy::default(),
        allowlist_revision: None,
        changed_files,
    };
    let result = run_gate(&input);
    assert_eq!(result.decision.outcome, GateOutcome::Passed);
    assert!(result.decision.findings.is_empty());
}

// ── Rollout mode: determine_rollout_mode ───────────────────────────

#[test]
fn determine_rollout_mode_no_prior_events_is_backfill() {
    let entries: Vec<ActivityEntryRef> = vec![];
    let mode = determine_rollout_mode(&entries, "sha-aaa", None, "default");
    assert_eq!(mode, RolloutMode::Backfill);
}

#[test]
fn determine_rollout_mode_same_head_sha_is_already_evaluated() {
    let entries = vec![gate_activity_entry(
        TRIPWIRE_EVENT_GATE_REPORT_ONLY,
        "sha-aaa",
        "default",
        "key-1",
        "2026-01-01T00:00:00Z",
    )];
    let mode = determine_rollout_mode(&entries, "sha-aaa", None, "default");
    assert_eq!(mode, RolloutMode::AlreadyEvaluated);
}

#[test]
fn determine_rollout_mode_different_head_sha_is_enforce() {
    let entries = vec![gate_activity_entry(
        TRIPWIRE_EVENT_GATE_REPORT_ONLY,
        "sha-old",
        "default",
        "key-old",
        "2026-01-01T00:00:00Z",
    )];
    let mode = determine_rollout_mode(&entries, "sha-new", None, "default");
    assert_eq!(mode, RolloutMode::Enforce);
}

#[test]
fn determine_rollout_mode_ignores_non_gate_events() {
    let entries = vec![ActivityEntryRef {
        event_type: "unrelated.event".to_owned(),
        payload: "{}".to_owned(),
        created_at: "2026-01-01T00:00:00Z".to_owned(),
    }];
    let mode = determine_rollout_mode(&entries, "sha-aaa", None, "default");
    assert_eq!(mode, RolloutMode::Backfill);
}

#[test]
fn determine_rollout_mode_mixed_events_different_sha_is_enforce() {
    let entries = vec![
        ActivityEntryRef {
            event_type: "unrelated.event".to_owned(),
            payload: "{}".to_owned(),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
        },
        gate_activity_entry(
            TRIPWIRE_EVENT_GATE_HELD,
            "sha-old",
            "default",
            "key-old",
            "2026-01-02T00:00:00Z",
        ),
    ];
    let mode = determine_rollout_mode(&entries, "sha-new", None, "default");
    assert_eq!(mode, RolloutMode::Enforce);
}

#[test]
fn determine_rollout_mode_multiple_events_same_sha_is_already_evaluated() {
    let entries = vec![
        gate_activity_entry(
            TRIPWIRE_EVENT_GATE_REPORT_ONLY,
            "sha-old",
            "default",
            "key-old",
            "2026-01-01T00:00:00Z",
        ),
        gate_activity_entry(
            TRIPWIRE_EVENT_GATE_HELD,
            "sha-mid",
            "default",
            "key-mid",
            "2026-01-02T00:00:00Z",
        ),
        gate_activity_entry(
            TRIPWIRE_EVENT_GATE_PASSED,
            "sha-aaa",
            "default",
            "key-cur",
            "2026-01-03T00:00:00Z",
        ),
    ];
    let mode = determine_rollout_mode(&entries, "sha-aaa", None, "default");
    assert_eq!(mode, RolloutMode::AlreadyEvaluated);
}

#[test]
fn determine_rollout_mode_same_head_sha_new_policy_revision_is_enforce() {
    let entries = vec![gate_activity_entry(
        TRIPWIRE_EVENT_GATE_REPORT_ONLY,
        "sha-aaa",
        "org-policy:1",
        "key-v1",
        "2026-01-01T00:00:00Z",
    )];
    let mode = determine_rollout_mode(&entries, "sha-aaa", None, "org-policy:2");
    assert_eq!(
        mode,
        RolloutMode::Enforce,
        "same head SHA but different policy revision must be Enforce"
    );
}

#[test]
fn determine_rollout_mode_multi_event_same_head_and_policy_is_already_evaluated() {
    let entries = vec![
        gate_activity_entry(
            TRIPWIRE_EVENT_GATE_REPORT_ONLY,
            "sha-old",
            "default",
            "key-old",
            "2026-01-01T00:00:00Z",
        ),
        gate_activity_entry(
            TRIPWIRE_EVENT_GATE_PASSED,
            "sha-aaa",
            "default",
            "key-aaa",
            "2026-01-02T00:00:00Z",
        ),
    ];
    let mode = determine_rollout_mode(&entries, "sha-aaa", None, "default");
    assert_eq!(mode, RolloutMode::AlreadyEvaluated);
}

#[test]
fn determine_rollout_mode_new_pr_after_publication_is_enforce() {
    let entries: Vec<ActivityEntryRef> = vec![];
    let mode = determine_rollout_mode(&entries, "sha-new", Some("2026-01-01T00:00:00Z"), "default");
    assert_eq!(
        mode,
        RolloutMode::Enforce,
        "new PR after policy publication must be Enforce"
    );
}

#[test]
fn determine_rollout_mode_existing_pr_before_publication_is_backfill() {
    let entries: Vec<ActivityEntryRef> = vec![];
    let mode = determine_rollout_mode(&entries, "sha-old", None, "default");
    assert_eq!(mode, RolloutMode::Backfill);
}

// ── is_tripwire_gate_event ─────────────────────────────────────────

#[test]
fn is_tripwire_gate_event_recognizes_all_three_types() {
    assert!(is_tripwire_gate_event(TRIPWIRE_EVENT_GATE_HELD));
    assert!(is_tripwire_gate_event(TRIPWIRE_EVENT_GATE_PASSED));
    assert!(is_tripwire_gate_event(TRIPWIRE_EVENT_GATE_REPORT_ONLY));
}

#[test]
fn is_tripwire_gate_event_rejects_non_gate_types() {
    assert!(!is_tripwire_gate_event("tripwire.hold.released"));
    assert!(!is_tripwire_gate_event("tripwire.tamper.label_removed"));
    assert!(!is_tripwire_gate_event("unrelated.event"));
}

// ── Policy report-only override ────────────────────────────────────

#[test]
fn make_report_only_forces_all_rules_to_report_only() {
    let policy = TripwirePolicy::default();
    let report_only = policy.make_report_only();
    assert!(report_only.migration.report_only);
    assert!(report_only.dependency_identity.report_only);
    assert!(report_only.network_egress.report_only);
    assert!(report_only.unsafe_code.report_only);
    assert!(report_only.boundary_path.report_only);
    assert!(report_only.large_delete_rewrite.report_only);
    assert!(report_only.ci_workflow.report_only);
    assert!(report_only.migration.enabled);
    assert!(report_only.dependency_identity.enabled);
    assert!(report_only.network_egress.enabled);
    assert!(report_only.unsafe_code.enabled);
    assert!(report_only.boundary_path.enabled);
    assert!(report_only.large_delete_rewrite.enabled);
    assert!(report_only.ci_workflow.enabled);
}

#[test]
fn backfill_migration_change_produces_report_only() {
    let policy = TripwirePolicy::default().make_report_only();
    let files = vec![pr_file(
        "migrations/20260101_create_users.sql",
        "added",
        20,
        0,
    )];
    let decision = evaluate_from_pr_files_with_policy(files, policy);
    assert_eq!(decision.outcome, GateOutcome::ReportOnly);
    assert_eq!(decision.enforcement_finding_count, 0);
    assert!(decision.report_only_finding_count > 0);
    assert_eq!(event_type_for(&decision), TRIPWIRE_EVENT_GATE_REPORT_ONLY);
}

#[test]
fn backfill_ci_workflow_change_produces_report_only() {
    let policy = TripwirePolicy::default().make_report_only();
    let files = vec![pr_file(".github/workflows/ci.yml", "modified", 15, 5)];
    let decision = evaluate_from_pr_files_with_policy(files, policy);
    assert_eq!(decision.outcome, GateOutcome::ReportOnly);
}

#[test]
fn backfill_all_seven_rules_produce_report_only() {
    let policy = TripwirePolicy::default().make_report_only();
    let files = vec![
        pr_file("migrations/001.sql", "added", 10, 0),
        pr_file("Cargo.toml", "modified", 2, 1),
        pr_file_with_patch(
            "src/webhook.rs",
            "modified",
            2,
            0,
            "@@ -1,0 +1,2 @@\n+Webhook::register(endpoint);\n+notify(payload);\n",
        ),
        pr_file_with_patch(
            "src/ffi.rs",
            "modified",
            2,
            0,
            "@@ -1,0 +1,2 @@\n+unsafe {\n+    ptr::read_volatile(addr);\n",
        ),
        pr_file("src/auth/mod.rs", "added", 50, 0),
        pr_file("src/legacy.rs", "modified", 5, 600),
        pr_file(".github/workflows/ci.yml", "modified", 10, 5),
    ];
    let decision = evaluate_from_pr_files_with_policy(files, policy);
    assert_eq!(decision.outcome, GateOutcome::ReportOnly);
    assert_eq!(decision.enforcement_finding_count, 0);
    assert!(decision.report_only_finding_count > 0);
    assert_eq!(event_type_for(&decision), TRIPWIRE_EVENT_GATE_REPORT_ONLY);
    for f in &decision.findings {
        assert_eq!(f.severity, TripwireFindingSeverity::ReportOnly);
    }
}

// ── Idempotency keys: head SHA / policy revision ───────────────────

#[test]
fn idempotency_key_changes_with_head_sha() {
    let files = vec![pr_file("migrations/001.sql", "added", 10, 0)];
    let changed = convert_pr_files(&files);
    let input_a = TripwireEvaluationInput {
        task_id: "task-001".to_owned(),
        project_id: "proj-001".to_owned(),
        pr_number: Some(42),
        head_sha: "sha-aaa".to_owned(),
        policy: TripwirePolicy::default(),
        allowlist_revision: None,
        changed_files: changed.clone(),
    };
    let input_b = TripwireEvaluationInput {
        task_id: "task-001".to_owned(),
        project_id: "proj-001".to_owned(),
        pr_number: Some(42),
        head_sha: "sha-bbb".to_owned(),
        policy: TripwirePolicy::default(),
        allowlist_revision: None,
        changed_files: changed,
    };
    let d_a = run_gate(&input_a).decision;
    let d_b = run_gate(&input_b).decision;
    assert_ne!(d_a.idempotency_key, d_b.idempotency_key);
}

#[test]
fn idempotency_key_changes_with_policy_revision() {
    let files = vec![pr_file("migrations/001.sql", "added", 10, 0)];
    let changed = convert_pr_files(&files);
    let mut policy_v1 = TripwirePolicy::default();
    policy_v1.policy_revision = "org-policy:1".to_owned();
    let mut policy_v2 = TripwirePolicy::default();
    policy_v2.policy_revision = "org-policy:2".to_owned();
    let input_v1 = TripwireEvaluationInput {
        task_id: "task-001".to_owned(),
        project_id: "proj-001".to_owned(),
        pr_number: Some(42),
        head_sha: "sha-aaa".to_owned(),
        policy: policy_v1,
        allowlist_revision: None,
        changed_files: changed.clone(),
    };
    let input_v2 = TripwireEvaluationInput {
        task_id: "task-001".to_owned(),
        project_id: "proj-001".to_owned(),
        pr_number: Some(42),
        head_sha: "sha-aaa".to_owned(),
        policy: policy_v2,
        allowlist_revision: None,
        changed_files: changed,
    };
    let d_v1 = run_gate(&input_v1).decision;
    let d_v2 = run_gate(&input_v2).decision;
    assert_ne!(d_v1.idempotency_key, d_v2.idempotency_key);
}

#[test]
fn duplicate_backfill_same_key() {
    let files = vec![pr_file("migrations/001.sql", "added", 10, 0)];
    let d1 = evaluate_from_pr_files(files.clone());
    let d2 = evaluate_from_pr_files(files);
    assert_eq!(d1.idempotency_key, d2.idempotency_key);
    assert_eq!(d1.outcome, d2.outcome);
}

// ── Regression: same head SHA + new policy revision is NOT skipped ──
//
// This is the core idempotency-policy-revision regression test.
// When a policy revision changes for the same head SHA,
// `determine_rollout_mode` must return `Enforce` (not
// `AlreadyEvaluated`), so the caller re-evaluates and emits a new gate
// event with the new policy revision's idempotency key.

#[test]
fn same_head_sha_new_policy_revision_not_skipped_by_rollout() {
    // Simulate: PR head "sha-abc" was previously evaluated under
    // "policy-v1" → the stored activity entry has policy_revision "policy-v1".
    let entries = vec![gate_activity_entry(
        TRIPWIRE_EVENT_GATE_REPORT_ONLY,
        "sha-abc",
        "policy-v1",
        "key-v1",
        "2026-01-01T00:00:00Z",
    )];

    // Current policy revision is now "policy-v2" — same head SHA.
    let mode = determine_rollout_mode(&entries, "sha-abc", None, "policy-v2");

    // Must NOT be AlreadyEvaluated — policy changed, so re-evaluate.
    assert_eq!(
        mode,
        RolloutMode::Enforce,
        "same head SHA with changed policy revision must not be idempotent-skip"
    );

    // Also verify the idempotency key changes (proving the new policy
    // revision produces a distinct key, not a duplicate of the old one).
    let mut old_policy = TripwirePolicy::default();
    old_policy.policy_revision = "policy-v1".to_owned();
    let mut new_policy = TripwirePolicy::default();
    new_policy.policy_revision = "policy-v2".to_owned();

    let files = vec![pr_file("migrations/001.sql", "added", 10, 0)];
    let changed = convert_pr_files(&files);

    let key_v1 = run_gate(&TripwireEvaluationInput {
        task_id: "task-001".to_owned(),
        project_id: "proj-001".to_owned(),
        pr_number: Some(42),
        head_sha: "sha-abc".to_owned(),
        policy: old_policy,
        allowlist_revision: None,
        changed_files: changed.clone(),
    })
    .decision
    .idempotency_key;

    let key_v2 = run_gate(&TripwireEvaluationInput {
        task_id: "task-001".to_owned(),
        project_id: "proj-001".to_owned(),
        pr_number: Some(42),
        head_sha: "sha-abc".to_owned(),
        policy: new_policy,
        allowlist_revision: None,
        changed_files: changed,
    })
    .decision
    .idempotency_key;

    assert_ne!(
        key_v1, key_v2,
        "idempotency key must change when policy revision changes for the same head SHA"
    );
}
