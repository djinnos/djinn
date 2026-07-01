// MCP tools for proposal refinement kickoff and status.
//
// The refinement workflow coordinates Advocate, Adversary, and Judge roles
// through bounded debate rounds. These tools expose the minimal control-plane
// surfaces: starting refinement, and reading the current refinement status
// derived from debate-trail entries.
//
// Refinement state is tracked via lightweight `proposal_revisions` lifecycle
// entries (`event_kind = "refinement_start"` / `"refinement_stop"`) with
// structured `event_metadata`. Current round and dry-round counts are derived
// from the debate trail at query time.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use crate::bridge::ProposalRefinementStartRequest;
use crate::server::DjinnMcpServer;
use crate::tools::proposal_ops::{
    DemandRoundResponse, EvidenceLifecyclePhase, NeedsEvidenceDemandResponse,
    NeedsEvidenceDemandResult, NeedsEvidenceStatus, ProposalRefinementStartResponse,
    ProposalRefinementStatusModel, ProposalRefinementStatusResponse, VerdictOverrideResponse,
};
use djinn_core::models::NeedsEvidenceClaim;
use djinn_core::models::{TaskStatus, TransitionAction};
use djinn_db::{
    NeedsEvidenceClaimLink, ProposalDebateTrailCreateInput, ProposalRepository, TaskRepository,
};

fn err_refinement_start(error: impl Into<String>) -> ProposalRefinementStartResponse {
    ProposalRefinementStartResponse {
        proposal_id: None,
        refinement: None,
        error: Some(error.into()),
    }
}

fn err_refinement_status(error: impl Into<String>) -> ProposalRefinementStatusResponse {
    ProposalRefinementStatusResponse {
        proposal_id: None,
        refinement: None,
        error: Some(error.into()),
    }
}

// ── Param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalRefinementStartParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
    /// User the refinement run is attributed to: owner of the spawned
    /// refinement (tribunal) tasks and the scope for per-user role-model
    /// resolution. Omit to attribute the run to the proposal author.
    #[serde(default)]
    pub owner_user_id: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalRefinementStatusParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalRefinementDemandRoundParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
    /// Why another round is being demanded. Recorded in proposal history.
    pub reason: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalRefinementResolveParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
    /// The human's decision: `accept` (keep the refined spec) or `reject`
    /// (revert the live spec to the pre-refinement snapshot).
    pub decision: String,
    /// Optional reviewer note — why accepted/rejected. Recorded for the audit
    /// trail.
    #[serde(default)]
    pub feedback: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalVerdictOverrideParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
    /// Why the verdict is being overridden. Required — recorded in proposal
    /// history for audit.
    pub reason: String,
    /// Optional debate-trail entry id of the verdict being overridden.
    #[serde(default)]
    pub overridden_verdict_entry_id: Option<String>,
}

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

