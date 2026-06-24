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
    CheckpointApproveResponse, CheckpointListResponse, CheckpointRejectResponse,
    CheckpointRevisionModel, NeedsEvidenceStatus, ProposalRefinementStartResponse,
    ProposalRefinementStatusModel, ProposalRefinementStatusResponse,
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
    /// Update authority mode: `checkpoint` (advocate revisions are proposed
    /// but not auto-applied) or `auto_accept` (revisions are applied as
    /// proposal updates). Defaults to `checkpoint`.
    #[serde(default = "default_update_authority")]
    pub update_authority: String,
}

fn default_update_authority() -> String {
    "checkpoint".to_string()
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalRefinementStatusParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CheckpointListParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CheckpointApproveParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
    /// Revision sequence number to approve.
    pub revision_seq: i32,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CheckpointRejectParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
    /// Revision sequence number to reject.
    pub revision_seq: i32,
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
    /// `update_authority` controls whether advocate revisions are applied
    /// automatically (`auto_accept`) or proposed for approval (`checkpoint`,
    /// the default). Same-model fallback is allowed when diverse models are
    /// unavailable — this is not presented as an error.
    #[tool(
        description = "Start proposal refinement for the given proposal. Validates the proposal exists and is in draft or in_review state. Records a refinement_start lifecycle event and delegates to the coordinator to initialize the runtime refinement loop. `update_authority` is `checkpoint` (default — advocate revisions require approval) or `auto_accept` (revisions are applied automatically). Returns the initial refinement status. Same-model fallback is used when diverse models are unavailable."
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

        // Validate update_authority.
        let authority = p.update_authority.as_str();
        if !matches!(authority, "checkpoint" | "auto_accept") {
            return Json(err_refinement_start(format!(
                "invalid update_authority: {authority:?} (expected checkpoint or auto_accept)"
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
        let metadata = json!({
            "update_authority": authority,
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
                    update_authority: authority.to_string(),
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
            update_authority: authority.to_string(),
            stop_reason: None,
            pending_checkpoint_count: 0,
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
        description = "Read proposal refinement status. Returns active flag, current round, dry-round count, total entries, update_authority (checkpoint or auto_accept), and stop_reason if refinement has ended. Derived from refinement lifecycle events and debate-trail entries."
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

    /// List pending checkpoint revisions for a proposal. Returns the
    /// pending advocate revisions that await human approval in checkpoint
    /// mode. In auto-accept mode the list is always empty.
    #[tool(
        description = "List pending checkpoint revisions for a proposal. Returns advocate revisions that await approval in checkpoint mode. Each entry includes the revision seq, round, author model, title, and a body preview. Empty list in auto-accept mode."
    )]
    pub async fn proposal_refinement_checkpoint_list(
        &self,
        Parameters(p): Parameters<CheckpointListParams>,
    ) -> Json<CheckpointListResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(CheckpointListResponse {
                proposal_id: None,
                pending: vec![],
                error: Some(format!("proposal not found: {}", p.proposal_id)),
            });
        };

        match repo.pending_checkpoint_revisions(&proposal.id).await {
            Ok(revisions) => {
                let pending = revisions
                    .iter()
                    .map(CheckpointRevisionModel::from_revision)
                    .collect();
                Json(CheckpointListResponse {
                    proposal_id: Some(proposal.id),
                    pending,
                    error: None,
                })
            }
            Err(e) => Json(CheckpointListResponse {
                proposal_id: Some(proposal.id),
                pending: vec![],
                error: Some(format!("failed to list pending revisions: {e}")),
            }),
        }
    }

    /// Approve a pending checkpoint revision: apply its title, body, and
    /// acceptance criteria to the live proposal. The revision row is marked
    /// as `checkpoint_approved` for audit. Idempotent — no-op if already
    /// approved or rejected.
    #[tool(
        description = "Approve a pending checkpoint revision. Applies the revision's body, title, and acceptance criteria to the live proposal. Marks the revision as checkpoint_approved. Idempotent — returns success even if the revision was already approved or rejected."
    )]
    pub async fn proposal_refinement_checkpoint_approve(
        &self,
        Parameters(p): Parameters<CheckpointApproveParams>,
    ) -> Json<CheckpointApproveResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(CheckpointApproveResponse {
                proposal_id: None,
                approved: false,
                error: Some(format!("proposal not found: {}", p.proposal_id)),
            });
        };

        let user_id = djinn_core::auth_context::current_user_id();

        match repo
            .approve_checkpoint_revision(&proposal.id, p.revision_seq, user_id.as_deref())
            .await
        {
            Ok(_) => Json(CheckpointApproveResponse {
                proposal_id: Some(proposal.id),
                approved: true,
                error: None,
            }),
            Err(e) => Json(CheckpointApproveResponse {
                proposal_id: Some(proposal.id),
                approved: false,
                error: Some(format!("failed to approve checkpoint revision: {e}")),
            }),
        }
    }

    /// Reject a pending checkpoint revision: mark it as `checkpoint_rejected`
    /// without modifying the live proposal body. Idempotent — no-op if
    /// already approved or rejected.
    #[tool(
        description = "Reject a pending checkpoint revision. Marks the revision as checkpoint_rejected without modifying the live proposal body. Idempotent — returns success even if the revision was already approved or rejected."
    )]
    pub async fn proposal_refinement_checkpoint_reject(
        &self,
        Parameters(p): Parameters<CheckpointRejectParams>,
    ) -> Json<CheckpointRejectResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(CheckpointRejectResponse {
                proposal_id: None,
                rejected: false,
                error: Some(format!("proposal not found: {}", p.proposal_id)),
            });
        };

        let user_id = djinn_core::auth_context::current_user_id();

        match repo
            .reject_checkpoint_revision(&proposal.id, p.revision_seq, user_id.as_deref())
            .await
        {
            Ok(_) => Json(CheckpointRejectResponse {
                proposal_id: Some(proposal.id),
                rejected: true,
                error: None,
            }),
            Err(e) => Json(CheckpointRejectResponse {
                proposal_id: Some(proposal.id),
                rejected: false,
                error: Some(format!("failed to reject checkpoint revision: {e}")),
            }),
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

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
            pending_checkpoint_count: 0,
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

    // Count pending checkpoint revisions.
    let pending_checkpoint_count = if update_authority == "checkpoint" {
        repo.pending_checkpoint_revisions(proposal_id)
            .await
            .map(|v| v.len() as i32)
            .unwrap_or(0)
    } else {
        0
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
        pending_checkpoint_count,
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
