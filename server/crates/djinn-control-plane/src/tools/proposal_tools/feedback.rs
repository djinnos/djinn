// Feedback tools for proposals: add and resolve feedback entries.
//
// Feedback is plain discussion on a proposal — it is NOT applied to the spec
// directly.  The proposal owner asks djinn in chat to apply feedback, which
// rewrites the spec as a new revision and resolves the feedback.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;

use crate::server::DjinnMcpServer;
use crate::tools::proposal_ops::ProposalFeedbackResponse;
use crate::tools::validation::validate_body;
use djinn_db::ProposalRepository;

use super::proposal_not_found_error;

// ── Param structs ───────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalFeedbackAddParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
    pub body: String,
    /// Parent feedback id for a threaded reply.
    pub parent_id: Option<String>,
    /// `user` (default) or `ai`.
    pub author_kind: Option<String>,
    /// Model id when author_kind is `ai`.
    pub author_model: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalFeedbackResolveParams {
    /// Feedback entry UUID.
    pub id: String,
    /// The proposal revision that addressed this feedback (omit for a plain
    /// dismissal with no spec change).
    pub resolved_revision_seq: Option<i32>,
}

// ── Response helpers ────────────────────────────────────────────────────────

pub(super) fn err_feedback(error: impl Into<String>) -> ProposalFeedbackResponse {
    ProposalFeedbackResponse {
        feedback: None,
        error: Some(error.into()),
    }
}

// ── Tool router ─────────────────────────────────────────────────────────────

#[tool_router(router = proposal_feedback_tool_router, vis = "pub(super)")]
impl DjinnMcpServer {
    /// Add a feedback entry (plain discussion) to a proposal.
    #[tool(
        description = "Add a feedback comment to a proposal. Feedback is plain discussion — it is NOT applied to the spec directly; the proposal owner asks djinn in chat to apply it, which rewrites the spec as a new revision and resolves the feedback. `author_kind` is `user` (default) or `ai` (set `author_model` for AI). `parent_id` threads a reply."
    )]
    pub async fn proposal_feedback_add(
        &self,
        Parameters(p): Parameters<ProposalFeedbackAddParams>,
    ) -> Json<ProposalFeedbackResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.proposal_id).await.ok().flatten() else {
            return Json(err_feedback(proposal_not_found_error(&p.proposal_id)));
        };
        if let Err(e) = validate_body(&p.body) {
            return Json(err_feedback(e));
        }
        let author_kind = p.author_kind.as_deref().unwrap_or("user");
        if !matches!(author_kind, "user" | "ai") {
            return Json(err_feedback(format!(
                "invalid author_kind: {author_kind:?} (expected user or ai)"
            )));
        }
        match repo
            .add_feedback(djinn_db::ProposalFeedbackCreateInput {
                proposal_id: &proposal.id,
                parent_id: p.parent_id.as_deref(),
                author_kind,
                author_model: p.author_model.as_deref(),
                body: &p.body,
            })
            .await
        {
            Ok(f) => Json(ProposalFeedbackResponse {
                feedback: Some((&f).into()),
                error: None,
            }),
            Err(e) => Json(err_feedback(e.to_string())),
        }
    }

    /// Resolve a feedback entry: collapse it out of the active thread.
    #[tool(
        description = "Resolve a feedback entry, collapsing it out of the active thread. Pass `resolved_revision_seq` with the proposal revision that addressed it (when a spec change was applied), or omit it for a plain dismissal. Requires edit rights on the proposal."
    )]
    pub async fn proposal_feedback_resolve(
        &self,
        Parameters(p): Parameters<ProposalFeedbackResolveParams>,
    ) -> Json<ProposalFeedbackResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(feedback) = repo.get_feedback(&p.id).await.ok().flatten() else {
            return Json(err_feedback(format!("feedback not found: {}", p.id)));
        };
        // Resolving a proposal's feedback is an edit on that proposal's review
        // state → requires edit rights, same gate as proposal_update.
        let author = repo
            .resolve(&feedback.proposal_id)
            .await
            .ok()
            .flatten()
            .and_then(|pr| pr.author_user_id);
        if let Err(e) = self.gate_proposal_edit(author.as_deref()).await {
            return Json(err_feedback(e));
        }
        match repo
            .set_feedback_resolved(&p.id, p.resolved_revision_seq)
            .await
        {
            Ok(f) => Json(ProposalFeedbackResponse {
                feedback: Some((&f).into()),
                error: None,
            }),
            Err(e) => Json(err_feedback(e.to_string())),
        }
    }
}
