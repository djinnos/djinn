use std::path::{Path, PathBuf};

use std::process::Stdio;

use tokio::time::{Duration, timeout};

use rmcp::model::Tool as RmcpTool;
use rmcp::object;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::AgentType;
use super::sandbox;
use crate::db::EpicRepository;
use crate::db::NoteRepository;
use crate::db::ProjectRepository;
use crate::db::SessionRepository;
use crate::db::TaskRepository;
use crate::models::Task;
use crate::server::AppState;

#[derive(Deserialize)]
struct IncomingToolCall {
    name: String,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Find the largest byte index <= `idx` that is a valid UTF-8 char boundary.
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    let mut i = idx;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

async fn dispatch_tool_call<T>(
    state: &AppState,
    tool_call: &T,
    worktree_path: &Path,
    agent_type: Option<AgentType>,
) -> Result<serde_json::Value, String>
where
    T: Serialize,
{
    let call: IncomingToolCall =
        from_value(serde_json::to_value(tool_call).map_err(|e| e.to_string())?)
            .map_err(|e| format!("invalid frontend tool payload: {e}"))?;

    // Resolve project_id from worktree_path so agent tools are project-scoped.
    let project_id = {
        let repo = ProjectRepository::new(state.db().clone(), state.events().clone());
        let path_str = worktree_path.to_string_lossy();
        repo.resolve(&path_str).await.ok().flatten()
    };

    if let Some(agent_type) = agent_type
        && !is_tool_allowed_for_agent(agent_type, &call.name)
    {
        return Err(format!(
            "tool `{}` is not allowed for agent type {:?}",
            call.name, agent_type
        ));
    }

    match call.name.as_str() {
        "task_list" => call_task_list(state, &call.arguments, project_id.as_deref()).await,
        "task_show" => call_task_show(state, &call.arguments).await,
        "task_create" => call_task_create(state, &call.arguments).await,
        "task_update" => call_task_update(state, &call.arguments).await,
        "task_update_ac" => call_task_update_ac(state, &call.arguments).await,
        "task_comment_add" => call_task_comment_add(state, &call.arguments).await,
        "request_pm" => call_request_pm(state, &call.arguments).await,
        "task_transition" => call_task_transition(state, &call.arguments).await,
        "task_delete_branch" => call_task_delete_branch(state, &call.arguments).await,
        "task_archive_activity" => call_task_archive_activity(state, &call.arguments).await,
        "task_reset_counters" => call_task_reset_counters(state, &call.arguments).await,
        "task_kill_session" => call_task_kill_session(state, &call.arguments).await,
        "task_blocked_list" => call_task_blocked_list(state, &call.arguments).await,
        "task_activity_list" => call_task_activity_list(state, &call.arguments).await,
        "epic_show" => call_epic_show(state, &call.arguments).await,
        "epic_update" => call_epic_update(state, &call.arguments).await,
        "epic_tasks" => call_epic_tasks(state, &call.arguments).await,
        "memory_read" => call_memory_read(state, &call.arguments).await,
        "memory_search" => call_memory_search(state, &call.arguments).await,
        "shell" => call_shell(&call.arguments, worktree_path).await,
        "write" => call_write(&call.arguments, worktree_path).await,
        "edit" => call_edit(&call.arguments, worktree_path).await,
        other => Err(format!("unknown djinn frontend tool: {other}")),
    }
}

#[derive(Deserialize)]
struct TaskListParams {
    status: Option<String>,
    issue_type: Option<String>,
    priority: Option<i64>,
    #[serde(alias = "q")]
    text: Option<String>,
    label: Option<String>,
    parent: Option<String>,
    sort: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Deserialize)]
struct TaskShowParams {
    id: String,
}

#[derive(Deserialize)]
struct TaskActivityListParams {
    id: String,
    #[serde(default)]
    event_type: Option<String>,
    #[serde(default)]
    actor_role: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct TaskUpdateParams {
    id: String,
    title: Option<String>,
    description: Option<String>,
    design: Option<String>,
    priority: Option<i64>,
    owner: Option<String>,
    labels_add: Option<Vec<String>>,
    labels_remove: Option<Vec<String>>,
    acceptance_criteria: Option<Vec<serde_json::Value>>,
    memory_refs_add: Option<Vec<String>>,
    memory_refs_remove: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct TaskUpdateAcParams {
    id: String,
    acceptance_criteria: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct TaskCreateParams {
    epic_id: String,
    title: String,
    issue_type: Option<String>,
    description: Option<String>,
    design: Option<String>,
    priority: Option<i64>,
    owner: Option<String>,
    status: Option<String>,
    acceptance_criteria: Option<Vec<String>>,
    blocked_by: Option<Vec<String>>,
    memory_refs: Option<Vec<String>>,
}


#[derive(Deserialize)]
struct EpicShowParams {
    id: String,
}

#[derive(Deserialize)]
struct EpicUpdateParams {
    id: String,
    title: Option<String>,
    description: Option<String>,
    status: Option<String>,
    memory_refs_add: Option<Vec<String>>,
    memory_refs_remove: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct EpicTasksParams {
    id: String,
    limit: Option<i64>,
    offset: Option<i64>,
}

#[derive(Deserialize)]
struct TaskCommentAddParams {
    id: String,
    body: String,
    actor_id: Option<String>,
    actor_role: Option<String>,
}

#[derive(Deserialize)]
struct MemoryReadParams {
    project: Option<String>,
    identifier: String,
}

#[derive(Deserialize)]
struct MemorySearchParams {
    project: Option<String>,
    query: String,
    folder: Option<String>,
    #[serde(rename = "type")]
    note_type: Option<String>,
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct ShellParams {
    command: String,
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
struct WriteParams {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct EditParams {
    path: String,
    old_text: String,
    new_text: String,
}

/// Normalize `Some("")` → `None`. OpenAI models often send empty strings
/// for optional parameters instead of omitting them, which breaks SQL filters.
fn non_empty(opt: Option<String>) -> Option<String> {
    opt.filter(|s| !s.is_empty())
}

async fn call_task_list(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: TaskListParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db().clone(), state.events().clone());

    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let query = crate::db::ListQuery {
        project_id: project_id.map(|s| s.to_string()),
        status: non_empty(p.status),
        issue_type: non_empty(p.issue_type),
        priority: p.priority.filter(|&v| v != 0),
        text: non_empty(p.text),
        label: non_empty(p.label),
        parent: non_empty(p.parent),
        sort: non_empty(p.sort).unwrap_or_else(|| "priority".to_string()),
        limit,
        offset,
    };

    let result = repo.list_filtered(query).await.map_err(|e| e.to_string())?;
    let has_more = offset + i64::try_from(result.tasks.len()).unwrap_or(0) < result.total_count;

    Ok(serde_json::json!({
        "tasks": result.tasks.iter().map(task_to_value).collect::<Vec<_>>(),
        "total": result.total_count,
        "limit": limit,
        "offset": offset,
        "has_more": has_more,
    }))
}

async fn call_task_show(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskShowParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db().clone(), state.events().clone());
    let session_repo = SessionRepository::new(state.db().clone(), state.events().clone());

    match repo.resolve(&p.id).await {
        Ok(Some(task)) => {
            let mut value = task_to_value(&task);
            if let Some(map) = value.as_object_mut() {
                let session_count = session_repo.count_for_task(&task.id).await.unwrap_or(0);
                let active_session = session_repo.active_for_task(&task.id).await.ok().flatten();
                map.insert(
                    "session_count".to_string(),
                    serde_json::json!(session_count),
                );
                map.insert(
                    "active_session".to_string(),
                    serde_json::json!(active_session),
                );

                // Include recent activity (comments, transitions) so agents
                // can see worker notes and review history.
                // Cap entries and payload sizes to prevent context-window blowup
                // on tasks with many sessions / verbose error logs.
                const MAX_ACTIVITY_ENTRIES: usize = 30;
                const MAX_PAYLOAD_CHARS: usize = 1500;
                let activity = repo.list_activity(&task.id).await.unwrap_or_default();
                let activity_json: Vec<serde_json::Value> = activity
                    .iter()
                    // Skip session_error events — they contain verbose diagnostics
                    // that are not useful for agent decision-making.
                    .filter(|e| e.event_type != "session_error")
                    .take(MAX_ACTIVITY_ENTRIES)
                    .map(|entry| {
                        let mut payload = serde_json::from_str::<serde_json::Value>(&entry.payload)
                            .unwrap_or(serde_json::json!({}));
                        // Truncate large payload string values (e.g. verification output).
                        if let Some(obj) = payload.as_object_mut() {
                            for value in obj.values_mut() {
                                if let Some(s) = value.as_str()
                                    && s.len() > MAX_PAYLOAD_CHARS
                                {
                                    let end = floor_char_boundary(s, MAX_PAYLOAD_CHARS);
                                    let truncated = format!(
                                        "{}… [truncated, {} total chars]",
                                        &s[..end],
                                        s.len()
                                    );
                                    *value = serde_json::json!(truncated);
                                }
                            }
                        }
                        serde_json::json!({
                            "id": entry.id,
                            "actor_role": entry.actor_role,
                            "event_type": entry.event_type,
                            "payload": payload,
                            "created_at": entry.created_at,
                        })
                    })
                    .collect();
                map.insert("activity".to_string(), serde_json::json!(activity_json));
            }
            Ok(value)
        }
        Ok(None) => Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) })),
        Err(e) => Err(e.to_string()),
    }
}

async fn call_task_activity_list(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    use crate::db::ActivityQuery;

    let p: TaskActivityListParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db().clone(), state.events().clone());

