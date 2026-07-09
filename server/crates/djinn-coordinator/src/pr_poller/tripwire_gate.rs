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
//!
//! # Rollout / backfill semantics
//!
//! The tripwire enforcement rollout has two phases:
//!
//! 1. **Backfill (report-only):** When the tripwire system first evaluates an
//!    existing open PR head — i.e. the PR was created *before* policy
//!    publication and the task has no prior tripwire gate activity — all
//!    findings are evaluated in report-only mode regardless of per-rule
//!    `report_only` flags.  A `tripwire.gate.report_only` activity event is
//!    logged with full findings, evidence, revisions, and idempotency keys.
//!    No `human-review-hold` label is applied and the PR is not blocked by
//!    this evaluation alone.
//!
//! 2. **Enforcement (new heads / new PRs after publication):** When a PR gets
//!    a new head SHA (developer pushes after policy publication), *or* when a
//!    PR is first opened after policy publication, the gate evaluates under
//!    the live policy and may produce a `tripwire.gate.held` event and
//!    `human-review-hold` label.  This uses the same delivered
//!    engine/policy/active-hold contracts.
//!
//! The distinction is derived from the task's activity log **and** the
//! policy publication timestamp (`policy_publication_ts`):
//!
//! - No prior gate events **and** PR created before policy publication →
//!   **Backfill** (existing PR, first evaluation).
//! - No prior gate events **and** PR created after policy publication →
//!   **Enforce** (new PR subject to live enforcement).
//! - Prior gate events exist for the *same* head SHA **and** the same
//!   policy revision → **Idempotent** (duplicate poll; no new event
//!   emitted).
//! - Prior gate events exist for the *same* head SHA but a *different*
//!   policy revision → **Enforce** (policy changed; re-evaluate).
//! - Prior gate events exist for a *different* head SHA → **Enforce**
//!   (new push after policy was enabled).
//!
//! Direct removal of the `human-review-hold` label is tamper, not a
//! release path — the reconciliation tick re-applies it.

use anyhow::Result;
use djinn_provider::github_api::{GitHubApiClient, PrFile};

use crate::tripwires::{
    ActivityEntryRef, ChangedFile, ChangedFileStatus, DiffHunk, GateOutcome,
    TRIPWIRE_EVENT_GATE_HELD, TRIPWIRE_EVENT_GATE_PASSED, TRIPWIRE_EVENT_GATE_REPORT_ONLY,
    TripwireEvaluationInput, TripwireFindingSummary, TripwireGateDecision,
    TripwireGateDecisionPayload, TripwirePolicy, all_rule_evaluators, evaluate,
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
#[allow(dead_code)] // Superseded by `evaluate_tripwire_gate_with_rollout`; kept for direct callers.
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

// ─── Rollout / backfill mode ───────────────────────────────────────────────

/// Determines how the tripwire gate should evaluate a PR head.
///
/// During the enforcement rollout, existing open PRs are backfilled in
/// report-only mode so findings are logged without blocking the PR.  A new
/// head SHA (developer push after policy publication) switches to full
/// enforcement per the active policy.  See the module-level documentation
/// for the full operational boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutMode {
    /// First evaluation of this task — existing open PR head (created
    /// before policy publication) being backfilled.  All findings are forced
    /// to report-only; no
    /// `human-review-hold` label is applied.
    Backfill,
    /// Evaluate under the live policy (enforcement-on per rule config).
    /// Returned for: new head SHA after prior gate activity, new PR after
    /// policy publication, or same head SHA with a changed policy revision.
    Enforce,
    /// This exact `(task_id, head_sha, policy_revision)` was already
    /// evaluated.  The caller must skip the evaluation to avoid duplicate
    /// activity events.
    AlreadyEvaluated,
}

