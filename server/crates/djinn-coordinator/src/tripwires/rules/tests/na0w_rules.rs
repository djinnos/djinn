//! Tests for the large_delete_or_rewrite and ci_workflow_change rule families
//! (na0w), plus sorted-findings, combined-decisions, and sorted-evidence tests.

use super::helpers::*;
use crate::tripwires::engine::{ChangedFile, ChangedFileStatus};
use crate::tripwires::policy::TripwirePolicy;
use crate::tripwires::reason_codes::TripwireRuleId;
use crate::tripwires::rules::{
    all_rule_evaluators, evaluate_ci_workflow_changes,
    evaluate_large_delete_or_rewrite,
};

// ═══════════════════════════════════════════════════════════════════════════
// ── large_delete_or_rewrite: policy helpers ──────────────────────────────
// ═══════════════════════════════════════════════════════════════════════════

fn policy_with_report_only_large_delete() -> TripwirePolicy {
    let mut p = default_policy();
    p.large_delete_rewrite.report_only = true;
    p
}

fn policy_with_report_only_ci_workflow() -> TripwirePolicy {
    let mut p = default_policy();
    p.ci_workflow.report_only = true;
    p
}

// ═══════════════════════════════════════════════════════════════════════════
// ── large_delete_or_rewrite: positive cases ─────────────────────────────
// ═══════════════════════════════════════════════════════════════════════════

/// A single file exceeding the per-file deletion threshold triggers.
#[test]
fn large_delete_per_file_threshold_triggers() {
    let files = vec![simple_file(
        "src/big_module.rs",
        ChangedFileStatus::Modified,
        10,
        500, // exceeds default per_file_line_threshold of 400
    )];
    let findings = evaluate_large_delete_or_rewrite(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].rule_id, TripwireRuleId::LargeDeleteOrRewrite);
    assert_eq!(findings[0].evidence_path, "src/big_module.rs");
    // File-level evidence (no line spans).
    assert!(findings[0].evidence_start_line.is_none());
    assert!(findings[0].evidence_end_line.is_none());
    assert!(!findings[0].report_only);
}

/// Exactly at the per-file threshold does NOT trigger (must exceed).
#[test]
fn large_delete_per_file_threshold_exact_does_not_trigger() {
    let files = vec![simple_file(
        "src/edge.rs",
        ChangedFileStatus::Modified,
        10,
        400, // exactly at threshold, not exceeding
    )];
    let findings = evaluate_large_delete_or_rewrite(&default_policy(), &files);
    // 400 is not > 400, so no per-file trigger.
    // Check percentage: 400/(400+10) = 97.5%, which exceeds 60%.
    // But this depends on whether per_file check triggers first...
    // Per-file: deletions(400) > per_file_line_threshold(400) → false (not strictly greater).
    // Percentage: (400*100)/410 = 97 > 60 → triggers.
    assert_eq!(findings.len(), 1);
}

