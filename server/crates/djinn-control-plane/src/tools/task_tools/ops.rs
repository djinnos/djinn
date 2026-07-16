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
    ActivityEntryResponse, ErrorOr, ErrorResponse, TaskResponse, task_ci_gate_snapshot,
    task_ci_gate_state, task_ci_status,
};
use djinn_core::models::{ActivityEntry, Task, TaskStatus, TransitionAction};
use djinn_db::{EffectiveCreatorProvenance, EpicRepository, TaskRepository};

const IMPACT_CHECK_WARNING: &str = "This task appears to involve a removal or rename. Consider calling `impact_check` before proceeding to avoid breaking compile-time consumers in other crates. See the planner prompt contract for details.";
const DESTRUCTIVE_TASK_KEYWORDS: [&str; 8] = [
    "remove",
    "delete",
    "drop",
    "rename",
    "relocate",
    "move",
    "extract",
    "split out",
];

pub(crate) fn task_to_response(task: &Task) -> TaskResponse {
    let ci = task_ci_gate_snapshot(task);
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
        ci_status: task_ci_status(task),
        ci_gate_state: task_ci_gate_state(task),
        ci_primary_blocking_check: ci.as_ref().and_then(|ci| ci.primary_blocking_check.clone()),
        ci_summary_reason: ci.as_ref().map(|ci| ci.summary_reason.clone()),
        ci_merge_blocked_reason: ci.as_ref().and_then(|ci| ci.merge_blocked_reason.clone()),
        ci,
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

fn impact_check_warning_for_create(request: &CreateTaskRequest) -> Option<String> {
    let title_and_description =
        format!("{}\n{}", request.title, request.description).to_lowercase();
    let has_destructive_keyword = DESTRUCTIVE_TASK_KEYWORDS
        .iter()
        .any(|keyword| title_and_description.contains(keyword));

    if !has_destructive_keyword {
        return None;
    }

    let description_references_impact_check =
        request.description.to_lowercase().contains("impact_check");
    let memory_refs_reference_impact_check = request
        .memory_refs
        .iter()
        .any(|memory_ref| memory_ref.to_lowercase().contains("impact_check"));

    if description_references_impact_check || memory_refs_reference_impact_check {
        None
    } else {
        Some(IMPACT_CHECK_WARNING.to_owned())
    }
}

pub async fn create_task(
    server: &DjinnMcpServer,
    project_id: &str,
    request: CreateTaskRequest,
) -> Json<ErrorOr<TaskResponse>> {
    if let Err(e) = super::validate_create_request(&request) {
        return Json(ErrorOr::Error(e));
    }

    let warning = impact_check_warning_for_create(&request);

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

    // Safety net: if this task is being created under an epic that is blocked
    // by other open epics, atomically chain the new task to the blocking epics'
    // open tasks. This prevents a stray child task under a blocked epic from
    // dispatching before its foundation work is done.
    if let Some(ref eid) = epic_id {
        let epic_repo = EpicRepository::new(server.state.db().clone(), server.state.event_bus());
        if let Ok(blocking_epics) = epic_repo.list_blockers(eid).await {
            for blocking_epic in blocking_epics {
                if blocking_epic.status == "closed" || blocking_epic.epic_id == *eid {
                    continue;
                }
                if let Ok(blocking_tasks) = repo.list_by_epic(&blocking_epic.epic_id).await {
                    for blocking_task in blocking_tasks {
                        if blocking_task.status != "closed"
                            && !resolved_blocker_ids.contains(&blocking_task.id)
                        {
                            resolved_blocker_ids.push(blocking_task.id);
                        }
                    }
                }
            }
        }
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
            // Authenticated MCP task creation: the current session user is the
            // explicit creator. If no session is present, resolution falls back
            // to parent-epic / proposal provenance inside the insert transaction.
            EffectiveCreatorProvenance {
                explicit_user_id: djinn_core::auth_context::current_user_id().as_deref(),
                source_task_id: None,
                proposal_id: None,
            },
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

    let mut response = task_to_response(&task);
    response.warning = warning;
    Json(ErrorOr::Ok(response))
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

    // Preserve the action for the post-transition rework-reopen chokepoint
    // (`repo.transition` consumes it by value).
    let action = request.action.clone();

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
        Ok(updated) => {
            // Supervisor-driven rework reopens applied through this tool
            // (task_review_reject* / lead_approve_conflict) must terminalize the
            // worker's in-flight `submitted` attempt to `reopened` and record a
            // durable rework marker — otherwise the transition leaves an
            // orphaned `submitted` attempt that wedges the respawn guard's
            // step-2 dedup forever (the ylme bug, previously fixed for the
            // in-process/RPC path in `DirectServices::transition_task`). The PR
            // poller's `apply_pr_transition` already owns the
            // PrCiFailed/PrChangesRequested/PrConflict reopens; the coordinator
            // chokepoint (reached here via the bridge, since control-plane
            // cannot depend on djinn-coordinator) is a no-op for every non-rework
            // action. Best-effort.
            if let Some(coordinator) = server.state.coordinator().await {
                coordinator
                    .record_supervisor_rework_reopen(&task.id, &action, request.reason.as_deref())
                    .await;
            }
            Json(ErrorOr::Ok(task_to_response(&updated)))
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::state::stubs::test_mcp_state;
    use djinn_core::events::EventBus;
    use djinn_db::{
        Database, EpicCreateInput, EpicRepository, ProjectRepository, ReadyQuery, TaskRepository,
    };

    fn epic_input<'a>(title: &'a str) -> EpicCreateInput<'a> {
        EpicCreateInput {
            title,
            description: "test epic",
            emoji: "🧪",
            color: "blue",
            owner: "worker",
            memory_refs: None,
            status: None,
            auto_breakdown: None,
            originating_adr_id: None,
            blocked_by: None,
        }
    }

    fn planning_task_request(title: &str, epic_ref: &str) -> CreateTaskRequest {
        CreateTaskRequest {
            title: title.to_owned(),
            description: "test task".to_owned(),
            design: "test design".to_owned(),
            issue_type: "planning".to_owned(),
            priority: 0,
            owner: "worker".to_owned(),
            status: None,
            acceptance_criteria: None,
            labels: Vec::new(),
            memory_refs: Vec::new(),
            blocked_by_refs: Vec::new(),
            agent_type: None,
            epic_ref: Some(epic_ref.to_owned()),
        }
    }

    #[test]
    fn create_task_warns_for_destructive_keywords_without_impact_check() {
        let mut request = planning_task_request("Remove legacy API", "epic");
        request.description = "Delete the old public helper and move callers.".to_owned();

        assert_eq!(
            impact_check_warning_for_create(&request).as_deref(),
            Some(IMPACT_CHECK_WARNING)
        );
    }

    #[test]
    fn create_task_suppresses_destructive_warning_with_impact_check_reference() {
        let mut request = planning_task_request("Rename public API", "epic");
        request.description = "Covered by prior `impact_check` for downstream crates.".to_owned();

        assert_eq!(impact_check_warning_for_create(&request), None);

        let mut request = planning_task_request("Relocate public API", "epic");
        request.memory_refs = vec!["research/impact_check-public-api-relocation".to_owned()];

        assert_eq!(impact_check_warning_for_create(&request), None);
    }

    #[test]
    fn create_task_does_not_warn_without_destructive_keywords() {
        let mut request = planning_task_request("Add status endpoint", "epic");
        request.description = "Implement a new read-only endpoint.".to_owned();

        assert_eq!(impact_check_warning_for_create(&request), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_under_blocked_epic_inherits_open_tasks_from_blocking_epic() {
        let db = Database::open_in_memory().unwrap();
        let state = test_mcp_state(db.clone());
        let server = DjinnMcpServer::new(state);
        let project = ProjectRepository::new(db.clone(), EventBus::noop())
            .create(
                "epic-blocker-propagation",
                "test",
                "epic-blocker-propagation",
            )
            .await
            .unwrap();
        let epic_repo = EpicRepository::new(db.clone(), EventBus::noop());
        let task_repo = TaskRepository::new(db.clone(), EventBus::noop());

        let foundation_epic = epic_repo
            .create_for_project(&project.id, epic_input("Foundation"))
            .await
            .unwrap();
        let blocked_epic = epic_repo
            .create_for_project(&project.id, epic_input("Dependent"))
            .await
            .unwrap();

        let open_foundation_task = task_repo
            .create_in_project(
                &project.id,
                Some(&foundation_epic.id),
                "open foundation task",
                "foundation",
                "foundation design",
                "planning",
                0,
                "worker",
                None,
                None,
            )
            .await
            .unwrap();
        let closed_foundation_task = task_repo
            .create_in_project(
                &project.id,
                Some(&foundation_epic.id),
                "closed foundation task",
                "foundation",
                "foundation design",
                "planning",
                0,
                "worker",
                Some("closed"),
                None,
            )
            .await
            .unwrap();

        epic_repo
            .add_blocker(&blocked_epic.id, &foundation_epic.id)
            .await
            .unwrap();

        let Json(created) = create_task(
            &server,
            &project.id,
            planning_task_request("dependent task", &blocked_epic.short_id),
        )
        .await;
        let dependent_task = match created {
            ErrorOr::Ok(task) => task,
            ErrorOr::Error(err) => panic!("task_create failed: {}", err.error),
        };

        let blockers = task_repo.list_blockers(&dependent_task.id).await.unwrap();
        assert_eq!(
            blockers
                .iter()
                .map(|b| b.task_id.as_str())
                .collect::<Vec<_>>(),
            vec![open_foundation_task.id.as_str()],
            "only open tasks from blocking epics should be propagated"
        );
        assert!(
            !blockers
                .iter()
                .any(|b| b.task_id == closed_foundation_task.id),
            "closed tasks in the blocking epic must not be propagated"
        );

        let ready = task_repo
            .list_ready(ReadyQuery {
                project_id: Some(project.id.clone()),
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            !ready.iter().any(|task| task.id == dependent_task.id),
            "dependent task must not be dispatchable while inherited blocker is open"
        );

        task_repo
            .set_status(&open_foundation_task.id, "closed")
            .await
            .unwrap();
        let ready = task_repo
            .list_ready(ReadyQuery {
                project_id: Some(project.id),
                limit: 50,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(
            ready.iter().any(|task| task.id == dependent_task.id),
            "dependent task should become dispatchable once inherited blocker closes"
        );
    }

    // ---- additive event compatibility regressions (4hx2 AC10) ---------
    //
    // Proves the control-plane activity rendering path handles unknown/
    // additive event types (such as `task_run_pretask_ran`) safely: it
    // does not crash, reject, or produce unbounded output.  The rendering
    // falls back gracefully to using the event_type as the `kind`, with
    // `details` = None and `summary` = None when the payload lacks the
    // expected top-level keys.

    /// `render_activity_metadata` handles the additive
    /// `task_run_pretask_ran` event type without crashing.
    ///
    /// The `kind` defaults to the raw event_type when the payload has no
    /// `kind` field.  `details` and `summary` are absent because the
    /// pretask payload uses a flat field set (not the `{kind, details,
    /// body}` shape the generic renderer expects).
    #[test]
    fn render_activity_metadata_handles_additive_pretask_event_safely() {
        let payload = serde_json::json!({
            "name": "prepare-test-db",
            "index": 0,
            "command": "sh prepare-test-db.sh",
            "failure_policy": "blocking",
            "started_at": "2026-07-08T12:00:00.000Z",
            "duration_ms": 1500,
            "exit_code": 0,
            "timed_out": false,
            "cancelled": false,
            "blocked": false,
            "output_tail": "",
            "output_truncated": false
        });

        let (kind, details, summary) = render_activity_metadata("task_run_pretask_ran", &payload);

        // kind falls back to the event_type string.
        assert_eq!(kind, "task_run_pretask_ran");
        // details is None (payload has no "details" key).
        assert!(details.is_none(), "pretask payload has no details key");
        // summary is None (payload has no "body" key).
        assert!(summary.is_none(), "pretask payload has no body key");
    }

    /// A blocked/failed `task_run_pretask_ran` payload (with
    /// `failure_class: "environmental"`) also renders safely.
    #[test]
    fn render_activity_metadata_handles_blocked_pretask_event_safely() {
        let payload = serde_json::json!({
            "name": "prepare-test-db",
            "index": 0,
            "command": "sh prepare-test-db.sh",
            "failure_policy": "blocking",
            "started_at": "2026-07-08T12:00:00.000Z",
            "duration_ms": 200,
            "exit_code": 1,
            "timed_out": false,
            "cancelled": false,
            "blocked": true,
            "failure_class": "environmental",
            "output_tail": "FATAL: relation does not exist\n[REDACTED]",
            "output_truncated": false
        });

        let (kind, details, summary) = render_activity_metadata("task_run_pretask_ran", &payload);

        assert_eq!(kind, "task_run_pretask_ran");
        assert!(details.is_none());
        assert!(summary.is_none());

        // The payload is a bounded JSON object (not unbounded growth).
        let serialized = serde_json::to_string(&payload).expect("must serialize");
        assert!(
            serialized.len() < 2048,
            "payload must be bounded, got {} bytes",
            serialized.len()
        );
    }

    /// `activity_entry_response` produces a well-formed response for an
    /// additive `task_run_pretask_ran` activity entry.  The response has
    /// `kind = "task_run_pretask_ran"`, the full payload is included as
    /// `payload`, and `details`/`summary` are absent (graceful fallback
    /// for the flat payload shape).
    ///
    /// This is the path a generic activity-feed or timeline renderer
    /// takes when surfacing an unknown event type — it must not crash or
    /// reject the entry.
    #[test]
    fn activity_entry_response_safely_surfaces_additive_pretask_payload() {
        let entry = ActivityEntry {
            id: "act-compat-001".to_owned(),
            task_id: Some("t-compat".to_owned()),
            actor_id: "system".to_owned(),
            actor_role: "system".to_owned(),
            event_type: "task_run_pretask_ran".to_owned(),
            payload: serde_json::json!({
                "name": "apply-schema",
                "index": 0,
                "command": "psql -f schema.sql",
                "failure_policy": "blocking",
                "started_at": "2026-07-08T12:00:00.000Z",
                "duration_ms": 3000,
                "exit_code": 0,
                "timed_out": false,
                "cancelled": false,
                "blocked": false,
                "output_tail": "CREATE TABLE\n",
                "output_truncated": false
            })
            .to_string(),
            created_at: "2026-07-08T12:00:03.000Z".to_owned(),
        };

        let response = activity_entry_response(entry);

        assert_eq!(response.id, "act-compat-001");
        assert_eq!(response.event_type, "task_run_pretask_ran");
        assert_eq!(response.kind, "task_run_pretask_ran");
        assert!(response.details.is_none());
        assert!(response.summary.is_none());

        // The response itself is bounded (no unbounded growth from
        // rendering the additive event).
        let serialized = serde_json::to_string(&response).expect("response must serialize");
        assert!(
            serialized.len() < 4096,
            "response must be bounded, got {} bytes",
            serialized.len()
        );

        // The full payload is surfaced for the UI/renderer.
        let payload = &response.payload.0;
        assert_eq!(payload["name"], "apply-schema");
        assert_eq!(payload["exit_code"], 0);
        assert_eq!(payload["blocked"], false);
    }

    /// A blocked `task_run_pretask_ran` entry with `failure_class` and
    /// redacted output is surfaced safely through
    /// `activity_entry_response`.  The redacted `output_tail` containing
    /// `[REDACTED]` markers is included in the payload without being
    /// stripped or mangled by the rendering path.
    #[test]
    fn activity_entry_response_preserves_redacted_pretask_output() {
        let entry = ActivityEntry {
            id: "act-compat-002".to_owned(),
            task_id: Some("t-compat-2".to_owned()),
            actor_id: "system".to_owned(),
            actor_role: "system".to_owned(),
            event_type: "task_run_pretask_ran".to_owned(),
            payload: serde_json::json!({
                "name": "migrate-db",
                "index": 0,
                "command": "psql [REDACTED]",
                "failure_policy": "blocking",
                "started_at": "2026-07-08T12:00:00.000Z",
                "duration_ms": 500,
                "exit_code": 1,
                "timed_out": false,
                "cancelled": false,
                "blocked": true,
                "failure_class": "environmental",
                "output_tail": "FATAL: password auth failed for user [REDACTED]\n[REDACTED]",
                "output_truncated": false
            })
            .to_string(),
            created_at: "2026-07-08T12:00:00.500Z".to_owned(),
        };

        let response = activity_entry_response(entry);
        let payload = &response.payload.0;

        // Redaction markers are preserved — the rendering path does not
        // strip or transform the payload.
        let output_tail = payload["output_tail"].as_str().unwrap();
        assert!(
            output_tail.contains("[REDACTED]"),
            "redaction markers must be preserved by the rendering path"
        );
        assert_eq!(payload["failure_class"], "environmental");
        assert_eq!(payload["blocked"], true);

        // Bounded output — even with redaction markers, the payload is small.
        assert!(
            output_tail.len() < 2048,
            "redacted output_tail must be bounded"
        );
    }
}
