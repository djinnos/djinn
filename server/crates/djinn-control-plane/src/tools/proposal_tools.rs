// MCP tools for the global Proposals layer (Phase 0).
//
// A proposal is a project-INDEPENDENT, collaboratively-authored artifact
// (spec body + acceptance criteria) that targets zero, one, or many projects
// via an editable M:N `proposal_targets` set. Discussion and suggestions share
// one `proposal_feedback` primitive (status == null → discussion; open/
// accepted/rejected → a trackable suggestion; author_kind == "ai" for future
// adversarial-review findings). This layer stands beside the epic/task
// execution engine and does NOT touch it — graduation into epics is a later
// phase.

use std::borrow::Cow;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use crate::tools::acting_user::acting_caps;
use crate::tools::epic_ops::AcceptanceCriterionItem;
use crate::tools::list_response::{
    self, ListMeta, NamedListResponse, named_list_response_schema, serialize_named_list_response,
};
use crate::tools::proposal_ops::{
    ProposalDeleteResponse, ProposalFeedbackResponse, ProposalModel, ProposalShowResponse,
    ProposalSignoffModel, ProposalSingleResponse, ProposalTargetModel, ProposalTargetsResponse,
};
use crate::tools::validation::{
    validate_ac_count, validate_body, validate_design, validate_feedback_status, validate_limit,
    validate_offset, validate_proposal_create_status, validate_proposal_status, validate_sort,
    validate_title,
};
use djinn_db::{ProjectRepository, ProposalListQuery, ProposalRepository};

// ── List response (NamedListResponse boilerplate, mirrors EpicListResponse) ──

#[derive(Clone)]
pub struct ProposalListResponse {
    pub proposals: Option<Vec<ProposalModel>>,
    pub meta: ListMeta,
}

impl NamedListResponse for ProposalListResponse {
    type Item = ProposalModel;
    const FIELD_NAME: &'static str = "proposals";
    const TITLE: &'static str = "ProposalListResponse";

    fn from_parts(items: Option<Vec<Self::Item>>, meta: ListMeta) -> Self {
        Self {
            proposals: items,
            meta,
        }
    }
    fn items(&self) -> Option<&Vec<Self::Item>> {
        self.proposals.as_ref()
    }
    fn meta(&self) -> &ListMeta {
        &self.meta
    }
}

impl Serialize for ProposalListResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serialize_named_list_response(self, serializer)
    }
}

impl schemars::JsonSchema for ProposalListResponse {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed(Self::TITLE)
    }
    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        named_list_response_schema::<ProposalModel>(generator, Self::TITLE, Self::FIELD_NAME)
    }
}

fn proposal_not_found_error(id: &str) -> String {
    format!("proposal not found: {id}")
}

/// List a proposal's targets and resolve each project id to an `owner/repo`
/// slug + name for display chips.
async fn target_models(
    proposal_repo: &ProposalRepository,
    project_repo: &ProjectRepository,
    proposal_id: &str,
) -> Result<Vec<ProposalTargetModel>, String> {
    let targets = proposal_repo
        .targets(proposal_id)
        .await
        .map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(targets.len());
    for t in &targets {
        let mut m = ProposalTargetModel::from(t);
        if let Ok(Some(p)) = project_repo.get(&t.project_id).await {
            m.project_path = Some(format!("{}/{}", p.github_owner, p.github_repo));
            m.project_name = Some(p.name);
        }
        out.push(m);
    }
    Ok(out)
}