/// One past the per-file threshold triggers.
#[test]
fn large_delete_per_file_threshold_one_past_triggers() {
    let files = vec![simple_file(
        "src/edge.rs",
        ChangedFileStatus::Modified,
        10,
        401, // 1 over threshold
    )];
    let findings = evaluate_large_delete_or_rewrite(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
}

/// Percentage-based rewrite threshold: file with high churn percentage.
#[test]
fn large_delete_rewrite_percentage_threshold_triggers() {
    // Default: file_rewrite_percentage_threshold = 60
    // 100 deletions, 20 additions → 100/(100+20) = 83% > 60% → trigger.
    let files = vec![simple_file(
        "src/rewrite.rs",
        ChangedFileStatus::Modified,
        20,
        100,
    )];
    let findings = evaluate_large_delete_or_rewrite(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
}

/// Percentage-based: file below threshold does not trigger percentage rule.
#[test]
fn large_delete_rewrite_percentage_below_threshold_no_trigger() {
    // 30 deletions, 70 additions → 30/(100) = 30% < 60%.
    // Per-file: 30 < 400, no trigger.
    let files = vec![simple_file(
        "src/mild.rs",
        ChangedFileStatus::Modified,
        70,
        30,
    )];
    let findings = evaluate_large_delete_or_rewrite(&default_policy(), &files);
    assert!(findings.is_empty());
}

/// Aggregate threshold: enough files with enough deletions.
#[test]
fn large_delete_aggregate_threshold_triggers() {
    // Default: aggregate_min_files = 5, aggregate_line_threshold = 1500
    // Each file: 201 additions, 301 deletions → total=502, pct=301*100/502=59.9% < 60%.
    // Per-file: 301 < 400 → no per-file trigger.
    let files = vec![
        simple_file("src/a.rs", ChangedFileStatus::Modified, 201, 301),
        simple_file("src/b.rs", ChangedFileStatus::Modified, 201, 301),
        simple_file("src/c.rs", ChangedFileStatus::Modified, 201, 301),
        simple_file("src/d.rs", ChangedFileStatus::Modified, 201, 301),
        simple_file("src/e.rs", ChangedFileStatus::Modified, 201, 301),
    ];
    // 5 files, total deletions = 1505 > 1500.
    // Aggregate: 5 files >= 5 min, 1505 > 1500 → triggers once.
    let findings = evaluate_large_delete_or_rewrite(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
    // Evidence is the first file.
    assert_eq!(findings[0].evidence_path, "src/a.rs");
    assert!(findings[0].evidence_start_line.is_none());
}

/// Aggregate threshold not met: not enough files.
#[test]
fn large_delete_aggregate_not_enough_files_no_trigger() {
    // Only 4 files (min is 5).
    let files = vec![
        simple_file("src/a.rs", ChangedFileStatus::Modified, 5, 400),
        simple_file("src/b.rs", ChangedFileStatus::Modified, 5, 400),
        simple_file("src/c.rs", ChangedFileStatus::Modified, 5, 400),
        simple_file("src/d.rs", ChangedFileStatus::Modified, 5, 400),
    ];
    // 4 files: per-file triggers for each (400 > 400 is false; check pct: 400/405=98% > 60%).
    // So 4 per-file percentage findings, no aggregate.
    let findings = evaluate_large_delete_or_rewrite(&default_policy(), &files);
    // Each file triggers on percentage: 400*100/405 = 98 > 60.
    assert_eq!(findings.len(), 4);
}

/// Aggregate threshold: enough files but deletions below threshold.
#[test]
fn large_delete_aggregate_below_line_threshold_no_trigger() {
    // 5 files, but total deletions = 500 < 1500. No individual exceeds 400 or 60%.
    let files = vec![
        simple_file("src/a.rs", ChangedFileStatus::Modified, 100, 90),
        simple_file("src/b.rs", ChangedFileStatus::Modified, 100, 90),
        simple_file("src/c.rs", ChangedFileStatus::Modified, 100, 90),
        simple_file("src/d.rs", ChangedFileStatus::Modified, 100, 90),
        simple_file("src/e.rs", ChangedFileStatus::Modified, 100, 90),
    ];
    // 5 files, total deletions = 450. Percentage: 90/190 = 47% < 60%.
    // No per-file triggers, no aggregate triggers.
    let findings = evaluate_large_delete_or_rewrite(&default_policy(), &files);
    assert!(findings.is_empty());
}

/// Both per-file and aggregate fire together.
#[test]
fn large_delete_per_file_and_aggregate_combined() {
    // big.rs: 500 deletions → per-file trigger (500 > 400, continues before percentage).
    // b..e.rs: 301 deletions, 201 additions → pct=59.96% < 60%, no per-file (301<400).
    let files = vec![
        simple_file("src/big.rs", ChangedFileStatus::Modified, 5, 500), // per-file trigger
        simple_file("src/b.rs", ChangedFileStatus::Modified, 201, 301),
        simple_file("src/c.rs", ChangedFileStatus::Modified, 201, 301),
        simple_file("src/d.rs", ChangedFileStatus::Modified, 201, 301),
        simple_file("src/e.rs", ChangedFileStatus::Modified, 201, 301),
    ];
    // 5 files, total deletions = 500+301*4 = 1704 > 1500 → aggregate trigger.
    let findings = evaluate_large_delete_or_rewrite(&default_policy(), &files);
    // 1 per-file (big.rs) + 1 aggregate = 2.
    assert_eq!(findings.len(), 2);
}

/// File with zero deletions does not trigger.
#[test]
fn large_delete_additions_only_no_trigger() {
    let files = vec![simple_file(
        "src/new_feature.rs",
        ChangedFileStatus::Added,
        1000,
        0,
    )];
    let findings = evaluate_large_delete_or_rewrite(&default_policy(), &files);
    assert!(findings.is_empty());
}

// ── large_delete_or_rewrite: generated/vendor exclusion ─────────────────

/// Excluded files do not count toward thresholds.
#[test]
fn large_delete_generated_file_excluded() {
    let files = vec![ChangedFile {
        is_generated: true,
        ..simple_file("target/gen.rs", ChangedFileStatus::Modified, 5, 500)
    }];
    let findings = evaluate_large_delete_or_rewrite(&default_policy(), &files);
    assert!(findings.is_empty());
}

/// Vendor files do not count toward thresholds.
#[test]
fn large_delete_vendor_file_excluded() {
    let files = vec![ChangedFile {
        is_vendor: true,
        ..simple_file("vendor/lib.rs", ChangedFileStatus::Modified, 5, 500)
    }];
    let findings = evaluate_large_delete_or_rewrite(&default_policy(), &files);
    assert!(findings.is_empty());
}

/// Mix of excluded and real files: only real files count.
#[test]
fn large_delete_mixed_excluded_and_real_files() {
    let files = vec![
        ChangedFile {
            is_generated: true,
            ..simple_file("target/gen.rs", ChangedFileStatus::Modified, 5, 1000)
        },
        simple_file("src/real.rs", ChangedFileStatus::Modified, 10, 100),
    ];
    // Only real.rs counts: 100 < 400, percentage 100/110 = 90% > 60%.
    let findings = evaluate_large_delete_or_rewrite(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].evidence_path, "src/real.rs");
}

// ── large_delete_or_rewrite: report-only / disabled ─────────────────────

/// Report-only flag propagates correctly.
#[test]
fn large_delete_report_only_flag_propagated() {
    let files = vec![simple_file(
        "src/big.rs",
        ChangedFileStatus::Modified,
        10,
        500,
    )];
    let findings =
        evaluate_large_delete_or_rewrite(&policy_with_report_only_large_delete(), &files);
    assert_eq!(findings.len(), 1);
    assert!(findings[0].report_only);
}

/// Disabled rule produces no findings.
#[test]
fn large_delete_rule_disabled_skips_evaluation() {
    let mut policy = default_policy();
    policy.large_delete_rewrite.enabled = false;
    let files = vec![simple_file(
        "src/big.rs",
        ChangedFileStatus::Modified,
        10,
        500,
    )];
    let findings = evaluate_large_delete_or_rewrite(&policy, &files);
    assert!(findings.is_empty());
}

// ── large_delete_or_rewrite: custom threshold tuning ────────────────────

/// Custom per-file threshold respected.
#[test]
fn large_delete_custom_per_file_threshold() {
    let mut policy = default_policy();
    policy.large_delete_rewrite.per_file_line_threshold = 100;
    let files = vec![simple_file(
        "src/mod.rs",
        ChangedFileStatus::Modified,
        5,
        150,
    )];
    let findings = evaluate_large_delete_or_rewrite(&policy, &files);
    // 150 > 100 → per-file trigger.
    assert_eq!(findings.len(), 1);
}

/// Custom aggregate threshold and min_files.
#[test]
fn large_delete_custom_aggregate_threshold() {
    let mut policy = default_policy();
    policy.large_delete_rewrite.aggregate_line_threshold = 200;
    policy.large_delete_rewrite.aggregate_min_files = 2;
    // 2 files, total deletions = 250 > 200.
    let files = vec![
        simple_file("src/a.rs", ChangedFileStatus::Modified, 5, 120),
        simple_file("src/b.rs", ChangedFileStatus::Modified, 5, 130),
    ];
    let findings = evaluate_large_delete_or_rewrite(&policy, &files);
    // No per-file trigger (120 < 400, 130 < 400). Check percentages: 120/125=96%>60%, 130/135=96%>60%.
    // Each file triggers on percentage. Plus aggregate: 2>=2, 250>200.
    assert_eq!(findings.len(), 3);
}

/// Percentage threshold at 100% (only full rewrites trigger).
#[test]
fn large_delete_100_percent_threshold() {
    let mut policy = default_policy();
    policy
        .large_delete_rewrite
        .file_rewrite_percentage_threshold = 100;
    // 100 deletions, 10 additions → 100/110 = 90% < 100%.
    let files = vec![simple_file(
        "src/mostly_deleted.rs",
        ChangedFileStatus::Modified,
        10,
        100,
    )];
    let findings = evaluate_large_delete_or_rewrite(&policy, &files);
    // 100 < 400 per-file, 90% < 100% → no trigger.
    assert!(findings.is_empty());
}

// ── large_delete_or_rewrite: file-level evidence ───────────────────────

/// All findings use file-level evidence (no line spans).
#[test]
fn large_delete_findings_use_file_level_evidence() {
    let files = vec![simple_file(
        "src/huge.rs",
        ChangedFileStatus::Modified,
        5,
        600,
    )];
    let findings = evaluate_large_delete_or_rewrite(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
    assert!(findings[0].evidence_start_line.is_none());
    assert!(findings[0].evidence_end_line.is_none());
}

// ── large_delete_or_rewrite: deleted file status ───────────────────────

/// A fully deleted file triggers (deletions count).
#[test]
fn large_delete_fully_deleted_file_triggers() {
    let files = vec![simple_file(
        "src/removed.rs",
        ChangedFileStatus::Deleted,
        0,
        500,
    )];
    let findings = evaluate_large_delete_or_rewrite(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].evidence_path, "src/removed.rs");
}

// ═══════════════════════════════════════════════════════════════════════════
// ── ci_workflow_change: positive cases ──────────────────────────────────
// ═══════════════════════════════════════════════════════════════════════════

/// GitHub workflow file triggers CI workflow finding.
#[test]
fn ci_workflow_github_workflows_triggers() {
    let files = vec![simple_file(
        ".github/workflows/ci.yml",
        ChangedFileStatus::Modified,
        10,
        5,
    )];
    let findings = evaluate_ci_workflow_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].rule_id, TripwireRuleId::CIWorkflowChange);
    assert_eq!(findings[0].evidence_path, ".github/workflows/ci.yml");
    // File-level evidence.
    assert!(findings[0].evidence_start_line.is_none());
    assert!(findings[0].evidence_end_line.is_none());
    assert!(!findings[0].report_only);
}

