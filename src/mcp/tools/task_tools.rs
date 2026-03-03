// MCP tools for task board operations (CRUD, listing, queries).

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::db::repositories::epic::EpicRepository;
use crate::db::repositories::task::{ActivityQuery, CountQuery, ListQuery, ReadyQuery, TaskRepository};
use crate::mcp::server::DjinnMcpServer;
use crate::models::task::{Task, TaskStatus, TransitionAction};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn task_to_value(t: &Task) -> serde_json::Value {
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
    /// Memory note permalinks to add to this task.
    pub memory_refs_add: Option<Vec<String>>,
    /// Memory note permalinks to remove from this task.
    pub memory_refs_remove: Option<Vec<String>>,
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

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskActivityListParams {
    /// Task UUID or short_id (optional — omit to query all tasks).
    pub id: Option<String>,
    /// Filter by event_type (e.g. "status_changed", "comment").
    pub event_type: Option<String>,
    /// ISO-8601 lower bound on created_at.
    pub from_time: Option<String>,
    /// ISO-8601 upper bound on created_at.
    pub to_time: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct BoardHealthParams {
    /// Hours before an in_progress task is considered stale (default: 24).
    pub stale_threshold_hours: Option<i64>,
    /// Project path — accepted for API compatibility, currently unused.
    pub project: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct BoardReconcileParams {
    /// Hours before an in_progress task is considered stale (default: 24).
    pub stale_threshold_hours: Option<i64>,
    /// Project path — accepted for API compatibility, currently unused.
    pub project: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskMemoryRefsParams {
    /// Task UUID or short_id.
    pub id: String,
    /// Absolute project path (accepted for API compatibility).
    pub project: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskTransitionParams {
    /// Task UUID or short_id.
    pub id: String,
    /// Transition action: accept, start, submit_task_review, task_review_start,
    /// task_review_reject, task_review_reject_conflict, task_review_approve,
    /// phase_review_start, phase_review_reject, phase_review_approve, reopen, close,
    /// release, release_task_review, release_phase_review, block, unblock, force_close,
    /// user_override.
    pub action: String,
    /// Required for: task_review_reject, task_review_reject_conflict, phase_review_reject,
    /// reopen, release, release_task_review, release_phase_review, block, force_close.
    pub reason: Option<String>,
    pub actor_id: Option<String>,
    pub actor_role: Option<String>,
    /// Required when action = "user_override". Allowed values: draft, open, needs_task_review,
    /// needs_phase_review, approved, closed.
    pub target_status: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskClaimParams {
    /// Filter by issue type. Positive ("task") or negative ("!epic").
    pub issue_type: Option<String>,
    /// Filter by label value.
    pub label: Option<String>,
    /// Filter by owner email.
    pub owner: Option<String>,
    /// Maximum priority to include (0=highest).
    pub priority_max: Option<i64>,
    /// Session ID of the claiming agent (recorded as actor_id in the activity log).
    pub session_id: Option<String>,
    /// Absolute project path (accepted for API compatibility, currently unused).
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

        let updated = match repo
            .update(&task.id, title, description, design, priority, owner, &labels_json, &ac_json)
            .await
        {
            Ok(t) => t,
            Err(e) => return Json(serde_json::json!({ "error": e.to_string() })),
        };

        // Apply memory_refs changes if requested.
        if p.memory_refs_add.is_some() || p.memory_refs_remove.is_some() {
            let mut refs: Vec<String> =
                serde_json::from_str(&updated.memory_refs).unwrap_or_default();
            if let Some(add) = &p.memory_refs_add {
                for r in add {
                    if !refs.contains(r) {
                        refs.push(r.clone());
                    }
                }
            }
            if let Some(remove) = &p.memory_refs_remove {
                refs.retain(|r| !remove.contains(r));
            }
            let refs_json = serde_json::to_string(&refs).unwrap_or_else(|_| "[]".into());
            match repo.update_memory_refs(&updated.id, &refs_json).await {
                Ok(t) => return Json(task_to_value(&t)),
                Err(e) => return Json(serde_json::json!({ "error": e.to_string() })),
            }
        }

        Json(task_to_value(&updated))
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

    /// Transition a work item through the state machine.
    #[tool(description = "Transition a work item to a new status (e.g., open, in_progress, review, approve, close). Accepts task ID (full UUID or short_id, e.g., 'k7m2'). For user_override action, use target_status to specify the destination column.")]
    pub async fn task_transition(
        &self,
        Parameters(p): Parameters<TaskTransitionParams>,
    ) -> Json<serde_json::Value> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());

        let Some(task) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(not_found(&p.id));
        };

        let action = match TransitionAction::parse(&p.action) {
            Ok(a) => a,
            Err(e) => return Json(serde_json::json!({ "error": e.to_string() })),
        };

        let target_override = if let Some(ref ts) = p.target_status {
            match TaskStatus::parse(ts) {
                Ok(s) => Some(s),
                Err(e) => return Json(serde_json::json!({ "error": e.to_string() })),
            }
        } else {
            None
        };

        let actor_id = p.actor_id.as_deref().unwrap_or("");
        let actor_role = p.actor_role.as_deref().unwrap_or("user");
        let reason = p.reason.as_deref();

        match repo.transition(&task.id, action, actor_id, actor_role, reason, target_override).await {
            Ok(updated) => Json(task_to_value(&updated)),
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Claim the next available work item and transition it to in_progress.
    #[tool(description = "Claim the next available work item (highest priority, oldest) and transition it to in_progress")]
    pub async fn task_claim(
        &self,
        Parameters(p): Parameters<TaskClaimParams>,
    ) -> Json<serde_json::Value> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let query = ReadyQuery {
            issue_type: p.issue_type,
            label: p.label,
            owner: p.owner,
            priority_max: p.priority_max,
            limit: 1,
        };
        let actor_id = p.session_id.as_deref().unwrap_or("");
        match repo.claim(query, actor_id, "coordinator").await {
            Ok(Some(task)) => Json(task_to_value(&task)),
            Ok(None) => Json(serde_json::json!({ "task": null })),
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

    /// Query the activity log with optional filters.
    #[tool(description = "List task activity log entries filtered by task_id, event_type, and/or time range.")]
    pub async fn task_activity_list(
        &self,
        Parameters(p): Parameters<TaskActivityListParams>,
    ) -> Json<serde_json::Value> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());

        // If an id was supplied, resolve it to a full UUID.
        let task_id = if let Some(ref id) = p.id {
            match repo.resolve(id).await {
                Ok(Some(t)) => Some(t.id),
                Ok(None) => return Json(not_found(id)),
                Err(e) => return Json(serde_json::json!({ "error": e.to_string() })),
            }
        } else {
            None
        };

        let q = ActivityQuery {
            task_id,
            event_type: p.event_type,
            from_time: p.from_time,
            to_time: p.to_time,
            limit: p.limit.unwrap_or(50).clamp(1, 200),
            offset: p.offset.unwrap_or(0).max(0),
        };

        match repo.query_activity(q).await {
            Ok(entries) => {
                let items: Vec<serde_json::Value> = entries
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "id":         e.id,
                            "task_id":    e.task_id,
                            "actor_id":   e.actor_id,
                            "actor_role": e.actor_role,
                            "event_type": e.event_type,
                            "payload":    serde_json::from_str::<serde_json::Value>(&e.payload)
                                              .unwrap_or(serde_json::json!({})),
                            "created_at": e.created_at,
                        })
                    })
                    .collect();
                Json(serde_json::json!({ "entries": items, "count": items.len() }))
            }
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Board health report: epic progress, stale tasks, and review queue.
    #[tool(description = "Returns aggregate health report (total notes, broken links, orphan notes, stale notes by folder).")]
    pub async fn board_health(
        &self,
        Parameters(p): Parameters<BoardHealthParams>,
    ) -> Json<serde_json::Value> {
        let stale_hours = p.stale_threshold_hours.unwrap_or(24).max(1);
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.board_health(stale_hours).await {
            Ok(report) => Json(report),
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Heal stale tasks and recover orphaned states.
    #[tool(description = "Trigger board reconciliation: heal stale tasks, recover stuck sessions, trigger overdue reviews, disable dead models, and reconcile phases. Returns action counts.")]
    pub async fn board_reconcile(
        &self,
        Parameters(p): Parameters<BoardReconcileParams>,
    ) -> Json<serde_json::Value> {
        let stale_hours = p.stale_threshold_hours.unwrap_or(24).max(1);
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.reconcile(stale_hours).await {
            Ok(result) => Json(result),
            Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// List memory note permalinks associated with a task. Accepts task ID
    /// (full UUID or short_id, e.g., 'k7m2').
    #[tool(description = "List memory note permalinks associated with a task. Accepts task ID (full UUID or short_id, e.g., 'k7m2').")]
    pub async fn task_memory_refs(
        &self,
        Parameters(p): Parameters<TaskMemoryRefsParams>,
    ) -> Json<serde_json::Value> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(task) = repo.resolve(&p.id).await.ok().flatten() else {
            return Json(not_found(&p.id));
        };
        let refs: serde_json::Value =
            serde_json::from_str(&task.memory_refs).unwrap_or(serde_json::json!([]));
        Json(serde_json::json!({ "id": task.id, "short_id": task.short_id, "memory_refs": refs }))
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
        let db = self.state.db();
        db.ensure_initialized().await.ok()?;
        sqlx::query_scalar::<_, String>(
            "SELECT id FROM epics WHERE id = ?1 OR short_id = ?1",
        )
        .bind(id_or_short)
        .fetch_optional(db.pool())
        .await
        .ok()
        .flatten()
    }
}