    // Resolve short_id to full UUID
    let task_id = match repo.resolve(&p.id).await {
        Ok(Some(task)) => task.id,
        Ok(None) => return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) })),
        Err(e) => return Err(e.to_string()),
    };

    let limit = p.limit.unwrap_or(30).min(50);
    let entries = repo
        .query_activity(ActivityQuery {
            task_id: Some(task_id),
            event_type: p.event_type,
            actor_role: p.actor_role,
            limit,
            ..Default::default()
        })
        .await
        .map_err(|e| e.to_string())?;

    const MAX_PAYLOAD_CHARS: usize = 1500;
    let activity_json: Vec<serde_json::Value> = entries
        .iter()
        .map(|entry| {
            let mut payload = serde_json::from_str::<serde_json::Value>(&entry.payload)
                .unwrap_or(serde_json::json!({}));
            if let Some(obj) = payload.as_object_mut() {
                for value in obj.values_mut() {
                    if let Some(s) = value.as_str()
                        && s.len() > MAX_PAYLOAD_CHARS
                    {
                        let end = floor_char_boundary(s, MAX_PAYLOAD_CHARS);
                        let truncated =
                            format!("{}… [truncated, {} total chars]", &s[..end], s.len());
                        *value = serde_json::json!(truncated);
                    }
                }
            }
            serde_json::json!({
                "actor_role": entry.actor_role,
                "event_type": entry.event_type,
                "payload": payload,
                "created_at": entry.created_at,
            })
        })
        .collect();

    Ok(serde_json::json!({ "count": activity_json.len(), "entries": activity_json }))
}

async fn call_epic_show(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: EpicShowParams = parse_args(arguments)?;
    let repo = EpicRepository::new(state.db().clone(), state.events().clone());

    match repo.resolve(&p.id).await {
        Ok(Some(epic)) => Ok(serde_json::to_value(epic).map_err(|e| e.to_string())?),
        Ok(None) => Ok(serde_json::json!({ "error": format!("epic not found: {}", p.id) })),
        Err(e) => Err(e.to_string()),
    }
}

async fn call_epic_update(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: EpicUpdateParams = parse_args(arguments)?;
    let repo = EpicRepository::new(state.db().clone(), state.events().clone());

    let Some(epic) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("epic not found: {}", p.id) }));
    };

    let title = p.title.as_deref().unwrap_or(&epic.title);
    let description = p.description.as_deref().unwrap_or(&epic.description);
    let emoji = epic.emoji.as_str();
    let color = epic.color.as_str();
    let owner = epic.owner.as_str();

    let updated = repo
        .update(&epic.id, title, description, emoji, color, owner)
        .await
        .map_err(|e| e.to_string())?;

    if let Some(_status) = p.status.as_deref() {
        // EpicRepository::update does not currently support status transitions.
    }

    if p.memory_refs_add.is_some() || p.memory_refs_remove.is_some() {
        // Epics do not currently persist memory_refs in storage; accepted for forward compatibility.
    }

    serde_json::to_value(updated).map_err(|e| e.to_string())
}

async fn call_epic_tasks(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: EpicTasksParams = parse_args(arguments)?;
    let epic_repo = EpicRepository::new(state.db().clone(), state.events().clone());

    let Some(epic) = epic_repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("epic not found: {}", p.id) }));
    };

    let task_repo = TaskRepository::new(state.db().clone(), state.events().clone());
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let offset = p.offset.unwrap_or(0).max(0);
    let mut all = task_repo.list_by_epic(&epic.id).await.map_err(|e| e.to_string())?;
    let total = i64::try_from(all.len()).unwrap_or(0);
    let start = usize::try_from(offset).unwrap_or(0).min(all.len());
    let end = start.saturating_add(usize::try_from(limit).unwrap_or(0)).min(all.len());
    let tasks = all.drain(start..end).map(|t| task_to_value(&t)).collect::<Vec<_>>();
    let has_more = i64::try_from(end).unwrap_or(0) < total;

    Ok(serde_json::json!({
        "tasks": tasks,
        "total": total,
        "limit": limit,
        "offset": offset,
        "has_more": has_more,
    }))
}

