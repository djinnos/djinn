// MCP tools for task board operations (CRUD, listing, queries).

use std::collections::HashMap;

use rmcp::{Json, handler::server::wrapper::Parameters, schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

use crate::server::DjinnMcpServer;
use crate::tools::AnyJson;
use crate::tools::validation::{
    validate_ac_count, validate_actor_id, validate_actor_role, validate_body, validate_description,
    validate_design, validate_issue_type, validate_label, validate_labels_count, validate_limit,
    validate_offset, validate_owner, validate_priority, validate_reason, validate_sort,
    validate_task_create_status, validate_title,
};
use djinn_core::models::SessionStatus;
use djinn_core::models::{IssueType, Task, TaskStatus, TransitionAction};
use djinn_db::EpicRepository;
use djinn_db::SessionRepository;
use djinn_db::{ActivityQuery, CountQuery, ListQuery, ReadyQuery, TaskRepository};

mod board;
pub mod ops;
mod types;

pub use self::ops::{
    CommentTaskRequest, CreateTaskRequest, TransitionTaskRequest, UpdateTaskRequest,
    add_task_comment, create_task, transition_task, update_task,
};
pub use self::types::*;

fn collapse_acceptance_criteria(
    acceptance_criteria: Option<Vec<AcceptanceCriterionItem>>,
) -> Option<Vec<String>> {
    acceptance_criteria.map(|items| {
        items
            .into_iter()
            .map(|item| match item {
                AcceptanceCriterionItem::Text(text) => text,
                AcceptanceCriterionItem::Structured(status) => status.criterion,
            })
            .collect()
    })
}

pub(crate) fn validate_create_request(request: &CreateTaskRequest) -> Result<(), ErrorResponse> {
    validate_title(&request.title).map_err(ErrorResponse::new)?;
    validate_description(&request.description).map_err(ErrorResponse::new)?;
    validate_design(&request.design).map_err(ErrorResponse::new)?;
    validate_issue_type(&request.issue_type).map_err(ErrorResponse::new)?;
    validate_priority(request.priority).map_err(ErrorResponse::new)?;
    validate_owner(&request.owner).map_err(ErrorResponse::new)?;

    if let Some(status) = request.status.as_deref() {
        validate_task_create_status(Some(status)).map_err(ErrorResponse::new)?;
    } else {
        validate_task_create_status(None).map_err(ErrorResponse::new)?;
    }

    validate_labels(&request.labels).map_err(ErrorResponse::new)?;

    if let Some(ac) = request.acceptance_criteria.as_ref() {
        validate_ac_count(ac.len()).map_err(ErrorResponse::new)?;
    }

    let uses_simple = IssueType::parse(&request.issue_type)
        .map(|issue_type| issue_type.uses_simple_lifecycle())
        .unwrap_or(false);
    if !uses_simple {
        let ac_empty = request
            .acceptance_criteria
            .as_ref()
            .map(|ac| ac.is_empty())
            .unwrap_or(true);
        if ac_empty {
            return Err(ErrorResponse::new(
                "acceptance_criteria is required for task/feature/bug issue types — \
                 provide an array of strings, e.g. [\"criterion 1\", \"criterion 2\"]",
            ));
        }
    }

    Ok(())
}

fn validate_update_request(request: &UpdateTaskRequest) -> Result<(), ErrorResponse> {
    if let Some(title) = request.title.as_deref() {
        validate_title(title).map_err(ErrorResponse::new)?;
    }
    if let Some(description) = request.description.as_deref() {
        validate_description(description).map_err(ErrorResponse::new)?;
    }
    if let Some(design) = request.design.as_deref() {
        validate_design(design).map_err(ErrorResponse::new)?;
    }
    if let Some(priority) = request.priority {
        validate_priority(priority).map_err(ErrorResponse::new)?;
    }
    if let Some(owner) = request.owner.as_deref() {
        validate_owner(owner).map_err(ErrorResponse::new)?;
    }
    validate_labels(&request.labels_add).map_err(ErrorResponse::new)?;
    if let Some(ac) = request.acceptance_criteria.as_ref() {
        validate_ac_count(ac.len()).map_err(ErrorResponse::new)?;
    }
    Ok(())
}

fn validate_transition_request(request: &TransitionTaskRequest) -> Result<(), ErrorResponse> {
    validate_actor_id(&request.actor_id).map_err(ErrorResponse::new)?;
    validate_actor_role(&request.actor_role).map_err(ErrorResponse::new)?;
    if let Some(reason) = request.reason.as_deref() {
        validate_reason(reason).map_err(ErrorResponse::new)?;
    }
    Ok(())
}

fn validate_comment_request(request: &CommentTaskRequest) -> Result<(), ErrorResponse> {
    validate_body(&request.body).map_err(ErrorResponse::new)?;
    validate_actor_id(&request.actor_id).map_err(ErrorResponse::new)?;
    validate_actor_role(&request.actor_role).map_err(ErrorResponse::new)?;
    Ok(())
}

// ── Tool implementations ─────────────────────────────────────────────────────

#[tool_router(router = task_tool_router, vis = "pub")]
impl DjinnMcpServer {
    /// Create a new work item under an epic.
    #[tool(
        description = "Create a new work item (task, feature, bug, spike, research, planning, or review) under an epic. Accepts epic_id as UUID or short_id. Use blocked_by to set blocker dependencies atomically at creation. Spike/research/planning/review use a simple lifecycle (open → in_progress → closed). acceptance_criteria is required for task/feature/bug types."
    )]
    pub async fn task_create(
        &self,
        Parameters(p): Parameters<TaskCreateParams>,
    ) -> Json<ErrorOr<TaskResponse>> {
        let description = p.description.unwrap_or_default();
        let design = p.design.unwrap_or_default();
        let issue_type = p.issue_type.unwrap_or_else(|| "task".to_owned());
        let status = p.status;
        let request = CreateTaskRequest {
            title: p.title,
            description,
            design,
            issue_type,
            priority: p.priority.unwrap_or(0),
            owner: p.owner.unwrap_or_default(),
            status,
            acceptance_criteria: collapse_acceptance_criteria(p.acceptance_criteria),
            labels: p.labels.unwrap_or_default(),
            memory_refs: p.memory_refs.unwrap_or_default(),
            blocked_by_refs: p.blocked_by.unwrap_or_default(),
            agent_type: p.agent_type,
            epic_ref: p.epic_id,
        };

        if let Err(e) = validate_create_request(&request) {
            return Json(ErrorOr::Error(e));
        }

        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };

        create_task(self, &project_id, request).await
    }

    /// Update allowed fields of a work item.
    #[tool(
        description = "Update allowed fields of a work item (title, description, acceptance_criteria, design, priority, owner, labels, epic_id, blocked_by_add, blocked_by_remove). Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_update(
        &self,
        Parameters(p): Parameters<TaskUpdateParams>,
    ) -> Json<ErrorOr<TaskResponse>> {
        let request = UpdateTaskRequest {
            id: p.id,
            title: p.title,
            description: p.description,
            design: p.design,
            priority: p.priority,
            owner: p.owner,
            acceptance_criteria: collapse_acceptance_criteria(p.acceptance_criteria),
            labels_add: p.labels_add.unwrap_or_default(),
            labels_remove: p.labels_remove.unwrap_or_default().into_iter().collect(),
            memory_refs_add: p.memory_refs_add.unwrap_or_default(),
            memory_refs_remove: p
                .memory_refs_remove
                .unwrap_or_default()
                .into_iter()
                .collect(),
            blocked_by_add_refs: p.blocked_by_add.unwrap_or_default(),
            blocked_by_remove_refs: p.blocked_by_remove.unwrap_or_default(),
            agent_type: p.agent_type,
            epic_ref: p.epic_id,
        };

        if let Err(e) = validate_update_request(&request) {
            return Json(ErrorOr::Error(e));
        }

        let project_id = match self.require_project_id(&p.project).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };

        update_task(self, &project_id, request).await
    }

    /// Show details of a work item. Accepts task UUID or short_id.
    #[tool(
        description = "Show details of a work item including recent activity and blockers. Accepts task ID (full UUID or short_id, e.g., 'k7m2')."
    )]
    pub async fn task_show(
        &self,
        Parameters(p): Parameters<TaskShowParams>,
    ) -> Json<ErrorOr<TaskShowResponse>> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        let session_repo = SessionRepository::new(self.state.db().clone(), self.state.event_bus());
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
                let task_run_repo = djinn_db::repositories::task_run::TaskRunRepository::new(
                    self.state.db().clone(),
                );
                let active_session_record =
                    session_repo.active_for_task(&t.id).await.ok().flatten();
                let active_session = match active_session_record {
                    Some(s) => {
                        let workspace_path = match s.task_run_id.as_deref() {
                            Some(run_id) => task_run_repo
                                .get(run_id)
                                .await
                                .ok()
                                .flatten()
                                .and_then(|run| run.workspace_path),
                            None => None,
                        };
                        Some(SessionRecordResponse {
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
                            workspace_path,
                        })
                    }
                    None => None,
                };
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
            project_id: Some(project_id.clone()),
            status: p.status,
            issue_type: p.issue_type,
            priority: p.priority.filter(|&v| v != 0),
            label: p.label,
            text: p.text,
            parent: None,
            sort: sort.to_owned(),
            limit,
            offset,
        };

        let repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        let session_repo = SessionRepository::new(self.state.db().clone(), self.state.event_bus());
        match repo.list_filtered(query).await {
            Ok(result) => {
                // Batch-fetch active sessions and session counts for the project
                let active_sessions = session_repo
                    .list_active_in_project(&project_id)
                    .await
                    .unwrap_or_default();
                let mut session_by_task: HashMap<String, _> = HashMap::new();
                for s in active_sessions {
                    if let Some(tid) = &s.task_id {
                        session_by_task.entry(tid.clone()).or_insert(s);
                    }
                }

                let task_ids: Vec<&str> = result.tasks.iter().map(|t| t.id.as_str()).collect();
                let session_counts = session_repo
                    .count_for_tasks(&task_ids)
                    .await
                    .unwrap_or_default();

                let tasks = result
                    .tasks
                    .iter()
                    .map(|t| {
                        let active = session_by_task.remove(&t.id).map(|s| ActiveSessionSummary {
                            session_id: s.id,
                            agent_type: s.agent_type,
                            model_id: s.model_id,
                            started_at: s.started_at,
                            status: s.status,
                        });
                        let count = session_counts.get(&t.id).copied().unwrap_or(0);
                        task_to_list_item(t, active, count)
                    })
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
            priority: p.priority.filter(|&v| v != 0),
            label: p.label,
            text: p.text,
            parent: None,
            group_by: p.group_by,
        };

        let repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
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
        let repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
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
        let repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
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

        let repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
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

        transition_task(
            self,
            &project_id,
            TransitionTaskRequest {
                id: p.id,
                action,
                actor_id: actor_id.to_owned(),
                actor_role: actor_role.to_owned(),
                reason: p.reason,
                target_override,
            },
        )
        .await
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

        let repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
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

        add_task_comment(
            self,
            &project_id,
            CommentTaskRequest {
                id: p.id,
                body: p.body,
                actor_id: actor_id.to_owned(),
                actor_role: actor_role.to_owned(),
            },
        )
        .await
    }

    /// Query the activity log with optional filters.
    #[tool(
        description = "List task activity log entries filtered by task_id, event_type, and/or time range."
    )]
    pub async fn task_activity_list(
        &self,
        Parameters(p): Parameters<TaskActivityListParams>,
    ) -> Json<ErrorOr<TaskActivityListResponse>> {
        let repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
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
        description = "Returns board health plus planner-facing memory-health summary (epic progress, stale tasks, review queue, duplicate clusters, low-confidence notes, stale notes, broken links, and orphans)."
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
        let repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
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
    /// Resolve an absolute project path to the canonical project UUID required by the public
    /// task mutation seam. External crates should call
    /// `djinn_mcp::tools::task_tools::{create_task, update_task, transition_task, add_task_comment}`
    /// with this project ID rather than re-implementing MCP/project lookup locally.
    pub async fn require_project_id_public(
        &self,
        project: &str,
    ) -> std::result::Result<String, ErrorResponse> {
        self.require_project_id(project).await
    }

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
        let repo = TaskRepository::new(self.state.db().clone(), self.state.event_bus());
        if let Ok(Some(task)) = repo.resolve_in_project(project_id, id).await {
            return Ok(task.id);
        }
        // Not found in tasks — check if it's an epic to give a clearer error.
        if let Ok(Some(_)) = EpicRepository::new(self.state.db().clone(), self.state.event_bus())
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
