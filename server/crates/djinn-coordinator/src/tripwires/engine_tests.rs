//! Unit tests for the pure tripwire engine primitives.
//!
//! Kept in a `#[path]` sibling file so `engine.rs` stays under the repo size
//! guard (see `scripts/check-file-size.sh`).

use super::*;
use crate::tripwires::policy::TripwirePolicy;
use crate::tripwires::reason_codes::*;

// ── Helpers ──────────────────────────────────────────────────────────

fn sample_input(task_id: &str, head_sha: &str) -> TripwireEvaluationInput {
    TripwireEvaluationInput {
        task_id: task_id.to_owned(),
        project_id: "proj_1".to_owned(),
        pr_number: Some(42),
        head_sha: head_sha.to_owned(),
        policy: TripwirePolicy::default(),
        allowlist_revision: None,
        changed_files: Vec::new(),
    }
}

fn migration_finding(_task_id: &str, _head_sha: &str, path: &str) -> RawFinding {
    RawFinding {
        rule_id: TripwireRuleId::MigrationChange,
        report_only: false,
        evidence_path: path.to_owned(),
        evidence_start_line: Some(1),
        evidence_end_line: Some(10),
        evidence_is_excluded: false,
    }
}

fn ci_report_only_finding(path: &str) -> RawFinding {
    RawFinding {
        rule_id: TripwireRuleId::CIWorkflowChange,
        report_only: true,
        evidence_path: path.to_owned(),
        evidence_start_line: None,
        evidence_end_line: None,
        evidence_is_excluded: false,
    }
}

fn make_evaluator(
    findings: Vec<RawFinding>,
) -> impl Fn(&TripwirePolicy, &[ChangedFile]) -> Vec<RawFinding> {
    move |_policy: &TripwirePolicy, _files: &[ChangedFile]| -> Vec<RawFinding> { findings.clone() }
}

// ── Deterministic ordering ───────────────────────────────────────────

/// Findings must be sorted by (rule_id, path, start_line, end_line,
/// severity) regardless of insertion order.
#[test]
fn findings_are_sorted_deterministically() {
    let raw_findings = vec![
        RawFinding {
            rule_id: TripwireRuleId::CIWorkflowChange,
            report_only: false,
            evidence_path: ".github/workflows/ci.yml".to_owned(),
            evidence_start_line: None,
            evidence_end_line: None,
            evidence_is_excluded: false,
        },
        RawFinding {
            rule_id: TripwireRuleId::MigrationChange,
            report_only: false,
            evidence_path: "migrations/001.sql".to_owned(),
            evidence_start_line: Some(1),
            evidence_end_line: Some(5),
            evidence_is_excluded: false,
        },
        RawFinding {
            rule_id: TripwireRuleId::MigrationChange,
            report_only: false,
            evidence_path: "migrations/001.sql".to_owned(),
            evidence_start_line: Some(10),
            evidence_end_line: Some(20),
            evidence_is_excluded: false,
        },
    ];

    let evaluator = make_evaluator(raw_findings);
    let input = sample_input("task_1", "sha_a");
    let decision = evaluate(&input, &[evaluator]);

    assert_eq!(decision.findings.len(), 3);
    // CI workflow < Migration (alphabetical by rule_id string)
    assert_eq!(
        decision.findings[0].rule_id,
        TripwireRuleId::CIWorkflowChange
    );
    assert_eq!(
        decision.findings[1].rule_id,
        TripwireRuleId::MigrationChange
    );
    assert_eq!(decision.findings[1].evidence.start_line, Some(1));
    assert_eq!(
        decision.findings[2].rule_id,
        TripwireRuleId::MigrationChange
    );
    assert_eq!(decision.findings[2].evidence.start_line, Some(10));
}