// ── Param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalCreateParams {
    pub title: String,
    /// Markdown spec body.
    pub body: Option<String>,
    /// Acceptance criteria: plain strings or `{criterion, met}` objects.
    pub acceptance_criteria: Option<Vec<AcceptanceCriterionItem>>,
    /// Target projects (UUIDs or owner/repo slugs) this proposal touches.
    /// Editable later via proposal_add_target / proposal_remove_target.
    pub target_projects: Option<Vec<String>>,
    /// Initial status: `draft` (default), `shared`, or `ready`.
    pub status: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalShowParams {
    /// Proposal UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalListParams {
    pub status: Option<String>,
    /// Filter by author user id.
    pub author: Option<String>,
    /// Filter to proposals targeting this project (UUID or owner/repo slug).
    pub target_project: Option<String>,
    /// Full-text search on title and body.
    pub text: Option<String>,
    /// Sort order: "created_desc" (default), "created", "updated", "updated_desc".
    pub sort: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalUpdateParams {
    /// Proposal UUID or short_id.
    pub id: String,
    pub title: Option<String>,
    pub body: Option<String>,
    /// Acceptance criteria: plain strings or `{criterion, met}` objects.
    pub acceptance_criteria: Option<Vec<AcceptanceCriterionItem>>,
    /// draft | in_review | approved | building | done | rejected | archived | superseded.
    pub status: Option<String>,
    /// UUID or short_id of the proposal that supersedes this one.
    pub superseded_by: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalDeleteParams {
    /// Proposal UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalTargetParams {
    /// Proposal UUID or short_id.
    pub id: String,
    /// Target project: UUID or owner/repo slug (must be registered).
    pub project: String,
    /// `primary` (a write-target, default) or `reference` (read-only context).
    pub role: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalFeedbackAddParams {
    /// Proposal UUID or short_id.
    pub proposal_id: String,
    pub body: String,
    /// Optional pointer to the spec section this is about.
    pub target_section: Option<String>,
    /// Parent feedback id for a threaded reply.
    pub parent_id: Option<String>,
    /// Omit for plain discussion; set `open` to file a trackable suggestion.
    pub status: Option<String>,
    /// `user` (default) or `ai`.
    pub author_kind: Option<String>,
    /// Model id when author_kind is `ai`.
    pub author_model: Option<String>,
    /// For an edit suggestion, the proposed new spec body. Accepting the
    /// feedback applies it (appending a revision).
    pub proposed_body: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalFeedbackAcceptParams {
    /// Feedback entry UUID.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalFeedbackResolveParams {
    /// Feedback entry UUID.
    pub id: String,
    /// `open` | `accepted` | `rejected`, or `none` to revert to discussion.
    pub status: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalSignoffParams {
    /// Proposal UUID or short_id.
    pub id: String,
    /// `scoped` (product) or `technical` (engineering).
    pub kind: String,
}

// ── Tool router ──────────────────────────────────────────────────────────────

#[tool_router(router = proposal_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Create a global proposal.
    #[tool(
        description = "Create a global, project-independent proposal (collaborative spec). Optionally pass `target_projects` (UUIDs or owner/repo slugs) the proposal touches and `acceptance_criteria`. The author is the authenticated caller. Status defaults to `draft`."
    )]
    pub async fn proposal_create(
        &self,
        Parameters(p): Parameters<ProposalCreateParams>,
    ) -> Json<ProposalSingleResponse> {
        let title = match validate_title(&p.title) {
            Ok(t) => t,
            Err(e) => return Json(err_single(e)),
        };
        let body = p.body.as_deref().unwrap_or("");
        if let Err(e) = validate_design(body) {
            return Json(err_single(e));
        }
        let ac = p.acceptance_criteria.unwrap_or_default();
        if let Err(e) = validate_ac_count(ac.len()) {
            return Json(err_single(e));
        }
        let status = match validate_proposal_create_status(p.status.as_deref()) {
            Ok(s) => s,
            Err(e) => return Json(err_single(e)),
        };
        let ac_json = serde_json::to_string(&ac).unwrap_or_else(|_| "[]".to_string());

        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let proposal = match repo
            .create(djinn_db::ProposalCreateInput {
                title: &title,
                body,
                acceptance_criteria: Some(&ac_json),
                status,
            })
            .await
        {
            Ok(p) => p,
            Err(e) => return Json(err_single(e.to_string())),
        };

        // Seed target projects (best-effort: unresolvable refs are skipped).
        if let Some(targets) = &p.target_projects {
            let project_repo =
                ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
            for t in targets {
                if let Ok(Some(pid)) = project_repo.resolve(t).await {
                    let _ = repo.add_target(&proposal.id, &pid, "primary").await;
                }
            }
        }

        Json(ProposalSingleResponse {
            proposal: Some(ProposalModel::from(&proposal)),
            error: None,
        })
    }

    /// Show a proposal with targets, feedback, revisions, and sign-offs.
    #[tool(
        description = "Show a proposal (by UUID or short_id) including target projects, the feedback/discussion thread, its revision history, and review sign-offs (each flagged `stale` when given against an older revision)."
    )]
    pub async fn proposal_show(
        &self,
        Parameters(p): Parameters<ProposalShowParams>,
    ) -> Json<ProposalShowResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_show(proposal_not_found_error(&p.id)));
        };
        let targets = match target_models(&repo, &project_repo, &proposal.id).await {
            Ok(t) => t,
            Err(e) => return Json(err_show(e)),
        };
        let feedback = match repo.feedback(&proposal.id).await {
            Ok(f) => f.iter().map(Into::into).collect(),
            Err(e) => return Json(err_show(e.to_string())),
        };
        let revisions = match repo.revisions(&proposal.id).await {
            Ok(r) => r.iter().map(Into::into).collect(),
            Err(e) => return Json(err_show(e.to_string())),
        };
        let signoffs = match repo.signoffs(&proposal.id).await {
            Ok(s) => s
                .iter()
                .map(|so| ProposalSignoffModel::from_signoff(so, proposal.latest_revision_seq))
                .collect(),
            Err(e) => return Json(err_show(e.to_string())),
        };
        Json(ProposalShowResponse {
            proposal: Some(ProposalModel::from(&proposal)),
            targets: Some(targets),
            feedback: Some(feedback),
            revisions: Some(revisions),
            signoffs: Some(signoffs),
            error: None,
        })
    }

    /// List proposals (global) with optional filters and pagination.
    #[tool(
        description = "List proposals globally (not scoped to a project) with optional filters: status, author, target_project (UUID or owner/repo slug), text. Offset-based pagination. Returns {proposals[], total_count, limit, offset, has_more}."
    )]
    pub async fn proposal_list(
        &self,
        Parameters(p): Parameters<ProposalListParams>,
    ) -> Json<ProposalListResponse> {
        let sort = p.sort.as_deref().unwrap_or("created_desc");
        if let Err(e) = validate_sort(
            sort,
            &["created", "created_desc", "updated", "updated_desc"],
        ) {
            return Json(list_response::error::<ProposalListResponse>(e));
        }
        let limit = validate_limit(p.limit.unwrap_or(25));
        let offset = validate_offset(p.offset.unwrap_or(0));

        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());

        // Resolve the target_project filter to a UUID if a slug was passed.
        let target_project_id = if let Some(ref tref) = p.target_project {
            let project_repo =
                ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
            match project_repo.resolve(tref).await {
                Ok(Some(id)) => Some(id),
                _ => {
                    return Json(list_response::error::<ProposalListResponse>(format!(
                        "target_project not found: {tref}"
                    )));
                }
            }
        } else {
            None
        };

        let query = ProposalListQuery {
            status: p.status,
            text: p.text,
            author_user_id: p.author,
            target_project_id,
            sort: sort.to_owned(),
            limit,
            offset,
        };
        match repo.list_filtered(query).await {
            Ok(result) => Json(list_response::success::<ProposalListResponse>(
                result.proposals.iter().map(ProposalModel::from).collect(),
                result.total_count,
                limit,
                offset,
            )),
            Err(e) => Json(list_response::error::<ProposalListResponse>(e.to_string())),
        }
    }

    /// Update a proposal's editable fields.
    #[tool(
        description = "Update a proposal (by UUID or short_id): title, body, acceptance_criteria, status (draft|shared|ready|archived|superseded), and superseded_by. Only provided fields change."
    )]
    pub async fn proposal_update(
        &self,
        Parameters(p): Parameters<ProposalUpdateParams>,
    ) -> Json<ProposalSingleResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(existing) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_single(proposal_not_found_error(&p.id)));
        };
        if let Err(e) = self
            .gate_proposal_edit(existing.author_user_id.as_deref())
            .await
        {
            return Json(err_single(e));
        }

        let title = if let Some(ref t) = p.title {
            match validate_title(t) {
                Ok(v) => v,
                Err(e) => return Json(err_single(e)),
            }
        } else {
            existing.title.clone()
        };

        let body = p.body.as_deref().unwrap_or(&existing.body);
        if let Err(e) = validate_design(body) {
            return Json(err_single(e));
        }

        let ac_json = if let Some(ac) = &p.acceptance_criteria {
            if let Err(e) = validate_ac_count(ac.len()) {
                return Json(err_single(e));
            }
            serde_json::to_string(ac).unwrap_or_else(|_| "[]".to_string())
        } else {
            existing.acceptance_criteria.clone()
        };

        let status = p.status.as_deref().unwrap_or(&existing.status);
        if let Err(e) = validate_proposal_status(status) {
            return Json(err_single(e));
        }

        // Resolve superseded_by to a canonical proposal id when provided.
        let superseded_by = if let Some(ref s) = p.superseded_by {
            match repo.resolve(s).await.ok().flatten() {
                Some(target) => Some(target.id),
                None => return Json(err_single(format!("superseded_by proposal not found: {s}"))),
            }
        } else {
            existing.superseded_by.clone()
        };

        match repo
            .update(
                &existing.id,
                djinn_db::ProposalUpdateInput {
                    title: &title,
                    body,
                    acceptance_criteria: &ac_json,
                    status,
                    superseded_by: superseded_by.as_deref(),
                },
            )
            .await
        {
            Ok(updated) => Json(ProposalSingleResponse {
                proposal: Some(ProposalModel::from(&updated)),
                error: None,
            }),
            Err(e) => Json(err_single(e.to_string())),
        }
    }

    /// Delete a proposal (cascades to its targets and feedback).
    #[tool(
        description = "Delete a proposal (by UUID or short_id). Cascades to its targets and feedback. Returns {ok}."
    )]
    pub async fn proposal_delete(
        &self,
        Parameters(p): Parameters<ProposalDeleteParams>,
    ) -> Json<ProposalDeleteResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(existing) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(ProposalDeleteResponse {
                ok: None,
                error: Some(proposal_not_found_error(&p.id)),
            });
        };
        if let Err(e) = self
            .gate_proposal_edit(existing.author_user_id.as_deref())
            .await
        {
            return Json(ProposalDeleteResponse {
                ok: None,
                error: Some(e),
            });
        }
        match repo.delete(&existing.id).await {
            Ok(()) => Json(ProposalDeleteResponse {
                ok: Some(true),
                error: None,
            }),
            Err(e) => Json(ProposalDeleteResponse {
                ok: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// Add (or re-role) a target project on a proposal.
    #[tool(
        description = "Add a target project to a proposal (or change its role if already present). `project` is a UUID or owner/repo slug; `role` is `primary` (default) or `reference`. This is the re-target capability — editable at any time. Returns the proposal's updated target list."
    )]
    pub async fn proposal_add_target(
        &self,
        Parameters(p): Parameters<ProposalTargetParams>,
    ) -> Json<ProposalTargetsResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_targets(proposal_not_found_error(&p.id)));
        };
        if let Err(e) = self
            .gate_proposal_edit(proposal.author_user_id.as_deref())
            .await
        {
            return Json(err_targets(e));
        }
        let role = p.role.as_deref().unwrap_or("primary");
        if !matches!(role, "primary" | "reference") {
            return Json(err_targets(format!(
                "invalid role: {role:?} (expected primary or reference)"
            )));
        }
        let project_id = match project_repo.resolve(&p.project).await {
            Ok(Some(id)) => id,
            _ => return Json(err_targets(format!("project not found: {}", p.project))),
        };
        if let Err(e) = repo.add_target(&proposal.id, &project_id, role).await {
            return Json(err_targets(e.to_string()));
        }
        finish_targets(&repo, &project_repo, &proposal.id).await
    }

    /// Remove a target project from a proposal.
    #[tool(
        description = "Remove a target project from a proposal. `project` is a UUID or owner/repo slug. No-op if it wasn't a target. Returns the proposal's updated target list."
    )]
    pub async fn proposal_remove_target(
        &self,
        Parameters(p): Parameters<ProposalTargetParams>,
    ) -> Json<ProposalTargetsResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_targets(proposal_not_found_error(&p.id)));
        };
        if let Err(e) = self
            .gate_proposal_edit(proposal.author_user_id.as_deref())
            .await
        {
            return Json(err_targets(e));
        }
        // Fall back to the raw value so a stale target can still be removed.
        let project_id = project_repo
            .resolve(&p.project)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| p.project.clone());
        if let Err(e) = repo.remove_target(&proposal.id, &project_id).await {
            return Json(err_targets(e.to_string()));
        }
        finish_targets(&repo, &project_repo, &proposal.id).await
    }

    /// Add a feedback entry (discussion or suggestion) to a proposal.
    #[tool(
        description = "Add feedback to a proposal. Omit `status` for plain discussion; set `status=open` to file a trackable suggestion (resolve later with proposal_feedback_resolve). `author_kind` is `user` (default) or `ai` (set `author_model` for AI). `parent_id` threads a reply."
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
        if let Some(ref s) = p.status
            && let Err(e) = validate_feedback_status(s)
        {
            return Json(err_feedback(e));
        }
        match repo
            .add_feedback(djinn_db::ProposalFeedbackCreateInput {
                proposal_id: &proposal.id,
                parent_id: p.parent_id.as_deref(),
                author_kind,
                author_model: p.author_model.as_deref(),
                body: &p.body,
                target_section: p.target_section.as_deref(),
                status: p.status.as_deref(),
                proposed_body: p.proposed_body.as_deref(),
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

    /// Accept a feedback entry, applying its proposed edit if it has one.
    #[tool(
        description = "Accept a feedback entry. If it's an edit suggestion (carries `proposed_body`), applies the proposed spec body — appending a revision — and marks it accepted. Otherwise just marks it accepted. Requires edit rights on the proposal."
    )]
    pub async fn proposal_feedback_accept(
        &self,
        Parameters(p): Parameters<ProposalFeedbackAcceptParams>,
    ) -> Json<ProposalFeedbackResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(feedback) = repo.get_feedback(&p.id).await.ok().flatten() else {
            return Json(err_feedback(format!("feedback not found: {}", p.id)));
        };
        // Accepting applies an edit → requires edit rights on the proposal.
        let author = repo
            .resolve(&feedback.proposal_id)
            .await
            .ok()
            .flatten()
            .and_then(|pr| pr.author_user_id);
        if let Err(e) = self.gate_proposal_edit(author.as_deref()).await {
            return Json(err_feedback(e));
        }
        match repo.accept_feedback(&p.id).await {
            Ok(f) => Json(ProposalFeedbackResponse {
                feedback: Some((&f).into()),
                error: None,
            }),
            Err(e) => Json(err_feedback(e.to_string())),
        }
    }

    /// Resolve (or reopen/clear) a feedback suggestion.
    #[tool(
        description = "Set the resolution status on a feedback entry: `open`, `accepted`, or `rejected` — or `none` to revert it to plain discussion. Returns the updated entry."
    )]
    pub async fn proposal_feedback_resolve(
        &self,
        Parameters(p): Parameters<ProposalFeedbackResolveParams>,
    ) -> Json<ProposalFeedbackResponse> {
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        if repo.get_feedback(&p.id).await.ok().flatten().is_none() {
            return Json(err_feedback(format!("feedback not found: {}", p.id)));
        }
        let status: Option<&str> = match p.status.as_str() {
            "none" | "clear" | "" => None,
            other => {
                if let Err(e) = validate_feedback_status(other) {
                    return Json(err_feedback(e));
                }
                Some(other)
            }
        };
        match repo.set_feedback_status(&p.id, status).await {
            Ok(f) => Json(ProposalFeedbackResponse {
                feedback: Some((&f).into()),
                error: None,
            }),
            Err(e) => Json(err_feedback(e.to_string())),
        }
    }

    /// Give a review sign-off (scoped or technical) as the authenticated user.
    #[tool(
        description = "Record a review sign-off on a proposal as the authenticated user. `kind` is `scoped` (product/scope) or `technical` (engineering). The sign-off anchors to the current head revision; later spec edits mark it stale. When a proposal in `in_review` has both a fresh scoped and technical sign-off, it auto-advances to `approved`."
    )]
    pub async fn proposal_signoff(
        &self,
        Parameters(p): Parameters<ProposalSignoffParams>,
    ) -> Json<ProposalSingleResponse> {
        if !matches!(p.kind.as_str(), "scoped" | "technical") {
            return Json(err_single(format!(
                "invalid sign-off kind: {:?} (expected scoped or technical)",
                p.kind
            )));
        }
        let Some(user_id) = djinn_core::auth_context::current_user_id() else {
            return Json(err_single(
                "sign-off requires an authenticated user".to_string(),
            ));
        };
        // Role gate: scoped = PM/engineer/admin; technical = engineer/admin.
        match acting_caps(self.state.db()).await {
            Ok(Some(caps)) if !caps.can_signoff(&p.kind) => {
                return Json(err_single(format!(
                    "a {} sign-off requires the {} role",
                    p.kind,
                    if p.kind == "technical" {
                        "engineer (or admin)"
                    } else {
                        "PM, engineer (or admin)"
                    }
                )));
            }
            Err(e) => return Json(err_single(e)),
            _ => {}
        }
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_single(proposal_not_found_error(&p.id)));
        };
        match repo.add_signoff(&proposal.id, &p.kind, &user_id).await {
            Ok(updated) => Json(ProposalSingleResponse {
                proposal: Some(ProposalModel::from(&updated)),
                error: None,
            }),
            Err(e) => Json(err_single(e.to_string())),
        }
    }

    /// Withdraw the authenticated user's sign-off of a given kind.
    #[tool(
        description = "Withdraw the authenticated user's sign-off (`scoped` or `technical`) from a proposal. May demote an `approved` proposal back to `in_review` if the approval gate is no longer met."
    )]
    pub async fn proposal_signoff_clear(
        &self,
        Parameters(p): Parameters<ProposalSignoffParams>,
    ) -> Json<ProposalSingleResponse> {
        let Some(user_id) = djinn_core::auth_context::current_user_id() else {
            return Json(err_single(
                "sign-off requires an authenticated user".to_string(),
            ));
        };
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_single(proposal_not_found_error(&p.id)));
        };
        match repo.clear_signoff(&proposal.id, &p.kind, &user_id).await {
            Ok(updated) => Json(ProposalSingleResponse {
                proposal: Some(ProposalModel::from(&updated)),
                error: None,
            }),
            Err(e) => Json(err_single(e.to_string())),
        }
    }
}