async fn call_task_create(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskCreateParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db().clone(), state.events().clone());

    let issue_type = p.issue_type.as_deref().unwrap_or("task");
    let description = p.description.as_deref().unwrap_or("");
    let design = p.design.as_deref().unwrap_or("");
    let priority = p.priority.unwrap_or(0);
    let owner = p.owner.as_deref().unwrap_or("");
    let status = match p.status.as_deref() {
        None => None,
        Some("backlog") => Some("backlog"),
        Some("open") => Some("open"),
        Some(other) => {
            return Err(format!(
                "invalid status: {other:?} (expected backlog or open)"
            ));
        }
    };

    // Resolve epic_id (accepts UUID or short_id).
    let epic_repo = EpicRepository::new(state.db().clone(), state.events().clone());
    let epic = epic_repo
        .resolve(&p.epic_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("epic not found: {}", p.epic_id))?;

    let mut task = repo
        .create(
            &epic.id,
            &p.title,
            description,
            design,
            issue_type,
            priority,
            owner,
            status,
        )
        .await
        .map_err(|e| e.to_string())?;

    // Set acceptance criteria if provided.
    if let Some(ref ac) = p.acceptance_criteria {
        let ac_items: Vec<serde_json::Value> = ac
            .iter()
            .map(|c| serde_json::json!({"criterion": c, "met": false}))
            .collect();
        let ac_json = serde_json::to_string(&ac_items).unwrap_or_else(|_| "[]".into());
        task = repo
            .update(
                &task.id,
                &task.title,
                &task.description,
                &task.design,
                task.priority,
                &task.owner,
                &task.labels,
                &ac_json,
            )
            .await
            .map_err(|e| e.to_string())?;
    }

    // Set memory_refs if provided.
    if let Some(ref refs) = p.memory_refs
        && !refs.is_empty()
    {
        let refs_json = serde_json::to_string(refs).unwrap_or_else(|_| "[]".into());
        if let Ok(t) = repo.update_memory_refs(&task.id, &refs_json).await {
            task = t;
        }
    }

    // Set blocked_by relationships if provided.
    if let Some(ref blockers) = p.blocked_by {
        for blocker_ref in blockers {
            // Resolve short_id to full UUID if needed.
            if let Ok(Some(blocker_task)) = repo.resolve(blocker_ref).await {
                let _ = repo.add_blocker(&task.id, &blocker_task.id).await;
            }
        }
    }

    Ok(task_to_value(&task))
}

async fn call_task_update(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskUpdateParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db().clone(), state.events().clone());

    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    let title = p.title.as_deref().unwrap_or(&task.title);
    let description = p.description.as_deref().unwrap_or(&task.description);
    let design = p.design.as_deref().unwrap_or(&task.design);
    let priority = p.priority.unwrap_or(task.priority);
    let owner = p.owner.as_deref().unwrap_or(&task.owner);

    let labels_json = if p.labels_add.is_some() || p.labels_remove.is_some() {
        let mut labels: Vec<String> = crate::models::parse_json_array(&task.labels);
        if let Some(add) = p.labels_add {
            for label in add {
                if !labels.contains(&label) {
                    labels.push(label);
                }
            }
        }
        if let Some(remove) = p.labels_remove {
            labels.retain(|v| !remove.contains(v));
        }
        serde_json::to_string(&labels).unwrap_or_else(|_| "[]".to_string())
    } else {
        task.labels.clone()
    };

    let ac_json = p
        .acceptance_criteria
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string()))
        .unwrap_or_else(|| task.acceptance_criteria.clone());

    let updated = repo
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
        .map_err(|e| e.to_string())?;

    if p.memory_refs_add.is_some() || p.memory_refs_remove.is_some() {
        let mut refs: Vec<String> = serde_json::from_str(&updated.memory_refs).unwrap_or_default();
        if let Some(add) = p.memory_refs_add {
            for r in add {
                if !refs.contains(&r) {
                    refs.push(r);
                }
            }
        }
        if let Some(remove) = p.memory_refs_remove {
            refs.retain(|r| !remove.contains(r));
        }
        let refs_json = serde_json::to_string(&refs).unwrap_or_else(|_| "[]".to_string());
        let out = repo
            .update_memory_refs(&updated.id, &refs_json)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(task_to_value(&out));
    }

    Ok(task_to_value(&updated))
}

async fn call_task_update_ac(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskUpdateAcParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db().clone(), state.events().clone());

    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    // Merge incoming AC with existing criteria so the `criterion` text is
    // preserved even when the reviewer only sends `{met: bool}` objects.
    let ac_json = merge_acceptance_criteria(&task.acceptance_criteria, &p.acceptance_criteria);

    let updated = repo
        .update(
            &task.id,
            &task.title,
            &task.description,
            &task.design,
            task.priority,
            &task.owner,
            &task.labels,
            &ac_json,
        )
        .await
        .map_err(|e| e.to_string())?;

    Ok(task_to_value(&updated))
}

async fn call_request_pm(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    #[derive(Deserialize)]
    struct RequestPmParams {
        id: String,
        reason: String,
        suggested_breakdown: Option<String>,
    }

    let p: RequestPmParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db().clone(), state.events().clone());

    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    // Log the PM request as a structured comment.
    let mut body = format!("[PM_REQUEST] {}", p.reason);
    if let Some(ref breakdown) = p.suggested_breakdown {
        body.push_str(&format!("\n\nSuggested breakdown:\n{breakdown}"));
    }
    let payload = serde_json::json!({ "body": body }).to_string();
    repo.log_activity(Some(&task.id), "worker-agent", "worker", "comment", &payload)
        .await
        .map_err(|e| e.to_string())?;

    // Escalate to PM intervention queue.
    let updated = repo
        .transition(
            &task.id,
            crate::models::TransitionAction::Escalate,
            "worker-agent",
            "worker",
            Some(&p.reason),
            None,
        )
        .await
        .map_err(|e| e.to_string())?;

    Ok(serde_json::json!({
        "status": "escalated",
        "task_id": updated.id,
        "new_status": updated.status,
        "message": "Task escalated to PM. Your session should end now."
    }))
}

async fn call_task_comment_add(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskCommentAddParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db().clone(), state.events().clone());

    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    let payload = serde_json::json!({ "body": p.body }).to_string();
    let actor_id = p.actor_id.as_deref().unwrap_or("goose-agent");
    let actor_role = p.actor_role.as_deref().unwrap_or("system");

    let entry = repo
        .log_activity(Some(&task.id), actor_id, actor_role, "comment", &payload)
        .await
        .map_err(|e| e.to_string())?;

    Ok(serde_json::json!({
        "id": entry.id,
        "task_id": entry.task_id,
        "actor_id": entry.actor_id,
        "actor_role": entry.actor_role,
        "event_type": entry.event_type,
        "payload": serde_json::from_str::<serde_json::Value>(&entry.payload).unwrap_or(serde_json::json!({})),
        "created_at": entry.created_at,
    }))
}

