// Validation helpers and status-derivation functions for proposal refinement.
//
// Extracted from `refinement_tools` to keep the tool-router module under the
// repository's per-file size guard.  All public items here are `pub(crate)`
// except `build_refinement_status`, `check_needs_evidence_cap`, and
// `ProposalRefinementDemandEvidenceParams` which have external callers or are
// part of the MCP schema surface.

use serde::Deserialize;
use sqlx::Row;

use crate::tools::proposal_ops::{
    EvidenceLifecyclePhase, EvidenceLifecycleState, NeedsEvidenceStatus,
    ProposalRefinementStatusModel,
};
use djinn_core::{
    models::{EvidenceFindings, NeedsEvidenceClaim},
    refinement_liveness::{
        RefinementLivenessEvidence, RefinementLivenessResult, RefinementParkKind,
        RefinementRunState,
    },
};
use djinn_db::{
    EvidenceRepository, ProposalDebateTrailCreateInput, ProposalRepository, TaskRepository,
};

use crate::tools::evidence_findings::{
    EvidenceCompletionV1, finalize_evidence_completion_v1_in_transaction,
    render_evidence_judge_projection,
};
use crate::tools::evidence_plan::EvidencePlanIdentity;

// ── Param struct ─────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalRefinementDemandEvidenceParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
    /// The debate round when the demand is issued (from the Judge's task
    /// description).
    pub round: i32,
    /// The proposal revision sequence the demand targets.
    pub against_revision_seq: i32,
    /// The feasibility question the evidence spike must answer.
    pub question: String,
    /// The subsystem or module under investigation.
    pub target_subsystem: String,
    /// What in the spec is unknown or unverified.
    pub spec_unknown_anchor: String,
    /// Why in-session research was insufficient to resolve the claim.
    pub insufficient_in_session_research: String,
    /// What the evidence spike should produce to resolve the claim.
    pub expected_findings: String,
    /// Caller-declared load-bearing threshold. The server must not infer this
    /// authority from legacy question prose.
    pub load_bearing_category: String,
}

// ── Demand-evidence validation helpers ───────────────────────────────────────

/// Terminal proposal statuses that cannot accept needs-evidence demands.
const TERMINAL_PROPOSAL_STATUSES: &[&str] = &["done", "rejected", "archived", "superseded"];

/// Generic, non-falsifiable question patterns that the Judge must not use.
/// A valid question must be specific enough to be proven or disproven by
/// concrete evidence; vague "investigate/improve" requests are rejected.
const GENERIC_QUESTION_PATTERNS: &[&str] = &[
    "investigate further",
    "improve",
    "design more",
    "research further",
    "look into",
    "explore more",
    "consider alternatives",
    "review more",
    "think about",
    "study more",
];

const PREFERENCE_QUESTION_PATTERNS: &[&str] = &[
    "should we prefer",
    "which do you prefer",
    "would be nicer",
    "style",
    "color",
    "colour",
    "better than",
    "best ",
];
const REPOSITORY_ANSWERABLE_PATTERNS: &[&str] = &[
    "is already in the repository",
    "can be answered by inspecting",
    "grep",
    "search the repository",
    "which function currently",
    "which function parses",
];