/// GitHub actions directory triggers.
#[test]
fn ci_workflow_github_actions_triggers() {
    let files = vec![simple_file(
        ".github/actions/deploy/action.yml",
        ChangedFileStatus::Added,
        50,
        0,
    )];
    let findings = evaluate_ci_workflow_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
}

/// GitLab CI file triggers.
#[test]
fn ci_workflow_gitlab_ci_triggers() {
    let files = vec![simple_file(
        ".gitlab-ci.yml",
        ChangedFileStatus::Modified,
        5,
        2,
    )];
    let findings = evaluate_ci_workflow_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
}

/// CircleCI config triggers.
#[test]
fn ci_workflow_circleci_triggers() {
    let files = vec![simple_file(
        ".circleci/config.yml",
        ChangedFileStatus::Modified,
        3,
        1,
    )];
    let findings = evaluate_ci_workflow_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
}

/// Deploy directory triggers.
#[test]
fn ci_workflow_deploy_directory_triggers() {
    let files = vec![simple_file(
        "deploy/production/manifest.yaml",
        ChangedFileStatus::Modified,
        5,
        2,
    )];
    let findings = evaluate_ci_workflow_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
}

/// Release directory triggers.
#[test]
fn ci_workflow_release_directory_triggers() {
    let files = vec![simple_file(
        "release/scripts/cut-release.sh",
        ChangedFileStatus::Added,
        100,
        0,
    )];
    let findings = evaluate_ci_workflow_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
}

