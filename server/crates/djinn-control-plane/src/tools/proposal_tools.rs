// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
// MCP tools for the global Proposals layer (Phase 0).
//
// A proposal is a project-INDEPENDENT, collaboratively-authored artifact
// (spec body + acceptance criteria) that targets zero, one, or many projects
// via an editable M:N `proposal_targets` set. Discussion and suggestions share
// one `proposal_feedback` primitive (status == null → discussion; open/
// accepted/rejected → a trackable suggestion; author_kind == "ai" for future
// adversarial-review findings). Sign-offs gate approval, revisions/diffs track
// edits, and `proposal_graduate` kicks an approved proposal off into one epic
// per primary target (the existing single-repo-write execution engine).

use std::borrow::Cow;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use crate::tools::acting_user::acting_caps;
use crate::tools::epic_ops::AcceptanceCriterionItem;
use crate::tools::list_response::{
    self, ListMeta, NamedListResponse, named_list_response_schema, serialize_named_list_response,
};
use crate::tools::proposal_blocks::validate_question_form_placement;
use crate::tools::proposal_ops::{
    ProposalDeleteResponse, ProposalEpicModel, ProposalFeedbackResponse, ProposalModel,
    ProposalReconcileObsoleteEpicResponse, ProposalShowResponse, ProposalSignoffModel,
    ProposalSingleResponse, ProposalTargetModel, ProposalTargetsResponse,
};
use crate::tools::validation::{
    validate_ac_count, validate_body, validate_design, validate_limit, validate_mdx_body,
    validate_offset, validate_proposal_create_status, validate_proposal_status, validate_sort,
    validate_title,
};
use djinn_db::{
    EpicRepository, ProjectRepository, ProposalListQuery, ProposalRepository, TaskRepository,
};

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
    /// Spec body (markdown or MDX depending on `body_format`).
    pub body: Option<String>,
    /// Acceptance criteria: plain strings or `{criterion, met}` objects.
    pub acceptance_criteria: Option<Vec<AcceptanceCriterionItem>>,
    /// Target projects (UUIDs or owner/repo slugs) this proposal touches.
    /// Editable later via proposal_add_target / proposal_remove_target.
    pub target_projects: Option<Vec<String>>,
    /// Initial status: `triage`, `draft` (default), or `in_review`. Proposer-
    /// role authors are always placed in `triage` regardless of this value.
    pub status: Option<String>,
    /// Body encoding: `markdown` (default) or `mdx` (block-aware).
    pub body_format: Option<String>,
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
    /// Body encoding: `markdown` (default) or `mdx` (block-aware).
    pub body_format: Option<String>,
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

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalSignoffParams {
    /// Proposal UUID or short_id.
    pub id: String,
    /// `scoped` (product) or `technical` (engineering).
    pub kind: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalGraduateParams {
    /// Proposal UUID or short_id (must be `approved`).
    pub id: String,
    /// Build owner — must be a participant (author or a sign-off giver).
    /// Defaults to the kicking-off user.
    pub owner_user_id: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalStopBuildParams {
    /// Proposal UUID or short_id (must be `building`).
    pub id: String,
    /// `abort` (tear the build down and revert to `approved`), `freeze` (hold
    /// the build's tasks out of dispatch, leaving epics/tasks/branches in
    /// place), or `unfreeze` (resume a frozen build).
    pub mode: String,
    /// Why the build is being stopped. Recorded as the force-close reason on
    /// each torn-down task. Required for `abort`.
    pub reason: Option<String>,
    /// When true on `abort`, compute and return the blast radius (epics, open
    /// tasks, running sessions) WITHOUT mutating anything.
    pub preview: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProposalReconcileObsoleteEpicParams {
    /// Proposal UUID or short_id. Alias for `proposal_id`.
    pub id: Option<String>,
    /// Proposal UUID or short_id. Alias for `id`.
    pub proposal_id: Option<String>,
    /// Epic UUID or short_id to retire from this proposal's graduated epics.
    pub epic_id: String,
    /// Why obsolete work is being force-closed. Defaults to a reconcile teardown reason.
    pub reason: Option<String>,
    /// When true, compute blast radius without closing tasks, closing/unlinking the epic, or killing sessions.
    pub preview: Option<bool>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct ProposalStopBuildResponse {
    pub ok: bool,
    pub mode: String,
    pub proposal_id: Option<String>,
    /// Resulting proposal status (`approved` after a non-preview abort,
    /// `building` after freeze/unfreeze/preview).
    pub status: Option<String>,
    /// `true` when this was a dry-run that did not mutate anything.
    pub preview: bool,
    /// Epics torn down (abort) or that would be (preview).
    pub epics_closed: i64,
    /// Worker tasks force-closed (abort) or open right now (preview).
    pub tasks_closed: i64,
    /// Running worker sessions killed (abort) or live now (preview).
    pub sessions_killed: i64,
    pub error: Option<String>,
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
        if let Err(e) = validate_mdx_body(body, p.body_format.as_deref()) {
            return Json(err_single(e));
        }
        let body_format = p.body_format.as_deref().unwrap_or("markdown");
        if body_format == "mdx"
            && let Err(e) = validate_question_form_placement(body)
        {
            return Json(err_single(e));
        }
        let ac = p.acceptance_criteria.unwrap_or_default();
        if let Err(e) = validate_ac_count(ac.len()) {
            return Json(err_single(e));
        }
        let requested_status = match validate_proposal_create_status(p.status.as_deref()) {
            Ok(s) => s,
            Err(e) => return Json(err_single(e)),
        };
        // Proposer-role authors are forced into `triage` (an inbox a PM/engineer
        // promotes to `draft`); higher roles default to their requested status
        // (or `draft`).
        let status = match acting_caps(self.state.db()).await {
            Ok(Some(caps)) if !caps.is_admin && caps.role == "proposer" => Some("triage"),
            Ok(_) => requested_status,
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
                body_format: p.body_format.as_deref(),
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
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let epics = match graduated_epic_models(
            &repo,
            &epic_repo,
            &project_repo,
            &proposal.id,
            proposal.latest_revision_seq,
            proposal.pending_reconcile,
        )
        .await
        {
            Ok(e) => e,
            Err(e) => return Json(err_show(e)),
        };
        Json(ProposalShowResponse {
            proposal: Some(ProposalModel::from(&proposal)),
            targets: Some(targets),
            feedback: Some(feedback),
            revisions: Some(revisions),
            signoffs: Some(signoffs),
            epics: Some(epics),
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
                result
                    .proposals
                    .iter()
                    .map(|(p, count)| ProposalModel::from_with_count(p, *count))
                    .collect(),
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
        // Effective body_format: explicitly passed, else the proposal's current
        // format (matches the repository's own fallback on update).
        let body_format = p
            .body_format
            .as_deref()
            .unwrap_or(existing.body_format.as_str());
        if let Err(e) = validate_mdx_body(body, Some(body_format)) {
            return Json(err_single(e));
        }
        if p.body.is_some()
            && body_format == "mdx"
            && let Err(e) = validate_question_form_placement(body)
        {
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
                    body_format: p.body_format.as_deref(),
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
                target_section: p.target_section.as_deref(),
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

    /// Kick off an approved proposal — graduate it into the execution engine.
    #[tool(
        description = "Kick off an approved proposal: hand it to the Planner (a single `epic_breakdown` task on the first primary target) which reads the spec + target repos and breaks it down into epics across the targets, set status to `building`, and record the build owner (must be a participant — the author or a sign-off giver; defaults to the caller). Requires the proposal to be `approved` and the engineer role (or admin)."
    )]
    pub async fn proposal_graduate(
        &self,
        Parameters(p): Parameters<ProposalGraduateParams>,
    ) -> Json<ProposalSingleResponse> {
        // Capability: engineer/admin only.
        match acting_caps(self.state.db()).await {
            Ok(Some(caps)) if !caps.can_kickoff() => {
                return Json(err_single(
                    "kicking off a build requires the engineer role (or admin)".to_string(),
                ));
            }
            Err(e) => return Json(err_single(e)),
            _ => {}
        }
        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());

        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(err_single(proposal_not_found_error(&p.id)));
        };
        if proposal.status != "approved" {
            return Json(err_single(format!(
                "proposal must be approved to kick off (current: {})",
                proposal.status
            )));
        }

        // Build owner must be a participant (author or sign-off giver).
        let participants = match repo.participants(&proposal.id).await {
            Ok(v) => v,
            Err(e) => return Json(err_single(e.to_string())),
        };
        let Some(owner) = p
            .owner_user_id
            .clone()
            .or_else(djinn_core::auth_context::current_user_id)
        else {
            return Json(err_single("a build owner is required".to_string()));
        };
        if !participants.is_empty() && !participants.contains(&owner) {
            return Json(err_single(
                "the build owner must be a participant (the author or a sign-off giver)"
                    .to_string(),
            ));
        }

        // At least one primary target is required — that is where the build
        // writes. The proposal-decomposition Planner reads the spec + targets
        // and creates the epics itself (cut over from the old mechanical
        // one-epic-per-primary fan-out).
        let targets = match repo.targets(&proposal.id).await {
            Ok(t) => t,
            Err(e) => return Json(err_single(e.to_string())),
        };
        let primaries: Vec<_> = targets.iter().filter(|t| t.role == "primary").collect();
        let Some(home_target) = primaries.first() else {
            return Json(err_single(
                "no primary target project to build — add a target first".to_string(),
            ));
        };
        let home_project_id = home_target.project_id.clone();

        // A human-readable target list for the breakdown task's design.
        let mut target_lines = Vec::with_capacity(targets.len());
        for t in &targets {
            let slug = match project_repo.get(&t.project_id).await {
                Ok(Some(proj)) => format!("{}/{}", proj.github_owner, proj.github_repo),
                _ => t.project_id.clone(),
            };
            target_lines.push(format!("- {slug} ({})", t.role));
        }

        let design = format!(
            "Decompose proposal `{}` ({}) into epics.\n\n\
             Call `proposal_show(id=\"{}\")` for the full spec, acceptance \
             criteria, and targets, then follow Workflow D (Proposal \
             Decomposition). Create one or more epics per `primary` target with \
             `epic_create(..., proposal_id=\"{}\")`, attach `reference` targets \
             as read-sources, and sequence cross-repo work with `blocked_by`. Do \
             NOT create worker tasks — each epic runs its own wave Planner.\n\n\
             Targets:\n{}",
            proposal.short_id,
            proposal.id,
            proposal.id,
            proposal.id,
            target_lines.join("\n"),
        );

        let ac = serde_json::json!([
            {"criterion": "Proposal read via proposal_show and target repos surveyed", "met": false},
            {"criterion": "One or more epics created per primary target (epic_create with proposal_id), with read_sources and blocked_by set for cross-repo ordering", "met": false},
            {"criterion": "submit_grooming called to finalize the breakdown", "met": false},
        ])
        .to_string();

        let title = format!("Break down proposal: {}", proposal.title);
        let task = match task_repo
            .create_in_project(
                &home_project_id,
                None,
                &title,
                &design,
                &design,
                "epic_breakdown",
                djinn_core::models::task::PRIORITY_CRITICAL,
                "planner",
                Some("open"),
                Some(&ac),
            )
            .await
        {
            Ok(t) => t,
            Err(e) => {
                return Json(err_single(format!("failed to create breakdown task: {e}")));
            }
        };
        // Attribute the breakdown task (and the epics the Planner spawns from it,
        // which inherit the session user) to the build owner so commits resolve
        // to a real GitHub account.
        let _ = task_repo.set_created_by_user_id(&task.id, &owner).await;
        // Record the breakdown task so a later proposal_stop_build can find and
        // force-close it (it has no epic, so it is not in proposal_epics).
        let _ = repo.set_breakdown_task(&proposal.id, &task.id).await;

        match repo.set_building(&proposal.id, &owner).await {
            Ok(updated) => Json(ProposalSingleResponse {
                proposal: Some(ProposalModel::from(&updated)),
                error: None,
            }),
            Err(e) => Json(err_single(e.to_string())),
        }
    }

    /// Stop an in-flight proposal build — the inverse of `proposal_graduate`.
    #[tool(
        description = "Stop an in-flight proposal build (mode: abort | freeze | unfreeze). `freeze` holds the build's tasks out of dispatch while leaving epics/tasks/branches in place; `unfreeze` resumes. `abort` tears the build down — kills running workers, force-closes every task (deleting branches so GitHub auto-closes their PRs), closes the epics, unlinks them, and reverts the proposal to `approved` so it can be edited and re-graduated. Pass preview=true with mode=abort for a read-only blast-radius (epics, open tasks, running sessions) without mutating. Requires the proposal to be `building` and the engineer role (or admin)."
    )]
    pub async fn proposal_stop_build(
        &self,
        Parameters(p): Parameters<ProposalStopBuildParams>,
    ) -> Json<ProposalStopBuildResponse> {
        let err = |msg: String| {
            Json(ProposalStopBuildResponse {
                ok: false,
                mode: String::new(),
                proposal_id: None,
                status: None,
                preview: false,
                epics_closed: 0,
                tasks_closed: 0,
                sessions_killed: 0,
                error: Some(msg),
            })
        };

        // Capability: engineer/admin only (same gate as graduate — this is the
        // inverse build-control operation).
        match acting_caps(self.state.db()).await {
            Ok(Some(caps)) if !caps.can_kickoff() => {
                return err("stopping a build requires the engineer role (or admin)".to_string());
            }
            Err(e) => return err(e),
            _ => {}
        }

        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(&p.id).await.ok().flatten() else {
            return err(proposal_not_found_error(&p.id));
        };

        match p.mode.as_str() {
            "freeze" | "unfreeze" => {
                if proposal.status != "building" {
                    return err(format!(
                        "only a building proposal can be {}d (current: {})",
                        p.mode, proposal.status
                    ));
                }
                let frozen = p.mode == "freeze";
                match repo.set_frozen(&proposal.id, frozen).await {
                    Ok(updated) => Json(ProposalStopBuildResponse {
                        ok: true,
                        mode: p.mode,
                        proposal_id: Some(updated.id),
                        status: Some(updated.status),
                        preview: false,
                        epics_closed: 0,
                        tasks_closed: 0,
                        sessions_killed: 0,
                        error: None,
                    }),
                    Err(e) => err(e.to_string()),
                }
            }
            "abort" => {
                if proposal.status != "building" {
                    return err(format!(
                        "only a building proposal can be aborted (current: {})",
                        proposal.status
                    ));
                }
                let preview = p.preview.unwrap_or(false);
                let reason = p.reason.as_deref().unwrap_or("proposal build aborted");
                self.abort_proposal_build(&repo, &proposal, reason, preview)
                    .await
            }
            other => err(format!(
                "unknown mode '{other}' — expected abort | freeze | unfreeze"
            )),
        }
    }

    /// Tear down exactly one obsolete graduated epic subtree during proposal reconcile.
    #[tool(
        description = "Proposal reconcile helper: retire one obsolete graduated epic subtree without aborting the build. Pass `proposal_id` (or `id`) and `epic_id` (UUID or short_id). The tool verifies the epic is linked to the building proposal, blocks and records AI feedback if any target task has merged work, supports `preview=true` for read-only blast radius, and otherwise force-closes only target-epic tasks, kills their live sessions, closes/unlinks only that epic, and leaves the proposal building with unrelated graduated epics linked. Response includes ok, proposal_id, epic_id, preview, blocked/error, epics_closed, tasks_closed, sessions_killed, and merged-work blocked details."
    )]
    pub async fn proposal_reconcile_obsolete_epic(
        &self,
        Parameters(p): Parameters<ProposalReconcileObsoleteEpicParams>,
    ) -> Json<ProposalReconcileObsoleteEpicResponse> {
        let err = |proposal_id: Option<String>, epic_id: Option<String>, msg: String| {
            Json(ProposalReconcileObsoleteEpicResponse {
                ok: false,
                proposal_id,
                epic_id,
                preview: p.preview.unwrap_or(false),
                blocked: false,
                blocked_feedback_id: None,
                blocked_feedback_body: None,
                merged_tasks: Vec::new(),
                epics_closed: 0,
                tasks_closed: 0,
                sessions_killed: 0,
                error: Some(msg),
            })
        };

        match acting_caps(self.state.db()).await {
            Ok(Some(caps)) if !caps.can_kickoff() => {
                return err(
                    None,
                    None,
                    "reconciling obsolete epics requires the engineer role (or admin)".to_string(),
                );
            }
            Err(e) => return err(None, None, e),
            _ => {}
        }

        let Some(proposal_ref) = p.proposal_id.as_deref().or(p.id.as_deref()) else {
            return err(None, None, "missing proposal_id (or id)".to_string());
        };

        let repo = ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(proposal) = repo.resolve(proposal_ref).await.ok().flatten() else {
            return err(None, None, proposal_not_found_error(proposal_ref));
        };
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(epic) = epic_repo.resolve(&p.epic_id).await.ok().flatten() else {
            return err(
                Some(proposal.id.clone()),
                None,
                format!("epic not found: {}", p.epic_id),
            );
        };

        let reason = p
            .reason
            .as_deref()
            .unwrap_or("proposal reconcile obsolete-subtree teardown");
        self.teardown_obsolete_proposal_epic(
            &repo,
            &proposal,
            &epic.id,
            reason,
            p.preview.unwrap_or(false),
        )
        .await
    }
}

