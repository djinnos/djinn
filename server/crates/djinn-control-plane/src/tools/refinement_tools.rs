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
    DemandRoundResponse, NeedsEvidenceDemandResponse, NeedsEvidenceDemandResult,
    ProposalRefinementStartResponse, ProposalRefinementStatusModel,
    ProposalRefinementStatusResponse, VerdictOverrideResponse,
};
pub use crate::tools::refinement_helpers::{
    ProposalRefinementDemandEvidenceParams, build_refinement_status,
};
use crate::tools::refinement_helpers::{refinement_is_active, validate_demand_evidence};
use djinn_core::models::{NeedsEvidenceClaim, TaskStatus, TransitionAction};
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

fn err_demand_evidence(error: impl Into<String>) -> NeedsEvidenceDemandResponse {
    NeedsEvidenceDemandResponse {
        proposal_id: None,
        accepted: false,
        result: None,
        error: Some(error.into()),
    }
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

        // Refinement dispatches tribunal tasks into the proposal's target
        // project (see create_refinement_task_with_context, which reads
        // targets[0].project_id). With no target the coordinator silently
        // fails task creation and terminates with an opaque agent_failure and
        // zero entries — so reject fast here with an actionable message.
        match repo.targets(&proposal.id).await {
            Ok(targets) if !targets.is_empty() => {}
            Ok(_) => {
                return Json(err_refinement_start(
                    "proposal has no target project; add one with proposal_add_target \
                     before starting refinement"
                        .to_string(),
                ));
            }
            Err(e) => {
                return Json(err_refinement_start(format!(
                    "failed to check proposal targets: {e}"
                )));
            }
        }

        // Lifecycle-level duplicate check — fast-path early return before
        // hitting the coordinator channel.
        if refinement_is_active(&repo, &proposal.id).await {
            return Json(err_refinement_start(
                "refinement is already active for this proposal".to_string(),
            ));
        }

        let owner_user_id = p
            .owner_user_id
            .clone()
            .or_else(|| proposal.author_user_id.clone())
            .filter(|id| !id.trim().is_empty());
        let Some(owner_user_id) = owner_user_id else {
            return Json(err_refinement_start(
                "effective_creator_unavailable: refinement owner could not be resolved",
            ));
        };
        match djinn_db::UserRepository::new(self.state.db().clone())
            .get_by_id(&owner_user_id)
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Json(err_refinement_start(
                    "effective_creator_unavailable: refinement owner does not exist",
                ));
            }
            Err(e) => {
                return Json(err_refinement_start(format!(
                    "effective_creator_unavailable: failed to resolve refinement owner: {e}"
                )));
            }
        }
        // Owner update and lifecycle start share a transaction; FK races roll back both.
        match repo
            .start_refinement_with_owner(&proposal.id, &owner_user_id, None)
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
                    owner_user_id: Some(owner_user_id.clone()),
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
            owner_user_id: Some(owner_user_id),
            current_round: Some(1),
            dry_rounds: 0,
            total_entries: 0,
            stop_reason: None,
            awaiting_review: false,
            judge_summary: None,
            snapshot_revision_seq: None,
            needs_evidence: None,
            evidence_lifecycle_state: crate::tools::proposal_ops::EvidenceLifecycleState::Active,
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

        // Same missing-target blind spot as refinement_start: a demanded round
        // dispatches a fresh tribunal task, which needs a target project. Reject
        // fast here rather than terminating with an opaque agent_failure. Checked
        // after the duplicate-active guard so a genuinely-active proposal still
        // reports "already active" first.
        match repo.targets(&proposal.id).await {
            Ok(targets) if !targets.is_empty() => {}
            Ok(_) => {
                return Json(DemandRoundResponse {
                    proposal_id: Some(proposal.id),
                    accepted: false,
                    refinement: None,
                    error: Some(
                        "proposal has no target project; add one with proposal_add_target \
                         before demanding a refinement round"
                            .to_string(),
                    ),
                });
            }
            Err(e) => {
                return Json(DemandRoundResponse {
                    proposal_id: Some(proposal.id),
                    accepted: false,
                    refinement: None,
                    error: Some(format!("failed to check proposal targets: {e}")),
                });
            }
        }

        // A demanded run retains the durable owner; legacy proposals may use
        // their author once, but still must resolve a concrete owner before a
        // lifecycle row or coordinator spawn is possible.
        let Some(owner_user_id) = proposal
            .refinement_owner_user_id
            .clone()
            .or_else(|| proposal.author_user_id.clone())
            .filter(|id| !id.trim().is_empty())
        else {
            return Json(DemandRoundResponse {
                proposal_id: Some(proposal.id),
                accepted: false,
                refinement: None,
                error: Some(
                    "effective_creator_unavailable: refinement owner could not be resolved".into(),
                ),
            });
        };
        let reviewer_feedback = p.reason.clone();
        let demand_metadata = serde_json::json!({ "source": "human_demand_round", "reason": reviewer_feedback, "reviewer_feedback": reviewer_feedback });
        if let Err(e) = repo
            .start_refinement_with_owner(&proposal.id, &owner_user_id, Some(&demand_metadata))
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
                    // Demand-round uses the persisted owner.
                    owner_user_id: Some(owner_user_id.clone()),
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
            owner_user_id: Some(owner_user_id),
            current_round: Some(1),
            dry_rounds: 0,
            total_entries: 0,
            stop_reason: None,
            awaiting_review: false,
            judge_summary: None,
            snapshot_revision_seq: None,
            needs_evidence: None,
            evidence_lifecycle_state: crate::tools::proposal_ops::EvidenceLifecycleState::Active,
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

#[cfg(test)]
mod tests;
