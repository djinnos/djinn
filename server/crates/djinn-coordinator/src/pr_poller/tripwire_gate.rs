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
    ChangedFile, ChangedFileStatus, DiffHunk, GateOutcome, TRIPWIRE_EVENT_GATE_HELD,
    TRIPWIRE_EVENT_GATE_PASSED, TRIPWIRE_EVENT_GATE_REPORT_ONLY, TripwireEvaluationInput,
    TripwireFindingSummary, TripwireGateDecision, TripwireGateDecisionPayload, TripwirePolicy,
    all_rule_evaluators, evaluate,
};

// ─── PrFile → ChangedFile conversion ─────────────────────────────────────

/// Parse a unified-diff `patch` string into [`DiffHunk`]s.
///
/// A hunk header has the form `@@ -old_start,old_lines +new_start,new_lines @@`.
/// Lines following the header start with ` ` (context), `+` (added), or
/// `-` (removed). Binary-file patches (patch starts with `Binary files`)
/// produce no hunks.
fn parse_patch_to_hunks(patch: &str) -> Vec<DiffHunk> {
    let mut hunks = Vec::new();
    let mut current: Option<DiffHunk> = None;

    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("@@") {
            // Flush any in-progress hunk.
            if let Some(h) = current.take() {
                hunks.push(h);
            }

            // Parse `@@ -old_start,old_lines +new_start,new_lines @@ optional`
            // The part before the second `@@` is `-o,l +n,l`.
            let header_body = rest.split("@@").next().unwrap_or("").trim();

            let (old_part, new_part) = split_hunk_header(header_body);
            let (old_start, old_lines) = parse_range(&old_part);
            let (new_start, new_lines) = parse_range(&new_part);

            current = Some(DiffHunk {
                new_start,
                new_lines,
                old_start,
                old_lines,
                diff_lines: Vec::new(),
            });
        } else if let Some(h) = current.as_mut() {
            // Diff line: context (' '), added ('+'), removed ('-').
            h.diff_lines.push(line.to_owned());
        }
        // Lines before the first @@ header are ignored (file-level metadata).
    }

    if let Some(h) = current.take() {
        hunks.push(h);
    }

    hunks
}

/// Split the hunk header body `" -10,5 +12,7 "` into old (`-10,5`) and
/// new (`+12,7`) parts.
fn split_hunk_header(body: &str) -> (String, String) {
    let trimmed = body.trim();
    // old part starts with '-'; new part starts with '+'.
    let old_part = trimmed
        .find('-')
        .map(|i| {
            trimmed[i..]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_owned()
        })
        .unwrap_or_default();
    let new_part = trimmed
        .find('+')
        .map(|i| {
            trimmed[i..]
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_owned()
        })
        .unwrap_or_default();
    (old_part, new_part)
}

/// Parse a range like `-10,5` or `+12` into `(start, lines)`.
/// Returns `(0, 0)` when the string is empty or malformed.
fn parse_range(s: &str) -> (u32, u32) {
    let s = s.trim_start_matches(['-', '+']);
    if s.is_empty() {
        return (0, 0);
    }
    if let Some((start_str, count_str)) = s.split_once(',') {
        let start: u32 = start_str.parse().unwrap_or(0);
        let count: u32 = count_str.parse().unwrap_or(0);
        (start, count)
    } else {
        let start: u32 = s.parse().unwrap_or(0);
        (start, 1) // single-line hunk when no comma
    }
}