// ── Build teardown (abort cascade) ───────────────────────────────────────────

impl DjinnMcpServer {
    /// Tear down a graduated build and revert the proposal to `approved`.
    ///
    /// Composes the existing teardown primitives, all idempotent and
    /// best-effort on side-effects: kill the running worker (pool), force-close
    /// each open task (`ForceClose` is exempt from the blocker guard, so order
    /// is irrelevant) and clean its branch/PR, close each graduated epic, unlink
    /// them so a re-graduation starts clean, and revert the proposal to
    /// `approved`. A `preview` returns the blast radius without mutating.
    async fn abort_proposal_build(
        &self,
        repo: &ProposalRepository,
        proposal: &djinn_core::models::Proposal,
        reason: &str,
        preview: bool,
    ) -> Json<ProposalStopBuildResponse> {
        use djinn_core::models::TransitionAction;

        let abort_err = |msg: String| {
            Json(ProposalStopBuildResponse {
                ok: false,
                mode: "abort".to_string(),
                proposal_id: Some(proposal.id.clone()),
                status: None,
                preview: false,
                epics_closed: 0,
                tasks_closed: 0,
                sessions_killed: 0,
                error: Some(msg),
            })
        };

        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let pool = self.state.pool().await;

        let graduated = match repo.graduated_epics(&proposal.id).await {
            Ok(v) => v,
            Err(e) => return abort_err(e.to_string()),
        };

        // Every non-closed task across the graduated epics, plus the breakdown
        // task (which has no epic, so it is not in proposal_epics).
        let mut open_tasks: Vec<djinn_core::models::Task> = Vec::new();
        for (epic_id, _project_id) in &graduated {
            if let Ok(tasks) = task_repo.list_by_epic(epic_id).await {
                open_tasks.extend(tasks.into_iter().filter(|t| t.status != "closed"));
            }
        }
        if let Some(breakdown_id) = &proposal.build_breakdown_task_id
            && let Ok(Some(task)) = task_repo.resolve(breakdown_id).await
            && task.status != "closed"
        {
            open_tasks.push(task);
        }

        // Count live worker sessions (used by both preview and the kill count).
        let mut live_sessions = 0i64;
        if let Some(pool) = pool.as_ref() {
            for t in &open_tasks {
                if pool.has_session(&t.id).await.unwrap_or(false) {
                    live_sessions += 1;
                }
            }
        }

        if preview {
            return Json(ProposalStopBuildResponse {
                ok: true,
                mode: "abort".to_string(),
                proposal_id: Some(proposal.id.clone()),
                status: Some(proposal.status.clone()),
                preview: true,
                epics_closed: graduated.len() as i64,
                tasks_closed: open_tasks.len() as i64,
                sessions_killed: live_sessions,
                error: None,
            });
        }

        // Act. Kill the live worker (graceful), force-close the task, clean its
        // branch/PR. Best-effort throughout: a failed kill/cleanup is logged by
        // the underlying primitive and the next reaper backstops it.
        let mut sessions_killed = 0i64;
        let mut tasks_closed = 0i64;
        for t in &open_tasks {
            if let Some(pool) = pool.as_ref()
                && pool.has_session(&t.id).await.unwrap_or(false)
            {
                let _ = pool.kill_session(&t.id).await;
                sessions_killed += 1;
            }
            if task_repo
                .transition(
                    &t.id,
                    TransitionAction::ForceClose,
                    "system",
                    "system",
                    Some(reason),
                    None,
                )
                .await
                .is_ok()
            {
                tasks_closed += 1;
                self.state.cleanup_task_branches(&t.id).await;
            }
        }

        let mut epics_closed = 0i64;
        for (epic_id, _project_id) in &graduated {
            if epic_repo.close(epic_id).await.is_ok() {
                epics_closed += 1;
            }
        }

        let _ = repo.unlink_epics(&proposal.id).await;
        let status = match repo.revert_to_approved(&proposal.id).await {
            Ok(updated) => updated.status,
            Err(e) => return abort_err(e.to_string()),
        };

        Json(ProposalStopBuildResponse {
            ok: true,
            mode: "abort".to_string(),
            proposal_id: Some(proposal.id.clone()),
            status: Some(status),
            preview: false,
            epics_closed,
            tasks_closed,
            sessions_killed,
            error: None,
        })
    }

