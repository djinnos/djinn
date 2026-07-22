// Validation helpers and status-derivation functions for proposal refinement.
//
// Extracted from `refinement_tools` to keep the tool-router module under the
// repository's per-file size guard.  All public items here are `pub(crate)`
// except `build_refinement_status`, `check_needs_evidence_cap`, and
// `ProposalRefinementDemandEvidenceParams` which have external callers or are
// part of the MCP schema surface.

use serde::Deserialize;

use crate::tools::proposal_ops::{
    EvidenceLifecyclePhase, EvidenceLifecycleState, NeedsEvidenceStatus,
    ProposalRefinementStatusModel,
};
use djinn_core::models::NeedsEvidenceClaim;
use djinn_db::{ProposalRepository, TaskRepository};

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

/// Find the active Judge task for a proposal's refinement run.
///
/// Queries the DB for an open or in-progress refinement task whose
/// `agent_type` is `"judge"` and whose title contains the proposal id.
/// Returns the task if found, or `None` when no Judge task is in flight
/// for this proposal.
async fn find_active_judge_task(
    task_repo: &TaskRepository,
    proposal_id: &str,
) -> Result<Option<djinn_core::models::Task>, String> {
    let open_tasks = task_repo
        .list_by_status("open")
        .await
        .map_err(|e| format!("failed to query open tasks: {e}"))?;
    let in_progress_tasks = task_repo
        .list_by_status("in_progress")
        .await
        .map_err(|e| format!("failed to query in_progress tasks: {e}"))?;

    let candidate = open_tasks.into_iter().chain(in_progress_tasks).find(|t| {
        t.issue_type == "refinement"
            && t.agent_type.as_deref() == Some("judge")
            && t.title.contains(proposal_id)
    });

    Ok(candidate)
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
    task_repo: &TaskRepository,
    proposal_id: &str,
) -> Result<String, String> {
    // The caller must have a session identity.
    let caller_user_id = djinn_core::auth_context::current_user_id();
    let Some(caller_id) = caller_user_id else {
        return Err("caller is not authenticated: no session user identity; \
             only the active Judge may demand evidence"
            .to_string());
    };

    // Find the active Judge task for this proposal's refinement run.
    let judge_task = find_active_judge_task(task_repo, proposal_id).await?;

    let Some(task) = judge_task else {
        return Err(
            "no active Judge task in flight for this proposal's refinement; \
             the caller cannot be verified as the active Judge"
                .to_string(),
        );
    };

    // The caller must match the Judge task's attributed user.
    let task_owner = task.created_by_user_id.as_str();
    if task_owner.is_empty() || task_owner != caller_id {
        return Err(format!(
            "caller '{}' is not the active Judge for this proposal \
             (Judge task {} attributed to '{}')",
            caller_id,
            task.id,
            if task_owner.is_empty() {
                "nobody"
            } else {
                task_owner
            },
        ));
    }

    Ok(task.id)
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
    task_repo: &TaskRepository,
    proposal: &djinn_core::models::Proposal,
    refinement: &ProposalRefinementStatusModel,
    params: &ProposalRefinementDemandEvidenceParams,
) -> Result<String, String> {
    // 0. Caller must be the active Judge for this proposal's refinement run.
    //    This check runs before any state inspection so that non-Judge
    //    callers receive a typed authorization rejection before any
    //    proposal/task/debate/lifecycle mutation.
    //    Returns the Judge task id on success.
    let judge_task_id = verify_active_judge_authorization(task_repo, &proposal.id).await?;

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

    // 5. `against_revision_seq` must be valid (not beyond latest).
    if params.against_revision_seq > proposal.latest_revision_seq {
        return Err(format!(
            "against_revision_seq {} exceeds the proposal's latest revision seq {}",
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

    // 7. `target_subsystem` must be non-empty.
    if params.target_subsystem.trim().is_empty() {
        return Err("target_subsystem must not be empty".to_string());
    }

    // 8. `spec_unknown_anchor` must be present in the reviewed proposal body.
    let anchor = params.spec_unknown_anchor.trim();
    if anchor.is_empty() {
        return Err("spec_unknown_anchor must not be empty".to_string());
    }
    // Look up the body of the reviewed revision. If the against_revision_seq
    // matches the current head, use the live proposal body; otherwise read
    // from proposal_revisions.
    let revision_body = if params.against_revision_seq >= proposal.latest_revision_seq {
        proposal.body.clone()
    } else {
        let revisions = repo
            .revisions(&proposal.id)
            .await
            .map_err(|e| format!("failed to read revisions: {e}"))?;
        revisions
            .iter()
            .rev()
            .find(|r| r.seq == params.against_revision_seq && r.event_kind == "spec_revision")
            .map(|r| r.body.clone())
            .unwrap_or_default()
    };
    if !revision_body.contains(anchor) {
        return Err(format!(
            "spec_unknown_anchor '{}' not found in the reviewed proposal revision (seq {})",
            anchor, params.against_revision_seq,
        ));
    }

    // 9. `insufficient_in_session_research` must be non-empty.
    if params.insufficient_in_session_research.trim().is_empty() {
        return Err(
            "insufficient_in_session_research must state what normal Judge research could not answer"
                .to_string(),
        );
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

    // 11. No existing open linked evidence spike.
    if proposal.linked_spike_task_id.is_some() {
        return Err(format!(
            "proposal already has an open linked evidence spike ({}); resolve it before demanding new evidence",
            proposal
                .linked_spike_task_id
                .as_deref()
                .unwrap_or("unknown"),
        ));
    }

    Ok(judge_task_id)
}

// ── Refinement status derivation ─────────────────────────────────────────────

/// Derive the current refinement status from lifecycle events and debate trail.
pub async fn build_refinement_status(
    repo: &ProposalRepository,
    proposal_id: &str,
) -> Result<ProposalRefinementStatusModel, String> {
    // Find the latest refinement_start entry.
    let revisions = repo
        .revisions(proposal_id)
        .await
        .map_err(|e| format!("failed to read revisions: {e}"))?;

    let latest_start = revisions
        .iter()
        .rev()
        .find(|r| r.event_kind == "refinement_start");

    let Some(_start_rev) = latest_start else {
        // No refinement started.
        return Ok(ProposalRefinementStatusModel {
            active: false,
            current_round: None,
            dry_rounds: 0,
            total_entries: 0,
            stop_reason: None,
            awaiting_review: false,
            judge_summary: None,
            snapshot_revision_seq: None,
            needs_evidence: None,
            evidence_lifecycle_state: EvidenceLifecycleState::Active,
        });
    };

    // Check if there's a refinement_stop after this start.
    let stop_after = revisions
        .iter()
        .rev()
        .find(|r| r.event_kind == "refinement_stop");

    let is_active = match (&stop_after, &latest_start) {
        (Some(stop), Some(start)) => stop.created_at <= start.created_at,
        _ => true,
    };

    // Read stop reason from stop metadata (if stopped).
    // The coordinator's persist_refinement_stop writes `reason_tag`, while
    // the refinement_start error handler may write `reason`. Try both.
    let stop_reason = if !is_active {
        stop_after
            .and_then(|s| s.event_metadata.as_ref())
            .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
            .and_then(|v| {
                v.get("reason_tag")
                    .or_else(|| v.get("stop_reason"))
                    .or_else(|| v.get("reason"))
                    .and_then(|r| r.as_str().map(String::from))
            })
    } else {
        None
    };

    // Detect the parked "awaiting human review" state: the autonomous tribunal
    // records a `refinement_awaiting_review` lifecycle event when it converges,
    // and the human's resolve records a `refinement_stop` after it.
    let latest_awaiting = revisions
        .iter()
        .rev()
        .find(|r| r.event_kind == "refinement_awaiting_review");
    let awaiting_review = match (&latest_awaiting, &stop_after, &latest_start) {
        (Some(aw), Some(stop), Some(start)) => {
            start.created_at <= aw.created_at && stop.created_at < aw.created_at
        }
        (Some(aw), None, Some(start)) => start.created_at <= aw.created_at,
        _ => false,
    };
    let (judge_summary, snapshot_revision_seq) = if awaiting_review {
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
    } else if is_active {
        EvidenceLifecycleState::Active
    } else {
        // Refinement stopped but proposal is not terminal.
        EvidenceLifecycleState::Active
    };

    Ok(ProposalRefinementStatusModel {
        active: is_active,
        current_round: Some(current_round),
        dry_rounds,
        total_entries,
        stop_reason,
        awaiting_review,
        judge_summary,
        snapshot_revision_seq,
        needs_evidence,
        evidence_lifecycle_state,
    })
}

/// Check if refinement is currently active for a proposal.
pub(crate) async fn refinement_is_active(repo: &ProposalRepository, proposal_id: &str) -> bool {
    build_refinement_status(repo, proposal_id)
        .await
        .map(|s| s.active)
        .unwrap_or(false)
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
