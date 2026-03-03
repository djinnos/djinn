// MCP tools for epic operations (CRUD, listing, queries).

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::Deserialize;

use crate::db::repositories::epic::{EpicCountQuery, EpicListQuery, EpicRepository, EpicTaskCounts};
use crate::db::repositories::task::{ListQuery, TaskRepository};
use crate::mcp::server::DjinnMcpServer;
use crate::mcp::tools::validation::{
    validate_color, validate_description, validate_emoji, validate_limit, validate_offset,
    validate_owner, validate_sort, validate_title,
};
use crate::mcp::tools::{ObjectJson, json_object};
use crate::models::epic::Epic;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn epic_to_value(e: &Epic) -> serde_json::Value {
    serde_json::json!({
        "id":          e.id,
        "short_id":    e.short_id,
        "title":       e.title,
        "description": e.description,
        "emoji":       e.emoji,
        "color":       e.color,
        "status":      e.status,
        "owner":       e.owner,
        "created_at":  e.created_at,
        "updated_at":  e.updated_at,
        "closed_at":   e.closed_at,
    })
}

fn epic_not_found(id: &str) -> serde_json::Value {
    serde_json::json!({ "error": format!("epic not found: {id}") })
}

fn task_to_value(t: &crate::models::task::Task) -> serde_json::Value {
    let labels: serde_json::Value =
        serde_json::from_str(&t.labels).unwrap_or(serde_json::json!([]));
    let ac: serde_json::Value =
        serde_json::from_str(&t.acceptance_criteria).unwrap_or(serde_json::json!([]));
    let memory_refs: serde_json::Value =
        serde_json::from_str(&t.memory_refs).unwrap_or(serde_json::json!([]));
    serde_json::json!({
        "id":                   t.id,
        "short_id":             t.short_id,
        "epic_id":              t.epic_id,
        "title":                t.title,
        "description":          t.description,
        "design":               t.design,
        "issue_type":           t.issue_type,
        "status":               t.status,
        "priority":             t.priority,
        "owner":                t.owner,
        "labels":               labels,
        "memory_refs":          memory_refs,
        "acceptance_criteria":  ac,
        "reopen_count":         t.reopen_count,
        "continuation_count":   t.continuation_count,
        "created_at":           t.created_at,
        "updated_at":           t.updated_at,
        "closed_at":            t.closed_at,
        "blocked_from_status":  t.blocked_from_status,
        "close_reason":         t.close_reason,
        "merge_commit_sha":     t.merge_commit_sha,
    })
}

fn enrich_with_counts(mut value: serde_json::Value, counts: &EpicTaskCounts) -> serde_json::Value {
    if let Some(map) = value.as_object_mut() {
        map.insert("task_count".to_string(), serde_json::json!(counts.task_count));
        map.insert("open_count".to_string(), serde_json::json!(counts.open_count));
        map.insert(
            "in_progress_count".to_string(),
            serde_json::json!(counts.in_progress_count),
        );
        map.insert(
            "closed_count".to_string(),
            serde_json::json!(counts.closed_count),
        );
    }
    value
}

