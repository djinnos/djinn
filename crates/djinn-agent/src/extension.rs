use std::path::{Path, PathBuf};

use std::process::Stdio;

use tokio::time::Duration;

use rmcp::model::Tool as RmcpTool;
use rmcp::object;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::sandbox;
use crate::context::AgentContext;
use crate::lsp::format_diagnostics_xml;
use djinn_core::models::Task;
use djinn_db::AgentRoleRepository;
use djinn_db::EpicRepository;
use djinn_db::NoteRepository;
use djinn_db::ProjectRepository;
use djinn_db::SessionRepository;
use djinn_db::TaskRepository;

#[derive(Deserialize)]
struct IncomingToolCall {
    name: String,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Find the largest byte index <= `idx` that is a valid UTF-8 char boundary.
#[cfg(test)]
fn floor_char_boundary(s: &str, idx: usize) -> usize {
    crate::truncate::floor_char_boundary(s, idx)
}

async fn dispatch_tool_call<T>(
    state: &AgentContext,
    tool_call: &T,
    worktree_path: &Path,
    allowed_schemas: Option<&[serde_json::Value]>,
    session_task_id: Option<&str>,
) -> Result<serde_json::Value, String>
where
    T: Serialize,
{
    let call: IncomingToolCall =
        from_value(serde_json::to_value(tool_call).map_err(|e| e.to_string())?)
            .map_err(|e| format!("invalid frontend tool payload: {e}"))?;

    // Resolve project_id from worktree_path so agent tools are project-scoped.
    let project_id = {
        let repo = ProjectRepository::new(state.db.clone(), state.event_bus.clone());
        let path_str = worktree_path.to_string_lossy();
        repo.resolve(&path_str).await.ok().flatten()
    };

    if let Some(schemas) = allowed_schemas
        && !is_tool_allowed_for_schemas(schemas, &call.name)
    {
        return Err(format!(
            "tool `{}` is not in the allowed schema list",
            call.name
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
        "memory_search" => call_memory_search(state, &call.arguments, session_task_id).await,
        "memory_list" => call_memory_list(state, &call.arguments).await,
        "memory_build_context" => call_memory_build_context(state, &call.arguments, session_task_id).await,
        "role_metrics" => call_role_metrics(state, &call.arguments).await,
        "role_amend_prompt" => call_role_amend_prompt(state, &call.arguments).await,
        "shell" => call_shell(&call.arguments, worktree_path).await,
        "read" => call_read(state, &call.arguments, worktree_path).await,
        "write" => call_write(state, &call.arguments, worktree_path).await,
        "edit" => call_edit(state, &call.arguments, worktree_path).await,
        "apply_patch" => call_apply_patch(state, &call.arguments, worktree_path).await,
        "lsp" => call_lsp(state, &call.arguments, worktree_path).await,
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
    /// Specialist role name to route this task (e.g. "rust-expert").
    agent_type: Option<String>,
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
    task_id: Option<String>,
}

#[derive(Deserialize)]
struct MemoryListParams {
    project: Option<String>,
    folder: Option<String>,
    #[serde(rename = "type")]
    note_type: Option<String>,
    depth: Option<i64>,
}

#[derive(Deserialize)]
struct MemoryBuildContextParams {
    project: Option<String>,
    url: String,
    /// Link traversal depth (default 1). Currently unused at the dispatch layer.
    _depth: Option<i64>,
    max_related: Option<i64>,
    budget: Option<i64>,
    task_id: Option<String>,
}

#[derive(Deserialize)]
struct RoleMetricsParams {
    project: Option<String>,
    role_id: Option<String>,
    window_days: Option<i64>,
}

#[derive(Deserialize)]
struct RoleAmendPromptParams {
    project: Option<String>,
    role_id: String,
    amendment: String,
    metrics_snapshot: Option<String>,
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

#[derive(Deserialize)]
struct ApplyPatchParams {
    patch: String,
}

#[derive(Deserialize)]
struct ReadParams {
    #[serde(alias = "path")]
    file_path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

/// Normalize `Some("")` → `None`. OpenAI models often send empty strings
/// for optional parameters instead of omitting them, which breaks SQL filters.
fn non_empty(opt: Option<String>) -> Option<String> {
    opt.filter(|s| !s.is_empty())
}

async fn call_task_list(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: TaskListParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());

    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let query = djinn_db::ListQuery {
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
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskShowParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
    let session_repo = SessionRepository::new(state.db.clone(), state.event_bus.clone());

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
                                    *value = serde_json::json!(crate::truncate::smart_truncate(
                                        s,
                                        MAX_PAYLOAD_CHARS
                                    ));
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
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    use djinn_db::ActivityQuery;

    let p: TaskActivityListParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());

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
                        *value = serde_json::json!(crate::truncate::smart_truncate(
                            s,
                            MAX_PAYLOAD_CHARS
                        ));
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
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: EpicShowParams = parse_args(arguments)?;
    let repo = EpicRepository::new(state.db.clone(), state.event_bus.clone());

    match repo.resolve(&p.id).await {
        Ok(Some(epic)) => Ok(serde_json::to_value(epic).map_err(|e| e.to_string())?),
        Ok(None) => Ok(serde_json::json!({ "error": format!("epic not found: {}", p.id) })),
        Err(e) => Err(e.to_string()),
    }
}

async fn call_epic_update(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: EpicUpdateParams = parse_args(arguments)?;
    let repo = EpicRepository::new(state.db.clone(), state.event_bus.clone());

    let Some(epic) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("epic not found: {}", p.id) }));
    };

    let title = p.title.as_deref().unwrap_or(&epic.title);
    let description = p.description.as_deref().unwrap_or(&epic.description);
    let emoji = epic.emoji.as_str();
    let color = epic.color.as_str();
    let owner = epic.owner.as_str();

    let updated = repo
        .update(
            &epic.id,
            djinn_db::EpicUpdateInput {
                title,
                description,
                emoji,
                color,
                owner,
                memory_refs: None,
            },
        )
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
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: EpicTasksParams = parse_args(arguments)?;
    let epic_repo = EpicRepository::new(state.db.clone(), state.event_bus.clone());

    let Some(epic) = epic_repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("epic not found: {}", p.id) }));
    };

    let task_repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let offset = p.offset.unwrap_or(0).max(0);
    let mut all = task_repo
        .list_by_epic(&epic.id)
        .await
        .map_err(|e| e.to_string())?;
    let total = i64::try_from(all.len()).unwrap_or(0);
    let start = usize::try_from(offset).unwrap_or(0).min(all.len());
    let end = start
        .saturating_add(usize::try_from(limit).unwrap_or(0))
        .min(all.len());
    let tasks = all
        .drain(start..end)
        .map(|t| task_to_value(&t))
        .collect::<Vec<_>>();
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
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskCreateParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());

    let issue_type = p.issue_type.as_deref().unwrap_or("task");
    let description = p.description.as_deref().unwrap_or("");
    let design = p.design.as_deref().unwrap_or("");
    let priority = p.priority.unwrap_or(0);
    let owner = p.owner.as_deref().unwrap_or("");
    let status = match p.status.as_deref() {
        None => None,
        Some("open") => Some("open"),
        Some(other) => {
            return Err(format!("invalid status: {other:?} (expected open)"));
        }
    };

    // Resolve epic_id (accepts UUID or short_id).
    let epic_repo = EpicRepository::new(state.db.clone(), state.event_bus.clone());
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

    // Set agent_type (specialist routing) if provided.
    if let Some(ref agent_type) = p.agent_type
        && !agent_type.is_empty()
        && let Ok(t) = repo.update_agent_type(&task.id, Some(agent_type)).await
    {
        task = t;
    }

    Ok(task_to_value(&task))
}

async fn call_task_update(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskUpdateParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());

    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    let title = p.title.as_deref().unwrap_or(&task.title);
    let description = p.description.as_deref().unwrap_or(&task.description);
    let design = p.design.as_deref().unwrap_or(&task.design);
    let priority = p.priority.unwrap_or(task.priority);
    let owner = p.owner.as_deref().unwrap_or(&task.owner);

    let labels_json = if p.labels_add.is_some() || p.labels_remove.is_some() {
        let mut labels: Vec<String> = djinn_core::models::parse_json_array(&task.labels);
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
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskUpdateAcParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());

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
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    #[derive(Deserialize)]
    struct RequestPmParams {
        id: String,
        reason: String,
        suggested_breakdown: Option<String>,
    }

    let p: RequestPmParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());

    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    // Log the Lead request as a structured comment.
    let mut body = format!("[LEAD_REQUEST] {}", p.reason);
    if let Some(ref breakdown) = p.suggested_breakdown {
        body.push_str(&format!("\n\nSuggested breakdown:\n{breakdown}"));
    }
    let payload = serde_json::json!({ "body": body }).to_string();
    repo.log_activity(
        Some(&task.id),
        "worker-agent",
        "worker",
        "comment",
        &payload,
    )
    .await
    .map_err(|e| e.to_string())?;

    // Escalate to Lead intervention queue.
    let updated = repo
        .transition(
            &task.id,
            djinn_core::models::TransitionAction::Escalate,
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
        "message": "Task escalated to Lead. Your session should end now."
    }))
}

