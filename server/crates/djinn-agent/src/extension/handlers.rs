use std::collections::HashSet;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use rmcp::Json;

use crate::context::AgentContext;
use crate::lsp::{SymbolQuery, parse_symbol_kind_filter};
use crate::mcp_client::McpToolRegistry;
use djinn_control_plane::tools::agent_tools::{
    AgentCreateParams as SharedAgentCreateParams, AgentMetricsParams as SharedAgentMetricsParams,
    create_agent as shared_create_agent, metrics_for_agents as shared_metrics_for_agents,
};
use djinn_control_plane::tools::epic_ops::{
    EpicShowRequest, EpicTasksRequest, EpicUpdateDeltaRequest,
};
use djinn_control_plane::tools::memory_tools::{
    BrokenLinksParams as SharedMemoryBrokenLinksParams,
    BuildContextParams as SharedMemoryBuildContextParams, EditParams as SharedMemoryEditParams,
    ExtractedAuditParams as SharedMemoryExtractedAuditParams,
    HealthParams as SharedMemoryHealthParams, ListParams as SharedMemoryListParams,
    OrphansParams as SharedMemoryOrphansParams, ReadParams as SharedMemoryReadParams,
    SearchParams as SharedMemorySearchParams, WriteParams as SharedMemoryWriteParams,
};
use djinn_control_plane::tools::task_tools::{
    CommentTaskRequest as SharedCommentTaskRequest, CreateTaskRequest as SharedCreateTaskRequest,
    TransitionTaskRequest as SharedTransitionTaskRequest,
    UpdateTaskRequest as SharedUpdateTaskRequest, add_task_comment as shared_add_task_comment,
    create_task as shared_create_task, transition_task as shared_transition_task,
    update_task as shared_update_task,
};
use djinn_db::repositories::proposal::ProposalAcceptanceCriteriaAmendment;
use djinn_db::{
    AgentRepository, EpicRepository, ProjectRepository, ProposalRepository, SessionRepository,
    TaskRepository,
};
use djinn_provider::github_api::GitHubApiClient;

use super::fuzzy::{MatchOutcome, UnicodeSpliceStatus, apply_match, find_match, match_note_for};
use super::helpers::*;
use super::sandbox;
use super::types::*;

mod ci;
// Internal renderer retained for focused coverage until artifact fetch wiring lands.
#[allow(dead_code)]
mod ci_artifact;
mod code_intel;
mod gate_guard;
mod jit_pitfalls;
// Retained for test coverage; production dispatch goes through djinn-mcp-extension.
#[allow(dead_code)]
mod memory_agent;
// Retained for test coverage; production dispatch goes through djinn-mcp-extension.
#[allow(dead_code)]
mod task_admin;
// Retained for test coverage; production dispatch goes through djinn-mcp-extension.
#[allow(dead_code)]
mod task_epic;
mod workspace;
mod workspace_helpers;

#[cfg(test)]
pub(crate) use jit_pitfalls::force_trace_candidate_serialization_failure_for_test;

// ── Re-exports for agent-internal callers ────────────────────────────────
// These handler functions are called directly from `direct_services.rs`,
// `chat_tools.rs`, and other agent-internal modules.  They remain local
// because they need concrete `AgentContext` / workspace / MCP-registry
// internals that `djinn-mcp-extension` does not own.
pub(crate) use ci::call_ci_job_log;
pub(crate) use code_intel::{call_code_graph, call_github_fetch_file, call_github_search};
#[cfg(test)]
pub(super) use code_intel::{call_code_graph_inner, call_lsp, should_pre_resolve_chat_key};
pub(crate) use task_admin::call_task_kill_session;
pub(crate) use workspace::{
    call_apply_patch, call_code_search, call_edit, call_read, call_shell, call_write,
};

// Re-export task_epic functions used by the local fallback dispatch.
pub(crate) use task_epic::call_request_planner;
// Re-export task_admin functions used by the local fallback dispatch.
use task_admin::{call_task_delete_branch, call_task_transition};

// ── Test-only re-exports ────────────────────────────────────────────────
// These handler functions are no longer in the production fallback dispatch
// (djinn-mcp-extension handles them), but extension tests still exercise
// them directly.  Re-export under `#[cfg(test)]` so tests compile.
#[cfg(test)]
pub(super) use task_epic::{
    call_epic_show, call_epic_tasks, call_epic_update, call_proposal_ac_amend,
    call_proposal_ac_set, call_proposal_reconcile_obsolete_epic,
};
// Deprecated compatibility handler: retained for drain-window tests only.
// Production dispatch no longer advertises or routes request_lead for
// workers/reviewers (epic 10qg).
#[cfg(test)]
pub(super) use task_epic::call_request_lead;

