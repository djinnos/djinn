//! Pure deterministic tripwire engine primitives.
//!
//! This module is the evaluation core of the tripwire gate path: it accepts
//! in-memory changed-file / diff-hunk inputs plus task / project / PR / head
//! identifiers and a [`TripwirePolicy`], and returns a deterministic
//! [`TripwireGateDecision`] containing findings, evidence spans, policy
//! revision, allowlist revision, and idempotency keys.
//!
//! The engine is **pure** — no DB, GitHub, LLM, or network calls — so it
//! can be unit-tested with fixtures and consumed by:
//!
//! - the rule fixtures (sibling tasks `imb4` and `na0w`) which produce
//!   findings for each rule family;
//! - the enforcement epic (`nptj`) which wires the gate into PR polling;
//! - the audit sampler (`zuir`) which replays gate decisions.
//!
//! # Idempotency keys
//!
//! Finding-level idempotency keys are SHA-256 digests over the tuple
//! `(task_id, head_sha, rule_id, evidence_path, evidence_start_line,
//! evidence_end_line, policy_revision)`. Gate-decision idempotency keys
//! are SHA-256 over `(task_id, head_sha, policy_revision,
//! allowlist_revision, sorted_finding_keys)`. Both are deterministic for
//! the same logical inputs.

#![allow(dead_code)]

use sha2::{Digest, Sha256};

use crate::tripwires::activity_payloads::{
    TripwireEvidenceSpan, TripwireFindingSummary, TripwireSeverity,
};
use crate::tripwires::policy::TripwirePolicy;
use crate::tripwires::reason_codes::{TripwireRuleId, reason_code_for_rule};

// ─── Changed-file / diff-hunk input structs ────────────────────────────────

/// Status of a changed file in the diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangedFileStatus {
    /// File was added.
    Added,
    /// File was modified.
    Modified,
    /// File was deleted.
    Deleted,
    /// File was renamed (old path → new path).
    Renamed,
}

/// An in-memory diff hunk inside a changed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    /// First new-side line of the hunk (1-based).
    pub new_start: u32,
    /// Number of new-side lines in the hunk.
    pub new_lines: u32,
    /// First old-side line of the hunk (1-based).
    pub old_start: u32,
    /// Number of old-side lines in the hunk.
    pub old_lines: u32,
    /// Raw diff lines (including `+`, `-`, ` ` prefixes) when available.
    pub diff_lines: Vec<String>,
}

/// In-memory representation of a changed file in the PR diff.
///
/// Carries everything the engine needs to classify and evaluate: path,
/// status, line counts, optional content / diff lines, and
/// generated/vendor classification inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    /// Repository-relative file path (new path for renames).
    pub path: String,
    /// Old path when the file was renamed.
    pub old_path: Option<String>,
    /// Change status.
    pub status: ChangedFileStatus,
    /// Lines added (new-side).
    pub additions: u32,
    /// Lines deleted (old-side).
    pub deletions: u32,
    /// Diff hunks, when available.
    pub hunks: Vec<DiffHunk>,
    /// Whether the file is classified as generated code by the caller.
    /// The engine uses this flag to apply [`TripwirePolicy::generated_exclusions`].
    pub is_generated: bool,
    /// Whether the file is classified as vendored third-party code by the
    /// caller. The engine uses this flag to apply
    /// [`TripwirePolicy::vendor_exclusions`].
    pub is_vendor: bool,
}

impl ChangedFile {
    /// Total lines changed (additions + deletions).
    pub fn total_lines_changed(&self) -> u32 {
        self.additions + self.deletions
    }

    /// Whether this file should be skipped due to generated/vendor
    /// classification.
    pub fn is_excluded(&self) -> bool {
        self.is_generated || self.is_vendor
    }
}

// ─── Evaluation input ──────────────────────────────────────────────────────

/// Input to the tripwire engine evaluation.
///
/// Contains all identity / context fields plus the policy and the
/// ordered list of changed files. The engine returns a
/// [`TripwireGateDecision`] that is deterministic for the same input.
#[derive(Debug, Clone)]
pub struct TripwireEvaluationInput {
    /// Task id this PR is associated with.
    pub task_id: String,
    /// Project id (multi-tenant key).
    pub project_id: String,
    /// PR number, when the evaluation is tied to a pull request.
    pub pr_number: Option<u64>,
    /// Head SHA the engine evaluates against.
    pub head_sha: String,
    /// Org policy snapshot to evaluate.
    pub policy: TripwirePolicy,
    /// Boundary allowlist revision to propagate. This is a convenience
    /// shortcut for `policy.allowlist.revision` but the caller may want to
    /// set it separately if the allowlist was read from a different source.
    /// When `None` the engine uses `policy.allowlist.revision`.
    pub allowlist_revision: Option<String>,
    /// Changed files in the PR diff, pre-sorted by the caller in a
    /// stable order (typically by path).
    pub changed_files: Vec<ChangedFile>,
}

