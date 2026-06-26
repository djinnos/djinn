// MCP tools for proposal refinement kickoff and status.
//
// The refinement workflow coordinates Advocate, Adversary, and Judge roles
// through bounded debate rounds. These tools expose the minimal control-plane
// surfaces: starting refinement with an update-authority mode, and reading
// the current refinement status derived from debate-trail entries.
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
    DemandRoundResponse, NeedsEvidenceStatus, ProposalRefinementStartResponse,
    ProposalRefinementStatusModel, ProposalRefinementStatusResponse, VerdictOverrideResponse,
};
use djinn_db::ProposalRepository;
use djinn_db::TaskRepository;

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
    /// Refinement is always checkpoint-gated: the advocate's revisions are
    /// proposed for explicit approval/rejection and never auto-applied.
    /// Same-model fallback is allowed when diverse models are unavailable —
    /// this is not presented as an error.
    #[tool(
        description = "Start proposal refinement for the given proposal. Validates the proposal exists and is in draft or in_review state. Records a refinement_start lifecycle event and delegates to the coordinator to initialize the runtime refinement loop. Refinement is always checkpoint-gated: the advocate's revisions are proposed for explicit approval/rejection and never auto-applied. Returns the initial refinement status. Same-model fallback is used when diverse models are unavailable."
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

        // Record refinement_start lifecycle entry. Refinement is always
        // checkpoint-gated; the field is retained for status-display compat.
        let metadata = json!({
            "update_authority": "checkpoint",
        });
        match repo
            .record_refinement_lifecycle(&proposal.id, "refinement_start", Some(&metadata))
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
            update_authority: "checkpoint".to_string(),
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
        description = "Read proposal refinement status. Returns active flag, current round, dry-round count, total entries, update_authority (always `checkpoint`), and stop_reason if refinement has ended. Derived from refinement lifecycle events and debate-trail entries."
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
    /// stopped (e.g. after a judge verdict, round cap, or spawn cap).
    /// Reuses the existing coordinator refinement loop — clears the stop
    /// state and re-enqueues an Advocate→Adversary→Judge cycle.
    /// Records the demand action in proposal history.
    #[tool(
        description = "Demand another tribunal round for a proposal whose refinement has stopped (e.g. after a judge verdict or round cap). Reuses the existing coordinator refinement loop. Records the action in proposal history. Returns an error if refinement is still active."
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

        // Only allow demanding a round for proposals in draft or in_review.
        if !matches!(proposal.status.as_str(), "draft" | "in_review") {
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

        // Record the demand-round action as a lifecycle event. Refinement is
        // always checkpoint-gated; the field is retained for status-display compat.
        let demand_metadata = serde_json::json!({
            "source": "human_demand_round",
            "reason": p.reason,
            "update_authority": "checkpoint",
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
            update_authority: "checkpoint".to_string(),
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

    let Some(start_rev) = latest_start else {
        // No refinement started.
        return Ok(ProposalRefinementStatusModel {
            active: false,
            current_round: None,
            dry_rounds: 0,
            total_entries: 0,
            update_authority: "checkpoint".to_string(),
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

    // Read update_authority from start metadata.
    let update_authority = start_rev
        .event_metadata
        .as_ref()
        .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
        .and_then(|v| v.get("update_authority")?.as_str().map(String::from))
        .unwrap_or_else(|| "checkpoint".to_string());

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
    let awaiting_review = match (&latest_awaiting, &stop_after) {
        (Some(aw), Some(stop)) => stop.created_at < aw.created_at,
        (Some(_), None) => true,
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
        let claim = proposal
            .as_ref()
            .and_then(|p| p.needs_evidence_claim.clone())
            .unwrap_or_default();
        Some(NeedsEvidenceStatus {
            claim,
            spike_task_id: spike_id.to_string(),
            spike_short_id: spike
                .as_ref()
                .map(|t| t.short_id.clone())
                .unwrap_or_default(),
            spike_status: spike
                .as_ref()
                .map(|t| t.status.clone())
                .unwrap_or_else(|| "unknown".to_string()),
        })
    } else {
        None
    };

    Ok(ProposalRefinementStatusModel {
        active: is_active,
        current_round: Some(current_round),
        dry_rounds,
        total_entries,
        update_authority,
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

#[cfg(test)]
mod tests;
