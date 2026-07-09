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
pub(super) fn parse_patch_to_hunks(patch: &str) -> Vec<DiffHunk> {
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
pub(crate) fn run_gate(input: &TripwireEvaluationInput) -> TripwireGateResult {
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
