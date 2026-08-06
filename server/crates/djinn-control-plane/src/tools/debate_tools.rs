// MCP tools for the proposal debate-trail (objections, rebuttals, verdicts).
//
// Debate-trail rows are distinct from `proposal_feedback` (human discussion):
// they are typed, carry blocking/agent-role metadata, and have a resolution/
// reopen lifecycle that supports the adversarial refinement workflow.

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::server::DjinnMcpServer;
use crate::tools::proposal_ops::{
    ProposalDebateTrailListResponse, ProposalDebateTrailModel, ProposalDebateTrailResponse,
    ProposalFeedbackSourceRowModel,
};
use djinn_db::{
    FeedbackRefinementDisposition, FeedbackRefinementDispositionInput,
    FeedbackRefinementRejectionInput, ProposalDebateTrailCreateInput, ProposalRepository,
};

fn debate_not_found_error(id: &str) -> String {
    format!("debate trail entry not found: {id}")
}

// `proposal_debate_append` deliberately accepts caller-provided speaker and
// body text. A feedback disposition therefore cannot use either field as its
// authority boundary: only this tool writes the structured marker below (the
// generic append surface always writes `body_metadata: None`).
const FEEDBACK_DISPOSITION_METADATA_KIND: &str = "human_feedback_disposition_v1";
const FEEDBACK_DISPOSITION_REJECTION_METADATA_KIND: &str =
    "human_feedback_disposition_rejection_v1";

fn feedback_disposition_metadata(
    feedback_entry_id: &str,
    disposition: &FeedbackRefinementDisposition,
) -> Value {
    match disposition {
        FeedbackRefinementDisposition::FixedRevision { revision_seq } => json!({
            "kind": FEEDBACK_DISPOSITION_METADATA_KIND,
            "human_feedback_entry_id": feedback_entry_id,
            "disposition": "fixed_by_revision",
            "fixed_by_revision": revision_seq,
        }),
        FeedbackRefinementDisposition::WontFix { reason } => json!({
            "kind": FEEDBACK_DISPOSITION_METADATA_KIND,
            "human_feedback_entry_id": feedback_entry_id,
            "disposition": "wont_fix",
            "reason": reason,
        }),
    }
}

fn feedback_disposition_from_entry(
    row: &djinn_core::models::ProposalDebateTrail,
    feedback_entry_id: &str,
) -> Option<FeedbackRefinementDisposition> {
    let metadata: Value = serde_json::from_str(row.body_metadata.as_deref()?).ok()?;
    let object = metadata.as_object()?;
    if object.get("kind")?.as_str()? != FEEDBACK_DISPOSITION_METADATA_KIND
        || object.get("human_feedback_entry_id")?.as_str()? != feedback_entry_id
    {
        return None;
    }
    match object.get("disposition")?.as_str()? {
        "fixed_by_revision" => object
            .get("fixed_by_revision")?
            .as_i64()?
            .try_into()
            .ok()
            .map(|revision_seq| FeedbackRefinementDisposition::FixedRevision { revision_seq }),
        "wont_fix" => object
            .get("reason")?
            .as_str()
            .filter(|reason| !reason.trim().is_empty())
            .map(|reason| FeedbackRefinementDisposition::WontFix {
                reason: reason.trim().to_owned(),
            }),
        _ => None,
    }
}

fn rejected_feedback_disposition_id(
    row: &djinn_core::models::ProposalDebateTrail,
    feedback_entry_id: &str,
) -> Option<String> {
    let metadata: Value = serde_json::from_str(row.body_metadata.as_deref()?).ok()?;
    let object = metadata.as_object()?;
    (object.get("kind")?.as_str()? == FEEDBACK_DISPOSITION_REJECTION_METADATA_KIND
        && object.get("human_feedback_entry_id")?.as_str()? == feedback_entry_id)
        .then(|| {
            object
                .get("rejected_disposition_entry_id")?
                .as_str()
                .map(str::to_owned)
        })
        .flatten()
}

fn pending_feedback_disposition<'a>(
    rows: &'a [djinn_core::models::ProposalDebateTrail],
    feedback_entry_id: &str,
) -> Option<(
    &'a djinn_core::models::ProposalDebateTrail,
    FeedbackRefinementDisposition,
)> {
    let rejected: std::collections::HashSet<String> = rows
        .iter()
        .filter_map(|row| rejected_feedback_disposition_id(row, feedback_entry_id))
        .collect();
    rows.iter()
        .filter_map(|row| {
            (!rejected.contains(row.id.as_str()))
                .then(|| feedback_disposition_from_entry(row, feedback_entry_id))
                .flatten()
                .map(|disposition| (row, disposition))
        })
        .max_by_key(|(row, _)| (&row.created_at, &row.id))
}