async fn call_memory_read(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: MemoryReadParams = parse_args(arguments)?;
    let project_path = resolve_project_path(p.project);
    let project_id = project_id_for_path(state, &project_path).await?;

    let repo = NoteRepository::new(state.db().clone(), state.events().clone());
    let note = resolve_note_by_identifier(&repo, &project_id, &p.identifier)
        .await
        .ok_or_else(|| format!("note not found: {}", p.identifier))?;

    let _ = repo.touch_accessed(&note.id).await;
    Ok(note_to_value(&note))
}

async fn call_memory_search(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: MemorySearchParams = parse_args(arguments)?;
    let project_path = resolve_project_path(p.project);
    let project_id = project_id_for_path(state, &project_path).await?;

    let repo = NoteRepository::new(state.db().clone(), state.events().clone());
    let limit = p.limit.unwrap_or(10).clamp(1, 100) as usize;
    let results = repo
        .search(
            &project_id,
            &p.query,
            p.folder.as_deref(),
            p.note_type.as_deref(),
            limit,
        )
        .await
        .map_err(|e| e.to_string())?;

    let items: Vec<serde_json::Value> = results
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "permalink": r.permalink,
                "title": r.title,
                "folder": r.folder,
                "note_type": r.note_type,
                "snippet": r.snippet,
            })
        })
        .collect();

    Ok(serde_json::json!({ "results": items }))
}

async fn call_shell(
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
) -> Result<serde_json::Value, String> {
    let p: ShellParams = parse_args(arguments)?;
    let timeout_ms = p.timeout_ms.unwrap_or(120_000).max(1000);

    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("cmd");
        c.arg("/c").arg(&p.command);
        c
    } else {
        let mut c = std::process::Command::new("bash");
        c.arg("-lc").arg(&p.command);
        c
    };

    sandbox::SANDBOX
        .apply(worktree_path, &mut cmd)
        .map_err(|e| e.to_string())?;

    cmd.current_dir(worktree_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = timeout(
        Duration::from_millis(timeout_ms),
        crate::process::output(cmd),
    )
    .await
    .map_err(|_| format!("shell timed out after {} ms", timeout_ms))?
    .map_err(|e| format!("failed to run shell command: {e}"))?;

    let stdout = truncate_shell_output(&String::from_utf8_lossy(&output.stdout));
    let stderr = truncate_shell_output(&String::from_utf8_lossy(&output.stderr));

    Ok(serde_json::json!({
        "ok": output.status.success(),
        "exit_code": output.status.code(),
        "stdout": stdout,
        "stderr": stderr,
        "workdir": worktree_path,
    }))
}

async fn call_write(
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
) -> Result<serde_json::Value, String> {
    let p: WriteParams = parse_args(arguments)?;
    let path = resolve_path(&p.path, worktree_path);

    // Ensure path is within worktree
    ensure_path_within_worktree(&path, worktree_path)?;

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("create dirs failed: {e}"))?;
    }
    tokio::fs::write(&path, &p.content)
        .await
        .map_err(|e| format!("write failed: {e}"))?;

    Ok(serde_json::json!({
        "ok": true,
        "path": path.display().to_string(),
        "bytes": p.content.len(),
    }))
}

async fn call_edit(
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
) -> Result<serde_json::Value, String> {
    let p: EditParams = parse_args(arguments)?;
    let path = resolve_path(&p.path, worktree_path);

    // Ensure path is within worktree
    ensure_path_within_worktree(&path, worktree_path)?;

    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("read failed: {e}"))?;

    let count = content.matches(&p.old_text as &str).count();
    if count == 0 {
        return Err(format!("old_text not found in file: {}", path.display()));
    }
    if count > 1 {
        return Err(format!(
            "old_text appears {} times in file (must be unique): {}",
            count,
            path.display()
        ));
    }

    let new_content = content.replacen(&p.old_text as &str, &p.new_text, 1);
    tokio::fs::write(&path, &new_content)
        .await
        .map_err(|e| format!("write failed: {e}"))?;

    Ok(serde_json::json!({
        "ok": true,
        "path": path.display().to_string(),
    }))
}

fn resolve_path(raw: &str, base: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let p = Path::new(raw);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn is_tool_allowed_for_agent(agent_type: AgentType, name: &str) -> bool {
    match name {
        "task_show" | "task_list" | "task_activity_list" | "task_comment_add" | "memory_read"
        | "memory_search" | "shell" | "task_create" => true,
        "write" | "edit" => matches!(agent_type, AgentType::Worker | AgentType::ConflictResolver),
        "request_pm" => matches!(agent_type, AgentType::Worker | AgentType::ConflictResolver),
        "task_update_ac" => matches!(agent_type, AgentType::TaskReviewer),
        "task_update"
        | "task_transition"
        | "task_delete_branch"
        | "task_archive_activity"
        | "task_reset_counters"
        | "task_kill_session"
        | "task_blocked_list"
        | "epic_show"
        | "epic_update"
        | "epic_tasks" => {
            matches!(agent_type, AgentType::PM | AgentType::Groomer)
        }
        _ => false,
    }
}

fn ensure_path_within_worktree(path: &Path, worktree_path: &Path) -> Result<(), String> {
    let canonical_base = std::fs::canonicalize(worktree_path)
        .map_err(|e| format!("failed to canonicalize worktree path: {e}"))?;

    let candidate = if path.exists() {
        std::fs::canonicalize(path).map_err(|e| format!("failed to canonicalize path: {e}"))?
    } else {
        let parent = path.parent().unwrap_or(path);
        let canonical_parent = std::fs::canonicalize(parent)
            .map_err(|e| format!("failed to canonicalize parent path: {e}"))?;
        canonical_parent.join(path.file_name().unwrap_or_default())
    };

    if !candidate.starts_with(&canonical_base) {
        return Err(format!("path is outside worktree: {}", path.display()));
    }

    Ok(())
}
fn parse_args<T>(
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    let args = arguments.clone().unwrap_or_default();
    serde_json::from_value(serde_json::Value::Object(args)).map_err(|e| e.to_string())
}

/// Merge incoming AC objects with existing stored criteria.
///
/// If an incoming object has a `criterion` field it is used as-is.  Otherwise
/// the `criterion` text is copied from the existing array at the same index so
/// that reviewer payloads like `[{"met": true}]` don't erase the text.
fn merge_acceptance_criteria(existing_json: &str, incoming: &[serde_json::Value]) -> String {
    let existing: Vec<serde_json::Value> = serde_json::from_str(existing_json).unwrap_or_default();

    let merged: Vec<serde_json::Value> = incoming
        .iter()
        .enumerate()
        .map(|(i, inc)| {
            let mut obj = inc.as_object().cloned().unwrap_or_default();
            // If the incoming object is missing `criterion`, copy from existing.
            if !obj.contains_key("criterion")
                && let Some(existing_criterion) = existing
                    .get(i)
                    .and_then(|e| e.get("criterion"))
                    .and_then(|v| v.as_str())
            {
                obj.insert(
                    "criterion".to_string(),
                    serde_json::Value::String(existing_criterion.to_string()),
                );
            }
            serde_json::Value::Object(obj)
        })
        .collect();

    serde_json::to_string(&merged).unwrap_or_else(|_| "[]".to_string())
}

async fn project_id_for_path(state: &AppState, project_path: &str) -> Result<String, String> {
    let repo = ProjectRepository::new(state.db().clone(), state.events().clone());
    repo.resolve(project_path)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("project not found: {project_path}"))
}

