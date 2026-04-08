use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use rmcp::Json;

use crate::context::AgentContext;
use crate::lsp::format_diagnostics_xml;
use crate::lsp::{SymbolQuery, parse_symbol_kind_filter};
use crate::mcp_client::McpToolRegistry;
use djinn_db::AgentRepository;
use djinn_db::EpicRepository;
use djinn_db::SessionRepository;
use djinn_db::TaskRepository;
use djinn_mcp::tools::agent_tools::{
    AgentCreateParams as SharedAgentCreateParams, AgentMetricsParams as SharedAgentMetricsParams,
    create_agent as shared_create_agent, metrics_for_agents as shared_metrics_for_agents,
};
use djinn_mcp::tools::epic_ops::{EpicShowRequest, EpicTasksRequest, EpicUpdateDeltaRequest};
use djinn_mcp::tools::memory_tools::{
    BrokenLinksParams as SharedMemoryBrokenLinksParams,
    BuildContextParams as SharedMemoryBuildContextParams, EditParams as SharedMemoryEditParams,
    HealthParams as SharedMemoryHealthParams, ListParams as SharedMemoryListParams,
    OrphansParams as SharedMemoryOrphansParams, ReadParams as SharedMemoryReadParams,
    SearchParams as SharedMemorySearchParams, WriteParams as SharedMemoryWriteParams,
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

use super::fuzzy::fuzzy_replace;
use super::helpers::*;
use super::sandbox;
use super::types::*;

pub(super) async fn dispatch_tool_call<T>(
    state: &AgentContext,
    tool_call: &T,
    worktree_path: &Path,
    allowed_schemas: Option<&[serde_json::Value]>,
    session_task_id: Option<&str>,
    session_role: Option<&str>,
    mcp_registry: Option<&McpToolRegistry>,
) -> Result<serde_json::Value, String>
where
    T: Serialize,
{
    let call: IncomingToolCall =
        from_value(serde_json::to_value(tool_call).map_err(|e| e.to_string())?)
            .map_err(|e| format!("invalid frontend tool payload: {e}"))?;

    // Resolve project metadata from worktree_path so agent tools stay project-scoped
    // while memory tools can still target canonical project-root note files.
    let project = {
        let repo = djinn_db::ProjectRepository::new(state.db.clone(), state.event_bus.clone());
        let path_str = worktree_path.to_string_lossy();
        match repo.resolve(&path_str).await.ok().flatten() {
            Some(project_id) => repo.get(&project_id).await.ok().flatten(),
            None => None,
        }
    };
    let project_id = project.as_ref().map(|project| project.id.clone());
    let canonical_project_path = project
        .as_ref()
        .map(|project| project.path.clone())
        .unwrap_or_else(|| worktree_path.display().to_string());
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
        "request_planner" => call_request_planner(state, &call.arguments).await,
        "task_transition" => {
            call_task_transition(state, &call.arguments, &worktree_project_path).await
        }
        "task_delete_branch" => call_task_delete_branch(state, &call.arguments).await,
        "task_archive_activity" => call_task_archive_activity(state, &call.arguments).await,
        "task_reset_counters" => call_task_reset_counters(state, &call.arguments).await,
        "task_kill_session" => call_task_kill_session(state, &call.arguments).await,
        "task_blocked_list" => call_task_blocked_list(state, &call.arguments).await,
        "task_activity_list" => call_task_activity_list(state, &call.arguments).await,
        "epic_show" => call_epic_show(state, &call.arguments, project_id.as_deref()).await,
        "epic_update" => call_epic_update(state, &call.arguments, project_id.as_deref()).await,
        "epic_tasks" => call_epic_tasks(state, &call.arguments, project_id.as_deref()).await,
        "epic_close" => call_epic_close(state, &call.arguments, project_id.as_deref()).await,
        "memory_read" => call_memory_read(state, &call.arguments, &canonical_project_path).await,
        "memory_search" => {
            call_memory_search(
                state,
                &call.arguments,
                session_task_id,
                &canonical_project_path,
            )
            .await
        }
        "memory_list" => call_memory_list(state, &call.arguments, &canonical_project_path).await,
        "memory_build_context" => {
            call_memory_build_context(
                state,
                &call.arguments,
                session_task_id,
                &canonical_project_path,
            )
            .await
        }
        "memory_write" => {
            call_memory_write(
                state,
                &call.arguments,
                &canonical_project_path,
                worktree_path,
            )
            .await
        }
        "memory_edit" => {
            call_memory_edit(
                state,
                &call.arguments,
                &canonical_project_path,
                worktree_path,
            )
            .await
        }
        "memory_health" => {
            call_memory_health(state, &call.arguments, &canonical_project_path).await
        }
        "memory_broken_links" => {
            call_memory_broken_links(state, &call.arguments, &canonical_project_path).await
        }
        "memory_orphans" => {
            call_memory_orphans(state, &call.arguments, &canonical_project_path).await
        }
        "agent_metrics" => {
            call_agent_metrics(state, &call.arguments, &canonical_project_path).await
        }
        "agent_amend_prompt" => {
            call_agent_amend_prompt(state, &call.arguments, &worktree_project_path).await
        }
        "agent_create" => call_agent_create(state, &call.arguments, &worktree_project_path).await,
        "ci_job_log" => call_ci_job_log(state, &call.arguments, session_task_id).await,
        // ADR-050 Chunk C: route code-reading tools through `working_root` so
        // architect/chat sessions resolve them against the canonical
        // `.djinn/worktrees/_index/` checkout instead of the per-task worktree.
        // Workers leave `working_root` unset and continue to see their own
        // worktree state.  Write/edit/apply_patch always target the worktree —
        // only architect/chat omit those tools entirely.
        "shell" => {
            let root = state.working_root_for(worktree_path);
            call_shell(&call.arguments, &root).await
        }
        "read" => {
            let root = state.working_root_for(worktree_path);
            call_read(state, &call.arguments, &root).await
        }
        "write" => call_write(state, &call.arguments, worktree_path).await,
        "edit" => call_edit(state, &call.arguments, worktree_path).await,
        "apply_patch" => call_apply_patch(state, &call.arguments, worktree_path).await,
        "lsp" => {
            let root = state.working_root_for(worktree_path);
            call_lsp(state, &call.arguments, &root).await
        }
        "code_graph" => {
            let root = state.working_root_for(worktree_path);
            let root_str = root.to_string_lossy().into_owned();
            call_code_graph(state, &call.arguments, &root_str).await
        }
        "github_search" => call_github_search(state, &call.arguments).await,
        other => {
            if let Some(registry) = mcp_registry
                && registry.has_tool(other)
            {
                registry.call_tool(other, call.arguments.clone()).await
            } else {
                Err(format!("unknown djinn frontend tool: {other}"))
            }
        }
    }
}