/// Running the engine twice with the same input must produce
/// byte-identical findings and idempotency keys.
#[test]
fn engine_output_is_idempotent() {
    let raw = vec![migration_finding("t1", "sha1", "migrations/001.sql")];
    let eval1 = make_evaluator(raw.clone());
    let eval2 = make_evaluator(raw);

    let mut input = sample_input("t1", "sha1");
    input.changed_files = vec![ChangedFile {
        path: "migrations/001.sql".to_owned(),
        old_path: None,
        status: ChangedFileStatus::Added,
        additions: 10,
        deletions: 0,
        hunks: Vec::new(),
        is_generated: false,
        is_vendor: false,
    }];

    let d1 = evaluate(&input, &[eval1]);
    let d2 = evaluate(&input, &[eval2]);

    assert_eq!(d1, d2, "same input must produce identical decisions");
}

// ── Report-only vs enforcement decisions ─────────────────────────────

/// When all findings are enforcement-on, outcome must be `Held`.
#[test]
fn enforcement_findings_produce_held_outcome() {
    let raw = vec![
        migration_finding("t1", "sha1", "migrations/001.sql"),
        RawFinding {
            rule_id: TripwireRuleId::UnsafeCodeChange,
            report_only: false,
            evidence_path: "src/native.rs".to_owned(),
            evidence_start_line: Some(10),
            evidence_end_line: Some(50),
            evidence_is_excluded: false,
        },
    ];
    let evaluator = make_evaluator(raw);
    let input = sample_input("t1", "sha1");
    let decision = evaluate(&input, &[evaluator]);

    assert_eq!(decision.outcome, GateOutcome::Held);
    assert_eq!(decision.enforcement_finding_count, 2);
    assert_eq!(decision.report_only_finding_count, 0);
}

/// When all findings are report-only, outcome must be `ReportOnly`.
#[test]
fn report_only_findings_produce_report_only_outcome() {
    let raw = vec![
        ci_report_only_finding(".github/workflows/ci.yml"),
        ci_report_only_finding(".github/workflows/deploy.yml"),
    ];
    let evaluator = make_evaluator(raw);
    let input = sample_input("t1", "sha1");
    let decision = evaluate(&input, &[evaluator]);

    assert_eq!(decision.outcome, GateOutcome::ReportOnly);
    assert_eq!(decision.enforcement_finding_count, 0);
    assert_eq!(decision.report_only_finding_count, 2);
    assert!(
        decision
            .findings
            .iter()
            .all(|f| f.severity == TripwireFindingSeverity::ReportOnly)
    );
}

/// When there are mixed enforcement + report-only findings, outcome
/// must be `Held` (enforcement wins).
#[test]
fn mixed_findings_produce_held_outcome() {
    let raw = vec![
        migration_finding("t1", "sha1", "migrations/001.sql"),
        ci_report_only_finding(".github/workflows/ci.yml"),
    ];
    let evaluator = make_evaluator(raw);
    let input = sample_input("t1", "sha1");
    let decision = evaluate(&input, &[evaluator]);

    assert_eq!(decision.outcome, GateOutcome::Held);
    assert_eq!(decision.enforcement_finding_count, 1);
    assert_eq!(decision.report_only_finding_count, 1);
}

/// When there are no findings, outcome must be `Passed`.
#[test]
fn empty_findings_produce_passed_outcome() {
    let evaluator = make_evaluator(Vec::new());
    let input = sample_input("t1", "sha1");
    let decision = evaluate(&input, &[evaluator]);

    assert_eq!(decision.outcome, GateOutcome::Passed);
    assert_eq!(decision.enforcement_finding_count, 0);
    assert_eq!(decision.report_only_finding_count, 0);
    assert!(decision.findings.is_empty());
}

// ── Generated/vendor exclusion plumbing ──────────────────────────────

