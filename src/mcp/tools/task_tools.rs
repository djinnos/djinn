// MCP tools for task board operations (CRUD, listing, queries).

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::db::connection::OptionalExt;
use crate::db::repositories::epic::EpicRepository;
use crate::db::repositories::task::{CountQuery, ListQuery, ReadyQuery, TaskRepository};
use crate::mcp::server::DjinnMcpServer;
use crate::models::task::Task;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn task_to_value(t: &Task) -> serde_json::Value {
    let labels: serde_json::Value =
        serde_json::from_str(&t.labels).unwrap_or(serde_json::json!([]));
    let ac: serde_json::Value =
        serde_json::from_str(&t.acceptance_criteria).unwrap_or(serde_json::json!([]));
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
        "acceptance_criteria":  ac,
        "reopen_count":         t.reopen_count,
        "continuation_count":   t.continuation_count,
        "created_at":           t.created_at,
        "updated_at":           t.updated_at,
        "closed_at":            t.closed_at,
        "blocked_from_status":  t.blocked_from_status,
        "close_reason":         t.close_reason,
    })
}

fn not_found(id: &str) -> serde_json::Value {
    serde_json::json!({ "error": format!("task not found: {id}") })
}

// ── Param / response structs ──────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskCreateParams {
    /// Parent epic ID — UUID or short_id (required).
    pub parent: String,
    pub title: String,
    /// Task type: "task" (default), "feature", or "bug".
    pub issue_type: Option<String>,
    pub description: Option<String>,
    pub design: Option<String>,
    pub priority: Option<i64>,
    pub owner: Option<String>,
    pub labels: Option<Vec<String>>,
    pub acceptance_criteria: Option<Vec<serde_json::Value>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskUpdateParams {
    /// Task UUID or short_id.
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub design: Option<String>,
    pub priority: Option<i64>,
    pub owner: Option<String>,
    /// Labels to add.
    pub labels_add: Option<Vec<String>>,
    /// Labels to remove.
    pub labels_remove: Option<Vec<String>>,
    /// Full replacement for acceptance_criteria.
    pub acceptance_criteria: Option<Vec<serde_json::Value>>,
    /// New parent epic UUID or short_id.
    pub parent: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskShowParams {
    /// Task UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskListParams {
    pub status: Option<String>,
    /// Positive ("task") or negative ("!epic") issue_type filter.
    pub issue_type: Option<String>,
    pub priority: Option<i64>,
    /// Filter by label value.
    pub label: Option<String>,
    /// Full-text search on title and description.
    pub text: Option<String>,
    /// Filter by parent epic UUID or short_id.
    pub parent: Option<String>,
    /// Sort order: "priority" (default), "created", "created_desc",
    /// "updated", "updated_desc", "closed".
    pub sort: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskCountParams {
    pub status: Option<String>,
    pub issue_type: Option<String>,
    pub priority: Option<i64>,
    pub label: Option<String>,
    pub text: Option<String>,
    pub parent: Option<String>,
    /// Group results by: "status", "priority", "issue_type", or "parent".
    pub group_by: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskParentGetParams {
    /// Task UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskChildrenListParams {
    /// Epic UUID or short_id.
    pub epic_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskCommentAddParams {
    /// Task UUID or short_id.
    pub id: String,
    /// Comment body text.
    pub body: String,
    pub actor_id: Option<String>,
    pub actor_role: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskBlockersAddParams {
    /// Task UUID or short_id (the task being blocked).
    pub id: String,
    /// Task UUID or short_id of the task that blocks it.
    pub blocking_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskBlockersRemoveParams {
    /// Task UUID or short_id (the blocked task).
    pub id: String,
    /// Task UUID or short_id of the blocker to remove.
    pub blocking_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskBlockersListParams {
    /// Task UUID or short_id.
    pub id: String,
    /// Absolute project path (optional).
    pub project: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskBlockedListParams {
    /// Task UUID or short_id.
    pub id: String,
    /// Absolute project path (optional).
    pub project: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskReadyParams {
    /// Filter by issue type. Positive ("task") or negative ("!epic").
    pub issue_type: Option<String>,
    /// Filter by label value.
    pub label: Option<String>,
    /// Filter by owner email.
    pub owner: Option<String>,
    /// Maximum priority to include (0=highest, higher numbers=lower priority).
    pub priority_max: Option<i64>,
    pub limit: Option<i64>,
    /// Absolute project path (optional).
    pub project: Option<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskListResponse {
    pub tasks: Vec<serde_json::Value>,
    pub total_count: i64,
    pub limit: i64,
    pub offset: i64,
    pub has_more: bool,
}

// ── Tool implementations ──────────────────────────────────────────────────────

#[tool_router(router = task_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Create a new work item (task, feature, or bug) under an epic.
    #[tool(description = "Create a new work item (epic, feature, task, or bug). Parent and blocked_by accept task ID (full UUID or short_id, e.g., 'k7m2').")]
    pub async fn task_create(
        &self,
        Parameters(p): Parameters<TaskCreateParams>,
    ) -> Json<serde_json::Value> {
        // Resolve parent epic.
        let Some(epic_id) = self.resolve_epic_id(&p.parent).await else {
            return Json(serde_json::json!({ "error": format!("epic not found: {}", p.parent) }));
        };

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());

        let issue_type = p.issue_type.as_deref().unwrap_or("task");
        let description = p.description.as_deref().unwrap_or("");
        let design = p.design.as_deref().unwrap_or("");
        let priority = p.priority.unwrap_or(0);
        let owner = p.owner.as_deref().unwrap_or("");

        let task = match repo
            .create(&epic_id, &p.title, description, design, issue_type, priority, owner)
            .await
        {
            Ok(t) => t,
            Err(e) => return Json(serde_json::json!({ "error": e.to_string() })),
        };

        // Apply labels / ac if provided.
        let has_labels = p.labels.as_ref().map(|v| !v.is_empty()).unwrap_or(false);
        let has_ac = p.acceptance_criteria.as_ref().map(|v| !v.is_empty()).unwrap_or(false);

        if has_labels || has_ac {
            let labels_json = p
                .labels
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()))
                .unwrap_or_else(|| task.labels.clone());
            let ac_json = p
                .acceptance_criteria
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()))
                .unwrap_or_else(|| task.acceptance_criteria.clone());

            let updated = repo
                .update(&task.id, &task.title, &task.description, &task.design,
                        task.priority, &task.owner, &labels_json, &ac_json)
                .await;
            if let Ok(t) = updated {
                return Json(task_to_value(&t));
            }
        }

        Json(task_to_value(&task))
    }

    /// Update allowed fields of a work item (title, description, acceptance_criteria,
    /// design, priority, owner, labels, parent).
    #[tool(description = "Update allowed fields of a work item (title, description, acceptance_criteria, design, priority, owner, labels, parent). Accepts task ID (full UUID or short_id, e.g., 'k7m2').")]
    pub async fn task_update(
        &self,
        Parameters(p): Parameters<TaskUpdateParams>,
    ) -> Json<serde_json::Value> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());

        let Some(task) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(not_found(&p.id));
        };

        // Resolve new parent epic if provided.
        let epic_id = if let Some(ref par) = p.parent {
            match self.resolve_epic_id(par).await {
                Some(id) => id,
                None => return Json(serde_json::json!({ "error": format!("epic not found: {par}") })),
            }
        } else {
            task.epic_id.clone()
        };

        // Apply partial field overrides.
        let title = p.title.as_deref().unwrap_or(&task.title);
        let description = p.description.as_deref().unwrap_or(&task.description);
        let design = p.design.as_deref().unwrap_or(&task.design);
        let priority = p.priority.unwrap_or(task.priority);
        let owner = p.owner.as_deref().unwrap_or(&task.owner);

        // Merge label changes.
        let labels_json = if p.labels_add.is_some() || p.labels_remove.is_some() {
            let mut current: Vec<String> =
                serde_json::from_str(&task.labels).unwrap_or_default();
            if let Some(add) = &p.labels_add {
                for lbl in add {
                    if !current.contains(lbl) {
                        current.push(lbl.clone());
                    }
                }
            }
            if let Some(remove) = &p.labels_remove {
                current.retain(|l| !remove.contains(l));
            }
            serde_json::to_string(&current).unwrap_or_else(|_| "[]".into())
        } else {
            task.labels.clone()
        };

        let ac_json = p
            .acceptance_criteria
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()))
            .unwrap_or_else(|| task.acceptance_criteria.clone());

        // If parent changed, move the task first.
        if epic_id != task.epic_id {
            if let Err(e) = repo.move_to_epic(&task.id, &epic_id).await {
                return Json(serde_json::json!({ "error": e.to_string() }));
            }
        }

        match repo
            .update(&task.id, title, description, design, priority, owner, &labels_json, &ac_json)
            .await
        {
            Ok(t) => Json(task_to_value(&t)),
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Show details of a work item. Accepts task UUID or short_id.
    #[tool(description = "Show details of a work item including recent activity and blockers. Accepts task ID (full UUID or short_id, e.g., 'k7m2').")]
    pub async fn task_show(
        &self,
        Parameters(p): Parameters<TaskShowParams>,
    ) -> Json<serde_json::Value> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.resolve(&p.id).await {
            Ok(Some(t)) => Json(task_to_value(&t)),
            Ok(None) => Json(not_found(&p.id)),
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// List work items with optional filters and offset-based pagination.
    #[tool(description = "List work items with optional filters and offset-based pagination. Returns {tasks[], total_count, limit, offset, has_more}.")]
    pub async fn task_list(
        &self,
        Parameters(p): Parameters<TaskListParams>,
    ) -> Json<TaskListResponse> {
        // Resolve parent epic if provided.
        let parent = if let Some(ref par) = p.parent {
            self.resolve_epic_id(par).await
        } else {
            None
        };
        // If a parent was specified but couldn't be resolved, return empty.
        if p.parent.is_some() && parent.is_none() {
            let limit = p.limit.unwrap_or(25).clamp(1, 100);
            let offset = p.offset.unwrap_or(0).max(0);
            return Json(TaskListResponse {
                tasks: vec![],
                total_count: 0,
                limit,
                offset,
                has_more: false,
            });
        }

        let limit = p.limit.unwrap_or(25).clamp(1, 100);
        let offset = p.offset.unwrap_or(0).max(0);

        let query = ListQuery {
            status: p.status,
            issue_type: p.issue_type,
            priority: p.priority,
            label: p.label,
            text: p.text,
            parent,
            sort: p.sort.unwrap_or_else(|| "priority".to_owned()),
            limit,
            offset,
        };

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.list_filtered(query).await {
            Ok(result) => {
                let tasks = result.tasks.iter().map(task_to_value).collect();
                Json(TaskListResponse {
                    has_more: offset + limit < result.total_count,
                    tasks,
                    total_count: result.total_count,
                    limit,
                    offset,
                })
            }
            Err(e) => {
                tracing::error!(error = %e, "task_list failed");
                Json(TaskListResponse {
                    tasks: vec![],
                    total_count: 0,
                    limit,
                    offset,
                    has_more: false,
                })
            }
        }
    }

    /// Count work items matching the filter, with optional grouping.
    #[tool(description = "Count work items matching the filter, with optional grouping.")]
    pub async fn task_count(
        &self,
        Parameters(p): Parameters<TaskCountParams>,
    ) -> Json<serde_json::Value> {
        let parent = if let Some(ref par) = p.parent {
            self.resolve_epic_id(par).await
        } else {
            None
        };

        let query = CountQuery {
            status: p.status,
            issue_type: p.issue_type,
            priority: p.priority,
            label: p.label,
            text: p.text,
            parent,
            group_by: p.group_by,
        };

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.count_grouped(query).await {
            Ok(v) => Json(v),
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Get the parent epic of a work item.
    #[tool(description = "Get the parent epic of a work item. Accepts task ID (full UUID or short_id, e.g., 'k7m2').")]
    pub async fn task_parent_get(
        &self,
        Parameters(p): Parameters<TaskParentGetParams>,
    ) -> Json<serde_json::Value> {
        let task_repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(task) = task_repo.resolve(&p.id).await.ok().flatten() else {
            return Json(not_found(&p.id));
        };

        let epic_repo = EpicRepository::new(self.state.db().clone(), self.state.events().clone());
        match epic_repo.get(&task.epic_id).await {
            Ok(Some(epic)) => Json(serde_json::json!({
                "id":          epic.id,
                "short_id":    epic.short_id,
                "title":       epic.title,
                "description": epic.description,
                "emoji":       epic.emoji,
                "color":       epic.color,
                "status":      epic.status,
                "owner":       epic.owner,
                "created_at":  epic.created_at,
                "updated_at":  epic.updated_at,
                "closed_at":   epic.closed_at,
            })),
            Ok(None) => Json(serde_json::json!({ "error": format!("epic not found: {}", task.epic_id) })),
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// List direct children (tasks/features/bugs) of an epic.
    #[tool(description = "List direct children of an epic. Accepts epic ID (full UUID or short_id, e.g., 'k7m2').")]
    pub async fn task_children_list(
        &self,
        Parameters(p): Parameters<TaskChildrenListParams>,
    ) -> Json<serde_json::Value> {
        let Some(epic_id) = self.resolve_epic_id(&p.epic_id).await else {
            return Json(serde_json::json!({ "error": format!("epic not found: {}", p.epic_id) }));
        };

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.list_by_epic(&epic_id).await {
            Ok(tasks) => {
                let items: Vec<serde_json::Value> = tasks.iter().map(task_to_value).collect();
                Json(serde_json::json!({ "tasks": items, "total_count": items.len() }))
            }
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Add a blocker relationship between two tasks. Rejects epics and circular dependencies.
    #[tool(description = "Add a blocker relationship: task 'id' is blocked by task 'blocking_id'. Both accept task ID (full UUID or short_id, e.g., 'k7m2'). Epics cannot participate in blocker relationships — both tasks must be non-epic (task, feature, or bug).")]
    pub async fn task_blockers_add(
        &self,
        Parameters(p): Parameters<TaskBlockersAddParams>,
    ) -> Json<serde_json::Value> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let task_id = match self.resolve_task_not_epic(&p.id).await {
            Ok(id) => id,
            Err(e) => return Json(e),
        };
        let blocking_id = match self.resolve_task_not_epic(&p.blocking_id).await {
            Ok(id) => id,
            Err(e) => return Json(e),
        };
        match repo.add_blocker(&task_id, &blocking_id).await {
            Ok(()) => Json(serde_json::json!({ "ok": true })),
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Remove a blocker relationship between two tasks.
    #[tool(description = "Remove a blocker relationship. Both task IDs accept (full UUID or short_id, e.g., 'k7m2').")]
    pub async fn task_blockers_remove(
        &self,
        Parameters(p): Parameters<TaskBlockersRemoveParams>,
    ) -> Json<serde_json::Value> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(task) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(not_found(&p.id));
        };
        let Some(blocker) = repo.resolve(&p.blocking_id).await.ok().flatten() else {
            return Json(not_found(&p.blocking_id));
        };
        match repo.remove_blocker(&task.id, &blocker.id).await {
            Ok(()) => Json(serde_json::json!({ "ok": true })),
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// List tasks that block the given task.
    #[tool(description = "List tasks that block the given task. Accepts task ID (full UUID or short_id, e.g., 'k7m2').")]
    pub async fn task_blockers_list(
        &self,
        Parameters(p): Parameters<TaskBlockersListParams>,
    ) -> Json<serde_json::Value> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(task) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(not_found(&p.id));
        };
        match repo.list_blockers(&task.id).await {
            Ok(refs) => {
                let blockers: Vec<serde_json::Value> = refs
                    .iter()
                    .map(|b| {
                        let resolved = matches!(b.status.as_str(), "approved" | "closed");
                        serde_json::json!({
                            "blocking_task_id":       b.task_id,
                            "blocking_task_short_id": b.short_id,
                            "blocking_task_title":    b.title,
                            "blocking_task_status":   b.status,
                            "resolved":               resolved,
                        })
                    })
                    .collect();
                Json(serde_json::json!({ "blockers": blockers }))
            }
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// List tasks that are blocked by the given task.
    #[tool(description = "List task IDs that are blocked by the given task. Accepts task ID (full UUID or short_id, e.g., 'k7m2'). Epics will never appear in blocker relationships.")]
    pub async fn task_blocked_list(
        &self,
        Parameters(p): Parameters<TaskBlockedListParams>,
    ) -> Json<serde_json::Value> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(task) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(not_found(&p.id));
        };
        match repo.list_blocked_by(&task.id).await {
            Ok(refs) => {
                let tasks: Vec<serde_json::Value> = refs
                    .iter()
                    .map(|b| {
                        serde_json::json!({
                            "task_id":  b.task_id,
                            "short_id": b.short_id,
                            "title":    b.title,
                            "status":   b.status,
                        })
                    })
                    .collect();
                Json(serde_json::json!({ "tasks": tasks }))
            }
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// List work items ready to start (open status with no blocking dependencies).
    #[tool(description = "List work items ready to start (open status with no blocking dependencies)")]
    pub async fn task_ready(
        &self,
        Parameters(p): Parameters<TaskReadyParams>,
    ) -> Json<serde_json::Value> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let query = ReadyQuery {
            issue_type: p.issue_type,
            label: p.label,
            owner: p.owner,
            priority_max: p.priority_max,
            limit: p.limit.unwrap_or(25).clamp(1, 100),
        };
        match repo.list_ready(query).await {
            Ok(tasks) => {
                let items: Vec<serde_json::Value> = tasks.iter().map(task_to_value).collect();
                Json(serde_json::json!({ "tasks": items }))
            }
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Add a comment to a work item. Creates an activity_log entry with event_type='comment'.
    #[tool(description = "Add a comment to a work item. Accepts task ID (full UUID or short_id, e.g., 'k7m2').")]
    pub async fn task_comment_add(
        &self,
        Parameters(p): Parameters<TaskCommentAddParams>,
    ) -> Json<serde_json::Value> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());

        let Some(task) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(not_found(&p.id));
        };

        let actor_id = p.actor_id.as_deref().unwrap_or("");
        let actor_role = p.actor_role.as_deref().unwrap_or("user");
        let payload = serde_json::json!({ "body": p.body }).to_string();

        match repo
            .log_activity(Some(&task.id), actor_id, actor_role, "comment", &payload)
            .await
        {
            Ok(entry) => Json(serde_json::json!({
                "id":         entry.id,
                "task_id":    entry.task_id,
                "actor_id":   entry.actor_id,
                "actor_role": entry.actor_role,
                "event_type": entry.event_type,
                "payload":    serde_json::from_str::<serde_json::Value>(&entry.payload)
                                  .unwrap_or(serde_json::json!({})),
                "created_at": entry.created_at,
            })),
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

impl DjinnMcpServer {
    /// Resolve a task UUID/short_id to its UUID, rejecting epics with a clear error.
    async fn resolve_task_not_epic(
        &self,
        id: &str,
    ) -> std::result::Result<String, serde_json::Value> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        if let Ok(Some(task)) = repo.resolve(id).await {
            return Ok(task.id);
        }
        // Not found in tasks — check if it's an epic to give a clearer error.
        if self.resolve_epic_id(id).await.is_some() {
            return Err(serde_json::json!({
                "error": format!("epics cannot participate in blocker relationships: {id}")
            }));
        }
        Err(serde_json::json!({ "error": format!("task not found: {id}") }))
    }

    /// Resolve an epic UUID or short_id to its UUID.
    pub(crate) async fn resolve_epic_id(&self, id_or_short: &str) -> Option<String> {
        let id = id_or_short.to_owned();
        self.state
            .db()
            .call(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT id FROM epics WHERE id = ?1 OR short_id = ?1",
                        [&id],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?)
            })
            .await
            .ok()
            .flatten()
    }
}