pub(super) async fn call_task_list(
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

pub(super) async fn call_task_show(
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

pub(super) async fn call_task_activity_list(
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

pub(super) async fn call_epic_show(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    resolved_project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EpicShowParams = parse_args(arguments)?;
    let project_id = match resolved_project_id {
        Some(id) => id.to_string(),
        None => resolve_project_id_for_agent_tools(state, arguments).await?,
    };
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

pub(super) async fn call_epic_update(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    resolved_project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EpicUpdateParams = parse_args(arguments)?;
    let project_id = match resolved_project_id {
        Some(id) => id.to_string(),
        None => resolve_project_id_for_agent_tools(state, arguments).await?,
    };
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
            status: p.status,
        },
    )
    .await;
    serde_json::to_value(response).map_err(|e| e.to_string())
}

pub(super) async fn call_epic_tasks(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    resolved_project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EpicTasksParams = parse_args(arguments)?;
    let project_id = match resolved_project_id {
        Some(id) => id.to_string(),
        None => resolve_project_id_for_agent_tools(state, arguments).await?,
    };
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

pub(super) async fn call_epic_close(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    resolved_project_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let p: EpicShowParams = parse_args(arguments)?;
    let project_id = match resolved_project_id {
        Some(id) => id.to_string(),
        None => resolve_project_id_for_agent_tools(state, arguments).await?,
    };
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

pub(super) async fn call_task_create(
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
            acceptance_criteria: p.acceptance_criteria.map(|criteria| {
                criteria
                    .into_iter()
                    .map(|item| acceptance_criterion_to_string(&item))
                    .collect()
            }),
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

pub(super) async fn call_task_update(
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

pub(super) async fn call_task_update_ac(
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

pub(super) async fn call_request_lead(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    #[derive(serde::Deserialize)]
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
    // On the 2nd+ escalation for the same task, auto-route to Planner
    // (per ADR-051 §8 — Planner is now the escalation ceiling above Lead).
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
        let planner_reason = format!(
            "Auto-escalated to Planner after {} Lead escalations. Latest reason: {}",
            escalation_count, p.reason
        );
        if let Some(ref coord) = coordinator {
            let _ = coord
                .dispatch_planner_escalation(&task.id, &planner_reason, &task.project_id)
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
            "status": "planner_escalated",
            "task_id": updated.id,
            "new_status": updated.status,
            "escalation_count": escalation_count,
            "message": "Task has been escalated multiple times. Routing to Planner for board-level review. Your session should end now."
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

pub(super) async fn call_request_planner(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    #[derive(serde::Deserialize)]
    struct RequestPlannerParams {
        id: String,
        reason: String,
    }

    let p: RequestPlannerParams = parse_args(arguments)?;
    let repo = TaskRepository::new(state.db.clone(), state.event_bus.clone());

    let Some(task) = repo.resolve(&p.id).await.map_err(|e| e.to_string())? else {
        return Ok(serde_json::json!({ "error": format!("task not found: {}", p.id) }));
    };

    let body = format!("[PLANNER_REQUEST] Lead escalating to Planner. {}", p.reason);
    let payload = serde_json::json!({ "body": body }).to_string();
    repo.log_activity(Some(&task.id), "lead-agent", "lead", "comment", &payload)
        .await
        .map_err(|e| e.to_string())?;

    let Some(coordinator) = state.coordinator().await else {
        return Ok(serde_json::json!({
            "error": "coordinator not available — cannot dispatch Planner"
        }));
    };

    let _ = coordinator
        .dispatch_planner_escalation(&task.id, &p.reason, &task.project_id)
        .await;

    Ok(serde_json::json!({
        "status": "planner_dispatched",
        "task_id": task.id,
        "message": "Planner has been dispatched to review this task. Your session should end now."
    }))
}

pub(super) async fn call_task_comment_add(
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

pub(super) async fn call_memory_read(
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

pub(super) async fn call_memory_search(
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

pub(super) async fn call_memory_list(
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

pub(super) async fn call_memory_build_context(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    session_task_id: Option<&str>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryBuildContextParams = parse_args(arguments)?;
    let project_path = project_path.to_owned();
    let task_id = p.task_id.or_else(|| session_task_id.map(ToOwned::to_owned));
    let url = p.url.unwrap_or_else(|| "/*".to_string());
    let max_related = p.max_related.or(p.limit);
    let server = djinn_mcp::server::DjinnMcpServer::new(state.to_mcp_state());
    Ok(serde_json::to_value(
        djinn_mcp::tools::memory_tools::ops::memory_build_context(
            &server,
            SharedMemoryBuildContextParams {
                project: project_path,
                url,
                depth: None,
                max_related,
                budget: p.budget,
                task_id: task_id.clone(),
                min_confidence: p.min_confidence,
            },
            task_id.as_deref(),
        )
        .await,
    )
    .unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_build_context response" }),
    ))
}

pub(super) async fn call_memory_health(
    state: &AgentContext,
    _arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let server = djinn_mcp::server::DjinnMcpServer::new(state.to_mcp_state());
    Ok(serde_json::to_value(
        djinn_mcp::tools::memory_tools::ops::memory_health(
            &server,
            SharedMemoryHealthParams {
                project: Some(project_path.to_owned()),
            },
        )
        .await,
    )
    .unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_health response" }),
    ))
}

pub(super) async fn call_memory_write(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
    worktree_root: &Path,
) -> Result<serde_json::Value, String> {
    let p: MemoryWriteParams = parse_args(arguments)?;
    let project_path = project_path.to_owned();
    let server = djinn_mcp::server::DjinnMcpServer::new(state.to_mcp_state());
    let worktree_root = Some(worktree_root.to_path_buf());
    let result = server
        .memory_write_with_worktree(
            rmcp::handler::server::wrapper::Parameters(SharedMemoryWriteParams {
                project: project_path,
                title: p.title,
                content: p.content,
                note_type: p.note_type,
                tags: p.tags,
                scope_paths: p.scope_paths,
            }),
            worktree_root,
        )
        .await;
    Ok(serde_json::to_value(result.0).unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_write response" }),
    ))
}

pub(super) async fn call_memory_edit(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
    worktree_root: &Path,
) -> Result<serde_json::Value, String> {
    let p: MemoryEditParams = parse_args(arguments)?;
    let project_path = project_path.to_owned();
    let server = djinn_mcp::server::DjinnMcpServer::new(state.to_mcp_state());
    let worktree_root = Some(worktree_root.to_path_buf());
    let result = server
        .memory_edit_with_worktree(
            rmcp::handler::server::wrapper::Parameters(SharedMemoryEditParams {
                project: project_path,
                identifier: p.identifier,
                operation: p.operation,
                content: p.content,
                find_text: p.find_text,
                section: p.section,
                note_type: p.note_type,
            }),
            worktree_root,
        )
        .await;
    Ok(serde_json::to_value(result.0).unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_edit response" }),
    ))
}

pub(super) async fn call_memory_broken_links(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryBrokenLinksLocalParams = parse_args(arguments)?;
    let server = djinn_mcp::server::DjinnMcpServer::new(state.to_mcp_state());
    Ok(serde_json::to_value(
        djinn_mcp::tools::memory_tools::ops::memory_broken_links(
            &server,
            SharedMemoryBrokenLinksParams {
                project: project_path.to_owned(),
                folder: p.folder,
            },
        )
        .await,
    )
    .unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_broken_links response" }),
    ))
}

pub(super) async fn call_memory_orphans(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: MemoryOrphansLocalParams = parse_args(arguments)?;
    let server = djinn_mcp::server::DjinnMcpServer::new(state.to_mcp_state());
    Ok(serde_json::to_value(
        djinn_mcp::tools::memory_tools::ops::memory_orphans(
            &server,
            SharedMemoryOrphansParams {
                project: project_path.to_owned(),
                folder: p.folder,
            },
        )
        .await,
    )
    .unwrap_or_else(
        |_| serde_json::json!({ "error": "failed to serialize memory_orphans response" }),
    ))
}

pub(super) async fn call_agent_metrics(
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
        window_days: raw.get("window_days").and_then(|v| v.as_i64()),
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
                "extraction_quality": entry.extraction_quality,
            })
        })
        .collect::<Vec<_>>();

    Ok(serde_json::json!({
        "roles": roles,
        "window_days": response.window_days,
    }))
}

pub(super) async fn call_agent_amend_prompt(
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

pub(super) async fn call_agent_create(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let project_id = project_id_for_path(state, project_path).await?;

    let mut raw = arguments.clone().unwrap_or_default();
    // Inject project so the shared params struct deserialises.
    raw.entry("project")
        .or_insert_with(|| serde_json::json!(project_path));
    let params: SharedAgentCreateParams = serde_json::from_value(serde_json::Value::Object(raw))
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

/// Fetch a GitHub Actions job log, optionally filtered to a specific step.
///
/// The raw log is cleaned (timestamps stripped, group markers removed) and
/// returned as-is. When the result exceeds the tool-result size limit, the
/// reply-loop automatically stashes the full output and the worker can
/// paginate with `output_view` / `output_grep`.
pub(super) async fn call_ci_job_log(
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

pub(crate) async fn call_shell(
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

pub(crate) async fn call_read(
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

pub(super) async fn call_write(
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

            // Signal repo-map watcher to refresh this worktree's SCIP index.
            state.event_bus.send(
                djinn_core::events::DjinnEventEnvelope::repo_map_refresh_requested(
                    &worktree_path.display().to_string(),
                    Some(&worktree_path.display().to_string()),
                ),
            );

            Ok(serde_json::json!({
                "ok": true,
                "path": path.display().to_string(),
                "bytes": p.content.len(),
                "diagnostics": diag_xml,
            }))
        })
        .await
}

pub(super) async fn call_edit(
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

            // Signal repo-map watcher to refresh this worktree's SCIP index.
            state.event_bus.send(
                djinn_core::events::DjinnEventEnvelope::repo_map_refresh_requested(
                    &worktree_path.display().to_string(),
                    Some(&worktree_path.display().to_string()),
                ),
            );

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

pub(super) async fn call_apply_patch(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
) -> Result<serde_json::Value, String> {
    let p: ApplyPatchParams = parse_args(arguments)?;

    // Parse the custom patch format
    let parsed = crate::patch::parse_patch(&p.patch)?;

    let worktree_key = worktree_path.display().to_string();

    // Validate all paths are within worktree and assert FileTime for updates/deletes
    for op in &parsed.operations {
        let raw_path = op.path();
        let resolved = resolve_path(raw_path, worktree_path);
        ensure_path_within_worktree(&resolved, worktree_path)?;

        match op {
            crate::patch::FileOp::Update { .. } | crate::patch::FileOp::Delete { .. } => {
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
            crate::patch::FileOp::Add { .. } => {
                // New files don't need FileTime assertion
            }
        }
    }

    // Apply all patch operations
    let results = crate::patch::apply_patch(&parsed, worktree_path).await?;

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

    // Signal repo-map watcher to refresh this worktree's SCIP index.
    if !affected.is_empty() {
        state.event_bus.send(
            djinn_core::events::DjinnEventEnvelope::repo_map_refresh_requested(
                &worktree_path.display().to_string(),
                Some(&worktree_path.display().to_string()),
            ),
        );
    }

    Ok(serde_json::json!({
        "ok": true,
        "files": affected,
        "diagnostics": diag_xml,
    }))
}

pub(crate) async fn call_lsp(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_path: &Path,
) -> Result<serde_json::Value, String> {
    let p: LspParams = parse_args(arguments)?;
    validate_symbol_only_params(p.operation.as_str(), &p)?;
    let path = resolve_path(&p.file_path, worktree_path);

    match p.operation.as_str() {
        "hover" => {
            let result = match (&p.symbol, p.line, p.character) {
                (Some(symbol), None, None) => {
                    state.lsp.hover_symbol(worktree_path, &path, symbol).await?
                }
                (None, Some(line), Some(character)) => {
                    // LSP uses 0-based positions; accept 1-based from agents
                    state
                        .lsp
                        .hover(
                            worktree_path,
                            &path,
                            line.saturating_sub(1),
                            character.saturating_sub(1),
                        )
                        .await?
                }
                (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
                    return Err(
                        "hover accepts either symbol or line+character, but not both".to_string(),
                    );
                }
                (None, Some(_), None) | (None, None, Some(_)) => {
                    return Err(
                        "hover requires both line and character when symbol is omitted".to_string(),
                    );
                }
                (None, None, None) => {
                    return Err("hover requires either symbol or line+character".to_string());
                }
            };
            Ok(serde_json::json!({ "operation": "hover", "result": result }))
        }
        "definition" => {
            let result = match (&p.symbol, p.line, p.character) {
                (Some(symbol), None, None) => {
                    state
                        .lsp
                        .go_to_definition_symbol(worktree_path, &path, symbol)
                        .await?
                }
                (None, Some(line), Some(character)) => {
                    state
                        .lsp
                        .go_to_definition(
                            worktree_path,
                            &path,
                            line.saturating_sub(1),
                            character.saturating_sub(1),
                        )
                        .await?
                }
                (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
                    return Err(
                        "definition accepts either symbol or line+character, but not both"
                            .to_string(),
                    );
                }
                (None, Some(_), None) | (None, None, Some(_)) => {
                    return Err(
                        "definition requires both line and character when symbol is omitted"
                            .to_string(),
                    );
                }
                (None, None, None) => {
                    return Err("definition requires either symbol or line+character".to_string());
                }
            };
            Ok(serde_json::json!({ "operation": "definition", "result": result }))
        }
        "references" => {
            let result = match (&p.symbol, p.line, p.character) {
                (Some(symbol), None, None) => {
                    state
                        .lsp
                        .find_references_symbol(worktree_path, &path, symbol)
                        .await?
                }
                (None, Some(line), Some(character)) => {
                    state
                        .lsp
                        .find_references(
                            worktree_path,
                            &path,
                            line.saturating_sub(1),
                            character.saturating_sub(1),
                        )
                        .await?
                }
                (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
                    return Err(
                        "references accepts either symbol or line+character, but not both"
                            .to_string(),
                    );
                }
                (None, Some(_), None) | (None, None, Some(_)) => {
                    return Err(
                        "references requires both line and character when symbol is omitted"
                            .to_string(),
                    );
                }
                (None, None, None) => {
                    return Err("references requires either symbol or line+character".to_string());
                }
            };
            Ok(serde_json::json!({ "operation": "references", "result": result }))
        }
        "symbols" => {
            let query = SymbolQuery {
                depth: p.depth,
                kinds: p
                    .kind
                    .as_deref()
                    .map(parse_symbol_kind_filter)
                    .transpose()?,
                name_filter: p.name_filter,
            };
            let result = state
                .lsp
                .document_symbols(worktree_path, &path, query)
                .await?;
            Ok(serde_json::json!({ "operation": "symbols", "result": result }))
        }
        other => Err(format!(
            "unknown LSP operation: {other}. Use: hover, definition, references, or symbols"
        )),
    }
}

pub(crate) async fn call_code_graph(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    project_path: &str,
) -> Result<serde_json::Value, String> {
    let p: CodeGraphParams = parse_args(arguments)?;
    let mcp_state = state.to_mcp_state();
    let graph_ops = mcp_state.repo_graph();
    // Use worktree project path if user did not supply an explicit project_path
    let effective_path = if p.project_path.is_empty() {
        project_path
    } else {
        &p.project_path
    };

    let result: serde_json::Value = match p.operation.as_str() {
        "neighbors" => {
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'neighbors'")?;
            let neighbors = graph_ops
                .neighbors(
                    effective_path,
                    key,
                    p.direction.as_deref(),
                    p.group_by.as_deref(),
                )
                .await?;
            serde_json::to_value(&neighbors).map_err(|e| format!("serialize error: {e}"))?
        }
        "ranked" => {
            let limit = p.limit.unwrap_or(20);
            let ranked = graph_ops
                .ranked(
                    effective_path,
                    p.kind_filter.as_deref(),
                    p.sort_by.as_deref(),
                    limit,
                )
                .await?;
            serde_json::to_value(&ranked).map_err(|e| format!("serialize error: {e}"))?
        }
        "implementations" => {
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'implementations'")?;
            let impls = graph_ops.implementations(effective_path, key).await?;
            serde_json::to_value(&impls).map_err(|e| format!("serialize error: {e}"))?
        }
        "impact" => {
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'impact'")?;
            let depth = p.limit.unwrap_or(3);
            let impact = graph_ops
                .impact(effective_path, key, depth, p.group_by.as_deref())
                .await?;
            serde_json::to_value(&impact).map_err(|e| format!("serialize error: {e}"))?
        }
        "search" => {
            let query = p
                .query
                .as_deref()
                .filter(|q| !q.is_empty())
                .ok_or("'query' is required for 'search'")?;
            let limit = p.limit.unwrap_or(20);
            let hits = graph_ops
                .search(effective_path, query, p.kind_filter.as_deref(), limit)
                .await?;
            serde_json::to_value(&hits).map_err(|e| format!("serialize error: {e}"))?
        }
        "cycles" => {
            let min_size = p.min_size.unwrap_or(2);
            let cycles = graph_ops
                .cycles(effective_path, p.kind_filter.as_deref(), min_size)
                .await?;
            serde_json::to_value(&cycles).map_err(|e| format!("serialize error: {e}"))?
        }
        "orphans" => {
            let limit = p.limit.unwrap_or(50);
            let orphans = graph_ops
                .orphans(
                    effective_path,
                    p.kind_filter.as_deref(),
                    p.visibility.as_deref(),
                    limit,
                )
                .await?;
            serde_json::to_value(&orphans).map_err(|e| format!("serialize error: {e}"))?
        }
        "path" => {
            let from = p
                .from
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or("'from' is required for 'path'")?;
            let to =
                p.to.as_deref()
                    .filter(|s| !s.is_empty())
                    .ok_or("'to' is required for 'path'")?;
            let path = graph_ops
                .path(effective_path, from, to, p.max_depth)
                .await?;
            serde_json::to_value(&path).map_err(|e| format!("serialize error: {e}"))?
        }
        "edges" => {
            let from_glob = p
                .from_glob
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or("'from_glob' is required for 'edges'")?;
            let to_glob = p
                .to_glob
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or("'to_glob' is required for 'edges'")?;
            let limit = p.limit.unwrap_or(100);
            let edges = graph_ops
                .edges(
                    effective_path,
                    from_glob,
                    to_glob,
                    p.edge_kind.as_deref(),
                    limit,
                )
                .await?;
            serde_json::to_value(&edges).map_err(|e| format!("serialize error: {e}"))?
        }
        "diff" => {
            let diff = graph_ops.diff(effective_path, p.since.as_deref()).await?;
            serde_json::to_value(&diff).map_err(|e| format!("serialize error: {e}"))?
        }
        "describe" => {
            let key = p
                .key
                .as_deref()
                .filter(|k| !k.is_empty())
                .ok_or("'key' is required for 'describe'")?;
            let description = graph_ops.describe(effective_path, key).await?;
            serde_json::to_value(&description).map_err(|e| format!("serialize error: {e}"))?
        }
        other => {
            return Err(format!(
                "unknown code_graph operation '{other}': expected one of \
                 'neighbors', 'ranked', 'impact', 'implementations', \
                 'search', 'cycles', 'orphans', 'path', 'edges', 'diff', 'describe'"
            ));
        }
    };
    Ok(result)
}

// ---------------------------------------------------------------------------
// github_search — search GitHub code via grep.app
// ---------------------------------------------------------------------------

pub(crate) async fn call_github_search(
    _state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    // Fresh client per call (mirrors the grep-mcp reference impl, which creates a
    // new aiohttp ClientSession per request) plus a browser-style User-Agent —
    // a custom `djinn-agent/0.1` UA on a long-lived pooled connection was getting
    // 429'd by grep.app even at <1 req/hr, so we avoid both signals.
    let client = reqwest::Client::builder()
        .user_agent(
            "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36",
        )
        .build()
        .map_err(|e| format!("http client build failed: {e}"))?;
    let params: GithubSearchParams = parse_args(arguments)?;
    super::github_search::search(
        &client,
        &params.query,
        params.language.as_deref(),
        params.repo.as_deref(),
        params.path.as_deref(),
    )
    .await
}

pub(super) async fn call_task_transition(
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

pub(super) async fn call_task_delete_branch(
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

pub(super) async fn call_task_archive_activity(
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

pub(super) async fn call_task_reset_counters(
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

pub(super) async fn call_task_kill_session(
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

pub(super) async fn call_task_blocked_list(
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
