mod shared_schemas;

use rmcp::model::Tool as RmcpTool;
use rmcp::object;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use shared_schemas::{shared_base_tool_schemas, shared_pm_tool_schemas, tool_task_transition};

use super::sandbox;
use crate::context::AgentContext;
use crate::lsp::format_diagnostics_xml;
use djinn_core::models::Task;
use djinn_db::AgentRepository;
use djinn_db::EpicRepository;
use djinn_db::ProjectRepository;
use djinn_db::SessionRepository;
use djinn_db::TaskRepository;
use djinn_mcp::tools::agent_tools::{
    AgentCreateParams as SharedAgentCreateParams, AgentMetricsParams as SharedAgentMetricsParams,
    create_agent as shared_create_agent, metrics_for_agents as shared_metrics_for_agents,
};
use djinn_mcp::tools::epic_ops::{EpicShowRequest, EpicTasksRequest, EpicUpdateDeltaRequest};
use djinn_mcp::tools::memory_tools::{
    BuildContextParams as SharedMemoryBuildContextParams, ListParams as SharedMemoryListParams,
    ReadParams as SharedMemoryReadParams, SearchParams as SharedMemorySearchParams,
};
use djinn_mcp::tools::task_tools::{
    CommentTaskRequest as SharedCommentTaskRequest, CreateTaskRequest as SharedCreateTaskRequest,
    TransitionTaskRequest as SharedTransitionTaskRequest,
    UpdateTaskRequest as SharedUpdateTaskRequest, add_task_comment as shared_add_task_comment,
    create_task as shared_create_task, transition_task as shared_transition_task,
    update_task as shared_update_task,
};
use djinn_provider::github_api::GitHubApiClient;
use djinn_provider::repos::CredentialRepository;
use rmcp::Json;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

#[derive(Deserialize)]
struct IncomingToolCall {
    name: String,
    arguments: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Supported djinn-agent → djinn-mcp integration seam for shared task mutation ops.
///
/// External callers should bridge their existing runtime context through
/// [`AgentContext::to_mcp_state`] and resolve the project id with
/// [`AgentContext::require_project_id_for_task_ops`] using the session/worktree root
/// rather than a crate-local source path. This preserves MCP-side project resolution
/// semantics and lets shared mutation helpers return the same public response shapes
/// and JSON error envelopes that agent dispatch tests assert.
async fn project_id_for_path(state: &AgentContext, project_path: &str) -> Result<String, String> {
    state
        .require_project_id_for_task_ops(project_path)
        .await
        .map_err(|error| error.error)
}

fn acceptance_criterion_to_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => map
            .get("criterion")
            .and_then(|criterion| criterion.as_str())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| value.to_string()),
        serde_json::Value::String(text) => text.clone(),
        _ => value.to_string(),
    }
}

fn task_response_to_value(
    response: djinn_mcp::tools::task_tools::TaskResponse,
) -> serde_json::Value {
    serde_json::to_value(response)
        .unwrap_or_else(|_| serde_json::json!({ "error": "failed to serialize task response" }))
}

fn activity_entry_to_value(
    response: djinn_mcp::tools::task_tools::ActivityEntryResponse,
) -> serde_json::Value {
    serde_json::to_value(response)
        .unwrap_or_else(|_| serde_json::json!({ "error": "failed to serialize activity response" }))
}

fn error_or_to_value<T>(
    response: djinn_mcp::tools::task_tools::ErrorOr<T>,
    ok: impl FnOnce(T) -> serde_json::Value,
) -> Result<serde_json::Value, String> {
    Ok(match response {
        djinn_mcp::tools::task_tools::ErrorOr::Ok(value) => ok(value),
        djinn_mcp::tools::task_tools::ErrorOr::Error(error) => {
            serde_json::json!({ "error": error.error })
        }
    })
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
    session_role: Option<&str>,
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
    let worktree_project_path = worktree_path.display().to_string();

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
        "task_create" => call_task_create(state, &call.arguments, &worktree_project_path).await,
        "task_update" => call_task_update(state, &call.arguments, &worktree_project_path).await,
        "task_update_ac" => call_task_update_ac(state, &call.arguments).await,
        "task_comment_add" => {
            call_task_comment_add(state, &call.arguments, session_role, &worktree_project_path)
                .await
        }
        "request_lead" => call_request_lead(state, &call.arguments).await,
        "request_architect" => call_request_architect(state, &call.arguments).await,
        "task_transition" => {
            call_task_transition(state, &call.arguments, &worktree_project_path).await
        }
        "task_delete_branch" => call_task_delete_branch(state, &call.arguments).await,
        "task_archive_activity" => call_task_archive_activity(state, &call.arguments).await,
        "task_reset_counters" => call_task_reset_counters(state, &call.arguments).await,
        "task_kill_session" => call_task_kill_session(state, &call.arguments).await,
        "task_blocked_list" => call_task_blocked_list(state, &call.arguments).await,
        "task_activity_list" => call_task_activity_list(state, &call.arguments).await,
        "epic_show" => call_epic_show(state, &call.arguments).await,
        "epic_update" => call_epic_update(state, &call.arguments).await,
        "epic_tasks" => call_epic_tasks(state, &call.arguments).await,
        "epic_close" => call_epic_close(state, &call.arguments).await,
        "memory_read" => call_memory_read(state, &call.arguments, &worktree_project_path).await,
        "memory_search" => {
            call_memory_search(state, &call.arguments, session_task_id, &worktree_project_path)
                .await
        }
        "memory_list" => call_memory_list(state, &call.arguments, &worktree_project_path).await,
        "memory_build_context" => {
            call_memory_build_context(
                state,
                &call.arguments,
                session_task_id,
                &worktree_project_path,
            )
            .await
        }
        "agent_metrics" => {
            call_agent_metrics(state, &call.arguments, &worktree_project_path).await
        }
        "agent_amend_prompt" => {
            call_agent_amend_prompt(state, &call.arguments, &worktree_project_path).await
        }
        "agent_create" => {
            call_agent_create(state, &call.arguments, &worktree_project_path).await
        }
        "ci_job_log" => call_ci_job_log(state, &call.arguments, session_task_id).await,
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
    #[serde(default)]
    blocked_by_add: Vec<String>,
    #[serde(default)]
    blocked_by_remove: Vec<String>,
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
    #[serde(rename = "status")]
    _status: Option<String>,
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
    identifier: String,
}

#[derive(Deserialize)]
struct MemorySearchParams {
    query: String,
    folder: Option<String>,
    #[serde(rename = "type")]
    note_type: Option<String>,
    limit: Option<i64>,
    task_id: Option<String>,
}