// ─── Gate decision output ──────────────────────────────────────────────────

/// Gate outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GateOutcome {
    /// No enforcement-on findings. Gate passes.
    Passed,
    /// At least one enforcement-on finding. Gate is held (human review required).
    Held,
    /// Findings exist but every one is report-only. Gate passes with advisory
    /// findings.
    ReportOnly,
}

/// Deterministic gate decision produced by the engine.
///
/// Contains the outcome, sorted findings, policy/allowlist revisions,
/// and a stable gate-level idempotency key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TripwireGateDecision {
    /// Gate outcome.
    pub outcome: GateOutcome,
    /// Findings emitted by the engine, sorted deterministically by
    /// `(rule_id, evidence_path, evidence_start_line, evidence_end_line,
    /// severity)`.
    pub findings: Vec<TripwireFinding>,
    /// Policy revision propagated from the input.
    pub policy_revision: String,
    /// Allowlist revision propagated from the input. `None` when no
    /// boundary rule tripped.
    pub allowlist_revision: Option<String>,
    /// Number of enforcement-on findings.
    pub enforcement_finding_count: u32,
    /// Number of report-only findings.
    pub report_only_finding_count: u32,
    /// Stable idempotency key for this gate decision.
    pub idempotency_key: String,
}

// ─── Finding ───────────────────────────────────────────────────────────────

/// A single tripwire finding with rule id, reason code, severity, evidence,
/// policy/allowlist revisions, and a deterministic idempotency key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TripwireFinding {
    /// Stable rule-family identifier.
    pub rule_id: TripwireRuleId,
    /// Stable, namespaced reason code.
    pub reason_code: &'static str,
    /// Enforcement or advisory severity.
    pub severity: TripwireFindingSeverity,
    /// Evidence span.
    pub evidence: EvidenceSpan,
    /// Policy revision at evaluation time.
    pub policy_revision: String,
    /// Allowlist revision at evaluation time, when applicable.
    pub allowlist_revision: Option<String>,
    /// Deterministic idempotency key for this finding.
    pub idempotency_key: String,
}

impl TripwireFinding {
    /// Convert to the compact [`TripwireFindingSummary`] used in activity
    /// payloads.
    pub fn to_summary(&self) -> TripwireFindingSummary {
        TripwireFindingSummary {
            rule_id: self.rule_id.as_str().to_owned(),
            reason_code: self.reason_code.to_owned(),
            severity: match self.severity {
                TripwireFindingSeverity::EnforceHold => TripwireSeverity::HumanReviewRequired,
                TripwireFindingSeverity::ReportOnly => TripwireSeverity::ReportOnly,
            },
            evidence: TripwireEvidenceSpan {
                path: self.evidence.path.clone(),
                start_line: self.evidence.start_line,
                end_line: self.evidence.end_line,
                evidence_redacted: false,
            },
            idempotency_key: self.idempotency_key.clone(),
        }
    }
}

/// Severity / action attached to a finding.
///
/// Maps directly to the activity-payload [`TripwireSeverity`] but lives
/// here as a simpler enum so the engine module does not depend on the
/// payload wire literals for its core logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TripwireFindingSeverity {
    /// Enforcement-on: gate held, human-review required.
    EnforceHold,
    /// Advisory-only: finding logged but gate passes.
    ReportOnly,
}

/// Evidence span inside a finding.
///
/// Supports both line-precise spans (`start_line` / `end_line` set) and
/// file-level spans (both `None`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceSpan {
    /// Repository-relative file path of the evidence.
    pub path: String,
    /// First line of the span, when known.
    pub start_line: Option<u32>,
    /// Last line of the span (inclusive), when known.
    pub end_line: Option<u32>,
}

impl EvidenceSpan {
    /// Construct a line-precise span.
    pub fn lines(path: impl Into<String>, start: u32, end: u32) -> Self {
        Self {
            path: path.into(),
            start_line: Some(start),
            end_line: Some(end),
        }
    }

    /// Construct a file-level span (no line numbers).
    pub fn file(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            start_line: None,
            end_line: None,
        }
    }
}

// ─── Idempotency key construction ─────────────────────────────────────────

