// MCP tools for epic operations (CRUD, listing, queries).

use std::borrow::Cow;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use crate::tools::epic_ops::{
    EpicModel, EpicShowRequest, EpicShowResponse, EpicSingleResponse, EpicTasksRequest,
    EpicTasksResponse, EpicUpdateRequest,
};
use crate::tools::list_response::{
    self, ListMeta, NamedListResponse, named_list_response_schema, serialize_named_list_response,
};
use crate::tools::validation::{
    validate_color, validate_description, validate_emoji, validate_epic_create_status,
    validate_limit, validate_offset, validate_owner, validate_sort, validate_title,
};
use djinn_core::models::Epic;
use djinn_db::{EpicCountQuery, EpicListQuery, EpicRepository, ProjectRepository};

#[derive(Clone)]
pub struct EpicListResponse {
    pub epics: Option<Vec<EpicModel>>,
    pub meta: ListMeta,
}

impl NamedListResponse for EpicListResponse {
    type Item = EpicModel;

    const FIELD_NAME: &'static str = "epics";
    const TITLE: &'static str = "EpicListResponse";

    fn from_parts(items: Option<Vec<Self::Item>>, meta: ListMeta) -> Self {
        Self { epics: items, meta }
    }

    fn items(&self) -> Option<&Vec<Self::Item>> {
        self.epics.as_ref()
    }

    fn meta(&self) -> &ListMeta {
        &self.meta
    }
}

impl Serialize for EpicListResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serialize_named_list_response(self, serializer)
    }
}

