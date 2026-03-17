// MCP tools for epic operations (CRUD, listing, queries).

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::db::{EpicCountQuery, EpicListQuery, EpicRepository, EpicTaskCounts};
use crate::db::{ListQuery, TaskRepository};
use crate::mcp::server::DjinnMcpServer;
use crate::mcp::tools::validation::{
    validate_color, validate_description, validate_emoji, validate_limit, validate_offset,
    validate_owner, validate_sort, validate_title,
};
use crate::models::Epic;
use crate::models::Task;

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
#[serde(untagged)]
pub enum AcceptanceCriterionItem {
    Text(String),
    Structured(AcceptanceCriterionStatus),
}

#[derive(Serialize, Deserialize, Clone, schemars::JsonSchema)]
pub struct AcceptanceCriterionStatus {
    pub criterion: String,
    #[serde(default)]
    pub met: bool,
}

// ── Helpers ──────────────────────────────────────────────────────────────────

#[derive(Serialize, schemars::JsonSchema)]
pub struct EpicModel {
    pub id: String,
    pub short_id: String,
    pub title: String,
    pub description: String,
    pub emoji: String,
    pub color: String,
    pub status: String,
    pub owner: String,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub memory_refs: Vec<String>,
}

impl From<&Epic> for EpicModel {
    fn from(e: &Epic) -> Self {
        Self {
            id: e.id.clone(),
            short_id: e.short_id.clone(),
            title: e.title.clone(),
            description: e.description.clone(),
            emoji: e.emoji.clone(),
            color: e.color.clone(),
            status: e.status.clone(),
            owner: e.owner.clone(),
            created_at: e.created_at.clone(),
            updated_at: e.updated_at.clone(),
            closed_at: e.closed_at.clone(),
            memory_refs: parse_string_array(&e.memory_refs),
        }
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct EpicWithCountsModel {
    #[serde(flatten)]
    pub epic: EpicModel,
    pub task_count: i64,
    pub open_count: i64,
    pub in_progress_count: i64,
    pub closed_count: i64,
}

impl EpicWithCountsModel {
    fn from_parts(epic: &Epic, counts: &EpicTaskCounts) -> Self {
        Self {
            epic: EpicModel::from(epic),
            task_count: counts.task_count,
            open_count: counts.open_count,
            in_progress_count: counts.in_progress_count,
            closed_count: counts.closed_count,
        }
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct EpicTaskModel {
    pub id: String,
    pub short_id: String,
    pub epic_id: Option<String>,
    pub title: String,
    pub description: String,
    pub design: String,
    pub issue_type: String,
    pub status: String,
    pub priority: i64,
    pub owner: String,
    pub labels: Vec<String>,
    pub memory_refs: Vec<String>,
    pub acceptance_criteria: Vec<AcceptanceCriterionItem>,
    pub reopen_count: i64,
    pub continuation_count: i64,
    pub verification_failure_count: i64,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub close_reason: Option<String>,
    pub merge_commit_sha: Option<String>,
}

impl From<&Task> for EpicTaskModel {
    fn from(t: &Task) -> Self {
        Self {
            id: t.id.clone(),
            short_id: t.short_id.clone(),
            epic_id: t.epic_id.clone(),
            title: t.title.clone(),
            description: t.description.clone(),
            design: t.design.clone(),
            issue_type: t.issue_type.clone(),
            status: t.status.clone(),
            priority: t.priority,
            owner: t.owner.clone(),
            labels: parse_string_array(&t.labels),
            memory_refs: parse_string_array(&t.memory_refs),
            acceptance_criteria: parse_acceptance_criteria_array(&t.acceptance_criteria),
            reopen_count: t.reopen_count,
            continuation_count: t.continuation_count,
            verification_failure_count: t.verification_failure_count,
            created_at: t.created_at.clone(),
            updated_at: t.updated_at.clone(),
            closed_at: t.closed_at.clone(),
            close_reason: t.close_reason.clone(),
            merge_commit_sha: t.merge_commit_sha.clone(),
        }
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct EpicSingleResponse {
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub epic: Option<EpicModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct EpicShowResponse {
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub epic: Option<EpicWithCountsModel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct EpicListResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epics: Option<Vec<EpicModel>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
pub struct EpicTasksResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tasks: Option<Vec<EpicTaskModel>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
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

fn epic_not_found_error(id: &str) -> String {
    format!("epic not found: {id}")
}

fn parse_string_array(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn parse_acceptance_criteria_array(raw: &str) -> Vec<AcceptanceCriterionItem> {
    let parsed = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    parsed
        .into_iter()
        .map(|item| {
            serde_json::from_value::<AcceptanceCriterionItem>(item.clone())
                .unwrap_or_else(|_| AcceptanceCriterionItem::Text(item.to_string()))
        })
        .collect()
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

// ── Tool implementations ─────────────────────────────────────────────────────

#[tool_router(router = epic_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Create a new epic.
    #[tool(
        description = "Create a new epic (top-level grouping entity). Returns the created epic."
    )]
    pub async fn epic_create(
        &self,
        Parameters(p): Parameters<EpicCreateParams>,
    ) -> Json<EpicSingleResponse> {
        let title = match validate_title(&p.title) {
            Ok(t) => t,
            Err(e) => {
                return Json(EpicSingleResponse {
                    epic: None,
                    error: Some(e),
                });
            }
        };
        let description = p.description.as_deref().unwrap_or("");
        if let Err(e) = validate_description(description) {
            return Json(EpicSingleResponse {
                epic: None,
                error: Some(e),
            });
        }
        let emoji = p.emoji.as_deref().unwrap_or("");
        if let Err(e) = validate_emoji(emoji) {
            return Json(EpicSingleResponse {
                epic: None,
                error: Some(e),
            });
        }
        let color = p.color.as_deref().unwrap_or("");
        if let Err(e) = validate_color(color) {
            return Json(EpicSingleResponse {
                epic: None,
                error: Some(e),
            });
        }
        let owner = match validate_owner(p.owner.as_deref().unwrap_or("")) {
            Ok(o) => o,
            Err(e) => {
                return Json(EpicSingleResponse {
                    epic: None,
                    error: Some(e),
                });
            }
        };

        let memory_refs_json = p.memory_refs
            .as_ref()
            .map(|refs| serde_json::to_string(refs).unwrap_or_else(|_| "[]".to_string()));

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
        match repo
            .create_for_project(&project_id, djinn_db::EpicCreateInput { title: &title, description, emoji, color, owner: &owner, memory_refs: memory_refs_json.as_deref() })
            .await
        {
            Ok(epic) => Json(EpicSingleResponse {
                epic: Some(EpicModel::from(&epic)),
                error: None,
            }),
            Err(e) => Json(EpicSingleResponse {
                epic: None,
                error: Some(e.to_string()),
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
        let repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(EpicShowResponse {
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
            return Json(EpicShowResponse {
                epic: None,
                error: Some(epic_not_found_error(&p.id)),
            });
        };
        let counts = match repo.task_counts(&epic.id).await {
            Ok(c) => c,
            Err(e) => {
                return Json(EpicShowResponse {
                    epic: None,
                    error: Some(e.to_string()),
                });
            }
        };
        Json(EpicShowResponse {
            epic: Some(EpicWithCountsModel::from_parts(&epic, &counts)),
            error: None,
        })
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
            return Json(EpicListResponse {
                epics: None,
                total_count: None,
                limit: None,
                offset: None,
                has_more: None,
                error: Some(e),
            });
        }
        let limit = validate_limit(p.limit.unwrap_or(25));
        let offset = validate_offset(p.offset.unwrap_or(0));

        let project_id = match self.resolve_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => {
                return Json(EpicListResponse {
                    epics: None,
                    total_count: None,
                    limit: None,
                    offset: None,
                    has_more: None,
                    error: Some(e),
                });
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
            Ok(result) => Json(EpicListResponse {
                epics: Some(result.epics.iter().map(EpicModel::from).collect()),
                total_count: Some(result.total_count),
                limit: Some(limit),
                offset: Some(offset),
                has_more: Some(offset + limit < result.total_count),
                error: None,
            }),
            Err(e) => Json(EpicListResponse {
                epics: None,
                total_count: None,
                limit: None,
                offset: None,
                has_more: None,
                error: Some(e.to_string()),
            }),
        }
    }

    /// Update allowed fields of an epic.
    #[tool(
        description = "Update allowed fields of an epic (title, description, emoji, color, owner). Accepts epic UUID or short_id."
    )]
    pub async fn epic_update(
        &self,
        Parameters(p): Parameters<EpicUpdateParams>,
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

        let title = if let Some(ref t) = p.title {
            match validate_title(t) {
                Ok(v) => v,
                Err(e) => {
                    return Json(EpicSingleResponse {
                        epic: None,
                        error: Some(e),
                    });
                }
            }
        } else {
            epic.title.clone()
        };
        let description = p.description.as_deref().unwrap_or(&epic.description);
        if let Err(e) = validate_description(description) {
            return Json(EpicSingleResponse {
                epic: None,
                error: Some(e),
            });
        }
        let emoji = p.emoji.as_deref().unwrap_or(&epic.emoji);
        if let Err(e) = validate_emoji(emoji) {
            return Json(EpicSingleResponse {
                epic: None,
                error: Some(e),
            });
        }
        let color = p.color.as_deref().unwrap_or(&epic.color);
        if let Err(e) = validate_color(color) {
            return Json(EpicSingleResponse {
                epic: None,
                error: Some(e),
            });
        }
        let owner = if let Some(ref o) = p.owner {
            match validate_owner(o) {
                Ok(v) => v,
                Err(e) => {
                    return Json(EpicSingleResponse {
                        epic: None,
                        error: Some(e),
                    });
                }
            }
        } else {
            epic.owner.clone()
        };

        let memory_refs_str = if let Some(ref refs) = p.memory_refs {
            serde_json::to_string(refs).unwrap_or_else(|_| "[]".to_string())
        } else {
            epic.memory_refs.clone()
        };

        match repo
             .update(&epic.id, djinn_db::EpicUpdateInput { title: &title, description, emoji, color, owner: &owner, memory_refs: Some(&memory_refs_str) })
            .await
        {
            Ok(updated) => Json(EpicSingleResponse {
                epic: Some(EpicModel::from(&updated)),
                error: None,
            }),
            Err(e) => Json(EpicSingleResponse {
                epic: None,
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
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.event_bus());
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
        let Some(epic) = epic_repo
            .resolve_in_project(&project_id, &p.epic_id)
            .await
            .ok()
            .flatten()
        else {
            return Json(EpicTasksResponse {
                tasks: None,
                total_count: None,
                limit: None,
                offset: None,
                has_more: None,
                error: Some(epic_not_found_error(&p.epic_id)),
            });
        };

        let sort = p.sort.as_deref().unwrap_or("priority");
        if let Err(e) = validate_sort(
            sort,
            &[
                "priority",
                "created",
                "created_desc",
                "updated",
                "updated_desc",
                "closed",
            ],
        ) {
            return Json(EpicTasksResponse {
                tasks: None,
                total_count: None,
                limit: None,
                offset: None,
                has_more: None,
                error: Some(e),
            });
        }
        let limit = validate_limit(p.limit.unwrap_or(25));
        let offset = validate_offset(p.offset.unwrap_or(0));

        let query = ListQuery {
            project_id: Some(project_id),
            parent: Some(epic.id),
            status: p.status,
            issue_type: p.issue_type,
            sort: sort.to_owned(),
            limit,
            offset,
            ..Default::default()
        };
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        match task_repo.list_filtered(query).await {
            Ok(result) => Json(EpicTasksResponse {
                tasks: Some(result.tasks.iter().map(EpicTaskModel::from).collect()),
                total_count: Some(result.total_count),
                limit: Some(limit),
                offset: Some(offset),
                has_more: Some(offset + limit < result.total_count),
                error: None,
            }),
            Err(e) => Json(EpicTasksResponse {
                tasks: None,
                total_count: None,
                limit: None,
                offset: None,
                has_more: None,
                error: Some(e.to_string()),
            }),
        }
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
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use djinn_db::EpicRepository;
    use crate::test_helpers::{
        create_test_app, create_test_app_with_db, create_test_db, create_test_epic,
        create_test_project, create_test_task, initialize_mcp_session, mcp_call_tool,
    };
    use crate::events::EventBus;

    #[tokio::test]
    async fn epic_create_success_shape() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let project = create_test_project(&db).await;
        let session_id = initialize_mcp_session(&app).await;

        let result = mcp_call_tool(
            &app,
            &session_id,
            "epic_create",
            json!({"project": project.path, "title": "New Epic"}),
        )
        .await;

        assert!(result.get("error").is_none());
        assert!(result["id"].as_str().is_some());
        assert!(result["short_id"].as_str().is_some());
        assert_eq!(result["status"], "open");
        assert_eq!(result["title"], "New Epic");

        let repo = EpicRepository::new(db.clone(), EventBus::noop());
        let created = repo
            .get(result["id"].as_str().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(created.title, "New Epic");
        assert_eq!(created.status, "open");
    }

    #[tokio::test]
    async fn epic_create_error_on_empty_title() {
        let app = create_test_app();
        let session_id = initialize_mcp_session(&app).await;
        let result = mcp_call_tool(
            &app,
            &session_id,
            "epic_create",
            json!({"project": "/tmp/epic-test", "title": ""}),
        )
        .await;
        assert!(result["error"].as_str().is_some());
    }

    #[tokio::test]
    async fn epic_show_found_shape_with_task_counts() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let _task = create_test_task(&db, &project.id, &epic.id).await;
        let session_id = initialize_mcp_session(&app).await;

        let result = mcp_call_tool(
            &app,
            &session_id,
            "epic_show",
            json!({"project": project.path, "id": epic.short_id}),
        )
        .await;

        assert!(result.get("error").is_none());
        assert_eq!(result["id"], epic.id);
        assert_eq!(result["task_count"], 1);
        assert_eq!(result["open_count"], 1);
    }

    #[tokio::test]
    async fn epic_show_not_found_error() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let project = create_test_project(&db).await;
        let session_id = initialize_mcp_session(&app).await;

        let result = mcp_call_tool(
            &app,
            &session_id,
            "epic_show",
            json!({"project": project.path, "id": "does-not-exist"}),
        )
        .await;

        assert!(
            result["error"]
                .as_str()
                .unwrap_or_default()
                .contains("epic not found")
        );
    }

    #[tokio::test]
    async fn epic_list_default_returns_epics() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let project = create_test_project(&db).await;
        let _e1 = create_test_epic(&db, &project.id).await;
        let _e2 = create_test_epic(&db, &project.id).await;
        let session_id = initialize_mcp_session(&app).await;

        let result = mcp_call_tool(
            &app,
            &session_id,
            "epic_list",
            json!({"project": project.path}),
        )
        .await;

        assert!(result.get("error").is_none());
        assert!(result["epics"].as_array().is_some());
        assert!(result["total_count"].as_i64().unwrap_or_default() >= 2);
    }

    #[tokio::test]
    async fn epic_list_filter_by_status() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let project = create_test_project(&db).await;
        let open_epic = create_test_epic(&db, &project.id).await;
        let closed_epic = create_test_epic(&db, &project.id).await;
        let session_id = initialize_mcp_session(&app).await;

        let _ = mcp_call_tool(
            &app,
            &session_id,
            "epic_close",
            json!({"project": project.path, "id": closed_epic.id}),
        )
        .await;

        let result = mcp_call_tool(
            &app,
            &session_id,
            "epic_list",
            json!({"project": project.path, "status": "open"}),
        )
        .await;

        let epics = result["epics"].as_array().cloned().unwrap_or_default();
        assert!(epics.iter().all(|e| e["status"] == "open"));
        assert!(epics.iter().any(|e| e["id"] == open_epic.id));
        assert!(!epics.iter().any(|e| e["id"] == closed_epic.id));
    }

    #[tokio::test]
    async fn epic_update_partial_fields() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let session_id = initialize_mcp_session(&app).await;

        let result = mcp_call_tool(
            &app,
            &session_id,
            "epic_update",
            json!({
                "project": project.path,
                "id": epic.id,
                "title": "Updated Epic",
                "description": "Updated description",
                "color": "#800080",
                "emoji": "🚀"
            }),
        )
        .await;

        assert!(
            result.get("error").is_none(),
            "unexpected error: {:?}",
            result
        );
        assert_eq!(result["title"], "Updated Epic");
        assert_eq!(result["description"], "Updated description");
        assert_eq!(result["color"], "#800080");
        assert_eq!(result["emoji"], "🚀");

        let repo = EpicRepository::new(db.clone(), EventBus::noop());
        let updated = repo.get(&epic.id).await.unwrap().unwrap();
        assert_eq!(updated.title, "Updated Epic");
        assert_eq!(updated.description, "Updated description");
        assert_eq!(updated.color, "#800080");
        assert_eq!(updated.emoji, "🚀");
    }

    #[tokio::test]
    async fn epic_close_success() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let session_id = initialize_mcp_session(&app).await;

        let result = mcp_call_tool(
            &app,
            &session_id,
            "epic_close",
            json!({"project": project.path, "id": epic.short_id}),
        )
        .await;

        assert!(result.get("error").is_none());
        assert_eq!(result["status"], "closed");
    }

    #[tokio::test]
    async fn epic_close_error_when_already_closed() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let session_id = initialize_mcp_session(&app).await;

        let _ = mcp_call_tool(
            &app,
            &session_id,
            "epic_close",
            json!({"project": project.path, "id": epic.id}),
        )
        .await;

        let result = mcp_call_tool(
            &app,
            &session_id,
            "epic_close",
            json!({"project": project.path, "id": epic.id}),
        )
        .await;

        assert!(
            result["error"]
                .as_str()
                .unwrap_or_default()
                .contains("already closed")
        );
    }

    #[tokio::test]
    async fn epic_reopen_success_from_closed() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let session_id = initialize_mcp_session(&app).await;

        let _ = mcp_call_tool(
            &app,
            &session_id,
            "epic_close",
            json!({"project": project.path, "id": epic.id}),
        )
        .await;

        let result = mcp_call_tool(
            &app,
            &session_id,
            "epic_reopen",
            json!({"project": project.path, "id": epic.short_id}),
        )
        .await;

        assert!(result.get("error").is_none());
        assert_eq!(result["status"], "open");
    }

    #[tokio::test]
    async fn epic_reopen_error_when_already_open() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let session_id = initialize_mcp_session(&app).await;

        let result = mcp_call_tool(
            &app,
            &session_id,
            "epic_reopen",
            json!({"project": project.path, "id": epic.id}),
        )
        .await;

        assert!(
            result["error"]
                .as_str()
                .unwrap_or_default()
                .contains("must be closed")
        );
    }

    #[tokio::test]
    async fn epic_delete_success_and_cascade_child_tasks() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let _task1 = create_test_task(&db, &project.id, &epic.id).await;
        let _task2 = create_test_task(&db, &project.id, &epic.id).await;
        let session_id = initialize_mcp_session(&app).await;

        let result = mcp_call_tool(
            &app,
            &session_id,
            "epic_delete",
            json!({"project": project.path, "id": epic.short_id}),
        )
        .await;

        assert!(result.get("error").is_none());
        assert_eq!(result["ok"], true);
        assert_eq!(result["deleted_task_count"], 2);

        let epic_repo = EpicRepository::new(db.clone(), EventBus::noop());
        let deleted_epic = epic_repo.get(&epic.id).await.expect("query epic");
        assert!(deleted_epic.is_none(), "epic should be deleted from DB");

        let tasks_result = mcp_call_tool(
            &app,
            &session_id,
            "epic_tasks",
            json!({"project": project.path, "epic_id": epic.id}),
        )
        .await;
        assert!(
            tasks_result["error"]
                .as_str()
                .unwrap_or_default()
                .contains("epic not found")
        );
    }

    #[tokio::test]
    async fn epic_tasks_returns_child_tasks() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let _task = create_test_task(&db, &project.id, &epic.id).await;
        let session_id = initialize_mcp_session(&app).await;

        let result = mcp_call_tool(
            &app,
            &session_id,
            "epic_tasks",
            json!({"project": project.path, "epic_id": epic.short_id}),
        )
        .await;

        assert!(result.get("error").is_none());
        assert_eq!(result["total_count"], 1);
        assert_eq!(
            result["tasks"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or_default(),
            1
        );
    }

    #[tokio::test]
    async fn epic_tasks_empty_when_no_tasks() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let session_id = initialize_mcp_session(&app).await;

        let result = mcp_call_tool(
            &app,
            &session_id,
            "epic_tasks",
            json!({"project": project.path, "epic_id": epic.id}),
        )
        .await;

        assert!(result.get("error").is_none());
        assert_eq!(result["total_count"], 0);
        assert_eq!(
            result["tasks"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or_default(),
            0
        );
    }

    #[tokio::test]
    async fn epic_count_plain_total() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let project = create_test_project(&db).await;
        let _e1 = create_test_epic(&db, &project.id).await;
        let _e2 = create_test_epic(&db, &project.id).await;
        let session_id = initialize_mcp_session(&app).await;

        let result = mcp_call_tool(
            &app,
            &session_id,
            "epic_count",
            json!({"project": project.path}),
        )
        .await;

        assert!(result.get("error").is_none());
        assert!(result["total_count"].as_i64().unwrap_or_default() >= 2);
    }