/// Convert a slice of GitHub API [`PrFile`]s to the engine's [`ChangedFile`]
/// representation.
///
/// The GitHub PR-files endpoint (`GET /pulls/{n}/files`) returns a flat
/// list with `status`, `additions`, `deletions`, `filename`, and — for
/// text files — a `patch` string containing the unified diff. When the
/// `patch` is present it is parsed into [`DiffHunk`]s so that line-scanning
/// rules (network egress, unsafe code) can evaluate added diff lines.
/// Files with `patch = None` (binary files, or responses that omit the
/// patch) get empty hunks and are evaluated at the file level only.
///
/// Files with unrecognised `status` strings are mapped to `Modified` as
/// a conservative default (GitHub may add new statuses).
pub fn convert_pr_files(pr_files: &[PrFile]) -> Vec<ChangedFile> {
    pr_files
        .iter()
        .map(|pf| {
            let hunks = pf
                .patch
                .as_deref()
                .filter(|p| !p.is_empty())
                .map(parse_patch_to_hunks)
                .unwrap_or_default();

            ChangedFile {
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
                hunks,
                is_generated: false,
                is_vendor: false,
            }
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
        ChangedFileStatus, TRIPWIRE_EVENT_GATE_HELD, TRIPWIRE_EVENT_GATE_PASSED,
        TRIPWIRE_EVENT_GATE_REPORT_ONLY, TripwireFindingSeverity,
    };

    /// Helper: build a `PrFile` with the given fields and no patch.
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

    /// Helper: build a `PrFile` with a patch string (unified diff).
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
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].hunks.len(), 1, "one hunk from patch");
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
        assert_eq!(converted[0].hunks.len(), 2, "two hunks from patch");
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

    // ── End-to-end evaluation tests via PrFile conversion ───────────────
    //
    // These tests start from representative `PrFile` payloads (as returned
    // by GitHub's `GET /pulls/{n}/files` endpoint), convert them through
    // `convert_pr_files`, and evaluate with `run_gate`. This proves the
    // full pipeline including patch → DiffHunk conversion works for all
    // seven rule families.

    /// Helper: convert PrFiles to ChangedFiles, then evaluate with default policy.
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

    /// Helper: convert PrFiles to ChangedFiles, then evaluate with a custom policy.
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

    /// Helper: select the event type from a gate decision.
    fn event_type_for(decision: &TripwireGateDecision) -> &'static str {
        match decision.outcome {
            GateOutcome::Held => TRIPWIRE_EVENT_GATE_HELD,
            GateOutcome::Passed => TRIPWIRE_EVENT_GATE_PASSED,
            GateOutcome::ReportOnly => TRIPWIRE_EVENT_GATE_REPORT_ONLY,
        }
    }

    // ── Rule 1: migration_change (file-level, no hunks needed) ──────────

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

    // ── Rule 2: dependency_identity_change (file-level, no hunks) ───────

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

    // ── Rule 3: network_egress_change (requires patch → hunks) ──────────

    #[test]
    fn network_egress_change_from_pr_file_produces_held_gate() {
        let patch =
            "@@ -1,1 +1,3 @@\n old line\n+Webhook::register(endpoint);\n+notify(payload);\n";
        let files = vec![pr_file_with_patch("src/http.rs", "modified", 2, 0, patch)];
        let decision = evaluate_from_pr_files(files);
        assert_eq!(decision.outcome, GateOutcome::Held);
        assert!(
            decision
                .findings
                .iter()
                .any(|f| f.rule_id.as_str() == "network_egress_change"),
            "network_egress_change must surface from PR-file patch conversion"
        );
        // Verify evidence is line-precise.
        let egress_finding = decision
            .findings
            .iter()
            .find(|f| f.rule_id.as_str() == "network_egress_change")
            .unwrap();
        assert!(
            egress_finding.evidence.start_line.is_some(),
            "evidence must have a line number"
        );
    }

    // ── Rule 4: unsafe_code_change (requires patch → hunks) ─────────────

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
                .any(|f| f.rule_id.as_str() == "unsafe_code_change"),
            "unsafe_code_change must surface from PR-file patch conversion"
        );
    }

    // ── Rule 5: boundary_path_change (file-level, no hunks needed) ──────

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
        // Boundary findings carry an allowlist revision.
        let boundary_finding = decision
            .findings
            .iter()
            .find(|f| f.rule_id.as_str() == "boundary_path_change")
            .unwrap();
        assert!(boundary_finding.allowlist_revision.is_some());
    }

    // ── Rule 6: large_delete_or_rewrite (file-level, no hunks) ──────────

    #[test]
    fn large_delete_or_rewrite_from_pr_file_produces_held_gate() {
        let files = vec![pr_file(
            "src/old_module.rs",
            "modified",
            10,
            600, // Exceeds default per-file threshold of 500
        )];
        let decision = evaluate_from_pr_files(files);
        assert_eq!(decision.outcome, GateOutcome::Held);
        assert!(
            decision
                .findings
                .iter()
                .any(|f| f.rule_id.as_str() == "large_delete_or_rewrite")
        );
    }

    // ── Rule 7: ci_workflow_change (file-level, no hunks) ───────────────

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

    // ── All seven rule families from PrFile payloads ────────────────────

    #[test]
    fn all_seven_rule_families_from_pr_files_produce_findings() {
        let files = vec![
            // 1. Migration (file-level)
            pr_file("migrations/001.sql", "added", 10, 0),
            // 2. Dependency identity (file-level)
            pr_file("Cargo.toml", "modified", 2, 1),
            // 3. Network egress (needs patch → hunks)
            pr_file_with_patch(
                "src/webhook.rs",
                "modified",
                2,
                0,
                "@@ -1,0 +1,2 @@\n+Webhook::register(endpoint);\n+notify(payload);\n",
            ),
            // 4. Unsafe code (needs patch → hunks, .rs extension)
            pr_file_with_patch(
                "src/ffi.rs",
                "modified",
                2,
                0,
                "@@ -1,0 +1,2 @@\n+unsafe {\n+    ptr::read_volatile(addr);\n",
            ),
            // 5. Boundary path (added status + auth path)
            pr_file("src/auth/mod.rs", "added", 50, 0),
            // 6. Large delete
            pr_file("src/legacy.rs", "modified", 5, 600),
            // 7. CI workflow
            pr_file(".github/workflows/ci.yml", "modified", 10, 5),
        ];

        let decision = evaluate_from_pr_files(files);

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

    // ── Report-only scenario from PrFile ────────────────────────────────

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

    // ── Report-only for network_egress from PrFile (patch-based) ────────

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
                .any(|f| f.rule_id.as_str() == "network_egress_change"),
            "network_egress_change must surface from patch as report-only"
        );
    }

    // ── Passed (no findings) from PrFile ────────────────────────────────

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

    // ── Idempotency key determinism from PrFile ─────────────────────────

    #[test]
    fn gate_idempotency_key_is_deterministic_from_pr_files() {
        let files = vec![pr_file("migrations/001.sql", "added", 10, 0)];
        let d1 = evaluate_from_pr_files(files.clone());
        let d2 = evaluate_from_pr_files(files);
        assert_eq!(d1.idempotency_key, d2.idempotency_key);
    }

    // ── Payload validation from PrFile ──────────────────────────────────

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
            .expect("payload must pass validation for a consistent decision");
    }

    // ── Mixed findings: enforcement dominates (from PrFile) ─────────────

    #[test]
    fn mixed_findings_enforcement_dominates_over_report_only_from_pr_files() {
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

    // ── Patch absent: network_egress/unsafe cannot surface ─────────────

    #[test]
    fn pr_file_without_patch_does_not_trigger_egress_or_unsafe() {
        // A .rs file with additions but no patch/hunks.
        // network_egress and unsafe_code scan diff lines only,
        // so without a patch they cannot match.
        let files = vec![pr_file("src/webhook.rs", "modified", 5, 0)];
        let decision = evaluate_from_pr_files(files);
        // No other rule matches this file, so outcome is Passed.
        assert_eq!(decision.outcome, GateOutcome::Passed);
        assert!(decision.findings.is_empty());
    }

    // ── Generated/vendor files are excluded ─────────────────────────────

    #[test]
    fn generated_files_are_excluded_from_evaluation() {
        let changed_files = vec![ChangedFile {
            path: "generated/bindings.rs".to_owned(),
            old_path: None,
            status: ChangedFileStatus::Added,
            additions: 5000,
            deletions: 0,
            hunks: Vec::new(),
            is_generated: true, // This file is classified as generated
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
        let decision = &result.decision;
        assert_eq!(decision.outcome, GateOutcome::Passed);
        assert!(decision.findings.is_empty());
    }
}