impl schemars::JsonSchema for EpicListResponse {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed(Self::TITLE)
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        named_list_response_schema::<EpicModel>(generator, Self::TITLE, Self::FIELD_NAME)
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct EpicDeleteResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_task_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct EpicCountGroup {
    pub key: String,
    pub count: i64,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct EpicCountResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<EpicCountGroup>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Response for the read-only multi-repo read-source tools.
#[derive(Serialize, schemars::JsonSchema)]
pub struct EpicReadSourcesResponse {
    /// The epic's read-source projects as `owner/repo` slugs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_sources: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A single epic-dependency reference returned by the blocker-listing tools.
#[derive(Serialize, schemars::JsonSchema)]
pub struct EpicBlockerItem {
    pub epic_id: String,
    pub short_id: String,
    pub title: String,
    pub status: String,
}

/// Response for the epic-dependency listing tools.
#[derive(Serialize, schemars::JsonSchema)]
pub struct EpicBlockersResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blockers: Option<Vec<EpicBlockerItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn epic_not_found_error(id: &str) -> String {
    format!("epic not found: {id}")
}

/// Resolve an epic's read-source project IDs to `owner/repo` slugs for
/// display. Falls back to the raw id if the project row is gone.
async fn resolve_read_source_slugs(
    epic_repo: &EpicRepository,
    project_repo: &ProjectRepository,
    epic_id: &str,
) -> Result<Vec<String>, String> {
    let ids = epic_repo
        .read_sources(epic_id)
        .await
        .map_err(|e| e.to_string())?;
    let mut slugs = Vec::with_capacity(ids.len());
    for id in ids {
        match project_repo.get(&id).await {
            Ok(Some(p)) => slugs.push(format!("{}/{}", p.github_owner, p.github_repo)),
            _ => slugs.push(id),
        }
    }
    Ok(slugs)
}

// ── Param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EpicCreateParams {
    /// Absolute project path.
    pub project: String,
    pub title: String,
    pub description: Option<String>,
    pub emoji: Option<String>,
    pub color: Option<String>,
    pub owner: Option<String>,
    /// Memory reference URLs for this epic (e.g. ADR paths).
    pub memory_refs: Option<Vec<String>>,
    /// Initial status: only "open" (the default). Epics are open → closed;
    /// pre-execution drafting lives in proposals now.
    pub status: Option<String>,
    /// When `false`, the coordinator will NOT auto-dispatch a breakdown
    /// Planner when this epic is created — the way to stage an epic without
    /// running it (replaces the old `drafting` status). Defaults to `true`.
    pub auto_breakdown: Option<bool>,
    /// ADR-051 Epic C — slug of the accepted ADR that spawned this
    /// epic.  Threaded into the breakdown Planner's session context
    /// so downstream task creation inherits the architectural rationale.
    pub originating_adr_id: Option<String>,
    /// Read-only multi-repo: other registered projects (UUIDs or
    /// owner/repo slugs) this epic's tasks may READ while still writing
    /// only to `project`. Set this when the work consults another repo —
    /// e.g. migrating code FROM project A INTO this epic's project, pass
    /// `read_sources: ["owner/A"]`.
    pub read_sources: Option<Vec<String>>,
    /// When this epic is created as part of decomposing a graduated proposal
    /// (Planner Mode D), pass the proposal UUID or short_id to record the
    /// `proposal → epic` link so the proposal can track what it became.
    pub proposal_id: Option<String>,
    /// Epic-level dependencies: UUIDs or short_ids of epics that must close
    /// before this epic's auto-breakdown can run.  Wired atomically at
    /// creation time so the `epic_created` event is only emitted after
    /// blocker edges exist in the DB.
    pub blocked_by: Option<Vec<String>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EpicShowParams {
    /// Absolute project path.
    pub project: String,
    /// Epic UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EpicListParams {
    /// Absolute project path.
    pub project: String,
    pub status: Option<String>,
    /// Full-text search on title and description.
    pub text: Option<String>,
    /// Sort order: "created" (default), "created_desc", "updated", "updated_desc".
    pub sort: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EpicUpdateParams {
    /// Absolute project path.
    pub project: String,
    /// Epic UUID or short_id.
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub emoji: Option<String>,
    pub color: Option<String>,
    pub owner: Option<String>,
    /// Memory reference URLs for this epic (e.g. ADR paths).
    pub memory_refs: Option<Vec<String>>,
    /// Target lifecycle status: "open" or "closed".
    pub status: Option<String>,
    /// Epic dependencies: epics (UUIDs or short_ids) that must CLOSE before
    /// this epic's wave-1 breakdown auto-dispatches. May reference epics in
    /// other projects (cross-repo ordering). Cycles are rejected.
    pub blocked_by_add: Option<Vec<String>>,
    /// Epic dependencies to remove (UUIDs or short_ids).
    pub blocked_by_remove: Option<Vec<String>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EpicBlockersParams {
    /// The epic's project (UUID or owner/repo slug).
    pub project: String,
    /// Epic UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EpicCloseParams {
    /// Absolute project path.
    pub project: String,
    /// Epic UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EpicReopenParams {
    /// Absolute project path.
    pub project: String,
    /// Epic UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EpicDeleteParams {
    /// Absolute project path.
    pub project: String,
    /// Epic UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EpicTasksParams {
    /// Absolute project path.
    pub project: String,
    /// Epic UUID or short_id.
    pub epic_id: String,
    pub status: Option<String>,
    /// Filter by issue type: "task", "feature", or "bug".
    pub issue_type: Option<String>,
    /// Sort order: "priority" (default), "created", "created_desc",
    /// "updated", "updated_desc", "closed".
    pub sort: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EpicCountParams {
    /// Absolute project path.
    pub project: String,
    pub status: Option<String>,
    /// Group results by: "status".
    pub group_by: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EpicReadSourceParams {
    /// The epic's OWN project (UUID or owner/repo slug) — the write target.
    pub project: String,
    /// Epic UUID or short_id.
    pub id: String,
    /// The read-source project to grant/revoke: UUID or owner/repo slug.
    /// Must reference an already-registered project.
    pub read_source: String,
}

/// Validated and normalised fields produced by [`validate_epic_create_params`].
struct ValidatedEpicCreateFields {
    title: String,
    description: String,
    emoji: String,
    color: String,
    owner: String,
    status: Option<String>,
    memory_refs_json: Option<String>,
}

/// Validate every scalar field of [`EpicCreateParams`] and return the
/// normalised values, or the exact same error string that the tool boundary
/// would produce so callers can forward it in `EpicSingleResponse.error`.
fn validate_epic_create_params(p: &EpicCreateParams) -> Result<ValidatedEpicCreateFields, String> {
    let title = validate_title(&p.title)?;
    let description = p.description.clone().unwrap_or_default();
    validate_description(&description)?;
    let emoji = p.emoji.clone().unwrap_or_default();
    validate_emoji(&emoji)?;
    let color = p.color.clone().unwrap_or_default();
    validate_color(&color)?;
    let owner = validate_owner(p.owner.as_deref().unwrap_or(""))?;
    let status = validate_epic_create_status(p.status.as_deref())?.map(|s| s.to_owned());
    let memory_refs_json = p
        .memory_refs
        .as_ref()
        .map(|refs| serde_json::to_string(refs).unwrap_or_else(|_| "[]".to_string()));
    Ok(ValidatedEpicCreateFields {
        title,
        description,
        emoji,
        color,
        owner,
        status,
        memory_refs_json,
    })
}

/// Resolve the target project and any `blocked_by` epic references for an
/// `epic_create` call.  Returns `(project_id, blocked_by_ids)` on success,
/// or an error string suitable for `EpicSingleResponse.error`.
///
/// The blocker refs are returned as resolved UUIDs so the caller can pass them
/// into `EpicRepository::create_for_project` — preserving atomic blocker-edge
/// insertion before the `epic_created` event.
async fn resolve_epic_create_project_and_blockers(
    server: &DjinnMcpServer,
    repo: &EpicRepository,
    p: &EpicCreateParams,
) -> Result<(String, Option<Vec<String>>), String> {
    let project_id = server.resolve_project_id(&p.project).await?;

    let blocked_by_ids: Option<Vec<String>> = match &p.blocked_by {
        Some(refs) if !refs.is_empty() => {
            let mut ids = Vec::new();
            for r in refs {
                match repo.resolve(r).await {
                    Ok(Some(e)) => ids.push(e.id),
                    _ => {
                        return Err(format!("blocker epic not found: {r}"));
                    }
                }
            }
            Some(ids)
        }
        _ => None,
    };

    Ok((project_id, blocked_by_ids))
}

/// Construct [`EpicCreateInput`] from validated fields and persist through
/// [`EpicRepository::create_for_project`].  Blocker refs are wired atomically
/// before the `epic_created` event is emitted.
async fn create_epic_for_project(
    repo: &EpicRepository,
    project_id: &str,
    validated: &ValidatedEpicCreateFields,
    p: &EpicCreateParams,
    blocked_by_ids: Option<&[String]>,
) -> Result<Epic, String> {
    let blocked_by_refs: Option<Vec<&str>> =
        blocked_by_ids.map(|ids| ids.iter().map(|s| s.as_str()).collect());

    repo.create_for_project(
        project_id,
        djinn_db::EpicCreateInput {
            title: &validated.title,
            description: &validated.description,
            emoji: &validated.emoji,
            color: &validated.color,
            owner: &validated.owner,
            memory_refs: validated.memory_refs_json.as_deref(),
            status: validated.status.as_deref(),
            auto_breakdown: p.auto_breakdown,
            originating_adr_id: p.originating_adr_id.as_deref(),
            blocked_by: blocked_by_refs.as_deref(),
        },
    )
    .await
    .map_err(|e| e.to_string())
}

/// Seed read-only multi-repo sources requested at epic creation time.
///
/// Best-effort: unresolvable sources, self-references, duplicates, and
/// add failures are silently skipped so epic creation still succeeds.
async fn seed_epic_read_sources(
    server: &DjinnMcpServer,
    repo: &EpicRepository,
    epic: &Epic,
    sources: Option<&[String]>,
) {
    if let Some(sources) = sources {
        let project_repo =
            ProjectRepository::new(server.state.db().clone(), server.state.event_bus());
        for src in sources {
            if let Ok(Some(src_id)) = project_repo.resolve(src).await
                && src_id != epic.project_id
            {
                let _ = repo.add_read_source(&epic.id, &src_id).await;
            }
        }
    }
}

/// Record a proposal → epic link when the epic was created from a graduated
/// proposal (Planner Mode D).
///
/// Best-effort: an unresolvable proposal ref or link failure does not fail
/// epic creation.
async fn link_epic_to_proposal(server: &DjinnMcpServer, epic: &Epic, proposal_ref: Option<&str>) {
    if let Some(proposal_ref) = proposal_ref {
        let proposal_repo =
            djinn_db::ProposalRepository::new(server.state.db().clone(), server.state.event_bus());
        if let Ok(Some(proposal)) = proposal_repo.resolve(proposal_ref).await {
            let _ = proposal_repo
                .link_epic(&proposal.id, &epic.id, &epic.project_id)
                .await;
        }
    }
}

// ── Tool implementations ─────────────────────────────────────────────────────

#[tool_router(router = epic_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Create a new epic.
    #[tool(
        description = "Create a new epic (top-level grouping entity). Returns the created epic. For read-only multi-repo work, pass `read_sources` with other registered projects (UUIDs or owner/repo slugs) the epic's tasks may READ — e.g. when the user wants to migrate code FROM project A INTO this epic's project, create the epic on the target project and set read_sources=[A]."
    )]
    pub async fn epic_create(
        &self,
        Parameters(p): Parameters<EpicCreateParams>,
    ) -> Json<EpicSingleResponse> {
        // ── Validate scalar fields ──────────────────────────────────────────
        let validated = match validate_epic_create_params(&p) {
            Ok(v) => v,
            Err(e) => {
                return Json(EpicSingleResponse {
                    epic: None,
                    error: Some(e),
                });
            }
        };

        // ── Resolve project & blocker refs ───────────────────────────────────
        let repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let (project_id, blocked_by_ids) =
            match resolve_epic_create_project_and_blockers(self, &repo, &p).await {
                Ok(v) => v,
                Err(e) => {
                    return Json(EpicSingleResponse {
                        epic: None,
                        error: Some(e),
                    });
                }
            };

        // ── Persist (blockers wired atomically) ──────────────────────────────
        match create_epic_for_project(
            &repo,
            &project_id,
            &validated,
            &p,
            blocked_by_ids.as_deref(),
        )
        .await
        {
            Ok(epic) => {
                // ── Best-effort post-create side effects ─────────────────────
                seed_epic_read_sources(self, &repo, &epic, p.read_sources.as_deref()).await;
                link_epic_to_proposal(self, &epic, p.proposal_id.as_deref()).await;

                Json(EpicSingleResponse {
                    epic: Some(EpicModel::from(&epic)),
                    error: None,
                })
            }
            Err(e) => Json(EpicSingleResponse {
                epic: None,
                error: Some(e),
            }),
        }
    }

    /// Show epic details with task count statistics.
    #[tool(
        description = "Show details of an epic including child task counts. Accepts epic UUID or short_id."
    )]
    pub async fn epic_show(
        &self,
        Parameters(p): Parameters<EpicShowParams>,
    ) -> Json<EpicShowResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(EpicShowResponse {
                    epic: None,
                    error: Some(e),
                });
            }
        };
        let repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        Json(
            crate::tools::epic_ops::epic_show(
                &repo,
                &project_id,
                EpicShowRequest {
                    project: p.project,
                    id: p.id,
                },
            )
            .await,
        )
    }

    /// List epics with optional filters and pagination.
    #[tool(
        description = "List epics with optional filters and offset-based pagination. Returns {epics[], total_count, limit, offset, has_more}."
    )]
    pub async fn epic_list(
        &self,
        Parameters(p): Parameters<EpicListParams>,
    ) -> Json<EpicListResponse> {
        let sort = p.sort.as_deref().unwrap_or("created");
        if let Err(e) = validate_sort(
            sort,
            &["created", "created_desc", "updated", "updated_desc"],
        ) {
            return Json(list_response::error::<EpicListResponse>(e));
        }
        let limit = validate_limit(p.limit.unwrap_or(25));
        let offset = validate_offset(p.offset.unwrap_or(0));

        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(list_response::error::<EpicListResponse>(e));
            }
        };
        let query = EpicListQuery {
            project_id: Some(project_id),
            status: p.status,
            text: p.text,
            sort: sort.to_owned(),
            limit,
            offset,
        };
        let repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        match repo.list_filtered(query).await {
            Ok(result) => {
                let mut epics: Vec<EpicModel> = result.epics.iter().map(EpicModel::from).collect();
                if let Err(e) = self.enrich_epic_proposal_refs(&mut epics).await {
                    return Json(list_response::error::<EpicListResponse>(e));
                }
                Json(list_response::success::<EpicListResponse>(
                    epics,
                    result.total_count,
                    limit,
                    offset,
                ))
            }
            Err(e) => Json(list_response::error::<EpicListResponse>(e.to_string())),
        }
    }

    /// Fill `proposal_short_id`/`proposal_title`/`proposal_status` on epics
    /// that carry a `proposal_id`, with one batched proposals lookup. Lets the
    /// board label proposal swimlanes without hydrating proposals.
    async fn enrich_epic_proposal_refs(
        &self,
        epics: &mut [EpicModel],
    ) -> std::result::Result<(), String> {
        let mut ids: Vec<String> = epics.iter().filter_map(|e| e.proposal_id.clone()).collect();
        ids.sort();
        ids.dedup();
        if ids.is_empty() {
            return Ok(());
        }
        let proposals =
            djinn_db::ProposalRepository::new(self.state.db().clone(), self.state.event_bus());
        let refs = proposals
            .refs_by_ids(&ids)
            .await
            .map_err(|e| e.to_string())?;
        let by_id: std::collections::HashMap<&str, &djinn_db::ProposalRef> =
            refs.iter().map(|r| (r.id.as_str(), r)).collect();
        for epic in epics.iter_mut() {
            if let Some(r) = epic.proposal_id.as_deref().and_then(|id| by_id.get(id)) {
                epic.proposal_short_id = Some(r.short_id.clone());
                epic.proposal_title = Some(r.title.clone());
                epic.proposal_status = Some(r.status.clone());
                epic.proposal_build_owner_user_id = r.build_owner_user_id.clone();
            }
        }
        Ok(())
    }

    /// Update allowed fields of an epic.
    #[tool(
        description = "Update allowed fields of an epic (title, description, emoji, color, owner, status). Accepts epic UUID or short_id. Status can be \"open\" or \"closed\". Use `blocked_by_add`/`blocked_by_remove` to set epic dependencies (epics that must close before this epic's breakdown runs) — these may reference epics in other projects for cross-repo ordering."
    )]
    pub async fn epic_update(
        &self,
        Parameters(p): Parameters<EpicUpdateParams>,
    ) -> Json<EpicSingleResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(EpicSingleResponse {
                    epic: None,
                    error: Some(e),
                });
            }
        };
        let repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());

        // Apply epic-dependency edits first (cross-project: resolve globally so
        // a blocker can live in another repo).
        if p.blocked_by_add.is_some() || p.blocked_by_remove.is_some() {
            let Some(target) = repo
                .resolve_in_project(&project_id, &p.id)
                .await
                .ok()
                .flatten()
            else {
                return Json(EpicSingleResponse {
                    epic: None,
                    error: Some(epic_not_found_error(&p.id)),
                });
            };
            let mut add_ids = Vec::new();
            for r in p.blocked_by_add.clone().unwrap_or_default() {
                match repo.resolve(&r).await {
                    Ok(Some(e)) => add_ids.push(e.id),
                    _ => {
                        return Json(EpicSingleResponse {
                            epic: None,
                            error: Some(format!("blocker epic not found: {r}")),
                        });
                    }
                }
            }
            // Tolerate unresolved refs on removal so stale edges can be cleared.
            let mut remove_ids = Vec::new();
            for r in p.blocked_by_remove.clone().unwrap_or_default() {
                match repo.resolve(&r).await {
                    Ok(Some(e)) => remove_ids.push(e.id),
                    _ => remove_ids.push(r),
                }
            }
            if let Err(e) = repo
                .update_blockers_atomic(&target.id, &add_ids, &remove_ids)
                .await
            {
                return Json(EpicSingleResponse {
                    epic: None,
                    error: Some(e.to_string()),
                });
            }
        }

        Json(
            crate::tools::epic_ops::epic_update(
                &repo,
                &project_id,
                EpicUpdateRequest {
                    project: p.project,
                    id: p.id,
                    title: p.title,
                    description: p.description,
                    emoji: p.emoji,
                    color: p.color,
                    owner: p.owner,
                    memory_refs: p.memory_refs,
                    status: p.status,
                },
            )
            .await,
        )
    }

    /// List the epics that block a given epic.
    #[tool(
        description = "List the epics that BLOCK a given epic (its dependencies — epics that must close before this epic's breakdown auto-dispatches). Accepts epic UUID or short_id. Returns {blockers:[{epic_id, short_id, title, status}]}."
    )]
    pub async fn epic_blockers_list(
        &self,
        Parameters(p): Parameters<EpicBlockersParams>,
    ) -> Json<EpicBlockersResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(EpicBlockersResponse {
                    blockers: None,
                    error: Some(e),
                });
            }
        };
        let repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(epic) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(EpicBlockersResponse {
                blockers: None,
                error: Some(epic_not_found_error(&p.id)),
            });
        };
        match repo.list_blockers(&epic.id).await {
            Ok(refs) => Json(EpicBlockersResponse {
                blockers: Some(
                    refs.into_iter()
                        .map(|b| EpicBlockerItem {
                            epic_id: b.epic_id,
                            short_id: b.short_id,
                            title: b.title,
                            status: b.status,
                        })
                        .collect(),
                ),
                error: None,
            }),
            Err(e) => Json(EpicBlockersResponse {
                blockers: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// List the epics blocked BY a given epic.
    #[tool(
        description = "List the epics blocked BY a given epic (its dependents — epics whose breakdown waits on this one). Accepts epic UUID or short_id. Returns {blockers:[{epic_id, short_id, title, status}]}."
    )]
    pub async fn epic_blocked_list(
        &self,
        Parameters(p): Parameters<EpicBlockersParams>,
    ) -> Json<EpicBlockersResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(EpicBlockersResponse {
                    blockers: None,
                    error: Some(e),
                });
            }
        };
        let repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(epic) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(EpicBlockersResponse {
                blockers: None,
                error: Some(epic_not_found_error(&p.id)),
            });
        };
        match repo.list_blocked_by(&epic.id).await {
            Ok(refs) => Json(EpicBlockersResponse {
                blockers: Some(
                    refs.into_iter()
                        .map(|b| EpicBlockerItem {
                            epic_id: b.epic_id,
                            short_id: b.short_id,
                            title: b.title,
                            status: b.status,
                        })
                        .collect(),
                ),
                error: None,
            }),
            Err(e) => Json(EpicBlockersResponse {
                blockers: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// Close an epic.
    #[tool(description = "Close an epic. Accepts epic UUID or short_id.")]
    pub async fn epic_close(
        &self,
        Parameters(p): Parameters<EpicCloseParams>,
    ) -> Json<EpicSingleResponse> {
        let repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(EpicSingleResponse {
                    epic: None,
                    error: Some(e),
                });
            }
        };
        let Some(epic) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(EpicSingleResponse {
                epic: None,
                error: Some(epic_not_found_error(&p.id)),
            });
        };
        if epic.status == "closed" {
            return Json(EpicSingleResponse {
                epic: None,
                error: Some("epic is already closed".to_string()),
            });
        }
        match repo.close(&epic.id).await {
            Ok(closed) => Json(EpicSingleResponse {
                epic: Some(EpicModel::from(&closed)),
                error: None,
            }),
            Err(e) => Json(EpicSingleResponse {
                epic: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// Reopen a closed epic.
    #[tool(description = "Reopen a closed epic. Accepts epic UUID or short_id.")]
    pub async fn epic_reopen(
        &self,
        Parameters(p): Parameters<EpicReopenParams>,
    ) -> Json<EpicSingleResponse> {
        let repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(EpicSingleResponse {
                    epic: None,
                    error: Some(e),
                });
            }
        };
        let Some(epic) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(EpicSingleResponse {
                epic: None,
                error: Some(epic_not_found_error(&p.id)),
            });
        };
        match repo.reopen(&epic.id).await {
            Ok(reopened) => Json(EpicSingleResponse {
                epic: Some(EpicModel::from(&reopened)),
                error: None,
            }),
            Err(e) => Json(EpicSingleResponse {
                epic: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// Delete an epic and its child tasks.
    #[tool(
        description = "Delete an epic and all its child tasks (CASCADE). Returns {ok, deleted_task_count}. Accepts epic UUID or short_id."
    )]
    pub async fn epic_delete(
        &self,
        Parameters(p): Parameters<EpicDeleteParams>,
    ) -> Json<EpicDeleteResponse> {
        let repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(EpicDeleteResponse {
                    ok: None,
                    deleted_task_count: None,
                    error: Some(e),
                });
            }
        };
        let Some(epic) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(EpicDeleteResponse {
                ok: None,
                deleted_task_count: None,
                error: Some(epic_not_found_error(&p.id)),
            });
        };
        match repo.delete_with_count(&epic.id).await {
            Ok(count) => Json(EpicDeleteResponse {
                ok: Some(true),
                deleted_task_count: Some(count),
                error: None,
            }),
            Err(e) => Json(EpicDeleteResponse {
                ok: None,
                deleted_task_count: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// List tasks under an epic with optional filters and pagination.
    #[tool(
        description = "List tasks under an epic with optional filters and pagination. Accepts epic UUID or short_id."
    )]
    pub async fn epic_tasks(
        &self,
        Parameters(p): Parameters<EpicTasksParams>,
    ) -> Json<EpicTasksResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(EpicTasksResponse {
                    tasks: None,
                    total_count: None,
                    limit: None,
                    offset: None,
                    has_more: None,
                    error: Some(e),
                });
            }
        };
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let task_repo =
            djinn_db::TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        Json(
            crate::tools::epic_ops::epic_tasks(
                &epic_repo,
                &task_repo,
                &project_id,
                EpicTasksRequest {
                    project: p.project,
                    epic_id: p.epic_id,
                    status: p.status,
                    issue_type: p.issue_type,
                    sort: p.sort,
                    limit: p.limit,
                    offset: p.offset,
                },
            )
            .await,
        )
    }

    /// Count epics with optional grouping.
    #[tool(description = "Count epics with optional grouping by status.")]
    pub async fn epic_count(
        &self,
        Parameters(p): Parameters<EpicCountParams>,
    ) -> Json<EpicCountResponse> {
        if let Some(ref gb) = p.group_by
            && let Err(e) = validate_sort(gb, &["status"])
        {
            return Json(EpicCountResponse {
                total_count: None,
                groups: None,
                error: Some(e),
            });
        }
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(EpicCountResponse {
                    total_count: None,
                    groups: None,
                    error: Some(e),
                });
            }
        };
        let query = EpicCountQuery {
            project_id: Some(project_id),
            status: p.status,
            group_by: p.group_by,
        };
        let repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        match repo.count_grouped(query).await {
            Ok(v) => {
                if let Some(total_count) = v.get("total_count").and_then(serde_json::Value::as_i64)
                {
                    return Json(EpicCountResponse {
                        total_count: Some(total_count),
                        groups: None,
                        error: None,
                    });
                }

                let groups = v
                    .get("groups")
                    .and_then(serde_json::Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|item| {
                                let key = item.get("key")?.as_str()?.to_string();
                                let count = item.get("count")?.as_i64()?;
                                Some(EpicCountGroup { key, count })
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();

                if !groups.is_empty() {
                    Json(EpicCountResponse {
                        total_count: None,
                        groups: Some(groups),
                        error: None,
                    })
                } else {
                    Json(EpicCountResponse {
                        total_count: None,
                        groups: None,
                        error: Some("invalid epic count response format".to_string()),
                    })
                }
            }
            Err(e) => Json(EpicCountResponse {
                total_count: None,
                groups: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// Grant an epic read-only access to another registered project.
    #[tool(
        description = "Grant an epic READ-ONLY access to another registered project (read-only multi-repo). The epic's tasks may then read that project's files, dependency graph, and notes, but still write ONLY to the epic's own project. Use when an epic needs to consult a source repo — e.g. migrating code FROM project A INTO the epic's project B, add A as a read source. `project` is the epic's own project; `id` is the epic UUID/short_id; `read_source` is the source project UUID or owner/repo slug (must already be registered). Returns the epic's updated read-source slug list."
    )]
    pub async fn epic_add_read_source(
        &self,
        Parameters(p): Parameters<EpicReadSourceParams>,
    ) -> Json<EpicReadSourcesResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(EpicReadSourcesResponse {
                    read_sources: None,
                    error: Some(e),
                });
            }
        };
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(epic) = epic_repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(EpicReadSourcesResponse {
                read_sources: None,
                error: Some(epic_not_found_error(&p.id)),
            });
        };
        let source_id = match project_repo.resolve(&p.read_source).await {
            Ok(Some(id)) => id,
            _ => {
                return Json(EpicReadSourcesResponse {
                    read_sources: None,
                    error: Some(format!("read-source project not found: {}", p.read_source)),
                });
            }
        };
        if source_id == epic.project_id {
            return Json(EpicReadSourcesResponse {
                read_sources: None,
                error: Some("an epic cannot add its own project as a read source".to_string()),
            });
        }
        if let Err(e) = epic_repo.add_read_source(&epic.id, &source_id).await {
            return Json(EpicReadSourcesResponse {
                read_sources: None,
                error: Some(e.to_string()),
            });
        }
        match resolve_read_source_slugs(&epic_repo, &project_repo, &epic.id).await {
            Ok(slugs) => Json(EpicReadSourcesResponse {
                read_sources: Some(slugs),
                error: None,
            }),
            Err(e) => Json(EpicReadSourcesResponse {
                read_sources: None,
                error: Some(e),
            }),
        }
    }

    /// Revoke an epic's read-only access to a project.
    #[tool(
        description = "Revoke an epic's READ-ONLY access to another project (read-only multi-repo). `project` is the epic's own project; `id` is the epic UUID/short_id; `read_source` is the source project UUID or owner/repo slug. No-op if it wasn't a read source. Returns the epic's updated read-source slug list."
    )]
    pub async fn epic_remove_read_source(
        &self,
        Parameters(p): Parameters<EpicReadSourceParams>,
    ) -> Json<EpicReadSourcesResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(EpicReadSourcesResponse {
                    read_sources: None,
                    error: Some(e),
                });
            }
        };
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(epic) = epic_repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(EpicReadSourcesResponse {
                read_sources: None,
                error: Some(epic_not_found_error(&p.id)),
            });
        };
        // Resolve the source ref; if it no longer resolves, fall back to the
        // raw value so a stale grant can still be removed by id.
        let source_id = project_repo
            .resolve(&p.read_source)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| p.read_source.clone());
        if let Err(e) = epic_repo.remove_read_source(&epic.id, &source_id).await {
            return Json(EpicReadSourcesResponse {
                read_sources: None,
                error: Some(e.to_string()),
            });
        }
        match resolve_read_source_slugs(&epic_repo, &project_repo, &epic.id).await {
            Ok(slugs) => Json(EpicReadSourcesResponse {
                read_sources: Some(slugs),
                error: None,
            }),
            Err(e) => Json(EpicReadSourcesResponse {
                read_sources: None,
                error: Some(e),
            }),
        }
    }

    /// List an epic's read-only source projects.
    #[tool(
        description = "List the read-only source projects granted to an epic (read-only multi-repo). Returns the epic's read-source slug list (owner/repo). `project` is the epic's own project; `id` is the epic UUID/short_id."
    )]
    pub async fn epic_list_read_sources(
        &self,
        Parameters(p): Parameters<EpicShowParams>,
    ) -> Json<EpicReadSourcesResponse> {
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(EpicReadSourcesResponse {
                    read_sources: None,
                    error: Some(e),
                });
            }
        };
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_repo = ProjectRepository::new(self.state.db().clone(), self.state.event_bus());
        let Some(epic) = epic_repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(EpicReadSourcesResponse {
                read_sources: None,
                error: Some(epic_not_found_error(&p.id)),
            });
        };
        match resolve_read_source_slugs(&epic_repo, &project_repo, &epic.id).await {
            Ok(slugs) => Json(EpicReadSourcesResponse {
                read_sources: Some(slugs),
                error: None,
            }),
            Err(e) => Json(EpicReadSourcesResponse {
                read_sources: None,
                error: Some(e),
            }),
        }
    }
}