/// Tiltfile triggers.
#[test]
fn ci_workflow_tiltfile_triggers() {
    let files = vec![simple_file("Tiltfile", ChangedFileStatus::Modified, 10, 5)];
    let findings = evaluate_ci_workflow_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
}

/// Makefile triggers.
#[test]
fn ci_workflow_makefile_triggers() {
    let files = vec![simple_file("Makefile", ChangedFileStatus::Modified, 5, 3)];
    let findings = evaluate_ci_workflow_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
}

/// Boundary check script triggers.
#[test]
fn ci_workflow_boundary_check_script_triggers() {
    let files = vec![simple_file(
        "scripts/check-capability-boundary.sh",
        ChangedFileStatus::Modified,
        10,
        5,
    )];
    let findings = evaluate_ci_workflow_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
}

/// Multiple CI files produce multiple findings.
#[test]
fn ci_workflow_multiple_files_multiple_findings() {
    let files = vec![
        simple_file(
            ".github/workflows/ci.yml",
            ChangedFileStatus::Modified,
            5,
            2,
        ),
        simple_file(
            ".github/workflows/deploy.yml",
            ChangedFileStatus::Added,
            100,
            0,
        ),
        simple_file("Makefile", ChangedFileStatus::Modified, 3, 1),
    ];
    let findings = evaluate_ci_workflow_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 3);
}