/// Find authority from its exact persisted task/intent/run correlation.
async fn find_active_evidence_authority_task(
    repo: &ProposalRepository,
    proposal_id: &str,
    against_revision_seq: i32,
    round: i32,
) -> Result<Option<(String, String)>, String> {
    let row = sqlx::query(
        "SELECT t.id, t.created_by_user_id FROM tasks t \
         JOIN refinement_dispatch_intents i ON i.id = t.refinement_intent_id \
         JOIN refinement_runs r ON r.id = i.run_id JOIN proposals p ON p.id = r.proposal_id \
         WHERE r.proposal_id = $1 AND r.state = 'running' AND p.latest_revision_seq = $2 \
           AND i.round = $3 AND i.state = 'materialized' AND i.task_id = t.id \
           AND t.issue_type = 'refinement' AND t.status IN ('open', 'in_progress') \
           AND t.agent_type IN ('judge', 'adversary') AND t.refinement_run_id = r.id \
           AND t.refinement_generation = r.generation AND t.refinement_round = i.round \
           AND t.refinement_phase = i.phase AND t.refinement_role = i.role AND t.agent_type = t.refinement_role \
           AND t.refinement_role IN ('judge', 'adversary') ORDER BY t.updated_at DESC, t.id DESC LIMIT 1",
    )
    .bind(proposal_id)
    .bind(against_revision_seq)
    .bind(round)
    .fetch_optional(repo.db().pool())
    .await
    .map_err(|e| format!("failed to query active refinement authority: {e}"))?;
    Ok(row.map(|row| (row.get("id"), row.get("created_by_user_id"))))
}

/// Verify the caller is the active Judge for this proposal's refinement run.
///
/// Checks:
/// - A session user identity exists (`auth_context::current_user_id()`).
/// - An active Judge task is in flight for this proposal.
/// - The caller's user id matches the Judge task's `created_by_user_id`.
///
/// Returns `Ok(judge_task_id)` when authorized, or `Err(rejection_reason)`
/// when the caller is not the active Judge.
pub(crate) async fn verify_active_judge_authorization(
    repo: &ProposalRepository,
    proposal_id: &str,
    against_revision_seq: i32,
    round: i32,
) -> Result<String, String> {
    // The caller must have a session identity.
    let caller_user_id = djinn_core::auth_context::current_user_id();
    let Some(caller_id) = caller_user_id else {
        return Err("caller is not authenticated: no session user identity; \
             only the active Judge may demand evidence"
            .to_string());
    };

    // Find the active Judge task for this proposal's refinement run.
    let judge_task =
        find_active_evidence_authority_task(repo, proposal_id, against_revision_seq, round).await?;

    let Some(task) = judge_task else {
        return Err(
            "no active Adversary or Judge task in flight for this proposal's refinement; \
             the caller cannot be verified as evidence authority"
                .to_string(),
        );
    };

    // The caller must match the Judge task's attributed user.
    let (task_id, task_owner) = task;
    let task_owner = task_owner.as_str();
    if task_owner.is_empty() || task_owner != caller_id {
        return Err(format!(
            "caller '{}' is not the active Adversary or Judge for this proposal \
             (authority task {} attributed to '{}')",
            caller_id,
            task_id,
            if task_owner.is_empty() {
                "nobody"
            } else {
                task_owner
            },
        ));
    }
    Ok(task_id)
}

