//! Public task-mutation operation seam shared by MCP handlers and external adapters.
//!
//! External callers should construct a [`DjinnMcpServer`] from an existing [`crate::McpState`],
//! resolve the project once with [`DjinnMcpServer::require_project_id_public`], then call the
//! mutation helpers re-exported from [`crate::tools::task_tools`]:
//!
//! ```ignore
//! use djinn_control_plane::server::DjinnMcpServer;
//! use djinn_control_plane::tools::task_tools::{
//!     CommentTaskRequest, CreateTaskRequest, TransitionTaskRequest, UpdateTaskRequest,
//!     add_task_comment, create_task, transition_task, update_task,
//! };
//!
//! let server = DjinnMcpServer::new(state.clone());
//! let project_id = server.require_project_id_public(project_path).await?;
//! let response = create_task(&server, &project_id, CreateTaskRequest { /* ... */ }).await;
//! ```
//!
//! Contract: callers must supply a server backed by real MCP state so repository/event-bus access,
//! project resolution, and blocker validation behave exactly like the MCP tool wrappers.

use rmcp::Json;

use std::collections::HashSet;

use crate::server::DjinnMcpServer;
use crate::tools::task_tools::types::{
    ActivityEntryResponse, ErrorOr, ErrorResponse, TaskResponse,
};
use djinn_core::models::{ActivityEntry, Task, TaskStatus, TransitionAction};
use djinn_db::TaskRepository;

pub(crate) fn task_to_response(task: &Task) -> TaskResponse {
    TaskResponse {
        id: task.id.clone(),
        short_id: task.short_id.clone(),
        epic_id: task.epic_id.clone(),
        title: task.title.clone(),
        description: task.description.clone(),
        design: task.design.clone(),
        status: task.status.clone(),
        issue_type: task.issue_type.clone(),
        priority: task.priority,
        owner: task.owner.clone(),
        acceptance_criteria: task
            .acceptance_criteria
            .trim()
            .is_empty()
            .then(Vec::new)
            .unwrap_or_else(|| {
                serde_json::from_str(&task.acceptance_criteria).unwrap_or_else(|_| Vec::new())
            }),
        labels: parse_string_array(&task.labels),
        memory_refs: parse_string_array(&task.memory_refs),
        reopen_count: task.reopen_count,
        continuation_count: task.continuation_count,
        total_reopen_count: task.total_reopen_count,
        intervention_count: task.intervention_count,
        last_intervention_at: task.last_intervention_at.clone(),
        agent_type: task.agent_type.clone(),
        created_by_user_id: task.created_by_user_id.clone(),
        created_at: task.created_at.clone(),
        updated_at: task.updated_at.clone(),
        closed_at: task.closed_at.clone(),
        close_reason: task.close_reason.clone(),
        merge_commit_sha: task.merge_commit_sha.clone(),
        merge_conflict_metadata: task
            .merge_conflict_metadata
            .as_deref()
            .and_then(|value| serde_json::from_str(value).ok())
            .map(crate::tools::AnyJson),
        pr_url: task.pr_url.clone(),
        warning: None,
    }
}

pub(crate) fn activity_entry_response(entry: ActivityEntry) -> ActivityEntryResponse {
    let payload_value: serde_json::Value =
        serde_json::from_str(&entry.payload).unwrap_or_else(|_| serde_json::json!({}));
    let (kind, details, summary) = render_activity_metadata(&entry.event_type, &payload_value);

    ActivityEntryResponse {
        id: entry.id,
        task_id: entry.task_id,
        actor_id: entry.actor_id,
        actor_role: entry.actor_role,
        event_type: entry.event_type,
        kind,
        payload: crate::tools::AnyJson(payload_value),
        details,
        summary,
        created_at: entry.created_at,
    }
}