/// Renamed CI file (old path in watched set) triggers.
#[test]
fn ci_workflow_renamed_from_watched_path_triggers() {
    let files = vec![ChangedFile {
        path: ".github/workflows/build.yml".to_owned(),
        old_path: Some(".github/workflows/ci.yml".to_owned()),
        status: ChangedFileStatus::Renamed,
        additions: 0,
        deletions: 0,
        hunks: Vec::new(),
        is_generated: false,
        is_vendor: false,
    }];
    let findings = evaluate_ci_workflow_changes(&default_policy(), &files);
    assert_eq!(findings.len(), 1);
}

// ── ci_workflow_change: negative cases ──────────────────────────────────

/// Non-CI file does not trigger.
#[test]
fn ci_workflow_non_ci_file_does_not_trigger() {
    let files = vec![simple_file(
        "src/main.rs",
        ChangedFileStatus::Modified,
        10,
        5,
    )];
    let findings = evaluate_ci_workflow_changes(&default_policy(), &files);
    assert!(findings.is_empty());
}

/// Regular scripts outside CI patterns do not trigger.
#[test]
fn ci_workflow_random_script_does_not_trigger() {
    let files = vec![simple_file(
        "scripts/generate-docs.sh",
        ChangedFileStatus::Modified,
        5,
        2,
    )];
    let findings = evaluate_ci_workflow_changes(&default_policy(), &files);
    assert!(findings.is_empty());
}

/// Disabled rule produces no findings.
#[test]
fn ci_workflow_rule_disabled_skips_evaluation() {
    let mut policy = default_policy();
    policy.ci_workflow.enabled = false;
    let files = vec![simple_file(
        ".github/workflows/ci.yml",
        ChangedFileStatus::Modified,
        10,
        5,
    )];
    let findings = evaluate_ci_workflow_changes(&policy, &files);
    assert!(findings.is_empty());
}