/// Findings from generated files must be excluded.
#[test]
fn generated_file_findings_are_excluded() {
    let raw = vec![RawFinding {
        rule_id: TripwireRuleId::MigrationChange,
        report_only: false,
        evidence_path: "target/debug/build.rs".to_owned(),
        evidence_start_line: None,
        evidence_end_line: None,
        evidence_is_excluded: true, // classified as generated
    }];
    let evaluator = make_evaluator(raw);
    let input = sample_input("t1", "sha1");
    let decision = evaluate(&input, &[evaluator]);

    assert_eq!(decision.outcome, GateOutcome::Passed);
    assert!(
        decision.findings.is_empty(),
        "generated-file finding must be excluded"
    );
}

/// Findings from vendor files must be excluded.
#[test]
fn vendor_file_findings_are_excluded() {
    let raw = vec![RawFinding {
        rule_id: TripwireRuleId::DependencyIdentityChange,
        report_only: false,
        evidence_path: "vendor/some-lib/index.js".to_owned(),
        evidence_start_line: Some(1),
        evidence_end_line: Some(5),
        evidence_is_excluded: true, // classified as vendor
    }];
    let evaluator = make_evaluator(raw);
    let input = sample_input("t1", "sha1");
    let decision = evaluate(&input, &[evaluator]);

    assert_eq!(decision.outcome, GateOutcome::Passed);
    assert!(
        decision.findings.is_empty(),
        "vendor-file finding must be excluded"
    );
}

/// Mixed: generated file excluded, real file kept.
#[test]
fn exclusion_only_affects_flagged_files() {
    let raw = vec![
        RawFinding {
            rule_id: TripwireRuleId::MigrationChange,
            report_only: false,
            evidence_path: "target/generated/migrations.rs".to_owned(),
            evidence_start_line: None,
            evidence_end_line: None,
            evidence_is_excluded: true,
        },
        migration_finding("t1", "sha1", "migrations/001.sql"),
    ];
    let evaluator = make_evaluator(raw);
    let input = sample_input("t1", "sha1");
    let decision = evaluate(&input, &[evaluator]);

    assert_eq!(decision.outcome, GateOutcome::Held);
    assert_eq!(decision.findings.len(), 1);
    assert_eq!(decision.findings[0].evidence.path, "migrations/001.sql");
}

/// ChangedFile::is_excluded helper reflects generated/vendor flags.
#[test]
fn changed_file_is_excluded_reflects_flags() {
    let normal = ChangedFile {
        path: "src/main.rs".to_owned(),
        old_path: None,
        status: ChangedFileStatus::Modified,
        additions: 5,
        deletions: 2,
        hunks: Vec::new(),
        is_generated: false,
        is_vendor: false,
    };
    assert!(!normal.is_excluded());

    let generated = ChangedFile {
        is_generated: true,
        ..normal.clone()
    };
    assert!(generated.is_excluded());

    let vendored = ChangedFile {
        is_vendor: true,
        ..normal
    };
    assert!(vendored.is_excluded());
}

// ── Revision propagation ─────────────────────────────────────────────

/// Policy revision must be propagated into every finding and the
/// decision.
#[test]
fn policy_revision_propagated_to_findings_and_decision() {
    let raw = vec![migration_finding("t1", "sha1", "migrations/001.sql")];
    let evaluator = make_evaluator(raw);
    let mut input = sample_input("t1", "sha1");
    input.policy.policy_revision = "org-policy:42".to_owned();

    let decision = evaluate(&input, &[evaluator]);

    assert_eq!(decision.policy_revision, "org-policy:42");
    for finding in &decision.findings {
        assert_eq!(finding.policy_revision, "org-policy:42");
    }
}

/// Allowlist revision must be propagated into boundary-path findings.
#[test]
fn allowlist_revision_propagated_to_boundary_findings() {
    let raw = vec![RawFinding {
        rule_id: TripwireRuleId::BoundaryPathChange,
        report_only: false,
        evidence_path: "scripts/capability-boundary-allowlist.toml".to_owned(),
        evidence_start_line: Some(1),
        evidence_end_line: Some(20),
        evidence_is_excluded: false,
    }];
    let evaluator = make_evaluator(raw);
    let mut input = sample_input("t1", "sha1");
    input.policy.allowlist.revision = "capability-boundary-allowlist:9".to_owned();

    let decision = evaluate(&input, &[evaluator]);

    assert_eq!(
        decision.allowlist_revision,
        Some("capability-boundary-allowlist:9".to_owned())
    );
    assert_eq!(
        decision.findings[0].allowlist_revision,
        Some("capability-boundary-allowlist:9".to_owned())
    );
}