pub(crate) fn render_activity_metadata(
    event_type: &str,
    payload: &serde_json::Value,
) -> (String, Option<crate::tools::AnyJson>, Option<String>) {
    let kind = if event_type == "loop_guard_tripped" {
        "loop_guard_tripped".to_owned()
    } else {
        payload
            .get("kind")
            .and_then(|value| value.as_str())
            .unwrap_or(event_type)
            .to_owned()
    };
    let details = payload.get("details").cloned().map(crate::tools::AnyJson);
    let summary = if kind == "loop_guard_tripped" {
        Some(loop_guard_activity_summary(payload))
    } else {
        payload
            .get("body")
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned)
    };

    (kind, details, summary)
}

pub(crate) fn loop_guard_activity_summary(payload: &serde_json::Value) -> String {
    let details = payload.get("details").unwrap_or(payload);
    let guard_kind = details
        .get("kind")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown_guard");
    let signature = details
        .get("offending_signature")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown_signature");
    let turn_span = details.get("turn_span");
    let start = turn_span
        .and_then(|value| value.get("start"))
        .and_then(|value| value.as_u64())
        .or_else(|| {
            turn_span
                .and_then(|value| value.get(0))
                .and_then(|value| value.as_u64())
        });
    let end = turn_span
        .and_then(|value| value.get("end"))
        .and_then(|value| value.as_u64())
        .or_else(|| {
            turn_span
                .and_then(|value| value.get(1))
                .and_then(|value| value.as_u64())
        });

    match (start, end) {
        (Some(start), Some(end)) => {
            format!("Loop guard `{guard_kind}` tripped on turns {start}..={end}: `{signature}`")
        }
        _ => format!("Loop guard `{guard_kind}` tripped: `{signature}`"),
    }
}

pub(crate) fn parse_string_array(value: &str) -> Vec<String> {
    serde_json::from_str(value).unwrap_or_default()
}

pub(crate) fn not_found(id: &str) -> ErrorResponse {
    ErrorResponse::new(format!("task not found: {id}"))
}