// ── Param structs ────────────────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EpicCreateParams {
    pub title: String,
    pub description: Option<String>,
    pub emoji: Option<String>,
    pub color: Option<String>,
    pub owner: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EpicShowParams {
    /// Epic UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EpicListParams {
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
    /// Epic UUID or short_id.
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub emoji: Option<String>,
    pub color: Option<String>,
    pub owner: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EpicCloseParams {
    /// Epic UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EpicReopenParams {
    /// Epic UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EpicDeleteParams {
    /// Epic UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EpicTasksParams {
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
    pub status: Option<String>,
    /// Group results by: "status".
    pub group_by: Option<String>,
}

// ── Tool implementations ─────────────────────────────────────────────────────

#[tool_router(router = epic_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Create a new epic.
    #[tool(description = "Create a new epic (top-level grouping entity). Returns the created epic.")]
    pub async fn epic_create(
        &self,
        Parameters(p): Parameters<EpicCreateParams>,
    ) -> Json<ObjectJson> {
        let title = match validate_title(&p.title) {
            Ok(t) => t,
            Err(e) => return json_object(serde_json::json!({ "error": e })),
        };
        let description = p.description.as_deref().unwrap_or("");
        if let Err(e) = validate_description(description) {
            return json_object(serde_json::json!({ "error": e }));
        }
        let emoji = p.emoji.as_deref().unwrap_or("");
        if let Err(e) = validate_emoji(emoji) {
            return json_object(serde_json::json!({ "error": e }));
        }
        let color = p.color.as_deref().unwrap_or("");
        if let Err(e) = validate_color(color) {
            return json_object(serde_json::json!({ "error": e }));
        }
        let owner = match validate_owner(p.owner.as_deref().unwrap_or("")) {
            Ok(o) => o,
            Err(e) => return json_object(serde_json::json!({ "error": e })),
        };

        let repo = EpicRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.create(&title, description, emoji, color, &owner).await {
            Ok(epic) => json_object(epic_to_value(&epic)),
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Show epic details with task count statistics.
    #[tool(
        description = "Show details of an epic including child task counts. Accepts epic UUID or short_id."
    )]
    pub async fn epic_show(
        &self,
        Parameters(p): Parameters<EpicShowParams>,
    ) -> Json<ObjectJson> {
        let repo = EpicRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(epic) = repo.resolve(&p.id).await.ok().flatten() else {
            return json_object(epic_not_found(&p.id));
        };
        let counts = match repo.task_counts(&epic.id).await {
            Ok(c) => c,
            Err(e) => return json_object(serde_json::json!({ "error": e.to_string() })),
        };
        json_object(enrich_with_counts(epic_to_value(&epic), &counts))
    }

    /// List epics with optional filters and pagination.
    #[tool(
        description = "List epics with optional filters and offset-based pagination. Returns {epics[], total_count, limit, offset, has_more}."
    )]
    pub async fn epic_list(
        &self,
        Parameters(p): Parameters<EpicListParams>,
    ) -> Json<ObjectJson> {
        let sort = p.sort.as_deref().unwrap_or("created");
        if let Err(e) = validate_sort(
            sort,
            &["created", "created_desc", "updated", "updated_desc"],
        ) {
            return json_object(serde_json::json!({ "error": e }));
        }
        let limit = validate_limit(p.limit.unwrap_or(25));
        let offset = validate_offset(p.offset.unwrap_or(0));

        let query = EpicListQuery {
            status: p.status,
            text: p.text,
            sort: sort.to_owned(),
            limit,
            offset,
        };
        let repo = EpicRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.list_filtered(query).await {
            Ok(result) => {
                let epics: Vec<_> =
                    result.epics.iter().map(epic_to_value).collect();
                json_object(serde_json::json!({
                    "epics":       epics,
                    "total_count": result.total_count,
                    "limit":       limit,
                    "offset":      offset,
                    "has_more":    offset + limit < result.total_count,
                }))
            }
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Update allowed fields of an epic.
    #[tool(
        description = "Update allowed fields of an epic (title, description, emoji, color, owner). Accepts epic UUID or short_id."
    )]
    pub async fn epic_update(
        &self,
        Parameters(p): Parameters<EpicUpdateParams>,
    ) -> Json<ObjectJson> {
        let repo = EpicRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(epic) = repo.resolve(&p.id).await.ok().flatten() else {
            return json_object(epic_not_found(&p.id));
        };

        let title = if let Some(ref t) = p.title {
            match validate_title(t) {
                Ok(v) => v,
                Err(e) => return json_object(serde_json::json!({ "error": e })),
            }
        } else {
            epic.title.clone()
        };
        let description = p.description.as_deref().unwrap_or(&epic.description);
        if let Err(e) = validate_description(description) {
            return json_object(serde_json::json!({ "error": e }));
        }
        let emoji = p.emoji.as_deref().unwrap_or(&epic.emoji);
        if let Err(e) = validate_emoji(emoji) {
            return json_object(serde_json::json!({ "error": e }));
        }
        let color = p.color.as_deref().unwrap_or(&epic.color);
        if let Err(e) = validate_color(color) {
            return json_object(serde_json::json!({ "error": e }));
        }
        let owner = if let Some(ref o) = p.owner {
            match validate_owner(o) {
                Ok(v) => v,
                Err(e) => return json_object(serde_json::json!({ "error": e })),
            }
        } else {
            epic.owner.clone()
        };

        match repo.update(&epic.id, &title, description, emoji, color, &owner).await {
            Ok(updated) => json_object(epic_to_value(&updated)),
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Close an epic.
    #[tool(description = "Close an epic. Accepts epic UUID or short_id.")]
    pub async fn epic_close(
        &self,
        Parameters(p): Parameters<EpicCloseParams>,
    ) -> Json<ObjectJson> {
        let repo = EpicRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(epic) = repo.resolve(&p.id).await.ok().flatten() else {
            return json_object(epic_not_found(&p.id));
        };
        if epic.status != "open" {
            return json_object(serde_json::json!({
                "error": format!("epic must be open to close (current: {})", epic.status)
            }));
        }
        match repo.close(&epic.id).await {
            Ok(closed) => json_object(epic_to_value(&closed)),
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Reopen a closed epic.
    #[tool(description = "Reopen a closed epic. Accepts epic UUID or short_id.")]
    pub async fn epic_reopen(
        &self,
        Parameters(p): Parameters<EpicReopenParams>,
    ) -> Json<ObjectJson> {
        let repo = EpicRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(epic) = repo.resolve(&p.id).await.ok().flatten() else {
            return json_object(epic_not_found(&p.id));
        };
        match repo.reopen(&epic.id).await {
            Ok(reopened) => json_object(epic_to_value(&reopened)),
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Delete an epic and its child tasks.
    #[tool(
        description = "Delete an epic and all its child tasks (CASCADE). Returns {ok, deleted_task_count}. Accepts epic UUID or short_id."
    )]
    pub async fn epic_delete(
        &self,
        Parameters(p): Parameters<EpicDeleteParams>,
    ) -> Json<ObjectJson> {
        let repo = EpicRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(epic) = repo.resolve(&p.id).await.ok().flatten() else {
            return json_object(epic_not_found(&p.id));
        };
        match repo.delete_with_count(&epic.id).await {
            Ok(count) => json_object(serde_json::json!({
                "ok": true,
                "deleted_task_count": count,
            })),
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// List tasks under an epic with optional filters and pagination.
    #[tool(
        description = "List tasks under an epic with optional filters and pagination. Accepts epic UUID or short_id."
    )]
    pub async fn epic_tasks(
        &self,
        Parameters(p): Parameters<EpicTasksParams>,
    ) -> Json<ObjectJson> {
        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(epic) = epic_repo.resolve(&p.epic_id).await.ok().flatten() else {
            return json_object(epic_not_found(&p.epic_id));
        };

        let sort = p.sort.as_deref().unwrap_or("priority");
        if let Err(e) = validate_sort(
            sort,
            &["priority", "created", "created_desc", "updated", "updated_desc", "closed"],
        ) {
            return json_object(serde_json::json!({ "error": e }));
        }
        let limit = validate_limit(p.limit.unwrap_or(25));
        let offset = validate_offset(p.offset.unwrap_or(0));

        let query = ListQuery {
            parent: Some(epic.id),
            status: p.status,
            issue_type: p.issue_type,
            sort: sort.to_owned(),
            limit,
            offset,
            ..Default::default()
        };
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        match task_repo.list_filtered(query).await {
            Ok(result) => {
                let tasks: Vec<_> =
                    result.tasks.iter().map(task_to_value).collect();
                json_object(serde_json::json!({
                    "tasks":       tasks,
                    "total_count": result.total_count,
                    "limit":       limit,
                    "offset":      offset,
                    "has_more":    offset + limit < result.total_count,
                }))
            }
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Count epics with optional grouping.
    #[tool(description = "Count epics with optional grouping by status.")]
    pub async fn epic_count(
        &self,
        Parameters(p): Parameters<EpicCountParams>,
    ) -> Json<ObjectJson> {
        if let Some(ref gb) = p.group_by {
            if let Err(e) = validate_sort(gb, &["status"]) {
                return json_object(serde_json::json!({ "error": e }));
            }
        }
        let query = EpicCountQuery {
            status: p.status,
            group_by: p.group_by,
        };
        let repo = EpicRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.count_grouped(query).await {
            Ok(v) => json_object(v),
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }
}