/// Empty path_globs produces no findings.
#[test]
fn ci_workflow_empty_globs_no_findings() {
    let mut policy = default_policy();
    policy.ci_workflow.path_globs = Vec::new();
    let files = vec![simple_file(
        ".github/workflows/ci.yml",
        ChangedFileStatus::Modified,
        10,
        5,
    )];
    let findings = evaluate_ci_workflow_changes(&policy, &files);
    assert!(findings.is_empty());
}

// ── ci_workflow_change: generated/vendor exclusion ──────────────────────

/// Generated CI file is excluded.
#[test]
fn ci_workflow_generated_file_excluded() {
    let files = vec![ChangedFile {
        is_generated: true,
        ..simple_file(
            ".github/workflows/ci.yml",
            ChangedFileStatus::Modified,
            10,
            5,
        )
    }];
    let findings = evaluate_ci_workflow_changes(&default_policy(), &files);
    assert!(findings.is_empty());
}

/// Vendor CI file is excluded.
#[test]
fn ci_workflow_vendor_file_excluded() {
    let files = vec![ChangedFile {
        is_vendor: true,
        ..simple_file(
            ".github/workflows/ci.yml",
            ChangedFileStatus::Modified,
            10,
            5,
        )
    }];
    let findings = evaluate_ci_workflow_changes(&default_policy(), &files);
    assert!(findings.is_empty());
}

// ── ci_workflow_change: report-only / disabled ──────────────────────────

/// Report-only flag propagates.
#[test]
fn ci_workflow_report_only_flag_propagated() {
    let files = vec![simple_file(
        ".github/workflows/ci.yml",
        ChangedFileStatus::Modified,
        10,
        5,
    )];
    let findings = evaluate_ci_workflow_changes(&policy_with_report_only_ci_workflow(), &files);
    assert_eq!(findings.len(), 1);
    assert!(findings[0].report_only);
}

// ── ci_workflow_change: custom globs ────────────────────────────────────

/// Custom path_globs are respected.
#[test]
fn ci_workflow_custom_globs_respected() {
    let mut policy = default_policy();
    policy.ci_workflow.path_globs = vec!["buildkite/**".to_owned()];
    let files = vec![
        simple_file("buildkite/pipeline.yml", ChangedFileStatus::Modified, 5, 2),
        simple_file(
            ".github/workflows/ci.yml",
            ChangedFileStatus::Modified,
            5,
            2,
        ),
    ];
    let findings = evaluate_ci_workflow_changes(&policy, &files);
    // Only buildkite matches the custom globs.
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].evidence_path, "buildkite/pipeline.yml");
}

// ═══════════════════════════════════════════════════════════════════════════
// ── Sorted findings ────────────────────────────────────────────────────
// ═══════════════════════════════════════════════════════════════════════════