async fn call_task_comment_add(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskCommentAddParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());

    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    let payload = serde_json::json!({ "body": p.body }).to_string();
    let actor_id = p.actor_id.as_deref().unwrap_or("agent");
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
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: MemoryReadParams = parse_args(arguments)?;
    let project_path = resolve_project_path(p.project);
    let project_id = project_id_for_path(state, &project_path).await?;

    let repo = NoteRepository::new(state.db.clone(), state.event_bus.clone());
    let note = resolve_note_by_identifier(&repo, &project_id, &p.identifier)
        .await
        .ok_or_else(|| format!("note not found: {}", p.identifier))?;

    let _ = repo.touch_accessed(&note.id).await;
    Ok(note_to_value(&note))
}

async fn call_memory_search(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    session_task_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: MemorySearchParams = parse_args(arguments)?;
    let project_path = resolve_project_path(p.project);
    let project_id = project_id_for_path(state, &project_path).await?;

    // Use task_id from args if provided, otherwise fall back to session context.
    let task_id = p.task_id.as_deref().or(session_task_id);

    let repo = NoteRepository::new(state.db.clone(), state.event_bus.clone());
    let limit = p.limit.unwrap_or(10).clamp(1, 100) as usize;
    let results = repo
        .search(
            &project_id,
            &p.query,
            task_id,
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

async fn call_memory_list(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: MemoryListParams = parse_args(arguments)?;
    let project_path = resolve_project_path(p.project);
    let project_id = project_id_for_path(state, &project_path).await?;

    let repo = NoteRepository::new(state.db.clone(), state.event_bus.clone());
    let depth = p.depth.unwrap_or(1);

    let notes = repo
        .list_compact(
            &project_id,
            p.folder.as_deref(),
            p.note_type.as_deref(),
            depth,
        )
        .await
        .map_err(|e| e.to_string())?;

    let items: Vec<serde_json::Value> = notes
        .into_iter()
        .map(|n| {
            serde_json::json!({
                "id": n.id,
                "permalink": n.permalink,
                "title": n.title,
                "note_type": n.note_type,
                "folder": n.folder,
                "updated_at": n.updated_at,
            })
        })
        .collect();

    Ok(serde_json::json!({ "notes": items }))
}

async fn call_memory_build_context(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    session_task_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: MemoryBuildContextParams = parse_args(arguments)?;
    let project_path = resolve_project_path(p.project);
    let project_id = project_id_for_path(state, &project_path).await?;

    let repo = NoteRepository::new(state.db.clone(), state.event_bus.clone());
    let max_related = p.max_related.unwrap_or(10).clamp(1, 50) as usize;
    let budget = p.budget.map(|b| b as usize);
    let task_id = p.task_id.as_deref().or(session_task_id);

    // Strip memory:// prefix.
    let url = p.url.strip_prefix("memory://").unwrap_or(&p.url);

    // Wildcard: return all notes in folder as primary.
    if url.ends_with("/*") {
        let folder = url.trim_end_matches("/*");
        let all = repo
            .list(&project_id, Some(folder))
            .await
            .map_err(|e| e.to_string())?;
        return Ok(serde_json::json!({
            "primary": all.iter().map(|n| serde_json::json!({
                "id": n.id,
                "permalink": n.permalink,
                "title": n.title,
                "content": n.content,
            })).collect::<Vec<_>>(),
            "related_l1": [],
            "related_l0": [],
        }));
    }

    match repo
        .build_context(&project_id, url, budget, task_id, max_related)
        .await
    {
        Ok(result) => Ok(serde_json::json!({
            "primary": result.primary.iter().map(|n| serde_json::json!({
                "id": n.id,
                "permalink": n.permalink,
                "title": n.title,
                "content": n.content,
            })).collect::<Vec<_>>(),
            "related_l1": result.related_l1.iter().map(|n| serde_json::json!({
                "id": n.id,
                "permalink": n.permalink,
                "title": n.title,
                "overview": n.overview_text,
            })).collect::<Vec<_>>(),
            "related_l0": result.related_l0.iter().map(|n| serde_json::json!({
                "id": n.id,
                "permalink": n.permalink,
                "title": n.title,
                "abstract": n.abstract_text,
            })).collect::<Vec<_>>(),
        })),
        Err(e) => Err(e.to_string()),
    }
}

async fn call_role_metrics(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: RoleMetricsParams = parse_args(arguments)?;
    let project_path = resolve_project_path(p.project);
    let project_id = project_id_for_path(state, &project_path).await?;
    let window_days = p.window_days.unwrap_or(30).max(1);

    let repo = AgentRoleRepository::new(state.db.clone(), state.event_bus.clone());

    let roles: Vec<djinn_core::models::AgentRole> = if let Some(ref id_or_name) = p.role_id {
        let role = repo.get(id_or_name).await.map_err(|e| e.to_string())?;
        let role = match role {
            Some(r) if r.project_id == project_id => Some(r),
            _ => repo
                .get_by_name_for_project(&project_id, id_or_name)
                .await
                .map_err(|e| e.to_string())?,
        };
        match role {
            Some(r) => vec![r],
            None => return Err(format!("agent_role not found: {id_or_name}")),
        }
    } else {
        repo.list_for_project(djinn_db::AgentRoleListQuery {
            project_id: project_id.clone(),
            base_role: None,
            limit: 200,
            offset: 0,
        })
        .await
        .map_err(|e| e.to_string())?
        .roles
    };

    let mut entries = Vec::with_capacity(roles.len());
    for role in &roles {
        let agent_type = match role.base_role.as_str() {
            "worker" | "resolver" => "worker",
            "reviewer" => "reviewer",
            "planner" => "planner",
            "lead" => "lead",
            other => other,
        };
        let m = repo
            .get_metrics(&project_id, agent_type, window_days)
            .await
            .unwrap_or(djinn_db::AgentRoleMetrics {
                success_rate: 0.0,
                avg_reopens: 0.0,
                verification_pass_rate: 0.0,
                completed_task_count: 0,
                avg_tokens: 0.0,
                avg_time_seconds: 0.0,
            });
        entries.push(serde_json::json!({
            "role_id": role.id,
            "role_name": role.name,
            "base_role": role.base_role,
            "success_rate": m.success_rate,
            "avg_reopens": m.avg_reopens,
            "verification_pass_rate": m.verification_pass_rate,
            "completed_task_count": m.completed_task_count,
            "avg_tokens": m.avg_tokens,
            "avg_time_seconds": m.avg_time_seconds,
        }));
    }

    Ok(serde_json::json!({
        "roles": entries,
        "window_days": window_days,
    }))
}

async fn call_role_amend_prompt(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: RoleAmendPromptParams = parse_args(arguments)?;
    let project_path = resolve_project_path(p.project);
    let project_id = project_id_for_path(state, &project_path).await?;

    let repo = AgentRoleRepository::new(state.db.clone(), state.event_bus.clone());

    // Resolve role by UUID or name.
    let role = {
        let by_id = repo.get(&p.role_id).await.map_err(|e| e.to_string())?;
        match by_id {
            Some(r) if r.project_id == project_id => Some(r),
            _ => repo
                .get_by_name_for_project(&project_id, &p.role_id)
                .await
                .map_err(|e| e.to_string())?,
        }
    };

    let role = role.ok_or_else(|| format!("agent_role not found: {}", p.role_id))?;

    // Only allow amending specialist roles. Prevents patching high-level orchestration roles.
    if matches!(role.base_role.as_str(), "architect" | "lead" | "planner") {
        return Err(format!(
            "cannot amend learned_prompt for base_role '{}'; only specialist roles (worker, reviewer, resolver) are eligible",
            role.base_role
        ));
    }

    let updated = repo
        .append_learned_prompt(&role.id, &p.amendment, p.metrics_snapshot.as_deref())
        .await
        .map_err(|e| e.to_string())?;

    Ok(serde_json::json!({
        "role_id": updated.id,
        "role_name": updated.name,
        "learned_prompt": updated.learned_prompt,
        "updated_at": updated.updated_at,
        "amendment_appended": true,
    }))
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
    crate::process::isolate_process_group(&mut cmd);
    let output = crate::process::output_with_kill(cmd, Duration::from_millis(timeout_ms))
        .await
        .map_err(|e| format!("failed to run shell command: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    Ok(serde_json::json!({
        "ok": output.status.success(),
        "exit_code": output.status.code(),
        "stdout": stdout,
        "stderr": stderr,
        "workdir": worktree_path,
    }))
}

async fn call_read(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
) -> Result<serde_json::Value, String> {
    let p: ReadParams = parse_args(arguments)?;
    let path = resolve_path(&p.file_path, worktree_path);
    ensure_path_within_worktree(&path, worktree_path)?;

    let bytes = tokio::fs::read(&path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            let parent = path.parent().unwrap_or(worktree_path);
            let suggestions = std::fs::read_dir(parent)
                .ok()
                .into_iter()
                .flat_map(|it| it.filter_map(Result::ok))
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|name| !name.is_empty())
                .take(10)
                .collect::<Vec<_>>();
            if suggestions.is_empty() {
                format!("file not found: {}", path.display())
            } else {
                format!(
                    "file not found: {}. similar filenames: {}",
                    path.display(),
                    suggestions.join(", ")
                )
            }
        } else {
            format!("read failed: {e}")
        }
    })?;

    if bytes.contains(&0) {
        return Err(format!("refusing to read binary file: {}", path.display()));
    }

    let text = String::from_utf8(bytes)
        .map_err(|_| format!("refusing to read binary file: {}", path.display()))?;
    let all_lines: Vec<String> = text
        .lines()
        .map(|line| {
            if line.chars().count() > 2000 {
                line.chars().take(2000).collect::<String>()
            } else {
                line.to_string()
            }
        })
        .collect();

    let offset = p.offset.unwrap_or(0);
    let limit = p.limit.unwrap_or(2000).min(2000);
    let start = offset.min(all_lines.len());
    let end = start.saturating_add(limit).min(all_lines.len());

    let mut numbered = String::new();
    for (i, line) in all_lines[start..end].iter().enumerate() {
        let line_no = start + i + 1;
        numbered.push_str(&format!("{:>6}\t{}\n", line_no, line));
    }

    state
        .file_time
        .read(&worktree_path.display().to_string(), &path)
        .await?;

    Ok(serde_json::json!({
        "path": path.display().to_string(),
        "offset": start,
        "limit": limit,
        "total_lines": all_lines.len(),
        "has_more": end < all_lines.len(),
        "content": numbered,
    }))
}