    #[tokio::test]
    async fn epic_count_grouped_by_status() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let project = create_test_project(&db).await;
        let _open = create_test_epic(&db, &project.id).await;
        let closed = create_test_epic(&db, &project.id).await;
        let session_id = initialize_mcp_session(&app).await;

        let _ = mcp_call_tool(
            &app,
            &session_id,
            "epic_close",
            json!({"project": project.path, "id": closed.id}),
        )
        .await;

        let result = mcp_call_tool(
            &app,
            &session_id,
            "epic_count",
            json!({"project": project.path, "group_by": "status"}),
        )
        .await;

        assert!(result.get("error").is_none());
        let groups = result["groups"].as_array().cloned().unwrap_or_default();
        assert!(groups.iter().any(|g| g["key"] == "open"));
        assert!(groups.iter().any(|g| g["key"] == "closed"));
    }

    #[tokio::test]
    async fn epic_create_with_memory_refs_roundtrip() {
        let db = create_test_db();
        let app = create_test_app_with_db(db.clone());
        let project = create_test_project(&db).await;
        let session_id = initialize_mcp_session(&app).await;

        let result = mcp_call_tool(
            &app,
            &session_id,
            "epic_create",
            json!({
                "project": project.path,
                "title": "Refs Epic",
                "memory_refs": ["decisions/adr-029"]
            }),
        )
        .await;

        assert!(result.get("error").is_none(), "create error: {:?}", result);
        let refs = result["memory_refs"].as_array().expect("memory_refs should be array");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0], "decisions/adr-029");

        // Verify via epic_show
        let show_result = mcp_call_tool(
            &app,
            &session_id,
            "epic_show",
            json!({"project": project.path, "id": result["short_id"].as_str().unwrap()}),
        )
        .await;

        assert!(show_result.get("error").is_none());
        let show_refs = show_result["memory_refs"].as_array().expect("memory_refs in show");
        assert_eq!(show_refs, &[json!("decisions/adr-029")]);
    }
}
