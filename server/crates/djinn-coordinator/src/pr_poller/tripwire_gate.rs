//! Tripwire gate evaluation for the PR poller.
//!
//! Fetches PR changed files via the GitHub API, converts them to
//! [`crate::tripwires::engine::ChangedFile`], evaluates them against the
//! deterministic tripwire engine, and produces a typed
//! [`TripwireGateDecision`] plus the matching activity-event type and
//! [`TripwireGateDecisionPayload`] for logging.
//!
//! This module is additive plumbing — it does not redefine any tripwire
//! contract types (policy, reason codes, activity payloads, idempotency keys).

use anyhow::Result;
use djinn_provider::github_api::{GitHubApiClient, PrFile};

use crate::tripwires::{
    ChangedFile, ChangedFileStatus, GateOutcome, TRIPWIRE_EVENT_GATE_HELD,
    TRIPWIRE_EVENT_GATE_PASSED, TRIPWIRE_EVENT_GATE_REPORT_ONLY, TripwireEvaluationInput,
    TripwireFindingSummary, TripwireGateDecision, TripwireGateDecisionPayload, TripwirePolicy,
    all_rule_evaluators, evaluate,
};

// ─── PrFile → ChangedFile conversion ─────────────────────────────────────

/// Convert a slice of GitHub API [`PrFile`]s to the engine's [`ChangedFile`]
/// representation.
///
/// The GitHub PR-files endpoint (`GET /pulls/{n}/files`) returns a flat
/// list with `status`, `additions`, `deletions`, and `filename` fields.
/// Diff hunks are **not** included in this endpoint's response, so the
/// converted [`ChangedFile`]s have empty `hunks` — rules that scan diff
/// lines (network egress, unsafe code) will fall back to file-level
/// evidence or skip the file when no hunks are present.
///
/// Files with unrecognised `status` strings are mapped to `Modified` as
/// a conservative default (GitHub may add new statuses).
pub fn convert_pr_files(pr_files: &[PrFile]) -> Vec<ChangedFile> {
    pr_files
        .iter()
        .map(|pf| ChangedFile {
            path: pf.filename.clone(),
            old_path: None, // GitHub's PR-files endpoint doesn't expose old_filename
            // in the minimal model; renames are flagged by status only.
            status: match pf.status.as_str() {
                "added" => ChangedFileStatus::Added,
                "removed" => ChangedFileStatus::Deleted,
                "renamed" => ChangedFileStatus::Renamed,
                _ => ChangedFileStatus::Modified, // "modified" + unknown
            },
            additions: pf.additions,
            deletions: pf.deletions,
            hunks: Vec::new(), // PR-files endpoint doesn't include per-hunk diffs
            is_generated: false,
            is_vendor: false,
        })
        .collect()
}

// ─── Gate evaluation result ──────────────────────────────────────────────

/// Result of a tripwire gate evaluation for a PR head.
///
/// Carries the deterministic [`TripwireGateDecision`], the matching
/// activity event type literal, and the pre-built
/// [`TripwireGateDecisionPayload`] ready for persistence.
#[derive(Debug, Clone)]
pub struct TripwireGateResult {
    /// The deterministic gate decision.
    pub decision: TripwireGateDecision,
    /// Activity event type: one of `TRIPWIRE_EVENT_GATE_HELD`,
    /// `TRIPWIRE_EVENT_GATE_PASSED`, `TRIPWIRE_EVENT_GATE_REPORT_ONLY`.
    pub event_type: &'static str,
    /// Pre-built activity payload for persistence.
    pub payload: TripwireGateDecisionPayload,
}

// ─── Gate evaluation ─────────────────────────────────────────────────────