pub async fn create_task(
    server: &DjinnMcpServer,
    project_id: &str,
    request: CreateTaskRequest,
) -> Json<ErrorOr<TaskResponse>> {
    if let Err(e) = super::validate_create_request(&request) {
        return Json(ErrorOr::Error(e));
    }

    let epic_id = if let Some(epic_ref) = request.epic_ref.as_deref() {
        let epic_repo =
            djinn_db::EpicRepository::new(server.state.db().clone(), server.state.event_bus());
        let Some(epic) = epic_repo
            .resolve_in_project(project_id, epic_ref)
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

    let repo = TaskRepository::new(server.state.db().clone(), server.state.event_bus());

    let mut resolved_blocker_ids = Vec::with_capacity(request.blocked_by_refs.len());
    for blocker_ref in &request.blocked_by_refs {
        let blocking_id = match server.resolve_task_not_epic(project_id, blocker_ref).await {
            Ok(id) => id,
            Err(e) => return Json(ErrorOr::Error(e)),
        };
        resolved_blocker_ids.push(blocking_id);
    }

    let ac_json = request
        .acceptance_criteria
        .filter(|criteria| !criteria.is_empty())
        .map(|criteria| serde_json::to_string(&criteria).unwrap_or_else(|_| "[]".into()));

    // Create the task AND its blocker edges atomically (one transaction) so the
    // coordinator's ready-poll never claims it as a dispatchable `open` task
    // before its blockers are committed — the planner↔dispatch race.
    let mut task = match repo
        .create_in_project_with_blockers(
            project_id,
            epic_id.as_deref(),
            &request.title,
            &request.description,
            &request.design,
            &request.issue_type,
            request.priority,
            &request.owner,
            request.status.as_deref(),
            ac_json.as_deref(),
            &resolved_blocker_ids,
        )
        .await
    {
        Ok(task) => task,
        Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
    };

    if !request.labels.is_empty() {
        let labels_json = serde_json::to_string(&request.labels).unwrap_or_else(|_| "[]".into());
        match repo
            .update(
                &task.id,
                &task.title,
                &task.description,
                &task.design,
                task.priority,
                &task.owner,
                &labels_json,
                &task.acceptance_criteria,
            )
            .await
        {
            Ok(updated) => task = updated,
            Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    if !request.memory_refs.is_empty() {
        let refs_json = serde_json::to_string(&request.memory_refs).unwrap_or_else(|_| "[]".into());
        match repo.update_memory_refs(&task.id, &refs_json).await {
            Ok(updated) => task = updated,
            Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    // (Blockers were wired atomically inside `create_in_project_with_blockers`
    // above — see the planner↔dispatch race note there.)

    if let Some(agent_type) = request.agent_type.as_deref() {
        let at = (!agent_type.is_empty()).then_some(agent_type);
        match repo.update_agent_type(&task.id, at).await {
            Ok(updated) => task = updated,
            Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    Json(ErrorOr::Ok(task_to_response(&task)))
}

pub async fn update_task(
    server: &DjinnMcpServer,
    project_id: &str,
    request: UpdateTaskRequest,
) -> Json<ErrorOr<TaskResponse>> {
    let repo = TaskRepository::new(server.state.db().clone(), server.state.event_bus());
    let Some(task) = repo
        .resolve_in_project(project_id, &request.id)
        .await
        .ok()
        .flatten()
    else {
        return Json(ErrorOr::Error(not_found(&request.id)));
    };

    let epic_id: Option<String> = if let Some(par) = request.epic_ref.as_deref() {
        let epic_repo =
            djinn_db::EpicRepository::new(server.state.db().clone(), server.state.event_bus());
        let Some(epic) = epic_repo
            .resolve_in_project(project_id, par)
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

    let title = request.title.as_deref().unwrap_or(&task.title);
    let description = request.description.as_deref().unwrap_or(&task.description);
    let design = request.design.as_deref().unwrap_or(&task.design);
    let priority = request.priority.unwrap_or(task.priority);
    let owner = request.owner.as_deref().unwrap_or(&task.owner);

    let labels_json = if request.labels_add.is_empty() && request.labels_remove.is_empty() {
        task.labels.clone()
    } else {
        let mut current: Vec<String> = parse_string_array(&task.labels);
        for label in &request.labels_add {
            if !current.contains(label) {
                current.push(label.clone());
            }
        }
        current.retain(|label| !request.labels_remove.contains(label));
        serde_json::to_string(&current).unwrap_or_else(|_| "[]".into())
    };

    let ac_json = request
        .acceptance_criteria
        .map(|criteria| serde_json::to_string(&criteria).unwrap_or_else(|_| "[]".into()))
        .unwrap_or_else(|| task.acceptance_criteria.clone());

    if epic_id != task.epic_id
        && let Err(e) = repo.move_to_epic(&task.id, epic_id.as_deref()).await
    {
        return Json(ErrorOr::Error(ErrorResponse::new(e.to_string())));
    }

    let mut updated = match repo
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
        Ok(updated) => updated,
        Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
    };

    if !request.memory_refs_add.is_empty() || !request.memory_refs_remove.is_empty() {
        let mut refs: Vec<String> = parse_string_array(&updated.memory_refs);
        for memory_ref in &request.memory_refs_add {
            if !refs.contains(memory_ref) {
                refs.push(memory_ref.clone());
            }
        }
        refs.retain(|memory_ref| !request.memory_refs_remove.contains(memory_ref));
        let refs_json = serde_json::to_string(&refs).unwrap_or_else(|_| "[]".into());
        match repo.update_memory_refs(&updated.id, &refs_json).await {
            Ok(task) => updated = task,
            Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    if !request.blocked_by_add_refs.is_empty() || !request.blocked_by_remove_refs.is_empty() {
        let mut add_ids = Vec::with_capacity(request.blocked_by_add_refs.len());
        for blocker_ref in &request.blocked_by_add_refs {
            let blocking_id = match server.resolve_task_not_epic(project_id, blocker_ref).await {
                Ok(id) => id,
                Err(e) => return Json(ErrorOr::Error(e)),
            };
            add_ids.push(blocking_id);
        }

        let mut remove_ids = Vec::with_capacity(request.blocked_by_remove_refs.len());
        for blocker_ref in &request.blocked_by_remove_refs {
            let blocking_id = match server.resolve_task_not_epic(project_id, blocker_ref).await {
                Ok(id) => id,
                Err(e) => return Json(ErrorOr::Error(e)),
            };
            remove_ids.push(blocking_id);
        }

        if let Err(e) = repo
            .update_blockers_atomic(&updated.id, &add_ids, &remove_ids)
            .await
        {
            return Json(ErrorOr::Error(ErrorResponse::new(e.to_string())));
        }
    }

    if let Some(agent_type) = request.agent_type.as_deref() {
        let at = (!agent_type.is_empty()).then_some(agent_type);
        match repo.update_agent_type(&updated.id, at).await {
            Ok(task) => updated = task,
            Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        }
    }

    Json(ErrorOr::Ok(task_to_response(&updated)))
}

pub async fn transition_task(
    server: &DjinnMcpServer,
    project_id: &str,
    request: TransitionTaskRequest,
) -> Json<ErrorOr<TaskResponse>> {
    if let Err(error) = super::validate_transition_request(&request) {
        return Json(ErrorOr::Error(error));
    }

    let repo = TaskRepository::new(server.state.db().clone(), server.state.event_bus());

    let Some(task) = repo
        .resolve_in_project(project_id, &request.id)
        .await
        .ok()
        .flatten()
    else {
        return Json(ErrorOr::Error(not_found(&request.id)));
    };

    match repo
        .transition(
            &task.id,
            request.action,
            &request.actor_id,
            &request.actor_role,
            request.reason.as_deref(),
            request.target_override,
        )
        .await
    {
        Ok(updated) => Json(ErrorOr::Ok(task_to_response(&updated))),
        Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
    }
}

pub async fn add_task_comment(
    server: &DjinnMcpServer,
    project_id: &str,
    request: CommentTaskRequest,
) -> Json<ErrorOr<ActivityEntryResponse>> {
    if let Err(error) = super::validate_comment_request(&request) {
        return Json(ErrorOr::Error(error));
    }

    let repo = TaskRepository::new(server.state.db().clone(), server.state.event_bus());

    let Some(task) = repo
        .resolve_in_project(project_id, &request.id)
        .await
        .ok()
        .flatten()
    else {
        return Json(ErrorOr::Error(not_found(&request.id)));
    };

    let payload = serde_json::json!({ "body": request.body }).to_string();
    match repo
        .log_activity(
            Some(&task.id),
            &request.actor_id,
            &request.actor_role,
            "comment",
            &payload,
        )
        .await
    {
        Ok(entry) => Json(ErrorOr::Ok(activity_entry_response(entry))),
        Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
    }
}

#[derive(Debug, Clone)]
pub struct CreateTaskRequest {
    pub title: String,
    pub description: String,
    pub design: String,
    pub issue_type: String,
    pub priority: i64,
    pub owner: String,
    pub status: Option<String>,
    pub acceptance_criteria: Option<Vec<String>>,
    pub labels: Vec<String>,
    pub memory_refs: Vec<String>,
    pub blocked_by_refs: Vec<String>,
    pub agent_type: Option<String>,
    pub epic_ref: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UpdateTaskRequest {
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub design: Option<String>,
    pub priority: Option<i64>,
    pub owner: Option<String>,
    pub acceptance_criteria: Option<Vec<String>>,
    pub labels_add: Vec<String>,
    pub labels_remove: HashSet<String>,
    pub memory_refs_add: Vec<String>,
    pub memory_refs_remove: HashSet<String>,
    pub blocked_by_add_refs: Vec<String>,
    pub blocked_by_remove_refs: Vec<String>,
    pub agent_type: Option<String>,
    pub epic_ref: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TransitionTaskRequest {
    pub id: String,
    pub action: TransitionAction,
    pub actor_id: String,
    pub actor_role: String,
    pub reason: Option<String>,
    pub target_override: Option<TaskStatus>,
}

#[derive(Debug, Clone)]
pub struct CommentTaskRequest {
    pub id: String,
    pub body: String,
    pub actor_id: String,
    pub actor_role: String,
}