fn feedback_obligation_is_open(
    entry: &djinn_core::models::ProposalDebateTrail,
    generation_state: &str,
) -> bool {
    entry.resolved_at.is_none() && entry.reopened_at.is_none() && generation_state == "injected"
}

fn is_feedback_acceptance_replay(
    entry: &djinn_core::models::ProposalDebateTrail,
    generation_state: &str,
    verdict: &str,
) -> bool {
    verdict == "accept"
        && entry.resolved_at.is_some()
        && entry.reopened_at.is_none()
        && matches!(generation_state, "accepted" | "wont_fix")
}

async fn debate_model(
    repo: &ProposalRepository,
    entry: &djinn_core::models::ProposalDebateTrail,
) -> Result<ProposalDebateTrailModel, String> {
    let mut model: ProposalDebateTrailModel = entry.into();
    if entry.kind == "human_feedback"
        && let Some(generation) = repo
            .feedback_refinement_generation_for_debate(&entry.id)
            .await
            .map_err(|e| e.to_string())?
    {
        model.source_feedback_id = Some(generation.injection.root_feedback_id);
        model.generation = Some(generation.injection.generation);
        model.disposition_state = Some(generation.injection.state);
        model.accepted_disposition = generation.injection.accepted_disposition;
        model.accepted_revision_seq = generation.injection.accepted_revision_seq;
        model.accepted_reason = generation.injection.accepted_reason;
        model.source_rows = generation
            .sources
            .into_iter()
            .map(|s| ProposalFeedbackSourceRowModel {
                source_feedback_id: s.source_feedback_id,
                source_ordinal: s.source_ordinal,
                source_parent_id: s.source_parent_id,
                author_kind: s.source_author_kind,
                author_user_id: s.source_author_user_id,
                author_model: s.source_author_model,
                body: s.source_body,
                severity: s.source_severity,
                created_at: s.source_created_at,
            })
            .collect();
    }
    Ok(model)
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
    #[serde(default)]
    pub verdict: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalFeedbackDispositionParams {
    pub id: String,
    pub disposition: String,
    #[serde(default)]
    pub fixed_by_revision: Option<i32>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalDebateReopenParams {
    /// Debate-trail entry UUID.
    pub id: String,
    /// Optional user ID to attribute the reopen action to. When omitted,
    /// falls back to the current session user (if any). Passed through
    /// to the audit trail as `reopened_by_user_id`.
    #[serde(default)]
    pub user_id: Option<String>,
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
                body_metadata: None,
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
                entries: {
                    let mut models = Vec::with_capacity(entries.len());
                    for entry in &entries {
                        match debate_model(&repo, entry).await {
                            Ok(model) => models.push(model),
                            Err(error) => return Json(err_debate_list(error)),
                        }
                    }
                    Some(models)
                },
                error: None,
            }),
            Err(e) => Json(err_debate_list(e.to_string())),
        }
    }

    /// Propose an Advocate disposition without resolving the feedback obligation.
    #[tool(
        description = "Propose fixed_by_revision or wont_fix for unresolved human_feedback. Fixed revisions must be newer than the objection and exist on its proposal; wont_fix needs reasoning. Judge acceptance is required before write-back."
    )]
    pub async fn proposal_feedback_disposition(
        &self,
        Parameters(p): Parameters<ProposalFeedbackDispositionParams>,
    ) -> Json<ProposalDebateTrailResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(entry) = repo.get_debate_trail_entry(&p.id).await.ok().flatten() else {
            return Json(err_debate(debate_not_found_error(&p.id)));
        };
        if entry.kind != "human_feedback"
            || entry.resolved_at.is_some()
            || entry.reopened_at.is_some()
        {
            return Json(err_debate(
                "feedback disposition requires an unresolved human_feedback entry",
            ));
        }
        let disposition = match p.disposition.as_str() {
            "fixed_by_revision" => {
                let Some(seq) = p.fixed_by_revision else {
                    return Json(err_debate("fixed_by_revision is required"));
                };
                if seq <= entry.against_revision_seq {
                    return Json(err_debate(
                        "fixed_by_revision must be newer than against_revision_seq",
                    ));
                }
                match repo.revisions(&entry.proposal_id).await {
                    Ok(rows) if rows.iter().any(|row| row.seq == seq) => {
                        FeedbackRefinementDisposition::FixedRevision { revision_seq: seq }
                    }
                    Ok(_) => {
                        return Json(err_debate(
                            "fixed_by_revision does not exist on this proposal",
                        ));
                    }
                    Err(e) => return Json(err_debate(e.to_string())),
                }
            }
            "wont_fix" => match p.reason.filter(|reason| !reason.trim().is_empty()) {
                Some(reason) => FeedbackRefinementDisposition::WontFix {
                    reason: reason.trim().to_owned(),
                },
                None => return Json(err_debate("wont_fix requires nonblank reasoning")),
            },
            _ => {
                return Json(err_debate(
                    "disposition must be fixed_by_revision or wont_fix",
                ));
            }
        };
        let body = match &disposition {
            FeedbackRefinementDisposition::FixedRevision { revision_seq } => {
                format!(
                    "feedback_disposition:{}:fixed_by_revision:{revision_seq}",
                    entry.id
                )
            }
            FeedbackRefinementDisposition::WontFix { reason } => {
                format!("feedback_disposition:{}:wont_fix:{reason}", entry.id)
            }
        };
        let metadata = feedback_disposition_metadata(&entry.id, &disposition);
        match repo
            .add_debate_trail_entry(ProposalDebateTrailCreateInput {
                proposal_id: &entry.proposal_id,
                kind: "rebuttal",
                body: &body,
                blocking: false,
                agent_role: "advocate",
                author_kind: "agent",
                author_model: None,
                source_task_id: None,
                against_revision_seq: entry.against_revision_seq,
                round: entry.round,
                body_metadata: Some(&metadata),
            })
            .await
        {
            Ok(_) => Json(ProposalDebateTrailResponse {
                entry: Some((&entry).into()),
                error: None,
            }),
            Err(e) => Json(err_debate(e.to_string())),
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
        if entry.kind == "human_feedback" {
            let Some(verdict) = p.verdict.as_deref() else {
                return Json(err_debate(
                    "human_feedback resolution requires explicit accept or reject verdict",
                ));
            };
            if !matches!(verdict, "accept" | "reject") {
                return Json(err_debate(
                    "human_feedback verdict must be accept or reject",
                ));
            }

            // A captured generation is the durable lifecycle authority for a
            // human-feedback obligation. Do not append needs-work after an
            // accepted or withdrawn generation has closed the debate row. A
            // repeated acceptance of an already accepted disposition is the
            // one intentional no-op, matching repository write-back replay.
            let generation = match repo
                .feedback_refinement_generation_for_debate(&entry.id)
                .await
            {
                Ok(Some(generation)) => generation,
                Ok(None) => {
                    return Json(err_debate(
                        "human_feedback entry has no materialized generation",
                    ));
                }
                Err(e) => return Json(err_debate(e.to_string())),
            };
            if !feedback_obligation_is_open(&entry, &generation.injection.state) {
                if is_feedback_acceptance_replay(&entry, &generation.injection.state, verdict) {
                    return Json(ProposalDebateTrailResponse {
                        entry: Some((&entry).into()),
                        error: None,
                    });
                }
                return Json(err_debate(
                    "feedback disposition requires an unresolved human_feedback obligation",
                ));
            }
            if verdict == "reject" {
                let Some(reason) = p.reason.filter(|reason| !reason.trim().is_empty()) else {
                    return Json(err_debate("reject requires needs-work reasoning"));
                };
                let rows = match repo.debate_trail(&entry.proposal_id).await {
                    Ok(rows) => rows,
                    Err(e) => return Json(err_debate(e.to_string())),
                };
                let Some((candidate, _)) = pending_feedback_disposition(&rows, &entry.id) else {
                    return Json(err_debate(
                        "human_feedback has no pending Advocate disposition",
                    ));
                };
                return match repo
                    .reject_feedback_refinement_disposition(FeedbackRefinementRejectionInput {
                        proposal_id: entry.proposal_id.clone(),
                        injection_id: generation.injection.id,
                        root_feedback_id: generation.injection.root_feedback_id,
                        generation: generation.injection.generation,
                        debate_entry_id: entry.id.clone(),
                        disposition_entry_id: candidate.id.clone(),
                        reason,
                    })
                    .await
                {
                    Ok(result) => Json(ProposalDebateTrailResponse {
                        entry: Some((&result.debate_entry).into()),
                        error: None,
                    }),
                    Err(e) => Json(err_debate(e.to_string())),
                };
            }
            let rows = match repo.debate_trail(&entry.proposal_id).await {
                Ok(rows) => rows,
                Err(e) => return Json(err_debate(e.to_string())),
            };
            let Some((candidate, disposition)) = pending_feedback_disposition(&rows, &entry.id)
            else {
                return Json(err_debate(
                    "human_feedback has no pending Advocate disposition",
                ));
            };
            if let FeedbackRefinementDisposition::FixedRevision { revision_seq } = &disposition {
                if *revision_seq <= entry.against_revision_seq {
                    return Json(err_debate(
                        "fixed_by_revision must be newer than against_revision_seq",
                    ));
                }
                match repo.revisions(&entry.proposal_id).await {
                    Ok(rows) if rows.iter().any(|row| row.seq == *revision_seq) => {}
                    Ok(_) => {
                        return Json(err_debate(
                            "fixed_by_revision does not exist on this proposal",
                        ));
                    }
                    Err(e) => return Json(err_debate(e.to_string())),
                }
            }
            return match repo
                .dispose_feedback_refinement_generation_for_disposition(
                    FeedbackRefinementDispositionInput {
                        proposal_id: entry.proposal_id.clone(),
                        injection_id: generation.injection.id,
                        root_feedback_id: generation.injection.root_feedback_id,
                        generation: generation.injection.generation,
                        debate_entry_id: entry.id.clone(),
                        disposition,
                    },
                    candidate.id.clone(),
                )
                .await
            {
                Ok(result) => Json(ProposalDebateTrailResponse {
                    entry: Some((&result.debate_entry).into()),
                    error: None,
                }),
                Err(e) => Json(err_debate(e.to_string())),
            };
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
        match repo
            .reopen_debate_trail_entry_with_user(&p.id, p.user_id.as_deref())
            .await
        {
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
            .get("entry")
            .and_then(|e| e.get("id"))
            .and_then(|v| v.as_str())
            .expect("response should have entry.id");

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
        let entry_id = resp
            .get("entry")
            .and_then(|e| e.get("id"))
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();

        // Resolve.
        let resolve_resp = server
            .dispatch_tool(
                "proposal_debate_resolve",
                serde_json::json!({ "id": entry_id }),
            )
            .await
            .expect("tool should be registered");
        assert!(resolve_resp.get("error").and_then(|v| v.as_str()).is_none());
        let resolve_entry = resolve_resp
            .get("entry")
            .expect("resolve response should have entry");
        assert!(
            resolve_entry
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
        let reopen_entry = reopen_resp
            .get("entry")
            .expect("reopen response should have entry");
        assert!(
            reopen_entry
                .get("reopened_at")
                .and_then(|v| v.as_str())
                .is_some(),
            "should be reopened"
        );
        // resolved_at should still be set.
        assert!(
            reopen_entry
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

    fn disposition_row(
        id: &str,
        metadata: Option<Value>,
    ) -> djinn_core::models::ProposalDebateTrail {
        djinn_core::models::ProposalDebateTrail {
            id: id.to_owned(),
            proposal_id: "proposal".to_owned(),
            kind: "rebuttal".to_owned(),
            body: "feedback_disposition:feedback:fixed_by_revision:2".to_owned(),
            blocking: false,
            agent_role: "advocate".to_owned(),
            author_kind: "agent".to_owned(),
            author_user_id: None,
            author_model: None,
            source_task_id: None,
            against_revision_seq: 1,
            round: 1,
            body_metadata: metadata.map(|value| value.to_string()),
            resolved_at: None,
            resolved_by_user_id: None,
            reopened_at: None,
            reopened_by_user_id: None,
            created_at: id.to_owned(),
            updated_at: id.to_owned(),
        }
    }

    #[test]
    fn pending_disposition_requires_control_plane_metadata() {
        let forged = disposition_row("forged", None);
        assert!(pending_feedback_disposition(&[forged], "feedback").is_none());
    }

    #[test]
    fn rejected_disposition_cannot_be_accepted_without_a_new_proposal() {
        let disposition = FeedbackRefinementDisposition::FixedRevision { revision_seq: 2 };
        let rejected = disposition_row(
            "rejected",
            Some(feedback_disposition_metadata("feedback", &disposition)),
        );
        let rejection = disposition_row(
            "verdict",
            Some(json!({
                "kind": FEEDBACK_DISPOSITION_REJECTION_METADATA_KIND,
                "human_feedback_entry_id": "feedback",
                "rejected_disposition_entry_id": "rejected",
            })),
        );
        assert!(pending_feedback_disposition(&[rejected.clone(), rejection], "feedback").is_none());

        let replacement = disposition_row(
            "replacement",
            Some(feedback_disposition_metadata("feedback", &disposition)),
        );
        let candidates = [rejected, replacement];
        let pending = pending_feedback_disposition(&candidates, "feedback")
            .expect("a replacement disposition should be pending");
        assert_eq!(pending.0.id, "replacement");
    }

    #[test]
    fn closed_feedback_obligation_reject_cannot_append_needs_work() {
        let mut accepted = disposition_row("accepted", None);
        accepted.resolved_at = Some("2026-08-05T00:00:00.000Z".to_owned());

        assert!(!feedback_obligation_is_open(&accepted, "accepted"));
        assert!(!is_feedback_acceptance_replay(
            &accepted, "accepted", "reject"
        ));
        assert!(is_feedback_acceptance_replay(
            &accepted, "accepted", "accept"
        ));

        let mut reopened = accepted;
        reopened.reopened_at = Some("2026-08-05T00:01:00.000Z".to_owned());
        assert!(!is_feedback_acceptance_replay(
            &reopened, "accepted", "accept"
        ));
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