/// Allowlist revision is absent from the decision when no boundary
/// rule tripped.
#[test]
fn allowlist_revision_absent_when_no_boundary_finding() {
    let raw = vec![migration_finding("t1", "sha1", "migrations/001.sql")];
    let evaluator = make_evaluator(raw);
    let input = sample_input("t1", "sha1");

    let decision = evaluate(&input, &[evaluator]);

    assert_eq!(decision.allowlist_revision, None);
    assert_eq!(decision.findings[0].allowlist_revision, None);
}

/// Allowlist revision from input override takes precedence.
#[test]
fn allowlist_revision_override_takes_precedence() {
    let raw = vec![RawFinding {
        rule_id: TripwireRuleId::BoundaryPathChange,
        report_only: false,
        evidence_path: "**/auth/**".to_owned(),
        evidence_start_line: None,
        evidence_end_line: None,
        evidence_is_excluded: false,
    }];
    let evaluator = make_evaluator(raw);
    let mut input = sample_input("t1", "sha1");
    input.policy.allowlist.revision = "from-policy:1".to_owned();
    input.allowlist_revision = Some("override:99".to_owned());

    let decision = evaluate(&input, &[evaluator]);

    assert_eq!(decision.allowlist_revision, Some("override:99".to_owned()));
    assert_eq!(
        decision.findings[0].allowlist_revision,
        Some("override:99".to_owned())
    );
}

// ── Idempotency key stability ────────────────────────────────────────

/// Finding idempotency keys must be stable for the same inputs.
#[test]
fn finding_idempotency_key_is_stable() {
    let k1 = build_finding_idempotency_key(
        "task_1",
        "sha_abc",
        TripwireRuleId::MigrationChange,
        "migrations/001.sql",
        Some(1),
        Some(10),
        "org-policy:1",
    );
    let k2 = build_finding_idempotency_key(
        "task_1",
        "sha_abc",
        TripwireRuleId::MigrationChange,
        "migrations/001.sql",
        Some(1),
        Some(10),
        "org-policy:1",
    );
    assert_eq!(k1, k2, "same inputs must produce same key");
    assert!(k1.starts_with("sha256:"), "key must use sha256 prefix");
}

/// Changing the head SHA must change the finding idempotency key.
#[test]
fn finding_key_changes_with_head_sha() {
    let k1 = build_finding_idempotency_key(
        "task_1",
        "sha_a",
        TripwireRuleId::MigrationChange,
        "migrations/001.sql",
        None,
        None,
        "org-policy:1",
    );
    let k2 = build_finding_idempotency_key(
        "task_1",
        "sha_b",
        TripwireRuleId::MigrationChange,
        "migrations/001.sql",
        None,
        None,
        "org-policy:1",
    );
    assert_ne!(k1, k2, "different head SHA must produce different key");
}

/// Changing the evidence path must change the finding idempotency key.
#[test]
fn finding_key_changes_with_evidence_path() {
    let k1 = build_finding_idempotency_key(
        "task_1",
        "sha_abc",
        TripwireRuleId::MigrationChange,
        "migrations/001.sql",
        Some(1),
        Some(10),
        "org-policy:1",
    );
    let k2 = build_finding_idempotency_key(
        "task_1",
        "sha_abc",
        TripwireRuleId::MigrationChange,
        "migrations/002.sql",
        Some(1),
        Some(10),
        "org-policy:1",
    );
    assert_ne!(k1, k2, "different evidence path must produce different key");
}

