// MCP tools for the proposal debate-trail (objections, rebuttals, verdicts).
//
// Debate-trail rows are distinct from `proposal_feedback` (human discussion):
// they are typed, carry blocking/agent-role metadata, and have a resolution/
// reopen lifecycle that supports the adversarial refinement workflow.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;

use crate::server::DjinnMcpServer;
use crate::tools::proposal_ops::{ProposalDebateTrailListResponse, ProposalDebateTrailResponse};
use djinn_db::{ProposalDebateTrailCreateInput, ProposalRepository};

fn debate_not_found_error(id: &str) -> String {
    format!("debate trail entry not found: {id}")
}

fn err_debate(error: impl Into<String>) -> ProposalDebateTrailResponse {
    ProposalDebateTrailResponse {
        entry: None,
        error: Some(error.into()),
    }
}

fn err_debate_list(error: impl Into<String>) -> ProposalDebateTrailListResponse {
    ProposalDebateTrailListResponse {
        proposal_id: None,
        entries: None,
        error: Some(error.into()),
    }
}

// ── Param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalDebateAppendParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
    /// `objection` | `rebuttal` | `verdict`.
    pub kind: String,
    /// Body text of the debate entry.
    pub body: String,
    /// When true, this entry blocks proposal readiness.
    /// Meaningful for objection and verdict kinds.
    #[serde(default)]
    pub blocking: bool,
    /// Agent role (e.g. "advocate", "adversary", "judge").
    pub agent_role: String,
    /// `agent` (default) or `user`.
    #[serde(default = "default_author_kind")]
    pub author_kind: String,
    /// Model id when `author_kind == "agent"`.
    pub author_model: Option<String>,
    /// Optional source task attribution.
    pub source_task_id: Option<String>,
    /// The proposal revision this entry is written against.
    pub against_revision_seq: i32,
    /// Debate round (1-based).
    pub round: i32,
}