fn resolve_project_path(project: Option<String>) -> String {
    match project {
        Some(path) => path,
        None => std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .display()
            .to_string(),
    }
}

async fn resolve_note_by_identifier(
    repo: &NoteRepository,
    project_id: &str,
    identifier: &str,
) -> Option<crate::models::Note> {
    repo.resolve(project_id, identifier).await.ok().flatten()
}

fn note_to_value(note: &crate::models::Note) -> serde_json::Value {
    note.to_value()
}

fn task_to_value(t: &Task) -> serde_json::Value {
    let labels = crate::models::parse_json_array(&t.labels);
    let ac: serde_json::Value =
        serde_json::from_str(&t.acceptance_criteria).unwrap_or(serde_json::json!([]));
    let memory_refs: serde_json::Value =
        serde_json::from_str(&t.memory_refs).unwrap_or(serde_json::json!([]));

    serde_json::json!({
        "id": t.id,
        "short_id": t.short_id,
        "epic_id": t.epic_id,
        "title": t.title,
        "description": t.description,
        "design": t.design,
        "issue_type": t.issue_type,
        "status": t.status,
        "priority": t.priority,
        "owner": t.owner,
        "labels": labels,
        "memory_refs": memory_refs,
        "acceptance_criteria": ac,
        "reopen_count": t.reopen_count,
        "continuation_count": t.continuation_count,
        "created_at": t.created_at,
        "updated_at": t.updated_at,
        "closed_at": t.closed_at,
        "close_reason": t.close_reason,
        "merge_commit_sha": t.merge_commit_sha,
    })
}

// ── PM-only tool params and handlers ─────────────────────────────────────────

#[derive(Deserialize)]
struct TaskTransitionParams {
    id: String,
    action: String,
    reason: Option<String>,
    target_status: Option<String>,
    /// Required when action = "force_close". UUIDs or short IDs of replacement
    /// tasks the PM created before closing this one.
    replacement_task_ids: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct TaskDeleteBranchParams {
    id: String,
}

#[derive(Deserialize)]
struct TaskArchiveActivityParams {
    id: String,
}

#[derive(Deserialize)]
struct TaskResetCountersParams {
    id: String,
}

async fn call_task_transition(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    use crate::db::repositories::task::transitions::{PM_MERGE_ACTIONS, merge_and_transition};
    use crate::models::{TaskStatus, TransitionAction};
    let p: TaskTransitionParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db().clone(), state.events().clone());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };
    let action = TransitionAction::parse(&p.action).map_err(|e| e.to_string())?;

    // PM approve: squash merge (verification gate runs inside merge_and_transition).
    if action == TransitionAction::PmApprove {
        if task.status != TaskStatus::InPmIntervention.as_str() {
            return Ok(
                serde_json::json!({ "error": "pm_approve is only valid from in_pm_intervention" }),
            );
        }

        let (merge_action, reason) = merge_and_transition(&task.id, state, &PM_MERGE_ACTIONS)
            .await
            .unwrap_or((
                TransitionAction::PmInterventionRelease,
                Some("merge_and_transition returned None".to_string()),
            ));
        let updated = repo
            .transition(
                &task.id,
                merge_action,
                "pm-agent",
                "pm",
                reason.as_deref(),
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        return Ok(task_to_value(&updated));
    }

    // Guard: force_close requires explicit replacement_task_ids that exist and are open.
    // Without this, the PM can close a task claiming "decomposition" without
    // actually creating the replacement subtasks, losing unmerged work.
    if action == TransitionAction::ForceClose {
        let ids = match p.replacement_task_ids {
            Some(ref ids) if !ids.is_empty() => ids,
            _ => {
                return Ok(serde_json::json!({
                    "error": "force_close requires replacement_task_ids: an array of task IDs (UUID or short ID) for the replacement tasks you created. Create subtasks with task_create first, then pass their IDs to force_close."
                }));
            }
        };
        let mut missing = Vec::new();
        for id in ids {
            match repo.resolve(id).await {
                Ok(Some(t)) if t.status == TaskStatus::Open.as_str() => {}
                Ok(Some(t)) => {
                    missing.push(format!("{} (status: {})", id, t.status));
                }
                _ => {
                    missing.push(format!("{} (not found)", id));
                }
            }
        }
        if !missing.is_empty() {
            return Ok(serde_json::json!({
                "error": format!(
                    "force_close replacement tasks must exist and be open. Problems: {}",
                    missing.join(", ")
                )
            }));
        }
    }

    let target = p
        .target_status
        .as_deref()
        .map(TaskStatus::parse)
        .transpose()
        .map_err(|e| e.to_string())?;
    let updated = repo
        .transition(
            &task.id,
            action,
            "pm-agent",
            "pm",
            p.reason.as_deref(),
            target,
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(task_to_value(&updated))
}

async fn call_task_delete_branch(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskDeleteBranchParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db().clone(), state.events().clone());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    // Interrupt and clean up any paused worker session (handles worktree cleanup too).
    crate::db::repositories::task::transitions::interrupt_paused_worker_session(&task.id, state)
        .await;
    crate::db::repositories::task::transitions::cleanup_paused_worker_session(&task.id, state)
        .await;

    // Delete the task branch from git.
    let project_dir = match crate::db::repositories::task::transitions::resolve_project_path_for_id(
        &task.project_id,
        state,
    )
    .await
    {
        Some(p) => std::path::PathBuf::from(p),
        None => return Ok(serde_json::json!({ "error": "project not found" })),
    };
    let base_branch = format!("task/{}", task.short_id);
    match state.git_actor(&project_dir).await {
        Ok(git) => {
            if let Err(e) = git.delete_branch(&base_branch).await {
                // Not fatal — branch may not exist
                tracing::warn!(task_id = %task.short_id, branch = %base_branch, error = %e, "PM reset_branch: branch deletion failed (may not exist)");
            }
        }
        Err(e) => {
            return Ok(serde_json::json!({ "error": format!("git actor failed: {e}") }));
        }
    }

    Ok(serde_json::json!({
        "ok": true,
        "task_id": task.short_id,
        "branch_deleted": base_branch,
    }))
}

async fn call_task_archive_activity(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskArchiveActivityParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db().clone(), state.events().clone());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };
    let count = repo
        .archive_activity_for_task(&task.id)
        .await
        .map_err(|e| e.to_string())?;
    Ok(serde_json::json!({ "ok": true, "task_id": task.short_id, "archived_count": count }))
}