/// Changing the policy revision must change the finding idempotency key.
#[test]
fn finding_key_changes_with_policy_revision() {
    let k1 = build_finding_idempotency_key(
        "task_1",
        "sha_abc",
        TripwireRuleId::MigrationChange,
        "migrations/001.sql",
        Some(1),
        Some(10),
        "org-policy:1",
    );
    let k2 = build_finding_idempotency_key(
        "task_1",
        "sha_abc",
        TripwireRuleId::MigrationChange,
        "migrations/001.sql",
        Some(1),
        Some(10),
        "org-policy:2",
    );
    assert_ne!(
        k1, k2,
        "different policy revision must produce different key"
    );
}

/// Changing the task id must change the finding idempotency key.
#[test]
fn finding_key_changes_with_task_id() {
    let k1 = build_finding_idempotency_key(
        "task_1",
        "sha_abc",
        TripwireRuleId::MigrationChange,
        "migrations/001.sql",
        None,
        None,
        "org-policy:1",
    );
    let k2 = build_finding_idempotency_key(
        "task_2",
        "sha_abc",
        TripwireRuleId::MigrationChange,
        "migrations/001.sql",
        None,
        None,
        "org-policy:1",
    );
    assert_ne!(k1, k2, "different task id must produce different key");
}

/// Changing the evidence span must change the finding idempotency key.
#[test]
fn finding_key_changes_with_evidence_span() {
    let k1 = build_finding_idempotency_key(
        "task_1",
        "sha_abc",
        TripwireRuleId::MigrationChange,
        "migrations/001.sql",
        Some(1),
        Some(10),
        "org-policy:1",
    );
    let k2 = build_finding_idempotency_key(
        "task_1",
        "sha_abc",
        TripwireRuleId::MigrationChange,
        "migrations/001.sql",
        Some(5),
        Some(20),
        "org-policy:1",
    );
    assert_ne!(k1, k2, "different evidence span must produce different key");
}

/// Gate idempotency key must be stable for the same inputs.
#[test]
fn gate_idempotency_key_is_stable() {
    let keys = vec!["sha256:aaa".to_owned(), "sha256:bbb".to_owned()];
    let k1 = build_gate_idempotency_key(
        "task_1",
        "sha_abc",
        "org-policy:1",
        Some("allowlist:1"),
        &keys,
    );
    let k2 = build_gate_idempotency_key(
        "task_1",
        "sha_abc",
        "org-policy:1",
        Some("allowlist:1"),
        &keys,
    );
    assert_eq!(k1, k2, "same inputs must produce same gate key");
}

/// Gate idempotency key must change with head SHA.
#[test]
fn gate_key_changes_with_head_sha() {
    let keys = vec!["sha256:aaa".to_owned()];
    let k1 = build_gate_idempotency_key("t", "sha_a", "p", None, &keys);
    let k2 = build_gate_idempotency_key("t", "sha_b", "p", None, &keys);
    assert_ne!(k1, k2);
}

/// Gate idempotency key must change with policy revision.
#[test]
fn gate_key_changes_with_policy_revision() {
    let keys = vec!["sha256:aaa".to_owned()];
    let k1 = build_gate_idempotency_key("t", "sha", "p1", None, &keys);
    let k2 = build_gate_idempotency_key("t", "sha", "p2", None, &keys);
    assert_ne!(k1, k2);
}

/// Gate idempotency key must be independent of finding insertion order
/// (because the caller sorts finding keys before passing them).
#[test]
fn gate_key_is_independent_of_finding_order() {
    let mut keys_a = vec!["sha256:aaa".to_owned(), "sha256:bbb".to_owned()];
    let mut keys_b = vec!["sha256:bbb".to_owned(), "sha256:aaa".to_owned()];
    keys_a.sort();
    keys_b.sort();

    let k1 = build_gate_idempotency_key("t", "sha", "p", None, &keys_a);
    let k2 = build_gate_idempotency_key("t", "sha", "p", None, &keys_b);
    assert_eq!(
        k1, k2,
        "sorted keys must produce the same gate key regardless of original order"
    );
}