fn default_author_kind() -> String {
    "agent".to_string()
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalDebateListParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalDebateResolveParams {
    /// Debate-trail entry UUID.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalDebateReopenParams {
    /// Debate-trail entry UUID.
    pub id: String,
}

// ── Tool router ──────────────────────────────────────────────────────────────

#[tool_router(router = debate_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Append a debate-trail entry (objection, rebuttal, or verdict) to a
    /// proposal. Validates that the proposal exists and `kind` is one of the
    /// allowed values. `blocking` is meaningful for objection and verdict rows.
    /// `author_kind` distinguishes agent-authored from user-authored entries.
    #[tool(
        description = "Append a debate-trail entry to a proposal. `kind` is `objection`, `rebuttal`, or `verdict`; `blocking` (default false) is meaningful for objection/verdict; `agent_role` labels the speaker (e.g. advocate, adversary, judge); `author_kind` is `agent` (default) or `user`; set `author_model` for agent attribution. Requires a valid `proposal_id` and `against_revision_seq` (the proposal revision being debated). `round` is 1-based."
    )]
    pub async fn proposal_debate_append(
        &self,
        Parameters(p): Parameters<ProposalDebateAppendParams>,
    ) -> Json<ProposalDebateTrailResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(err_debate(format!("proposal not found: {}", p.proposal_id)));
        };
        let kind = p.kind.as_str();
        if !matches!(kind, "objection" | "rebuttal" | "verdict") {
            return Json(err_debate(format!(
                "invalid kind: {kind:?} (expected objection, rebuttal, or verdict)"
            )));
        }
        if p.body.trim().is_empty() {
            return Json(err_debate("body must not be empty".to_string()));
        }
        if p.agent_role.trim().is_empty() {
            return Json(err_debate("agent_role must not be empty".to_string()));
        }
        let author_kind = p.author_kind.as_str();
        if !matches!(author_kind, "agent" | "user") {
            return Json(err_debate(format!(
                "invalid author_kind: {author_kind:?} (expected agent or user)"
            )));
        }
        if p.round < 1 {
            return Json(err_debate(format!("round must be >= 1 (got {})", p.round)));
        }

        match repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &proposal.id,
                kind,
                body: &p.body,
                blocking: p.blocking,
                agent_role: &p.agent_role,
                author_kind,
                author_model: p.author_model.as_deref(),
                source_task_id: p.source_task_id.as_deref(),
                against_revision_seq: p.against_revision_seq,
                round: p.round,
            })
            .await
        {
            Ok(entry) => Json(ProposalDebateTrailResponse {
                entry: Some((&entry).into()),
                error: None,
            }),
            Err(e) => Json(err_debate(e.to_string())),
        }
    }

    /// List all debate-trail entries for a proposal, ordered by round then
    /// creation time. Returns an empty list when the proposal has no debate
    /// trail entries.
    #[tool(
        description = "List debate-trail entries for a proposal (by UUID or short_id). Returns all rows ordered by round then creation time. Debate entries are distinct from feedback and include typed objections, rebuttals, and verdicts with blocking/reopen state."
    )]
    pub async fn proposal_debate_list(
        &self,
        Parameters(p): Parameters<ProposalDebateListParams>,
    ) -> Json<ProposalDebateTrailListResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(err_debate_list(format!(
                "proposal not found: {}",
                p.proposal_id
            )));
        };
        match repo.debate_trail(&proposal.id).await {
            Ok(entries) => Json(ProposalDebateTrailListResponse {
                proposal_id: Some(proposal.id),
                entries: Some(entries.iter().map(Into::into).collect()),
                error: None,
            }),
            Err(e) => Json(err_debate_list(e.to_string())),
        }
    }

    /// Resolve a debate-trail entry, marking it as addressed. Requires edit
    /// rights on the parent proposal (same gate as `proposal_update`).
    /// Idempotent — resolving an already-resolved entry is a no-op.
    #[tool(
        description = "Resolve a debate-trail entry, marking it as addressed. Requires edit rights on the parent proposal. Clears any prior reopen state. Idempotent — resolving an already-resolved entry is a no-op."
    )]
    pub async fn proposal_debate_resolve(
        &self,
        Parameters(p): Parameters<ProposalDebateResolveParams>,
    ) -> Json<ProposalDebateTrailResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(entry) = repo.get_debate_trail_entry(&p.id).await.ok().flatten() else {
            return Json(err_debate(debate_not_found_error(&p.id)));
        };
        // Resolving a debate entry is an edit on the proposal's review state
        // → requires edit rights, same gate as proposal_feedback_resolve.
        let author = repo
            .resolve(&entry.proposal_id)
            .await
            .ok()
            .flatten()
            .and_then(|pr| pr.author_user_id);
        if let Err(e) = self.gate_proposal_edit(author.as_deref()).await {
            return Json(err_debate(e));
        }
        match repo.resolve_debate_trail_entry(&p.id).await {
            Ok(updated) => Json(ProposalDebateTrailResponse {
                entry: Some((&updated).into()),
                error: None,
            }),
            Err(e) => Json(err_debate(e.to_string())),
        }
    }

    /// Reopen a previously resolved debate-trail entry. Requires edit rights
    /// on the parent proposal. No-op (idempotent) if already open.
    #[tool(
        description = "Reopen a previously resolved debate-trail entry. Requires edit rights on the parent proposal. No-op if already open. The entry returns to the open state and can be re-resolved later."
    )]
    pub async fn proposal_debate_reopen(
        &self,
        Parameters(p): Parameters<ProposalDebateReopenParams>,
    ) -> Json<ProposalDebateTrailResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(entry) = repo.get_debate_trail_entry(&p.id).await.ok().flatten() else {
            return Json(err_debate(debate_not_found_error(&p.id)));
        };
        let author = repo
            .resolve(&entry.proposal_id)
            .await
            .ok()
            .flatten()
            .and_then(|pr| pr.author_user_id);
        if let Err(e) = self.gate_proposal_edit(author.as_deref()).await {
            return Json(err_debate(e));
        }
        match repo.reopen_debate_trail_entry(&p.id).await {
            Ok(updated) => Json(ProposalDebateTrailResponse {
                entry: Some((&updated).into()),
                error: None,
            }),
            Err(e) => Json(err_debate(e.to_string())),
        }
    }
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
    async fn debate_append_and_list_round_trip() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Test Proposal",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Append an objection.
        let resp = server
            .dispatch_tool(
                "proposal_debate_append",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "kind": "objection",
                    "body": "Missing test coverage",
                    "blocking": true,
                    "agent_role": "adversary",
                    "author_kind": "agent",
                    "against_revision_seq": 1,
                    "round": 1,
                }),
            )
            .await
            .expect("tool should be registered");
        assert!(
            resp.get("error").and_then(|v| v.as_str()).is_none(),
            "expected no error, got: {:?}",
            resp.get("error")
        );
        let entry_id = resp
            .get("id")
            .and_then(|v| v.as_str())
            .expect("response should have id");

        // List.
        let list_resp = server
            .dispatch_tool(
                "proposal_debate_list",
                serde_json::json!({ "proposal_id": proposal.id }),
            )
            .await
            .expect("tool should be registered");
        assert!(list_resp.get("error").and_then(|v| v.as_str()).is_none());
        let entries = list_resp
            .get("entries")
            .and_then(|v| v.as_array())
            .expect("should have entries array");
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].get("kind").and_then(|v| v.as_str()),
            Some("objection")
        );
        assert_eq!(
            entries[0].get("id").and_then(|v| v.as_str()),
            Some(entry_id)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debate_resolve_and_reopen_lifecycle() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Lifecycle Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Append.
        let resp = server
            .dispatch_tool(
                "proposal_debate_append",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "kind": "verdict",
                    "body": "Proposal is ready",
                    "blocking": false,
                    "agent_role": "judge",
                    "author_kind": "agent",
                    "against_revision_seq": 1,
                    "round": 1,
                }),
            )
            .await
            .expect("tool should be registered");
        let entry_id = resp.get("id").and_then(|v| v.as_str()).unwrap().to_string();

        // Resolve.
        let resolve_resp = server
            .dispatch_tool(
                "proposal_debate_resolve",
                serde_json::json!({ "id": entry_id }),
            )
            .await
            .expect("tool should be registered");
        assert!(resolve_resp.get("error").and_then(|v| v.as_str()).is_none());
        assert!(
            resolve_resp
                .get("resolved_at")
                .and_then(|v| v.as_str())
                .is_some(),
            "should be resolved"
        );

        // Reopen.
        let reopen_resp = server
            .dispatch_tool(
                "proposal_debate_reopen",
                serde_json::json!({ "id": entry_id }),
            )
            .await
            .expect("tool should be registered");
        assert!(reopen_resp.get("error").and_then(|v| v.as_str()).is_none());
        assert!(
            reopen_resp
                .get("reopened_at")
                .and_then(|v| v.as_str())
                .is_some(),
            "should be reopened"
        );
        // resolved_at should still be set.
        assert!(
            reopen_resp
                .get("resolved_at")
                .and_then(|v| v.as_str())
                .is_some(),
            "resolved_at should persist after reopen"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debate_append_rejects_invalid_kind() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let proposal = repo
            .create(ProposalCreateInput {
                title: "Validation Test",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let resp = server
            .dispatch_tool(
                "proposal_debate_append",
                serde_json::json!({
                    "proposal_id": proposal.id,
                    "kind": "invalid_kind",
                    "body": "test",
                    "agent_role": "advocate",
                    "against_revision_seq": 1,
                    "round": 1,
                }),
            )
            .await
            .expect("tool should be registered");
        let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(
            error.contains("invalid kind"),
            "error should mention invalid kind: {error}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debate_append_rejects_nonexistent_proposal() {
        let (server, _db) = test_server().await;

        let resp = server
            .dispatch_tool(
                "proposal_debate_append",
                serde_json::json!({
                    "proposal_id": "nonexistent",
                    "kind": "objection",
                    "body": "test",
                    "agent_role": "advocate",
                    "against_revision_seq": 1,
                    "round": 1,
                }),
            )
            .await
            .expect("tool should be registered");
        let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(
            error.contains("proposal not found"),
            "error should mention proposal not found: {error}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debate_list_proposal_isolation() {
        let (server, db) = test_server().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        let p1 = repo
            .create(ProposalCreateInput {
                title: "Proposal A",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();
        let p2 = repo
            .create(ProposalCreateInput {
                title: "Proposal B",
                body: "body",
                acceptance_criteria: Some("[]"),
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        // Append to p1 only.
        server
            .dispatch_tool(
                "proposal_debate_append",
                serde_json::json!({
                    "proposal_id": p1.id,
                    "kind": "objection",
                    "body": "only on p1",
                    "agent_role": "adversary",
                    "against_revision_seq": 1,
                    "round": 1,
                }),
            )
            .await
            .expect("tool should be registered");

        // p1 should have 1 entry.
        let list1 = server
            .dispatch_tool(
                "proposal_debate_list",
                serde_json::json!({ "proposal_id": p1.id }),
            )
            .await
            .unwrap();
        assert_eq!(
            list1
                .get("entries")
                .and_then(|v| v.as_array())
                .unwrap()
                .len(),
            1
        );

        // p2 should have 0 entries.
        let list2 = server
            .dispatch_tool(
                "proposal_debate_list",
                serde_json::json!({ "proposal_id": p2.id }),
            )
            .await
            .unwrap();
        assert_eq!(
            list2
                .get("entries")
                .and_then(|v| v.as_array())
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn debate_resolve_nonexistent_entry_returns_error() {
        let (server, _db) = test_server().await;
        let resp = server
            .dispatch_tool(
                "proposal_debate_resolve",
                serde_json::json!({ "id": "nonexistent-uuid" }),
            )
            .await
            .expect("tool should be registered");
        let error = resp.get("error").and_then(|v| v.as_str()).unwrap();
        assert!(error.contains("not found"));
    }
}