async fn call_task_reset_counters(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskResetCountersParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db().clone(), state.events().clone());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };
    sqlx::query(
        "UPDATE tasks SET reopen_count = 0, continuation_count = 0, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1"
    )
    .bind(&task.id)
    .execute(state.db().pool())
    .await
    .map_err(|e| e.to_string())?;
    let updated = repo
        .get(&task.id)
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or(task.clone());
    let _ = state.events().send(crate::events::DjinnEvent::TaskUpdated {
        task: updated.clone(),
        from_sync: false,
    });
    Ok(
        serde_json::json!({ "ok": true, "task_id": task.short_id, "reopen_count": 0, "continuation_count": 0 }),
    )
}

#[derive(Deserialize)]
struct TaskKillSessionParams {
    id: String,
}

async fn call_task_kill_session(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskKillSessionParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db().clone(), state.events().clone());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    // Interrupt the paused session and delete saved conversation.
    // This forces a fresh session on next dispatch without deleting the branch.
    crate::db::repositories::task::transitions::interrupt_paused_worker_session(&task.id, state)
        .await;
    crate::db::repositories::task::transitions::cleanup_paused_worker_session(&task.id, state)
        .await;

    Ok(serde_json::json!({
        "ok": true,
        "task_id": task.short_id,
        "message": "Paused session interrupted and conversation deleted. Next dispatch will start a fresh session."
    }))
}


fn tool_epic_show() -> RmcpTool {
    RmcpTool::new(
        "epic_show".to_string(),
        "Show details for an epic by UUID or short ID.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Epic UUID or short ID"}
            }
        }),
    )
}

fn tool_epic_update() -> RmcpTool {
    RmcpTool::new(
        "epic_update".to_string(),
        "Update epic fields (title/description) and accept memory ref delta args for groomer workflows.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Epic UUID or short ID"},
                "title": {"type": "string"},
                "description": {"type": "string"},
                "status": {"type": "string"},
                "memory_refs_add": {"type": "array", "items": {"type": "string"}},
                "memory_refs_remove": {"type": "array", "items": {"type": "string"}}
            }
        }),
    )
}

fn tool_epic_tasks() -> RmcpTool {
    RmcpTool::new(
        "epic_tasks".to_string(),
        "List tasks for an epic with pagination.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Epic UUID or short ID"},
                "limit": {"type": "integer"},
                "offset": {"type": "integer"}
            }
        }),
    )
}

fn tool_task_list() -> RmcpTool {
    RmcpTool::new(
        "task_list".to_string(),
        "List tasks with optional filters and pagination.".to_string(),
        object!({
            "type": "object",
            "properties": {
                "status": {"type": "string"},
                "issue_type": {"type": "string"},
                "priority": {"type": "integer"},
                "text": {"type": "string", "description": "Free-text search in title/description"},
                "label": {"type": "string"},
                "parent": {"type": "string", "description": "Epic ID to filter by"},
                "sort": {"type": "string"},
                "limit": {"type": "integer"},
                "offset": {"type": "integer"}
            }
        }),
    )
}

async fn call_task_blocked_list(
    state: &AppState,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskShowParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db().clone(), state.events().clone());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };
    let blocked = repo
        .list_blocked_by(&task.id)
        .await
        .map_err(|e| e.to_string())?;
    let tasks: Vec<serde_json::Value> = blocked
        .iter()
        .map(|b| {
            serde_json::json!({
                "task_id": b.task_id,
                "short_id": b.short_id,
                "title": b.title,
                "status": b.status,
            })
        })
        .collect();
    Ok(serde_json::json!({ "tasks": tasks }))
}

fn tool_task_blocked_list() -> RmcpTool {
    RmcpTool::new(
        "task_blocked_list".to_string(),
        "List tasks that are blocked by the given task. Use before decomposing to check downstream dependents.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"}
            }
        }),
    )
}

fn tool_task_activity_list() -> RmcpTool {
    RmcpTool::new(
        "task_activity_list".to_string(),
        "Query a task's activity log with optional filters. Returns comments, status transitions, verification results, and other events. Use to inspect PM guidance, reviewer feedback, or verification history.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"},
                "event_type": {"type": "string", "description": "Filter by event type: comment, status_changed, commands_run, merge_conflict, task_review_start"},
                "actor_role": {"type": "string", "description": "Filter by actor: pm, task_reviewer, worker, verification, system"},
                "limit": {"type": "integer", "description": "Max entries to return (default 30, max 50)"}
            }
        }),
    )
}

fn tool_task_show() -> RmcpTool {
    RmcpTool::new(
        "task_show".to_string(),
        "Show details of a work item including recent activity and blockers.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"}
            }
        }),
    )
}

fn tool_task_update_ac() -> RmcpTool {
    RmcpTool::new(
        "task_update_ac".to_string(),
        "Update acceptance criteria met/unmet state on a task. Each criterion must include 'met: true' or 'met: false'.".to_string(),
        object!({
            "type": "object",
            "required": ["id", "acceptance_criteria"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"},
                "acceptance_criteria": {"type": "array", "items": {"type": "object"}, "description": "Full acceptance criteria array with met state updated"}
            }
        }),
    )
}

fn tool_task_comment_add() -> RmcpTool {
    RmcpTool::new(
        "task_comment_add".to_string(),
        "Add a comment to a work item.".to_string(),
        object!({
            "type": "object",
            "required": ["id", "body"],
            "properties": {
                "id": {"type": "string"},
                "body": {"type": "string"},
                "actor_id": {"type": "string"},
                "actor_role": {"type": "string"}
            }
        }),
    )
}

fn tool_request_pm() -> RmcpTool {
    RmcpTool::new(
        "request_pm".to_string(),
        "Request PM intervention for the current task. Use when the task is too large to complete reliably, the design is ambiguous, or you are stuck. Adds a comment with your reason and suggested breakdown, then escalates to the PM queue. Your session will effectively end after this call."
            .to_string(),
        object!({
            "type": "object",
            "required": ["id", "reason"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short_id"},
                "reason": {"type": "string", "description": "Why PM intervention is needed (e.g. task too large, design ambiguous, blocked on decision)"},
                "suggested_breakdown": {"type": "string", "description": "Optional suggested split: list of smaller tasks the PM should create"}
            }
        }),
    )
}

fn tool_memory_read() -> RmcpTool {
    RmcpTool::new(
        "memory_read".to_string(),
        "Read a note by permalink or title.".to_string(),
        object!({
            "type": "object",
            "required": ["identifier"],
            "properties": {
                "project": {"type": "string", "description": "Absolute project path"},
                "identifier": {"type": "string"}
            }
        }),
    )
}

fn tool_memory_search() -> RmcpTool {
    RmcpTool::new(
        "memory_search".to_string(),
        "Search notes in project memory.".to_string(),
        object!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "project": {"type": "string", "description": "Absolute project path"},
                "query": {"type": "string"},
                "folder": {"type": "string"},
                "type": {"type": "string"},
                "limit": {"type": "integer"}
            }
        }),
    )
}

