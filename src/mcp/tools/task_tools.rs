// MCP tools for task board operations (CRUD, listing, queries).

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::db::repositories::epic::EpicRepository;
use crate::db::repositories::session::SessionRepository;
use crate::db::repositories::task::{
    ActivityQuery, CountQuery, ListQuery, ReadyQuery, TaskRepository,
};
use crate::mcp::server::DjinnMcpServer;
use crate::mcp::tools::validation::{
    validate_ac_count, validate_actor_id, validate_actor_role, validate_body, validate_description,
    validate_design, validate_issue_type, validate_label, validate_labels_count, validate_limit,
    validate_offset, validate_owner, validate_priority, validate_reason, validate_sort,
    validate_title,
};
use crate::mcp::tools::{AnyJson, ObjectJson, json_object};
use crate::models::session::SessionStatus;
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
        "merge_commit_sha":     t.merge_commit_sha,
    })
}

fn not_found(id: &str) -> serde_json::Value {
    serde_json::json!({ "error": format!("task not found: {id}") })
}

/// Validate and collect labels, returning the validated list or an error.
fn validate_labels(labels: &[String]) -> Result<Vec<String>, String> {
    validate_labels_count(labels.len())?;
    labels.iter().map(|l| validate_label(l)).collect()
}

// ── Param / response structs ─────────────────────────────────────────────────

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskCreateParams {
    /// Absolute project path.
    pub project: String,
    /// Parent epic ID — UUID or short_id (required).
    pub epic_id: Option<String>,
    pub title: String,
    /// Task type: "task" (default), "feature", or "bug".
    pub issue_type: Option<String>,
    pub description: Option<String>,
    pub design: Option<String>,
    pub priority: Option<i64>,
    pub owner: Option<String>,
    pub labels: Option<Vec<String>>,
    pub acceptance_criteria: Option<Vec<AnyJson>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskUpdateParams {
    /// Absolute project path.
    pub project: String,
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
    pub acceptance_criteria: Option<Vec<AnyJson>>,
    /// New parent epic UUID or short_id.
    pub epic_id: Option<String>,
    /// Memory note permalinks to add to this task.
    pub memory_refs_add: Option<Vec<String>>,
    /// Memory note permalinks to remove from this task.
    pub memory_refs_remove: Option<Vec<String>>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskShowParams {
    /// Absolute project path.
    pub project: String,
    /// Task UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskListParams {
    /// Absolute project path.
    pub project: String,
    pub status: Option<String>,
    /// Positive ("task") or negative ("!epic") issue_type filter.
    pub issue_type: Option<String>,
    pub priority: Option<i64>,
    /// Filter by label value.
    pub label: Option<String>,
    /// Full-text search on title and description.
    pub text: Option<String>,
    /// Sort order: "priority" (default), "created", "created_desc",
    /// "updated", "updated_desc", "closed".
    pub sort: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskCountParams {
    /// Absolute project path.
    pub project: String,
    pub status: Option<String>,
    pub issue_type: Option<String>,
    pub priority: Option<i64>,
    pub label: Option<String>,
    pub text: Option<String>,
    /// Group results by: "status", "priority", "issue_type", or "epic".
    pub group_by: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskCommentAddParams {
    /// Absolute project path.
    pub project: String,
    /// Task UUID or short_id.
    pub id: String,
    /// Comment body text.
    pub body: String,
    pub actor_id: Option<String>,
    pub actor_role: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskBlockersAddParams {
    /// Absolute project path.
    pub project: String,
    /// Task UUID or short_id (the task being blocked).
    pub id: String,
    /// Task UUID or short_id of the task that blocks it.
    pub blocking_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskBlockersRemoveParams {
    /// Absolute project path.
    pub project: String,
    /// Task UUID or short_id (the blocked task).
    pub id: String,
    /// Task UUID or short_id of the blocker to remove.
    pub blocking_id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskBlockersListParams {
    /// Absolute project path.
    pub project: String,
    /// Task UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskBlockedListParams {
    /// Absolute project path.
    pub project: String,
    /// Task UUID or short_id.
    pub id: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskReadyParams {
    /// Filter by label value.
    pub label: Option<String>,
    /// Filter by owner email.
    pub owner: Option<String>,
    /// Maximum priority to include (0=highest, higher numbers=lower priority).
    pub priority_max: Option<i64>,
    pub limit: Option<i64>,
    /// Absolute project path.
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskActivityListParams {
    /// Absolute project path.
    pub project: String,
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
    /// Absolute project path.
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct BoardReconcileParams {
    /// Hours before an in_progress task is considered stale (default: 24).
    pub stale_threshold_hours: Option<i64>,
    /// Absolute project path.
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskMemoryRefsParams {
    /// Task UUID or short_id.
    pub id: String,
    /// Absolute project path.
    pub project: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskTransitionParams {
    /// Absolute project path.
    pub project: String,
    /// Task UUID or short_id.
    pub id: String,
    /// Transition action: accept, start, submit_task_review, task_review_start,
    /// task_review_reject, task_review_reject_conflict, task_review_approve,
    /// reopen, close, release, release_task_review, block, unblock, force_close,
    /// user_override.
    pub action: String,
    /// Required for: task_review_reject, task_review_reject_conflict,
    /// reopen, release, release_task_review, block, force_close.
    pub reason: Option<String>,
    pub actor_id: Option<String>,
    pub actor_role: Option<String>,
    /// Required when action = "user_override". Allowed values: draft, open, needs_task_review,
    /// in_task_review, in_progress, blocked, closed.
    pub target_status: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TaskClaimParams {
    /// Filter by label value.
    pub label: Option<String>,
    /// Filter by owner email.
    pub owner: Option<String>,
    /// Maximum priority to include (0=highest).
    pub priority_max: Option<i64>,
    /// Session ID of the claiming agent (recorded as actor_id in the activity log).
    pub session_id: Option<String>,
    /// Absolute project path.
    pub project: String,
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct TaskListResponse {
    pub tasks: Vec<AnyJson>,
    pub total_count: i64,
    pub limit: i64,
    pub offset: i64,
    pub has_more: bool,
}

// ── Tool implementations ─────────────────────────────────────────────────────

#[tool_router(router = task_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Create a new work item (task, feature, or bug) under an epic.
    #[tool(
        description = "Create a new work item (task, feature, or bug) under an epic. Accepts epic_id as UUID or short_id."
    )]
    pub async fn task_create(
        &self,
        Parameters(p): Parameters<TaskCreateParams>,
    ) -> Json<ObjectJson> {
        // Validate fields.
        let title = match validate_title(&p.title) {
            Ok(t) => t,
            Err(e) => return json_object(serde_json::json!({ "error": e })),
        };
        let description = p.description.as_deref().unwrap_or("");
        if let Err(e) = validate_description(description) {
            return json_object(serde_json::json!({ "error": e }));
        }
        let design = p.design.as_deref().unwrap_or("");
        if let Err(e) = validate_design(design) {
            return json_object(serde_json::json!({ "error": e }));
        }
        let issue_type = p.issue_type.as_deref().unwrap_or("task");
        if let Err(e) = validate_issue_type(issue_type) {
            return json_object(serde_json::json!({ "error": e }));
        }
        let priority = p.priority.unwrap_or(0);
        if let Err(e) = validate_priority(priority) {
            return json_object(serde_json::json!({ "error": e }));
        }
        let owner = match validate_owner(p.owner.as_deref().unwrap_or("")) {
            Ok(o) => o,
            Err(e) => return json_object(serde_json::json!({ "error": e })),
        };
        if let Some(ref labels) = p.labels {
            if let Err(e) = validate_labels(labels) {
                return json_object(serde_json::json!({ "error": e }));
            }
        }
        if let Some(ref ac) = p.acceptance_criteria {
            if let Err(e) = validate_ac_count(ac.len()) {
                return json_object(serde_json::json!({ "error": e }));
            }
        }

        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        // Resolve parent epic (optional).
        let epic_id = if let Some(epic_ref) = p.epic_id.as_deref() {
            let epic_repo =
                EpicRepository::new(self.state.db().clone(), self.state.events().clone());
            let Some(epic) = epic_repo
                .resolve_in_project(&project_id, epic_ref)
                .await
                .ok()
                .flatten()
            else {
                return json_object(
                    serde_json::json!({ "error": format!("epic not found: {epic_ref}") }),
                );
            };
            Some(epic.id)
        } else {
            None
        };

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());

        let task = match repo
            .create_in_project(
                &project_id,
                epic_id.as_deref(),
                &title,
                description,
                design,
                issue_type,
                priority,
                &owner,
            )
            .await
        {
            Ok(t) => t,
            Err(e) => return json_object(serde_json::json!({ "error": e.to_string() })),
        };

        // Apply labels / ac if provided.
        let has_labels = p.labels.as_ref().map(|v| !v.is_empty()).unwrap_or(false);
        let has_ac = p
            .acceptance_criteria
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false);

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
                .update(
                    &task.id,
                    &task.title,
                    &task.description,
                    &task.design,
                    task.priority,
                    &task.owner,
                    &labels_json,
                    &ac_json,
                )
                .await;
            if let Ok(t) = updated {
                return json_object(task_to_value(&t));
            }
        }

        json_object(task_to_value(&task))
    }

    /// Update allowed fields of a work item.
    #[tool(
        description = "Update allowed fields of a work item (title, description, acceptance_criteria, design, priority, owner, labels, epic_id). Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_update(
        &self,
        Parameters(p): Parameters<TaskUpdateParams>,
    ) -> Json<ObjectJson> {
        // Validate provided fields.
        if let Some(ref t) = p.title {
            if let Err(e) = validate_title(t) {
                return json_object(serde_json::json!({ "error": e }));
            }
        }
        if let Some(ref d) = p.description {
            if let Err(e) = validate_description(d) {
                return json_object(serde_json::json!({ "error": e }));
            }
        }
        if let Some(ref d) = p.design {
            if let Err(e) = validate_design(d) {
                return json_object(serde_json::json!({ "error": e }));
            }
        }
        if let Some(prio) = p.priority {
            if let Err(e) = validate_priority(prio) {
                return json_object(serde_json::json!({ "error": e }));
            }
        }
        if let Some(ref o) = p.owner {
            if let Err(e) = validate_owner(o) {
                return json_object(serde_json::json!({ "error": e }));
            }
        }
        if let Some(ref add) = p.labels_add {
            if let Err(e) = validate_labels(add) {
                return json_object(serde_json::json!({ "error": e }));
            }
        }
        if let Some(ref ac) = p.acceptance_criteria {
            if let Err(e) = validate_ac_count(ac.len()) {
                return json_object(serde_json::json!({ "error": e }));
            }
        }

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        let Some(task) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return json_object(not_found(&p.id));
        };

        // Resolve new parent epic if provided.
        let epic_id: Option<String> = if let Some(ref par) = p.epic_id {
            let epic_repo =
                EpicRepository::new(self.state.db().clone(), self.state.events().clone());
            let Some(epic) = epic_repo
                .resolve_in_project(&project_id, par)
                .await
                .ok()
                .flatten()
            else {
                return json_object(
                    serde_json::json!({ "error": format!("epic not found: {par}") }),
                );
            };
            Some(epic.id)
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
            let mut current: Vec<String> = serde_json::from_str(&task.labels).unwrap_or_default();
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
            if let Err(e) = validate_labels_count(current.len()) {
                return json_object(serde_json::json!({ "error": e }));
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
            if let Err(e) = repo.move_to_epic(&task.id, epic_id.as_deref()).await {
                return json_object(serde_json::json!({ "error": e.to_string() }));
            }
        }

        let updated = match repo
            .update(
                &task.id,
                title,
                description,
                design,
                priority,
                owner,
                &labels_json,
                &ac_json,
            )
            .await
        {
            Ok(t) => t,
            Err(e) => return json_object(serde_json::json!({ "error": e.to_string() })),
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
                Ok(t) => return json_object(task_to_value(&t)),
                Err(e) => return json_object(serde_json::json!({ "error": e.to_string() })),
            }
        }

        json_object(task_to_value(&updated))
    }

    /// Show details of a work item. Accepts task UUID or short_id.
    #[tool(
        description = "Show details of a work item including recent activity and blockers. Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_show(&self, Parameters(p): Parameters<TaskShowParams>) -> Json<ObjectJson> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let session_repo =
            SessionRepository::new(self.state.db().clone(), self.state.events().clone());
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        match repo.resolve_in_project(&project_id, &p.id).await {
            Ok(Some(t)) => {
                let mut value = task_to_value(&t);
                if let Some(map) = value.as_object_mut() {
                    let session_count = session_repo.count_for_task(&t.id).await.unwrap_or(0);
                    let active_session = session_repo.active_for_task(&t.id).await.ok().flatten();
                    map.insert(
                        "session_count".to_string(),
                        serde_json::json!(session_count),
                    );
                    map.insert(
                        "active_session".to_string(),
                        serde_json::json!(active_session),
                    );
                }
                json_object(value)
            }
            Ok(None) => json_object(not_found(&p.id)),
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// List work items with optional filters and offset-based pagination.
    #[tool(
        description = "List work items with optional filters and offset-based pagination. Returns {tasks[], total_count, limit, offset, has_more}."
    )]
    pub async fn task_list(
        &self,
        Parameters(p): Parameters<TaskListParams>,
    ) -> Json<TaskListResponse> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(_) => {
                let limit = validate_limit(p.limit.unwrap_or(25));
                let offset = validate_offset(p.offset.unwrap_or(0));
                return Json(TaskListResponse {
                    tasks: vec![],
                    total_count: 0,
                    limit,
                    offset,
                    has_more: false,
                });
            }
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
            let limit = validate_limit(p.limit.unwrap_or(25));
            let offset = validate_offset(p.offset.unwrap_or(0));
            tracing::error!(error = %e, "task_list: invalid sort");
            return Json(TaskListResponse {
                tasks: vec![],
                total_count: 0,
                limit,
                offset,
                has_more: false,
            });
        }
        if let Some(prio) = p.priority {
            if let Err(e) = validate_priority(prio) {
                let limit = validate_limit(p.limit.unwrap_or(25));
                let offset = validate_offset(p.offset.unwrap_or(0));
                tracing::error!(error = %e, "task_list: invalid priority");
                return Json(TaskListResponse {
                    tasks: vec![],
                    total_count: 0,
                    limit,
                    offset,
                    has_more: false,
                });
            }
        }

        let limit = validate_limit(p.limit.unwrap_or(25));
        let offset = validate_offset(p.offset.unwrap_or(0));

        let query = ListQuery {
            project_id: Some(project_id),
            status: p.status,
            issue_type: p.issue_type,
            priority: p.priority,
            label: p.label,
            text: p.text,
            parent: None,
            sort: sort.to_owned(),
            limit,
            offset,
        };

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.list_filtered(query).await {
            Ok(result) => {
                let tasks = result
                    .tasks
                    .iter()
                    .map(|t| AnyJson::from(task_to_value(t)))
                    .collect();
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
    pub async fn task_count(&self, Parameters(p): Parameters<TaskCountParams>) -> Json<ObjectJson> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        if let Some(ref gb) = p.group_by {
            if let Err(e) = validate_sort(gb, &["status", "priority", "issue_type", "epic"]) {
                return json_object(serde_json::json!({ "error": e }));
            }
        }
        if let Some(prio) = p.priority {
            if let Err(e) = validate_priority(prio) {
                return json_object(serde_json::json!({ "error": e }));
            }
        }

        let query = CountQuery {
            project_id: Some(project_id),
            status: p.status,
            issue_type: p.issue_type,
            priority: p.priority,
            label: p.label,
            text: p.text,
            parent: None,
            group_by: p.group_by,
        };

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.count_grouped(query).await {
            Ok(v) => json_object(v),
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Add a blocker relationship between two tasks. Rejects epics and circular dependencies.
    #[tool(
        description = "Add a blocker relationship: task 'id' is blocked by task 'blocking_id'. Both accept task ID (full UUID or short_id, e.g., 'k7m2'). Epics cannot participate in blocker relationships — both tasks must be non-epic (task, feature, or bug)."
    )]
    pub async fn task_blockers_add(
        &self,
        Parameters(p): Parameters<TaskBlockersAddParams>,
    ) -> Json<ObjectJson> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let task_id = match self.resolve_task_not_epic(&project_id, &p.id).await {
            Ok(id) => id,
            Err(e) => return json_object(e),
        };
        let blocking_id = match self
            .resolve_task_not_epic(&project_id, &p.blocking_id)
            .await
        {
            Ok(id) => id,
            Err(e) => return json_object(e),
        };
        match repo.add_blocker(&task_id, &blocking_id).await {
            Ok(()) => json_object(serde_json::json!({ "ok": true })),
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Remove a blocker relationship between two tasks.
    #[tool(
        description = "Remove a blocker relationship. Both task IDs accept (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_blockers_remove(
        &self,
        Parameters(p): Parameters<TaskBlockersRemoveParams>,
    ) -> Json<ObjectJson> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(task) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return json_object(not_found(&p.id));
        };
        let Some(blocker) = repo
            .resolve_in_project(&project_id, &p.blocking_id)
            .await
            .ok()
            .flatten()
        else {
            return json_object(not_found(&p.blocking_id));
        };
        match repo.remove_blocker(&task.id, &blocker.id).await {
            Ok(()) => json_object(serde_json::json!({ "ok": true })),
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// List tasks that block the given task.
    #[tool(
        description = "List tasks that block the given task. Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_blockers_list(
        &self,
        Parameters(p): Parameters<TaskBlockersListParams>,
    ) -> Json<ObjectJson> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(task) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return json_object(not_found(&p.id));
        };
        match repo.list_blockers(&task.id).await {
            Ok(refs) => {
                let blockers: Vec<_> = refs
                    .iter()
                    .map(|b| {
                        let resolved = b.status == "closed";
                        serde_json::json!({
                            "blocking_task_id":       b.task_id,
                            "blocking_task_short_id": b.short_id,
                            "blocking_task_title":    b.title,
                            "blocking_task_status":   b.status,
                            "resolved":               resolved,
                        })
                    })
                    .collect();
                json_object(serde_json::json!({ "blockers": blockers }))
            }
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// List tasks that are blocked by the given task.
    #[tool(
        description = "List task IDs that are blocked by the given task. Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_blocked_list(
        &self,
        Parameters(p): Parameters<TaskBlockedListParams>,
    ) -> Json<ObjectJson> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(task) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return json_object(not_found(&p.id));
        };
        match repo.list_blocked_by(&task.id).await {
            Ok(refs) => {
                let tasks: Vec<_> = refs
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
                json_object(serde_json::json!({ "tasks": tasks }))
            }
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// List work items ready to start (open status with no blocking dependencies).
    #[tool(
        description = "List work items ready to start (open status with no blocking dependencies)"
    )]
    pub async fn task_ready(&self, Parameters(p): Parameters<TaskReadyParams>) -> Json<ObjectJson> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        if let Some(ref o) = p.owner {
            if let Err(e) = validate_owner(o) {
                return json_object(serde_json::json!({ "error": e }));
            }
        }
        if let Some(pmax) = p.priority_max {
            if let Err(e) = validate_priority(pmax) {
                return json_object(serde_json::json!({ "error": e }));
            }
        }

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let query = ReadyQuery {
            project_id: Some(project_id),
            issue_type: None,
            label: p.label,
            owner: p.owner,
            priority_max: p.priority_max,
            limit: validate_limit(p.limit.unwrap_or(25)),
        };
        match repo.list_ready(query).await {
            Ok(tasks) => {
                let items: Vec<_> = tasks.iter().map(task_to_value).collect();
                json_object(serde_json::json!({ "tasks": items }))
            }
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Transition a work item through the state machine.
    #[tool(
        description = "Transition a work item to a new status (e.g., open, in_progress, review, approve, close). Accepts task ID (full UUID or short_id, e.g., 'k7m2'). For user_override action, use target_status to specify the destination column."
    )]
    pub async fn task_transition(
        &self,
        Parameters(p): Parameters<TaskTransitionParams>,
    ) -> Json<ObjectJson> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let actor_id = p.actor_id.as_deref().unwrap_or("");
        if let Err(e) = validate_actor_id(actor_id) {
            return json_object(serde_json::json!({ "error": e }));
        }
        let actor_role = p.actor_role.as_deref().unwrap_or("user");
        if let Err(e) = validate_actor_role(actor_role) {
            return json_object(serde_json::json!({ "error": e }));
        }
        if let Some(ref r) = p.reason {
            if let Err(e) = validate_reason(r) {
                return json_object(serde_json::json!({ "error": e }));
            }
        }

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());

        let Some(task) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return json_object(not_found(&p.id));
        };

        let action = match TransitionAction::parse(&p.action) {
            Ok(a) => a,
            Err(e) => return json_object(serde_json::json!({ "error": e.to_string() })),
        };

        let target_override = if let Some(ref ts) = p.target_status {
            match TaskStatus::parse(ts) {
                Ok(s) => Some(s),
                Err(e) => return json_object(serde_json::json!({ "error": e.to_string() })),
            }
        } else {
            None
        };

        let reason = p.reason.as_deref();

        match repo
            .transition(
                &task.id,
                action,
                actor_id,
                actor_role,
                reason,
                target_override,
            )
            .await
        {
            Ok(updated) => json_object(task_to_value(&updated)),
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Claim the next available work item and transition it to in_progress.
    #[tool(
        description = "Claim the next available work item (highest priority, oldest) and transition it to in_progress"
    )]
    pub async fn task_claim(&self, Parameters(p): Parameters<TaskClaimParams>) -> Json<ObjectJson> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        if let Some(ref o) = p.owner {
            if let Err(e) = validate_owner(o) {
                return json_object(serde_json::json!({ "error": e }));
            }
        }

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let query = ReadyQuery {
            project_id: Some(project_id),
            issue_type: None,
            label: p.label,
            owner: p.owner,
            priority_max: p.priority_max,
            limit: 1,
        };
        let actor_id = p.session_id.as_deref().unwrap_or("");
        match repo.claim(query, actor_id, "coordinator").await {
            Ok(Some(task)) => json_object(task_to_value(&task)),
            Ok(None) => json_object(serde_json::json!({ "task": null })),
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Add a comment to a work item. Creates an activity_log entry with event_type='comment'.
    #[tool(
        description = "Add a comment to a work item. Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_comment_add(
        &self,
        Parameters(p): Parameters<TaskCommentAddParams>,
    ) -> Json<ObjectJson> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        if let Err(e) = validate_body(&p.body) {
            return json_object(serde_json::json!({ "error": e }));
        }
        let actor_id = p.actor_id.as_deref().unwrap_or("");
        if let Err(e) = validate_actor_id(actor_id) {
            return json_object(serde_json::json!({ "error": e }));
        }
        let actor_role = p.actor_role.as_deref().unwrap_or("user");
        if let Err(e) = validate_actor_role(actor_role) {
            return json_object(serde_json::json!({ "error": e }));
        }

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());

        let Some(task) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return json_object(not_found(&p.id));
        };

        let payload = serde_json::json!({ "body": p.body }).to_string();

        match repo
            .log_activity(Some(&task.id), actor_id, actor_role, "comment", &payload)
            .await
        {
            Ok(entry) => json_object(serde_json::json!({
                "id":         entry.id,
                "task_id":    entry.task_id,
                "actor_id":   entry.actor_id,
                "actor_role": entry.actor_role,
                "event_type": entry.event_type,
                "payload":    serde_json::from_str::<serde_json::Value>(&entry.payload)
                                  .unwrap_or(serde_json::json!({})),
                "created_at": entry.created_at,
            })),
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Query the activity log with optional filters.
    #[tool(
        description = "List task activity log entries filtered by task_id, event_type, and/or time range."
    )]
    pub async fn task_activity_list(
        &self,
        Parameters(p): Parameters<TaskActivityListParams>,
    ) -> Json<ObjectJson> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };

        // If an id was supplied, resolve it to a full UUID.
        let task_id = if let Some(ref id) = p.id {
            match repo.resolve_in_project(&project_id, id).await {
                Ok(Some(t)) => Some(t.id),
                Ok(None) => return json_object(not_found(id)),
                Err(e) => return json_object(serde_json::json!({ "error": e.to_string() })),
            }
        } else {
            None
        };

        let q = ActivityQuery {
            project_id: Some(project_id),
            task_id,
            event_type: p.event_type,
            from_time: p.from_time,
            to_time: p.to_time,
            limit: validate_limit(p.limit.unwrap_or(50)),
            offset: validate_offset(p.offset.unwrap_or(0)),
        };

        match repo.query_activity(q).await {
            Ok(entries) => {
                let items: Vec<_> = entries
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
                json_object(serde_json::json!({ "entries": items, "count": items.len() }))
            }
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Board health report: epic progress, stale tasks, and review queue.
    #[tool(
        description = "Returns aggregate health report (total notes, broken links, orphan notes, stale notes by folder)."
    )]
    pub async fn board_health(
        &self,
        Parameters(p): Parameters<BoardHealthParams>,
    ) -> Json<ObjectJson> {
        if let Err(e) = self.require_project_id(&p.project).await {
            return e;
        }
        let stale_hours = p.stale_threshold_hours.unwrap_or(24).max(1);
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        match repo.board_health(stale_hours).await {
            Ok(report) => json_object(report),
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// Heal stale tasks and recover orphaned states.
    #[tool(
        description = "Trigger board reconciliation: heal stale tasks, recover stuck sessions, trigger overdue reviews, disable dead models, and reconcile phases. Returns action counts."
    )]
    pub async fn board_reconcile(
        &self,
        Parameters(p): Parameters<BoardReconcileParams>,
    ) -> Json<ObjectJson> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let stale_hours = p.stale_threshold_hours.unwrap_or(24).max(1);
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(supervisor) = self.state.supervisor().await else {
            return json_object(serde_json::json!({ "error": "supervisor actor not initialized" }));
        };
        let Some(coordinator) = self.state.coordinator().await else {
            return json_object(
                serde_json::json!({ "error": "coordinator actor not initialized" }),
            );
        };
        let session_repo =
            SessionRepository::new(self.state.db().clone(), self.state.events().clone());

        match repo.reconcile(stale_hours).await {
            Ok(mut result) => {
                let running_sessions = match session_repo.list_active_in_project(&project_id).await
                {
                    Ok(sessions) => sessions,
                    Err(e) => return json_object(serde_json::json!({ "error": e.to_string() })),
                };

                let mut finalized_stale_session_ids = Vec::new();
                for session in running_sessions {
                    let has_runtime_session = match supervisor.has_session(&session.task_id).await {
                        Ok(v) => v,
                        Err(e) => {
                            return json_object(serde_json::json!({ "error": e.to_string() }));
                        }
                    };
                    if has_runtime_session {
                        continue;
                    }
                    if session_repo
                        .update(
                            &session.id,
                            SessionStatus::Interrupted,
                            session.tokens_in,
                            session.tokens_out,
                        )
                        .await
                        .is_ok()
                    {
                        finalized_stale_session_ids.push(session.id);
                    }
                }

                let recovery_triggered = if finalized_stale_session_ids.is_empty() {
                    false
                } else {
                    coordinator
                        .trigger_dispatch_for_project(&project_id)
                        .await
                        .is_ok()
                };

                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "stale_sessions_finalized".to_string(),
                        serde_json::json!(finalized_stale_session_ids.len()),
                    );
                    obj.insert(
                        "stale_session_ids".to_string(),
                        serde_json::json!(finalized_stale_session_ids),
                    );
                    obj.insert(
                        "recovery_triggered".to_string(),
                        serde_json::json!(recovery_triggered),
                    );
                }

                json_object(result)
            }
            Err(e) => json_object(serde_json::json!({ "error": e.to_string() })),
        }
    }

    /// List memory note permalinks associated with a task.
    #[tool(
        description = "List memory note permalinks associated with a task. Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_memory_refs(
        &self,
        Parameters(p): Parameters<TaskMemoryRefsParams>,
    ) -> Json<ObjectJson> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return e,
        };
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(task) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return json_object(not_found(&p.id));
        };
        let refs: serde_json::Value =
            serde_json::from_str(&task.memory_refs).unwrap_or(serde_json::json!([]));
        json_object(
            serde_json::json!({ "id": task.id, "short_id": task.short_id, "memory_refs": refs }),
        )
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

impl DjinnMcpServer {
    async fn require_project_id(
        &self,
        project: &str,
    ) -> std::result::Result<String, Json<ObjectJson>> {
        self.resolve_project_id(project)
            .await
            .map_err(|e| json_object(serde_json::json!({ "error": e })))
    }

    /// Resolve a task UUID/short_id to its UUID, rejecting epics with a clear error.
    async fn resolve_task_not_epic(
        &self,
        project_id: &str,
        id: &str,
    ) -> std::result::Result<String, serde_json::Value> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        if let Ok(Some(task)) = repo.resolve_in_project(project_id, id).await {
            return Ok(task.id);
        }
        // Not found in tasks — check if it's an epic to give a clearer error.
        if let Ok(Some(_)) =
            EpicRepository::new(self.state.db().clone(), self.state.events().clone())
                .resolve_in_project(project_id, id)
                .await
        {
            return Err(serde_json::json!({
                "error": format!("epics cannot participate in blocker relationships: {id}")
            }));
        }
        Err(serde_json::json!({ "error": format!("task not found: {id}") }))
    }
}