/// Determine the [`RolloutMode`] for a PR head by examining the task's
/// tripwire gate activity log.
///
/// The logic (in priority order):
///
/// 1. If a gate event already exists for this exact `head_sha` **and** the
///    same `policy_revision` → [`RolloutMode::AlreadyEvaluated`]
///    (idempotent skip).
/// 2. If a gate event exists for this `head_sha` but a **different**
///    `policy_revision` → [`RolloutMode::Enforce`] (policy changed).
/// 3. If any gate event exists for this task but for a different head SHA →
///    [`RolloutMode::Enforce`] (new push after policy publication).
/// 4. If no gate events exist and the PR was created **after** policy
///    publication → [`RolloutMode::Enforce`] (new PR under enforcement).
/// 5. If no gate events exist and the PR was created **before** policy
///    publication (or no `policy_publication_ts` provided) →
///    [`RolloutMode::Backfill`] (existing PR, first evaluation).
///
/// # Arguments
///
/// * `entries` — tripwire-related activity entries for the task.
/// * `head_sha` — the current PR head SHA being evaluated.
/// * `policy_publication_ts` — RFC 3339 timestamp of policy publication.
///   Used to distinguish existing open PRs (backfill) from new PRs after
///   publication (enforce).  When `None`, the absence of prior gate events
///   defaults to backfill.
/// * `current_policy_revision` — the policy revision being evaluated.
///   Used to ensure idempotency is scoped to `(head_sha, policy_revision)`
///   so that a policy change triggers re-evaluation for the same head.
pub fn determine_rollout_mode<'a, I>(
    entries: I,
    head_sha: &str,
    policy_publication_ts: Option<&str>,
    current_policy_revision: &str,
) -> RolloutMode
where
    I: IntoIterator<Item = &'a ActivityEntryRef>,
{
    let mut has_prior_event = false;

    for entry in entries {
        if !is_tripwire_gate_event(&entry.event_type) {
            continue;
        }

        // Try to extract head_sha from the payload.
        if let Ok(payload) = serde_json::from_str::<TripwireGateDecisionPayload>(&entry.payload) {
            if payload.head_sha == head_sha {
                if payload.policy_revision == current_policy_revision {
                    // Same head SHA + same policy revision — idempotent.
                    return RolloutMode::AlreadyEvaluated;
                }
                // Same head SHA but different policy revision — re-evaluate
                // under the new policy.
                return RolloutMode::Enforce;
            }
            has_prior_event = true;
        }
    }

    if has_prior_event {
        RolloutMode::Enforce
    } else {
        // No prior gate events — this is a first evaluation.  Distinguish
        // existing-open-PR backfill from new-PR-after-publication enforcement
        // using the policy publication timestamp.
        //
        // The caller compares PR creation time against policy publication
        // time and passes `Some(...)` only for PRs created after publication.
        // `None` here means the PR predates publication → backfill.
        match policy_publication_ts {
            Some(_) => RolloutMode::Enforce,
            None => RolloutMode::Backfill,
        }
    }
}

/// Returns `true` when the event type is one of the three tripwire gate
/// decision events (`tripwire.gate.held`, `tripwire.gate.passed`,
/// `tripwire.gate.report_only`).
pub fn is_tripwire_gate_event(event_type: &str) -> bool {
    matches!(
        event_type,
        TRIPWIRE_EVENT_GATE_HELD | TRIPWIRE_EVENT_GATE_PASSED | TRIPWIRE_EVENT_GATE_REPORT_ONLY
    )
}

