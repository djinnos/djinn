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
use djinn_db::{AgentRepository, EpicRepository, SessionRepository, TaskRepository};
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

mod ci;
mod code_intel;
mod memory_agent;
mod task_admin;
mod task_epic;
mod workspace;

use ci::call_ci_job_log;
pub(crate) use code_intel::{call_code_graph, call_github_search, call_lsp};
use memory_agent::{
    call_agent_amend_prompt, call_agent_create, call_agent_metrics, call_memory_broken_links,
    call_memory_build_context, call_memory_edit, call_memory_health, call_memory_list,
    call_memory_orphans, call_memory_read, call_memory_search, call_memory_write,
};
use task_admin::{
    call_task_archive_activity, call_task_blocked_list, call_task_delete_branch,
    call_task_kill_session, call_task_reset_counters, call_task_transition,
};
use task_epic::{
    call_epic_close, call_request_lead, call_request_planner, call_task_activity_list,
    call_task_comment_add, call_task_create, call_task_list, call_task_show, call_task_update,
    call_task_update_ac,
};
pub(crate) use task_epic::{call_epic_show, call_epic_tasks, call_epic_update};
use workspace::{call_apply_patch, call_edit};
pub(crate) use workspace::{call_read, call_shell, call_write};

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