    /// Tear down exactly one obsolete graduated epic subtree for proposal reconcile.
    ///
    /// This is the scoped form of the abort cascade: it never touches the
    /// proposal build metadata, the breakdown task, or unrelated graduated epics.
    /// If any task in the target subtree has merged work, the operation records
    /// AI feedback and returns blocked before preview success or mutation.
    async fn teardown_obsolete_proposal_epic(
        &self,
        repo: &ProposalRepository,
        proposal: &djinn_core::models::Proposal,
        epic_id: &str,
        reason: &str,
        preview: bool,
    ) -> Json<ProposalReconcileObsoleteEpicResponse> {
        use djinn_core::models::TransitionAction;

        let scoped_err = |msg: String| {
            Json(ProposalReconcileObsoleteEpicResponse {
                ok: false,
                proposal_id: Some(proposal.id.clone()),
                epic_id: Some(epic_id.to_string()),
                preview: false,
                blocked: false,
                blocked_feedback_id: None,
                blocked_feedback_body: None,
                merged_tasks: Vec::new(),
                epics_closed: 0,
                tasks_closed: 0,
                sessions_killed: 0,
                error: Some(msg),
            })
        };

        if proposal.status != "building" {
            return scoped_err(format!(
                "only a building proposal can tear down an obsolete epic (current: {})",
                proposal.status
            ));
        }

        let graduated = match repo.graduated_epics(&proposal.id).await {
            Ok(v) => v,
            Err(e) => return scoped_err(e.to_string()),
        };
        if !graduated.iter().any(|(id, _)| id == epic_id) {
            return scoped_err(format!(
                "epic {epic_id} is not a graduated epic for proposal {}",
                proposal.id
            ));
        }

        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let pool = self.state.pool().await;

        let all_tasks = match task_repo.list_by_epic(epic_id).await {
            Ok(tasks) => tasks,
            Err(e) => return scoped_err(e.to_string()),
        };

        let merged_tasks: Vec<_> = all_tasks
            .iter()
            .filter(|t| t.merge_commit_sha.is_some())
            .collect();
        if !merged_tasks.is_empty() {
            let task_list = merged_tasks
                .iter()
                .map(|t| {
                    format!(
                        "- {} ({}) merged at {}",
                        t.short_id,
                        t.id,
                        t.merge_commit_sha.as_deref().unwrap_or("<unknown>")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let body = format!(
                "Obsolete-subtree teardown is blocked for epic `{epic_id}` because it contains merged work. Preserve the subtree and reconcile manually before unlinking or closing it.\n\nMerged tasks:\n{task_list}"
            );
            let feedback = repo
                .add_feedback(djinn_db::ProposalFeedbackCreateInput {
                    proposal_id: &proposal.id,
                    parent_id: None,
                    author_kind: "ai",
                    author_model: Some("proposal-reconcile"),
                    body: &body,
                    target_section: None,
                })
                .await;
            return Json(ProposalReconcileObsoleteEpicResponse {
                ok: false,
                proposal_id: Some(proposal.id.clone()),
                epic_id: Some(epic_id.to_string()),
                preview: false,
                blocked: true,
                blocked_feedback_id: feedback.as_ref().ok().map(|f| f.id.clone()),
                blocked_feedback_body: Some(body),
                merged_tasks: merged_tasks.iter().map(|t| t.id.clone()).collect(),
                epics_closed: 0,
                tasks_closed: 0,
                sessions_killed: 0,
                error: Some(format!(
                    "obsolete epic teardown blocked by merged work in {} task(s)",
                    merged_tasks.len()
                )),
            });
        }

        let open_tasks: Vec<_> = all_tasks
            .into_iter()
            .filter(|t| t.status != "closed")
            .collect();

        let mut live_sessions = 0i64;
        if let Some(pool) = pool.as_ref() {
            for t in &open_tasks {
                if pool.has_session(&t.id).await.unwrap_or(false) {
                    live_sessions += 1;
                }
            }
        }

        if preview {
            return Json(ProposalReconcileObsoleteEpicResponse {
                ok: true,
                proposal_id: Some(proposal.id.clone()),
                epic_id: Some(epic_id.to_string()),
                preview: true,
                blocked: false,
                blocked_feedback_id: None,
                blocked_feedback_body: None,
                merged_tasks: Vec::new(),
                epics_closed: 1,
                tasks_closed: open_tasks.len() as i64,
                sessions_killed: live_sessions,
                error: None,
            });
        }

        let mut sessions_killed = 0i64;
        let mut tasks_closed = 0i64;
        for t in &open_tasks {
            if let Some(pool) = pool.as_ref()
                && pool.has_session(&t.id).await.unwrap_or(false)
            {
                let _ = pool.kill_session(&t.id).await;
                sessions_killed += 1;
            }
            if task_repo
                .transition(
                    &t.id,
                    TransitionAction::ForceClose,
                    "system",
                    "system",
                    Some(reason),
                    None,
                )
                .await
                .is_ok()
            {
                tasks_closed += 1;
                self.state.cleanup_task_branches(&t.id).await;
            }
        }

        let epics_closed = if epic_repo.close(epic_id).await.is_ok() {
            1
        } else {
            0
        };
        if let Err(e) = repo.unlink_epic(&proposal.id, epic_id).await {
            return scoped_err(e.to_string());
        }

        Json(ProposalReconcileObsoleteEpicResponse {
            ok: true,
            proposal_id: Some(proposal.id.clone()),
            epic_id: Some(epic_id.to_string()),
            preview: false,
            blocked: false,
            blocked_feedback_id: None,
            blocked_feedback_body: None,
            merged_tasks: Vec::new(),
            epics_closed,
            tasks_closed,
            sessions_killed,
            error: None,
        })
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
        epics: None,
        error: Some(error.into()),
    }
}

/// Resolve a proposal's graduated epics to `{epic_short_id, project_path,
/// status}` display models.
async fn graduated_epic_models(
    repo: &ProposalRepository,
    epic_repo: &EpicRepository,
    project_repo: &ProjectRepository,
    proposal_id: &str,
    latest_revision_seq: i32,
    pending_reconcile: bool,
) -> Result<Vec<ProposalEpicModel>, String> {
    let links = repo
        .graduated_epics(proposal_id)
        .await
        .map_err(|e| e.to_string())?;
    let reconciliations = repo
        .latest_epic_reconciliations(proposal_id)
        .await
        .map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(links.len());
    for (epic_id, project_id) in links {
        let Some(epic) = epic_repo.get(&epic_id).await.ok().flatten() else {
            continue;
        };
        let reconciled_at_revision_seq = reconciliations.get(&epic_id).copied();
        let needs_reconcile = pending_reconcile
            && reconciled_at_revision_seq
                .map(|seq| seq < latest_revision_seq)
                .unwrap_or(false);
        let project_path = match project_repo.get(&project_id).await {
            Ok(Some(p)) => format!("{}/{}", p.github_owner, p.github_repo),
            _ => project_id.clone(),
        };
        out.push(ProposalEpicModel {
            epic_id,
            epic_short_id: epic.short_id,
            epic_title: epic.title,
            epic_emoji: epic.emoji,
            project_path,
            status: epic.status,
            reconciled_at_revision_seq,
            needs_reconcile,
        });
    }
    Ok(out)
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

#[cfg(test)]
mod stop_build_tests {
    use super::*;
    use crate::server::DjinnMcpServer;
    use crate::state::stubs::test_mcp_state;
    use djinn_core::events::EventBus;
    use djinn_db::{
        Database, EpicCreateInput, EpicRepository, ProjectRepository, ProposalCreateInput,
    };

    /// A `building` proposal: one graduated epic with two open worker tasks,
    /// plus a recorded breakdown task. The slot pool is the test stub (no live
    /// sessions), so the cascade exercises the DB-observable teardown.
    async fn building_proposal() -> (
        DjinnMcpServer,
        Database,
        String,
        String,
        Vec<String>,
        String,
    ) {
        let db = Database::open_in_memory().unwrap();
        db.ensure_initialized().await.unwrap();
        let bus = EventBus::noop();
        let project = ProjectRepository::new(db.clone(), bus.clone())
            .create("svc-stop", "test", "svc-stop")
            .await
            .unwrap();

        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        let proposal = prepo
            .create(ProposalCreateInput {
                title: "Stop me",
                body: "",
                acceptance_criteria: None,
                status: None,
                body_format: None,
            })
            .await
            .unwrap();

        let epic = EpicRepository::new(db.clone(), bus.clone())
            .create_for_project(
                &project.id,
                EpicCreateInput {
                    title: "E",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: None,
                    auto_breakdown: Some(false),
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();

        let trepo = TaskRepository::new(db.clone(), bus.clone());
        let mut task_ids = Vec::new();
        for i in 0..2 {
            let t = trepo
                .create_in_project(
                    &project.id,
                    Some(&epic.id),
                    &format!("t{i}"),
                    "",
                    "",
                    "task",
                    0,
                    "",
                    Some("open"),
                    Some("[\"do\"]"),
                )
                .await
                .unwrap();
            task_ids.push(t.id);
        }
        let breakdown = trepo
            .create_in_project(
                &project.id,
                None,
                "breakdown",
                "",
                "",
                "epic_breakdown",
                0,
                "planner",
                Some("open"),
                None,
            )
            .await
            .unwrap();

        prepo
            .link_epic(&proposal.id, &epic.id, &project.id)
            .await
            .unwrap();
        prepo
            .set_breakdown_task(&proposal.id, &breakdown.id)
            .await
            .unwrap();
        prepo.set_building(&proposal.id, "owner").await.unwrap();

        (
            DjinnMcpServer::new(test_mcp_state(db.clone())),
            db,
            proposal.id,
            epic.id,
            task_ids,
            breakdown.id,
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn freeze_and_unfreeze_toggle_the_flag() {
        let (server, db, pid, _e, _t, _b) = building_proposal().await;
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());

        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "freeze".into(),
                reason: None,
                preview: None,
            }))
            .await
            .0;
        assert!(r.ok);
        assert!(repo.get(&pid).await.unwrap().unwrap().build_frozen);

        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "unfreeze".into(),
                reason: None,
                preview: None,
            }))
            .await
            .0;
        assert!(r.ok);
        assert!(!repo.get(&pid).await.unwrap().unwrap().build_frozen);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn preview_reports_blast_radius_without_mutating() {
        let (server, db, pid, epic_id, _t, _b) = building_proposal().await;
        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "abort".into(),
                reason: None,
                preview: Some(true),
            }))
            .await
            .0;
        assert!(r.ok);
        assert!(r.preview);
        assert_eq!(r.epics_closed, 1);
        assert_eq!(r.tasks_closed, 3, "2 worker tasks + 1 breakdown task");

