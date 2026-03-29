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
use djinn_db::{EpicCountQuery, EpicListQuery, EpicRepository};

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

fn epic_not_found_error(id: &str) -> String {
    format!("epic not found: {id}")
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
    /// Initial status: "drafting" (default) or "open".
    pub status: Option<String>,
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
    /// Target lifecycle status: "drafting" or "open".
    pub status: Option<String>,
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

        let status = match validate_epic_create_status(p.status.as_deref()) {
            Ok(s) => s,
            Err(e) => {
                return Json(EpicSingleResponse {
                    epic: None,
                    error: Some(e),
                });
            }
        };

        let memory_refs_json = p
            .memory_refs
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
            .create_for_project(
                &project_id,
                djinn_db::EpicCreateInput {
                    title: &title,
                    description,
                    emoji,
                    color,
                    owner: &owner,
                    memory_refs: memory_refs_json.as_deref(),
                    status,
                },
            )
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
            Ok(result) => Json(list_response::success::<EpicListResponse>(
                result.epics.iter().map(EpicModel::from).collect(),
                result.total_count,
                limit,
                offset,
            )),
            Err(e) => Json(list_response::error::<EpicListResponse>(e.to_string())),
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
}