// ─── hfhw cutover note ──────────────────────────────────────────────────
//
// Tool dispatch is now two-phase (see `extension/mod.rs`):
//
// 1. `djinn_mcp_extension::dispatch::dispatch_tool_call` handles most tools
//    through `ExtensionContext` / `SupervisorServices`:
//      task_*, epic_*, proposal_*, memory_*, agent_*, lsp, ci_job_log,
//      github_search, task_archive_activity, task_reset_counters,
//      task_blocked_list.
//
// 2. This local fallback handles ONLY tools that require djinn-agent
//    internals (workspace mutation, code_graph, skill_read, destructive
//    task admin, request_planner, dynamic MCP registry).
//
// Duplicated production handlers for tools in group (1) have been removed
// from this fallback dispatch.  The handler modules (`memory_agent`,
// `task_epic`, etc.) are retained for their sub-handler implementations
// and test coverage.

// Central tool-call dispatch: each arg is a distinct collaborator/context the
// handlers need; a bag struct would only relocate the same fields.
#[allow(clippy::too_many_arguments)]
pub(super) async fn dispatch_tool_call(
    state: &AgentContext,
    _services: &dyn djinn_supervisor::SupervisorServices,
    prepared: djinn_mcp_extension::compatibility::PreparedToolCall,
    worktree_path: &Path,
    allowed_schemas: Option<&[serde_json::Value]>,
    session_task_id: Option<&str>,
    session_role: Option<&str>,
    mcp_registry: Option<&McpToolRegistry>,
    cancel: &super::ToolCancellation,
) -> djinn_core::tool_call::ToolCallOutcome {
    let call = IncomingToolCall {
        name: prepared.name,
        arguments: prepared.arguments,
    };
    let warnings = prepared.compatibility_warnings;

    let project = {
        let repo = djinn_db::ProjectRepository::new(state.db.clone(), state.event_bus.clone());
        let mut candidates: Vec<String> = vec![worktree_path.to_string_lossy().into_owned()];
        if let Some(args) = call.arguments.as_ref()
            && let Some(proj) = args.get("project").and_then(|v| v.as_str())
        {
            candidates.push(proj.to_string());
        }
        let mut resolved: Option<djinn_core::models::Project> = None;
        for cand in candidates {
            if let Ok(Some(id)) = repo.resolve(&cand).await
                && let Ok(Some(proj)) = repo.get(&id).await
            {
                resolved = Some(proj);
                break;
            }
        }
        if resolved.is_none()
            && let Some(default_id) = state.default_project_id.as_deref()
            && !default_id.is_empty()
            && let Ok(Some(proj)) = repo.get(default_id).await
        {
            resolved = Some(proj);
        }
        resolved
    };
    let project_id = project.as_ref().map(|project| project.id.clone());
    let _project_ref = project
        .as_ref()
        .map(|project| project.slug())
        .unwrap_or_else(|| worktree_path.display().to_string());
    let worktree_project_path = worktree_path.display().to_string();

    // ── Fail-closed: reject dynamic MCP registry tools when an allowlist is
    // active.  MCP registry tools are registered at runtime from external
    // servers and are NOT part of the evidence-spike (or any restricted)
    // schema surface.  Without this guard, the catch-all arm below would
    // happily dispatch an arbitrary MCP tool even though it was never
    // vetted for the restricted profile.
    //
    // For normal (unrestricted) sessions, `allowed_schemas` is `None` and
    // this block is a no-op.
    if allowed_schemas.is_some()
        && !matches!(
            call.name.as_str(),
            "request_planner"
                | "task_transition"
                | "task_delete_branch"
                | "task_kill_session"
                | "shell"
                | "read"
                | "code_search"
                | "write"
                | "edit"
                | "apply_patch"
                | "code_graph"
                | "skill_read"
        )
        && let Some(registry) = mcp_registry
        && registry.has_tool(&call.name)
    {
        return djinn_core::tool_call::ToolCallOutcome::from_result(Err(format!(
            "tool `{}` is a dynamic MCP registry tool and is not permitted under the active restricted profile",
            call.name
        )));
    }

    let result = match call.name.as_str() {
        // ── Agent-local task admin tools ─────────────────────────────────
        // These require djinn-agent internals (task_merge, knowledge_promotion)
        // and are NOT handled by djinn-mcp-extension.
        "request_planner" => call_request_planner(state, &call.arguments).await,
        "task_transition" => {
            call_task_transition(state, &call.arguments, &worktree_project_path).await
        }
        "task_delete_branch" => call_task_delete_branch(state, &call.arguments).await,
        "task_kill_session" => call_task_kill_session(state, &call.arguments).await,

        // ── Workspace mutation / execution tools ────────────────────────
        // Need concrete AgentContext::working_root_for, sandbox, repo_access.
        "shell" => {
            let root = state.working_root_for(worktree_path);
            call_shell(state, &call.arguments, &root, session_role, cancel).await
        }
        "read" => {
            let root = state.working_root_for(worktree_path);
            call_read(state, &call.arguments, &root).await
        }
        "code_search" => call_code_search(state, &call.arguments).await,
        "write" => {
            call_write(
                state,
                &call.arguments,
                worktree_path,
                project_id.as_deref(),
                session_task_id,
                session_role,
            )
            .await
        }
        "edit" => {
            call_edit(
                state,
                &call.arguments,
                worktree_path,
                project_id.as_deref(),
                session_task_id,
                session_role,
            )
            .await
        }
        "apply_patch" => {
            call_apply_patch(
                state,
                &call.arguments,
                worktree_path,
                project_id.as_deref(),
                session_task_id,
                session_role,
            )
            .await
        }

        // ── Code graph (agent-local bridge) ─────────────────────────────
        // djinn-mcp-extension returns Unhandled for code_graph because it
        // needs control-plane graph bridges + chat pre-resolve.
        "code_graph" => {
            let root = state.working_root_for(worktree_path);
            let root_str = root.to_string_lossy().into_owned();
            let pid = project_id.as_deref().unwrap_or("");
            call_code_graph(state, &call.arguments, pid, &root_str).await
        }

        // ── Skill read (agent-local) ────────────────────────────────────
        // Needs native skills and repository-convention discovery for session context.
        "skill_read" => {
            let root = state.working_root_for(worktree_path);
            call_skill_read(&call.arguments, &root, state, session_role, session_task_id).await
        }

        // ── Dynamic MCP registry fallback ───────────────────────────────
        // Tools registered via McpToolRegistry (external MCP servers).
        other => {
            if let Some(registry) = mcp_registry
                && registry.has_tool(other)
            {
                registry.call_tool(other, call.arguments.clone()).await
            } else {
                Err(format!("unknown djinn frontend tool: {other}"))
            }
        }
    };
    match djinn_core::tool_call::ToolCallOutcome::from_result(result) {
        djinn_core::tool_call::ToolCallOutcome::Success { value, .. } => {
            djinn_core::tool_call::ToolCallOutcome::Success { value, warnings }
        }
        failure => failure,
    }
}