        // Nothing was mutated.
        let repo = ProposalRepository::new(db.clone(), EventBus::noop());
        assert_eq!(repo.get(&pid).await.unwrap().unwrap().status, "building");
        let epic = EpicRepository::new(db.clone(), EventBus::noop())
            .get(&epic_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(epic.status, "open");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_teardown_preview_reports_blast_radius_without_mutating() {
        let (server, db, pid, epic_id, task_ids, breakdown_id) = building_proposal().await;
        let r = server
            .dispatch_tool(
                "proposal_reconcile_obsolete_epic",
                serde_json::json!({
                    "proposal_id": pid.clone(),
                    "epic_id": epic_id.clone(),
                    "preview": true,
                }),
            )
            .await
            .unwrap();
        assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r.get("preview").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r.get("epics_closed").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(
            r.get("tasks_closed").and_then(|v| v.as_i64()),
            Some(task_ids.len() as i64)
        );

        let bus = EventBus::noop();
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        assert_eq!(prepo.get(&pid).await.unwrap().unwrap().status, "building");
        let links = prepo.graduated_epics(&pid).await.unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].0, epic_id);

        let erepo = EpicRepository::new(db.clone(), bus.clone());
        assert_eq!(erepo.get(&epic_id).await.unwrap().unwrap().status, "open");
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        for tid in task_ids.iter().chain(std::iter::once(&breakdown_id)) {
            assert_eq!(trepo.get(tid).await.unwrap().unwrap().status, "open");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_teardown_closes_only_target_epic_and_preserves_build() {
        let (server, db, pid, target_epic_id, target_task_ids, breakdown_id) =
            building_proposal().await;
        let bus = EventBus::noop();
        let project_repo = ProjectRepository::new(db.clone(), bus.clone());
        let project_id = project_repo
            .resolve("test/svc-stop")
            .await
            .unwrap()
            .unwrap();
        let project = project_repo.get(&project_id).await.unwrap().unwrap();
        let other_epic = EpicRepository::new(db.clone(), bus.clone())
            .create_for_project(
                &project.id,
                EpicCreateInput {
                    title: "Other",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                    status: None,
                    auto_breakdown: Some(false),
                    originating_adr_id: None,
                },
            )
            .await
            .unwrap();
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        let other_task = trepo
            .create_in_project(
                &project.id,
                Some(&other_epic.id),
                "other-task",
                "",
                "",
                "task",
                0,
                "",
                Some("open"),
                Some("[\"do\"]"),
            )
            .await
            .unwrap();
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        prepo
            .link_epic(&pid, &other_epic.id, &project.id)
            .await
            .unwrap();
        let r = server
            .dispatch_tool(
                "proposal_reconcile_obsolete_epic",
                serde_json::json!({
                    "proposal_id": pid.clone(),
                    "epic_id": target_epic_id.clone(),
                    "reason": "obsolete after reconcile",
                }),
            )
            .await
            .unwrap();
        assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(r.get("epics_closed").and_then(|v| v.as_i64()), Some(1));
        assert_eq!(
            r.get("tasks_closed").and_then(|v| v.as_i64()),
            Some(target_task_ids.len() as i64)
        );
        assert_eq!(r.get("blocked").and_then(|v| v.as_bool()), Some(false));

        let p = prepo.get(&pid).await.unwrap().unwrap();
        assert_eq!(p.status, "building");
        assert_eq!(
            p.build_breakdown_task_id.as_deref(),
            Some(breakdown_id.as_str())
        );
        assert_eq!(p.build_owner_user_id.as_deref(), Some("owner"));
        assert_eq!(
            prepo.graduated_epics(&pid).await.unwrap(),
            vec![(other_epic.id.clone(), project.id.clone())]
        );

        let erepo = EpicRepository::new(db.clone(), bus.clone());
        assert_eq!(
            erepo.get(&target_epic_id).await.unwrap().unwrap().status,
            "closed"
        );
        assert_eq!(
            erepo.get(&other_epic.id).await.unwrap().unwrap().status,
            "open"
        );

        for tid in &target_task_ids {
            assert_eq!(trepo.get(tid).await.unwrap().unwrap().status, "closed");
        }
        assert_eq!(
            trepo.get(&other_task.id).await.unwrap().unwrap().status,
            "open"
        );
        assert_eq!(
            trepo.get(&breakdown_id).await.unwrap().unwrap().status,
            "open"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scoped_teardown_blocks_merged_work_before_preview_or_mutation() {
        let (server, db, pid, target_epic_id, target_task_ids, breakdown_id) =
            building_proposal().await;
        let bus = EventBus::noop();
        let trepo = TaskRepository::new(db.clone(), bus.clone());
        trepo
            .set_merge_commit_sha(&target_task_ids[0], "abc123")
            .await
            .unwrap();
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        let r = server
            .dispatch_tool(
                "proposal_reconcile_obsolete_epic",
                serde_json::json!({
                    "id": pid.clone(),
                    "epic_id": target_epic_id.clone(),
                    "preview": true,
                }),
            )
            .await
            .unwrap();
        assert_eq!(r.get("ok").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(r.get("blocked").and_then(|v| v.as_bool()), Some(true));
        assert!(
            r.get("error")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .contains("blocked by merged work")
        );
        assert_eq!(r.get("epics_closed").and_then(|v| v.as_i64()), Some(0));
        assert_eq!(r.get("tasks_closed").and_then(|v| v.as_i64()), Some(0));
        assert!(
            r.get("blocked_feedback_body")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .contains("contains merged work")
        );

        let p = prepo.get(&pid).await.unwrap().unwrap();
        assert_eq!(p.status, "building");
        assert_eq!(
            p.build_breakdown_task_id.as_deref(),
            Some(breakdown_id.as_str())
        );
        assert_eq!(
            prepo.graduated_epics(&pid).await.unwrap(),
            vec![(
                target_epic_id.clone(),
                trepo
                    .get(&target_task_ids[0])
                    .await
                    .unwrap()
                    .unwrap()
                    .project_id
            )]
        );
        assert_eq!(
            EpicRepository::new(db.clone(), bus.clone())
                .get(&target_epic_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "open"
        );
        for tid in &target_task_ids {
            assert_eq!(trepo.get(tid).await.unwrap().unwrap().status, "open");
        }
        let feedback = prepo.feedback(&pid).await.unwrap();
        assert_eq!(feedback.len(), 1);
        assert_eq!(feedback[0].author_kind, "ai");
        assert!(feedback[0].body.contains("contains merged work"));
        assert!(feedback[0].body.contains(&target_task_ids[0]));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abort_tears_down_and_reverts_to_approved() {
        let (server, db, pid, epic_id, task_ids, breakdown_id) = building_proposal().await;
        let r = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid.clone(),
                mode: "abort".into(),
                reason: Some("changed my mind".into()),
                preview: Some(false),
            }))
            .await
            .0;
        assert!(r.ok, "abort failed: {:?}", r.error);
        assert_eq!(r.status.as_deref(), Some("approved"));
        assert_eq!(r.epics_closed, 1);
        assert_eq!(r.tasks_closed, 3);

        let bus = EventBus::noop();
        let prepo = ProposalRepository::new(db.clone(), bus.clone());
        let p = prepo.get(&pid).await.unwrap().unwrap();
        assert_eq!(p.status, "approved");
        assert!(p.build_owner_user_id.is_none());
        assert!(p.build_breakdown_task_id.is_none());
        assert!(prepo.graduated_epics(&pid).await.unwrap().is_empty());

        let epic = EpicRepository::new(db.clone(), bus.clone())
            .get(&epic_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(epic.status, "closed");

        let trepo = TaskRepository::new(db.clone(), bus.clone());
        for tid in task_ids.iter().chain(std::iter::once(&breakdown_id)) {
            let t = trepo.get(tid).await.unwrap().unwrap();
            assert_eq!(t.status, "closed", "task {tid} should be force-closed");
        }

        // A second abort is rejected — the proposal is no longer building.
        let r2 = server
            .proposal_stop_build(Parameters(ProposalStopBuildParams {
                id: pid,
                mode: "abort".into(),
                reason: Some("again".into()),
                preview: Some(false),
            }))
            .await
            .0;
        assert!(!r2.ok);
    }
}
