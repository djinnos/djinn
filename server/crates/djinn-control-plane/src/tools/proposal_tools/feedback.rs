// Feedback tools for proposals: add and resolve feedback entries.
//
// Feedback is plain discussion on a proposal — it is NOT applied to the spec
// directly. Blocking feedback starts or joins tribunal refinement; advisory
// feedback remains stored discussion and never dispatches a refinement round.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;

use crate::server::DjinnMcpServer;
use crate::tools::proposal_ops::ProposalFeedbackResponse;
use crate::tools::refinement_tools::admit_refinement_run;
use crate::tools::validation::validate_body;
use djinn_db::{ProposalRepository, RefinementAdmissionSource};
use djinn_roles::{AgentType, tool_schemas_for};

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
    /// Feedback entry UUID. Only its original author may withdraw it.
    pub id: String,
}

// ── Response helpers ────────────────────────────────────────────────────────

pub(super) fn err_feedback(error: impl Into<String>) -> ProposalFeedbackResponse {
    ProposalFeedbackResponse {
        feedback: None,
        error: Some(error.into()),
    }
}

/// The active role-schema registry owns tribunal tool contracts. Missing
/// registration or any part of either operation's input contract fails closed
/// before new work can create an obligation the tribunal cannot drain.
pub(crate) fn human_feedback_disposition_contract_available() -> bool {
    human_feedback_disposition_contract_available_for_schemas(
        &tool_schemas_for(AgentType::Advocate),
        &tool_schemas_for(AgentType::Judge),
    )
}

/// Validate the concrete role schemas that tribunal agents receive.
///
/// This accepts schema slices rather than looking up roles itself so tests can
/// prove that each missing tool, property, or enum contract fails closed.
/// `inputSchema` is the MCP serialization owned by the active schema registry.
pub(crate) fn human_feedback_disposition_contract_available_for_schemas(
    advocate_schemas: &[serde_json::Value],
    judge_schemas: &[serde_json::Value],
) -> bool {
    fn tool_input_schema<'a>(
        schemas: &'a [serde_json::Value],
        tool_name: &str,
    ) -> Option<&'a serde_json::Value> {
        schemas.iter().find_map(|tool| {
            (tool.get("name").and_then(serde_json::Value::as_str) == Some(tool_name))
                .then(|| tool.get("inputSchema"))
                .flatten()
        })
    }

    fn string_enum_is(
        schema: &serde_json::Value,
        property: &str,
        expected: &[&str],
        required: bool,
    ) -> bool {
        if schema.get("type").and_then(serde_json::Value::as_str) != Some("object") {
            return false;
        }
        if required
            && !schema
                .get("required")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|fields| fields.iter().any(|field| field.as_str() == Some(property)))
        {
            return false;
        }
        let Some(field_schema) = schema
            .get("properties")
            .and_then(|properties| properties.get(property))
        else {
            return false;
        };
        if field_schema.get("type").and_then(serde_json::Value::as_str) != Some("string") {
            return false;
        }
        let Some(values) = field_schema
            .get("enum")
            .and_then(serde_json::Value::as_array)
        else {
            return false;
        };
        values.len() == expected.len()
            && expected.iter().all(|expected_value| {
                values
                    .iter()
                    .any(|value| value.as_str() == Some(expected_value))
            })
    }

    let Some(advocate) = tool_input_schema(advocate_schemas, "proposal_feedback_disposition")
    else {
        return false;
    };
    let Some(judge) = tool_input_schema(judge_schemas, "proposal_debate_resolve") else {
        return false;
    };

    // Advocate requires its source-specific disposition. Judge's global resolve
    // tool retains ordinary-objection behavior, so verdict is optional there but
    // its human-feedback branch must remain a typed accept/reject contract.
    string_enum_is(
        advocate,
        "disposition",
        &["fixed_by_revision", "wont_fix"],
        true,
    ) && string_enum_is(judge, "verdict", &["accept", "reject"], false)
}

