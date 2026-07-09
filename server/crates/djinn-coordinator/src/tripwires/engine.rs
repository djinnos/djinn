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

use djinn_core::test_paths::is_test_path;
use sha2::{Digest, Sha256};

use crate::tripwires::activity_payloads::{
    TripwireEvidenceSpan, TripwireFindingSummary, TripwireSeverity,
};
use crate::tripwires::policy::TripwirePolicy;
use crate::tripwires::reason_codes::{TripwireRuleId, reason_code_for_rule};

/// Human-readable annotation stamped on a finding whose evidence path is a
/// test file and was therefore downgraded from enforcement to report-only.
pub const TEST_PATH_DOWNGRADE_REASON: &str =
    "test-path: evidence path is a test file; downgraded to report-only";

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
    /// Head-independent content fingerprint. Stable across PR heads while the
    /// underlying flagged content (rule + file + patch hunk) is unchanged;
    /// unlike [`idempotency_key`](TripwireFinding::idempotency_key) it does
    /// NOT include `head_sha`. The gate's release carry-forward keys on this
    /// to recognise a finding already adjudicated on a prior head.
    pub content_fingerprint: String,
    /// When set, a human-readable reason this finding was downgraded from
    /// enforcement to report-only (e.g. the evidence path is a test file).
    pub downgrade_reason: Option<String>,
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
            content_fingerprint: self.content_fingerprint.clone(),
            downgrade_reason: self.downgrade_reason.clone(),
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

/// Build a head-independent content fingerprint for a finding.
///
/// The fingerprint is a SHA-256 hex digest over
/// `(rule_id, evidence_path, additions, deletions, added_line_content)` where
/// `added_line_content` is the concatenation of every added (`+`-prefixed)
/// diff line of the matching changed file, with the `+` stripped. It
/// deliberately excludes `head_sha`, `base_sha`, and line numbers so that the
/// same flagged change carried across a rebase / new push — identical patch
/// content for the file — yields the same fingerprint. When no matching
/// changed file is available (or it has no hunks) the fingerprint still
/// covers the rule + path + coarse add/delete counts, which is stable for an
/// unchanged file-level finding.
pub fn build_finding_content_fingerprint(
    rule_id: TripwireRuleId,
    evidence_path: &str,
    changed_file: Option<&ChangedFile>,
) -> String {
    let (additions, deletions, added_content) = match changed_file {
        Some(file) => {
            let mut added = String::new();
            for hunk in &file.hunks {
                for line in &hunk.diff_lines {
                    if let Some(rest) = line.strip_prefix('+') {
                        added.push_str(rest);
                        added.push('\n');
                    }
                }
            }
            (file.additions, file.deletions, added)
        }
        None => (0, 0, String::new()),
    };
    let payload = format!(
        "{}:{}:{}:{}:{}",
        rule_id.as_str(),
        evidence_path,
        additions,
        deletions,
        added_content,
    );
    let hash = Sha256::digest(payload.as_bytes());
    format!("fp:sha256:{}", hex::encode(hash))
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

            // Test-path downgrade annotation: rule evaluators already flip
            // `report_only` for test-path evidence (see rules::mod), so a
            // test-path finding arrives here as report-only. Stamp the reason
            // so the activity payload explains why enforcement was skipped.
            let downgrade_reason = if is_test_path(&raw.evidence_path) {
                Some(TEST_PATH_DOWNGRADE_REASON.to_owned())
            } else {
                None
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

            let content_fingerprint = build_finding_content_fingerprint(
                rule_id,
                &raw.evidence_path,
                input
                    .changed_files
                    .iter()
                    .find(|f| f.path == raw.evidence_path),
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
                content_fingerprint,
                downgrade_reason,
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
#[path = "engine_tests.rs"]
mod tests;