/// Validate demand-evidence parameters and proposal/refinement state before
/// any mutation occurs. Returns `Ok(())` when the demand is valid, or
/// `Err(rejection_reason)` when it should be rejected without side effects.
///
/// Checks (in order):
/// 0. **Caller is the active Judge** — verifies caller identity via
///    `auth_context::current_user_id()` matches the active Judge task's
///    `created_by_user_id` for this proposal's refinement run.
/// 1. **Proposal not terminal** — terminal proposals cannot accept demands.
/// 2. **Refinement active** — must be in an active refinement run.
/// 3. **Refinement not awaiting review** — the Judge must still be
///    adjudicating (not converged/parked for human accept/reject).
/// 4. **Round matches** — demand round must equal the current refinement
///    round (prevents stale or ahead-of-time demands).
/// 5. **`against_revision_seq` valid** — must be `<=` the proposal's
///    `latest_revision_seq` (cannot target a future revision).
/// 6. **Question specific & falsifiable** — non-empty, has a question mark,
///    and does not match any generic pattern.
/// 7. **`target_subsystem` non-empty** — must identify a concrete subsystem.
/// 8. **`spec_unknown_anchor` present in reviewed body** — the anchor text
///    must appear in the proposal revision being reviewed.
/// 9. **`insufficient_in_session_research` non-empty** — must state what
///    normal Judge research could not answer.
/// 10. **Needs-evidence cap not exhausted** — uses persisted substrate
///     helpers (no in-memory counters).
/// 11. **No existing open linked evidence spike** — a proposal can have at
///     most one open spike at a time.
pub(crate) async fn validate_demand_evidence(
    repo: &ProposalRepository,
    _task_repo: &TaskRepository,
    proposal: &djinn_core::models::Proposal,
    refinement: &ProposalRefinementStatusModel,
    params: &ProposalRefinementDemandEvidenceParams,
) -> Result<String, String> {
    // 0. Caller must be the active Judge for this proposal's refinement run.
    //    This check runs before any state inspection so that non-Judge
    //    callers receive a typed authorization rejection before any
    //    proposal/task/debate/lifecycle mutation.
    //    Returns the Judge task id on success.
    let judge_task_id = verify_active_judge_authorization(
        repo,
        &proposal.id,
        params.against_revision_seq,
        params.round,
    )
    .await?;

    // 1. Terminal proposals cannot accept demands.
    if TERMINAL_PROPOSAL_STATUSES.contains(&proposal.status.as_str()) {
        return Err(format!(
            "proposal status '{}' is terminal; needs-evidence demands are not accepted",
            proposal.status
        ));
    }

    // 2. Refinement must be active.
    if !refinement.active {
        return Err(
            "refinement is not active for this proposal; start refinement before demanding evidence"
                .to_string(),
        );
    }

    // 3. Refinement must not have converged (awaiting human review).
    if refinement.awaiting_review {
        return Err(
            "refinement has converged and is awaiting human review; demands are not accepted"
                .to_string(),
        );
    }

    // 4. Round must match the current refinement round.
    let current_round = refinement.current_round.unwrap_or(1);
    if params.round != current_round {
        return Err(format!(
            "demand round {} does not match the current refinement round {}",
            params.round, current_round,
        ));
    }

    // A demand is authority for the exact current revision, not an older
    // revision which happens still to be in history.
    if params.against_revision_seq != proposal.latest_revision_seq {
        return Err(format!(
            "against_revision_seq {} does not match the proposal's active revision seq {}",
            params.against_revision_seq, proposal.latest_revision_seq,
        ));
    }

    // 6. Question must be specific and falsifiable.
    let question_trimmed = params.question.trim();
    if question_trimmed.is_empty() {
        return Err("question must not be empty".to_string());
    }
    if !question_trimmed.contains('?') {
        return Err(
            "question must be falsifiable: include a '?' to indicate a concrete question to answer"
                .to_string(),
        );
    }
    let question_lower = question_trimmed.to_lowercase();
    for pattern in GENERIC_QUESTION_PATTERNS {
        if question_lower.contains(pattern) {
            return Err(format!(
                "question is too generic ('{pattern}' detected); specify a concrete, falsifiable claim"
            ));
        }
    }

    if PREFERENCE_QUESTION_PATTERNS
        .iter()
        .any(|pattern| question_lower.contains(pattern))
    {
        return Err(
            "question is preference-only; evidence demands require a load-bearing uncertainty"
                .to_string(),
        );
    }

    // 7. `target_subsystem` must be non-empty.
    if params.target_subsystem.trim().is_empty() {
        return Err("target_subsystem must not be empty".to_string());
    }

    if !matches!(
        params.load_bearing_category.trim(),
        "feasibility"
            | "safety"
            | "integrity"
            | "compatibility"
            | "rollout"
            | "core_acceptance_criteria"
    ) {
        return Err("load_bearing_category must be feasibility, safety, integrity, compatibility, rollout, or core_acceptance_criteria".to_string());
    }

    // `spec_unknown_anchor` is an explicit caller assertion. Do not inspect
    // proposal prose, Open questions, or QuestionForm to manufacture it.
    let anchor = params.spec_unknown_anchor.trim();
    if anchor.is_empty() {
        return Err("spec_unknown_anchor must not be empty".to_string());
    }
    // 9. `insufficient_in_session_research` must be non-empty.
    if params.insufficient_in_session_research.trim().is_empty() {
        return Err(
            "insufficient_in_session_research must state what normal Judge research could not answer"
                .to_string(),
        );
    }

    // Classify the explicit request only. Proposal prose and question-form
    // content never manufacture or validate a finding.
    let rationale = params.insufficient_in_session_research.to_lowercase();
    if REPOSITORY_ANSWERABLE_PATTERNS
        .iter()
        .any(|pattern| question_lower.contains(pattern) || rationale.contains(pattern))
    {
        return Err("demand is repository-answerable; inspect the repository instead of allocating evidence".to_string());
    }
    let expected = params.expected_findings.trim();
    if expected.is_empty() {
        return Err(
            "expected_findings must name focused checks the evidence spike will perform"
                .to_string(),
        );
    }
    if expected.len() < 8 || expected.eq_ignore_ascii_case("findings") {
        return Err("expected_findings must name focused, concrete checks".to_string());
    }

    // 10. Cap must not be exhausted.
    match check_needs_evidence_cap(repo, &proposal.id).await {
        Ok(cap_status) => {
            if cap_status.no_refinement_run {
                return Err(
                    "no active refinement run for this proposal; cap accounting unavailable"
                        .to_string(),
                );
            }
            if cap_status.cap_exceeded {
                return Err(format!(
                    "needs-evidence cap reached ({}/{}); no more demands allowed this run",
                    cap_status.count, cap_status.cap,
                ));
            }
        }
        Err(e) => return Err(e),
    }

    Ok(judge_task_id)
}

