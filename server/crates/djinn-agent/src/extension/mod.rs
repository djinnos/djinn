use std::path::{Path, PathBuf};

mod fuzzy;
mod schemas;

use std::process::Stdio;

use tokio::time::Duration;

use rmcp::model::Tool as RmcpTool;
use rmcp::object;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub(crate) use self::schemas::{
    tool_schemas_architect, tool_schemas_lead, tool_schemas_planner, tool_schemas_reviewer,
    tool_schemas_worker,
};

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

fn from_value<T>(value: serde_json::Value) -> Result<T, serde_json::Error>
where
    T: DeserializeOwned,
{
    serde_json::from_value(value)
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
        "request_lead" => call_request_lead(state, &call.arguments).await,
        "request_architect" => call_request_architect(state, &call.arguments).await,
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
        "memory_build_context" => {
            call_memory_build_context(state, &call.arguments, session_task_id).await
        }
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

mod params;

use self::params::*;

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

async fn call_request_lead(
    state: &AgentContext,
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
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
    let timeout_ms = p.timeout_ms.unwrap_or(120_000).clamp(1000, 600_000);

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
                fuzzy::fuzzy_replace(&content, &p.old_text, &p.new_text, &path)?;
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

    // Lead approve: squash merge (verification gate runs inside merge_and_transition).
    if action == TransitionAction::LeadApprove {
        if task.status != TaskStatus::InLeadIntervention.as_str() {
            return Ok(
                serde_json::json!({ "error": "lead_approve is only valid from in_lead_intervention" }),
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
                    TransitionAction::LeadInterventionComplete,
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


pub(crate) use self::schemas::call_tool;