async fn call_write(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
) -> Result<serde_json::Value, String> {
    let p: WriteParams = parse_args(arguments)?;
    let path = resolve_path(&p.path, worktree_path);

    // Ensure path is within worktree
    ensure_path_within_worktree(&path, worktree_path)?;

    state
        .file_time
        .with_lock(&path, async {
            if path.exists() {
                state
                    .file_time
                    .assert(&worktree_path.display().to_string(), &path)
                    .await
                    .map_err(|e| match e.as_str() {
                        _ if e.starts_with(
                            "file must be read before modification in this session:",
                        ) =>
                        {
                            format!(
                                "You must read the file {} before overwriting it. Use the read tool first",
                                path.display()
                            )
                        }
                        _ if e.starts_with(
                            "file was modified since last read in this session:",
                        ) =>
                        {
                            format!(
                                "File {} has been modified since last read. Please read it again.",
                                path.display()
                            )
                        }
                        _ => e,
                    })?;
            }

            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("create dirs failed: {e}"))?;
            }
            tokio::fs::write(&path, &p.content)
                .await
                .map_err(|e| format!("write failed: {e}"))?;

            state
                .file_time
                .read(&worktree_path.display().to_string(), &path)
                .await?;

            state.lsp.touch_file(worktree_path, &path, true).await;
            let diag_xml = format_diagnostics_xml(state.lsp.diagnostics(worktree_path).await);

            Ok(serde_json::json!({
                "ok": true,
                "path": path.display().to_string(),
                "bytes": p.content.len(),
                "diagnostics": diag_xml,
            }))
        })
        .await
}

/// Multi-layer fuzzy string replacement for the edit tool.
///
/// Tries matching strategies in order of strictness:
/// 1. Exact match
/// 2. Line-trimmed match (trailing whitespace stripped per line)
/// 3. Whitespace-normalized match (runs of whitespace collapsed to single space)
/// 4. Indentation-flexible match (leading whitespace stripped per line)
///
/// Returns `(new_content, optional_match_note)`.
fn fuzzy_replace(
    content: &str,
    old_text: &str,
    new_text: &str,
    path: &Path,
) -> Result<(String, Option<String>), String> {
    // Layer 1: Exact match
    let count = content.matches(old_text).count();
    if count == 1 {
        return Ok((content.replacen(old_text, new_text, 1), None));
    }
    if count > 1 {
        return Err(format!(
            "old_text appears {count} times in file (must be unique): {}",
            path.display()
        ));
    }

    // Layer 2: Line-trimmed match (trim trailing whitespace per line)
    if let Some(result) = try_line_trimmed_match(content, old_text, new_text) {
        return match result {
            FuzzyResult::Unique(new_content) => Ok((
                new_content,
                Some("(matched after trimming trailing whitespace)".to_string()),
            )),
            FuzzyResult::Ambiguous(n) => Err(format!(
                "old_text appears {n} times after trimming trailing whitespace \
                 (must be unique): {}",
                path.display()
            )),
        };
    }

    // Layer 3: Whitespace-normalized match
    if let Some(result) = try_whitespace_normalized_match(content, old_text, new_text) {
        return match result {
            FuzzyResult::Unique(new_content) => Ok((
                new_content,
                Some("(matched with whitespace normalization)".to_string()),
            )),
            FuzzyResult::Ambiguous(n) => Err(format!(
                "old_text appears {n} times after whitespace normalization \
                 (must be unique): {}",
                path.display()
            )),
        };
    }

    // Layer 4: Indentation-flexible match
    if let Some(result) = try_indentation_flexible_match(content, old_text, new_text) {
        return match result {
            FuzzyResult::Unique(new_content) => Ok((
                new_content,
                Some("(matched with flexible indentation)".to_string()),
            )),
            FuzzyResult::Ambiguous(n) => Err(format!(
                "old_text appears {n} times after stripping indentation \
                 (must be unique): {}",
                path.display()
            )),
        };
    }

    Err(format!("old_text not found in file: {}", path.display()))
}

enum FuzzyResult {
    Unique(String),
    Ambiguous(usize),
}

/// Trim trailing whitespace from each line, then find the match.
fn try_line_trimmed_match(content: &str, old_text: &str, new_text: &str) -> Option<FuzzyResult> {
    let trimmed_content: String = content
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed_old: String = old_text
        .lines()
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");

    let count = trimmed_content.matches(&trimmed_old as &str).count();
    if count == 0 {
        return None;
    }
    if count > 1 {
        return Some(FuzzyResult::Ambiguous(count));
    }

    let start = trimmed_content.find(&trimmed_old)?;
    let end = start + trimmed_old.len();

    let (orig_start, orig_end) = map_trimmed_to_original(content, &trimmed_content, start, end);
    let mut result = String::with_capacity(content.len());
    result.push_str(&content[..orig_start]);
    result.push_str(new_text);
    result.push_str(&content[orig_end..]);
    Some(FuzzyResult::Unique(result))
}

