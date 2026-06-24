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
    CheckpointRevisionModel, ProposalRefinementStartResponse, ProposalRefinementStatusModel,
    ProposalRefinementStatusResponse,
};
use djinn_db::ProposalRepository;

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

    Ok(ProposalRefinementStatusModel {
        active: is_active,
        current_round: Some(current_round),
        dry_rounds,
        total_entries,
        update_authority,
        stop_reason,
        pending_checkpoint_count,
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
mod tests {
    use super::*;
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_core::events::EventBus;
    use djinn_db::{Database, ProposalCreateInput};
    use std::sync::Arc;

    async fn test_server() -> (DjinnMcpServer, Database) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        (DjinnMcpServer::new(test_mcp_state(db.clone())), db)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_start_creates_lifecycle_event() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Refinement Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None, // defaults to draft
                body_format: None,
            })
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .expect("tool should be registered");

        assert!(
            resp.get("error").and_then(|v| v.as_str()).is_none(),
            "expected no error, got: {:?}",
            resp.get("error")
        );
        let refinement = resp.get("refinement").expect("should have refinement");
        assert_eq!(
            refinement.get("active").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            refinement.get("update_authority").and_then(|v| v.as_str()),
            Some("checkpoint")
        );
        assert_eq!(
            refinement.get("current_round").and_then(|v| v.as_i64()),
            Some(1)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_start_with_auto_accept() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Auto Accept Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "update_authority": "auto_accept"
                }),
            )
            .await
            .expect("tool should be registered");

        let refinement = resp.get("refinement").expect("should have refinement");
        assert_eq!(
            refinement.get("update_authority").and_then(|v| v.as_str()),
            Some("auto_accept")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_start_rejects_invalid_authority() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Invalid Authority",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "update_authority": "invalid"
                }),
            )
            .await
            .expect("tool should be registered");

        let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(
            error.contains("invalid update_authority"),
            "should reject invalid authority: {error}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_start_rejects_building_proposal() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Building Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: Some("building"),
                body_format: None,
            })
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .expect("tool should be registered");

        let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(
            error.contains("does not support refinement"),
            "should reject building status: {error}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_status_returns_inactive_when_not_started() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "No Refinement",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_refinement_status",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .expect("tool should be registered");

        let refinement = resp.get("refinement").expect("should have refinement");
        assert_eq!(
            refinement.get("active").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            refinement.get("current_round").and_then(|v| v.as_i64()),
            None
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_status_reflects_debate_trail_rounds() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Debate Round Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: Some("in_review"),
                body_format: None,
            })
            .await
            .unwrap();

        // Start refinement.
        server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .unwrap();

        // Add a non-blocking objection in round 1.
        server
            .dispatch_tool(
                "proposal_debate_append",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "kind": "objection",
                    "body": "Minor issue",
                    "blocking": false,
                    "agent_role": "adversary",
                    "author_kind": "agent",
                    "against_revision_seq": 1,
                    "round": 1,
                }),
            )
            .await
            .unwrap();

        // Add a non-blocking objection in round 2.
        server
            .dispatch_tool(
                "proposal_debate_append",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "kind": "objection",
                    "body": "Another minor issue",
                    "blocking": false,
                    "agent_role": "adversary",
                    "author_kind": "agent",
                    "against_revision_seq": 1,
                    "round": 2,
                }),
            )
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_refinement_status",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .unwrap();

        let refinement = resp.get("refinement").expect("should have refinement");
        assert_eq!(
            refinement.get("active").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            refinement.get("current_round").and_then(|v| v.as_i64()),
            Some(2)
        );
        assert_eq!(
            refinement.get("total_entries").and_then(|v| v.as_i64()),
            Some(2)
        );
        // Both rounds are non-blocking → 2 dry rounds.
        assert_eq!(
            refinement.get("dry_rounds").and_then(|v| v.as_i64()),
            Some(2)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_start_rejects_nonexistent_proposal() {
        let (server, _db) = test_server().await;

        let resp = server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": "nonexistent" }),
            )
            .await
            .expect("tool should be registered");

        let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(
            error.contains("proposal not found"),
            "should mention proposal not found: {error}"
        );
    }

    /// Test that when the coordinator rejects the refinement start (e.g.
    /// duplicate active run), the error is propagated through the response
    /// and a `refinement_stop` lifecycle entry is recorded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_start_propagates_coordinator_error_and_records_stop() {
        use crate::bridge::CoordinatorOps;
        use async_trait::async_trait;

        /// Coordinator stub that always rejects refinement starts.
        struct RejectingCoordinator;
        #[async_trait]
        impl CoordinatorOps for RejectingCoordinator {
            fn get_status(&self) -> Result<crate::bridge::CoordinatorStatus, String> {
                Err("not initialized".into())
            }
            async fn trigger_dispatch_for_project(&self, _: &str) -> Result<(), String> {
                Err("not initialized".into())
            }
            async fn start_proposal_refinement(
                &self,
                _: crate::bridge::ProposalRefinementStartRequest,
            ) -> Result<(), String> {
                Err("duplicate refinement active".to_string())
            }
        }

        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let state = crate::state::stubs::test_mcp_state(db.clone());
        // Override the coordinator to one that rejects.
        let state = crate::state::McpState::with_enrichment(
            db.clone(),
            state.event_bus(),
            state.catalog().clone(),
            state.health_tracker().clone(),
            Some(Arc::new(RejectingCoordinator) as Arc<dyn CoordinatorOps>),
            None,
            None,
            None,
            state.lsp().clone(),
            Arc::new(crate::state::stubs::StubRuntimeOps),
            Arc::new(crate::state::stubs::StubGitOps),
            Arc::new(crate::state::stubs::StubRepoGraphOps),
            None,
        );
        let server = DjinnMcpServer::new(state);

        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Reject Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .expect("tool should be registered");

        let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(
            error.contains("coordinator rejected refinement start"),
            "should mention coordinator rejection: {error}"
        );
        assert!(
            error.contains("duplicate refinement active"),
            "should include the coordinator error message: {error}"
        );

        // Verify a refinement_stop lifecycle entry was recorded.
        let revisions = repo.revisions(&proposal.id).await.unwrap();
        let starts: Vec<_> = revisions
            .iter()
            .filter(|r| r.event_kind == "refinement_start")
            .collect();
        let stops: Vec<_> = revisions
            .iter()
            .filter(|r| r.event_kind == "refinement_stop")
            .collect();
        assert_eq!(starts.len(), 1, "expected one refinement_start");
        assert_eq!(stops.len(), 1, "expected one refinement_stop");
        // The stop metadata should include the error reason.
        let stop_meta: serde_json::Value =
            serde_json::from_str(stops[0].event_metadata.as_deref().unwrap_or("{}")).unwrap();
        assert_eq!(
            stop_meta.get("source").and_then(|v| v.as_str()),
            Some("coordinator_start_failure")
        );
    }

    /// Test that when the coordinator is not wired (None), the tool records
    /// a refinement_stop and returns an error rather than leaving a dangling
    /// lifecycle entry.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_start_records_stop_when_coordinator_unavailable() {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        // Build McpState with no coordinator (None).
        let state = crate::state::McpState::new(
            db.clone(),
            EventBus::noop(),
            crate::state::stubs::test_mcp_state(db.clone())
                .catalog()
                .clone(),
            crate::state::stubs::test_mcp_state(db.clone())
                .health_tracker()
                .clone(),
            None, // no coordinator
            None,
            None,
            None,
            Arc::new(crate::state::stubs::StubLspOps),
            Arc::new(crate::state::stubs::StubRuntimeOps),
            Arc::new(crate::state::stubs::StubGitOps),
            Arc::new(crate::state::stubs::StubRepoGraphOps),
        );
        let server = DjinnMcpServer::new(state);

        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "No Coordinator Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_refinement_start",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .expect("tool should be registered");

        let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(
            error.contains("coordinator not available"),
            "should mention coordinator unavailable: {error}"
        );

        // Verify the lifecycle was cleaned up.
        let revisions = repo.revisions(&proposal.id).await.unwrap();
        let stops: Vec<_> = revisions
            .iter()
            .filter(|r| r.event_kind == "refinement_stop")
            .collect();
        assert_eq!(stops.len(), 1, "expected one refinement_stop");
    }

    // ── Checkpoint list/approve/reject tests ────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checkpoint_list_returns_empty_for_no_pending() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Empty Checkpoint List",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_refinement_checkpoint_list",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .expect("tool should be registered");

        assert!(
            resp.get("error").and_then(|v| v.as_str()).is_none(),
            "expected no error, got: {:?}",
            resp.get("error")
        );
        let pending = resp.get("pending").and_then(|v| v.as_array()).unwrap();
        assert!(pending.is_empty(), "expected no pending revisions");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checkpoint_list_returns_pending_revisions() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Pending Checkpoint",
                body: "original body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Simulate a pending checkpoint revision by updating the proposal
        // (which creates a spec_revision) and then marking it as pending.
        repo.update(
            &proposal.id,
            djinn_db::ProposalUpdateInput {
                title: "Advocate Revised Title",
                body: "advocate revised body content",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();

        // Mark the latest revision as checkpoint_pending.
        let updated = repo.get(&proposal.id).await.unwrap().unwrap();
        let meta = serde_json::json!({
            "source": "refinement_loop",
            "role": "advocate",
            "round": 1,
            "authority": "checkpoint",
            "checkpoint_status": "pending",
        });
        repo.set_latest_revision_event_metadata(&proposal.id, updated.latest_revision_seq, &meta)
            .await
            .unwrap();

        // Revert the live body to simulate the coordinator's checkpoint revert.
        // (In production the coordinator does this; here we do it manually.)

        let resp = server
            .dispatch_tool(
                "proposal_refinement_checkpoint_list",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .expect("tool should be registered");

        let pending = resp.get("pending").and_then(|v| v.as_array()).unwrap();
        assert_eq!(pending.len(), 1, "expected one pending revision");
        let first = &pending[0];
        assert_eq!(
            first.get("title").and_then(|v| v.as_str()),
            Some("Advocate Revised Title")
        );
        assert_eq!(first.get("role").and_then(|v| v.as_str()), Some("advocate"));
        assert_eq!(first.get("round").and_then(|v| v.as_i64()), Some(1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checkpoint_approve_applies_pending_revision() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Original Title",
                body: "original body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Simulate advocate revision + pending marking.
        repo.update(
            &proposal.id,
            djinn_db::ProposalUpdateInput {
                title: "Advocate Title",
                body: "advocate body",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();

        let updated = repo.get(&proposal.id).await.unwrap().unwrap();
        let pending_seq = updated.latest_revision_seq;
        let meta = serde_json::json!({
            "source": "refinement_loop",
            "role": "advocate",
            "round": 1,
            "authority": "checkpoint",
            "checkpoint_status": "pending",
        });
        repo.set_latest_revision_event_metadata(&proposal.id, pending_seq, &meta)
            .await
            .unwrap();

        // Approve the pending revision.
        let resp = server
            .dispatch_tool(
                "proposal_refinement_checkpoint_approve",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "revision_seq": pending_seq,
                }),
            )
            .await
            .expect("tool should be registered");

        assert!(
            resp.get("error").and_then(|v| v.as_str()).is_none(),
            "expected no error, got: {:?}",
            resp.get("error")
        );
        assert_eq!(resp.get("approved").and_then(|v| v.as_bool()), Some(true));

        // The live proposal should now have the advocate's body.
        let final_proposal = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(final_proposal.title, "Advocate Title");
        assert_eq!(final_proposal.body, "advocate body");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checkpoint_reject_leaves_live_body_unchanged() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Original Title",
                body: "original body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Simulate advocate revision + pending marking + revert.
        repo.update(
            &proposal.id,
            djinn_db::ProposalUpdateInput {
                title: "Advocate Title",
                body: "advocate body",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();

        let updated = repo.get(&proposal.id).await.unwrap().unwrap();
        let pending_seq = updated.latest_revision_seq;
        let meta = serde_json::json!({
            "source": "refinement_loop",
            "role": "advocate",
            "round": 1,
            "authority": "checkpoint",
            "checkpoint_status": "pending",
        });
        repo.set_latest_revision_event_metadata(&proposal.id, pending_seq, &meta)
            .await
            .unwrap();

        // Reject the pending revision.
        let resp = server
            .dispatch_tool(
                "proposal_refinement_checkpoint_reject",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "revision_seq": pending_seq,
                }),
            )
            .await
            .expect("tool should be registered");

        assert!(
            resp.get("error").and_then(|v| v.as_str()).is_none(),
            "expected no error, got: {:?}",
            resp.get("error")
        );
        assert_eq!(resp.get("rejected").and_then(|v| v.as_bool()), Some(true));

        // The live proposal body is unchanged (reject doesn't modify it).
        let final_proposal = repo.get(&proposal.id).await.unwrap().unwrap();
        assert_eq!(final_proposal.body, "advocate body");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checkpoint_approve_is_idempotent() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Idempotent Approve",
                body: "original body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Create a pending revision.
        repo.update(
            &proposal.id,
            djinn_db::ProposalUpdateInput {
                title: "Pending Title",
                body: "pending body",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();

        let updated = repo.get(&proposal.id).await.unwrap().unwrap();
        let seq = updated.latest_revision_seq;
        let meta = serde_json::json!({
            "checkpoint_status": "pending",
            "role": "advocate",
            "round": 1,
            "authority": "checkpoint",
        });
        repo.set_latest_revision_event_metadata(&proposal.id, seq, &meta)
            .await
            .unwrap();

        // First approve.
        let resp1 = server
            .dispatch_tool(
                "proposal_refinement_checkpoint_approve",
                serde_json::json!({ "proposal_id": proposal.id, "revision_seq": seq }),
            )
            .await
            .unwrap();
        assert_eq!(resp1.get("approved").and_then(|v| v.as_bool()), Some(true));

        // Second approve (idempotent — the revision is no longer pending).
        let resp2 = server
            .dispatch_tool(
                "proposal_refinement_checkpoint_approve",
                serde_json::json!({ "proposal_id": proposal.id, "revision_seq": seq }),
            )
            .await
            .unwrap();
        assert_eq!(resp2.get("approved").and_then(|v| v.as_bool()), Some(true));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn checkpoint_reject_is_idempotent() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Idempotent Reject",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        repo.update(
            &proposal.id,
            djinn_db::ProposalUpdateInput {
                title: "Advocate Title",
                body: "advocate body",
                acceptance_criteria: "[]",
                status: "draft",
                superseded_by: None,
                body_format: None,
                event_metadata: None,
            },
        )
        .await
        .unwrap();

        let updated = repo.get(&proposal.id).await.unwrap().unwrap();
        let seq = updated.latest_revision_seq;
        let meta = serde_json::json!({
            "checkpoint_status": "pending",
            "role": "advocate",
            "round": 1,
            "authority": "checkpoint",
        });
        repo.set_latest_revision_event_metadata(&proposal.id, seq, &meta)
            .await
            .unwrap();

        // First reject.
        let resp1 = server
            .dispatch_tool(
                "proposal_refinement_checkpoint_reject",
                serde_json::json!({ "proposal_id": proposal.id, "revision_seq": seq }),
            )
            .await
            .unwrap();
        assert_eq!(resp1.get("rejected").and_then(|v| v.as_bool()), Some(true));

        // Second reject (idempotent — no longer pending).
        let resp2 = server
            .dispatch_tool(
                "proposal_refinement_checkpoint_reject",
                serde_json::json!({ "proposal_id": proposal.id, "revision_seq": seq }),
            )
            .await
            .unwrap();
        assert_eq!(resp2.get("rejected").and_then(|v| v.as_bool()), Some(true));
    }
}