// ── Permission gates ─────────────────────────────────────────────────────────

impl DjinnMcpServer {
    /// Gate a direct spec edit: allowed for the author, a PM, an engineer, or
    /// an admin. `Ok(())` when unauthenticated (trusted/system path).
    async fn gate_proposal_edit(&self, author_user_id: Option<&str>) -> Result<(), String> {
        if let Some(caps) = acting_caps(self.state.db()).await? {
            let is_author = author_user_id == Some(caps.user_id.as_str());
            if !caps.can_edit(is_author) {
                return Err(
                    "editing this proposal requires its author, a PM, an engineer, or an admin"
                        .to_string(),
                );
            }
        }
        Ok(())
    }
}

// ── Small response constructors ──────────────────────────────────────────────

fn err_show(error: impl Into<String>) -> ProposalShowResponse {
    ProposalShowResponse {
        proposal: None,
        targets: None,
        feedback: None,
        revisions: None,
        signoffs: None,
        error: Some(error.into()),
    }
}

fn err_single(error: impl Into<String>) -> ProposalSingleResponse {
    ProposalSingleResponse {
        proposal: None,
        error: Some(error.into()),
    }
}

fn err_targets(error: impl Into<String>) -> ProposalTargetsResponse {
    ProposalTargetsResponse {
        targets: None,
        error: Some(error.into()),
    }
}

fn err_feedback(error: impl Into<String>) -> ProposalFeedbackResponse {
    ProposalFeedbackResponse {
        feedback: None,
        error: Some(error.into()),
    }
}

async fn finish_targets(
    repo: &ProposalRepository,
    project_repo: &ProjectRepository,
    proposal_id: &str,
) -> Json<ProposalTargetsResponse> {
    match target_models(repo, project_repo, proposal_id).await {
        Ok(targets) => Json(ProposalTargetsResponse {
            targets: Some(targets),
            error: None,
        }),
        Err(e) => Json(err_targets(e)),
    }
}