/// Build a deterministic idempotency key for a single finding.
///
/// The key is a SHA-256 hex digest of the canonical representation:
///
/// ```text
/// {task_id}:{head_sha}:{rule_id}:{evidence_path}:{start_line}:{end_line}:{policy_revision}
/// ```
///
/// Where `start_line` and `end_line` are empty strings when the evidence
/// is file-level.
pub fn build_finding_idempotency_key(
    task_id: &str,
    head_sha: &str,
    rule_id: TripwireRuleId,
    evidence_path: &str,
    evidence_start_line: Option<u32>,
    evidence_end_line: Option<u32>,
    policy_revision: &str,
) -> String {
    let start = evidence_start_line
        .map(|n| n.to_string())
        .unwrap_or_default();
    let end = evidence_end_line.map(|n| n.to_string()).unwrap_or_default();
    let payload = format!(
        "{}:{}:{}:{}:{}:{}:{}",
        task_id,
        head_sha,
        rule_id.as_str(),
        evidence_path,
        start,
        end,
        policy_revision,
    );
    let hash = Sha256::digest(payload.as_bytes());
    format!("sha256:{}", hex::encode(hash))
}

/// Build a deterministic idempotency key for a gate decision.
///
/// The key is a SHA-256 hex digest of:
///
/// ```text
/// {task_id}:{head_sha}:{policy_revision}:{allowlist_revision}:{sorted_finding_keys_joined_by_comma}
/// ```
///
/// Finding keys are sorted lexicographically before joining so the
/// gate key is independent of finding insertion order.
pub fn build_gate_idempotency_key(
    task_id: &str,
    head_sha: &str,
    policy_revision: &str,
    allowlist_revision: Option<&str>,
    sorted_finding_keys: &[String],
) -> String {
    let allowlist = allowlist_revision.unwrap_or("");
    let joined_keys = sorted_finding_keys.join(",");
    let payload = format!(
        "{}:{}:{}:{}:{}",
        task_id, head_sha, policy_revision, allowlist, joined_keys,
    );
    let hash = Sha256::digest(payload.as_bytes());
    format!("sha256:{}", hex::encode(hash))
}

// ─── Engine evaluation ─────────────────────────────────────────────────────

/// Evaluate the tripwire policy against the changed files and produce a
/// deterministic [`TripwireGateDecision`].
///
/// The engine is pure: it only reads from the input and the policy; it
/// does not perform I/O. Rule evaluation is left to sibling tasks (`imb4`,
/// `na0w`) which will call [`collect_findings`] with their per-rule
/// functions. This function provides the orchestration skeleton:
///
/// 1. Run each enabled rule function against the changed files.
/// 2. Apply generated/vendor exclusions when a file is flagged.
/// 3. Classify each finding as enforcement or report-only based on the
///    rule's `report_only` flag.
/// 4. Sort findings deterministically.
/// 5. Compute the gate decision and idempotency keys.
///
/// The `rule_evaluators` parameter is a slice of rule evaluator functions.
/// Each takes the evaluation input and returns zero or more raw
/// [`RawFinding`]s. The engine then wraps them with severity, revisions,
/// and idempotency keys.
pub fn evaluate<E>(input: &TripwireEvaluationInput, rule_evaluators: &[E]) -> TripwireGateDecision
where
    E: Fn(&TripwirePolicy, &[ChangedFile]) -> Vec<RawFinding>,
{
    let allowlist_rev = input
        .allowlist_revision
        .as_deref()
        .unwrap_or(&input.policy.allowlist.revision);

    // ── Collect raw findings from all enabled rules ──────────────────────
    let mut findings: Vec<TripwireFinding> = Vec::new();

    for evaluator in rule_evaluators {
        let raw_findings = evaluator(&input.policy, &input.changed_files);
        for raw in raw_findings {
            // Skip findings from excluded (generated/vendor) files.
            if raw.evidence_is_excluded {
                continue;
            }

            let (rule_id, report_only) = (raw.rule_id, raw.report_only);

            let severity = if report_only {
                TripwireFindingSeverity::ReportOnly
            } else {
                TripwireFindingSeverity::EnforceHold
            };

            let idempotency_key = build_finding_idempotency_key(
                &input.task_id,
                &input.head_sha,
                rule_id,
                &raw.evidence_path,
                raw.evidence_start_line,
                raw.evidence_end_line,
                &input.policy.policy_revision,
            );

            findings.push(TripwireFinding {
                rule_id,
                reason_code: reason_code_for_rule(rule_id),
                severity,
                evidence: EvidenceSpan {
                    path: raw.evidence_path,
                    start_line: raw.evidence_start_line,
                    end_line: raw.evidence_end_line,
                },
                policy_revision: input.policy.policy_revision.clone(),
                allowlist_revision: if matches!(rule_id, TripwireRuleId::BoundaryPathChange) {
                    Some(allowlist_rev.to_owned())
                } else {
                    None
                },
                idempotency_key,
            });
        }
    }

    // ── Sort findings deterministically ──────────────────────────────────
    sort_findings(&mut findings);

    // ── Classify outcome ─────────────────────────────────────────────────
    let enforcement_count = findings
        .iter()
        .filter(|f| f.severity == TripwireFindingSeverity::EnforceHold)
        .count() as u32;
    let report_only_count = findings
        .iter()
        .filter(|f| f.severity == TripwireFindingSeverity::ReportOnly)
        .count() as u32;

    let outcome = if enforcement_count > 0 {
        GateOutcome::Held
    } else if report_only_count > 0 {
        GateOutcome::ReportOnly
    } else {
        GateOutcome::Passed
    };

    // ── Compute gate idempotency key ─────────────────────────────────────
    let mut finding_keys: Vec<String> =
        findings.iter().map(|f| f.idempotency_key.clone()).collect();
    finding_keys.sort();

    let needs_allowlist = findings
        .iter()
        .any(|f| f.rule_id == TripwireRuleId::BoundaryPathChange);

    let gate_idempotency_key = build_gate_idempotency_key(
        &input.task_id,
        &input.head_sha,
        &input.policy.policy_revision,
        if needs_allowlist {
            Some(allowlist_rev)
        } else {
            None
        },
        &finding_keys,
    );

    TripwireGateDecision {
        outcome,
        findings,
        policy_revision: input.policy.policy_revision.clone(),
        allowlist_revision: if needs_allowlist {
            Some(allowlist_rev.to_owned())
        } else {
            None
        },
        enforcement_finding_count: enforcement_count,
        report_only_finding_count: report_only_count,
        idempotency_key: gate_idempotency_key,
    }
}