#[derive(Deserialize)]
struct MemoryListParams {
    folder: Option<String>,
    #[serde(rename = "type")]
    note_type: Option<String>,
    depth: Option<i64>,
}

#[derive(Deserialize)]
struct MemoryBuildContextParams {
    url: String,
    /// Link traversal depth (default 1). Currently unused at the dispatch layer.
    _depth: Option<i64>,
    max_related: Option<i64>,
    budget: Option<i64>,
    task_id: Option<String>,
}

#[derive(Deserialize)]
struct AgentAmendPromptParams {
    agent_id: String,
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

async fn resolve_project_id_for_agent_tools(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<String, String> {
    let project_id = arguments
        .as_ref()
        .and_then(|map| map.get("project"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .map(|project| async move {
            let repo = ProjectRepository::new(state.db.clone(), state.event_bus.clone());
            repo.resolve(project)
                .await
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("project not found: {project}"))
        });

    if let Some(project_id) = project_id {
        return project_id.await;
    }

    let repo = ProjectRepository::new(state.db.clone(), state.event_bus.clone());
    let projects = repo.list().await.map_err(|e| e.to_string())?;
    match projects.as_slice() {
        [project] => Ok(project.id.clone()),
        [] => Err("no project configured for agent tool call".to_string()),
        _ => Err("project is required when multiple projects are configured".to_string()),
    }
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
    let project_id = resolve_project_id_for_agent_tools(state, arguments).await?;
    let repo = EpicRepository::new(state.db.clone(), state.event_bus.clone());
    let response = djinn_mcp::tools::epic_ops::epic_show(
        &repo,
        &project_id,
        EpicShowRequest {
            project: String::new(),
            id: p.id,
        },
    )
    .await;
    serde_json::to_value(response).map_err(|e| e.to_string())
}

async fn call_epic_update(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: EpicUpdateParams = parse_args(arguments)?;
    let project_id = resolve_project_id_for_agent_tools(state, arguments).await?;
    let repo = EpicRepository::new(state.db.clone(), state.event_bus.clone());
    let response = djinn_mcp::tools::epic_ops::epic_update_with_delta(
        &repo,
        &project_id,
        EpicUpdateDeltaRequest {
            project: String::new(),
            id: p.id,
            title: p.title,
            description: p.description,
            emoji: None,
            color: None,
            owner: None,
            memory_refs_add: p.memory_refs_add,
            memory_refs_remove: p.memory_refs_remove,
        },
    )
    .await;
    serde_json::to_value(response).map_err(|e| e.to_string())
}

async fn call_epic_tasks(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: EpicTasksParams = parse_args(arguments)?;
    let project_id = resolve_project_id_for_agent_tools(state, arguments).await?;
    let epic_repo = EpicRepository::new(state.db.clone(), state.event_bus.clone());
    let task_repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
    let response = djinn_mcp::tools::epic_ops::epic_tasks(
        &epic_repo,
        &task_repo,
        &project_id,
        EpicTasksRequest {
            project: String::new(),
            epic_id: p.id,
            status: None,
            issue_type: None,
            sort: None,
            limit: p.limit,
            offset: p.offset,
        },
    )
    .await;
    let mut value = serde_json::to_value(response).map_err(|e| e.to_string())?;
    if let Some(map) = value.as_object_mut()
        && let Some(total_count) = map.remove("total_count")
    {
        map.insert("total".to_string(), total_count);
    }
    Ok(value)
}

async fn call_epic_close(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    let p: EpicShowParams = parse_args(arguments)?;
    let project_id = resolve_project_id_for_agent_tools(state, arguments).await?;
    let repo = EpicRepository::new(state.db.clone(), state.event_bus.clone());
    let epic = repo
        .resolve_in_project(&project_id, &p.id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("epic not found: {}", p.id))?;
    if epic.status == "closed" {
        return Err("epic is already closed".to_string());
    }
    let closed = repo.close(&epic.id).await.map_err(|e| e.to_string())?;
    serde_json::to_value(serde_json::json!({
        "epic": {
            "id": closed.id,
            "short_id": closed.short_id,
            "title": closed.title,
            "status": closed.status,
        }
    }))
    .map_err(|e| e.to_string())
}

async fn call_task_create(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: TaskCreateParams = parse_args(arguments)?;
    let status = match p.status.as_deref() {
        None => None,
        Some("open") => Some("open"),
        Some(other) => {
            return Err(format!("invalid status: {other:?} (expected open)"));
        }
    };
    let project_id = project_id_for_path(state, project_path).await?;
    let server = djinn_mcp::server::DjinnMcpServer::new(state.to_mcp_state());
    let Json(response) = shared_create_task(
        &server,
        &project_id,
        SharedCreateTaskRequest {
            title: p.title,
            description: p.description.unwrap_or_default(),
            design: p.design.unwrap_or_default(),
            issue_type: p.issue_type.unwrap_or_else(|| "task".to_string()),
            priority: p.priority.unwrap_or(0),
            owner: p.owner.unwrap_or_default(),
            status: status.map(str::to_string),
            acceptance_criteria: p.acceptance_criteria,
            labels: Vec::new(),
            memory_refs: p.memory_refs.unwrap_or_default(),
            blocked_by_refs: p.blocked_by.unwrap_or_default(),
            agent_type: p.agent_type,
            epic_ref: Some(p.epic_id),
        },
    )
    .await;

    error_or_to_value(response, task_response_to_value)
}

async fn call_task_update(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: TaskUpdateParams = parse_args(arguments)?;
    let project_id = project_id_for_path(state, project_path).await?;
    let server = djinn_mcp::server::DjinnMcpServer::new(state.to_mcp_state());
    let Json(response) = shared_update_task(
        &server,
        &project_id,
        SharedUpdateTaskRequest {
            id: p.id,
            title: p.title,
            description: p.description,
            design: p.design,
            priority: p.priority,
            owner: p.owner,
            acceptance_criteria: p.acceptance_criteria.map(|criteria| {
                criteria
                    .into_iter()
                    .map(|item| acceptance_criterion_to_string(&item))
                    .collect()
            }),
            labels_add: p.labels_add.unwrap_or_default(),
            labels_remove: p
                .labels_remove
                .unwrap_or_default()
                .into_iter()
                .collect::<HashSet<_>>(),
            memory_refs_add: p.memory_refs_add.unwrap_or_default(),
            memory_refs_remove: p
                .memory_refs_remove
                .unwrap_or_default()
                .into_iter()
                .collect::<HashSet<_>>(),
            blocked_by_add_refs: p.blocked_by_add,
            blocked_by_remove_refs: p.blocked_by_remove,
            agent_type: None,
            epic_ref: None,
        },
    )
    .await;

    error_or_to_value(response, task_response_to_value)
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

async fn call_request_lead(
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

    // Check escalation count via coordinator.
    // On the 2nd+ escalation for the same task, auto-route to Architect.
    let coordinator = state.coordinator().await;
    let escalation_count = if let Some(ref coord) = coordinator {
        coord
            .increment_escalation_count(&task.id)
            .await
            .unwrap_or(1)
    } else {
        1
    };

    if escalation_count >= 2 {
        let architect_reason = format!(
            "Auto-escalated to Architect after {} Lead escalations. Latest reason: {}",
            escalation_count, p.reason
        );
        if let Some(ref coord) = coordinator {
            let _ = coord
                .dispatch_architect_escalation(&task.id, &architect_reason, &task.project_id)
                .await;
        }
    }

    // Escalate the task to needs_lead_intervention in all cases.
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

    if escalation_count >= 2 {
        Ok(serde_json::json!({
            "status": "architect_escalated",
            "task_id": updated.id,
            "new_status": updated.status,
            "escalation_count": escalation_count,
            "message": "Task has been escalated multiple times. Routing to Architect for review. Your session should end now."
        }))
    } else {
        Ok(serde_json::json!({
            "status": "escalated",
            "task_id": updated.id,
            "new_status": updated.status,
            "escalation_count": escalation_count,
            "message": "Task escalated to Lead. Your session should end now."
        }))
    }
}

async fn call_request_architect(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    #[derive(Deserialize)]
    struct RequestArchitectParams {
        id: String,
        reason: String,
    }

    let p: RequestArchitectParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());

    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    let body = format!(
        "[ARCHITECT_REQUEST] Lead escalating to Architect. {}",
        p.reason
    );
    let payload = serde_json::json!({ "body": body }).to_string();
    repo.log_activity(Some(&task.id), "lead-agent", "lead", "comment", &payload)
        .await
        .map_err(|e| e.to_string())?;

    let Some(coordinator) = state.coordinator().await else {
        return Ok(serde_json::json!({
            "error": "coordinator not available — cannot dispatch Architect"
        }));
    };

    let _ = coordinator
        .dispatch_architect_escalation(&task.id, &p.reason, &task.project_id)
        .await;

    Ok(serde_json::json!({
        "status": "architect_dispatched",
        "task_id": task.id,
        "message": "Architect has been dispatched to review this task. Your session should end now."
    }))
}

async fn call_task_comment_add(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    session_role: Option<&str>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: TaskCommentAddParams = parse_args(arguments)?;
    let default_role = session_role.unwrap_or("system");
    let project_id = project_id_for_path(state, project_path).await?;
    let server = djinn_mcp::server::DjinnMcpServer::new(state.to_mcp_state());
    let Json(response) = shared_add_task_comment(
        &server,
        &project_id,
        SharedCommentTaskRequest {
            id: p.id,
            body: p.body,
            actor_id: p.actor_id.unwrap_or_else(|| default_role.to_string()),
            actor_role: p.actor_role.unwrap_or_else(|| default_role.to_string()),
        },
    )
    .await;

    error_or_to_value(response, activity_entry_to_value)
}

async fn call_memory_read(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryReadParams = parse_args(arguments)?;
    let project_path = project_path.to_owned();
    let server = djinn_mcp::server::DjinnMcpServer::new(state.to_mcp_state());
    Ok(serde_json::to_value(
        djinn_mcp::tools::memory_tools::ops::memory_read(
            &server,
            SharedMemoryReadParams {
                project: project_path,
                identifier: p.identifier,
            },
        )
        .await,
    )
    .unwrap_or_else(|_| serde_json::json!({ "error": "failed to serialize memory_read response" })))
}

async fn call_memory_search(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    session_task_id: Option<&str>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemorySearchParams = parse_args(arguments)?;
    let project_path = project_path.to_owned();
    let task_id = p.task_id.or_else(|| session_task_id.map(ToOwned::to_owned));
    let server = djinn_mcp::server::DjinnMcpServer::new(state.to_mcp_state());
    Ok(serde_json::to_value(
        djinn_mcp::tools::memory_tools::ops::memory_search(
            &server,
            SharedMemorySearchParams {
                project: project_path,
                query: p.query,
                folder: p.folder,
                note_type: p.note_type,
                limit: p.limit,
            },
            task_id.as_deref(),
        )
        .await,
    )
    .unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_search response" }),
    ))
}

async fn call_memory_list(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryListParams = parse_args(arguments)?;
    let project_path = project_path.to_owned();
    let server = djinn_mcp::server::DjinnMcpServer::new(state.to_mcp_state());
    Ok(serde_json::to_value(
        djinn_mcp::tools::memory_tools::ops::memory_list(
            &server,
            SharedMemoryListParams {
                project: project_path,
                folder: p.folder,
                note_type: p.note_type,
                depth: p.depth,
            },
        )
        .await,
    )
    .unwrap_or_else(|_| serde_json::json!({ "error": "failed to serialize memory_list response" })))
}

async fn call_memory_build_context(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    session_task_id: Option<&str>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryBuildContextParams = parse_args(arguments)?;
    let project_path = project_path.to_owned();
    let task_id = p.task_id.or_else(|| session_task_id.map(ToOwned::to_owned));
    let server = djinn_mcp::server::DjinnMcpServer::new(state.to_mcp_state());
    Ok(serde_json::to_value(
        djinn_mcp::tools::memory_tools::ops::memory_build_context(
            &server,
            SharedMemoryBuildContextParams {
                project: project_path,
                url: p.url,
                depth: None,
                max_related: p.max_related,
                budget: p.budget,
                task_id: task_id.clone(),
            },
            task_id.as_deref(),
        )
        .await,
    )
    .unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_build_context response" }),
    ))
}

async fn call_agent_metrics(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let project_id = project_id_for_path(state, project_path).await?;

    let raw = arguments.clone().unwrap_or_default();
    let params = SharedAgentMetricsParams {
        project: project_path.to_owned(),
        agent_id: raw
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned),
        window_days: raw
            .get("window_days")
            .and_then(|v| v.as_i64()),
    };

