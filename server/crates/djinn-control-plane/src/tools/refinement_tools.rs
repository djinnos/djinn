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

use crate::server::DjinnMcpServer;
use crate::tools::proposal_ops::{
    ProposalRefinementStartResponse, ProposalRefinementStatusModel,
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

// ── Tool router ──────────────────────────────────────────────────────────────

#[tool_router(router = refinement_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Start proposal refinement. Validates the proposal is in a state that
    /// supports refinement (draft or in_review), records a `refinement_start`
    /// lifecycle entry in the proposal revision history, and returns the initial
    /// refinement status.
    ///
    /// `update_authority` controls whether advocate revisions are applied
    /// automatically (`auto_accept`) or proposed for approval (`checkpoint`,
    /// the default). Same-model fallback is allowed when diverse models are
    /// unavailable — this is not presented as an error.
    #[tool(
        description = "Start proposal refinement for the given proposal. Validates the proposal exists and is in draft or in_review state. Records a refinement_start lifecycle event. `update_authority` is `checkpoint` (default — advocate revisions require approval) or `auto_accept` (revisions are applied automatically). Returns the initial refinement status. Same-model fallback is used when diverse models are unavailable."
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

        // Check if refinement is already active (latest refinement_start
        // without a corresponding refinement_stop).
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

        let refinement = ProposalRefinementStatusModel {
            active: true,
            current_round: Some(1),
            dry_rounds: 0,
            total_entries: 0,
            update_authority: authority.to_string(),
            stop_reason: None,
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
    let stop_reason = if !is_active {
        stop_after
            .and_then(|s| s.event_metadata.as_ref())
            .and_then(|m| serde_json::from_str::<serde_json::Value>(m).ok())
            .and_then(|v| v.get("stop_reason")?.as_str().map(String::from))
    } else {
        None
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
}
