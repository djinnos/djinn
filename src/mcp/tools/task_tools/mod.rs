// MCP tools for task board operations (CRUD, listing, queries).

use std::collections::HashMap;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::db::repositories::epic::EpicRepository;
use crate::db::repositories::project::ProjectRepository;
use crate::db::repositories::session::SessionRepository;
use crate::db::repositories::task::{
    ActivityQuery, CountQuery, ListQuery, ReadyQuery, TaskRepository,
};
use crate::mcp::server::DjinnMcpServer;
use crate::mcp::tools::AnyJson;
use crate::mcp::tools::validation::{
    validate_ac_count, validate_actor_id, validate_actor_role, validate_body, validate_description,
    validate_design, validate_issue_type, validate_label, validate_labels_count, validate_limit,
    validate_offset, validate_owner, validate_priority, validate_reason, validate_sort,
    validate_title,
};
use crate::models::session::SessionStatus;
use crate::models::task::{Task, TaskStatus, TransitionAction};

mod board;
mod types;
#[cfg(test)]
mod tests;

pub use types::*;

// ── Tool implementations ─────────────────────────────────────────────────────

#[tool_router(router = task_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Create a new work item (task, feature, or bug) under an epic.
    #[tool(
        description = "Create a new work item (task, feature, or bug) under an epic. Accepts epic_id as UUID or short_id. Use blocked_by to set blocker dependencies atomically at creation."
    )]
    pub async fn task_create(
        &self,
        Parameters(p): Parameters<TaskCreateParams>,
    ) -> Json<ErrorOr<TaskResponse>> {
        // Validate fields.
        let title = match validate_title(&p.title) {
            Ok(t) => t,
            Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e))),
        };
        let description = p.description.as_deref().unwrap_or("");
        if let Err(e) = validate_description(description) {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        let design = p.design.as_deref().unwrap_or("");
        if let Err(e) = validate_design(design) {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        let issue_type = p.issue_type.as_deref().unwrap_or("task");
        if let Err(e) = validate_issue_type(issue_type) {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        let priority = p.priority.unwrap_or(0);
        if let Err(e) = validate_priority(priority) {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        let owner = match validate_owner(p.owner.as_deref().unwrap_or("")) {
            Ok(o) => o,
            Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e))),
        };
        if let Some(ref labels) = p.labels
            && let Err(e) = validate_labels(labels)
        {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        if let Some(ref ac) = p.acceptance_criteria
            && let Err(e) = validate_ac_count(ac.len())
        {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }

        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
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
                return Json(ErrorOr::Error(ErrorResponse::new(format!(
                    "epic not found: {epic_ref}"
                ))));
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
            Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        };

        // Apply labels / ac if provided.
        let has_labels = p.labels.as_ref().map(|v| !v.is_empty()).unwrap_or(false);
        let has_ac = p
            .acceptance_criteria
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false);

        let mut task = task;

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

            if let Ok(t) = repo
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
                .await
            {
                task = t;
            }
        }

        // Apply memory_refs if provided.
        if let Some(ref refs) = p.memory_refs
            && !refs.is_empty() {
                let refs_json = serde_json::to_string(refs).unwrap_or_else(|_| "[]".into());
                if let Ok(t) = repo.update_memory_refs(&task.id, &refs_json).await {
                    task = t;
                }
            }

        // Apply blocked_by relationships atomically at creation.
        if let Some(ref blockers) = p.blocked_by {
            for blocker_ref in blockers {
                let blocking_id = match self
                    .resolve_task_not_epic(&project_id, blocker_ref)
                    .await
                {
                    Ok(id) => id,
                    Err(e) => return Json(ErrorOr::Error(e)),
                };
                if let Err(e) = repo.add_blocker(&task.id, &blocking_id).await {
                    return Json(ErrorOr::Error(ErrorResponse::new(e.to_string())));
                }
            }
        }

        Json(ErrorOr::Ok(task_to_response(&task)))
    }

    /// Update allowed fields of a work item.
    #[tool(
        description = "Update allowed fields of a work item (title, description, acceptance_criteria, design, priority, owner, labels, epic_id, blocked_by_add, blocked_by_remove). Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_update(
        &self,
        Parameters(p): Parameters<TaskUpdateParams>,
    ) -> Json<ErrorOr<TaskResponse>> {
        // Validate provided fields.
        if let Some(ref t) = p.title
            && let Err(e) = validate_title(t)
        {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        if let Some(ref d) = p.description
            && let Err(e) = validate_description(d)
        {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        if let Some(ref d) = p.design
            && let Err(e) = validate_design(d)
        {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        if let Some(prio) = p.priority
            && let Err(e) = validate_priority(prio)
        {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        if let Some(ref o) = p.owner
            && let Err(e) = validate_owner(o)
        {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        if let Some(ref add) = p.labels_add
            && let Err(e) = validate_labels(add)
        {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        if let Some(ref ac) = p.acceptance_criteria
            && let Err(e) = validate_ac_count(ac.len())
        {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };

        let Some(task) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(ErrorOr::Error(not_found(&p.id)));
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
                return Json(ErrorOr::Error(ErrorResponse::new(format!(
                    "epic not found: {par}"
                ))));
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
            let mut current: Vec<String> = crate::models::parse_json_array(&task.labels);
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
                return Json(ErrorOr::Error(ErrorResponse::new(e)));
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
        if epic_id != task.epic_id
            && let Err(e) = repo.move_to_epic(&task.id, epic_id.as_deref()).await
        {
            return Json(ErrorOr::Error(ErrorResponse::new(e.to_string())));
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
            Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        };

        // Apply memory_refs changes if requested.
        let updated = if p.memory_refs_add.is_some() || p.memory_refs_remove.is_some() {
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
                Ok(t) => t,
                Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
            }
        } else {
            updated
        };

        // Apply blocker changes if requested.
        if let Some(ref add) = p.blocked_by_add {
            for blocker_ref in add {
                let blocking_id = match self
                    .resolve_task_not_epic(&project_id, blocker_ref)
                    .await
                {
                    Ok(id) => id,
                    Err(e) => return Json(ErrorOr::Error(e)),
                };
                if let Err(e) = repo.add_blocker(&updated.id, &blocking_id).await {
                    return Json(ErrorOr::Error(ErrorResponse::new(e.to_string())));
                }
            }
        }
        if let Some(ref remove) = p.blocked_by_remove {
            for blocker_ref in remove {
                let blocking_id = match self
                    .resolve_task_not_epic(&project_id, blocker_ref)
                    .await
                {
                    Ok(id) => id,
                    Err(e) => return Json(ErrorOr::Error(e)),
                };
                if let Err(e) = repo.remove_blocker(&updated.id, &blocking_id).await {
                    return Json(ErrorOr::Error(ErrorResponse::new(e.to_string())));
                }
            }
        }

        Json(ErrorOr::Ok(task_to_response(&updated)))
    }

    /// Show details of a work item. Accepts task UUID or short_id.
    #[tool(
        description = "Show details of a work item including recent activity and blockers. Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_show(
        &self,
        Parameters(p): Parameters<TaskShowParams>,
    ) -> Json<ErrorOr<TaskShowResponse>> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let session_repo =
            SessionRepository::new(self.state.db().clone(), self.state.events().clone());
        let task_result = if let Some(project) = &p.project {
            let project_id = match self.require_project_id(project).await {
                Ok(id) => id,
                Err(e) => return Json(ErrorOr::Error(e)),
            };
            repo.resolve_in_project(&project_id, &p.id).await
        } else {
            repo.resolve(&p.id).await
        };
        match task_result {
            Ok(Some(t)) => {
                let session_count = session_repo.count_for_task(&t.id).await.unwrap_or(0);
                let active_session = session_repo
                    .active_for_task(&t.id)
                    .await
                    .ok()
                    .flatten()
                    .map(|s| SessionRecordResponse {
                        id: s.id,
                        project_id: s.project_id,
                        task_id: s.task_id.unwrap_or_default(),
                        model_id: s.model_id,
                        agent_type: s.agent_type,
                        started_at: s.started_at,
                        ended_at: s.ended_at,
                        status: s.status,
                        tokens_in: s.tokens_in,
                        tokens_out: s.tokens_out,
                        worktree_path: s.worktree_path,
                        goose_session_id: s.goose_session_id,
                    });
                Json(ErrorOr::Ok(TaskShowResponse {
                    task: task_to_response(&t),
                    session_count,
                    active_session,
                }))
            }
            Ok(None) => Json(ErrorOr::Error(not_found(&p.id))),
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
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
        if let Some(prio) = p.priority
            && let Err(e) = validate_priority(prio)
        {
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
                let tasks = result.tasks.iter().map(task_to_list_item).collect();
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
    ) -> Json<ErrorOr<TaskCountSuccess>> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        if let Some(ref gb) = p.group_by
            && let Err(e) = validate_sort(gb, &["status", "priority", "issue_type", "epic"])
        {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        if let Some(prio) = p.priority
            && let Err(e) = validate_priority(prio)
        {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
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
            Ok(v) => match serde_json::from_value::<TaskCountSuccess>(v) {
                Ok(parsed) => Json(ErrorOr::Ok(parsed)),
                Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
            },
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// List tasks that block the given task.
    #[tool(
        description = "List tasks that block the given task. Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_blockers_list(
        &self,
        Parameters(p): Parameters<TaskBlockersListParams>,
    ) -> Json<ErrorOr<TaskBlockersListResponse>> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(task) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(ErrorOr::Error(not_found(&p.id)));
        };
        match repo.list_blockers(&task.id).await {
            Ok(refs) => {
                let blockers = refs
                    .iter()
                    .map(|b| TaskBlockerItemResponse {
                        blocking_task_id: b.task_id.clone(),
                        blocking_task_short_id: b.short_id.clone(),
                        blocking_task_title: b.title.clone(),
                        blocking_task_status: b.status.clone(),
                        resolved: b.status == "closed",
                    })
                    .collect();
                Json(ErrorOr::Ok(TaskBlockersListResponse { blockers }))
            }
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// List tasks that are blocked by the given task.
    #[tool(
        description = "List task IDs that are blocked by the given task. Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_blocked_list(
        &self,
        Parameters(p): Parameters<TaskBlockedListParams>,
    ) -> Json<ErrorOr<TaskBlockedListResponse>> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let task_result = if let Some(project) = &p.project {
            let project_id = match self.require_project_id(project).await {
                Ok(id) => id,
                Err(e) => return Json(ErrorOr::Error(e)),
            };
            repo.resolve_in_project(&project_id, &p.id).await
        } else {
            repo.resolve(&p.id).await
        };
        let Some(task) = task_result.ok().flatten() else {
            return Json(ErrorOr::Error(not_found(&p.id)));
        };
        match repo.list_blocked_by(&task.id).await {
            Ok(refs) => {
                let tasks = refs
                    .iter()
                    .map(|b| TaskBlockedItemResponse {
                        task_id: b.task_id.clone(),
                        short_id: b.short_id.clone(),
                        title: b.title.clone(),
                        status: b.status.clone(),
                    })
                    .collect();
                Json(ErrorOr::Ok(TaskBlockedListResponse { tasks }))
            }
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// List work items ready to start (open status with no blocking dependencies).
    #[tool(
        description = "List work items ready to start (open status with no blocking dependencies)"
    )]
    pub async fn task_ready(
        &self,
        Parameters(p): Parameters<TaskReadyParams>,
    ) -> Json<ErrorOr<TaskReadyResponse>> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        if let Some(ref o) = p.owner
            && let Err(e) = validate_owner(o)
        {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        if let Some(pmax) = p.priority_max
            && let Err(e) = validate_priority(pmax)
        {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
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
            Ok(tasks) => Json(ErrorOr::Ok(TaskReadyResponse {
                tasks: tasks.iter().map(task_to_response).collect(),
            })),
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// Transition a work item through the state machine.
    #[tool(
        description = "Transition a work item to a new status (e.g., open, in_progress, review, approve, close). Accepts task ID (full UUID or short_id, e.g., 'k7m2'). For user_override action, use target_status to specify the destination column."
    )]
    pub async fn task_transition(
        &self,
        Parameters(p): Parameters<TaskTransitionParams>,
    ) -> Json<ErrorOr<TaskResponse>> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        let actor_id = p.actor_id.as_deref().unwrap_or("");
        if let Err(e) = validate_actor_id(actor_id) {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        let actor_role = p.actor_role.as_deref().unwrap_or("user");
        if let Err(e) = validate_actor_role(actor_role) {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        if let Some(ref r) = p.reason
            && let Err(e) = validate_reason(r)
        {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());

        let Some(task) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(ErrorOr::Error(not_found(&p.id)));
        };

        let action = match TransitionAction::parse(&p.action) {
            Ok(a) => a,
            Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        };

        let target_override = if let Some(ref ts) = p.target_status {
            match TaskStatus::parse(ts) {
                Ok(s) => Some(s),
                Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
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
            Ok(updated) => Json(ErrorOr::Ok(task_to_response(&updated))),
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// Claim the next available work item and transition it to in_progress.
    #[tool(
        description = "Claim the next available work item (highest priority, oldest) and transition it to in_progress"
    )]
    pub async fn task_claim(
        &self,
        Parameters(p): Parameters<TaskClaimParams>,
    ) -> Json<ErrorOr<TaskClaimSuccess>> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        if let Some(ref o) = p.owner
            && let Err(e) = validate_owner(o)
        {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
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
            Ok(Some(task)) => Json(ErrorOr::Ok(TaskClaimSuccess::Task(task_to_response(&task)))),
            Ok(None) => Json(ErrorOr::Ok(TaskClaimSuccess::NoTask { task: None })),
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// Add a comment to a work item. Creates an activity_log entry with event_type='comment'.
    #[tool(
        description = "Add a comment to a work item. Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_comment_add(
        &self,
        Parameters(p): Parameters<TaskCommentAddParams>,
    ) -> Json<ErrorOr<ActivityEntryResponse>> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        if let Err(e) = validate_body(&p.body) {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        let actor_id = p.actor_id.as_deref().unwrap_or("");
        if let Err(e) = validate_actor_id(actor_id) {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }
        let actor_role = p.actor_role.as_deref().unwrap_or("user");
        if let Err(e) = validate_actor_role(actor_role) {
            return Json(ErrorOr::Error(ErrorResponse::new(e)));
        }

        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());

        let Some(task) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(ErrorOr::Error(not_found(&p.id)));
        };

        let payload = serde_json::json!({ "body": p.body }).to_string();

        match repo
            .log_activity(Some(&task.id), actor_id, actor_role, "comment", &payload)
            .await
        {
            Ok(entry) => Json(ErrorOr::Ok(ActivityEntryResponse {
                id: entry.id,
                task_id: entry.task_id,
                actor_id: entry.actor_id,
                actor_role: entry.actor_role,
                event_type: entry.event_type,
                payload: parse_any_json(&entry.payload),
                created_at: entry.created_at,
            })),
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// Query the activity log with optional filters.
    #[tool(
        description = "List task activity log entries filtered by task_id, event_type, and/or time range."
    )]
    pub async fn task_activity_list(
        &self,
        Parameters(p): Parameters<TaskActivityListParams>,
    ) -> Json<ErrorOr<TaskActivityListResponse>> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };

        // If an id was supplied, resolve it to a full UUID.
        let task_id = if let Some(ref id) = p.id {
            match repo.resolve_in_project(&project_id, id).await {
                Ok(Some(t)) => Some(t.id),
                Ok(None) => return Json(ErrorOr::Error(not_found(id))),
                Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
            }
        } else {
            None
        };

        let q = ActivityQuery {
            project_id: Some(project_id),
            task_id,
            event_type: p.event_type,
            actor_role: p.actor_role,
            from_time: p.from_time,
            to_time: p.to_time,
            limit: validate_limit(p.limit.unwrap_or(50)),
            offset: validate_offset(p.offset.unwrap_or(0)),
        };

        match repo.query_activity(q).await {
            Ok(entries) => {
                let items: Vec<ActivityEntryResponse> = entries
                    .iter()
                    .map(|e| ActivityEntryResponse {
                        id: e.id.clone(),
                        task_id: e.task_id.clone(),
                        actor_id: e.actor_id.clone(),
                        actor_role: e.actor_role.clone(),
                        event_type: e.event_type.clone(),
                        payload: parse_any_json(&e.payload),
                        created_at: e.created_at.clone(),
                    })
                    .collect();
                Json(ErrorOr::Ok(TaskActivityListResponse {
                    count: items.len(),
                    entries: items,
                }))
            }
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    /// Board health report: epic progress, stale tasks, and review queue.
    #[tool(
        description = "Returns aggregate health report (total notes, broken links, orphan notes, stale notes by folder)."
    )]
    pub async fn board_health(
        &self,
        Parameters(p): Parameters<BoardHealthParams>,
    ) -> Json<ErrorOr<BoardHealthResponse>> {
        board::board_health_impl(self, p).await
    }

    /// Heal stale tasks and recover orphaned states.
    #[tool(
        description = "Trigger board reconciliation: heal stale tasks, recover stuck sessions, trigger overdue reviews, disable dead models, and reconcile phases. Returns action counts."
    )]
    pub async fn board_reconcile(
        &self,
        Parameters(p): Parameters<BoardReconcileParams>,
    ) -> Json<ErrorOr<BoardReconcileResponse>> {
        board::board_reconcile_impl(self, p).await
    }

    /// List memory note permalinks associated with a task.
    #[tool(
        description = "List memory note permalinks associated with a task. Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_memory_refs(
        &self,
        Parameters(p): Parameters<TaskMemoryRefsParams>,
    ) -> Json<ErrorOr<TaskMemoryRefsResponse>> {
        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        let repo = TaskRepository::new(self.state.db().clone(), self.state.events().clone());
        let Some(task) = repo
            .resolve_in_project(&project_id, &p.id)
            .await
            .ok()
            .flatten()
        else {
            return Json(ErrorOr::Error(not_found(&p.id)));
        };
        Json(ErrorOr::Ok(TaskMemoryRefsResponse {
            id: task.id,
            short_id: task.short_id,
            memory_refs: parse_string_array(&task.memory_refs),
        }))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

impl DjinnMcpServer {
    async fn require_project_id(
        &self,
        project: &str,
    ) -> std::result::Result<String, ErrorResponse> {
        self.resolve_project_id(project)
            .await
            .map_err(ErrorResponse::new)
    }

    /// Resolve a task UUID/short_id to its UUID, rejecting epics with a clear error.
    async fn resolve_task_not_epic(
        &self,
        project_id: &str,
        id: &str,
    ) -> std::result::Result<String, ErrorResponse> {
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
            return Err(ErrorResponse::new(format!(
                "epics cannot participate in blocker relationships: {id}"
            )));
        }
        Err(ErrorResponse::new(format!("task not found: {id}")))
    }
}