/// Applies only to new feedback work. Readiness and materialized generations
/// remain drainable after either switch is disabled.
pub(crate) fn can_activate_feedback_refinement_with_contract(
    blocking: bool,
    in_review: bool,
    auto_resume: bool,
    capture: bool,
    contract_available: bool,
) -> bool {
    blocking && in_review && auto_resume && capture && contract_available
}

pub(crate) fn can_activate_feedback_refinement(
    blocking: bool,
    in_review: bool,
    auto_resume: bool,
    capture: bool,
) -> bool {
    can_activate_feedback_refinement_with_contract(
        blocking,
        in_review,
        auto_resume,
        capture,
        human_feedback_disposition_contract_available(),
    )
}

fn feedback_auto_resume_boundary_id(feedback_id: &str) -> String {
    format!("feedback:auto-resume:boundary:{feedback_id}")
}

// ── Tool router ─────────────────────────────────────────────────────────────

#[tool_router(router = proposal_feedback_tool_router, vis = "pub(super)")]
impl DjinnMcpServer {
    /// Add a feedback entry (plain discussion) to a proposal.
    #[tool(
        description = "Add a feedback comment to a proposal. Feedback never rewrites the spec directly: blocking feedback on an in-review proposal starts or joins tribunal refinement, while advisory feedback is stored without dispatch. `author_kind` is `user` (default) or `ai` (set `author_model` for AI). `parent_id` threads a reply."
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
        let controls = self.state.feedback_refinement_controls();
        let activation_enabled = can_activate_feedback_refinement(
            severity == "blocking",
            proposal.status == "in_review",
            controls.auto_resume,
            controls.capture,
        );
        match repo
            .add_feedback_with_severity_and_pending_handoff(
                djinn_db::ProposalFeedbackCreateInput {
                    proposal_id: &proposal.id,
                    parent_id: p.parent_id.as_deref(),
                    author_kind,
                    author_model: p.author_model.as_deref(),
                    body: &p.body,
                },
                severity,
                activation_enabled,
            )
            .await
        {
            Ok((f, handoff_persisted)) => {
                // Advisory feedback is discussion only. A blocking row is the
                // durable lifecycle boundary for its auto-demand. Using the row
                // identity (rather than a permanent proposal identity) means a
                // row committed after an earlier capture cutoff remains eligible
                // to admit the subsequent cohort; replaying this exact boundary
                // still resolves to Existing and cannot duplicate its owner.
                if activation_enabled {
                    match admit_refinement_run(
                        self,
                        &repo,
                        &proposal.id,
                        RefinementAdmissionSource::Demand {
                            demand_id: feedback_auto_resume_boundary_id(&f.id),
                        },
                        None,
                    )
                    .await
                    {
                        Ok(admission) if admission.admitted => {
                            if handoff_persisted
                                && let Err(error) = repo
                                    .complete_pending_feedback_refinement_handoff(
                                        &proposal.id,
                                        &admission.run_id,
                                    )
                                    .await
                            {
                                return Json(err_feedback(error.to_string()));
                            }
                        }
                        Err(rejection)
                            if rejection.code == "already_active" && handoff_persisted => {}
                        Err(rejection) if rejection.code == "already_active" => {
                            return Json(err_feedback(
                                "refinement ownership changed before durable handoff admission",
                            ));
                        }
                        Ok(_) | Err(_) => {}
                    }
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

#[cfg(test)]
mod tests {
    use super::{
        can_activate_feedback_refinement_with_contract, feedback_auto_resume_boundary_id,
        human_feedback_disposition_contract_available_for_schemas,
    };

    fn disposition_schema() -> serde_json::Value {
        serde_json::json!({
            "name": "proposal_feedback_disposition",
            "inputSchema": {"type": "object", "required": ["id", "disposition"], "properties": {
                "disposition": {"type": "string", "enum": ["fixed_by_revision", "wont_fix"]}
            }}
        })
    }

    fn resolve_schema() -> serde_json::Value {
        serde_json::json!({
            "name": "proposal_debate_resolve",
            "inputSchema": {"type": "object", "required": ["id"], "properties": {
                "verdict": {"type": "string", "enum": ["accept", "reject"]}
            }}
        })
    }

    #[test]
    fn activation_requires_blocking_review_controls_and_contract() {
        assert!(can_activate_feedback_refinement_with_contract(
            true, true, true, true, true
        ));
        assert!(!can_activate_feedback_refinement_with_contract(
            false, true, true, true, true
        ));
        assert!(!can_activate_feedback_refinement_with_contract(
            true, false, true, true, true
        ));
        assert!(!can_activate_feedback_refinement_with_contract(
            true, true, false, true, true
        ));
        assert!(!can_activate_feedback_refinement_with_contract(
            true, true, true, false, true
        ));
        assert!(!can_activate_feedback_refinement_with_contract(
            true, true, true, true, false
        ));
    }

    #[test]
    fn role_schema_contract_requires_each_role_and_semantic_field() {
        let advocate = vec![disposition_schema()];
        let judge = vec![resolve_schema()];
        assert!(human_feedback_disposition_contract_available_for_schemas(
            &advocate, &judge
        ));
        assert!(!human_feedback_disposition_contract_available_for_schemas(
            &[],
            &judge
        ));
        assert!(!human_feedback_disposition_contract_available_for_schemas(
            &advocate,
            &[]
        ));

        let malformed_disposition = vec![serde_json::json!({
            "name": "proposal_feedback_disposition",
            "inputSchema": {"properties": {"id": {"type": "string"}}}
        })];
        assert!(!human_feedback_disposition_contract_available_for_schemas(
            &malformed_disposition,
            &judge,
        ));

        let malformed_disposition_variants = vec![serde_json::json!({
            "name": "proposal_feedback_disposition",
            "inputSchema": {"properties": {
                "disposition": {"type": "string", "enum": ["fixed_by_revision"]}
            }}
        })];
        assert!(!human_feedback_disposition_contract_available_for_schemas(
            &malformed_disposition_variants,
            &judge,
        ));

        let disposition_not_required = vec![serde_json::json!({
            "name": "proposal_feedback_disposition",
            "inputSchema": {"type": "object", "required": ["id"], "properties": {
                "disposition": {"type": "string", "enum": ["fixed_by_revision", "wont_fix"]}
            }}
        })];
        assert!(!human_feedback_disposition_contract_available_for_schemas(
            &disposition_not_required,
            &judge,
        ));

        let disposition_not_string = vec![serde_json::json!({
            "name": "proposal_feedback_disposition",
            "inputSchema": {"type": "object", "required": ["id", "disposition"], "properties": {
                "disposition": {"type": "integer", "enum": ["fixed_by_revision", "wont_fix"]}
            }}
        })];
        assert!(!human_feedback_disposition_contract_available_for_schemas(
            &disposition_not_string,
            &judge,
        ));

        let missing_verdict = vec![serde_json::json!({
            "name": "proposal_debate_resolve",
            "inputSchema": {"properties": {"id": {"type": "string"}}}
        })];
        assert!(!human_feedback_disposition_contract_available_for_schemas(
            &advocate,
            &missing_verdict,
        ));

        let verdict_not_string = vec![serde_json::json!({
            "name": "proposal_debate_resolve",
            "inputSchema": {"type": "object", "required": ["id"], "properties": {
                "verdict": {"type": "integer", "enum": ["accept", "reject"]}
            }}
        })];
        assert!(!human_feedback_disposition_contract_available_for_schemas(
            &advocate,
            &verdict_not_string,
        ));

        let malformed_verdict = vec![serde_json::json!({
            "name": "proposal_debate_resolve",
            "inputSchema": {"properties": {
                "verdict": {"type": "string", "enum": ["accept"]}
            }}
        })];
        assert!(!human_feedback_disposition_contract_available_for_schemas(
            &advocate,
            &malformed_verdict,
        ));
    }

    #[test]
    fn auto_resume_identity_is_stable_per_boundary_not_per_proposal() {
        let first = feedback_auto_resume_boundary_id("feedback-a");
        assert_eq!(first, feedback_auto_resume_boundary_id("feedback-a"));
        assert_ne!(first, feedback_auto_resume_boundary_id("feedback-b"));
    }
}