/// Findings from new rules integrate with engine sorting.
#[test]
fn large_delete_and_ci_workflow_findings_sort_deterministically() {
    use crate::tripwires::engine::{TripwireEvaluationInput, evaluate};

    let client_line = format!("let client = {}::Client::new();", "reqwest");
    let files = vec![
        simple_file("migrations/001.sql", ChangedFileStatus::Added, 10, 0),
        file_with_added_lines("src/http_client.rs", &[client_line.as_str()]),
        file_with_added_lines("src/native.rs", &["unsafe { ptr::null() }"]),
        simple_file(
            "server/src/auth/login.rs",
            ChangedFileStatus::Modified,
            10,
            5,
        ),
        simple_file(
            ".github/workflows/ci.yml",
            ChangedFileStatus::Modified,
            5,
            2,
        ),
        simple_file("src/big.rs", ChangedFileStatus::Modified, 5, 500),
    ];

    let input = TripwireEvaluationInput {
        task_id: "sort_test".to_owned(),
        project_id: "proj_1".to_owned(),
        pr_number: Some(1),
        head_sha: "abc123".to_owned(),
        policy: default_policy(),
        allowlist_revision: None,
        changed_files: files,
    };

    let evaluators = all_rule_evaluators();
    let decision = evaluate(&input, &evaluators);

    // Verify findings are sorted by (rule_id, path, start_line, end_line, severity).
    for window in decision.findings.windows(2) {
        let a = &window[0];
        let b = &window[1];
        let ord = a
            .rule_id
            .as_str()
            .cmp(b.rule_id.as_str())
            .then_with(|| a.evidence.path.cmp(&b.evidence.path))
            .then_with(|| a.evidence.start_line.cmp(&b.evidence.start_line))
            .then_with(|| a.evidence.end_line.cmp(&b.evidence.end_line))
            .then_with(|| a.severity.cmp(&b.severity));
        assert!(
            ord != std::cmp::Ordering::Greater,
            "findings not sorted: {:?} should come before {:?}",
            a,
            b
        );
    }

    // CI workflow and large delete findings should be present.
    let rule_ids: Vec<TripwireRuleId> = decision.findings.iter().map(|f| f.rule_id).collect();
    assert!(rule_ids.contains(&TripwireRuleId::CIWorkflowChange));
    assert!(rule_ids.contains(&TripwireRuleId::LargeDeleteOrRewrite));
}

// ═══════════════════════════════════════════════════════════════════════════
// ── Combined decisions ─────────────────────────────────────────────────
// ═══════════════════════════════════════════════════════════════════════════

/// Combined: migration + large delete + CI workflow all fire in one gate.
#[test]
fn combined_migration_large_delete_ci_workflow_gate() {
    use crate::tripwires::engine::{GateOutcome, TripwireEvaluationInput, evaluate};

    let files = vec![
        simple_file("migrations/001.sql", ChangedFileStatus::Added, 50, 0),
        simple_file(
            ".github/workflows/ci.yml",
            ChangedFileStatus::Modified,
            10,
            5,
        ),
        simple_file("src/big.rs", ChangedFileStatus::Modified, 5, 500),
    ];

    let input = TripwireEvaluationInput {
        task_id: "combo_test".to_owned(),
        project_id: "proj_1".to_owned(),
        pr_number: Some(99),
        head_sha: "def456".to_owned(),
        policy: default_policy(),
        allowlist_revision: None,
        changed_files: files,
    };

    let evaluators = all_rule_evaluators();
    let decision = evaluate(&input, &evaluators);

    assert_eq!(decision.outcome, GateOutcome::Held);
    assert_eq!(decision.enforcement_finding_count, 3);
    assert_eq!(decision.report_only_finding_count, 0);

    let rule_ids: Vec<TripwireRuleId> = decision.findings.iter().map(|f| f.rule_id).collect();
    assert!(rule_ids.contains(&TripwireRuleId::MigrationChange));
    assert!(rule_ids.contains(&TripwireRuleId::CIWorkflowChange));
    assert!(rule_ids.contains(&TripwireRuleId::LargeDeleteOrRewrite));
}

/// Combined: all report-only produces ReportOnly outcome.
#[test]
fn combined_all_rules_report_only_outcome() {
    use crate::tripwires::engine::{GateOutcome, TripwireEvaluationInput, evaluate};

    let files = vec![
        simple_file("migrations/001.sql", ChangedFileStatus::Added, 50, 0),
        simple_file(
            ".github/workflows/ci.yml",
            ChangedFileStatus::Modified,
            10,
            5,
        ),
        simple_file("src/big.rs", ChangedFileStatus::Modified, 5, 500),
    ];

    let mut policy = default_policy();
    policy.migration.report_only = true;
    policy.large_delete_rewrite.report_only = true;
    policy.ci_workflow.report_only = true;

    let input = TripwireEvaluationInput {
        task_id: "combo_ro".to_owned(),
        project_id: "proj_1".to_owned(),
        pr_number: Some(99),
        head_sha: "ghi789".to_owned(),
        policy,
        allowlist_revision: None,
        changed_files: files,
    };

    let evaluators = all_rule_evaluators();
    let decision = evaluate(&input, &evaluators);

    assert_eq!(decision.outcome, GateOutcome::ReportOnly);
    assert_eq!(decision.enforcement_finding_count, 0);
    assert_eq!(decision.report_only_finding_count, 3);
}

