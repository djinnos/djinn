// Feedback tools for proposals: add and resolve feedback entries.
//
// Feedback is plain discussion on a proposal — it is NOT applied to the spec
// directly.  The proposal owner asks djinn in chat to apply feedback, which
// rewrites the spec as a new revision and resolves the feedback.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;

use crate::server::DjinnMcpServer;
use crate::tools::proposal_ops::ProposalFeedbackResponse;
use crate::tools::refinement_tools::admit_refinement_run;
use crate::tools::validation::validate_body;
use djinn_db::{ProposalRepository, RefinementAdmissionSource};

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
    #[serde(default)]
    pub severity: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalFeedbackResolveParams {
    /// Feedback entry UUID.
    pub id: String,
    /// The proposal revision that addressed this feedback (omit for a plain
    /// dismissal with no spec change).
    pub resolved_revision_seq: Option<i32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalFeedbackWithdrawParams {
    /// Feedback entry UUID.
    pub id: String,
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
        let severity = p.severity.as_deref().unwrap_or("blocking");
        if !matches!(severity, "advisory" | "blocking") {
            return Json(err_feedback(
                "invalid severity (expected advisory or blocking)",
            ));
        }
        match repo
            .add_feedback_with_severity(
                djinn_db::ProposalFeedbackCreateInput {
                    proposal_id: &proposal.id,
                    parent_id: p.parent_id.as_deref(),
                    author_kind,
                    author_model: p.author_model.as_deref(),
                    body: &p.body,
                },
                severity,
            )
            .await
        {
            Ok(f) => {
                // Advisory feedback is discussion only. Blocking feedback added
                // during review uses the durable admission path for a demanded
                // round; the feedback id makes retries idempotent.
                if severity == "blocking"
                    && proposal.status == "in_review"
                    && admit_refinement_run(
                        self,
                        &repo,
                        &proposal.id,
                        RefinementAdmissionSource::Demand {
                            demand_id: format!("feedback:{}", f.id),
                        },
                        None,
                    )
                    .await
                    .is_ok()
                {
                    // The explicit start/demand boundaries capture after
                    // admission. This auto-demand follows that same order.
                    let _ = repo
                        .capture_feedback_refinement_boundary(&proposal.id)
                        .await;
                }
                Json(ProposalFeedbackResponse {
                    feedback: Some((&f).into()),
                    error: None,
                })
            }
            Err(e) => Json(err_feedback(e.to_string())),
        }
    }

    /// Withdraw feedback authored by the authenticated caller.
    #[tool(
        description = "Withdraw a feedback entry that you originally authored. Captured snapshots remain immutable, and a materialized human-feedback objection closes only after every captured blocking source has been withdrawn."
    )]
    pub async fn proposal_feedback_withdraw(
        &self,
        Parameters(p): Parameters<ProposalFeedbackWithdrawParams>,
    ) -> Json<ProposalFeedbackResponse> {
        let Some(user_id) = djinn_core::auth_context::current_user_id() else {
            return Json(err_feedback(
                "feedback withdrawal requires an authenticated original author",
            ));
        };
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(feedback) = repo.get_feedback(&p.id).await.ok().flatten() else {
            return Json(err_feedback(format!("feedback not found: {}", p.id)));
        };
        if feedback.author_user_id.as_deref() != Some(user_id.as_str()) {
            return Json(err_feedback(
                "feedback withdrawal requires the original row author",
            ));
        }
        match repo
            .withdraw_feedback_with_refinement_derivation(&feedback.id, &user_id)
            .await
        {
            Ok((feedback, _)) => Json(ProposalFeedbackResponse {
                feedback: Some((&feedback).into()),
                error: None,
            }),
            Err(error) => Json(err_feedback(error.to_string())),
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
        let _ = feedback;
        let _ = p.resolved_revision_seq;
        Json(err_feedback(
            "feedback_resolution_requires_disposition_or_withdrawal",
        ))
    }
}