    let response = shared_metrics_for_agents(
        &AgentRepository::new(state.db.clone(), state.event_bus.clone()),
        &project_id,
        params,
    )
    .await;

    let roles = response
        .agents
        .unwrap_or_default()
        .into_iter()
        .map(|entry| {
            serde_json::json!({
                "agent_id": entry.agent_id,
                "agent_name": entry.agent_name,
                "base_role": entry.base_role,
                "learned_prompt": entry.learned_prompt,
                "success_rate": entry.success_rate,
                "avg_reopens": entry.avg_reopens,
                "verification_pass_rate": entry.verification_pass_rate,
                "completed_task_count": entry.completed_task_count,
                "avg_tokens": entry.avg_tokens,
                "avg_time_seconds": entry.avg_time_seconds,
            })
        })
        .collect::<Vec<_>>();

    Ok(serde_json::json!({
        "roles": roles,
        "window_days": response.window_days,
    }))
}

async fn call_agent_amend_prompt(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: AgentAmendPromptParams = parse_args(arguments)?;
    let project_id = project_id_for_path(state, project_path).await?;

    let repo = AgentRepository::new(state.db.clone(), state.event_bus.clone());

    // Resolve role by UUID or name.
    let role = {
        let by_id = repo.get(&p.agent_id).await.map_err(|e| e.to_string())?;
        match by_id {
            Some(r) if r.project_id == project_id => Some(r),
            _ => repo
                .get_by_name_for_project(&project_id, &p.agent_id)
                .await
                .map_err(|e| e.to_string())?,
        }
    };

    let role = role.ok_or_else(|| format!("agent not found: {}", p.agent_id))?;

    // Only allow amending specialist roles. Prevents patching high-level orchestration roles.
    if matches!(role.base_role.as_str(), "architect" | "lead" | "planner") {
        return Err(format!(
            "cannot amend learned_prompt for base_role '{}'; only specialist roles (worker, reviewer) are eligible",
            role.base_role
        ));
    }

    let updated = repo
        .append_learned_prompt(&role.id, &p.amendment, p.metrics_snapshot.as_deref())
        .await
        .map_err(|e| e.to_string())?;

    Ok(serde_json::json!({
        "agent_id": updated.id,
        "agent_name": updated.name,
        "learned_prompt": updated.learned_prompt,
        "updated_at": updated.updated_at,
        "amendment_appended": true,
    }))
}