/// Combined: mixed enforcement and report-only across rule families.
#[test]
fn combined_mixed_enforcement_and_report_only() {
    use crate::tripwires::engine::{GateOutcome, TripwireEvaluationInput, evaluate};

    let client_line = format!("let client = {}::Client::new();", "reqwest");
    let files = vec![
        simple_file("migrations/001.sql", ChangedFileStatus::Added, 10, 0),
        file_with_added_lines("src/http_client.rs", &[client_line.as_str()]),
        simple_file(
            ".github/workflows/ci.yml",
            ChangedFileStatus::Modified,
            5,
            2,
        ),
        simple_file("src/big.rs", ChangedFileStatus::Modified, 5, 500),
    ];

    // Migration and large_delete are enforcement, CI is report-only.
    let mut policy = default_policy();
    policy.ci_workflow.report_only = true;

    let input = TripwireEvaluationInput {
        task_id: "mixed_test".to_owned(),
        project_id: "proj_1".to_owned(),
        pr_number: Some(50),
        head_sha: "sha_mixed".to_owned(),
        policy,
        allowlist_revision: None,
        changed_files: files,
    };

    let evaluators = all_rule_evaluators();
    let decision = evaluate(&input, &evaluators);

    // Held because at least one enforcement finding (migration + large_delete + egress).
    assert_eq!(decision.outcome, GateOutcome::Held);
    assert!(decision.enforcement_finding_count > 0);
    // CI workflow finding should be report-only.
    let ci_findings: Vec<_> = decision
        .findings
        .iter()
        .filter(|f| f.rule_id == TripwireRuleId::CIWorkflowChange)
        .collect();
    assert_eq!(ci_findings.len(), 1);
    assert_eq!(
        ci_findings[0].severity,
        crate::tripwires::engine::TripwireFindingSeverity::ReportOnly
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// ── large_delete_or_rewrite: sorted evidence ───────────────────────────
// ═══════════════════════════════════════════════════════════════════════════

/// Multiple large-delete findings are deterministically sorted when run
/// through the engine.
#[test]
fn large_delete_findings_deterministic_ordering() {
    use crate::tripwires::engine::{TripwireEvaluationInput, evaluate};

    // Create files that trigger both per-file and aggregate.
    let files = vec![
        simple_file("src/zzz.rs", ChangedFileStatus::Modified, 5, 500),
        simple_file("src/aaa.rs", ChangedFileStatus::Modified, 5, 500),
    ];

    let input = TripwireEvaluationInput {
        task_id: "order_test".to_owned(),
        project_id: "proj_1".to_owned(),
        pr_number: Some(1),
        head_sha: "sha_order".to_owned(),
        policy: default_policy(),
        allowlist_revision: None,
        changed_files: files,
    };

    let decision = evaluate(&input, &all_rule_evaluators());

    // Find the large_delete findings.
    let large_findings: Vec<_> = decision
        .findings
        .iter()
        .filter(|f| f.rule_id == TripwireRuleId::LargeDeleteOrRewrite)
        .collect();

    // Should be sorted by path.
    for window in large_findings.windows(2) {
        assert!(
            window[0].evidence.path <= window[1].evidence.path,
            "large delete findings not sorted by path: {} > {}",
            window[0].evidence.path,
            window[1].evidence.path,
        );
    }
}