/// Evaluate the tripwire engine with the given input and evaluators.
///
/// This is a thin helper that calls [`evaluate`] with dereffed boxed
/// evaluators and wraps the result into a [`TripwireGateResult`].
fn run_gate(input: &TripwireEvaluationInput) -> TripwireGateResult {
    let evaluators = all_rule_evaluators();
    // Box<dyn Fn(...) + Send + Sync> implements Fn(...) via blanket impl,
    // so passing the boxed vec as a slice satisfies evaluate's generic bound.
    let decision = evaluate(input, &evaluators);

    let event_type = match decision.outcome {
        GateOutcome::Held => TRIPWIRE_EVENT_GATE_HELD,
        GateOutcome::Passed => TRIPWIRE_EVENT_GATE_PASSED,
        GateOutcome::ReportOnly => TRIPWIRE_EVENT_GATE_REPORT_ONLY,
    };

    let findings: Vec<TripwireFindingSummary> =
        decision.findings.iter().map(|f| f.to_summary()).collect();

    let now = ::time::OffsetDateTime::now_utc()
        .format(&::time::format_description::well_known::Rfc3339)
        .unwrap_or_default();

    let payload = TripwireGateDecisionPayload {
        event_type: event_type.to_owned(),
        task_id: input.task_id.clone(),
        project_id: input.project_id.clone(),
        pr_number: input.pr_number,
        head_sha: input.head_sha.clone(),
        base_sha: None,
        policy_revision: decision.policy_revision.clone(),
        allowlist_revision: decision.allowlist_revision.clone(),
        findings,
        enforcement_finding_count: decision.enforcement_finding_count,
        report_only_finding_count: decision.report_only_finding_count,
        idempotency_key: decision.idempotency_key.clone(),
        decided_at: Some(now),
    };

    TripwireGateResult {
        decision,
        event_type,
        payload,
    }
}