async fn call_agent_create(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let project_id = project_id_for_path(state, project_path).await?;

    let mut raw = arguments.clone().unwrap_or_default();
    // Inject project so the shared params struct deserialises.
    raw.entry("project")
        .or_insert_with(|| serde_json::json!(project_path));
    let params: SharedAgentCreateParams =
        serde_json::from_value(serde_json::Value::Object(raw))
            .map_err(|e| format!("invalid arguments: {e}"))?;

    let response = shared_create_agent(
        &AgentRepository::new(state.db.clone(), state.event_bus.clone()),
        &project_id,
        params,
    )
    .await;

    match response.agent {
        Some(agent) => Ok(serde_json::json!({
            "agent_id": agent.id,
            "agent_name": agent.name,
            "base_role": agent.base_role,
            "created": true,
        })),
        None => Err(response
            .error
            .unwrap_or_else(|| "failed to create agent".to_string())),
    }
}

// ─── CI job log tool ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CiJobLogParams {
    job_id: u64,
    step: Option<String>,
}

/// Fetch a GitHub Actions job log, optionally filtered to a specific step.
///
/// The raw log is cleaned (timestamps stripped, group markers removed) and
/// returned as-is. When the result exceeds the tool-result size limit, the
/// reply-loop automatically stashes the full output and the worker can
/// paginate with `output_view` / `output_grep`.
async fn call_ci_job_log(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    session_task_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: CiJobLogParams = parse_args(arguments)?;

    let task_id = session_task_id.ok_or("ci_job_log requires a task context (session_task_id)")?;

    // Find the CI failure activity entry that contains the owner/repo context.
    // The PR poller stores this alongside the body when logging CI failures.
    let task_repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());

    let (owner, repo) = {
        let entries = task_repo
            .list_activity(task_id)
            .await
            .map_err(|e| format!("failed to list activity: {e}"))?;

        let mut found = None;
        for entry in entries.iter().rev() {
            if entry.event_type != "comment" || entry.actor_role != "verification" {
                continue;
            }
            if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&entry.payload)
                && let Some(ci_jobs) = payload.get("ci_jobs").and_then(|v| v.as_array())
            {
                let has_job = ci_jobs
                    .iter()
                    .any(|j| j.get("job_id").and_then(|v| v.as_u64()) == Some(p.job_id));
                if has_job {
                    let o = payload
                        .get("owner")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let r = payload
                        .get("repo")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !o.is_empty() && !r.is_empty() {
                        found = Some((o, r));
                        break;
                    }
                }
            }
        }
        found.ok_or_else(|| {
            format!(
                "Could not find CI job metadata for job_id={} in task {} activity.  \
                 This tool can only fetch logs for jobs recorded in the task activity log.",
                p.job_id, task_id
            )
        })?
    };

    let cred_repo = Arc::new(CredentialRepository::new(
        state.db.clone(),
        state.event_bus.clone(),
    ));
    let gh_client = GitHubApiClient::new(cred_repo);

    let raw_log = gh_client
        .get_job_logs(&owner, &repo, p.job_id)
        .await
        .map_err(|e| format!("failed to fetch job log: {e}"))?;

    let cleaned = clean_actions_log(&raw_log);

    // Optionally filter to just the requested step.
    let output = if let Some(ref step_name) = p.step {
        extract_step_log(&cleaned, step_name).unwrap_or_else(|| {
            format!(
                "Step '{}' not found in the job log. Returning full cleaned log.\n\n{}",
                step_name, cleaned
            )
        })
    } else {
        cleaned
    };

    Ok(serde_json::Value::String(output))
}