fn reindent_replacement(matched_block: &str, replacement: &str) -> String {
    let matched_lines: Vec<&str> = matched_block.split('\n').collect();
    let replacement_lines: Vec<&str> = replacement.split('\n').collect();

    if replacement_lines.is_empty() {
        return String::new();
    }

    let matched_base_indent = matched_lines
        .iter()
        .find(|line| !line.trim().is_empty())
        .map_or("", |line| leading_whitespace(line));

    let replacement_base_indent = replacement_lines
        .iter()
        .find(|line| !line.trim().is_empty())
        .map_or("", |line| leading_whitespace(line));

    replacement_lines
        .iter()
        .map(|line| {
            if line.is_empty() {
                return String::new();
            }

            let replacement_indent = leading_whitespace(line);
            let relative_indent = replacement_indent
                .strip_prefix(replacement_base_indent)
                .unwrap_or(replacement_indent);

            format!(
                "{matched_base_indent}{relative_indent}{}",
                &line[replacement_indent.len()..]
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn leading_whitespace(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

/// Map byte positions from a trimmed version back to the original content.
fn map_trimmed_to_original(
    original: &str,
    trimmed: &str,
    trimmed_start: usize,
    trimmed_end: usize,
) -> (usize, usize) {
    let orig_lines: Vec<&str> = original.split('\n').collect();
    let trimmed_lines: Vec<&str> = trimmed.split('\n').collect();

    let mut orig_offset = 0usize;
    let mut trimmed_offset = 0usize;
    let mut result_start = 0usize;
    let mut result_end = 0usize;
    let mut found_start = false;
    let mut found_end = false;

    for (i, (orig_line, trimmed_line)) in orig_lines.iter().zip(trimmed_lines.iter()).enumerate() {
        let newline: usize = usize::from(i < orig_lines.len() - 1);

        if !found_start && trimmed_start < trimmed_offset + trimmed_line.len() + newline {
            let offset_in_line = trimmed_start - trimmed_offset;
            result_start = orig_offset + offset_in_line;
            found_start = true;
        }

        if !found_end && trimmed_end <= trimmed_offset + trimmed_line.len() + newline {
            let offset_in_line = trimmed_end - trimmed_offset;
            let clamped = offset_in_line.min(orig_line.len() + newline);
            result_end = orig_offset + clamped;
            found_end = true;
        }

        orig_offset += orig_line.len() + newline;
        trimmed_offset += trimmed_line.len() + newline;

        if found_start && found_end {
            break;
        }
    }

    (result_start, result_end)
}

/// Collapse all runs of spaces/tabs to a single space, then find the match.
fn try_whitespace_normalized_match(
    content: &str,
    old_text: &str,
    new_text: &str,
) -> Option<FuzzyResult> {
    let (norm_content, content_map) = normalize_whitespace_with_map(content);
    let (norm_old, _) = normalize_whitespace_with_map(old_text);

    let count = norm_content.matches(&norm_old as &str).count();
    if count == 0 {
        return None;
    }
    if count > 1 {
        return Some(FuzzyResult::Ambiguous(count));
    }

    let norm_start = norm_content.find(&norm_old)?;
    let norm_end = norm_start + norm_old.len();

    let orig_start = content_map[norm_start];
    let orig_end = if norm_end >= content_map.len() {
        content.len()
    } else {
        content_map[norm_end]
    };

    let mut result = String::with_capacity(content.len());
    result.push_str(&content[..orig_start]);
    result.push_str(new_text);
    result.push_str(&content[orig_end..]);
    Some(FuzzyResult::Unique(result))
}

/// Normalize whitespace: collapse runs of spaces/tabs to a single space.
/// Returns (normalized_string, map from normalized byte index to original byte
/// index).
fn normalize_whitespace_with_map(s: &str) -> (String, Vec<usize>) {
    let mut normalized = String::with_capacity(s.len());
    let mut map: Vec<usize> = Vec::with_capacity(s.len());
    let mut in_ws = false;
    let bytes = s.as_bytes();

    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' || b == b'\r' {
            in_ws = false;
            normalized.push(b as char);
            map.push(i);
        } else if b == b' ' || b == b'\t' {
            if !in_ws {
                normalized.push(' ');
                map.push(i);
                in_ws = true;
            }
        } else {
            in_ws = false;
            normalized.push(b as char);
            map.push(i);
        }
    }

    (normalized, map)
}

/// Strip leading whitespace from each line, match, then apply edit preserving
/// the file's original indentation.
fn try_indentation_flexible_match(
    content: &str,
    old_text: &str,
    new_text: &str,
) -> Option<FuzzyResult> {
    let stripped_content: String = content
        .lines()
        .map(|l| l.trim_start())
        .collect::<Vec<_>>()
        .join("\n");
    let stripped_old: String = old_text
        .lines()
        .map(|l| l.trim_start())
        .collect::<Vec<_>>()
        .join("\n");

    if stripped_old.is_empty() {
        return None;
    }

    let count = stripped_content.matches(&stripped_old as &str).count();
    if count == 0 {
        return None;
    }
    if count > 1 {
        return Some(FuzzyResult::Ambiguous(count));
    }

    let stripped_start = stripped_content.find(&stripped_old)?;

    let match_start_line = stripped_content[..stripped_start]
        .chars()
        .filter(|&c| c == '\n')
        .count();
    let old_line_count = stripped_old.chars().filter(|&c| c == '\n').count() + 1;

    let content_lines: Vec<&str> = content.lines().collect();

    let mut orig_start = 0usize;
    for line in &content_lines[..match_start_line] {
        orig_start += line.len() + 1;
    }
    let mut orig_end = orig_start;
    for (i, line) in content_lines[match_start_line..]
        .iter()
        .enumerate()
        .take(old_line_count)
    {
        orig_end += line.len();
        if match_start_line + i + 1 < content_lines.len() {
            orig_end += 1;
        }
    }
    orig_end = orig_end.min(content.len());

    let matched_block = &content[orig_start..orig_end];
    let reindented = reindent_replacement(matched_block, new_text);

    let needs_trailing_newline = content[..orig_end].ends_with('\n') && !reindented.ends_with('\n');

    let mut result = String::with_capacity(content.len());
    result.push_str(&content[..orig_start]);
    result.push_str(&reindented);
    if needs_trailing_newline {
        result.push('\n');
    }
    result.push_str(&content[orig_end..]);
    Some(FuzzyResult::Unique(result))
}

async fn call_edit(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
) -> Result<serde_json::Value, String> {
    let p: EditParams = parse_args(arguments)?;
    let path = resolve_path(&p.path, worktree_path);

    // Ensure path is within worktree
    ensure_path_within_worktree(&path, worktree_path)?;

    state
        .file_time
        .with_lock(&path, async {
            state
                .file_time
                .assert(&worktree_path.display().to_string(), &path)
                .await
                .map_err(|e| match e.as_str() {
                    _ if e
                        .starts_with("file must be read before modification in this session:") =>
                    {
                        format!(
                            "You must read the file {} before editing it. Use the read tool first",
                            path.display()
                        )
                    }
                    _ if e.starts_with("file was modified since last read in this session:") => {
                        format!(
                            "File {} has been modified since last read. Please read it again.",
                            path.display()
                        )
                    }
                    _ => e,
                })?;

            let content = tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| format!("read failed: {e}"))?;

            let (new_content, match_note) =
                fuzzy_replace(&content, &p.old_text, &p.new_text, &path)?;
            tokio::fs::write(&path, &new_content)
                .await
                .map_err(|e| format!("write failed: {e}"))?;

            state
                .file_time
                .read(&worktree_path.display().to_string(), &path)
                .await?;

            state.lsp.touch_file(worktree_path, &path, true).await;
            let diag_xml = format_diagnostics_xml(state.lsp.diagnostics(worktree_path).await);

            let mut result = serde_json::json!({
                "ok": true,
                "path": path.display().to_string(),
                "diagnostics": diag_xml,
            });
            if let Some(note) = match_note {
                result["match_note"] = serde_json::Value::String(note);
            }
            Ok(result)
        })
        .await
}

async fn call_apply_patch(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
) -> Result<serde_json::Value, String> {
    let p: ApplyPatchParams = parse_args(arguments)?;

    // Parse the custom patch format
    let parsed = super::patch::parse_patch(&p.patch)?;

    let worktree_key = worktree_path.display().to_string();

    // Validate all paths are within worktree and assert FileTime for updates/deletes
    for op in &parsed.operations {
        let raw_path = op.path();
        let resolved = resolve_path(raw_path, worktree_path);
        ensure_path_within_worktree(&resolved, worktree_path)?;

        match op {
            super::patch::FileOp::Update { .. } | super::patch::FileOp::Delete { .. } => {
                state
                    .file_time
                    .assert(&worktree_key, &resolved)
                    .await
                    .map_err(|e| {
                        if e.starts_with("file must be read before modification in this session:") {
                            format!(
                                "You must read the file {} before editing it. \
                                 Use the read tool first",
                                resolved.display()
                            )
                        } else if e
                            .starts_with("file was modified since last read in this session:")
                        {
                            format!(
                                "File {} has been modified since last read. \
                                 Please read it again.",
                                resolved.display()
                            )
                        } else {
                            e
                        }
                    })?;
            }
            super::patch::FileOp::Add { .. } => {
                // New files don't need FileTime assertion
            }
        }
    }

    // Apply all patch operations
    let results = super::patch::apply_patch(&parsed, worktree_path).await?;

    // Update FileTime and notify LSP for each affected file
    let mut affected = Vec::new();
    for (file_path, action) in &results {
        if *action != "deleted" {
            state.file_time.read(&worktree_key, file_path).await?;
            state.lsp.touch_file(worktree_path, file_path, true).await;
        }
        affected.push(serde_json::json!({
            "path": file_path.display().to_string(),
            "action": action,
        }));
    }

    let diag_xml = format_diagnostics_xml(state.lsp.diagnostics(worktree_path).await);

    Ok(serde_json::json!({
        "ok": true,
        "files": affected,
        "diagnostics": diag_xml,
    }))
}

#[derive(Deserialize)]
struct LspParams {
    operation: String,
    file_path: String,
    line: Option<u32>,
    character: Option<u32>,
}

async fn call_lsp(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
) -> Result<serde_json::Value, String> {
    let p: LspParams = parse_args(arguments)?;
    let path = resolve_path(&p.file_path, worktree_path);

    match p.operation.as_str() {
        "hover" => {
            let line = p.line.ok_or("line is required for hover")?;
            let character = p.character.ok_or("character is required for hover")?;
            // LSP uses 0-based positions; accept 1-based from agents
            let result = state
                .lsp
                .hover(
                    worktree_path,
                    &path,
                    line.saturating_sub(1),
                    character.saturating_sub(1),
                )
                .await?;
            Ok(serde_json::json!({ "operation": "hover", "result": result }))
        }
        "definition" => {
            let line = p.line.ok_or("line is required for definition")?;
            let character = p.character.ok_or("character is required for definition")?;
            let result = state
                .lsp
                .go_to_definition(
                    worktree_path,
                    &path,
                    line.saturating_sub(1),
                    character.saturating_sub(1),
                )
                .await?;
            Ok(serde_json::json!({ "operation": "definition", "result": result }))
        }
        "references" => {
            let line = p.line.ok_or("line is required for references")?;
            let character = p.character.ok_or("character is required for references")?;
            let result = state
                .lsp
                .find_references(
                    worktree_path,
                    &path,
                    line.saturating_sub(1),
                    character.saturating_sub(1),
                )
                .await?;
            Ok(serde_json::json!({ "operation": "references", "result": result }))
        }
        "symbols" => {
            let result = state.lsp.document_symbols(worktree_path, &path).await?;
            Ok(serde_json::json!({ "operation": "symbols", "result": result }))
        }
        other => Err(format!(
            "unknown LSP operation: {other}. Use: hover, definition, references, or symbols"
        )),
    }
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

fn is_tool_allowed_for_schemas(schemas: &[serde_json::Value], name: &str) -> bool {
    schemas
        .iter()
        .any(|schema| schema.get("name").and_then(|n| n.as_str()) == Some(name))
}

#[cfg(test)]
fn is_tool_allowed_for_agent(agent_type: super::AgentType, name: &str) -> bool {
    let schemas = agent_type.tool_schemas();
    is_tool_allowed_for_schemas(&schemas, name)
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

async fn project_id_for_path(state: &AgentContext, project_path: &str) -> Result<String, String> {
    let repo = ProjectRepository::new(state.db.clone(), state.event_bus.clone());
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
) -> Option<djinn_core::models::Note> {
    repo.resolve(project_id, identifier).await.ok().flatten()
}

fn note_to_value(note: &djinn_core::models::Note) -> serde_json::Value {
    note.to_value()
}

fn task_to_value(t: &Task) -> serde_json::Value {
    let labels = djinn_core::models::parse_json_array(&t.labels);
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
        "agent_type": t.agent_type,
    })
}

// ── Lead-only tool params and handlers ────────────────────────────────────────

#[derive(Deserialize)]
struct TaskTransitionParams {
    id: String,
    action: String,
    reason: Option<String>,
    target_status: Option<String>,
    /// Required when action = "force_close". UUIDs or short IDs of replacement
    /// tasks the Lead created before closing this one.
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
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    use crate::task_merge::{PM_MERGE_ACTIONS, VerificationGateFn, merge_and_transition};
    use djinn_core::models::{TaskStatus, TransitionAction};
    let p: TaskTransitionParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
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

        let gate_state = state.clone();
        let gate: VerificationGateFn = Box::new(move |task_id: String, project_path: String| {
            let s = gate_state.clone();
            Box::pin(async move {
                crate::actors::slot::verification::run_verification_gate(
                    &task_id,
                    &project_path,
                    &s,
                )
                .await
            })
        });
        let (merge_action, reason) =
            merge_and_transition(&task.id, state, &PM_MERGE_ACTIONS, Some(gate))
                .await
                .unwrap_or((
                    TransitionAction::PmInterventionComplete,
                    Some("merge_and_transition returned None".to_string()),
                ));
        let updated = repo
            .transition(
                &task.id,
                merge_action,
                "lead-agent",
                "lead",
                reason.as_deref(),
                None,
            )
            .await
            .map_err(|e| e.to_string())?;
        return Ok(task_to_value(&updated));
    }

    // Guard: force_close requires either replacement_task_ids (for decomposition)
    // or a reason (for redundant/already-landed tasks). This prevents the Lead from
    // silently closing tasks without explanation while still allowing closure of
    // tasks whose work already landed on main.
    if action == TransitionAction::ForceClose {
        let has_replacements = p
            .replacement_task_ids
            .as_ref()
            .is_some_and(|ids| !ids.is_empty());
        let has_reason = p.reason.as_ref().is_some_and(|r: &String| !r.is_empty());

        if !has_replacements && !has_reason {
            return Ok(serde_json::json!({
                "error": "force_close requires either replacement_task_ids (array of subtask IDs created as replacements) or a reason string explaining why the task is being closed (e.g. work already landed on main, task is redundant)."
            }));
        }

        // Validate replacement task IDs if provided (skip empty arrays)
        if let Some(ref ids) = p.replacement_task_ids
            && !ids.is_empty()
        {
            let mut missing = Vec::new();
            for id in ids {
                match repo.resolve(id).await {
                    Ok(Some(t))
                        if t.status == TaskStatus::Open.as_str()
                            || t.status == TaskStatus::Closed.as_str() => {}
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
                        "force_close replacement tasks must exist and be open or closed. Problems: {}",
                        missing.join(", ")
                    )
                }));
            }

            // Auto-transfer downstream blocker edges: any task that was blocked by
            // the closing task should now be blocked by the last replacement task.
            // This prevents premature dispatch when force_close auto-resolves blockers
            // on the transition that follows.
            let last_replacement_id = ids.last().unwrap();
            if let Ok(Some(last_task)) = repo.resolve(last_replacement_id).await {
                let downstream = repo.list_blocked_by(&task.id).await.unwrap_or_default();
                for blocked_ref in &downstream {
                    let _ = repo.add_blocker(&blocked_ref.task_id, &last_task.id).await;
                }
            }
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
            "lead-agent",
            "lead",
            p.reason.as_deref(),
            target,
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(task_to_value(&updated))
}