/// Fetch PR changed files, evaluate the tripwire engine, and return a
/// [`TripwireGateResult`].
///
/// Uses [`TripwirePolicy::default`] (the safe, enforcement-on posture)
/// and [`all_rule_evaluators`] (all seven rule families). No LLM or
/// provider call is made in the gate path — only the GitHub PR-files
/// endpoint and the pure deterministic engine.
///
/// # Arguments
///
/// * `gh_client` — authenticated GitHub API client for the installation.
/// * `owner`, `repo` — repository owner/name.
/// * `pull_number` — PR number.
/// * `task_id` — task UUID for idempotency key derivation.
/// * `project_id` — project UUID for the activity payload.
/// * `head_sha` — current head SHA of the PR.
pub async fn evaluate_tripwire_gate(
    gh_client: &GitHubApiClient,
    owner: &str,
    repo: &str,
    pull_number: u64,
    task_id: &str,
    project_id: &str,
    head_sha: &str,
) -> Result<TripwireGateResult> {
    // 1. Fetch changed files from GitHub.
    let pr_files = gh_client.get_pr_files(owner, repo, pull_number).await?;

    // 2. Convert to engine types.
    let changed_files = convert_pr_files(&pr_files);

    // 3. Build evaluation input with default policy.
    let input = TripwireEvaluationInput {
        task_id: task_id.to_owned(),
        project_id: project_id.to_owned(),
        pr_number: Some(pull_number),
        head_sha: head_sha.to_owned(),
        policy: TripwirePolicy::default(),
        allowlist_revision: None,
        changed_files,
    };

    // 4. Evaluate with all seven rule families.
    Ok(run_gate(&input))
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tripwires::{
        ChangedFileStatus, DiffHunk, TRIPWIRE_EVENT_GATE_HELD, TRIPWIRE_EVENT_GATE_PASSED,
        TRIPWIRE_EVENT_GATE_REPORT_ONLY, TripwireFindingSeverity,
    };

    /// Helper: build a `PrFile` with the given fields.
    fn pr_file(filename: &str, status: &str, additions: u32, deletions: u32) -> PrFile {
        PrFile {
            sha: "deadbeef".to_owned(),
            filename: filename.to_owned(),
            status: status.to_owned(),
            additions,
            deletions,
            changes: additions + deletions,
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
    fn convert_pr_files_produces_empty_hunks() {
        let files = vec![pr_file("src/lib.rs", "modified", 10, 5)];
        let converted = convert_pr_files(&files);
        assert!(converted[0].hunks.is_empty());
    }

    #[test]
    fn convert_pr_files_empty_input() {
        let converted = convert_pr_files(&[]);
        assert!(converted.is_empty());
    }

    // ── Evaluation tests (offline, no GitHub API) ───────────────────────
    //
    // These tests call `run_gate` directly with synthetic `ChangedFile`
    // inputs so they cover the seven rule families without network calls.

    /// Helper: build a `ChangedFile` for testing.
    fn changed_file(
        path: &str,
        status: ChangedFileStatus,
        additions: u32,
        deletions: u32,
    ) -> ChangedFile {
        ChangedFile {
            path: path.to_owned(),
            old_path: None,
            status,
            additions,
            deletions,
            hunks: Vec::new(),
            is_generated: false,
            is_vendor: false,
        }
    }

    /// Helper: build a `ChangedFile` with diff hunks for line-scanning rules.
    fn changed_file_with_hunks(
        path: &str,
        additions: u32,
        deletions: u32,
        hunks: Vec<DiffHunk>,
    ) -> ChangedFile {
        ChangedFile {
            path: path.to_owned(),
            old_path: None,
            status: ChangedFileStatus::Modified,
            additions,
            deletions,
            hunks,
            is_generated: false,
            is_vendor: false,
        }
    }

    /// Helper: run the tripwire engine on given changed files with default policy.
    fn evaluate_default(changed_files: Vec<ChangedFile>) -> TripwireGateDecision {
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

    /// Helper: select the event type from a gate decision.
    fn event_type_for(decision: &TripwireGateDecision) -> &'static str {
        match decision.outcome {
            GateOutcome::Held => TRIPWIRE_EVENT_GATE_HELD,
            GateOutcome::Passed => TRIPWIRE_EVENT_GATE_PASSED,
            GateOutcome::ReportOnly => TRIPWIRE_EVENT_GATE_REPORT_ONLY,
        }
    }

    // ── Rule 1: migration_change ────────────────────────────────────────

    #[test]
    fn migration_change_produces_held_gate() {
        let files = vec![changed_file(
            "migrations/20260101_create_users.sql",
            ChangedFileStatus::Added,
            20,
            0,
        )];
        let decision = evaluate_default(files);
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

    // ── Rule 2: dependency_identity_change ──────────────────────────────

    #[test]
    fn dependency_identity_change_produces_held_gate() {
        let files = vec![changed_file(
            "Cargo.toml",
            ChangedFileStatus::Modified,
            5,
            3,
        )];
        let decision = evaluate_default(files);
        assert_eq!(decision.outcome, GateOutcome::Held);
        assert!(
            decision
                .findings
                .iter()
                .any(|f| f.rule_id.as_str() == "dependency_identity_change")
        );
    }

    // ── Rule 3: network_egress_change ───────────────────────────────────

    #[test]
    fn network_egress_change_produces_held_gate() {
        let hunk = DiffHunk {
            new_start: 10,
            new_lines: 3,
            old_start: 10,
            old_lines: 1,
            diff_lines: vec![
                " context line".to_owned(),
                "+use reqwest::Client;".to_owned(),
                " another context".to_owned(),
            ],
        };
        let files = vec![changed_file_with_hunks("src/http.rs", 3, 1, vec![hunk])];
        let decision = evaluate_default(files);
        assert_eq!(decision.outcome, GateOutcome::Held);
        assert!(
            decision
                .findings
                .iter()
                .any(|f| f.rule_id.as_str() == "network_egress_change")
        );
        // Verify evidence is line-precise.
        let egress_finding = decision
            .findings
            .iter()
            .find(|f| f.rule_id.as_str() == "network_egress_change")
            .unwrap();
        assert!(egress_finding.evidence.start_line.is_some());
    }

    // ── Rule 4: unsafe_code_change ──────────────────────────────────────

    #[test]
    fn unsafe_code_change_produces_held_gate() {
        let hunk = DiffHunk {
            new_start: 1,
            new_lines: 2,
            old_start: 1,
            old_lines: 0,
            diff_lines: vec!["+unsafe {".to_owned(), "+    ptr::null();".to_owned()],
        };
        let files = vec![changed_file_with_hunks("src/ffi.rs", 2, 0, vec![hunk])];
        let decision = evaluate_default(files);
        assert_eq!(decision.outcome, GateOutcome::Held);
        assert!(
            decision
                .findings
                .iter()
                .any(|f| f.rule_id.as_str() == "unsafe_code_change")
        );
    }

    // ── Rule 5: boundary_path_change ────────────────────────────────────

    #[test]
    fn boundary_path_change_produces_held_gate() {
        let files = vec![changed_file(
            "src/auth/permissions.rs",
            ChangedFileStatus::Added,
            100,
            0,
        )];
        let decision = evaluate_default(files);
        assert_eq!(decision.outcome, GateOutcome::Held);
        assert!(
            decision
                .findings
                .iter()
                .any(|f| f.rule_id.as_str() == "boundary_path_change")
        );
        // Boundary findings carry an allowlist revision.
        let boundary_finding = decision
            .findings
            .iter()
            .find(|f| f.rule_id.as_str() == "boundary_path_change")
            .unwrap();
        assert!(boundary_finding.allowlist_revision.is_some());
    }

    // ── Rule 6: large_delete_or_rewrite ─────────────────────────────────

    #[test]
    fn large_delete_or_rewrite_produces_held_gate() {
        let files = vec![changed_file(
            "src/old_module.rs",
            ChangedFileStatus::Modified,
            10,
            600, // Exceeds default per-file threshold of 500
        )];
        let decision = evaluate_default(files);
        assert_eq!(decision.outcome, GateOutcome::Held);
        assert!(
            decision
                .findings
                .iter()
                .any(|f| f.rule_id.as_str() == "large_delete_or_rewrite")
        );
    }

    // ── Rule 7: ci_workflow_change ──────────────────────────────────────

    #[test]
    fn ci_workflow_change_produces_held_gate() {
        let files = vec![changed_file(
            ".github/workflows/ci.yml",
            ChangedFileStatus::Modified,
            15,
            5,
        )];
        let decision = evaluate_default(files);
        assert_eq!(decision.outcome, GateOutcome::Held);
        assert!(
            decision
                .findings
                .iter()
                .any(|f| f.rule_id.as_str() == "ci_workflow_change")
        );
    }

    // ── Report-only scenario ────────────────────────────────────────────

    #[test]
    fn report_only_findings_produce_report_only_gate() {
        // Build a policy where migration changes are report-only.
        let mut policy = TripwirePolicy::default();
        policy.migration.report_only = true;

        let files = vec![ChangedFile {
            path: "migrations/001_init.sql".to_owned(),
            old_path: None,
            status: ChangedFileStatus::Added,
            additions: 50,
            deletions: 0,
            hunks: Vec::new(),
            is_generated: false,
            is_vendor: false,
        }];

        let input = TripwireEvaluationInput {
            task_id: "task-002".to_owned(),
            project_id: "proj-001".to_owned(),
            pr_number: Some(99),
            head_sha: "def456".to_owned(),
            policy,
            allowlist_revision: None,
            changed_files: files,
        };
        let result = run_gate(&input);
        let decision = &result.decision;

        assert_eq!(decision.outcome, GateOutcome::ReportOnly);
        assert_eq!(decision.enforcement_finding_count, 0);
        assert!(decision.report_only_finding_count > 0);
        assert_eq!(event_type_for(decision), TRIPWIRE_EVENT_GATE_REPORT_ONLY);

        // Verify findings carry report-only severity.
        for f in &decision.findings {
            assert_eq!(f.severity, TripwireFindingSeverity::ReportOnly);
        }
    }

    // ── Passed (no findings) ────────────────────────────────────────────

    #[test]
    fn no_matching_files_produces_passed_gate() {
        let files = vec![changed_file(
            "src/main.rs",
            ChangedFileStatus::Modified,
            5,
            2,
        )];
        let decision = evaluate_default(files);
        assert_eq!(decision.outcome, GateOutcome::Passed);
        assert_eq!(decision.enforcement_finding_count, 0);
        assert_eq!(decision.report_only_finding_count, 0);
        assert!(decision.findings.is_empty());
        assert_eq!(event_type_for(&decision), TRIPWIRE_EVENT_GATE_PASSED);
    }

    // ── Idempotency key determinism ─────────────────────────────────────

    #[test]
    fn gate_idempotency_key_is_deterministic() {
        let files = vec![changed_file(
            "migrations/001.sql",
            ChangedFileStatus::Added,
            10,
            0,
        )];
        let d1 = evaluate_default(files.clone());
        let d2 = evaluate_default(files);
        assert_eq!(d1.idempotency_key, d2.idempotency_key);
    }

    // ── Payload validation ──────────────────────────────────────────────

    #[test]
    fn payload_validation_passes_for_consistent_decision() {
        let files = vec![changed_file(
            "migrations/001.sql",
            ChangedFileStatus::Added,
            10,
            0,
        )];
        let input = TripwireEvaluationInput {
            task_id: "task-001".to_owned(),
            project_id: "proj-001".to_owned(),
            pr_number: Some(42),
            head_sha: "abc123".to_owned(),
            policy: TripwirePolicy::default(),
            allowlist_revision: None,
            changed_files: files,
        };
        let result = run_gate(&input);
        result
            .payload
            .validate()
            .expect("payload must pass validation for a consistent decision");
    }

    // ── Mixed findings: enforcement dominates ───────────────────────────

    #[test]
    fn mixed_findings_enforcement_dominates_over_report_only() {
        // Migration (enforcement) + CI workflow (report-only) → Held.
        let mut policy = TripwirePolicy::default();
        policy.ci_workflow.report_only = true;

        let files = vec![
            ChangedFile {
                path: "migrations/002.sql".to_owned(),
                old_path: None,
                status: ChangedFileStatus::Added,
                additions: 20,
                deletions: 0,
                hunks: Vec::new(),
                is_generated: false,
                is_vendor: false,
            },
            ChangedFile {
                path: ".github/workflows/release.yml".to_owned(),
                old_path: None,
                status: ChangedFileStatus::Modified,
                additions: 10,
                deletions: 5,
                hunks: Vec::new(),
                is_generated: false,
                is_vendor: false,
            },
        ];

        let input = TripwireEvaluationInput {
            task_id: "task-mixed".to_owned(),
            project_id: "proj-001".to_owned(),
            pr_number: Some(77),
            head_sha: "mixed-sha".to_owned(),
            policy,
            allowlist_revision: None,
            changed_files: files,
        };
        let result = run_gate(&input);
        let decision = &result.decision;

        assert_eq!(decision.outcome, GateOutcome::Held);
        assert!(decision.enforcement_finding_count > 0);
        assert!(decision.report_only_finding_count > 0);
        assert_eq!(event_type_for(decision), TRIPWIRE_EVENT_GATE_HELD);
    }

    // ── Generated/vendor files are excluded ─────────────────────────────

    #[test]
    fn generated_files_are_excluded_from_evaluation() {
        let files = vec![ChangedFile {
            path: "generated/bindings.rs".to_owned(),
            old_path: None,
            status: ChangedFileStatus::Added,
            additions: 5000,
            deletions: 0,
            hunks: Vec::new(),
            is_generated: true, // This file is classified as generated
            is_vendor: false,
        }];
        let decision = evaluate_default(files);
        assert_eq!(decision.outcome, GateOutcome::Passed);
        assert!(decision.findings.is_empty());
    }

    // ── All seven rule families can surface findings ────────────────────

    #[test]
    fn all_seven_rule_families_produce_findings() {
        let mut policy = TripwirePolicy::default();
        // Enable all rules with enforcement (not report-only).
        policy.migration.enabled = true;
        policy.migration.report_only = false;
        policy.dependency_identity.enabled = true;
        policy.dependency_identity.report_only = false;
        policy.network_egress.enabled = true;
        policy.network_egress.report_only = false;
        policy.unsafe_code.enabled = true;
        policy.unsafe_code.report_only = false;
        policy.boundary_path.enabled = true;
        policy.boundary_path.report_only = false;
        policy.large_delete_rewrite.enabled = true;
        policy.large_delete_rewrite.report_only = false;
        policy.ci_workflow.enabled = true;
        policy.ci_workflow.report_only = false;

        let files = vec![
            // Migration
            ChangedFile {
                path: "migrations/001.sql".to_owned(),
                old_path: None,
                status: ChangedFileStatus::Added,
                additions: 10,
                deletions: 0,
                hunks: Vec::new(),
                is_generated: false,
                is_vendor: false,
            },
            // Dependency identity
            ChangedFile {
                path: "Cargo.toml".to_owned(),
                old_path: None,
                status: ChangedFileStatus::Modified,
                additions: 2,
                deletions: 1,
                hunks: Vec::new(),
                is_generated: false,
                is_vendor: false,
            },
            // Network egress (needs hunks)
            ChangedFile {
                path: "src/webhook.rs".to_owned(),
                old_path: None,
                status: ChangedFileStatus::Modified,
                additions: 5,
                deletions: 0,
                hunks: vec![DiffHunk {
                    new_start: 1,
                    new_lines: 3,
                    old_start: 1,
                    old_lines: 0,
                    diff_lines: vec![
                        "+// new".to_owned(),
                        "+use reqwest::Client;".to_owned(),
                        "+// done".to_owned(),
                    ],
                }],
                is_generated: false,
                is_vendor: false,
            },
            // Unsafe code (needs hunks with .rs extension)
            ChangedFile {
                path: "src/ffi.rs".to_owned(),
                old_path: None,
                status: ChangedFileStatus::Modified,
                additions: 3,
                deletions: 0,
                hunks: vec![DiffHunk {
                    new_start: 1,
                    new_lines: 2,
                    old_start: 1,
                    old_lines: 0,
                    diff_lines: vec!["+unsafe {".to_owned(), "+    do_something();".to_owned()],
                }],
                is_generated: false,
                is_vendor: false,
            },
            // Boundary path (added status + auth path)
            ChangedFile {
                path: "src/auth/mod.rs".to_owned(),
                old_path: None,
                status: ChangedFileStatus::Added,
                additions: 50,
                deletions: 0,
                hunks: Vec::new(),
                is_generated: false,
                is_vendor: false,
            },
            // Large delete
            ChangedFile {
                path: "src/legacy.rs".to_owned(),
                old_path: None,
                status: ChangedFileStatus::Modified,
                additions: 5,
                deletions: 600,
                hunks: Vec::new(),
                is_generated: false,
                is_vendor: false,
            },
            // CI workflow
            ChangedFile {
                path: ".github/workflows/ci.yml".to_owned(),
                old_path: None,
                status: ChangedFileStatus::Modified,
                additions: 10,
                deletions: 5,
                hunks: Vec::new(),
                is_generated: false,
                is_vendor: false,
            },
        ];

        let input = TripwireEvaluationInput {
            task_id: "task-all-rules".to_owned(),
            project_id: "proj-001".to_owned(),
            pr_number: Some(100),
            head_sha: "all-rules-sha".to_owned(),
            policy,
            allowlist_revision: None,
            changed_files: files,
        };
        let result = run_gate(&input);
        let decision = &result.decision;

        assert_eq!(decision.outcome, GateOutcome::Held);

        let rule_ids: Vec<&str> = decision
            .findings
            .iter()
            .map(|f| f.rule_id.as_str())
            .collect();

        // Verify each of the seven rule families produced at least one finding.
        assert!(
            rule_ids.contains(&"migration_change"),
            "migration_change must surface"
        );
        assert!(
            rule_ids.contains(&"dependency_identity_change"),
            "dependency_identity_change must surface"
        );
        assert!(
            rule_ids.contains(&"network_egress_change"),
            "network_egress_change must surface"
        );
        assert!(
            rule_ids.contains(&"unsafe_code_change"),
            "unsafe_code_change must surface"
        );
        assert!(
            rule_ids.contains(&"boundary_path_change"),
            "boundary_path_change must surface"
        );
        assert!(
            rule_ids.contains(&"large_delete_or_rewrite"),
            "large_delete_or_rewrite must surface"
        );
        assert!(
            rule_ids.contains(&"ci_workflow_change"),
            "ci_workflow_change must surface"
        );
    }
}