fn tool_shell() -> RmcpTool {
    RmcpTool::new(
        "shell".to_string(),
        "Execute shell commands in the task worktree. Commands always run from the worktree root."
            .to_string(),
        object!({
            "type": "object",
            "required": ["command"],
            "properties": {
                "command": {"type": "string", "description": "Shell command to execute"},
                "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default 120000)"}
            }
        }),
    )
}

fn tool_write() -> RmcpTool {
    RmcpTool::new(
        "write".to_string(),
        "Write content to a file, creating it or overwriting if it exists. Path must be within the task worktree.".to_string(),
        object!({
            "type": "object",
            "required": ["path", "content"],
            "properties": {
                "path": {"type": "string", "description": "Absolute or worktree-relative file path"},
                "content": {"type": "string", "description": "File content to write"}
            }
        }),
    )
}

fn tool_edit() -> RmcpTool {
    RmcpTool::new(
        "edit".to_string(),
        "Edit a file by replacing exact text. Finds old_text and replaces with new_text. Fails if old_text is not found or is ambiguous (appears multiple times).".to_string(),
        object!({
            "type": "object",
            "required": ["path", "old_text", "new_text"],
            "properties": {
                "path": {"type": "string", "description": "Absolute or worktree-relative file path"},
                "old_text": {"type": "string", "description": "Exact text to find and replace"},
                "new_text": {"type": "string", "description": "Replacement text"}
            }
        }),
    )
}

#[allow(dead_code)]
fn tool_task_create() -> RmcpTool {
    RmcpTool::new(
        "task_create".to_string(),
        "Create a new task under an epic. Use blocked_by to set dependencies and acceptance_criteria to define success criteria at creation.".to_string(),
        object!({
            "type": "object",
            "required": ["epic_id", "title"],
            "properties": {
                "epic_id": {"type": "string"},
                "title": {"type": "string"},
                "issue_type": {"type": "string"},
                "description": {"type": "string"},
                "design": {"type": "string"},
                "priority": {"type": "integer"},
                "owner": {"type": "string"},
                "status": {"type": "string", "description": "Optional initial status. Allowed: backlog or open."},
                "acceptance_criteria": {"type": "array", "items": {"type": "string"}, "description": "List of acceptance criteria strings."},
                "blocked_by": {"type": "array", "items": {"type": "string"}, "description": "Task IDs (UUID or short_id) that block this task."},
                "memory_refs": {"type": "array", "items": {"type": "string"}, "description": "Memory note permalinks to attach."}
            }
        }),
    )
}

fn tool_task_update() -> RmcpTool {
    RmcpTool::new(
        "task_update".to_string(),
        "Update task fields: title, description, design, priority, owner, labels, acceptance_criteria, memory_refs.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string"},
                "title": {"type": "string"},
                "description": {"type": "string"},
                "design": {"type": "string"},
                "priority": {"type": "integer"},
                "owner": {"type": "string"},
                "labels_add": {"type": "array", "items": {"type": "string"}},
                "labels_remove": {"type": "array", "items": {"type": "string"}},
                "acceptance_criteria": {"type": "array", "items": {"type": "object"}},
                "memory_refs_add": {"type": "array", "items": {"type": "string"}},
                "memory_refs_remove": {"type": "array", "items": {"type": "string"}}
            }
        }),
    )
}

fn tool_task_transition() -> RmcpTool {
    RmcpTool::new(
        "task_transition".to_string(),
        "Execute a state machine transition on a task. PM can use: pm_intervention_complete (rescope and reopen for worker), pm_approve (approve implementation and merge), force_close (requires replacement_task_ids), escalate.".to_string(),
        object!({
            "type": "object",
            "required": ["id", "action"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"},
                "action": {"type": "string", "description": "Transition action"},
                "reason": {"type": "string"},
                "target_status": {"type": "string"},
                "replacement_task_ids": {"type": "array", "items": {"type": "string"}, "description": "Required for force_close: IDs of replacement tasks created before closing this one"}
            }
        }),
    )
}

#[allow(dead_code)]
fn tool_task_delete_branch() -> RmcpTool {
    RmcpTool::new(
        "task_delete_branch".to_string(),
        "Delete the task's git branch, worktree, and paused session so the next worker starts with a clean slate.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"}
            }
        }),
    )
}

#[allow(dead_code)]
fn tool_task_archive_activity() -> RmcpTool {
    RmcpTool::new(
        "task_archive_activity".to_string(),
        "Soft-delete all activity entries (comments, session errors, rejections) for a task. The worker on the next attempt will only see post-intervention activity.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"}
            }
        }),
    )
}

#[allow(dead_code)]
fn tool_task_reset_counters() -> RmcpTool {
    RmcpTool::new(
        "task_reset_counters".to_string(),
        "Reset reopen_count and continuation_count to zero. Use when the task has been meaningfully rescoped and old retry history is no longer relevant.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"}
            }
        }),
    )
}

#[allow(dead_code)]
fn tool_task_kill_session() -> RmcpTool {
    RmcpTool::new(
        "task_kill_session".to_string(),
        "Kill the paused worker session and delete its saved conversation. The next dispatch will start a fresh session. Unlike task_delete_branch, this preserves the branch and any committed code.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"}
            }
        }),
    )
}

/// Truncate shell output to prevent blowing the context window.
/// Hard cap at 50 KB — both line count and byte size are enforced.
fn truncate_shell_output(raw: &str) -> String {
    const MAX_LINES: usize = 2000;
    const MAX_BYTES: usize = 50_000;

    if raw.len() <= MAX_BYTES && raw.split('\n').count() <= MAX_LINES {
        return raw.to_string();
    }

    let total_lines = raw.split('\n').count();
    let total_bytes = raw.len();

    // Take last lines that fit within MAX_BYTES
    let mut preview_bytes = 0;
    let mut preview_lines: Vec<&str> = Vec::new();
    for line in raw.rsplit('\n') {
        let line_bytes = line.len() + 1; // +1 for newline
        if preview_bytes + line_bytes > MAX_BYTES && !preview_lines.is_empty() {
            break;
        }
        preview_bytes += line_bytes;
        preview_lines.push(line);
    }
    preview_lines.reverse();
    let preview = preview_lines.join("\n");

    let reason = format!(
        "Output truncated to {} KB ({} lines / {} bytes total).",
        MAX_BYTES / 1000,
        total_lines,
        total_bytes
    );

    format!(
        "{preview}\n\n[{reason} Use shell commands like `head`, `tail`, or `sed -n '100,200p'` to read sections.]"
    )
}

fn from_value<T>(value: serde_json::Value) -> Result<T, serde_json::Error>
where
    T: DeserializeOwned,
{
    serde_json::from_value(value)
}