// ── Refinement status derivation ─────────────────────────────────────────────

const REFINEMENT_HEARTBEAT_GRACE_MILLIS: i64 = 60_000;

/// Keep the response bounded while exposing which shared-evaluator branch won.
pub(crate) fn liveness_fields(result: &RefinementLivenessResult) -> (&'static str, Option<String>) {
    match result {
        RefinementLivenessResult::Terminal { .. } => ("terminal", None),
        RefinementLivenessResult::Stale { .. } => ("stale", None),
        RefinementLivenessResult::Live { evidence } => (
            "live",
            Some(
                match evidence {
                    RefinementLivenessEvidence::AwaitingReviewPark => "awaiting_review_park",
                    RefinementLivenessEvidence::AwaitingEvidencePark => "awaiting_evidence_park",
                    RefinementLivenessEvidence::PendingIntent { .. } => "pending_intent",
                    RefinementLivenessEvidence::ClaimedIntent { .. } => "claimed_intent",
                    RefinementLivenessEvidence::MaterializedIntent { .. } => "materialized_intent",
                    RefinementLivenessEvidence::OpenTask { .. } => "open_task",
                    RefinementLivenessEvidence::QueuedTask { .. } => "queued_task",
                    RefinementLivenessEvidence::RunningTask { .. } => "running_task",
                    RefinementLivenessEvidence::PoolPausedTask { .. } => "pool_paused_task",
                    RefinementLivenessEvidence::LiveSession { .. } => "live_session",
                    RefinementLivenessEvidence::BetweenPhase { .. } => "between_phase",
                    RefinementLivenessEvidence::FreshHeartbeat { .. } => "fresh_heartbeat",
                }
                .to_string(),
            ),
        ),
    }
}

fn run_state_name(state: RefinementRunState) -> &'static str {
    match state {
        RefinementRunState::Active => "active",
        RefinementRunState::Parked => "parked",
        RefinementRunState::Terminal => "terminal",
    }
}