async fn call_task_delete_branch(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskDeleteBranchParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    // Interrupt the paused worker session record.
    crate::task_merge::interrupt_paused_worker_session(&task.id, state).await;

    // Resolve project dir — needed for teardown_worktree.
    let project_dir =
        match crate::task_merge::resolve_project_path_for_id(&task.project_id, state).await {
            Some(p) => std::path::PathBuf::from(p),
            None => return Ok(serde_json::json!({ "error": "project not found" })),
        };

    // Tear down: LSP shutdown → worktree removal → branch deletion (correct order).
    let worktree_path = project_dir
        .join(".djinn")
        .join("worktrees")
        .join(&task.short_id);
    let base_branch = format!("task/{}", task.short_id);
    crate::actors::slot::teardown_worktree(
        &task.short_id,
        &worktree_path,
        &project_dir,
        state,
        true,
    )
    .await;

    Ok(serde_json::json!({
        "ok": true,
        "task_id": task.short_id,
        "branch_deleted": base_branch,
    }))
}

async fn call_task_archive_activity(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskArchiveActivityParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
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
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskResetCountersParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };
    sqlx::query(
        "UPDATE tasks SET reopen_count = 0, continuation_count = 0, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1"
    )
    .bind(&task.id)
    .execute(state.db.pool())
    .await
    .map_err(|e| e.to_string())?;
    let updated = repo
        .get(&task.id)
        .await
        .map_err(|e| e.to_string())?
        .unwrap_or(task.clone());
    state
        .event_bus
        .send(djinn_core::events::DjinnEventEnvelope::task_updated(
            &updated, false,
        ));
    Ok(
        serde_json::json!({ "ok": true, "task_id": task.short_id, "reopen_count": 0, "continuation_count": 0 }),
    )
}

#[derive(Deserialize)]
struct TaskKillSessionParams {
    id: String,
}