/// Returns the JSON tool schemas for the given agent type, suitable for
/// passing directly to the `LlmProvider::stream` call in the Djinn-native
/// reply loop.
pub(crate) fn tool_schemas(agent_type: AgentType) -> Vec<serde_json::Value> {
    let mut tool_values = vec![
        serde_json::to_value(tool_task_show()).expect("serialize tool_task_show"),
        serde_json::to_value(tool_task_list()).expect("serialize tool_task_list"),
        serde_json::to_value(tool_task_activity_list()).expect("serialize tool_task_activity_list"),
        serde_json::to_value(tool_task_comment_add()).expect("serialize tool_task_comment_add"),
        serde_json::to_value(tool_memory_read()).expect("serialize tool_memory_read"),
        serde_json::to_value(tool_memory_search()).expect("serialize tool_memory_search"),
    ];

    tool_values.push(serde_json::to_value(tool_shell()).expect("serialize tool_shell"));

    if matches!(agent_type, AgentType::Worker | AgentType::ConflictResolver) {
        tool_values.push(serde_json::to_value(tool_write()).expect("serialize tool_write"));
        tool_values.push(serde_json::to_value(tool_edit()).expect("serialize tool_edit"));
        tool_values
            .push(serde_json::to_value(tool_request_pm()).expect("serialize tool_request_pm"));
    }

    if matches!(agent_type, AgentType::TaskReviewer) {
        tool_values.push(
            serde_json::to_value(tool_task_update_ac()).expect("serialize tool_task_update_ac"),
        );
    }

    if matches!(agent_type, AgentType::PM | AgentType::Groomer) {
        for value in [
            serde_json::to_value(tool_task_create()).expect("serialize tool_task_create"),
            serde_json::to_value(tool_task_update()).expect("serialize tool_task_update"),
            serde_json::to_value(tool_task_transition()).expect("serialize tool_task_transition"),
            serde_json::to_value(tool_task_delete_branch())
                .expect("serialize tool_task_delete_branch"),
            serde_json::to_value(tool_task_archive_activity())
                .expect("serialize tool_task_archive_activity"),
            serde_json::to_value(tool_task_reset_counters())
                .expect("serialize tool_task_reset_counters"),
            serde_json::to_value(tool_task_kill_session())
                .expect("serialize tool_task_kill_session"),
            serde_json::to_value(tool_task_blocked_list())
                .expect("serialize tool_task_blocked_list"),
            serde_json::to_value(tool_epic_show()).expect("serialize tool_epic_show"),
            serde_json::to_value(tool_epic_update()).expect("serialize tool_epic_update"),
            serde_json::to_value(tool_epic_tasks()).expect("serialize tool_epic_tasks"),
        ] {
            tool_values.push(value);
        }
    }

    tool_values
}

/// Public entry point for the Djinn-native reply loop to call a tool by name.
///
/// `arguments` should be the `input` field from a `ContentBlock::ToolUse`
/// converted to an `Option<Map>`:
///
/// ```rust,ignore
/// let args = match input {
///     Value::Object(map) => Some(map),
///     _ => None,
/// };
/// ```
pub(crate) async fn call_tool(
    state: &AppState,
    name: &str,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
) -> Result<serde_json::Value, String> {
    let synthetic = serde_json::json!({ "name": name, "arguments": arguments });
    dispatch_tool_call(state, &synthetic, worktree_path, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floor_char_boundary_ascii() {
        assert_eq!(floor_char_boundary("hello", 3), 3);
    }

    #[test]
    fn floor_char_boundary_multibyte_interior() {
        // '─' (U+2500) is 3 bytes: E2 94 80
        let s = "─";
        assert_eq!(floor_char_boundary(s, 1), 0);
        assert_eq!(floor_char_boundary(s, 2), 0);
        assert_eq!(floor_char_boundary(s, 3), 3);
    }

    #[test]
    fn floor_char_boundary_emoji() {
        // '🔥' is 4 bytes
        let s = "🔥x";
        assert_eq!(floor_char_boundary(s, 1), 0);
        assert_eq!(floor_char_boundary(s, 2), 0);
        assert_eq!(floor_char_boundary(s, 3), 0);
        assert_eq!(floor_char_boundary(s, 4), 4);
        assert_eq!(floor_char_boundary(s, 5), 5);
    }

    #[test]
    fn floor_char_boundary_beyond_len() {
        assert_eq!(floor_char_boundary("hi", 100), 2);
    }

    #[test]
    fn floor_char_boundary_zero() {
        assert_eq!(floor_char_boundary("hello", 0), 0);
    }

    #[tokio::test]
    async fn write_rejects_symlink_escape_outside_worktree() {
        use tempfile::tempdir;

        let worktree = tempdir().expect("temp worktree");
        let outside = tempdir().expect("outside dir");
        let link = worktree.path().join("escape-link");

        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &link).expect("create symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(outside.path(), &link).expect("create symlink");

        let args = Some(
            serde_json::json!({"path":"escape-link/pwned.txt","content":"owned"})
                .as_object()
                .expect("obj")
                .clone(),
        );

        let result = call_write(&args, worktree.path()).await;
        assert!(result.is_err());
        let err = result.err().unwrap_or_default();
        assert!(err.contains("outside worktree"));
        assert!(!outside.path().join("pwned.txt").exists());
    }

    #[test]
    fn worker_cannot_use_pm_only_tool() {
        assert!(!is_tool_allowed_for_agent(
            AgentType::Worker,
            "task_transition"
        ));
        assert!(is_tool_allowed_for_agent(AgentType::PM, "task_transition"));
    }

    #[test]
    fn shell_timeout_defaults_and_minimum() {
        fn resolve_timeout(t: Option<u64>) -> u64 {
            t.unwrap_or(120_000).max(1000)
        }
        assert_eq!(resolve_timeout(None), 120_000);
        assert_eq!(resolve_timeout(Some(0)), 1000);
    }

    #[test]
    fn tool_schemas_include_role_specific_tools() {
        fn names(agent: AgentType) -> Vec<String> {
            tool_schemas(agent)
                .into_iter()
                .filter_map(|v| {
                    v.get("name")
                        .and_then(|n| n.as_str())
                        .map(ToString::to_string)
                })
                .collect()
        }

        let worker = names(AgentType::Worker);
        assert!(worker.len() > 1);
        assert!(worker.iter().any(|n| n == "shell"));
        assert!(worker.iter().any(|n| n == "write"));
        assert!(worker.iter().any(|n| n == "edit"));

        let reviewer = names(AgentType::TaskReviewer);
        assert!(reviewer.len() > 1);
        assert!(reviewer.iter().any(|n| n == "task_update_ac"));

        let pm = names(AgentType::PM);
        assert!(pm.len() > 1);
        assert!(pm.iter().any(|n| n == "task_create"));
        assert!(pm.iter().any(|n| n == "task_transition"));

        let groomer = names(AgentType::Groomer);
        assert!(groomer.len() > 1);
        assert!(groomer.iter().any(|n| n == "task_create"));
        assert!(groomer.iter().any(|n| n == "task_transition"));
    }
}