/// Derive the current refinement status from an exact-run snapshot plus
/// compatible display-only lifecycle and debate content.
pub async fn build_refinement_status(
    repo: &ProposalRepository,
    proposal_id: &str,
) -> Result<ProposalRefinementStatusModel, String> {
    // This is the only liveness authority for status. The repository loads the
    // exact run and evaluates it in one read-only repeatable-read observation.
    let exact = repo
        .load_current_refinement_run_snapshot(proposal_id, REFINEMENT_HEARTBEAT_GRACE_MILLIS)
        .await
        .map_err(|e| format!("failed to load current refinement run snapshot: {e}"))?;
    // Legacy rows below supply compatible display-only content; they never
    // decide whether an exact run is live, stale, parked, or terminal.
    let revisions = repo
        .revisions(proposal_id)
        .await
        .map_err(|e| format!("failed to read revisions: {e}"))?;

    // Read legacy display-only review content. Older proposals record a
    // `refinement_awaiting_review` lifecycle event when they converge, and the
    // human's resolve records a `refinement_stop` after it. Preserve the
    // historical latest-start/awaiting/stop ordering for display fields only;
    // exact-run liveness below remains the sole authority for `active`.
    let latest_start = revisions
        .iter()
        .rev()
        .find(|r| r.event_kind == "refinement_start");
    let latest_awaiting = revisions
        .iter()
        .rev()
        .find(|r| r.event_kind == "refinement_awaiting_review");
    let latest_stop = revisions
        .iter()
        .rev()
        .find(|r| r.event_kind == "refinement_stop");
    let legacy_awaiting_review = match (latest_start, latest_awaiting, latest_stop) {
        (Some(start), Some(awaiting), Some(stop)) => {
            start.created_at <= awaiting.created_at && stop.created_at < awaiting.created_at
        }
        (Some(start), Some(awaiting), None) => start.created_at <= awaiting.created_at,
        _ => false,
    };
    let (judge_summary, snapshot_revision_seq) = if legacy_awaiting_review {
        let meta = latest_awaiting
            .and_then(|r| r.event_metadata.as_ref())
            .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok());
        let summary = meta
            .as_ref()
            .and_then(|v| v.get("judge_summary")?.as_str().map(String::from));
        let snap = meta
            .as_ref()
            .and_then(|v| v.get("snapshot_revision_seq")?.as_i64())
            .map(|n| n as i32);
        (summary, snap)
    } else {
        (None, None)
    };

    // Derive round and dry-round counts from debate trail.
    let trail = repo
        .debate_trail(proposal_id)
        .await
        .map_err(|e| format!("failed to read debate trail: {e}"))?;

    let total_entries = trail.len() as i32;

    // Current round = max round in the debate trail, or 1 if no entries yet.
    let current_round = trail.iter().map(|e| e.round).max().unwrap_or(1);

    // Dry rounds: count consecutive adversary rounds at the end that produced
    // no new blocking objections.
    let dry_rounds = if trail.is_empty() {
        0
    } else {
        let max_round = current_round;
        let mut consecutive_dry = 0;
        for round in (1..=max_round).rev() {
            let has_blocking_objection = trail.iter().any(|e| {
                e.round == round
                    && e.kind == "objection"
                    && e.blocking
                    && e.agent_role == "adversary"
            });
            if !has_blocking_objection {
                consecutive_dry += 1;
            } else {
                break;
            }
        }
        consecutive_dry
    };

    // Derive needs-evidence state from the proposal's linked spike.
    let proposal = repo
        .get(proposal_id)
        .await
        .map_err(|e| format!("failed to load proposal: {e}"))?;

    let needs_evidence = if let Some(ref spike_id) = proposal
        .as_ref()
        .and_then(|p| p.linked_spike_task_id.as_ref())
    {
        let task_repo = TaskRepository::new(repo.db().clone(), repo.events().clone());
        let spike = task_repo.get(spike_id).await.ok().flatten();

        // Parse the stored claim: try structured JSON first, fall back to
        // legacy plain-string claims without panicking.
        let raw_claim = proposal
            .as_ref()
            .and_then(|p| p.needs_evidence_claim.as_deref())
            .unwrap_or("");
        let parsed_claim = NeedsEvidenceClaim::parse_stored(Some(raw_claim)).unwrap_or(None);

        // Derive the display claim and structured fields.
        let (
            claim_str,
            question,
            target_subsystem,
            spec_unknown_anchor,
            round,
            against_revision_seq,
            created_by_task_id,
            insufficient_in_session_research,
            expected_findings,
        ) = if let Some(ref c) = parsed_claim {
            (
                c.question.clone(),
                Some(c.question.clone()),
                Some(c.target_subsystem.clone()),
                Some(c.spec_unknown_anchor.clone()),
                Some(c.round),
                Some(c.against_revision_seq),
                Some(c.created_by_task_id.clone()),
                Some(c.insufficient_in_session_research.clone()),
                Some(c.expected_findings.clone()),
            )
        } else {
            // Legacy plain-string claim or empty.
            (
                raw_claim.to_string(),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
        };

        // Derive the evidence lifecycle phase from persisted lifecycle events.
        // Reuse the already-fetched revisions to avoid a second DB round trip.
        // Walk backwards to find the latest evidence lifecycle event for this
        // specific spike task id.
        let spike_id_str = spike_id.to_string();
        let mut evidence_phase = None;
        let mut failure_reason = None;
        for rev in revisions.iter().rev() {
            if rev.event_kind != "refinement_awaiting_evidence_started"
                && rev.event_kind != "refinement_evidence_received"
                && rev.event_kind != "refinement_evidence_failed"
            {
                continue;
            }
            // Confirm this lifecycle event is for the current spike by
            // parsing the wrapped metadata JSON.
            let parsed_meta = rev
                .event_metadata
                .as_ref()
                .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok());
            let meta_inner = parsed_meta.as_ref().and_then(|v| v.get("metadata"));
            let event_spike_id = meta_inner
                .and_then(|m| m.get("spike_task_id"))
                .and_then(|v| v.as_str());
            if event_spike_id != Some(&spike_id_str) {
                continue;
            }
            // Found the latest matching lifecycle event.
            match rev.event_kind.as_str() {
                "refinement_awaiting_evidence_started" => {
                    evidence_phase = Some(EvidenceLifecyclePhase::AwaitingEvidence);
                }
                "refinement_evidence_received" => {
                    evidence_phase = Some(EvidenceLifecyclePhase::EvidenceReceived);
                }
                "refinement_evidence_failed" => {
                    evidence_phase = Some(EvidenceLifecyclePhase::EvidenceFailed);
                    failure_reason = meta_inner
                        .and_then(|m| m.get("failure_reason"))
                        .and_then(|v| v.as_str().map(String::from));
                }
                _ => {}
            }
            break;
        }

        Some(NeedsEvidenceStatus {
            claim: claim_str,
            spike_task_id: spike_id.to_string(),
            spike_short_id: spike
                .as_ref()
                .map(|t| t.short_id.clone())
                .unwrap_or_default(),
            spike_status: spike
                .as_ref()
                .map(|t| t.status.clone())
                .unwrap_or_else(|| "unknown".to_string()),
            question,
            target_subsystem,
            spec_unknown_anchor,
            round,
            against_revision_seq,
            created_by_task_id,
            insufficient_in_session_research,
            expected_findings,
            evidence_phase,
            failure_reason,
        })
    } else {
        None
    };

    // ── Derive top-level evidence lifecycle state ────────────────────────
    //
    // Uses durable proposal fields, lifecycle events, and linked-spike
    // task status.  Precedence (highest → lowest):
    //
    //   1. Terminal       — proposal status is done/rejected/archived/superseded
    //   2. PausedOrFrozen — admin freeze is active (`build_frozen = true`)
    //   3. EvidenceFailed — persisted failure lifecycle event
    //   4. EvidenceReceived — persisted receipt lifecycle event
    //   5. AwaitingEvidence — open linked evidence spike
    //   6. Active — refinement running, no evidence parking
    //
    // `dispatch_paused` is coordinator-internal (in-memory) and not
    // available to the control-plane, so PausedOrFrozen here is derived
    // solely from `build_frozen`.

    let proposal_status = proposal.as_ref().map(|p| p.status.as_str()).unwrap_or("");

    let evidence_lifecycle_state = if TERMINAL_PROPOSAL_STATUSES.contains(&proposal_status) {
        EvidenceLifecycleState::Terminal
    } else if proposal.as_ref().is_some_and(|p| p.build_frozen) {
        EvidenceLifecycleState::PausedOrFrozen
    } else if let Some(ref ne) = needs_evidence {
        match ne.evidence_phase {
            Some(EvidenceLifecyclePhase::EvidenceFailed) => EvidenceLifecycleState::EvidenceFailed,
            Some(EvidenceLifecyclePhase::EvidenceReceived) => {
                EvidenceLifecycleState::EvidenceReceived
            }
            Some(EvidenceLifecyclePhase::AwaitingEvidence) => {
                EvidenceLifecycleState::AwaitingEvidence
            }
            // Linked spike exists but no lifecycle event recorded yet:
            // the spike was just created and the
            // `refinement_awaiting_evidence_started` event may not
            // have been written yet.  Still awaiting.
            None => EvidenceLifecycleState::AwaitingEvidence,
        }
    } else if exact
        .as_ref()
        .is_some_and(|exact| matches!(exact.liveness, RefinementLivenessResult::Live { .. }))
    {
        EvidenceLifecycleState::Active
    } else {
        // Refinement stopped but proposal is not terminal.
        EvidenceLifecycleState::Active
    };

    let liveness = exact.as_ref().map(|exact| liveness_fields(&exact.liveness));
    let active = exact
        .as_ref()
        .is_some_and(|exact| matches!(exact.liveness, RefinementLivenessResult::Live { .. }));
    let exact_awaiting_review = exact.as_ref().is_some_and(|exact| {
        matches!(
            exact.snapshot.park.as_ref().map(|park| park.kind),
            Some(RefinementParkKind::AwaitingReview)
        )
    });
    let awaiting_review = exact_awaiting_review || legacy_awaiting_review;
    // Retain the historical stop label as display-only compatibility data
    // when the exact run has no terminal reason. It never affects `active`,
    // run state, or liveness; those remain exact-snapshot-only.
    let legacy_stop_reason = revisions.iter().rev().find_map(|revision| {
        (revision.event_kind == "refinement_stop")
            .then_some(revision.event_metadata.as_ref())
            .flatten()
            .and_then(|metadata| serde_json::from_str::<serde_json::Value>(metadata).ok())
            .and_then(|metadata| {
                ["reason_tag", "stop_reason", "reason"]
                    .iter()
                    .find_map(|key| metadata.get(key).and_then(|value| value.as_str()))
                    .map(str::to_owned)
            })
    });
    let stop_reason = exact
        .as_ref()
        .and_then(|exact| match &exact.liveness {
            RefinementLivenessResult::Terminal { reason } => {
                reason.as_ref().map(|reason| reason.tag().to_string())
            }
            _ => None,
        })
        .or(legacy_stop_reason);

    Ok(ProposalRefinementStatusModel {
        active,
        run_id: exact
            .as_ref()
            .map(|exact| exact.snapshot.run.run_id.clone()),
        generation: exact.as_ref().map(|exact| exact.generation),
        run_state: exact
            .as_ref()
            .map(|exact| run_state_name(exact.snapshot.run.state).to_string()),
        liveness: liveness.as_ref().map(|(state, _)| (*state).to_string()),
        liveness_evidence: liveness.and_then(|(_, evidence)| evidence),
        last_heartbeat_at: exact.as_ref().and_then(|exact| {
            exact
                .snapshot
                .heartbeat
                .as_ref()
                .map(|heartbeat| heartbeat.heartbeat_at.0)
        }),
        // Preserve the inactive no-run wire shape: an empty debate trail did
        // not historically expose a synthetic first round. Exact runs do
        // expose their current snapshot even before the first trail entry.
        current_round: (!trail.is_empty() || exact.is_some()).then_some(current_round),
        dry_rounds,
        total_entries,
        stop_reason,
        awaiting_review,
        judge_summary: if awaiting_review { judge_summary } else { None },
        snapshot_revision_seq: if awaiting_review {
            snapshot_revision_seq
        } else {
            None
        },
        needs_evidence,
        evidence_lifecycle_state,
    })
}