// ── Evaluator integration ────────────────────────────────────────────

/// Multiple evaluators must all run and their findings combined.
#[test]
fn multiple_evaluators_produce_combined_findings() {
    let eval1 = make_evaluator(vec![migration_finding("t1", "sha1", "migrations/001.sql")]);
    let eval2 = make_evaluator(vec![RawFinding {
        rule_id: TripwireRuleId::CIWorkflowChange,
        report_only: true,
        evidence_path: ".github/workflows/ci.yml".to_owned(),
        evidence_start_line: None,
        evidence_end_line: None,
        evidence_is_excluded: false,
    }]);

    let input = sample_input("t1", "sha1");
    let decision = evaluate(&input, &[eval1, eval2]);

    assert_eq!(decision.findings.len(), 2);
    assert_eq!(decision.outcome, GateOutcome::Held);
    assert_eq!(decision.enforcement_finding_count, 1);
    assert_eq!(decision.report_only_finding_count, 1);
}

// ── to_summary conversion ────────────────────────────────────────────

/// TripwireFinding::to_summary must correctly map severity and fields.
#[test]
fn finding_to_summary_maps_correctly() {
    let finding = TripwireFinding {
        rule_id: TripwireRuleId::MigrationChange,
        reason_code: REASON_MIGRATION_CHANGE,
        severity: TripwireFindingSeverity::EnforceHold,
        evidence: EvidenceSpan::lines("migrations/001.sql", 1, 10),
        policy_revision: "org-policy:1".to_owned(),
        allowlist_revision: None,
        idempotency_key: "sha256:test".to_owned(),
        content_fingerprint: "fp:sha256:test".to_owned(),
        downgrade_reason: None,
    };

    let summary = finding.to_summary();
    assert_eq!(summary.rule_id, "migration_change");
    assert_eq!(summary.content_fingerprint, "fp:sha256:test");
    assert_eq!(summary.reason_code, REASON_MIGRATION_CHANGE);
    assert_eq!(summary.severity, TripwireSeverity::HumanReviewRequired);
    assert_eq!(summary.evidence.path, "migrations/001.sql");
    assert_eq!(summary.evidence.start_line, Some(1));
    assert_eq!(summary.evidence.end_line, Some(10));
    assert_eq!(summary.idempotency_key, "sha256:test");
}

/// to_summary must map ReportOnly correctly.
#[test]
fn finding_to_summary_maps_report_only() {
    let finding = TripwireFinding {
        rule_id: TripwireRuleId::CIWorkflowChange,
        reason_code: REASON_CI_WORKFLOW_CHANGE,
        severity: TripwireFindingSeverity::ReportOnly,
        evidence: EvidenceSpan::file(".github/workflows/ci.yml"),
        policy_revision: "org-policy:1".to_owned(),
        allowlist_revision: None,
        idempotency_key: "sha256:ro".to_owned(),
        content_fingerprint: "fp:sha256:ro".to_owned(),
        downgrade_reason: None,
    };

    let summary = finding.to_summary();
    assert_eq!(summary.severity, TripwireSeverity::ReportOnly);
    assert_eq!(summary.evidence.start_line, None);
    assert_eq!(summary.evidence.end_line, None);
}

// ── EvidenceSpan constructors ────────────────────────────────────────

/// EvidenceSpan::lines must produce a line-precise span.
#[test]
fn evidence_span_lines_constructor() {
    let span = EvidenceSpan::lines("src/main.rs", 10, 20);
    assert_eq!(span.path, "src/main.rs");
    assert_eq!(span.start_line, Some(10));
    assert_eq!(span.end_line, Some(20));
}