// ─── Finding sort ──────────────────────────────────────────────────────────

/// Sort findings deterministically by `(rule_id, evidence_path,
/// evidence_start_line, evidence_end_line, severity)`.
///
/// This ordering is stable across evaluations with the same inputs and
/// is used both for the findings vector and for the gate idempotency key
/// construction.
pub fn sort_findings(findings: &mut [TripwireFinding]) {
    findings.sort_by(|a, b| {
        a.rule_id
            .as_str()
            .cmp(b.rule_id.as_str())
            .then_with(|| a.evidence.path.cmp(&b.evidence.path))
            .then_with(|| a.evidence.start_line.cmp(&b.evidence.start_line))
            .then_with(|| a.evidence.end_line.cmp(&b.evidence.end_line))
            .then_with(|| a.severity.cmp(&b.severity))
    });
}

// ─── Raw finding (intermediate, produced by rule evaluators) ───────────────

/// Intermediate finding produced by a rule evaluator before the engine
/// wraps it with severity, revisions, and idempotency keys.
///
/// Rule evaluators (added by sibling tasks `imb4` and `na0w`) return
/// `Vec<RawFinding>`. The engine then filters excluded files, applies
/// report_only classification, and builds the final [`TripwireFinding`].
#[derive(Debug, Clone)]
pub struct RawFinding {
    /// Rule family that produced this finding.
    pub rule_id: TripwireRuleId,
    /// Whether the rule's `report_only` flag was set at evaluation time.
    pub report_only: bool,
    /// Evidence file path.
    pub evidence_path: String,
    /// Evidence start line, when known.
    pub evidence_start_line: Option<u32>,
    /// Evidence end line, when known.
    pub evidence_end_line: Option<u32>,
    /// Whether the evidence file was classified as generated or vendor.
    /// The engine skips findings with this flag set.
    pub evidence_is_excluded: bool,
}

/// A rule evaluator function: takes the policy and changed files, returns
/// zero or more [`RawFinding`]s.
///
/// This is a trait alias used for documentation. In function signatures,
/// prefer the generic bound `E: Fn(&TripwirePolicy, &[ChangedFile]) ->
/// Vec<RawFinding>` so callers can pass function pointers, closures, or
/// `Box<dyn Fn(...)>` without additional allocation.
pub type RuleEvaluator = dyn Fn(&TripwirePolicy, &[ChangedFile]) -> Vec<RawFinding> + Send + Sync;

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
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
        move |_policy: &TripwirePolicy, _files: &[ChangedFile]| -> Vec<RawFinding> {
            findings.clone()
        }
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
        };

        let summary = finding.to_summary();
        assert_eq!(summary.rule_id, "migration_change");
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
}