/// Inspect the current needs-evidence cap status for a proposal.
///
/// Returns the cap status reconstructed from persisted debate/lifecycle
/// rows — no in-memory counters.
///
/// When `no_refinement_run` is true, the caller should not issue demands
/// (there is no active refinement to park). When `cap_exceeded` is true,
/// the caller must reject the demand before any spike/link write occurs.
pub async fn check_needs_evidence_cap(
    repo: &ProposalRepository,
    proposal_id: &str,
) -> Result<djinn_db::NeedsEvidenceCapStatus, String> {
    repo.needs_evidence_cap_status_for_current_run(proposal_id)
        .await
        .map_err(|e| format!("failed to check needs-evidence cap: {e}"))
}

/// Atomically persist V1 and its legacy-compatible debate row for the exact
/// proposal-linked evidence spike. The transaction is committed only after both
/// inserts have succeeded, so either insertion failure rolls both back.
pub async fn complete_linked_refinement_evidence_v1(
    repo: &ProposalRepository,
    proposal_id: &str,
    spike_task_id: &str,
    identity: &EvidencePlanIdentity,
    completion: EvidenceCompletionV1,
) -> Result<(), String> {
    let proposal = repo
        .get(proposal_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "proposal not found".to_string())?;
    if proposal.linked_spike_task_id.as_deref() != Some(spike_task_id) {
        return Err("proposal is not linked to this evidence spike".to_string());
    }
    let claim = NeedsEvidenceClaim::parse_stored(proposal.needs_evidence_claim.as_deref())
        .map_err(|error| format!("invalid linked evidence claim: {error}"))?
        .ok_or_else(|| "linked evidence spike has no structured claim".to_string())?;
    let evidence = EvidenceRepository::new(repo.db().clone());
    repo.db()
        .ensure_initialized()
        .await
        .map_err(|error| error.to_string())?;
    let mut tx = repo
        .db()
        .pool()
        .begin()
        .await
        .map_err(|error| error.to_string())?;
    let projection =
        finalize_evidence_completion_v1_in_transaction(&evidence, &mut tx, identity, completion)
            .await
            .map_err(|error| error.to_string())?;
    let rendered =
        render_evidence_judge_projection(&projection.payload).map_err(|error| error.to_string())?;
    let legacy = EvidenceFindings {
        answer: rendered.clone(),
        evidence: vec![rendered.clone()],
        code_paths_inspected: Vec::new(),
        confidence: 0.0,
        residual_risks: vec!["Structured V1 projection is authoritative.".to_string()],
        recommendation_for_advocate:
            "Use the structured projection and its gaps when resuming refinement.".to_string(),
    };
    let metadata = serde_json::to_value(legacy).map_err(|error| error.to_string())?;
    repo.add_debate_trail_entry_in_tx(
        &mut tx,
        ProposalDebateTrailCreateInput {
            proposal_id,
            kind: "evidence_findings",
            body: &rendered,
            blocking: false,
            agent_role: "spike",
            author_kind: "agent",
            author_model: None,
            source_task_id: Some(spike_task_id),
            against_revision_seq: claim.against_revision_seq,
            round: claim.round,
            body_metadata: Some(&metadata),
        },
    )
    .await
    .map_err(|error| error.to_string())?;
    tx.commit().await.map_err(|error| error.to_string())
}