/// EvidenceSpan::file must produce a file-level span.
#[test]
fn evidence_span_file_constructor() {
    let span = EvidenceSpan::file("Cargo.lock");
    assert_eq!(span.path, "Cargo.lock");
    assert_eq!(span.start_line, None);
    assert_eq!(span.end_line, None);
}

// ── ChangedFile total_lines_changed ──────────────────────────────────

#[test]
fn changed_file_total_lines_changed() {
    let f = ChangedFile {
        path: "src/lib.rs".to_owned(),
        old_path: None,
        status: ChangedFileStatus::Modified,
        additions: 15,
        deletions: 7,
        hunks: Vec::new(),
        is_generated: false,
        is_vendor: false,
    };
    assert_eq!(f.total_lines_changed(), 22);
}

// ── End-to-end gate key stability ────────────────────────────────────

/// Full end-to-end: running evaluate twice must produce identical
/// gate idempotency keys.
#[test]
fn gate_key_end_to_end_stability() {
    let raw = vec![
        migration_finding("t1", "sha1", "migrations/001.sql"),
        RawFinding {
            rule_id: TripwireRuleId::CIWorkflowChange,
            report_only: true,
            evidence_path: ".github/workflows/ci.yml".to_owned(),
            evidence_start_line: None,
            evidence_end_line: None,
            evidence_is_excluded: false,
        },
    ];

    let eval_a = make_evaluator(raw.clone());
    let eval_b = make_evaluator(raw);

    let input = sample_input("t1", "sha1");
    let d1 = evaluate(&input, &[eval_a]);
    let d2 = evaluate(&input, &[eval_b]);

    assert_eq!(
        d1.idempotency_key, d2.idempotency_key,
        "gate idempotency key must be identical for same input"
    );
}

/// Different head SHA must produce different gate idempotency key in
/// full end-to-end.
#[test]
fn gate_key_end_to_end_changes_with_head_sha() {
    let raw = vec![migration_finding("t1", "sha1", "migrations/001.sql")];

    let eval_a = make_evaluator(raw.clone());
    let eval_b = make_evaluator(raw);

    let mut input_a = sample_input("t1", "sha_a");
    let mut input_b = sample_input("t1", "sha_b");
    input_a.policy.policy_revision = "org-policy:1".to_owned();
    input_b.policy.policy_revision = "org-policy:1".to_owned();

    let d1 = evaluate(&input_a, &[eval_a]);
    let d2 = evaluate(&input_b, &[eval_b]);

    assert_ne!(
        d1.idempotency_key, d2.idempotency_key,
        "different head SHA must produce different gate key"
    );
}

/// Different policy revision must produce different gate idempotency
/// key in full end-to-end.
#[test]
fn gate_key_end_to_end_changes_with_policy_revision() {
    let raw = vec![migration_finding("t1", "sha1", "migrations/001.sql")];

    let eval_a = make_evaluator(raw.clone());
    let eval_b = make_evaluator(raw);

    let mut input_a = sample_input("t1", "sha1");
    input_a.policy.policy_revision = "org-policy:1".to_owned();
    let mut input_b = sample_input("t1", "sha1");
    input_b.policy.policy_revision = "org-policy:2".to_owned();

    let d1 = evaluate(&input_a, &[eval_a]);
    let d2 = evaluate(&input_b, &[eval_b]);

    assert_ne!(
        d1.idempotency_key, d2.idempotency_key,
        "different policy revision must produce different gate key"
    );
}

// ── Content fingerprint + test-path downgrade annotation ─────────────

fn changed_file(path: &str, added: &[&str], additions: u32, deletions: u32) -> ChangedFile {
    let diff_lines: Vec<String> = added.iter().map(|l| format!("+{l}")).collect();
    ChangedFile {
        path: path.to_owned(),
        old_path: None,
        status: ChangedFileStatus::Modified,
        additions,
        deletions,
        hunks: vec![DiffHunk {
            new_start: 1,
            new_lines: added.len() as u32,
            old_start: 1,
            old_lines: 0,
            diff_lines,
        }],
        is_generated: false,
        is_vendor: false,
    }
}