fn err_demand_evidence(error: impl Into<String>) -> NeedsEvidenceDemandResponse {
    NeedsEvidenceDemandResponse {
        proposal_id: None,
        accepted: false,
        result: None,
        error: Some(error.into()),
    }
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
async fn verify_active_judge_authorization(
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
    let task_owner = task.created_by_user_id.as_deref().unwrap_or("");
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
async fn validate_demand_evidence(
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

// ── Tool router ──────────────────────────────────────────────────────────────

#[tool_router(router = refinement_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Start proposal refinement. Validates the proposal is in a state that
    /// supports refinement (draft or in_review), records a `refinement_start`
    /// lifecycle entry, delegates to the coordinator to initialize the runtime
    /// refinement loop, and returns the initial refinement status.
    ///
    /// The coordinator is authoritative for duplicate-start rejection. If the
    /// coordinator rejects the start (e.g. duplicate active run), a
    /// `refinement_stop` lifecycle entry is recorded with the error reason.
    ///
    /// The autonomous tribunal (Adversary → Advocate → Judge) revises the spec
    /// in place and parks for a single human accept/reject when it converges.
    /// Same-model fallback is allowed when diverse models are unavailable —
    /// this is not presented as an error.
    #[tool(
        description = "Start proposal refinement for the given proposal. Validates the proposal exists and is in draft or in_review state. Records a refinement_start lifecycle event and delegates to the coordinator to initialize the runtime refinement loop. The autonomous tribunal revises the spec in place and parks for a single human accept/reject when the judge converges. Returns the initial refinement status. Same-model fallback is used when diverse models are unavailable."
    )]
    pub async fn proposal_refinement_start(
        &self,
        Parameters(p): Parameters<ProposalRefinementStartParams>,
    ) -> Json<ProposalRefinementStartResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(err_refinement_start(format!(
                "proposal not found: {}",
                p.proposal_id
            )));
        };

        // Only allow refinement for proposals in draft or in_review.
        if !matches!(proposal.status.as_str(), "draft" | "in_review") {
            return Json(err_refinement_start(format!(
                "proposal status '{}' does not support refinement (must be draft or in_review)",
                proposal.status
            )));
        }

        // Lifecycle-level duplicate check — fast-path early return before
        // hitting the coordinator channel.
        if refinement_is_active(&repo, &proposal.id).await {
            return Json(err_refinement_start(
                "refinement is already active for this proposal".to_string(),
            ));
        }

        // Record refinement_start lifecycle entry.
        match repo
            .record_refinement_lifecycle(&proposal.id, "refinement_start", None)
            .await
        {
            Ok(_) => {}
            Err(e) => {
                return Json(err_refinement_start(format!(
                    "failed to record refinement_start: {e}"
                )));
            }
        }

        // Delegate to the coordinator to start the runtime refinement loop.
        // The coordinator is authoritative for duplicate-start rejection — if
        // the coordinator has a live run for this proposal (e.g. race between
        // two MCP tool calls), it rejects the start and we record a
        // refinement_stop lifecycle entry to close the dangling start.
        let coordinator = self.state.coordinator().await;
        match coordinator {
            Some(coordinator_handle) => {
                let request = ProposalRefinementStartRequest {
                    proposal_id: proposal.id.clone(),
                    current_revision_seq: proposal.latest_revision_seq,
                    // Attribute to the explicitly-chosen user, else the proposal
                    // author. This owns the tribunal tasks and scopes per-user
                    // model resolution (so refinement uses the attributed user's
                    // Plan-role models instead of a hardcoded fallback).
                    owner_user_id: p
                        .owner_user_id
                        .clone()
                        .or_else(|| proposal.author_user_id.clone()),
                };
                if let Err(e) = coordinator_handle.start_proposal_refinement(request).await {
                    let stop_metadata = json!({
                        "stop_reason": e,
                        "source": "coordinator_start_failure",
                    });
                    let _ = repo
                        .record_refinement_lifecycle(
                            &proposal.id,
                            "refinement_stop",
                            Some(&stop_metadata),
                        )
                        .await;
                    return Json(err_refinement_start(format!(
                        "coordinator rejected refinement start: {e}"
                    )));
                }
            }
            None => {
                // No coordinator wired — record the failure and return an
                // error.  This keeps the lifecycle entry from dangling without
                // runtime ownership.
                let stop_metadata = json!({
                    "stop_reason": "coordinator not available",
                    "source": "coordinator_start_failure",
                });
                let _ = repo
                    .record_refinement_lifecycle(
                        &proposal.id,
                        "refinement_stop",
                        Some(&stop_metadata),
                    )
                    .await;
                return Json(err_refinement_start(
                    "coordinator not available".to_string(),
                ));
            }
        }

        let refinement = ProposalRefinementStatusModel {
            active: true,
            current_round: Some(1),
            dry_rounds: 0,
            total_entries: 0,
            stop_reason: None,
            awaiting_review: false,
            judge_summary: None,
            snapshot_revision_seq: None,
            needs_evidence: None,
        };

        Json(ProposalRefinementStartResponse {
            proposal_id: Some(proposal.id),
            refinement: Some(refinement),
            error: None,
        })
    }

    /// Read the current refinement status for a proposal. Returns whether
    /// refinement is active, the current round, dry-round count, total
    /// debate-trail entries, update-authority mode, and stop reason (if any).
    ///
    /// Status is derived from the refinement lifecycle events and debate trail.
    /// Returns an empty (inactive) status when refinement has not been started.
    #[tool(
        description = "Read proposal refinement status. Returns active flag, current round, dry-round count, total entries, and stop_reason if refinement has ended. Derived from refinement lifecycle events and debate-trail entries."
    )]
    pub async fn proposal_refinement_status(
        &self,
        Parameters(p): Parameters<ProposalRefinementStatusParams>,
    ) -> Json<ProposalRefinementStatusResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(err_refinement_status(format!(
                "proposal not found: {}",
                p.proposal_id
            )));
        };

        match build_refinement_status(&repo, &proposal.id).await {
            Ok(refinement) => Json(ProposalRefinementStatusResponse {
                proposal_id: Some(proposal.id),
                refinement: Some(refinement),
                error: None,
            }),
            Err(e) => Json(err_refinement_status(e)),
        }
    }

    /// Demand another tribunal round for a proposal whose refinement has
    /// stopped (e.g. after a judge verdict, round cap, or spawn cap) or is
    /// parked awaiting human review.
    /// Reuses the existing coordinator refinement loop — clears the stop or
    /// parked review state and re-enqueues an Advocate→Adversary→Judge cycle.
    /// Records the demand action in proposal history.
    #[tool(
        description = "Demand another tribunal round for a proposal whose refinement has stopped (e.g. after a judge verdict or round cap) or is parked awaiting human review. Reuses the existing coordinator refinement loop. Records the action in proposal history. Returns an error if refinement is still actively running."
    )]
    pub async fn proposal_refinement_demand_round(
        &self,
        Parameters(p): Parameters<ProposalRefinementDemandRoundParams>,
    ) -> Json<DemandRoundResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(DemandRoundResponse {
                proposal_id: None,
                accepted: false,
                refinement: None,
                error: Some(format!("proposal not found: {}", p.proposal_id)),
            });
        };

        let current_refinement = match build_refinement_status(&repo, &proposal.id).await {
            Ok(status) => status,
            Err(e) => {
                return Json(DemandRoundResponse {
                    proposal_id: Some(proposal.id),
                    accepted: false,
                    refinement: None,
                    error: Some(e),
                });
            }
        };

        // Only allow demanding a round for proposals in draft or in_review,
        // except for the explicit human-review path: a converged tribunal parks
        // in `awaiting_review` while the latest start remains lifecycle-active,
        // and a human may demand another round from that parked state.
        if !matches!(proposal.status.as_str(), "draft" | "in_review")
            && !current_refinement.awaiting_review
        {
            return Json(DemandRoundResponse {
                proposal_id: Some(proposal.id),
                accepted: false,
                refinement: None,
                error: Some(format!(
                    "proposal status '{}' does not support demanding a refinement round (must be draft or in_review)",
                    proposal.status
                )),
            });
        }

        // Protect against true duplicate active loops. Parked awaiting-review
        // refinements are intentionally allowed: the fresh refinement_start
        // below transitions the lifecycle back into an active rerun and the
        // coordinator demand path is invoked exactly once.
        if current_refinement.active && !current_refinement.awaiting_review {
            return Json(DemandRoundResponse {
                proposal_id: Some(proposal.id),
                accepted: false,
                refinement: Some(current_refinement),
                error: Some("refinement is already active for this proposal".to_string()),
            });
        }

        // Record the demand-round action as a lifecycle event.
        let reviewer_feedback = p.reason.clone();
        let demand_metadata = serde_json::json!({
            "source": "human_demand_round",
            "reason": reviewer_feedback,
            "reviewer_feedback": reviewer_feedback,
        });
        if let Err(e) = repo
            .record_refinement_lifecycle(&proposal.id, "refinement_start", Some(&demand_metadata))
            .await
        {
            return Json(DemandRoundResponse {
                proposal_id: Some(proposal.id),
                accepted: false,
                refinement: None,
                error: Some(format!("failed to record demand-round event: {e}")),
            });
        }

        // Delegate to the coordinator.
        let coordinator = self.state.coordinator().await;
        match coordinator {
            Some(coordinator_handle) => {
                let request = ProposalRefinementStartRequest {
                    proposal_id: proposal.id.clone(),
                    current_revision_seq: proposal.latest_revision_seq,
                    // Demand-round reuses the proposal author for attribution.
                    owner_user_id: proposal.author_user_id.clone(),
                };
                if let Err(e) = coordinator_handle
                    .demand_proposal_refinement_round(request)
                    .await
                {
                    let _ = repo
                        .record_refinement_lifecycle(
                            &proposal.id,
                            "refinement_stop",
                            Some(&serde_json::json!({
                                "stop_reason": e,
                                "source": "demand_round_failure",
                            })),
                        )
                        .await;
                    return Json(DemandRoundResponse {
                        proposal_id: Some(proposal.id),
                        accepted: false,
                        refinement: None,
                        error: Some(format!("coordinator rejected demand: {e}")),
                    });
                }
            }
            None => {
                let _ = repo
                    .record_refinement_lifecycle(
                        &proposal.id,
                        "refinement_stop",
                        Some(&serde_json::json!({
                            "stop_reason": "coordinator not available",
                            "source": "demand_round_failure",
                        })),
                    )
                    .await;
                return Json(DemandRoundResponse {
                    proposal_id: Some(proposal.id),
                    accepted: false,
                    refinement: None,
                    error: Some("coordinator not available".to_string()),
                });
            }
        }

        let refinement = ProposalRefinementStatusModel {
            active: true,
            current_round: Some(1),
            dry_rounds: 0,
            total_entries: 0,
            stop_reason: None,
            awaiting_review: false,
            judge_summary: None,
            snapshot_revision_seq: None,
            needs_evidence: None,
        };

        Json(DemandRoundResponse {
            proposal_id: Some(proposal.id),
            accepted: true,
            refinement: Some(refinement),
            error: None,
        })
    }

    #[tool(
        description = "Resolve the human's single accept/reject review of a converged proposal refinement (the autonomous Adversary→Advocate→Judge tribunal parks for one human review when it converges). `decision` is `accept` (keep the refined spec) or `reject` (revert the live spec to the pre-refinement snapshot). Optional `feedback` is recorded. Errors if no refinement is parked awaiting review."
    )]
    pub async fn proposal_refinement_resolve(
        &self,
        Parameters(p): Parameters<ProposalRefinementResolveParams>,
    ) -> Json<crate::tools::proposal_ops::ResolveReviewResponse> {
        use crate::tools::proposal_ops::ResolveReviewResponse;
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(ResolveReviewResponse {
                proposal_id: None,
                resolved: false,
                error: Some(format!("proposal not found: {}", p.proposal_id)),
            });
        };

        let accept = match p.decision.as_str() {
            "accept" => true,
            "reject" => false,
            other => {
                return Json(ResolveReviewResponse {
                    proposal_id: Some(proposal.id),
                    resolved: false,
                    error: Some(format!(
                        "invalid decision: {other:?} (expected `accept` or `reject`)"
                    )),
                });
            }
        };

        match self.state.coordinator().await {
            Some(handle) => {
                if let Err(e) = handle
                    .resolve_refinement_review(proposal.id.clone(), accept, p.feedback.clone())
                    .await
                {
                    return Json(ResolveReviewResponse {
                        proposal_id: Some(proposal.id),
                        resolved: false,
                        error: Some(format!("coordinator could not resolve review: {e}")),
                    });
                }
            }
            None => {
                return Json(ResolveReviewResponse {
                    proposal_id: Some(proposal.id),
                    resolved: false,
                    error: Some("coordinator not available".to_string()),
                });
            }
        }

        Json(ResolveReviewResponse {
            proposal_id: Some(proposal.id),
            resolved: true,
            error: None,
        })
    }

    /// Override a latest `needs-work` verdict with auditable sign-off/approval
    /// metadata. The override is scoped to the current revision — later edits
    /// that advance the proposal revision make the override stale, preventing
    /// silent inheritance by future revisions.
    #[tool(
        description = "Override a judge needs-work verdict with auditable approval metadata. The override is scoped to the current proposal revision — later spec edits that advance the revision make it stale. Records who overrode, when, and why in proposal history."
    )]
    pub async fn proposal_verdict_override(
        &self,
        Parameters(p): Parameters<ProposalVerdictOverrideParams>,
    ) -> Json<VerdictOverrideResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(VerdictOverrideResponse {
                proposal_id: None,
                overridden: false,
                override_on_revision_seq: None,
                error: Some(format!("proposal not found: {}", p.proposal_id)),
            });
        };

        if p.reason.trim().is_empty() {
            return Json(VerdictOverrideResponse {
                proposal_id: Some(proposal.id),
                overridden: false,
                override_on_revision_seq: None,
                error: Some("override reason must not be empty".to_string()),
            });
        }

        // Record the override as a lifecycle event scoped to current revision.
        let override_seq = proposal.latest_revision_seq;
        let user_id = djinn_core::auth_context::current_user_id();
        let override_metadata = serde_json::json!({
            "source": "human_verdict_override",
            "override_by": user_id,
            "override_reason": p.reason,
            "override_on_revision_seq": override_seq,
            "overridden_verdict_entry_id": p.overridden_verdict_entry_id,
        });

        if let Err(e) = repo
            .record_refinement_lifecycle(&proposal.id, "verdict_override", Some(&override_metadata))
            .await
        {
            return Json(VerdictOverrideResponse {
                proposal_id: Some(proposal.id),
                overridden: false,
                override_on_revision_seq: None,
                error: Some(format!("failed to record verdict override: {e}")),
            });
        }

        Json(VerdictOverrideResponse {
            proposal_id: Some(proposal.id),
            overridden: true,
            override_on_revision_seq: Some(override_seq),
            error: None,
        })
    }

    /// Demand a read-only evidence spike for an insufficiently-evidenced
    /// feasibility claim. The Judge calls this when in-session research is
    /// not enough to resolve a load-bearing claim in the spec.
    ///
    /// Validates the caller is the active Judge, the proposal is not terminal,
    /// the refinement round/revision match, the question is falsifiable, the
    /// spec anchor exists, and the per-run needs-evidence cap is not exhausted.
    /// On acceptance: creates a single read-only evidence spike task (Architect
    /// routing), writes a `needs_evidence` debate-trail entry with structured
    /// `NeedsEvidenceClaimLink` metadata, links the spike to the proposal via
    /// `set_structured_needs_evidence_spike`, parks the proposal, and writes a
    /// `refinement_awaiting_evidence_started` lifecycle event.
    ///
    /// Race protection: concurrent valid demands cannot produce two open
    /// spikes because `set_structured_needs_evidence_spike` atomically sets
    /// `linked_spike_task_id` only when the column is NULL. The loser's
    /// spike task is closed, and a conflict response is returned.
    #[tool(
        description = "Demand a read-only evidence spike for an insufficiently-evidenced feasibility claim. The Judge calls this when in-session research cannot resolve a load-bearing claim. Validates the proposal and refinement state, checks the per-run needs-evidence cap, records the structured claim, creates a linked read-only spike task for the Architect, writes a needs_evidence debate entry, and parks refinement until spike findings arrive."
    )]
    pub async fn proposal_refinement_demand_evidence(
        &self,
        Parameters(p): Parameters<ProposalRefinementDemandEvidenceParams>,
    ) -> Json<NeedsEvidenceDemandResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(err_demand_evidence(format!(
                "proposal not found: {}",
                p.proposal_id
            )));
        };

        // Build refinement status for validation.
        let refinement = match build_refinement_status(&repo, &proposal.id).await {
            Ok(status) => status,
            Err(e) => {
                return Json(NeedsEvidenceDemandResponse {
                    proposal_id: Some(proposal.id),
                    accepted: false,
                    result: None,
                    error: Some(e),
                });
            }
        };

        // Run the full validation gate before any mutation. This ensures
        // no-mutation-on-reject: no lifecycle events, no debate entries, no
        // proposal fields are touched when the demand is invalid.
        // Returns the Judge task id on success.
        let judge_task_id =
            match validate_demand_evidence(&repo, &task_repo, &proposal, &refinement, &p).await {
                Ok(id) => id,
                Err(e) => {
                    return Json(NeedsEvidenceDemandResponse {
                        proposal_id: Some(proposal.id),
                        accepted: false,
                        result: None,
                        error: Some(e),
                    });
                }
            };

        // Build the structured claim for persistence. The claim JSON is stored
        // on the proposal alongside the spike task id.
        let claim = NeedsEvidenceClaim {
            question: p.question.clone(),
            target_subsystem: p.target_subsystem.clone(),
            spec_unknown_anchor: p.spec_unknown_anchor.clone(),
            insufficient_in_session_research: p.insufficient_in_session_research.clone(),
            expected_findings: p.expected_findings.clone(),
            round: p.round,
            against_revision_seq: p.against_revision_seq,
            created_by_task_id: judge_task_id.clone(),
        };

        // ── Step 1: Resolve project_id for the spike task ────────────────
        let project_id = match repo.targets(&proposal.id).await {
            Ok(targets) if !targets.is_empty() => targets[0].project_id.clone(),
            _ => {
                return Json(NeedsEvidenceDemandResponse {
                    proposal_id: Some(proposal.id),
                    accepted: false,
                    result: None,
                    error: Some(
                        "proposal has no target project; cannot create evidence spike task"
                            .to_string(),
                    ),
                });
            }
        };

        // ── Step 2: Create the evidence spike task ───────────────────────
        let spike_title = format!("Evidence spike: {}", p.question.trim());
        let spike_description = format!(
            "## Evidence Spike\n\n\
             **Proposal:** {proposal_id} (short_id: {short_id})\n\
             **Question:** {question}\n\
             **Target subsystem:** {target_subsystem}\n\
             **Spec unknown anchor:** {spec_unknown_anchor}\n\
             **Insufficiency rationale:** {insufficiency}\n\
             **Expected findings:** {expected_findings}\n\n\
             ### Read-Only Constraints\n\n\
             This is a **read-only** evidence investigation. The spike must:\n\
             - Only read and analyze existing code, docs, and specs.\n\
             - NOT modify, create, or delete any production files.\n\
             - Produce structured findings as evidence for the Judge.\n\
             - Return findings via the evidence_findings debate-trail entry.",
            proposal_id = proposal.id,
            short_id = proposal.short_id,
            question = p.question.trim(),
            target_subsystem = p.target_subsystem.trim(),
            spec_unknown_anchor = p.spec_unknown_anchor.trim(),
            insufficiency = p.insufficient_in_session_research.trim(),
            expected_findings = p.expected_findings.trim(),
        );

        let labels = vec![
            "refinement-evidence".to_string(),
            "read-only".to_string(),
            format!("proposal:{}", proposal.short_id),
        ];

        let spike_task = match task_repo
            .create_in_project(
                &project_id,
                None, // no epic parent
                &spike_title,
                &spike_description,
                "", // no design field
                "spike",
                0,  // default priority
                "", // no owner
                Some("open"),
                None, // no acceptance criteria
            )
            .await
        {
            Ok(task) => task,
            Err(e) => {
                return Json(NeedsEvidenceDemandResponse {
                    proposal_id: Some(proposal.id),
                    accepted: false,
                    result: None,
                    error: Some(format!("failed to create evidence spike task: {e}")),
                });
            }
        };

        // Set labels on the spike task.
        if let Err(e) = task_repo
            .update_labels(
                &spike_task.id,
                &serde_json::to_string(&labels).unwrap_or_else(|_| "[]".into()),
            )
            .await
        {
            // Best-effort: log but don't fail the entire demand.
            tracing::warn!(
                spike_task_id = %spike_task.id,
                error = %e,
                "failed to set labels on evidence spike task"
            );
        }

        // Set agent_type = "architect" for Architect routing.
        if let Err(e) = task_repo
            .update_agent_type(&spike_task.id, Some("architect"))
            .await
        {
            tracing::warn!(
                spike_task_id = %spike_task.id,
                error = %e,
                "failed to set agent_type on evidence spike task"
            );
        }

        // ── Step 3: Write needs_evidence debate-trail entry ──────────────
        let claim_link = NeedsEvidenceClaimLink::from_claim(&proposal.id, &spike_task.id, &claim);
        let claim_link_value = claim_link.to_value();

        if let Err(e) = repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &proposal.id,
                kind: "needs_evidence",
                body: &p.question,
                blocking: true,
                agent_role: "judge",
                author_kind: "agent",
                author_model: None,
                source_task_id: Some(&judge_task_id),
                against_revision_seq: p.against_revision_seq,
                round: p.round,
                body_metadata: Some(&claim_link_value),
            })
            .await
        {
            // Clean up the spike task on failure.
            let _ = task_repo.delete(&spike_task.id).await;
            return Json(NeedsEvidenceDemandResponse {
                proposal_id: Some(proposal.id),
                accepted: false,
                result: None,
                error: Some(format!("failed to record needs_evidence debate entry: {e}")),
            });
        }

        // ── Step 4: Link spike to proposal (race-safe atomic) ─────────
        //
        // `try_set_structured_needs_evidence_spike` atomically sets
        // `linked_spike_task_id` and `needs_evidence_claim` only when
        // `linked_spike_task_id IS NULL`. Returns `None` when a
        // concurrent demand already won the race.
        let link_result = repo
            .try_set_structured_needs_evidence_spike(&proposal.id, &spike_task.id, &claim)
            .await;

        match link_result {
            Ok(Some(_updated_proposal)) => {
                // We won the race — the spike is linked to the proposal.
            }
            Ok(None) => {
                // A concurrent demand already linked a spike (or the
                // proposal was deleted). Clean up our spike and report
                // the conflict.
                let _ = task_repo
                    .transition(
                        &spike_task.id,
                        TransitionAction::ForceClose,
                        "system",
                        "system",
                        Some("duplicate demand; superseded"),
                        Some(TaskStatus::Closed),
                    )
                    .await;
                // Re-read to get the winner's spike id for the error.
                let winner_id = repo
                    .get(&proposal.id)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|p| p.linked_spike_task_id.clone())
                    .unwrap_or_else(|| "unknown".to_string());
                return Json(NeedsEvidenceDemandResponse {
                    proposal_id: Some(proposal.id),
                    accepted: false,
                    result: None,
                    error: Some(format!(
                        "proposal already has an open linked evidence spike ({}); \
                         concurrent demand was rejected",
                        winner_id
                    )),
                });
            }
            Err(e) => {
                // The atomic UPDATE failed — likely a DB error. Clean up.
                let _ = task_repo.delete(&spike_task.id).await;
                return Json(NeedsEvidenceDemandResponse {
                    proposal_id: Some(proposal.id),
                    accepted: false,
                    result: None,
                    error: Some(format!("failed to link evidence spike to proposal: {e}")),
                });
            }
        }

        // ── Step 5: Record refinement_awaiting_evidence_started lifecycle ─
        if let Err(e) = repo
            .record_awaiting_evidence_started(
                &proposal.id,
                &spike_task.id,
                &judge_task_id,
                p.round,
                p.against_revision_seq,
            )
            .await
        {
            tracing::warn!(
                proposal_id = %proposal.id,
                spike_task_id = %spike_task.id,
                error = %e,
                "failed to record refinement_awaiting_evidence_started lifecycle event; \
                 spike is linked but lifecycle event is missing"
            );
        }

        Json(NeedsEvidenceDemandResponse {
            proposal_id: Some(proposal.id),
            accepted: true,
            result: Some(NeedsEvidenceDemandResult {
                claim: claim.question.clone(),
                spike_task_id: Some(spike_task.id.clone()),
                against_revision_seq: p.against_revision_seq,
                round: p.round,
            }),
            error: None,
        })
    }
}

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
        ) = if let Some(ref c) = parsed_claim {
            (
                c.question.clone(),
                Some(c.question.clone()),
                Some(c.target_subsystem.clone()),
                Some(c.spec_unknown_anchor.clone()),
                Some(c.round),
                Some(c.against_revision_seq),
                Some(c.created_by_task_id.clone()),
            )
        } else {
            // Legacy plain-string claim or empty.
            (raw_claim.to_string(), None, None, None, None, None, None)
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
            evidence_phase,
            failure_reason,
        })
    } else {
        None
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
    })
}

/// Check if refinement is currently active for a proposal.
async fn refinement_is_active(repo: &ProposalRepository, proposal_id: &str) -> bool {
    build_refinement_status(repo, proposal_id)
        .await
        .map(|s| s.active)
        .unwrap_or(false)
}

/// Check the Phase 1 needs-evidence cap for the current refinement run.
///
/// This is the primary control-plane helper that the Judge demand tool
/// (sibling epic `6tjy`) should call before creating/linking a spike.
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

#[cfg(test)]
mod tests;