/// Strip GitHub Actions noise from a raw job log.
///
/// Removes ISO-8601 timestamp prefixes and `##[group]`/`##[endgroup]`
/// markers while preserving `##[error]` and `##[warning]` content.
fn clean_actions_log(raw_log: &str) -> String {
    raw_log
        .lines()
        .map(|line| {
            // Strip leading ISO-8601 timestamp prefix (29 chars like "2026-03-24T17:10:50.0448487Z ")
            line.get(..29)
                .filter(|prefix| {
                    prefix.len() >= 20
                        && prefix.as_bytes().first() == Some(&b'2')
                        && prefix.contains('T')
                        && prefix.ends_with(' ')
                })
                .map(|_| &line[29..])
                .unwrap_or(line)
        })
        .filter(|line| !line.starts_with("##[endgroup]"))
        .map(|line| {
            line.strip_prefix("##[group]")
                .or_else(|| line.strip_prefix("##[error]"))
                .or_else(|| line.strip_prefix("##[warning]"))
                .unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract the log section for a specific step name.
///
/// GitHub Actions logs use `##[group]Run <step>` / `##[endgroup]` to delimit
/// steps. After cleaning (which strips `##[group]` prefixes), the step
/// boundaries become plain text lines starting with `Run ...` or the step
/// name itself. We look for the step name in these boundary lines and return
/// everything between the start and the next boundary (or end of log).
fn extract_step_log(cleaned_log: &str, step_name: &str) -> Option<String> {
    let lines: Vec<&str> = cleaned_log.lines().collect();
    let step_lower = step_name.to_lowercase();

    // Find the start of the target step section.
    // After cleaning, step headers look like:
    //   "Run cd server && cargo test ..." or just the step name
    // We search for lines that contain the step name (case-insensitive).
    let mut start_idx = None;
    let mut end_idx = lines.len();

    // Track step boundaries — lines that look like GitHub Actions step headers.
    // These typically start with "Run " after group marker removal, or match
    // known step patterns. We use a heuristic: if a line exactly matches one
    // of the step names from the job, it's a boundary.
    //
    // Simpler approach: scan for the step name, then collect until the next
    // recognizable boundary or end of log.
    for (i, line) in lines.iter().enumerate() {
        if line.to_lowercase().contains(&step_lower) && start_idx.is_none() {
            start_idx = Some(i);
        }
    }

    let start = start_idx?;

    // Look for the next step boundary after our start.
    // Step boundaries in cleaned logs are hard to detect generically.
    // Use a practical heuristic: "Post Run " lines mark cleanup steps,
    // and "Complete job" marks the end.
    for (i, line) in lines.iter().enumerate().skip(start + 1) {
        let trimmed = line.trim();
        if trimmed.starts_with("Post Run ") || trimmed == "Complete job" {
            end_idx = i;
            break;
        }
    }

    let section: Vec<&str> = lines[start..end_idx].to_vec();
    if section.is_empty() {
        None
    } else {
        Some(section.join("\n"))
    }
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
        "verification_failure_count": t.verification_failure_count,
        "total_reopen_count": t.total_reopen_count,
        "total_verification_failure_count": t.total_verification_failure_count,
        "intervention_count": t.intervention_count,
        "last_intervention_at": t.last_intervention_at,
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
    project_path: &str,
) -> Result<serde_json::Value, String> {
    use djinn_core::models::{TaskStatus, TransitionAction};
    let p: TaskTransitionParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());
    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };
    let action = TransitionAction::parse(&p.action).map_err(|e| e.to_string())?;

    // Lead approve: transition to approved; coordinator handles PR creation separately.
    if action == TransitionAction::LeadApprove {
        if task.status != TaskStatus::InLeadIntervention.as_str() {
            return Ok(
                serde_json::json!({ "error": "lead_approve is only valid from in_lead_intervention" }),
            );
        }

        let updated = repo
            .transition(
                &task.id,
                TransitionAction::LeadApprove,
                "lead-agent",
                "lead",
                None,
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
    let project_id = project_id_for_path(state, project_path).await?;
    let server = djinn_mcp::server::DjinnMcpServer::new(state.to_mcp_state());
    let Json(response) = shared_transition_task(
        &server,
        &project_id,
        SharedTransitionTaskRequest {
            id: task.id,
            action,
            actor_id: "lead-agent".to_string(),
            actor_role: "lead".to_string(),
            reason: p.reason,
            target_override: target,
        },
    )
    .await;
    error_or_to_value(response, task_response_to_value)
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
        "UPDATE tasks SET reopen_count = 0, continuation_count = 0, intervention_count = intervention_count + 1, last_intervention_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?1"
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

fn tool_request_lead() -> RmcpTool {
    RmcpTool::new(
        "request_lead".to_string(),
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

fn tool_request_architect() -> RmcpTool {
    RmcpTool::new(
        "request_architect".to_string(),
        "Escalate to the Architect when the task requires strategic technical review that is beyond Lead intervention scope. Use when the problem is architectural, requires codebase-wide analysis, or has failed multiple Lead interventions. Adds a comment and dispatches the Architect. Your session should end after this call."
            .to_string(),
        object!({
            "type": "object",
            "required": ["id", "reason"],
            "properties": {
                "id": {"type": "string", "description": "Task UUID or short_id"},
                "reason": {"type": "string", "description": "Why Architect escalation is needed (e.g. architectural ambiguity, repeated Lead failures, codebase-wide impact)"}
            }
        }),
    )
}

fn tool_role_amend_prompt() -> RmcpTool {
    RmcpTool::new(
        "agent_amend_prompt".to_string(),
        "Append a prompt amendment to a specialist agent role's learned_prompt. The amendment is appended after existing content (never replacing it) and logged to learned_prompt_history. Only applicable to specialist roles (worker, reviewer base_role). Do NOT use on architect, lead, or planner roles.".to_string(),
        object!({
            "type": "object",
            "required": ["agent_id", "amendment"],
            "properties": {
                "project": {"type": "string", "description": "Absolute project path"},
                "agent_id": {"type": "string", "description": "Agent UUID or name to amend"},
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

fn from_value<T>(value: serde_json::Value) -> Result<T, serde_json::Error>
where
    T: DeserializeOwned,
{
    serde_json::from_value(value)
}

fn tool_ci_job_log() -> RmcpTool {
    RmcpTool::new(
        "ci_job_log".to_string(),
        "Fetch the full log for a GitHub Actions CI job. When CI fails, the activity log \
         tells you the job_id. Call this tool to see the actual error output. Optionally \
         filter to a specific failed step name. If the output is large, use output_view \
         or output_grep to navigate it."
            .to_string(),
        object!({
            "type": "object",
            "required": ["job_id"],
            "properties": {
                "job_id": {"type": "integer", "description": "The GitHub Actions job ID from the CI failure activity"},
                "step": {"type": "string", "description": "Optional step name to filter the log to (e.g. 'Tests')"}
            }
        }),
    )
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
    let mut tool_values = shared_base_tool_schemas();
    tool_values.push(serde_json::to_value(tool_shell()).expect("serialize tool_shell"));
    tool_values.push(serde_json::to_value(tool_read()).expect("serialize tool_read"));
    tool_values.push(serde_json::to_value(tool_lsp()).expect("serialize tool_lsp"));
    tool_values.push(serde_json::to_value(tool_ci_job_log()).expect("serialize tool_ci_job_log"));
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
    tool_values
        .push(serde_json::to_value(tool_request_lead()).expect("serialize tool_request_lead"));
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
    for value in shared_pm_tool_schemas() {
        tool_values.push(value);
    }
    for value in [
        serde_json::to_value(tool_task_delete_branch()).expect("serialize tool_task_delete_branch"),
        serde_json::to_value(tool_task_archive_activity())
            .expect("serialize tool_task_archive_activity"),
        serde_json::to_value(tool_task_reset_counters())
            .expect("serialize tool_task_reset_counters"),
        serde_json::to_value(tool_task_kill_session()).expect("serialize tool_task_kill_session"),
        serde_json::to_value(tool_request_architect()).expect("serialize tool_request_architect"),
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
    let mut tool_values = base_tool_schemas();
    for value in shared_pm_tool_schemas() {
        tool_values.push(value);
    }
    tool_values.push(
        serde_json::to_value(tool_task_transition()).expect("serialize tool_task_transition"),
    );
    for value in [
        serde_json::to_value(tool_task_delete_branch()).expect("serialize tool_task_delete_branch"),
        serde_json::to_value(tool_task_archive_activity())
            .expect("serialize tool_task_archive_activity"),
        serde_json::to_value(tool_task_reset_counters())
            .expect("serialize tool_task_reset_counters"),
        serde_json::to_value(tool_task_kill_session()).expect("serialize tool_task_kill_session"),
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
    for value in shared_pm_tool_schemas() {
        tool_values.push(value);
    }
    tool_values.push(
        serde_json::to_value(tool_task_transition()).expect("serialize tool_task_transition"),
    );
    tool_values.push(
        serde_json::to_value(shared_schemas::tool_task_comment_add())
            .expect("serialize tool_task_comment_add"),
    );
    tool_values.push(
        serde_json::to_value(shared_schemas::tool_memory_build_context())
            .expect("serialize tool_memory_build_context"),
    );
    tool_values.push(
        serde_json::to_value(shared_schemas::tool_role_metrics())
            .expect("serialize tool_role_metrics"),
    );
    tool_values.push(
        serde_json::to_value(shared_schemas::tool_role_create())
            .expect("serialize tool_role_create"),
    );
    for value in [
        serde_json::to_value(tool_task_delete_branch()).expect("serialize tool_task_delete_branch"),
        serde_json::to_value(tool_task_archive_activity())
            .expect("serialize tool_task_archive_activity"),
        serde_json::to_value(tool_task_reset_counters())
            .expect("serialize tool_task_reset_counters"),
        serde_json::to_value(tool_task_kill_session()).expect("serialize tool_task_kill_session"),
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
    session_role: Option<&str>,
) -> Result<serde_json::Value, String> {
    let synthetic = serde_json::json!({ "name": name, "arguments": arguments });
    dispatch_tool_call(
        state,
        &synthetic,
        worktree_path,
        None,
        session_task_id,
        session_role,
    )
    .await
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentType;
    use crate::test_helpers::create_test_db;
    use crate::test_helpers::{
        agent_context_from_db, create_test_epic, create_test_project, create_test_task,
    };
    use djinn_core::events::EventBus;
    use djinn_db::NoteRepository;
    use std::path::{Path, PathBuf};
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
        let worktree = crate::test_helpers::test_tempdir("djinn-ext-worktree-");
        let outside = crate::test_helpers::test_tempdir("djinn-ext-outside-");
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

    #[tokio::test]
    async fn call_tool_dispatches_task_create_with_public_response_shape() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let mut state = agent_context_from_db(db.clone(), CancellationToken::new());
        state.task_ops_project_path_override = Some(project.path.clone().into());

        let response = call_tool(
            &state,
            "task_create",
            Some(
                serde_json::json!({
                    "epic_id": epic.short_id,
                    "title": "Dispatch-created task",
                    "description": "Created through extension dispatch",
                    "design": "Keep the response shape stable",
                    "priority": 3,
                    "owner": "planner",
                    "acceptance_criteria": ["first criterion"],
                    "memory_refs": ["decisions/adr-041-unified-tool-service-layer-in-djinn-mcp"],
                    "agent_type": "rust-expert"
                })
                .as_object()
                .expect("task_create args object")
                .clone(),
            ),
            Path::new(&project.path),
            None,
            Some("planner"),
        )
        .await
        .expect("task_create dispatch should succeed");

        assert_eq!(
            response.get("title").and_then(|v| v.as_str()),
            Some("Dispatch-created task")
        );
        assert_eq!(
            response.get("description").and_then(|v| v.as_str()),
            Some("Created through extension dispatch")
        );
        assert_eq!(response.get("priority").and_then(|v| v.as_i64()), Some(3));
        assert_eq!(
            response.get("owner").and_then(|v| v.as_str()),
            Some("planner")
        );
        assert_eq!(
            response.get("status").and_then(|v| v.as_str()),
            Some("open")
        );
        assert_eq!(response.get("agent_type").and_then(|v| v.as_str()), None);
        assert_eq!(
            response
                .get("acceptance_criteria")
                .and_then(|v| v.as_array())
                .and_then(|items| items.first())
                .and_then(|item| item
                    .as_str()
                    .or_else(|| item.get("criterion").and_then(|v| v.as_str()))),
            Some("first criterion")
        );
        assert_eq!(
            response
                .get("memory_refs")
                .and_then(|v| v.as_array())
                .and_then(|items| items.first())
                .and_then(|v| v.as_str()),
            Some("decisions/adr-041-unified-tool-service-layer-in-djinn-mcp")
        );
    }

    #[tokio::test]
    async fn call_tool_dispatches_task_update_with_public_response_shape() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let mut state = agent_context_from_db(db.clone(), CancellationToken::new());
        state.task_ops_project_path_override = Some(project.path.clone().into());

        let response = call_tool(
            &state,
            "task_update",
            Some(
                serde_json::json!({
                    "id": task.short_id,
                    "title": "Dispatch-updated task",
                    "description": "Updated through extension dispatch",
                    "design": "Keep the update response shape stable",
                    "priority": 2,
                    "owner": "planner",
                    "labels_add": ["migration-test"],
                    "acceptance_criteria": [{"criterion": "updated criterion", "met": false}],
                    "memory_refs_add": ["decisions/adr-041-unified-tool-service-layer-in-djinn-mcp"]
                })
                .as_object()
                .expect("task_update args object")
                .clone(),
            ),
            Path::new(&project.path),
            Some(&task.id),
            Some("planner"),
        )
        .await
        .expect("task_update dispatch should succeed");

        assert_eq!(
            response.get("id").and_then(|v| v.as_str()),
            Some(task.id.as_str())
        );
        assert_eq!(
            response.get("short_id").and_then(|v| v.as_str()),
            Some(task.short_id.as_str())
        );
        assert_eq!(
            response.get("title").and_then(|v| v.as_str()),
            Some("Dispatch-updated task")
        );
        assert_eq!(
            response.get("description").and_then(|v| v.as_str()),
            Some("Updated through extension dispatch")
        );
        assert_eq!(
            response.get("design").and_then(|v| v.as_str()),
            Some("Keep the update response shape stable")
        );
        assert_eq!(response.get("priority").and_then(|v| v.as_i64()), Some(2));
        assert_eq!(
            response.get("owner").and_then(|v| v.as_str()),
            Some("planner")
        );
        assert_eq!(
            response
                .get("labels")
                .and_then(|v| v.as_array())
                .map(|labels| labels
                    .iter()
                    .filter_map(|value| value.as_str())
                    .collect::<Vec<_>>()),
            Some(vec!["migration-test"])
        );
        assert_eq!(
            response
                .get("acceptance_criteria")
                .and_then(|v| v.as_array())
                .and_then(|items| items.first())
                .and_then(|item| item
                    .as_str()
                    .or_else(|| item.get("criterion").and_then(|v| v.as_str()))),
            Some("updated criterion")
        );
        assert_eq!(
            response
                .get("memory_refs")
                .and_then(|v| v.as_array())
                .and_then(|items| items.first())
                .and_then(|v| v.as_str()),
            Some("decisions/adr-041-unified-tool-service-layer-in-djinn-mcp")
        );
    }

    #[tokio::test]
    async fn call_tool_dispatches_comment_and_transition_flows() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let mut state = agent_context_from_db(db.clone(), CancellationToken::new());
        state.task_ops_project_path_override = Some(project.path.clone().into());

        let comment = call_tool(
            &state,
            "task_comment_add",
            Some(
                serde_json::json!({
                    "id": task.short_id,
                    "body": "Dispatch-level architect note"
                })
                .as_object()
                .expect("task_comment_add args object")
                .clone(),
            ),
            Path::new(&project.path),
            Some(&task.id),
            Some("architect"),
        )
        .await
        .expect("task_comment_add dispatch should succeed");

        assert_eq!(
            comment.get("task_id").and_then(|v| v.as_str()),
            Some(task.id.as_str())
        );
        assert_eq!(
            comment.get("actor_id").and_then(|v| v.as_str()),
            Some("architect")
        );
        assert_eq!(
            comment.get("actor_role").and_then(|v| v.as_str()),
            Some("architect")
        );
        assert_eq!(
            comment.get("event_type").and_then(|v| v.as_str()),
            Some("comment")
        );
        assert_eq!(
            comment
                .get("payload")
                .and_then(|v| v.get("body"))
                .and_then(|v| v.as_str()),
            Some("Dispatch-level architect note")
        );

        let transitioned = call_tool(
            &state,
            "task_transition",
            Some(
                serde_json::json!({
                    "id": task.short_id,
                    "action": "start"
                })
                .as_object()
                .expect("task_transition args object")
                .clone(),
            ),
            Path::new(&project.path),
            Some(&task.id),
            Some("lead"),
        )
        .await
        .expect("task_transition dispatch should succeed");

        assert_eq!(
            transitioned.get("id").and_then(|v| v.as_str()),
            Some(task.id.as_str())
        );
        assert_eq!(
            transitioned.get("short_id").and_then(|v| v.as_str()),
            Some(task.short_id.as_str())
        );
        assert_eq!(
            transitioned.get("status").and_then(|v| v.as_str()),
            Some("in_progress")
        );
        assert_eq!(
            transitioned.get("title").and_then(|v| v.as_str()),
            Some(task.title.as_str())
        );
    }

    #[tokio::test]
    async fn call_tool_dispatches_agent_ops_through_shared_agent_seam() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let state = agent_context_from_db(db.clone(), CancellationToken::new());

        let create_response = call_tool(
            &state,
            "agent_create",
            Some(
                serde_json::json!({
                    "project": project.path,
                    "name": "Rust specialist",
                    "base_role": "worker",
                    "description": "Handles Rust-heavy tasks",
                    "system_prompt_extensions": "Focus on Rust diagnostics",
                    "model_preference": "gpt-5"
                })
                .as_object()
                .expect("agent_create args object")
                .clone(),
            ),
            Path::new(&project.path),
            None,
            Some("architect"),
        )
        .await
        .expect("agent_create dispatch should succeed");

        assert_eq!(
            create_response
                .get("agent_name")
                .and_then(|value| value.as_str()),
            Some("Rust specialist")
        );
        assert_eq!(
            create_response
                .get("base_role")
                .and_then(|value| value.as_str()),
            Some("worker")
        );
        assert_eq!(
            create_response
                .get("created")
                .and_then(|value| value.as_bool()),
            Some(true)
        );
        let created_agent_id = create_response
            .get("agent_id")
            .and_then(|value| value.as_str())
            .expect("agent id in create response")
            .to_string();

        let metrics_response = call_tool(
            &state,
            "agent_metrics",
            Some(
                serde_json::json!({
                    "project": project.path,
                    "agent_id": created_agent_id,
                    "window_days": 14
                })
                .as_object()
                .expect("agent_metrics args object")
                .clone(),
            ),
            Path::new(&project.path),
            None,
            Some("architect"),
        )
        .await
        .expect("agent_metrics dispatch should succeed");

        assert_eq!(
            metrics_response
                .get("window_days")
                .and_then(|value| value.as_i64()),
            Some(14)
        );
        let roles = metrics_response
            .get("roles")
            .and_then(|value| value.as_array())
            .expect("roles array in metrics response");
        assert_eq!(roles.len(), 1);
        assert_eq!(
            roles[0].get("agent_name").and_then(|value| value.as_str()),
            Some("Rust specialist")
        );
        assert_eq!(
            roles[0].get("base_role").and_then(|value| value.as_str()),
            Some("worker")
        );
        assert!(roles[0].get("learned_prompt").is_some());
    }

    #[tokio::test]
    async fn call_tool_dispatches_memory_ops_through_shared_memory_seam() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic = create_test_epic(&db, &project.id).await;
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let mut state = agent_context_from_db(db.clone(), CancellationToken::new());
        state.task_ops_project_path_override = Some(project.path.clone().into());

        let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
        let seed = note_repo
            .create(
                &project.id,
                Path::new(&project.path),
                "Shared Memory Seed",
                "Architecture guidance with [[Shared Memory Related]] references.",
                "adr",
                "[]",
            )
            .await
            .expect("create seed note");
        note_repo
            .create(
                &project.id,
                Path::new(&project.path),
                "Shared Memory Related",
                "Related architecture context.",
                "reference",
                "[]",
            )
            .await
            .expect("create related note");

        let search_response = call_tool(
            &state,
            "memory_search",
            Some(
                serde_json::json!({
                    "project": project.path,
                    "query": "architecture",
                    "limit": 5
                })
                .as_object()
                .expect("memory_search args object")
                .clone(),
            ),
            Path::new(&project.path),
            Some(&task.id),
            Some("architect"),
        )
        .await
        .expect("memory_search dispatch should succeed");
        assert!(
            search_response.get("error").is_none()
                || search_response
                    .get("error")
                    .is_some_and(|value| value.is_null())
        );
        assert!(
            search_response
                .get("results")
                .and_then(|value| value.as_array())
                .is_some_and(|results| !results.is_empty())
        );

        let read_response = call_tool(
            &state,
            "memory_read",
            Some(
                serde_json::json!({
                    "project": project.path,
                    "identifier": seed.permalink
                })
                .as_object()
                .expect("memory_read args object")
                .clone(),
            ),
            Path::new(&project.path),
            Some(&task.id),
            Some("architect"),
        )
        .await
        .expect("memory_read dispatch should succeed");
        assert!(
            read_response.get("error").is_none()
                || read_response
                    .get("error")
                    .is_some_and(|value| value.is_null())
        );
        assert_eq!(
            read_response
                .get("permalink")
                .and_then(|value| value.as_str()),
            Some(seed.permalink.as_str())
        );

        let list_response = call_tool(
            &state,
            "memory_list",
            Some(
                serde_json::json!({
                    "project": project.path,
                    "folder": "decisions",
                    "depth": 1
                })
                .as_object()
                .expect("memory_list args object")
                .clone(),
            ),
            Path::new(&project.path),
            Some(&task.id),
            Some("architect"),
        )
        .await
        .expect("memory_list dispatch should succeed");
        assert!(
            list_response.get("error").is_none()
                || list_response
                    .get("error")
                    .is_some_and(|value| value.is_null())
        );
        assert!(
            list_response
                .get("notes")
                .and_then(|value| value.as_array())
                .is_some_and(|notes| !notes.is_empty())
        );

        let context_response = call_tool(
            &state,
            "memory_build_context",
            Some(
                serde_json::json!({
                    "project": project.path,
                    "url": format!("memory://{}", seed.permalink),
                    "budget": 512,
                    "max_related": 5
                })
                .as_object()
                .expect("memory_build_context args object")
                .clone(),
            ),
            Path::new(&project.path),
            Some(&task.id),
            Some("architect"),
        )
        .await
        .expect("memory_build_context dispatch should succeed");
        assert!(
            context_response.get("error").is_none()
                || context_response
                    .get("error")
                    .is_some_and(|value| value.is_null())
        );
        assert_eq!(
            context_response
                .get("primary")
                .and_then(|value| value.as_array())
                .map(|items| items.len()),
            Some(1)
        );
    }

    #[test]
    fn worker_cannot_use_pm_only_tool() {
        // submit_decision is PM-only (ADR-036: finalize tools are role-specific).
        assert!(!is_tool_allowed_for_agent(
            AgentType::Worker,
            "submit_decision"
        ));
        assert!(is_tool_allowed_for_agent(
            AgentType::Lead,
            "submit_decision"
        ));
        // task_transition is not in the PM tool set (removed by ADR-036).
        assert!(!is_tool_allowed_for_agent(
            AgentType::Lead,
            "task_transition"
        ));
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
        let worktree = crate::test_helpers::test_tempdir("djinn-ext-worktree-");
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
        let worktree = crate::test_helpers::test_tempdir("djinn-ext-worktree-");
        let outside = crate::test_helpers::test_tempdir("djinn-ext-outside-");
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
        let worktree = crate::test_helpers::test_tempdir("djinn-ext-resolve-");
        let base = worktree.path();

        let relative = resolve_path("src/main.rs", base);
        assert_eq!(relative, base.join("src/main.rs"));

        let absolute = resolve_path("/etc/hosts", base);
        assert_eq!(absolute, PathBuf::from("/etc/hosts"));

        let normalized = resolve_path("./src/../Cargo.toml", base);
        assert_eq!(normalized, base.join("Cargo.toml"));
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

    #[tokio::test]
    async fn epic_extension_handlers_match_shared_epic_ops_behavior() {
        let db = create_test_db();
        let project = create_test_project(&db).await;
        let epic_repo = EpicRepository::new(db.clone(), EventBus::noop());
        let epic = epic_repo
            .update(
                &create_test_epic(&db, &project.id).await.id,
                djinn_db::EpicUpdateInput {
                    title: "test-epic",
                    description: "test epic description",
                    emoji: "🧪",
                    color: "#0000ff",
                    owner: "test-owner",
                    memory_refs: Some("[]"),
                },
            )
            .await
            .expect("normalize test epic color");
        let task = create_test_task(&db, &project.id, &epic.id).await;
        let state = agent_context_from_db(db, CancellationToken::new());

        let show_args = Some(
            serde_json::json!({
                "project": project.path,
                "id": epic.short_id,
            })
            .as_object()
            .expect("show args object")
            .clone(),
        );
        let show_value = call_epic_show(&state, &show_args)
            .await
            .expect("epic_show succeeds");
        assert_eq!(show_value["id"], epic.id);
        assert_eq!(show_value["task_count"], serde_json::json!(1));
        assert!(show_value.get("error").is_none());

        let update_args = Some(
            serde_json::json!({
                "project": project.path,
                "id": epic.short_id,
                "title": "updated epic title",
                "description": "updated epic description",
                "status": "ignored-by-extension-contract",
                "memory_refs_add": ["notes/adr-041"],
            })
            .as_object()
            .expect("update args object")
            .clone(),
        );
        let update_value = call_epic_update(&state, &update_args)
            .await
            .expect("epic_update succeeds");
        let epic_model: djinn_mcp::tools::epic_ops::EpicSingleResponse =
            serde_json::from_value(update_value.clone()).expect("parse epic update response");
        let epic_model = epic_model.epic.expect("updated epic payload");
        assert_eq!(epic_model.title, "updated epic title");
        assert_eq!(epic_model.description, "updated epic description");
        assert_eq!(epic_model.memory_refs, vec!["notes/adr-041".to_string()]);
        assert!(update_value.get("error").is_none());

        let tasks_args = Some(
            serde_json::json!({
                "project": project.path,
                "id": epic.short_id,
                "limit": 10,
                "offset": 0,
            })
            .as_object()
            .expect("tasks args object")
            .clone(),
        );
        let tasks_value = call_epic_tasks(&state, &tasks_args)
            .await
            .expect("epic_tasks succeeds");
        assert_eq!(tasks_value["total"], serde_json::json!(1));
        assert_eq!(tasks_value["limit"], serde_json::json!(10));
        assert_eq!(tasks_value["offset"], serde_json::json!(0));
        assert_eq!(tasks_value["has_more"], serde_json::json!(false));
        assert_eq!(tasks_value["tasks"][0]["id"], task.id);
        assert!(tasks_value.get("total_count").is_none());
        assert!(tasks_value.get("error").is_none());
    }
}