/// Load the full content of an assigned skill on demand (G5 progressive
/// disclosure).  For native skills (platform-owned, compiled-in), the body is
/// served from the immutable native registry rather than the worktree.  For
/// project/worktree skills, the body is resolved read-only from `.claude/skills/`
/// or `.opencode/skills/`.
///
/// Native skills are only served when the session role has the skill assigned
/// (i.e., the native skill is recommended for the role AND the session's
/// authoring trigger resolved it).  Unknown or unassigned names produce a clean
/// error.
async fn call_skill_read(
    arguments: &Option<serde_json::Map<String, serde_json::Value>>,
    worktree_root: &Path,
    state: &AgentContext,
    session_role: Option<&str>,
    session_task_id: Option<&str>,
) -> Result<serde_json::Value, String> {
    let name = arguments
        .as_ref()
        .and_then(|m| m.get("name"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "skill_read requires a non-empty `name` argument".to_string())?;

    // ── Native skill path ──────────────────────────────────────────────────
    if let Some(native) = crate::native_skills::native_skill(name) {
        let role = session_role.unwrap_or("");
        let recommended = crate::native_skills::native_skill_names_for_role(role);
        if !recommended.contains(&name) {
            return Err(format!(
                "unknown skill `{name}`: not an assigned skill for this session"
            ));
        }

        let task_issue_type = if let Some(task_id) = session_task_id {
            let task_repo =
                djinn_db::TaskRepository::new(state.db.clone(), state.event_bus.clone());
            // `session_task_id` is the task UUID in production (the reply loop
            // passes `ctx.task_id == task.id`), but callers/tests may pass a
            // short_id. `resolve` accepts both — using `get_by_short_id` here
            // silently returned None for the UUID, leaving issue_type empty so
            // the authoring trigger never fired and `visual-spec` was never
            // assignable to ANY role in production.
            match task_repo.resolve(task_id).await {
                Ok(Some(task)) => task.issue_type,
                _ => String::new(),
            }
        } else {
            String::new()
        };

        // Route through the SAME classifier session construction uses, so this
        // gate never drifts from the set of roles that actually get the skill
        // assigned. (It used to hardcode planner-only and rejected the Advocate.)
        let trigger =
            crate::actors::slot::lifecycle::task_classifier::classify_native_skill_trigger_by_type(
                role,
                &task_issue_type,
            );

        if trigger.is_none() {
            return Err(format!(
                "unknown skill `{name}`: not an assigned skill for this session"
            ));
        }

        tracing::info!(
            skill = %name,
            version = %native.version,
            role = %role,
            "skill_read: serving native skill from registry"
        );

        return Ok(serde_json::json!({
            "name": native.name,
            "description": native.description,
            "required": true,
            "content": native.content,
            "version": native.version,
        }));
    }

    // ── Project/worktree skill path ────────────────────────────────────────
    let skill = crate::skills::load_skills(worktree_root, &[name.to_string()])
        .into_iter()
        .next()
        .ok_or_else(|| format!("unknown skill `{name}`: not an assigned skill for this session"))?;

    Ok(serde_json::json!({
        "name": skill.name,
        "description": skill.description,
        "required": skill.required,
        "content": skill.content,
    }))
}