/// The content fingerprint is head-independent: two heads with the same
/// changed-file content produce the same fingerprint, while the
/// idempotency key (head-dependent) differs.
#[test]
fn content_fingerprint_is_head_independent() {
    let raw = vec![RawFinding {
        rule_id: TripwireRuleId::UnsafeCodeChange,
        report_only: false,
        evidence_path: "src/ffi.rs".to_owned(),
        evidence_start_line: Some(1),
        evidence_end_line: Some(1),
        evidence_is_excluded: false,
    }];
    let file = changed_file("src/ffi.rs", &["unsafe { x() }"], 1, 0);

    let mut input_a = sample_input("t1", "head-a");
    input_a.changed_files = vec![file.clone()];
    let mut input_b = sample_input("t1", "head-b");
    input_b.changed_files = vec![file];

    let d1 = evaluate(&input_a, &[make_evaluator(raw.clone())]);
    let d2 = evaluate(&input_b, &[make_evaluator(raw)]);

    assert_eq!(
        d1.findings[0].content_fingerprint, d2.findings[0].content_fingerprint,
        "identical content must share a fingerprint across heads"
    );
    assert_ne!(
        d1.findings[0].idempotency_key, d2.findings[0].idempotency_key,
        "idempotency key must still vary with head"
    );
    assert!(d1.findings[0].content_fingerprint.starts_with("fp:sha256:"));
}

/// Changed content produces a different fingerprint.
#[test]
fn content_fingerprint_changes_with_content() {
    let raw = vec![RawFinding {
        rule_id: TripwireRuleId::UnsafeCodeChange,
        report_only: false,
        evidence_path: "src/ffi.rs".to_owned(),
        evidence_start_line: Some(1),
        evidence_end_line: Some(1),
        evidence_is_excluded: false,
    }];
    let mut input_a = sample_input("t1", "head-a");
    input_a.changed_files = vec![changed_file("src/ffi.rs", &["unsafe { a() }"], 1, 0)];
    let mut input_b = sample_input("t1", "head-a");
    input_b.changed_files = vec![changed_file("src/ffi.rs", &["unsafe { b() }"], 1, 0)];

    let d1 = evaluate(&input_a, &[make_evaluator(raw.clone())]);
    let d2 = evaluate(&input_b, &[make_evaluator(raw)]);
    assert_ne!(
        d1.findings[0].content_fingerprint, d2.findings[0].content_fingerprint,
        "different content must produce different fingerprints"
    );
}

/// A test-path finding is annotated with the downgrade reason; a
/// production-path finding is not.
#[test]
fn test_path_finding_carries_downgrade_reason() {
    let raw = vec![RawFinding {
        rule_id: TripwireRuleId::UnsafeCodeChange,
        // As the rules layer would emit for a test path.
        report_only: true,
        evidence_path: "crates/foo/src/ffi_test.rs".to_owned(),
        evidence_start_line: Some(1),
        evidence_end_line: Some(1),
        evidence_is_excluded: false,
    }];
    let input = sample_input("t1", "head-a");
    let d = evaluate(&input, &[make_evaluator(raw)]);
    assert_eq!(d.findings[0].severity, TripwireFindingSeverity::ReportOnly);
    assert_eq!(
        d.findings[0].downgrade_reason.as_deref(),
        Some(TEST_PATH_DOWNGRADE_REASON)
    );
    assert_eq!(
        d.findings[0].to_summary().downgrade_reason.as_deref(),
        Some(TEST_PATH_DOWNGRADE_REASON)
    );
}

#[test]
fn production_path_finding_has_no_downgrade_reason() {
    let raw = vec![migration_finding("t1", "head-a", "migrations/001.sql")];
    let input = sample_input("t1", "head-a");
    let d = evaluate(&input, &[make_evaluator(raw)]);
    assert!(d.findings[0].downgrade_reason.is_none());
}