async fn call_task_kill_session(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskKillSessionParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    // Interrupt the paused session record; clean up the worktree without deleting the branch
    // so the task can resume on the same branch next dispatch.
    crate::task_merge::interrupt_paused_worker_session(&task.id, state).await;
    if let Some(project_path_str) =
        crate::task_merge::resolve_project_path_for_id(&task.project_id, state).await
    {
        let project_dir = std::path::PathBuf::from(&project_path_str);
        let worktree_path = project_dir
            .join(".djinn")
            .join("worktrees")
            .join(&task.short_id);
        crate::actors::slot::teardown_worktree(
            &task.short_id,
            &worktree_path,
            &project_dir,
            state,
            false,
        )
        .await;
    }

    Ok(serde_json::json!({
        "ok": true,
        "task_id": task.short_id,
        "message": "Paused session interrupted and worktree cleaned up. Next dispatch will start a fresh session."
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
        "Update epic fields (title/description) and accept memory ref delta args for planner workflows.".to_string(),
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
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: TaskShowParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
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
        "Query a task's activity log with optional filters. Returns comments, status transitions, verification results, and other events. Use to inspect Lead guidance, reviewer feedback, or verification history.".to_string(),
        object!({
            "type": "object",
            "required": ["id"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"},
                "event_type": {"type": "string", "description": "Filter by event type: comment, status_changed, commands_run, merge_conflict, task_review_start"},
                "actor_role": {"type": "string", "description": "Filter by actor: lead, reviewer, worker, verification, system"},
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

fn tool_request_pm() -> RmcpTool {
    RmcpTool::new(
        "request_pm".to_string(),
        "Request Lead intervention for the current task. Use when the task is too large to complete reliably, the design is ambiguous, or you are stuck. Adds a comment with your reason and suggested breakdown, then escalates to the Lead queue. Your session will effectively end after this call."
            .to_string(),
        object!({
            "type": "object",
            "required": ["id", "reason"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short_id"},
                "reason": {"type": "string", "description": "Why Lead intervention is needed (e.g. task too large, design ambiguous, blocked on decision)"},
                "suggested_breakdown": {"type": "string", "description": "Optional suggested split: list of smaller tasks the Lead should create"}
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
                "limit": {"type": "integer"},
                "task_id": {"type": "string", "description": "Task ID for affinity scoring; defaults to the current session task"}
            }
        }),
    )
}

fn tool_memory_list() -> RmcpTool {
    RmcpTool::new(
        "memory_list".to_string(),
        "List notes in project memory. Returns compact summaries without full content.".to_string(),
        object!({
            "type": "object",
            "properties": {
                "project": {"type": "string", "description": "Absolute project path"},
                "folder": {"type": "string", "description": "Filter by folder (e.g. \"decisions\")"},
                "type": {"type": "string", "description": "Filter by note type (e.g. \"adr\", \"reference\", \"research\")"},
                "depth": {"type": "integer", "description": "Depth control: 0 = unlimited, 1 = exact folder (default), N = N levels"}
            }
        }),
    )
}

fn tool_memory_build_context() -> RmcpTool {
    RmcpTool::new(
        "memory_build_context".to_string(),
        "Build context from a memory note with progressive disclosure. Returns full content for primary notes, overview for direct linked notes, abstract for discovered related notes. Use url='folder/*' to return all notes in a folder. Seed notes are never dropped by budget constraints.".to_string(),
        object!({
            "type": "object",
            "required": ["url"],
            "properties": {
                "project": {"type": "string", "description": "Absolute project path"},
                "url": {"type": "string", "description": "Memory URI: 'memory://folder/note', 'folder/note', or 'folder/*' for all notes in a folder"},
                "depth": {"type": "integer", "description": "Link traversal depth (default 1)"},
                "max_related": {"type": "integer", "description": "Maximum related notes to return (default 10)"},
                "budget": {"type": "integer", "description": "Token budget for context (default 4096)"},
                "task_id": {"type": "string", "description": "Task ID for affinity scoring"}
            }
        }),
    )
}

fn tool_role_metrics() -> RmcpTool {
    RmcpTool::new(
        "role_metrics".to_string(),
        "Return aggregated effectiveness metrics per agent role: success_rate, avg_tokens, avg_time_seconds, verification_pass_rate, avg_reopens, completed_task_count. Omit role_id to return metrics for all roles in the project.".to_string(),
        object!({
            "type": "object",
            "properties": {
                "project": {"type": "string", "description": "Absolute project path"},
                "role_id": {"type": "string", "description": "Role UUID or name; omit for all roles"},
                "window_days": {"type": "integer", "description": "Days back for session data (default 30)"}
            }
        }),
    )
}

fn tool_role_amend_prompt() -> RmcpTool {
    RmcpTool::new(
        "role_amend_prompt".to_string(),
        "Append a prompt amendment to a specialist agent role's learned_prompt. The amendment is appended after existing content (never replacing it) and logged to learned_prompt_history. Only applicable to specialist roles (worker, reviewer, resolver base_role). Do NOT use on architect, lead, or planner roles.".to_string(),
        object!({
            "type": "object",
            "required": ["role_id", "amendment"],
            "properties": {
                "project": {"type": "string", "description": "Absolute project path"},
                "role_id": {"type": "string", "description": "Role UUID or name to amend"},
                "amendment": {"type": "string", "description": "Amendment text to append to learned_prompt"},
                "metrics_snapshot": {"type": "string", "description": "JSON string of current metrics for the history record"}
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

fn tool_read() -> RmcpTool {
    RmcpTool::new(
        "read".to_string(),
        "Read a file with line numbers and pagination. Rejects binary files.".to_string(),
        object!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string" },
                "offset": { "type": "integer", "minimum": 0 },
                "limit": { "type": "integer", "minimum": 1 }
            },
            "required": ["file_path"]
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
                "status": {"type": "string", "description": "Optional initial status. Allowed: open (default)."},
                "acceptance_criteria": {"type": "array", "items": {"type": "string"}, "description": "List of acceptance criteria strings."},
                "blocked_by": {"type": "array", "items": {"type": "string"}, "description": "Task IDs (UUID or short_id) that block this task."},
                "memory_refs": {"type": "array", "items": {"type": "string"}, "description": "Memory note permalinks to attach."},
                "agent_type": {"type": "string", "description": "Specialist role name to route this task (e.g. 'rust-expert'). Must match a configured AgentRole name for this project."}
            }
        }),
    )
}

fn tool_task_update() -> RmcpTool {
    RmcpTool::new(
        "task_update".to_string(),
        "Update task fields: title, description, design, priority, owner, labels, acceptance_criteria, memory_refs, blocked_by.".to_string(),
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
                "memory_refs_remove": {"type": "array", "items": {"type": "string"}},
                "blocked_by_add": {"type": "array", "items": {"type": "string"}, "description": "Task IDs to add as blockers"},
                "blocked_by_remove": {"type": "array", "items": {"type": "string"}, "description": "Task IDs to remove as blockers"}
            }
        }),
    )
}

fn tool_task_transition() -> RmcpTool {
    RmcpTool::new(
        "task_transition".to_string(),
        "Execute a state machine transition on a task. Lead can use: pm_intervention_complete (rescope and reopen for worker), pm_approve (approve implementation and merge), force_close (requires replacement_task_ids for decomposition OR reason for redundant/already-landed tasks), escalate.".to_string(),
        object!({
            "type": "object",
            "required": ["id", "action"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"},
                "action": {"type": "string", "description": "Transition action"},
                "reason": {"type": "string"},
                "target_status": {"type": "string"},
                "replacement_task_ids": {"type": "array", "items": {"type": "string"}, "description": "For force_close with decomposition: IDs of replacement subtasks. Not required if closing a redundant task with a reason."}
            }
        }),
    )
}

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

fn tool_task_comment_add() -> RmcpTool {
    RmcpTool::new(
        "task_comment_add".to_string(),
        "Add a comment or strategic observation to a task's activity log.".to_string(),
        object!({
            "type": "object",
            "required": ["id", "body"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short ID"},
                "body": {"type": "string", "description": "Comment body to add to the activity log"}
            }
        }),
    )
}

fn from_value<T>(value: serde_json::Value) -> Result<T, serde_json::Error>
where
    T: DeserializeOwned,
{
    serde_json::from_value(value)
}

fn tool_output_view() -> RmcpTool {
    RmcpTool::new(
        "output_view".to_string(),
        "Paginated view of a truncated tool output. When a tool result was truncated, \
         the full output is stashed and can be browsed here by tool_use_id."
            .to_string(),
        object!({
            "type": "object",
            "required": ["tool_use_id"],
            "properties": {
                "tool_use_id": {"type": "string", "description": "The tool_use_id from the truncated result"},
                "offset": {"type": "integer", "minimum": 0, "description": "Line offset (0-based, default 0)"},
                "limit": {"type": "integer", "minimum": 1, "description": "Number of lines to return (default 200)"}
            }
        }),
    )
}

fn tool_output_grep() -> RmcpTool {
    RmcpTool::new(
        "output_grep".to_string(),
        "Regex search within a truncated tool output. Returns matching lines with \
         context from the full stashed output."
            .to_string(),
        object!({
            "type": "object",
            "required": ["tool_use_id", "pattern"],
            "properties": {
                "tool_use_id": {"type": "string", "description": "The tool_use_id from the truncated result"},
                "pattern": {"type": "string", "description": "Regex pattern to search for"},
                "context_lines": {"type": "integer", "minimum": 0, "description": "Lines of context around each match (default 3)"}
            }
        }),
    )
}

fn base_tool_schemas() -> Vec<serde_json::Value> {
    let mut tool_values = vec![
        serde_json::to_value(tool_task_show()).expect("serialize tool_task_show"),
        serde_json::to_value(tool_task_list()).expect("serialize tool_task_list"),
        serde_json::to_value(tool_task_activity_list()).expect("serialize tool_task_activity_list"),
        serde_json::to_value(tool_memory_read()).expect("serialize tool_memory_read"),
        serde_json::to_value(tool_memory_search()).expect("serialize tool_memory_search"),
        serde_json::to_value(tool_memory_list()).expect("serialize tool_memory_list"),
    ];
    tool_values.push(serde_json::to_value(tool_shell()).expect("serialize tool_shell"));
    tool_values.push(serde_json::to_value(tool_read()).expect("serialize tool_read"));
    tool_values.push(serde_json::to_value(tool_lsp()).expect("serialize tool_lsp"));
    tool_values.push(serde_json::to_value(tool_output_view()).expect("serialize tool_output_view"));
    tool_values.push(serde_json::to_value(tool_output_grep()).expect("serialize tool_output_grep"));
    tool_values
}

/// Tool schemas for Worker and Resolver: base + file-editing tools.
pub(crate) fn tool_schemas_worker() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    tool_values.push(serde_json::to_value(tool_write()).expect("serialize tool_write"));
    tool_values.push(serde_json::to_value(tool_edit()).expect("serialize tool_edit"));
    tool_values.push(serde_json::to_value(tool_apply_patch()).expect("serialize tool_apply_patch"));
    tool_values.push(serde_json::to_value(tool_request_pm()).expect("serialize tool_request_pm"));
    tool_values.push(
        serde_json::to_value(crate::roles::finalize::tool_submit_work())
            .expect("serialize tool_submit_work"),
    );
    tool_values
}

/// Tool schemas for Reviewer: base + submit_review finalize tool.
/// task_update_ac is excluded — submit_review sets AC atomically.
pub(crate) fn tool_schemas_reviewer() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    tool_values.push(
        serde_json::to_value(crate::roles::finalize::tool_submit_review())
            .expect("serialize tool_submit_review"),
    );
    tool_values
}

/// Tool schemas for Lead: base + task/epic management tools + submit_decision finalize tool.
/// task_comment_add and task_transition are excluded — submit_decision drives transitions.
pub(crate) fn tool_schemas_lead() -> Vec<serde_json::Value> {
    tool_schemas_pm()
}

/// Tool schemas for PM (Lead): base + task/epic management tools + submit_decision finalize tool.
/// task_comment_add and task_transition are excluded — submit_decision drives transitions.
pub(crate) fn tool_schemas_pm() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    for value in [
        serde_json::to_value(tool_task_create()).expect("serialize tool_task_create"),
        serde_json::to_value(tool_task_update()).expect("serialize tool_task_update"),
        serde_json::to_value(tool_task_delete_branch()).expect("serialize tool_task_delete_branch"),
        serde_json::to_value(tool_task_archive_activity())
            .expect("serialize tool_task_archive_activity"),
        serde_json::to_value(tool_task_reset_counters())
            .expect("serialize tool_task_reset_counters"),
        serde_json::to_value(tool_task_kill_session()).expect("serialize tool_task_kill_session"),
        serde_json::to_value(tool_task_blocked_list()).expect("serialize tool_task_blocked_list"),
        serde_json::to_value(tool_epic_show()).expect("serialize tool_epic_show"),
        serde_json::to_value(tool_epic_update()).expect("serialize tool_epic_update"),
        serde_json::to_value(tool_epic_tasks()).expect("serialize tool_epic_tasks"),
        serde_json::to_value(crate::roles::finalize::tool_submit_decision())
            .expect("serialize tool_submit_decision"),
    ] {
        tool_values.push(value);
    }
    tool_values
}

/// Tool schemas for Planner: base + task/epic management tools + submit_grooming finalize tool.
/// task_comment_add is excluded — submit_grooming captures session output.
pub(crate) fn tool_schemas_planner() -> Vec<serde_json::Value> {
    tool_schemas_groomer()
}

/// Tool schemas for Groomer (Planner): base + task/epic management tools + submit_grooming finalize tool.
/// task_comment_add is excluded — submit_grooming captures session output.
pub(crate) fn tool_schemas_groomer() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    for value in [
        serde_json::to_value(tool_task_create()).expect("serialize tool_task_create"),
        serde_json::to_value(tool_task_update()).expect("serialize tool_task_update"),
        serde_json::to_value(tool_task_transition()).expect("serialize tool_task_transition"),
        serde_json::to_value(tool_task_delete_branch()).expect("serialize tool_task_delete_branch"),
        serde_json::to_value(tool_task_archive_activity())
            .expect("serialize tool_task_archive_activity"),
        serde_json::to_value(tool_task_reset_counters())
            .expect("serialize tool_task_reset_counters"),
        serde_json::to_value(tool_task_kill_session()).expect("serialize tool_task_kill_session"),
        serde_json::to_value(tool_task_blocked_list()).expect("serialize tool_task_blocked_list"),
        serde_json::to_value(tool_epic_show()).expect("serialize tool_epic_show"),
        serde_json::to_value(tool_epic_update()).expect("serialize tool_epic_update"),
        serde_json::to_value(tool_epic_tasks()).expect("serialize tool_epic_tasks"),
        serde_json::to_value(crate::roles::finalize::tool_submit_grooming())
            .expect("serialize tool_submit_grooming"),
    ] {
        tool_values.push(value);
    }
    tool_values
}

/// Tool schemas for Architect: read-only tools, task/epic management, submit_work,
/// and agent effectiveness tools (role_metrics, memory_build_context, role_amend_prompt).
/// Does not include write/edit/apply_patch. The Architect diagnoses and directs but does not write code.
pub(crate) fn tool_schemas_architect() -> Vec<serde_json::Value> {
    let mut tool_values = base_tool_schemas();
    for value in [
        serde_json::to_value(tool_task_create()).expect("serialize tool_task_create"),
        serde_json::to_value(tool_task_comment_add()).expect("serialize tool_task_comment_add"),
        serde_json::to_value(tool_task_transition()).expect("serialize tool_task_transition"),
        serde_json::to_value(tool_task_kill_session()).expect("serialize tool_task_kill_session"),
        serde_json::to_value(tool_task_blocked_list()).expect("serialize tool_task_blocked_list"),
        serde_json::to_value(tool_epic_show()).expect("serialize tool_epic_show"),
        serde_json::to_value(tool_epic_update()).expect("serialize tool_epic_update"),
        serde_json::to_value(tool_epic_tasks()).expect("serialize tool_epic_tasks"),
        serde_json::to_value(tool_memory_build_context())
            .expect("serialize tool_memory_build_context"),
        serde_json::to_value(tool_role_metrics()).expect("serialize tool_role_metrics"),
        serde_json::to_value(tool_role_amend_prompt()).expect("serialize tool_role_amend_prompt"),
        serde_json::to_value(crate::roles::finalize::tool_submit_work())
            .expect("serialize tool_submit_work"),
    ] {
        tool_values.push(value);
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
    state: &AgentContext,
    name: &str,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
    session_task_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let synthetic = serde_json::json!({ "name": name, "arguments": arguments });
    dispatch_tool_call(state, &synthetic, worktree_path, None, session_task_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentType;
    use crate::test_helpers::create_test_db;
    use std::path::Path;
    use tokio_util::sync::CancellationToken;

    pub(crate) mod fuzzy_replace_tests {
        use super::*;

        #[test]
        fn rebases_multiline_replacement_using_matched_indentation() {
            let content = "fn main() {\n    match value {\n        Some(x) => {\n            process(x);\n        }\n    }\n}\n";
            let old_text = "match value {\n    Some(x) => {\n        process(x);\n    }\n}";
            let new_text = "match value {\n    Some(x) => {\n        if ready {\n            process(x);\n        }\n    }\n}";

            let (updated, note) = fuzzy_replace(content, old_text, new_text, Path::new("test.rs"))
                .expect("fuzzy replace should succeed");

            assert_eq!(note.as_deref(), Some("(matched with flexible indentation)"));
            assert!(updated.contains(
                "    match value {\n        Some(x) => {\n            if ready {\n                process(x);\n            }\n        }\n    }"
            ));
        }

        #[test]
        fn preserves_later_nested_indent_when_first_replacement_line_is_less_indented() {
            let content =
                "impl Example {\n        if condition {\n            run();\n        }\n}\n";
            let old_text = "if condition {\n    run();\n}";
            let new_text =
                "if condition {\n    let nested = || {\n        run();\n    };\n    nested();\n}";

            let (updated, note) = fuzzy_replace(content, old_text, new_text, Path::new("test.rs"))
                .expect("fuzzy replace should succeed");

            assert_eq!(note.as_deref(), Some("(matched with flexible indentation)"));
            assert!(updated.contains(
                "        if condition {\n            let nested = || {\n                run();\n            };\n            nested();\n        }"
            ));
        }

        #[test]
        fn reindent_replacement_preserves_internal_relative_indentation() {
            let matched_block = "        if ready {\n            execute();\n        }";
            let replacement =
                "if ready {\n    let nested = || {\n        execute();\n    };\n    nested();\n}";

            assert_eq!(
                reindent_replacement(matched_block, replacement),
                "        if ready {\n            let nested = || {\n                execute();\n            };\n            nested();\n        }"
            );
        }
    }

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

        let state =
            crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
        let result = call_write(&state, &args, worktree.path()).await;
        assert!(result.is_err());
        let err = result.err().unwrap_or_default();
        assert!(err.contains("outside worktree"));
        assert!(!outside.path().join("pwned.txt").exists());
    }

    #[test]
    fn worker_cannot_use_pm_only_tool() {
        // submit_decision is PM-only (ADR-036: finalize tools are role-specific).
        assert!(!is_tool_allowed_for_agent(
            AgentType::Worker,
            "submit_decision"
        ));
        assert!(is_tool_allowed_for_agent(AgentType::Lead, "submit_decision"));
        // task_transition is not in the PM tool set (removed by ADR-036).
        assert!(!is_tool_allowed_for_agent(AgentType::Lead, "task_transition"));
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
        fn schema_names(schemas: Vec<serde_json::Value>) -> Vec<String> {
            schemas
                .into_iter()
                .filter_map(|v| {
                    v.get("name")
                        .and_then(|n| n.as_str())
                        .map(ToString::to_string)
                })
                .collect()
        }

        let worker = schema_names(tool_schemas_worker());
        assert!(worker.iter().any(|n| n == "shell"));
        assert!(worker.iter().any(|n| n == "write"));
        assert!(worker.iter().any(|n| n == "edit"));
        assert!(worker.iter().any(|n| n == "submit_work"));
        assert!(!worker.iter().any(|n| n == "task_comment_add"));

        let reviewer = schema_names(tool_schemas_reviewer());
        assert!(reviewer.iter().any(|n| n == "submit_review"));
        assert!(!reviewer.iter().any(|n| n == "task_update_ac"));
        assert!(!reviewer.iter().any(|n| n == "task_comment_add"));

        let lead = schema_names(tool_schemas_lead());
        assert!(lead.iter().any(|n| n == "task_create"));
        assert!(lead.iter().any(|n| n == "submit_decision"));
        assert!(!lead.iter().any(|n| n == "task_transition"));
        assert!(!lead.iter().any(|n| n == "task_comment_add"));

        let planner = schema_names(tool_schemas_planner());
        assert!(planner.iter().any(|n| n == "task_create"));
        assert!(planner.iter().any(|n| n == "task_transition"));
        assert!(planner.iter().any(|n| n == "submit_grooming"));
        assert!(!planner.iter().any(|n| n == "task_comment_add"));

        let architect = schema_names(tool_schemas_architect());
        assert!(architect.iter().any(|n| n == "shell"));
        assert!(architect.iter().any(|n| n == "read"));
        assert!(architect.iter().any(|n| n == "task_create"));
        assert!(architect.iter().any(|n| n == "task_comment_add"));
        assert!(architect.iter().any(|n| n == "task_transition"));
        assert!(architect.iter().any(|n| n == "task_kill_session"));
        assert!(architect.iter().any(|n| n == "submit_work"));
        // Architect must NOT have code-writing tools.
        assert!(!architect.iter().any(|n| n == "write"));
        assert!(!architect.iter().any(|n| n == "edit"));
        assert!(!architect.iter().any(|n| n == "apply_patch"));
    }

    #[test]
    fn ensure_path_within_worktree_accepts_in_tree_and_rejects_traversal() {
        use tempfile::tempdir;

        let worktree = tempdir().expect("temp worktree");
        let nested = worktree.path().join("nested");
        std::fs::create_dir_all(&nested).expect("create nested");
        let in_tree = nested.join("file.txt");
        ensure_path_within_worktree(&in_tree, worktree.path()).expect("in-tree path should pass");

        let traversal = worktree.path().join("..").join("..").join("escape.txt");
        let err = ensure_path_within_worktree(&traversal, worktree.path())
            .expect_err("traversal should be rejected");
        assert!(err.contains("outside worktree"));
    }

    #[test]
    fn ensure_path_within_worktree_rejects_symlink_escape() {
        use tempfile::tempdir;

        let worktree = tempdir().expect("temp worktree");
        let outside = tempdir().expect("outside");
        let link = worktree.path().join("escape-link");

        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &link).expect("create symlink");
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(outside.path(), &link).expect("create symlink");

        let escaped = link.join("leak.txt");
        let err = ensure_path_within_worktree(&escaped, worktree.path())
            .expect_err("symlink escape should be rejected");
        assert!(err.contains("outside worktree"));
    }

    #[test]
    fn is_tool_allowed_for_schemas_handles_empty_and_invalid_entries() {
        assert!(!is_tool_allowed_for_schemas(&[], "shell"));

        let schemas = vec![
            serde_json::json!({}),
            serde_json::json!({"name": null}),
            serde_json::json!({"name": 42}),
            serde_json::json!({"name": "shell"}),
        ];
        assert!(is_tool_allowed_for_schemas(&schemas, "shell"));
        assert!(!is_tool_allowed_for_schemas(&schemas, "read"));
    }

    #[test]
    fn resolve_path_handles_relative_absolute_and_normalization() {
        let base = Path::new("/tmp/worktree");

        let relative = resolve_path("src/main.rs", base);
        assert_eq!(relative, PathBuf::from("/tmp/worktree/src/main.rs"));

        let absolute = resolve_path("/etc/hosts", base);
        assert_eq!(absolute, PathBuf::from("/etc/hosts"));

        let normalized = resolve_path("./src/../Cargo.toml", base);
        assert_eq!(normalized, PathBuf::from("/tmp/worktree/Cargo.toml"));
    }

    fn tool_names(schemas: &[serde_json::Value]) -> Vec<&str> {
        schemas
            .iter()
            .filter_map(|v| v.get("name").and_then(|n| n.as_str()))
            .collect()
    }

    #[test]
    fn snapshot_worker_tool_names() {
        let schemas = tool_schemas_worker();
        let names = tool_names(&schemas);
        insta::assert_json_snapshot!("worker_tool_names", names);
    }

    #[test]
    fn snapshot_worker_tool_schemas() {
        insta::assert_json_snapshot!("worker_tool_schemas", tool_schemas_worker());
    }

    #[test]
    fn snapshot_reviewer_tool_names() {
        let schemas = tool_schemas_reviewer();
        let names = tool_names(&schemas);
        insta::assert_json_snapshot!("reviewer_tool_names", names);
    }

    #[test]
    fn snapshot_reviewer_tool_schemas() {
        insta::assert_json_snapshot!("reviewer_tool_schemas", tool_schemas_reviewer());
    }

    #[test]
    fn snapshot_lead_tool_names() {
        let schemas = tool_schemas_lead();
        let names = tool_names(&schemas);
        insta::assert_json_snapshot!("lead_tool_names", names);
    }

    #[test]
    fn snapshot_lead_tool_schemas() {
        insta::assert_json_snapshot!("lead_tool_schemas", tool_schemas_lead());
    }

    #[test]
    fn snapshot_planner_tool_names() {
        let schemas = tool_schemas_planner();
        let names = tool_names(&schemas);
        insta::assert_json_snapshot!("planner_tool_names", names);
    }

    #[test]
    fn snapshot_planner_tool_schemas() {
        insta::assert_json_snapshot!("planner_tool_schemas", tool_schemas_planner());
    }

    #[test]
    fn snapshot_architect_tool_names() {
        let schemas = tool_schemas_architect();
        let names = tool_names(&schemas);
        insta::assert_json_snapshot!("architect_tool_names", names);
    }

    #[test]
    fn snapshot_architect_tool_schemas() {
        insta::assert_json_snapshot!("architect_tool_schemas", tool_schemas_architect());
    }
}

fn tool_apply_patch() -> RmcpTool {
    RmcpTool::new(
        "apply_patch".to_string(),
        concat!(
            "Apply a patch to one or more files using a custom LLM-friendly format. ",
            "Uses content-based context matching (not line numbers). Format:\n\n",
            "*** Begin Patch\n",
            "*** Update File: path/to/file.rs\n",
            "@@ context_line_from_file\n",
            " context line (unchanged)\n",
            "-old line to remove\n",
            "+new line to add\n",
            " context line (unchanged)\n\n",
            "*** Add File: path/to/new_file.rs\n",
            "+line 1\n",
            "+line 2\n\n",
            "*** Delete File: path/to/old_file.rs\n",
            "*** End Patch\n\n",
            "Rules: ' ' prefix = context (must match file), '-' = delete, '+' = add. ",
            "The @@ line text is searched in the file to locate each chunk. ",
            "Multiple @@ chunks per file are allowed. ",
            "Files being updated or deleted must be read first.",
        )
        .to_string(),
        object!({
            "type": "object",
            "required": ["patch"],
            "properties": {
                "patch": {"type": "string", "description": "Patch content in the custom format (see tool description)"}
            }
        }),
    )
}

fn tool_lsp() -> RmcpTool {
    RmcpTool::new(
        "lsp".to_string(),
        "Query the Language Server Protocol for code navigation. Operations: hover (type info at position), definition (go to definition), references (find all references), symbols (list document symbols). Line and character are 1-based.".to_string(),
        object!({
            "type": "object",
            "required": ["operation", "file_path"],
            "properties": {
                "operation": {
                    "type": "string",
                    "enum": ["hover", "definition", "references", "symbols"],
                    "description": "LSP operation to perform"
                },
                "file_path": {
                    "type": "string",
                    "description": "Absolute or worktree-relative file path"
                },
                "line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based line number (required for hover, definition, references)"
                },
                "character": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based column number (required for hover, definition, references)"
                }
            }
        }),
    )
}