/// Evaluate the tripwire gate with rollout-aware policy selection.
///
/// This is the primary entry point for the PR poller's tripwire
/// integration.  It determines the [`RolloutMode`] from the task's
/// activity log, applies report-only policy override for backfill
/// evaluations, and delegates to [`run_gate`].
///
/// Returns `Ok(None)` when the mode is [`RolloutMode::AlreadyEvaluated`]
/// (idempotent skip — no new event should be emitted).
///
/// # Arguments
///
/// * `gh_client` — authenticated GitHub API client for the installation.
/// * `owner`, `repo` — repository owner/name.
/// * `pull_number` — PR number.
/// * `task_id` — task UUID for idempotency key derivation.
/// * `project_id` — project UUID for the activity payload.
/// * `head_sha` — current head SHA of the PR.
/// * `entries` — tripwire-related activity entries for the task (used to
///   determine the rollout mode).
/// * `policy_publication_ts` — RFC 3339 timestamp of policy publication.
///   Pass `None` when the PR was created before policy publication (backfill
///   path).  Pass `Some(...)` for PRs created after publication.
/// * `current_policy_revision` — the policy revision being evaluated today.
///   Scoped into idempotency so policy changes trigger re-evaluation.
#[allow(clippy::too_many_arguments)]
pub async fn evaluate_tripwire_gate_with_rollout(
    gh_client: &GitHubApiClient,
    owner: &str,
    repo: &str,
    pull_number: u64,
    task_id: &str,
    project_id: &str,
    head_sha: &str,
    entries: &[ActivityEntryRef],
    policy_publication_ts: Option<&str>,
    current_policy_revision: &str,
) -> Result<Option<(TripwireGateResult, RolloutMode)>> {
    let mode = determine_rollout_mode(
        entries,
        head_sha,
        policy_publication_ts,
        current_policy_revision,
    );

    match mode {
        RolloutMode::AlreadyEvaluated => {
            tracing::debug!(
                task_id,
                head_sha,
                "Tripwire gate: head SHA already evaluated — skipping (idempotent)"
            );
            return Ok(None);
        }
        RolloutMode::Backfill => {
            tracing::info!(
                task_id,
                head_sha,
                "Tripwire gate: backfill mode — evaluating in report-only"
            );
        }
        RolloutMode::Enforce => {
            tracing::info!(
                task_id,
                head_sha,
                "Tripwire gate: enforce mode — evaluating under live policy"
            );
        }
    }

    // 1. Fetch changed files from GitHub.
    let pr_files = gh_client.get_pr_files(owner, repo, pull_number).await?;

    // 2. Convert to engine types.
    let changed_files = convert_pr_files(&pr_files);

    // 3. Build evaluation input — backfill uses report-only policy.
    let policy = match mode {
        RolloutMode::Backfill => TripwirePolicy::default().make_report_only(),
        _ => TripwirePolicy::default(),
    };

    let input = TripwireEvaluationInput {
        task_id: task_id.to_owned(),
        project_id: project_id.to_owned(),
        pr_number: Some(pull_number),
        head_sha: head_sha.to_owned(),
        policy,
        allowlist_revision: None,
        changed_files,
    };

    // 4. Evaluate with all seven rule families.
    Ok(Some((run_gate(&input), mode)))
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

    // ── Rollout mode: determine_rollout_mode ─────────────────────────────

    /// Helper: build an `ActivityEntryRef` with a gate event payload.
    fn gate_activity_entry(
        event_type: &str,
        head_sha: &str,
        policy_revision: &str,
        idempotency_key: &str,
        created_at: &str,
    ) -> ActivityEntryRef {
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

    // AC: No prior gate events + no policy_publication_ts → Backfill.
    #[test]
    fn determine_rollout_mode_no_prior_events_is_backfill() {
        let entries: Vec<ActivityEntryRef> = vec![];
        let mode = determine_rollout_mode(&entries, "sha-aaa", None, "default");
        assert_eq!(mode, RolloutMode::Backfill);
    }

    // AC: Prior events for same head SHA + same policy revision →
    // AlreadyEvaluated (idempotent).
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

    // AC: Prior events for different head SHA → Enforce (new push).
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

    // AC: Non-gate events are ignored; backfill when no gate events exist.
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

    // AC: Mixed events — gate event for different SHA dominates → Enforce.
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

    // AC: Multiple gate events for different SHAs, then same SHA + same
    // policy → AlreadyEvaluated (same SHA + same policy takes precedence).
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

    // AC: Same head SHA + different policy revision → Enforce (policy
    // changed; must re-evaluate).
    #[test]
    fn determine_rollout_mode_same_head_sha_new_policy_revision_is_enforce() {
        let entries = vec![gate_activity_entry(
            TRIPWIRE_EVENT_GATE_REPORT_ONLY,
            "sha-aaa",
            "org-policy:1",
            "key-v1",
            "2026-01-01T00:00:00Z",
        )];
        // Current policy revision is org-policy:2 — different from the
        // stored org-policy:1.
        let mode = determine_rollout_mode(&entries, "sha-aaa", None, "org-policy:2");
        assert_eq!(
            mode,
            RolloutMode::Enforce,
            "same head SHA but different policy revision must be Enforce"
        );
    }

    // AC: Same head SHA + same policy revision in a multi-event log →
    // AlreadyEvaluated even when other SHAs are present.
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

    // AC: New PR after policy publication (no prior events +
    // policy_publication_ts is Some) → Enforce.
    #[test]
    fn determine_rollout_mode_new_pr_after_publication_is_enforce() {
        let entries: Vec<ActivityEntryRef> = vec![];
        let mode =
            determine_rollout_mode(&entries, "sha-new", Some("2026-01-01T00:00:00Z"), "default");
        assert_eq!(
            mode,
            RolloutMode::Enforce,
            "new PR after policy publication must be Enforce, not Backfill"
        );
    }

    // AC: Existing PR before policy publication (no prior events +
    // policy_publication_ts is None) → Backfill.
    #[test]
    fn determine_rollout_mode_existing_pr_before_publication_is_backfill() {
        let entries: Vec<ActivityEntryRef> = vec![];
        let mode = determine_rollout_mode(&entries, "sha-old", None, "default");
        assert_eq!(mode, RolloutMode::Backfill);
    }

    // ── Rollout mode: is_tripwire_gate_event ─────────────────────────────

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

    // ── Rollout mode: policy report-only override ────────────────────────

    /// Backfill policy must force all findings to report-only.
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

        // All rules must still be enabled.
        assert!(report_only.migration.enabled);
        assert!(report_only.dependency_identity.enabled);
        assert!(report_only.network_egress.enabled);
        assert!(report_only.unsafe_code.enabled);
        assert!(report_only.boundary_path.enabled);
        assert!(report_only.large_delete_rewrite.enabled);
        assert!(report_only.ci_workflow.enabled);
    }

    /// Backfill evaluation of migration change must produce ReportOnly
    /// (not Held).
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
        // Event type must be report-only.
        assert_eq!(event_type_for(&decision), TRIPWIRE_EVENT_GATE_REPORT_ONLY);
    }

    /// Backfill evaluation of CI workflow change must produce ReportOnly.
    #[test]
    fn backfill_ci_workflow_change_produces_report_only() {
        let policy = TripwirePolicy::default().make_report_only();
        let files = vec![pr_file(".github/workflows/ci.yml", "modified", 15, 5)];
        let decision = evaluate_from_pr_files_with_policy(files, policy);
        assert_eq!(decision.outcome, GateOutcome::ReportOnly);
    }

    /// Backfill evaluation of all seven rule families must produce
    /// ReportOnly (not Held) even though every rule trips.
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

        // All findings must be report-only severity.
        for f in &decision.findings {
            assert_eq!(f.severity, TripwireFindingSeverity::ReportOnly);
        }
    }

    // ── Rollout mode: idempotency keys change with head SHA / policy ────

    /// Idempotency key must change when head SHA changes (same policy).
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
        assert_ne!(
            d_a.idempotency_key, d_b.idempotency_key,
            "idempotency key must change when head SHA changes"
        );
    }

    /// Idempotency key must change when policy revision changes (same
    /// head SHA).
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
        assert_ne!(
            d_v1.idempotency_key, d_v2.idempotency_key,
            "idempotency key must change when policy revision changes"
        );
    }

    /// Duplicate backfill for same task/head/policy is idempotent — the
    /// same gate decision idempotency key is produced.
    #[test]
    fn duplicate_backfill_same_key() {
        let files = vec![pr_file("migrations/001.sql", "added", 10, 0)];
        let d1 = evaluate_from_pr_files(files.clone());
        let d2 = evaluate_from_pr_files(files);
        assert_eq!(d1.idempotency_key, d2.idempotency_key);
        assert_eq!(d1.outcome, d2.outcome);
    }
}
